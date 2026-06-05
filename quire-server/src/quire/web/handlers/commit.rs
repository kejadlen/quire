//! Handler for the commit detail page.

use axum::extract::State;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};

use super::super::error::WebError;
use super::super::templates::{CommitParent, CommitTemplate, nav_sections};
use super::git::RepoView;
use super::render;
use crate::Quire;
use crate::quire::web::paths::CommitPath;

pub async fn commit_view(
    CommitPath { repo, sha }: CommitPath,
    State(quire): State<Quire>,
    auth: super::super::auth::Auth,
) -> Result<Response, WebError> {
    let repo_display = repo.trim_end_matches(".git").to_string();
    let repo_name = super::super::db::resolve_repo_name(&repo);
    let git_repo = quire.repo(&repo_name)?;

    let sha_clone = sha.clone();
    let result = tokio::task::spawn_blocking(move || {
        let reader = RepoView::new(&git_repo);

        // Resolve the short SHA to a full one.
        let full_sha = reader
            .run(&["rev-parse", &sha_clone])
            .unwrap_or(sha_clone.clone());

        let info = reader.run(&[
            "log",
            "-1",
            "--format=%H%n%P%n%s%n%b%n%an%n%ae%n%at",
            &full_sha,
        ])?;

        let mut lines = info.lines();
        let sha = lines.next()?.to_string();
        let parents_str = lines.next().unwrap_or("").to_string();
        let subject = lines.next().unwrap_or("").to_string();

        // Body is everything between subject and the last 3 lines (author, email, timestamp).
        let remaining: Vec<&str> = lines.collect();
        let n = remaining.len();
        if n < 3 {
            return None;
        }
        let author = remaining[n - 3].to_string();
        let email = remaining[n - 2].to_string();
        let timestamp_str = remaining[n - 1];
        let body = if n > 3 {
            remaining[..n - 3].join("\n")
        } else {
            String::new()
        };

        let timestamp_ms: i64 = timestamp_str.parse().ok().map(|secs: i64| secs * 1000)?;

        let parent_shars: Vec<String> = parents_str
            .split_whitespace()
            .filter(|s| !s.is_empty())
            .map(|p| p.to_string())
            .collect();

        let diff = reader
            .run(&["log", "-1", "--patch", "--format=", &full_sha])
            .unwrap_or_default();

        Some((
            sha,
            author,
            email,
            timestamp_ms,
            subject,
            body,
            parent_shars,
            diff,
        ))
    })
    .await?;

    let Some((sha, author, email, timestamp_ms, subject, body, parent_shas, diff)) = result else {
        return Ok(StatusCode::NOT_FOUND.into_response());
    };

    let parents: Vec<CommitParent> = parent_shas
        .into_iter()
        .map(|p| CommitParent {
            commit_url: format!("/{repo_display}/commits/{p}"),
            sha: p,
        })
        .collect();

    let sha_short = if sha.len() >= 8 {
        sha[..8].to_string()
    } else {
        sha.clone()
    };

    let tmpl = CommitTemplate {
        sections: nav_sections(&repo_display, "log", auth.is_authenticated()),
        repo: repo_display,
        crumbs: None,
        sha: sha.clone(),
        sha_short,
        sha_head: sha[..sha.len().min(4)].to_string(),
        sha_tail: if sha.len() > 4 {
            sha[4..sha.len().min(8)].to_string()
        } else {
            String::new()
        },
        author,
        email,
        date_relative: super::super::format::format_timestamp_relative(timestamp_ms),
        date_iso: super::super::format::format_timestamp_iso(timestamp_ms),
        subject,
        body,
        parents,
        diff,
    };
    Ok(render(&tmpl))
}
