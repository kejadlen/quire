//! Handler for the repository home page.

use axum::extract::{Path as AxumPath, State};
use axum::http::StatusCode;
use axum::response::IntoResponse;
use axum::response::Response;

use super::super::db;
use super::super::templates::{RepoHomeTemplate, RunListRow};
use super::git::{read_bookmarks, read_git_data, read_tags};
use super::render;
use crate::Quire;

pub async fn repo_home(State(quire): State<Quire>, AxumPath(repo): AxumPath<String>) -> Response {
    let repo_display = repo.trim_end_matches(".git").to_string();
    let repo_name = db::resolve_repo_name(&repo);
    let git_repo = match quire.repo(&repo_name) {
        Ok(r) if r.exists() => r,
        _ => return StatusCode::NOT_FOUND.into_response(),
    };

    let q = quire.clone();
    let rn = repo_name.clone();
    let recent_runs: Vec<RunListRow> = match tokio::task::spawn_blocking(move || {
        db::load_runs(&q, &rn)
    })
    .await
    {
        Ok(Ok(runs)) => runs
            .into_iter()
            .take(5)
            .map(|r| RunListRow {
                id: r.id,
                outcome: r.outcome,
                sha: r.sha,
                ref_name: r.ref_name,
                created_at: r.created_at,
                dispatched_at: r.dispatched_at,
                resolved_at: r.resolved_at,
            })
            .collect(),
        Ok(Err(e)) => {
            tracing::warn!(repo = %repo, error = &e as &(dyn std::error::Error + 'static), "failed to load runs for home");
            vec![]
        }
        Err(_) => vec![],
    };

    let (head, readme_html, bookmarks, tags, recent_changes) =
        tokio::task::spawn_blocking(move || read_git_data(&git_repo))
            .await
            .unwrap_or_default();

    let tmpl = RepoHomeTemplate {
        repo: repo_display,
        crumbs: vec![],
        head,
        readme_html,
        bookmarks,
        tags,
        recent_runs,
        recent_changes,
        active_section: "overview".to_string(),
    };
    render(&tmpl)
}
