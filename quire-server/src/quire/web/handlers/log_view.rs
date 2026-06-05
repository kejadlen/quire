//! Handler for the repository commit log page.

use axum::extract::State;
use axum::response::Response;

use super::super::error::WebError;
use super::super::templates::{LogTemplate, nav_sections};
use super::git::RepoView;
use super::render;
use crate::Quire;
use crate::quire::web::paths::LogPath;

pub async fn log_view(
    LogPath { repo }: LogPath,
    State(quire): State<Quire>,
    auth: super::super::auth::Auth,
) -> Result<Response, WebError> {
    let repo_display = repo.trim_end_matches(".git").to_string();
    let repo_name = super::super::db::resolve_repo_name(&repo);
    let git_repo = quire.repo(&repo_name)?;

    let repo_d = repo_display.clone();
    let (changes, bookmark, sha_short) = tokio::task::spawn_blocking(move || {
        let reader = RepoView::new(&git_repo);
        let changes = reader.recent_changes(&repo_d);
        let bookmark = reader
            .run(&["symbolic-ref", "--short", "HEAD"])
            .unwrap_or_else(|| "main".to_string());
        let sha_short = reader
            .run(&["rev-parse", "--short", "HEAD"])
            .unwrap_or_else(|| "unknown".to_string());
        (changes, bookmark, sha_short)
    })
    .await?;

    let tmpl = LogTemplate {
        sections: nav_sections(&repo_display, "log", auth.is_authenticated()),
        repo: repo_display,
        crumbs: None,
        changes,
        bookmark,
        sha_short,
    };
    Ok(render(&tmpl))
}
