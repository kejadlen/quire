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
fn init_sentry(quire: &QuireCi) -> Option<ClientInitGuard> {
    let config = match quire.global_config() {
        Ok(config) => config,
        Err(e) => {
            tracing::warn!(
                error = %e,
                "failed to load global config, skipping Sentry init"
            );
            return None;
        }
    };

    let sentry_config = config.sentry.as_ref()?;
    let dsn = match sentry_config.dsn.reveal() {
        Ok(dsn) => dsn,
        Err(e) => {
            tracing::warn!(
                error = &e as &(dyn std::error::Error + 'static),
                "failed to resolve Sentry DSN, skipping Sentry init"
            );
            return None;
        }
    };

    Some(sentry::init((
        dsn,
        telemetry::sentry_client_options(VERSION),
    )))
}

pub async fn run(quire: QuireCi) -> Result<()> {
    let config = quire.global_config().ok();
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

    axum::serve(listener, app).await.into_diagnostic()?;

    Ok(())
}
