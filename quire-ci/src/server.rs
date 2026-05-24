use std::net::SocketAddr;

use axum::Router;
use axum::routing::get;
use quire_core::telemetry::{self, FmtMode};

use crate::quire::QuireCi;

const VERSION: &str = env!("QUIRE_VERSION");

async fn health() -> &'static str {
    "ok"
}

async fn index() -> String {
    format!("quire-ci {VERSION}\n")
}

#[derive(Debug, thiserror::Error, miette::Diagnostic)]
pub enum Error {
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),

    #[error(transparent)]
    Secret(#[from] quire_core::secret::Error),

    #[error(transparent)]
    Telemetry(#[from] quire_core::telemetry::Error),
}

pub type Result<T> = std::result::Result<T, Error>;

pub async fn run(quire: QuireCi) -> Result<()> {
    let port = quire.config().port;

    let miette_layer = telemetry::MietteLayer::new();
    let _guard = telemetry::init_telemetry(
        miette_layer,
        FmtMode::AutoJson,
        quire.config().sentry.as_ref(),
        VERSION,
    )?;

    let app = Router::new()
        .route("/health", get(health))
        .route("/", get(index));

    let addr = SocketAddr::from(([0, 0, 0, 0], port));
    tracing::info!(%addr, "starting HTTP server");

    let listener = tokio::net::TcpListener::bind(addr).await?;

    axum::serve(listener, app).await?;

    Ok(())
}
