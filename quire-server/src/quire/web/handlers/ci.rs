//! Handlers for CI run list and run detail pages.

use axum::extract::{Path as AxumPath, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};

use super::super::db;
use super::super::templates::{
    Crumb, DetailJob, DetailRun, DetailShEvent, RunDetailTemplate, RunListRow, RunListTemplate,
};
use super::git::RepoView;
use super::{render, render_error};
use crate::Quire;

pub async fn run_list(State(quire): State<Quire>, AxumPath(repo): AxumPath<String>) -> Response {
    let repo_display = repo.trim_end_matches(".git").to_string();
    let repo_name = db::resolve_repo_name(&repo);
    let git_repo = match quire.repo(&repo_name) {
        Ok(r) if r.exists() => r,
        _ => return StatusCode::NOT_FOUND.into_response(),
    };

    let q = quire.clone();
    let rn = repo_name.clone();
    let runs_handle = tokio::task::spawn_blocking(move || db::load_runs(&q, &rn));
    let refs_handle = tokio::task::spawn_blocking(move || {
        let r = RepoView::new(&git_repo);
        (r.bookmarks(), r.tags())
    });

    let runs = match runs_handle.await {
        Ok(Ok(r)) => r,
        Ok(Err(e)) => {
            tracing::error!(repo = %repo, error = &e as &(dyn std::error::Error + 'static), "failed to load runs");
            return render_error(
                repo_display,
                StatusCode::INTERNAL_SERVER_ERROR,
                "Failed to load runs",
                e.to_string(),
            );
        }
        Err(_) => {
            tracing::error!("spawn_blocking task panicked");
            return StatusCode::INTERNAL_SERVER_ERROR.into_response();
        }
    };
    let (bookmarks, tags) = refs_handle.await.unwrap_or_else(|e| {
        tracing::warn!(error = %e, "refs task panicked");
        Default::default()
    });

    let template_runs: Vec<RunListRow> = runs
        .into_iter()
        .map(|r| RunListRow {
            id: r.id,
            outcome: r.outcome,
            sha: r.sha,
            ref_name: r.ref_name,
            created_at: r.created_at,
            dispatched_at: r.dispatched_at,
            resolved_at: r.resolved_at,
        })
        .collect();

    let tmpl = RunListTemplate {
        repo: repo_display,
        crumbs: vec![],
        runs: template_runs,
        bookmarks,
        tags,
        active_section: "ci".to_string(),
    };
    render(&tmpl)
}

pub async fn run_detail(
    State(quire): State<Quire>,
    AxumPath((repo, run_id)): AxumPath<(String, String)>,
) -> Response {
    let repo_display = repo.trim_end_matches(".git").to_string();
    let repo_name = db::resolve_repo_name(&repo);
    let git_repo = match quire.repo(&repo_name) {
        Ok(r) if r.exists() => r,
        _ => return StatusCode::NOT_FOUND.into_response(),
    };
    if !db::is_valid_run_id(&run_id) {
        return StatusCode::NOT_FOUND.into_response();
    }

    let refs_handle = tokio::task::spawn_blocking(move || {
        let r = RepoView::new(&git_repo);
        (r.bookmarks(), r.tags())
    });

    let q = quire.clone();
    let rn = repo_name.clone();
    let ri = run_id.clone();
    let detail = match tokio::task::spawn_blocking(move || db::load_run_detail(&q, &rn, &ri)).await
    {
        Ok(Ok(d)) => d,
        Ok(Err(ref e)) if is_no_rows(e) => return StatusCode::NOT_FOUND.into_response(),
        Ok(Err(e)) => {
            tracing::error!(repo = %repo, run_id = %run_id, error = &e as &(dyn std::error::Error + 'static), "failed to load run detail");
            return render_error(
                repo_display,
                StatusCode::INTERNAL_SERVER_ERROR,
                "Failed to load run",
                e.to_string(),
            );
        }
        Err(_) => {
            tracing::error!("spawn_blocking task panicked");
            return StatusCode::INTERNAL_SERVER_ERROR.into_response();
        }
    };

    let detail_run = DetailRun {
        outcome: detail.run.outcome,
        sha: detail.run.sha,
        ref_name: detail.run.ref_name,
        created_at: detail.run.created_at,
        dispatched_at: detail.run.dispatched_at,
        resolved_at: detail.run.resolved_at,
    };

    // Group sh events by job_id, preserving DB order so positional index
    // matches launch order.
    let mut events_by_job: std::collections::HashMap<&str, Vec<&db::ShEvent>> =
        std::collections::HashMap::new();
    for ev in &detail.sh_events {
        events_by_job.entry(&ev.job_id).or_default().push(ev);
    }

    let runs_base = quire.base_dir().join("runs").join(&repo_name);
    let run_dir = runs_base.join(&run_id);
    let job_dir_base = run_dir.join("jobs");

    // Spawn quire-ci.log read concurrently with the per-sh log reads below.
    let quire_ci_log_path = run_dir.join("quire-ci.log");
    let quire_ci_log_handle: tokio::task::JoinHandle<String> =
        tokio::spawn(async move { read_log(&quire_ci_log_path).await });

    // Build a flat list of log paths so we can issue all reads concurrently
    // and reassemble in order.
    let mut log_handles: Vec<tokio::task::JoinHandle<String>> = Vec::new();

    for job in &detail.jobs {
        let job_events = events_by_job
            .get(job.job_id.as_str())
            .map(Vec::as_slice)
            .unwrap_or(&[]);
        let job_dir = db::is_safe_path_segment(&job.job_id).then(|| job_dir_base.join(&job.job_id));
        if job_dir.is_none() && !job_events.is_empty() {
            tracing::warn!(job_id = %job.job_id, "skipping CRI log reads for unsafe job_id");
        }

        for (i, _ev) in job_events.iter().enumerate() {
            let sh_n = i + 1;
            match &job_dir {
                Some(dir) => {
                    let path = dir.join(format!("sh-{sh_n}.log"));
                    log_handles.push(tokio::spawn(async move { read_log(&path).await }));
                }
                None => {
                    log_handles.push(tokio::spawn(async { String::new() }));
                }
            }
        }
    }

    // Await all spawned reads in spawn order to preserve the index mapping.
    let mut log_results: Vec<String> = Vec::with_capacity(log_handles.len());
    for handle in log_handles {
        log_results.push(handle.await.unwrap_or_else(|e| {
            tracing::warn!(error = %e, "log read task panicked");
            String::new()
        }));
    }

    // Reassemble: walk jobs/events in the same order, pulling from log_results.
    let mut log_idx = 0;
    let mut detail_jobs: Vec<DetailJob> = Vec::with_capacity(detail.jobs.len());
    for job in &detail.jobs {
        let job_events = events_by_job
            .get(job.job_id.as_str())
            .map(Vec::as_slice)
            .unwrap_or(&[]);

        let mut detail_sh_events: Vec<DetailShEvent> = Vec::with_capacity(job_events.len());
        for ev in job_events {
            detail_sh_events.push(DetailShEvent {
                started_at_ms: ev.started_at_ms,
                finished_at_ms: ev.finished_at_ms,
                exit_code: ev.exit_code,
                cmd: ev.cmd.clone(),
                log_content: log_results[log_idx].clone(),
            });
            log_idx += 1;
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

    let quire_ci_log = quire_ci_log_handle.await.unwrap_or_else(|e| {
        tracing::warn!(error = %e, "quire-ci.log read task panicked");
        String::new()
    });
    let (bookmarks, tags) = refs_handle.await.unwrap_or_else(|e| {
        tracing::warn!(error = %e, "refs task panicked");
        Default::default()
    });

    let crumbs = vec![
        Crumb::with_href("ci", format!("/{}/ci", repo_display)),
        Crumb::new(detail_run.sha_short()),
    ];
    let tmpl = RunDetailTemplate {
        repo: repo_display,
        crumbs,
        run: detail_run,
        jobs: detail_jobs,
        quire_ci_log,
        bookmarks,
        tags,
        active_section: "ci".to_string(),
    };
    render(&tmpl)
}

fn is_no_rows(err: &crate::error::Error) -> bool {
    matches!(
        err,
        crate::error::Error::Sql(rusqlite::Error::QueryReturnedNoRows)
    )
}

async fn read_log(path: &std::path::Path) -> String {
    match fs_err::tokio::read_to_string(path).await {
        Ok(content) => content,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => String::new(),
        Err(e) => {
            tracing::warn!(path = %path.display(), error = &e as &(dyn std::error::Error + 'static), "failed to read CRI log");
            String::new()
        }
    }
}
