use std::net::SocketAddr;

use axum::Router;
use axum::routing::get;
use miette::{IntoDiagnostic, Result};

use crate::quire::QuireCi;

const VERSION: &str = env!("QUIRE_VERSION");

async fn health() -> &'static str {
    "ok"
}

async fn index() -> String {
    format!("quire-ci {VERSION}\n")
}

pub async fn run(quire: QuireCi) -> Result<()> {
    let port = quire.config().port;

    let _sentry = init_sentry(&quire)?;
    let miette_layer = quire_core::telemetry::MietteLayer::new();
    let _tracing_guard = quire_core::telemetry::init_tracing(
        miette_layer,
        quire_core::telemetry::FmtMode::AutoJson,
    )?;

    let app = Router::new()
        .route("/health", get(health))
        .route("/", get(index));

    let addr = SocketAddr::from(([0, 0, 0, 0], port));
    tracing::info!(%addr, "starting HTTP server");

    let listener = tokio::net::TcpListener::bind(addr)
        .await
        .into_diagnostic()?;

    axum::serve(listener, app).await.into_diagnostic()
}

/// Initialize Sentry if the global config provides a DSN.
fn init_sentry(quire: &QuireCi) -> Result<Option<sentry::ClientInitGuard>> {
    let Some(sentry_config) = quire.config().sentry.as_ref() else {
        return Ok(None);
    };
    let dsn = sentry_config.dsn.reveal().into_diagnostic()?;
    Ok(Some(sentry::init((
        dsn,
        quire_core::telemetry::sentry_client_options(VERSION),
    ))))
}
