//! Handlers for the bookmarks and tags listing pages.

use axum::extract::State;
use axum::http::StatusCode;
use axum::response::IntoResponse;
use axum::response::Response;

use super::super::templates::{BookmarksTemplate, Crumb, TagsTemplate, nav_sections};
use super::git::RepoView;
use super::render;
use crate::Quire;
use crate::quire::web::paths::{BookmarksPath, TagsPath};

pub async fn bookmarks_view(
    BookmarksPath { repo }: BookmarksPath,
    State(quire): State<Quire>,
    auth: super::super::auth::Auth,
) -> Response {
    let repo_display = repo.trim_end_matches(".git").to_string();
    let repo_name = super::super::db::resolve_repo_name(&repo);
    let git_repo = match quire.repo(&repo_name) {
        Ok(r) if r.exists() => r,
        _ => return StatusCode::NOT_FOUND.into_response(),
    };

    let (bookmarks, tags) = tokio::task::spawn_blocking(move || {
        let reader = RepoView::new(&git_repo);
        (reader.bookmarks(), reader.tags())
    })
    .await
    .unwrap_or_default();

    let crumbs = vec![Crumb::new("bookmarks")];
    let tmpl = BookmarksTemplate {
        sections: nav_sections(&repo_display, "bookmarks", auth.is_authenticated()),
        repo: repo_display,
        crumbs,
        bookmarks,
        tags,
    };
    render(&tmpl)
}

pub async fn tags_view(
    TagsPath { repo }: TagsPath,
    State(quire): State<Quire>,
    auth: super::super::auth::Auth,
) -> Response {
    let repo_display = repo.trim_end_matches(".git").to_string();
    let repo_name = super::super::db::resolve_repo_name(&repo);
    let git_repo = match quire.repo(&repo_name) {
        Ok(r) if r.exists() => r,
        _ => return StatusCode::NOT_FOUND.into_response(),
    };

    let (bookmarks, tags) = tokio::task::spawn_blocking(move || {
        let reader = RepoView::new(&git_repo);
        (reader.bookmarks(), reader.tags())
    })
    .await
    .unwrap_or_default();

    let crumbs = vec![Crumb::new("tags")];
    let tmpl = TagsTemplate {
        sections: nav_sections(&repo_display, "tags", auth.is_authenticated()),
        repo: repo_display,
        crumbs,
        bookmarks,
        tags,
    };
    render(&tmpl)
}
