use miette::Result;

use quire::Quire;

pub async fn run(quire: &Quire, web_routes: axum::Router, api_routes: axum::Router) -> Result<()> {
    crate::server::run(quire, web_routes, api_routes).await
}
