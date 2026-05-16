use miette::Result;

use quire::Quire;

pub async fn run(quire: &Quire, ci_routes: axum::Router, api_routes: axum::Router) -> Result<()> {
    crate::server::run(quire, ci_routes, api_routes).await
}
