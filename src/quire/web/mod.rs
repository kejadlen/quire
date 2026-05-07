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

use crate::Quire;

/// Bare CI routes without any auth middleware.
///
/// The caller decides whether to layer auth on top (e.g.
/// `.layer(middleware::from_fn(auth::require_auth))`).
pub fn router(quire: Quire) -> axum::Router {
    axum::Router::new()
        .route("/style.css", axum::routing::get(handlers::stylesheet))
        .route("/{repo}", axum::routing::get(handlers::repo_redirect))
        .route("/{repo}/ci", axum::routing::get(handlers::run_list))
        .route(
            "/{repo}/ci/{run_id}",
            axum::routing::get(handlers::run_detail),
        )
        .with_state(quire)
}
