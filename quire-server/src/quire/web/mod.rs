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
        config, file_view, repo_home, run_detail, run_list, stylesheet, tree_view, tree_view_path,
    },
};

/// Routes that require authentication.
///
/// Currently only the CI views: run list and run detail pages.
pub fn ci_router(quire: Quire) -> Router {
    Router::new()
        .route("/{repo}/ci", get(run_list))
        .route("/{repo}/ci/{run_id}", get(run_detail))
        .with_state(quire)
}

/// Public routes with no auth required.
pub fn public_router(quire: Quire) -> Router {
    Router::new()
        .route("/style.css", get(stylesheet))
        .route("/{repo}", get(repo_home))
        .route("/{repo}/tree", get(tree_view))
        .route("/{repo}/tree/{*path}", get(tree_view_path))
        .route("/{repo}/blob/{*path}", get(file_view))
        .route("/config", get(config))
        .with_state(quire)
}
