//! Read-only CI web view.
//!
//! Two pages:
//! - `GET /repo/<name>/ci` — most-recent runs for a repo.
//! - `GET /repo/<name>/ci/<run-id>` — per-run detail with jobs and logs.
//!
//! Server-rendered HTML via Askama templates. JavaScript-optional.

use askama::Template;
use axum::extract::{FromRequestParts, Path as AxumPath, State};
use axum::http::StatusCode;
use axum::http::request::Parts;
use axum::middleware::{self, Next};
use axum::response::{Html, IntoResponse, Response};
use jiff::Timestamp;
use rusqlite::Connection;

use crate::Quire;

// ── Template structs ───────────────────────────────────────────────

#[derive(askama::Template)]
#[template(path = "ci/run_list.html")]
struct RunListTemplate {
    repo: String,
    page: String,
    runs: Vec<RunListRow>,
}

struct RunListRow {
    id: String,
    state_color: String,
    sha_short: String,
    ref_short: String,
    queued: String,
    duration: String,
}

#[derive(askama::Template)]
#[template(path = "ci/run_detail.html")]
struct RunDetailTemplate {
    repo: String,
    page: String,
    run: DetailRun,
    jobs: Vec<DetailJob>,
}

struct DetailRun {
    state: String,
    state_color: String,
    sha_short: String,
    ref_short: String,
    queued: String,
    started: String,
    finished: String,
    duration: String,
}

struct DetailJob {
    job_id: String,
    state: String,
    state_color: String,
    duration: String,
    exit_str: String,
    sh_events: Vec<DetailShEvent>,
}

struct DetailShEvent {
    index: usize,
    duration: String,
    exit_code: i32,
    cmd_display: String,
    log_content: String,
}

#[derive(askama::Template)]
#[template(path = "error.html")]
struct ErrorTemplate {
    repo: String,
    page: String,
    title: String,
    detail: String,
}

// ── Data access structs (from DB rows) ─────────────────────────────

struct RunRow {
    id: String,
    state: String,
    sha: String,
    ref_name: String,
    queued_at_ms: i64,
    started_at_ms: Option<i64>,
    finished_at_ms: Option<i64>,
}

struct JobRow {
    job_id: String,
    state: String,
    exit_code: Option<i32>,
    started_at_ms: Option<i64>,
    finished_at_ms: Option<i64>,
}

struct ShEvent {
    job_id: String,
    started_at_ms: i64,
    finished_at_ms: i64,
    exit_code: i32,
    cmd: String,
}

// ── Auth ───────────────────────────────────────────────────────────

/// Identity extracted from the `Remote-User` header injected by the
/// reverse proxy. Present means authenticated; absent means
/// unauthenticated. Both are valid — individual handlers (or future
/// middleware) decide whether to require auth.
#[derive(Clone, Debug)]
pub struct RemoteUser(pub Option<String>);

impl RemoteUser {
    /// Whether the request carries an authenticated identity.
    pub fn is_authenticated(&self) -> bool {
        self.0.is_some()
    }

    /// The username, if authenticated.
    pub fn username(&self) -> Option<&str> {
        self.0.as_deref()
    }
}

impl<S: Send + Sync> FromRequestParts<S> for RemoteUser {
    type Rejection = std::convert::Infallible;

    async fn from_request_parts(parts: &mut Parts, _state: &S) -> Result<Self, Self::Rejection> {
        let user = parts
            .headers
            .get("Remote-User")
            .and_then(|v| v.to_str().ok())
            .map(|s| s.to_string());
        Ok(RemoteUser(user))
    }
}

fn state_color(state: &str) -> &'static str {
    match state {
        "complete" => "c-ok",
        "failed" => "c-bad",
        _ => "c-muted",
    }
}

fn format_timestamp(ms: i64) -> String {
    match Timestamp::from_millisecond(ms) {
        Ok(ts) => {
            let now = Timestamp::now();
            let span = now.since(ts).unwrap_or_else(|_| jiff::Span::new());
            let hours = span.get_hours().abs();
            let minutes = span.get_minutes().abs();
            if hours < 1 {
                if minutes < 1 {
                    "just now".to_string()
                } else {
                    format!("{minutes}m ago")
                }
            } else if hours < 24 {
                format!("{hours}h ago")
            } else {
                ts.to_string()
            }
        }
        Err(_) => format!("{ms}ms"),
    }
}

fn format_duration(start: Option<i64>, end: Option<i64>) -> String {
    match (start, end) {
        (Some(s), Some(e)) => {
            let ms = e - s;
            if ms < 1000 {
                format!("{ms}ms")
            } else {
                format!("{}s", ms / 1000)
            }
        }
        _ => "—".to_string(),
    }
}

fn format_duration_exact(start: i64, end: i64) -> String {
    let ms = end - start;
    if ms < 1000 {
        format!("{ms}ms")
    } else {
        format!("{}s", ms / 1000)
    }
}

// ── Data loading ───────────────────────────────────────────────────

fn load_runs(quire: &Quire, repo: &str) -> Result<Vec<RunRow>, String> {
    let db = Connection::open(quire.db_path()).map_err(|e| e.to_string())?;
    let mut stmt = db
        .prepare(
            "SELECT id, state, sha, ref_name, queued_at_ms, started_at_ms, finished_at_ms
             FROM runs WHERE repo = ?1
             ORDER BY queued_at_ms DESC
             LIMIT 50",
        )
        .map_err(|e| e.to_string())?;

    let rows = stmt
        .query_map(rusqlite::params![repo], |row| {
            Ok(RunRow {
                id: row.get(0)?,
                state: row.get(1)?,
                sha: row.get(2)?,
                ref_name: row.get(3)?,
                queued_at_ms: row.get(4)?,
                started_at_ms: row.get(5)?,
                finished_at_ms: row.get(6)?,
            })
        })
        .map_err(|e| e.to_string())?
        .collect::<Result<Vec<_>, _>>()
        .map_err(|e| e.to_string())?;

    Ok(rows)
}

fn load_run_detail(
    quire: &Quire,
    repo: &str,
    run_id: &str,
) -> Result<(RunRow, Vec<JobRow>, Vec<ShEvent>), String> {
    let db = Connection::open(quire.db_path()).map_err(|e| e.to_string())?;

    let run = db
        .query_row(
            "SELECT id, state, sha, ref_name, queued_at_ms, started_at_ms, finished_at_ms
             FROM runs WHERE id = ?1 AND repo = ?2",
            rusqlite::params![run_id, repo],
            |row| {
                Ok(RunRow {
                    id: row.get(0)?,
                    state: row.get(1)?,
                    sha: row.get(2)?,
                    ref_name: row.get(3)?,
                    queued_at_ms: row.get(4)?,
                    started_at_ms: row.get(5)?,
                    finished_at_ms: row.get(6)?,
                })
            },
        )
        .map_err(|e| e.to_string())?;

    let mut job_stmt = db
        .prepare(
            "SELECT job_id, state, exit_code, started_at_ms, finished_at_ms
             FROM jobs WHERE run_id = ?1
             ORDER BY rowid",
        )
        .map_err(|e| e.to_string())?;

    let jobs = job_stmt
        .query_map(rusqlite::params![run_id], |row| {
            Ok(JobRow {
                job_id: row.get(0)?,
                state: row.get(1)?,
                exit_code: row.get(2)?,
                started_at_ms: row.get(3)?,
                finished_at_ms: row.get(4)?,
            })
        })
        .map_err(|e| e.to_string())?
        .collect::<Result<Vec<_>, _>>()
        .map_err(|e| e.to_string())?;

    let mut sh_stmt = db
        .prepare(
            "SELECT job_id, started_at_ms, finished_at_ms, exit_code, cmd
             FROM sh_events WHERE run_id = ?1
             ORDER BY job_id, started_at_ms",
        )
        .map_err(|e| e.to_string())?;

    let sh_events = sh_stmt
        .query_map(rusqlite::params![run_id], |row| {
            Ok(ShEvent {
                job_id: row.get(0)?,
                started_at_ms: row.get(1)?,
                finished_at_ms: row.get(2)?,
                exit_code: row.get(3)?,
                cmd: row.get(4)?,
            })
        })
        .map_err(|e| e.to_string())?
        .collect::<Result<Vec<_>, _>>()
        .map_err(|e| e.to_string())?;

    Ok((run, jobs, sh_events))
}

/// Determine the 1-based sh index for an event within its job.
fn sh_index_for_event(events: &[ShEvent], job_id: &str, event_idx: usize) -> usize {
    let mut n = 0;
    for (i, ev) in events.iter().enumerate() {
        if ev.job_id == job_id && i <= event_idx {
            n += 1;
        }
    }
    n
}

/// Resolve a URL slug to the on-disk repo name.
///
/// URLs use clean names (`foo`), disk/DB use `foo.git`.
fn resolve_repo_name(slug: &str) -> String {
    if slug.ends_with(".git") {
        slug.to_string()
    } else {
        format!("{slug}.git")
    }
}

// ── Handlers ───────────────────────────────────────────────────────

pub async fn run_list(
    State(quire): State<Quire>,
    AxumPath(repo): AxumPath<String>,
    user: RemoteUser,
) -> Html<String> {
    let _user = user;
    let repo_display = repo.trim_end_matches(".git").to_string();
    let repo_name = resolve_repo_name(&repo);

    let runs = match load_runs(&quire, &repo_name) {
        Ok(r) => r,
        Err(e) => {
            tracing::error!(repo = %repo, error = %e, "failed to load runs");
            let tmpl = ErrorTemplate {
                repo: repo_display.clone(),
                page: "error".to_string(),
                title: "Failed to load runs".to_string(),
                detail: e,
            };
            return Html(tmpl.render().unwrap_or_default());
        }
    };

    let template_runs: Vec<RunListRow> = runs
        .iter()
        .map(|r| RunListRow {
            id: r.id.clone(),
            state_color: state_color(&r.state).to_string(),
            sha_short: r.sha[..r.sha.len().min(8)].to_string(),
            ref_short: r.ref_name.trim_start_matches("refs/heads/").to_string(),
            queued: format_timestamp(r.queued_at_ms),
            duration: format_duration(r.started_at_ms, r.finished_at_ms),
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
    let repo_name = resolve_repo_name(&repo);

    let result = load_run_detail(&quire, &repo_name, &run_id);
    let (run, jobs, sh_events) = match result {
        Ok(d) => d,
        Err(e) => {
            tracing::error!(repo = %repo, run_id = %run_id, error = %e, "failed to load run detail");
            let tmpl = ErrorTemplate {
                repo: repo_display.clone(),
                page: "error".to_string(),
                title: "Failed to load run".to_string(),
                detail: e,
            };
            return Html(tmpl.render().unwrap_or_default());
        }
    };

    let sha_short = run.sha[..run.sha.len().min(8)].to_string();
    let detail_run = DetailRun {
        state: run.state.clone(),
        state_color: state_color(&run.state).to_string(),
        sha_short: sha_short.clone(),
        ref_short: run.ref_name.trim_start_matches("refs/heads/").to_string(),
        queued: format_timestamp(run.queued_at_ms),
        started: run.started_at_ms.map_or("—".to_string(), format_timestamp),
        finished: run.finished_at_ms.map_or("—".to_string(), format_timestamp),
        duration: format_duration(run.started_at_ms, run.finished_at_ms),
    };

    // Load CRI log contents for each sh event.
    let runs_base = quire.base_dir().join("runs").join(&repo_name);
    let mut log_contents: std::collections::HashMap<(String, usize), String> =
        std::collections::HashMap::new();
    for (idx, ev) in sh_events.iter().enumerate() {
        let sh_n = sh_index_for_event(&sh_events, &ev.job_id, idx);
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
        let job_shs: Vec<(usize, &ShEvent)> = sh_events
            .iter()
            .enumerate()
            .filter(|(_, e)| e.job_id == job.job_id)
            .collect();

        let mut detail_sh_events: Vec<DetailShEvent> = Vec::new();
        for (global_idx, ev) in &job_shs {
            let sh_n = sh_index_for_event(&sh_events, &ev.job_id, *global_idx);
            let cmd_display = if ev.cmd.len() > 120 {
                &ev.cmd[..120]
            } else {
                &ev.cmd
            };

            let log = log_contents
                .get(&(ev.job_id.clone(), sh_n))
                .map(|s| s.to_string())
                .unwrap_or_default();

            detail_sh_events.push(DetailShEvent {
                index: sh_n,
                duration: format_duration_exact(ev.started_at_ms, ev.finished_at_ms),
                exit_code: ev.exit_code,
                cmd_display: cmd_display.to_string(),
                log_content: log,
            });
        }

        detail_jobs.push(DetailJob {
            job_id: job.job_id.clone(),
            state: job.state.clone(),
            state_color: state_color(&job.state).to_string(),
            duration: format_duration(job.started_at_ms, job.finished_at_ms),
            exit_str: job
                .exit_code
                .map(|c| format!(" · exit {c}"))
                .unwrap_or_default(),
            sh_events: detail_sh_events,
        });
    }

    let tmpl = RunDetailTemplate {
        repo: repo_display,
        page: format!("ci · {sha_short}"),
        run: detail_run,
        jobs: detail_jobs,
    };
    Html(tmpl.render().unwrap_or_default())
}

// ── Router ─────────────────────────────────────────────────────────

pub fn router(quire: Quire) -> axum::Router {
    let ci_routes = axum::Router::new()
        .route("/{repo}/ci", axum::routing::get(run_list))
        .route("/{repo}/ci/{run_id}", axum::routing::get(run_detail))
        .layer(middleware::from_fn(require_auth));

    ci_routes.with_state(quire)
}

/// Middleware that rejects unauthenticated requests.
///
/// CI routes require auth per the access matrix in PLAN.md.
/// Returns 401 so the client knows auth is required.
async fn require_auth(request: axum::extract::Request, next: Next) -> Response {
    let user = request
        .headers()
        .get("Remote-User")
        .and_then(|v| v.to_str().ok());

    if user.is_none() {
        return StatusCode::UNAUTHORIZED.into_response();
    }

    next.run(request).await
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn run_list_template_renders_empty() {
        let tmpl = RunListTemplate {
            repo: "test.git".to_string(),
            page: "ci".to_string(),
            runs: vec![],
        };
        let html = tmpl.render().unwrap();
        assert!(html.contains("no runs yet"));
        assert!(html.contains("ci · test.git"));
    }

    #[test]
    fn run_list_template_renders_runs() {
        let tmpl = RunListTemplate {
            repo: "test.git".to_string(),
            page: "ci".to_string(),
            runs: vec![RunListRow {
                id: "abc123".to_string(),
                state_color: "c-ok".to_string(),
                sha_short: "deadbeef".to_string(),
                ref_short: "main".to_string(),
                queued: "just now".to_string(),
                duration: "1s".to_string(),
            }],
        };
        let html = tmpl.render().unwrap();
        assert!(html.contains("deadbeef"));
        assert!(html.contains("main"));
        assert!(html.contains("/test.git/ci/abc123"));
    }

    #[test]
    fn format_duration_shows_ms_for_subsecond() {
        assert_eq!(format_duration(Some(0), Some(500)), "500ms");
    }

    #[test]
    fn format_duration_shows_seconds() {
        assert_eq!(format_duration(Some(0), Some(3500)), "3s");
    }

    #[test]
    fn format_duration_dash_when_missing() {
        assert_eq!(format_duration(None, None), "—");
    }
}
