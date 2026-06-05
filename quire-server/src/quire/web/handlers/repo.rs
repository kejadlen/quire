//! Handler for the repository home page.

use axum::extract::State;
use axum::http::StatusCode;
use axum::response::IntoResponse;
use axum::response::Response;

use super::super::auth::Auth;
use super::super::db;
use super::super::templates::{RepoHomeTemplate, RunListRow, nav_sections};
use super::git::RepoView;
use super::render;
use crate::Quire;
use crate::quire::web::paths::RepoPath;

pub async fn repo_home(
    RepoPath { repo }: RepoPath,
    State(quire): State<Quire>,
    auth: Auth,
) -> Response {
    let repo_display = repo.trim_end_matches(".git").to_string();
    let repo_name = db::resolve_repo_name(&repo);
    let git_repo = match quire.repo(&repo_name) {
        Ok(r) if r.exists() => r,
        _ => return StatusCode::NOT_FOUND.into_response(),
    };

    let q = quire.clone();
    let rn = repo_name.clone();
    let is_authed = auth.is_authenticated();
    let recent_runs: Vec<RunListRow> = if is_authed {
        match tokio::task::spawn_blocking(move || db::load_runs(&q, &rn)).await {
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
        }
    } else {
        vec![]
    };

    let rd = repo_display.clone();
    let (head, readme_html, recent_changes) =
        tokio::task::spawn_blocking(move || RepoView::new(&git_repo).read_all(&rd))
            .await
            .unwrap_or_default();

    let tmpl = RepoHomeTemplate {
        sections: nav_sections(&repo_display, "overview", is_authed),
        repo: repo_display,
        crumbs: None,
        head,
        readme_html,
        recent_runs,
        recent_changes,
    };
    render(&tmpl)
}
