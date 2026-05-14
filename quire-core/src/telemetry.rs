use std::cell::RefCell;
use std::error::Error;
use std::sync::Arc;

thread_local! {
    static MIETTE_RENDER: RefCell<Option<String>> = const { RefCell::new(None) };
}

type RenderFn = Box<dyn (Fn(&(dyn Error + 'static)) -> Option<String>) + Send + Sync>;

/// A [`tracing_subscriber::Layer`] that intercepts `record_error` calls,
/// renders the error as a naratable miette diagnostic, and stashes the
/// result in a thread-local for [`before_send`] to attach to the Sentry event.
///
/// Register concrete error types with [`MietteLayer::with_type`]. The layer
/// walks the full source chain at each registered type, so transparent wrapper
/// errors don't need separate registration — registering the outermost type is
/// sufficient when it carries `#[diagnostic(transparent)]`.
///
/// # Layer ordering
///
/// Register this layer **before** `sentry_tracing::layer()` in the `.with()`
/// chain. `tracing_subscriber::Layered::on_event` dispatches inner-first, so
/// the layer added earlier fires first — and this layer must set the
/// thread-local before sentry-tracing's `on_event` calls `capture_event`
/// (which invokes `before_send` synchronously).
///
/// ```ignore
/// tracing_subscriber::registry()
///     .with(miette_layer)              // fires first — sets thread-local
///     .with(sentry_tracing::layer())   // fires next — capture_event reads it
///     .with(fmt_layer)
///     .with(filter)
///     .init();
/// ```
pub struct MietteLayer {
    renderers: Arc<Vec<RenderFn>>,
}

impl Default for MietteLayer {
    fn default() -> Self {
        Self::new()
    }
}

impl MietteLayer {
    pub fn new() -> Self {
        Self {
            renderers: Arc::new(Vec::new()),
        }
    }

    /// Register a concrete error type for miette rendering.
    ///
    /// When an error field is recorded via `record_error`, the layer tries
    /// `downcast_ref::<T>` at each level of the source chain. The first match
    /// is rendered with [`miette::NarratableReportHandler`] and stashed in the
    /// thread-local for [`before_send`] to attach.
    pub fn with_type<T>(mut self) -> Self
    where
        T: miette::Diagnostic + 'static,
    {
        Arc::get_mut(&mut self.renderers)
            .expect("no other Arc refs at construction time")
            .push(Box::new(|err: &(dyn Error + 'static)| {
                let mut cur: Option<&(dyn Error + 'static)> = Some(err);
                while let Some(e) = cur {
                    if let Some(diag) = e.downcast_ref::<T>() {
                        let mut buf = String::new();
                        if miette::NarratableReportHandler::new()
                            .render_report(&mut buf, diag)
                            .is_ok()
                            && !buf.trim().is_empty()
                        {
                            return Some(buf);
                        }
                    }
                    cur = e.source();
                }
                None
            }));
        self
    }

    fn try_render(&self, err: &(dyn Error + 'static)) -> Option<String> {
        self.renderers.iter().find_map(|r| r(err))
    }
}

struct ErrorVisitor<'a> {
    layer: &'a MietteLayer,
}

impl tracing::field::Visit for ErrorVisitor<'_> {
    fn record_error(&mut self, _field: &tracing::field::Field, value: &(dyn Error + 'static)) {
        if let Some(rendered) = self.layer.try_render(value) {
            MIETTE_RENDER.with(|cell| *cell.borrow_mut() = Some(rendered));
        }
    }

    fn record_debug(&mut self, _: &tracing::field::Field, _: &dyn std::fmt::Debug) {}
}

impl<S: tracing::Subscriber> tracing_subscriber::Layer<S> for MietteLayer {
    fn on_event(
        &self,
        event: &tracing::Event<'_>,
        _ctx: tracing_subscriber::layer::Context<'_, S>,
    ) {
        // Clear stale data first — handles the case where a previous error
        // was not captured by Sentry (e.g. a WARN that becomes a breadcrumb
        // rather than an event, so before_send never fires to clear it).
        MIETTE_RENDER.with(|cell| *cell.borrow_mut() = None);

        if *event.metadata().level() > tracing::Level::WARN {
            return;
        }

        let mut visitor = ErrorVisitor { layer: self };
        event.record(&mut visitor);
    }
}

/// Sentry `before_send` hook: reads the thread-local miette rendering and
/// attaches it to `extra["diagnostic"]` before the event is sent.
///
/// Install at Sentry init time:
///
/// ```ignore
/// sentry::init((dsn, sentry::ClientOptions {
///     before_send: Some(std::sync::Arc::new(quire_telemetry::before_send)),
///     ..Default::default()
/// }));
/// ```
///
/// The hook consumes the thread-local so each event gets at most one attachment
/// and stale data from un-captured events is cleaned up automatically.
pub fn before_send(
    mut event: sentry::protocol::Event<'static>,
) -> Option<sentry::protocol::Event<'static>> {
    if let Some(rendered) = MIETTE_RENDER.with(|cell| cell.borrow_mut().take()) {
        event
            .extra
            .insert("diagnostic".into(), serde_json::Value::String(rendered));
    }
    Some(event)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Arc, Mutex};
    use tracing_subscriber::layer::SubscriberExt;

    #[derive(Debug, thiserror::Error, miette::Diagnostic)]
    #[error("outer message")]
    struct TestDiag {
        #[help]
        help: String,
    }

    /// Reads the thread-local in its `on_event` to simulate what
    /// sentry-tracing's layer does (capture_event → before_send reads the
    /// thread-local). Lets us verify that MietteLayer fires *before* this
    /// layer in the chain.
    struct CaptureLayer {
        seen: Arc<Mutex<Option<String>>>,
    }

    impl<S: tracing::Subscriber> tracing_subscriber::Layer<S> for CaptureLayer {
        fn on_event(
            &self,
            _event: &tracing::Event<'_>,
            _ctx: tracing_subscriber::layer::Context<'_, S>,
        ) {
            MIETTE_RENDER.with(|cell| {
                if let Some(v) = cell.borrow().as_ref() {
                    *self.seen.lock().unwrap() = Some(v.clone());
                }
            });
        }
    }

    /// Regression test: tracing_subscriber dispatches `on_event` inner-first,
    /// so the layer registered *earlier* via `.with()` fires first. The
    /// MietteLayer must be registered before `sentry_tracing::layer()` so its
    /// thread-local is populated by the time sentry-tracing's `on_event`
    /// calls `capture_event` (and thus `before_send`).
    #[test]
    fn miette_layer_fires_before_downstream_layers() {
        let seen = Arc::new(Mutex::new(None));
        let capture = CaptureLayer { seen: seen.clone() };
        let miette = MietteLayer::new().with_type::<TestDiag>();

        let subscriber = tracing_subscriber::registry().with(miette).with(capture);

        tracing::subscriber::with_default(subscriber, || {
            let err = TestDiag {
                help: "try again".into(),
            };
            tracing::error!(error = &err as &(dyn std::error::Error + 'static), "failed",);
        });

        let rendered = seen
            .lock()
            .unwrap()
            .clone()
            .expect("downstream layer should have observed the rendered diagnostic");
        assert!(
            rendered.contains("outer message"),
            "rendering should include the diagnostic message: {rendered}"
        );
    }
}
