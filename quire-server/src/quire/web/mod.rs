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
use axum_extra::routing::RouterExt;

use crate::{
    Quire,
    quire::web::handlers::{
        config, repo_home, run_detail, run_list, stylesheet, tree_view, tree_view_path,
    },
};

pub use paths::{RepoPath, RunDetailPath, RunListPath, TreePath, TreeRootPath};

pub mod paths {
    use axum_extra::routing::TypedPath;
    use serde::Deserialize;

    #[derive(TypedPath, Deserialize)]
    #[typed_path("/{repo}")]
    pub struct RepoPath {
        pub repo: String,
    }

    #[derive(TypedPath, Deserialize)]
    #[typed_path("/{repo}/ci")]
    pub struct RunListPath {
        pub repo: String,
    }

    #[derive(TypedPath, Deserialize)]
    #[typed_path("/{repo}/ci/{run_id}")]
    pub struct RunDetailPath {
        pub repo: String,
        pub run_id: String,
    }

    #[derive(TypedPath, Deserialize)]
    #[typed_path("/{repo}/tree")]
    pub struct TreeRootPath {
        pub repo: String,
    }

    #[derive(TypedPath, Deserialize)]
    #[typed_path("/{repo}/tree/{*path}")]
    pub struct TreePath {
        pub repo: String,
        pub path: String,
    }
}

/// Routes that require authentication.
///
/// Currently only the CI views: run list and run detail pages.
pub fn ci_router(quire: Quire) -> Router {
    Router::new()
        .typed_get(run_list)
        .typed_get(run_detail)
        .with_state(quire)
}

/// Public routes with no auth required.
pub fn public_router(quire: Quire) -> Router {
    Router::new()
        .route("/style.css", get(stylesheet))
        .typed_get(repo_home)
        .typed_get(tree_view)
        .typed_get(tree_view_path)
        .route("/config", get(config))
        .with_state(quire)
}
