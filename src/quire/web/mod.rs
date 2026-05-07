//! Read-only CI web view.
//!
//! Two pages:
//! - `GET /<name>/ci` — most-recent runs for a repo.
//! - `GET /<name>/ci/<run-id>` — per-run detail with jobs and logs.
//!
//! Server-rendered HTML via Askama templates. JavaScript-optional.

pub mod auth;
pub mod db;
pub mod format;
pub mod handlers;
pub mod templates;

use axum::middleware;

use crate::Quire;

pub fn router(quire: Quire) -> axum::Router {
    let ci_routes = axum::Router::new()
        .route("/{repo}/ci", axum::routing::get(handlers::run_list))
        .route(
            "/{repo}/ci/{run_id}",
            axum::routing::get(handlers::run_detail),
        )
        .layer(middleware::from_fn(auth::require_auth));

    ci_routes.with_state(quire)
}
