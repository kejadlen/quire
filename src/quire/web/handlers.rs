//! Route handlers for the web view.

use askama::Template;
use axum::extract::{Path as AxumPath, State};
use axum::http::StatusCode;
use axum::response::{Html, IntoResponse, Response};

use super::db;
use super::templates::*;
use crate::Quire;
use crate::error::display_chain;

/// Render a template into an HTML response, returning 500 on render failure.
fn render<T: Template>(tmpl: &T) -> Response {
    match tmpl.render() {
        Ok(body) => Html(body).into_response(),
        Err(e) => {
            tracing::error!(error = %e, "template render failed");
            (StatusCode::INTERNAL_SERVER_ERROR, "internal error").into_response()
        }
    }
}

/// Render the error template with the given status, falling back to plain
/// text if the error template itself fails to render.
fn render_error(repo: String, status: StatusCode, title: &str, detail: String) -> Response {
    let tmpl = ErrorTemplate {
        repo,
        page: "error".to_string(),
        title: title.to_string(),
        detail: detail.clone(),
    };
    match tmpl.render() {
        Ok(body) => (status, Html(body)).into_response(),
        Err(e) => {
            tracing::error!(error = %e, "error template render failed");
            (status, format!("{title}\n\n{detail}\n")).into_response()
        }
    }
}

pub async fn run_list(State(quire): State<Quire>, AxumPath(repo): AxumPath<String>) -> Response {
    let repo_display = repo.trim_end_matches(".git").to_string();
    let repo_name = db::resolve_repo_name(&repo);
    if quire.repo(&repo_name).is_err() {
        return StatusCode::NOT_FOUND.into_response();
    }

    let runs = match db::load_runs(&quire, &repo_name) {
        Ok(r) => r,
        Err(e) => {
            tracing::error!(repo = %repo, error = %display_chain(&e), "failed to load runs");
            return render_error(
                repo_display,
                StatusCode::INTERNAL_SERVER_ERROR,
                "Failed to load runs",
                display_chain(&e).to_string(),
            );
        }
    };

    let template_runs: Vec<RunListRow> = runs
        .into_iter()
        .map(|r| RunListRow {
            id: r.id,
            state: r.state,
            sha: r.sha,
            ref_name: r.ref_name,
            queued_at_ms: r.queued_at_ms,
            started_at_ms: r.started_at_ms,
            finished_at_ms: r.finished_at_ms,
        })
        .collect();

    let tmpl = RunListTemplate {
        repo: repo_display,
        page: "ci".to_string(),
        runs: template_runs,
    };
    render(&tmpl)
}

pub async fn run_detail(
    State(quire): State<Quire>,
    AxumPath((repo, run_id)): AxumPath<(String, String)>,
) -> Response {
    let repo_display = repo.trim_end_matches(".git").to_string();
    let repo_name = db::resolve_repo_name(&repo);
    if quire.repo(&repo_name).is_err() || !db::is_valid_run_id(&run_id) {
        return StatusCode::NOT_FOUND.into_response();
    }

    let result = db::load_run_detail(&quire, &repo_name, &run_id);
    let (run, jobs, sh_events) = match result {
        Ok(d) => d,
        Err(e) => {
            tracing::error!(repo = %repo, run_id = %run_id, error = %display_chain(&e), "failed to load run detail");
            return render_error(
                repo_display,
                StatusCode::INTERNAL_SERVER_ERROR,
                "Failed to load run",
                display_chain(&e).to_string(),
            );
        }
    };

    let detail_run = DetailRun {
        state: run.state,
        sha: run.sha,
        ref_name: run.ref_name,
        queued_at_ms: run.queued_at_ms,
        started_at_ms: run.started_at_ms,
        finished_at_ms: run.finished_at_ms,
    };

    // Load CRI log contents for each sh event.
    let runs_base = quire.base_dir().join("runs").join(&repo_name);
    let mut log_contents: std::collections::HashMap<(String, usize), String> =
        std::collections::HashMap::new();
    for (idx, ev) in sh_events.iter().enumerate() {
        if !db::is_safe_path_segment(&ev.job_id) {
            tracing::warn!(job_id = %ev.job_id, "skipping CRI log read for unsafe job_id");
            continue;
        }
        let sh_n = db::sh_index_for_event(&sh_events, &ev.job_id, idx);
        let key = (ev.job_id.clone(), sh_n);
        if log_contents.contains_key(&key) {
            continue;
        }
        let log_path = runs_base
            .join(&run_id)
            .join("jobs")
            .join(&ev.job_id)
            .join(format!("sh-{sh_n}.log"));
        match fs_err::tokio::read_to_string(&log_path).await {
            Ok(content) => {
                log_contents.insert(key, content);
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => {
                tracing::warn!(path = %log_path.display(), error = %e, "failed to read CRI log");
            }
        }
    }

    let mut detail_jobs: Vec<DetailJob> = Vec::new();
    for job in &jobs {
        let job_shs: Vec<(usize, &db::ShEvent)> = sh_events
            .iter()
            .enumerate()
            .filter(|(_, e)| e.job_id == job.job_id)
            .collect();

        let mut detail_sh_events: Vec<DetailShEvent> = Vec::new();
        for (global_idx, ev) in &job_shs {
            let sh_n = db::sh_index_for_event(&sh_events, &ev.job_id, *global_idx);

            let log = log_contents
                .get(&(ev.job_id.clone(), sh_n))
                .cloned()
                .unwrap_or_default();

            detail_sh_events.push(DetailShEvent {
                index: sh_n,
                started_at_ms: ev.started_at_ms,
                finished_at_ms: ev.finished_at_ms,
                exit_code: ev.exit_code,
                cmd: ev.cmd.clone(),
                log_content: log,
            });
        }

        detail_jobs.push(DetailJob {
            job_id: job.job_id.clone(),
            state: job.state.clone(),
            exit_code: job.exit_code,
            started_at_ms: job.started_at_ms,
            finished_at_ms: job.finished_at_ms,
            sh_events: detail_sh_events,
        });
    }

    let tmpl = RunDetailTemplate {
        repo: repo_display,
        page: format!("ci · {}", detail_run.sha_short()),
        run: detail_run,
        jobs: detail_jobs,
    };
    render(&tmpl)
}
