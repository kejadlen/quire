use std::net::SocketAddr;

use axum::Router;
use axum::routing::get;
use miette::{IntoDiagnostic, Result};
use tracing_subscriber::EnvFilter;
use tracing_subscriber::prelude::__tracing_subscriber_SubscriberExt;
use tracing_subscriber::util::SubscriberInitExt;

const VERSION: &str = env!("QUIRE_VERSION");

async fn health() -> &'static str {
    "ok"
}

async fn index() -> String {
    format!("quire-ci {VERSION}\n")
}

pub async fn run(port: u16) -> Result<()> {
    let filter = EnvFilter::builder()
        .with_env_var("QUIRE_LOG")
        .from_env()
        .into_diagnostic()?;

    let fmt_layer = tracing_subscriber::fmt::layer().with_writer(std::io::stderr);

    tracing_subscriber::registry()
        .with(fmt_layer)
        .with(filter)
        .init();

    let app = Router::new()
        .route("/health", get(health))
        .route("/", get(index));

    let addr = SocketAddr::from(([0, 0, 0, 0], port));
    tracing::info!(%addr, "starting HTTP server");

    let listener = tokio::net::TcpListener::bind(addr)
        .await
        .into_diagnostic()?;

    axum::serve(listener, app).await.into_diagnostic()?;

    tracing::info!(version = %VERSION, "server shutdown complete");

    Ok(())
}
