//! Route handlers for the web view.

use askama::Template;
use axum::extract::{Path as AxumPath, State};
use axum::response::Html;

use super::auth::RemoteUser;
use super::db;
use super::templates::*;
use crate::Quire;

pub async fn run_list(
    State(quire): State<Quire>,
    AxumPath(repo): AxumPath<String>,
    user: RemoteUser,
) -> Html<String> {
    let _user = user;
    let repo_display = repo.trim_end_matches(".git").to_string();
    let repo_name = db::resolve_repo_name(&repo);

    let runs = match db::load_runs(&quire, &repo_name) {
        Ok(r) => r,
        Err(e) => {
            tracing::error!(repo = %repo, error = %e, "failed to load runs");
            let tmpl = ErrorTemplate {
                repo: repo_display,
                page: "error".to_string(),
                title: "Failed to load runs".to_string(),
                detail: e,
            };
            return Html(tmpl.render().unwrap_or_default());
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
    Html(tmpl.render().unwrap_or_default())
}

pub async fn run_detail(
    State(quire): State<Quire>,
    AxumPath((repo, run_id)): AxumPath<(String, String)>,
    user: RemoteUser,
) -> Html<String> {
    let _user = user;
    let repo_display = repo.trim_end_matches(".git").to_string();
    let repo_name = db::resolve_repo_name(&repo);

    let result = db::load_run_detail(&quire, &repo_name, &run_id);
    let (run, jobs, sh_events) = match result {
        Ok(d) => d,
        Err(e) => {
            tracing::error!(repo = %repo, run_id = %run_id, error = %e, "failed to load run detail");
            let tmpl = ErrorTemplate {
                repo: repo_display,
                page: "error".to_string(),
                title: "Failed to load run".to_string(),
                detail: e,
            };
            return Html(tmpl.render().unwrap_or_default());
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
        if log_path.exists() {
            match fs_err::read_to_string(&log_path) {
                Ok(content) => {
                    log_contents.insert(key, content);
                }
                Err(e) => {
                    tracing::warn!(path = %log_path.display(), error = %e, "failed to read CRI log");
                }
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
    Html(tmpl.render().unwrap_or_default())
}
