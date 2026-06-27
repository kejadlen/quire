//! Handlers for CI run list and run detail pages.

use axum::extract::State;
use axum::response::Response;

use super::super::db;
use super::super::error::WebError;
use super::super::templates::{
    self, DetailJob, DetailRun, DetailShEvent, RunListRow, nav_sections,
};
use super::render;
use crate::Quire;
use crate::quire::web::paths::{RunDetailPath, RunListPath};

pub async fn run_list(
    RunListPath { repo }: RunListPath,
    State(quire): State<Quire>,
) -> Result<Response, WebError> {
    let repo_display = repo.trim_end_matches(".git").to_string();
    let repo_name = db::resolve_repo_name(&repo);
    quire.repo(&repo_name)?;

    let q = quire.clone();
    let rn = repo_name.clone();
    let runs = tokio::task::spawn_blocking(move || db::load_runs(&q, &rn)).await??;

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

    let sections = nav_sections(&repo_display, "ci", true);
    Ok(render(templates::run_list(
        &repo_display,
        None,
        &template_runs,
        &sections,
    )))
}

pub async fn run_detail(
    RunDetailPath { repo, run_id }: RunDetailPath,
    State(quire): State<Quire>,
) -> Result<Response, WebError> {
    let repo_display = repo.trim_end_matches(".git").to_string();
    let repo_name = db::resolve_repo_name(&repo);
    quire.repo(&repo_name)?;

    let q = quire.clone();
    let rn = repo_name.clone();
    let ri = run_id.clone();
    let detail = tokio::task::spawn_blocking(move || db::load_run_detail(&q, &rn, &ri)).await??;

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
        log_results.push(handle.await.unwrap_or_default());
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

    let quire_ci_log = quire_ci_log_handle.await.unwrap_or_default();

    let sections = nav_sections(&repo_display, "ci", true);
    Ok(render(templates::run_detail(
        &repo_display,
        None,
        &detail_run,
        &detail_jobs,
        &quire_ci_log,
        &sections,
    )))
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
