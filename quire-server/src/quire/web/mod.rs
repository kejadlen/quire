//! Read-only CI web view.
//!
//! Two pages:
//! - `GET /<name>/ci` — most-recent runs for a repo.
//! - `GET /<name>/ci/<run-id>` — per-run detail with jobs and logs.
//!
//! Server-rendered HTML via Askama templates. JavaScript-optional.

pub mod api;
pub mod auth;
pub mod db;
pub mod format;
pub mod handlers;
pub mod templates;

use axum::{Router, routing::get};

use crate::{
    Quire,
    quire::web::handlers::{
        config, repo_home, run_detail, run_list, stylesheet, tree_view, tree_view_path,
    },
};

/// Bare CI routes without any auth middleware.
///
/// The caller decides whether to layer auth on top (e.g.
/// `.layer(middleware::from_fn(auth::require_auth))`).
pub fn router(quire: Quire) -> Router {
    Router::new()
        .route("/style.css", get(stylesheet))
        .route("/{repo}", get(repo_home))
        .route("/{repo}/ci", get(run_list))
        .route("/{repo}/ci/{run_id}", get(run_detail))
        .route("/{repo}/tree", get(tree_view))
        .route("/{repo}/tree/{*path}", get(tree_view_path))
        .route("/config", get(config))
        .with_state(quire)
}
