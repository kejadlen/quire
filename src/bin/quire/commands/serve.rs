use std::net::SocketAddr;

use axum::Router;
use axum::routing::get;
use miette::IntoDiagnostic;
use miette::Result;

use quire::Quire;

async fn health() -> &'static str {
    "ok"
}

async fn index() -> &'static str {
    "quire\n"
}

pub async fn run(_quire: &Quire) -> Result<()> {
    let addr: SocketAddr = ([0, 0, 0, 0], 3000).into();

    let app = Router::new()
        .route("/health", get(health))
        .route("/", get(index));

    tracing::info!(%addr, "starting HTTP server");

    let listener = tokio::net::TcpListener::bind(addr)
        .await
        .into_diagnostic()?;

    axum::serve(listener, app).await.into_diagnostic()
}
