use std::net::SocketAddr;

use axum::Router;
use axum::routing::get;
use miette::{IntoDiagnostic, Result};
use quire_core::telemetry::{self, FmtMode};
use sentry::ClientInitGuard;

use crate::quire::QuireCi;

const VERSION: &str = env!("QUIRE_VERSION");

async fn health() -> &'static str {
    "ok"
}

async fn index() -> String {
    format!("quire-ci {VERSION}\n")
}

/// Initialize Sentry if the global config provides a DSN.
///
/// Returns the guard if initialized, or None if Sentry is not configured.
/// Logs a warning on failure but does not abort.
fn init_sentry(quire: &QuireCi) -> Option<ClientInitGuard> {
    let config = quire
        .global_config()
        .inspect_err(|e| {
            tracing::warn!(
                error = %e,
                "failed to load global config, skipping Sentry init"
            );
        })
        .ok()?;

    let sentry_config = config.sentry.as_ref()?;
    let dsn = sentry_config
        .dsn
        .reveal()
        .inspect_err(|e| {
            tracing::warn!(
                error = %e,
                "failed to resolve Sentry DSN, skipping Sentry init"
            );
        })
        .ok()?;

    Some(sentry::init((
        dsn,
        telemetry::sentry_client_options(VERSION),
    )))
}

pub async fn run(quire: QuireCi) -> Result<()> {
    let config = quire
        .global_config()
        .inspect(|c| tracing::info!(port = c.port, "loaded config"))
        .inspect_err(|e| tracing::warn!(error = %e, "proceeding with defaults"))
        .ok();

    let port = config.as_ref().map(|c| c.port).unwrap_or(3000);

    let _sentry = init_sentry(&quire);
    let miette_layer = telemetry::MietteLayer::new();
    let _tracing_guard = telemetry::init_tracing(miette_layer, FmtMode::AutoJson)?;

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
