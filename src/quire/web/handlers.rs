//! Route handlers for the web view.

use askama::Template;
use axum::extract::{Path as AxumPath, State};
use axum::http::{StatusCode, header};
use axum::response::{Html, IntoResponse, Redirect, Response};

use super::db;
use super::templates::*;
use crate::Quire;
use crate::error::display_chain;

pub async fn repo_redirect(
    State(quire): State<Quire>,
    AxumPath(repo): AxumPath<String>,
) -> Response {
    let repo_name = db::resolve_repo_name(&repo);
    match quire.repo(&repo_name) {
        Ok(r) if r.exists() => {}
        _ => return StatusCode::NOT_FOUND.into_response(),
    }
    Redirect::temporary(&format!("/{}/ci", repo.trim_end_matches(".git"))).into_response()
}

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

/// Serve the compiled-in stylesheet.
pub async fn stylesheet() -> Response {
    (
        [(header::CONTENT_TYPE, "text/css; charset=utf-8")],
        include_str!("../../../static/style.css"),
    )
        .into_response()
}

/// True when the error is "query returned no rows" (i.e. resource not found).
fn is_no_rows(err: &crate::error::Error) -> bool {
    matches!(
        err,
        crate::error::Error::Sql(rusqlite::Error::QueryReturnedNoRows)
    )
}

/// Read a CRI log file, returning empty on NotFound and on any other
/// error after logging it.
async fn read_log(path: &std::path::Path) -> String {
    match fs_err::tokio::read_to_string(path).await {
        Ok(content) => content,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => String::new(),
        Err(e) => {
            tracing::warn!(path = %path.display(), error = %e, "failed to read CRI log");
            String::new()
        }
    }
}

/// Render the error template with the given status, falling back to plain
/// text if the error template itself fails to render.
fn render_error(repo: String, status: StatusCode, title: &str, detail: String) -> Response {
    let tmpl = ErrorTemplate {
        repo,
        crumbs: vec![Crumb::new("error")],
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
    match quire.repo(&repo_name) {
        Ok(r) if r.exists() => {}
        _ => return StatusCode::NOT_FOUND.into_response(),
    };

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
        crumbs: vec![Crumb::new("ci")],
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
    match quire.repo(&repo_name) {
        Ok(r) if r.exists() => {}
        _ => return StatusCode::NOT_FOUND.into_response(),
    };
    if !db::is_valid_run_id(&run_id) {
        return StatusCode::NOT_FOUND.into_response();
    }

    let result = db::load_run_detail(&quire, &repo_name, &run_id);
    let detail = match result {
        Ok(d) => d,
        Err(ref e) if is_no_rows(e) => return StatusCode::NOT_FOUND.into_response(),
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
        state: detail.run.state,
        sha: detail.run.sha,
        ref_name: detail.run.ref_name,
        queued_at_ms: detail.run.queued_at_ms,
        started_at_ms: detail.run.started_at_ms,
        finished_at_ms: detail.run.finished_at_ms,
    };

    // Group sh events by job_id, preserving DB order so positional index
    // matches launch order.
    let mut events_by_job: std::collections::HashMap<&str, Vec<&db::ShEvent>> =
        std::collections::HashMap::new();
    for ev in &detail.sh_events {
        events_by_job.entry(&ev.job_id).or_default().push(ev);
    }

    let runs_base = quire.base_dir().join("runs").join(&repo_name);
    let job_dir_base = runs_base.join(&run_id).join("jobs");

    let mut detail_jobs: Vec<DetailJob> = Vec::with_capacity(detail.jobs.len());
    for job in &detail.jobs {
        let job_events = events_by_job
            .get(job.job_id.as_str())
            .map(Vec::as_slice)
            .unwrap_or(&[]);
        let job_dir = db::is_safe_path_segment(&job.job_id).then(|| job_dir_base.join(&job.job_id));
        if job_dir.is_none() {
            tracing::warn!(job_id = %job.job_id, "skipping CRI log reads for unsafe job_id");
        }

        let mut detail_sh_events: Vec<DetailShEvent> = Vec::with_capacity(job_events.len());
        for (i, ev) in job_events.iter().enumerate() {
            let sh_n = i + 1;
            let log_content = match &job_dir {
                Some(dir) => read_log(&dir.join(format!("sh-{sh_n}.log"))).await,
                None => String::new(),
            };
            detail_sh_events.push(DetailShEvent {
                started_at_ms: ev.started_at_ms,
                finished_at_ms: ev.finished_at_ms,
                exit_code: ev.exit_code,
                cmd: ev.cmd.clone(),
                log_content,
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

    let crumbs = vec![
        Crumb::with_href("ci", format!("/{}/ci", repo_display)),
        Crumb::new(detail_run.sha_short()),
    ];
    let tmpl = RunDetailTemplate {
        repo: repo_display,
        crumbs,
        run: detail_run,
        jobs: detail_jobs,
    };
    render(&tmpl)
}

#[cfg(test)]
mod tests {
    use axum::body::Body;
    use axum::http::{Request, StatusCode};
    use tower::ServiceExt;

    use crate::Quire;

    /// Build a test axum Router with the CI routes, backed by a tempdir.
    struct TestEnv {
        _dir: tempfile::TempDir,
        quire: Quire,
    }

    impl TestEnv {
        fn new() -> Self {
            let dir = tempfile::tempdir().expect("tempdir");
            let quire = Quire::new(dir.path().to_path_buf());

            // Create repos dir + a bare repo so `quire.repo("example.git")` resolves.
            let repos_dir = quire.repos_dir();
            let bare = repos_dir.join("example.git");
            fs_err::create_dir_all(&bare).expect("mkdir bare repo");

            // Initialise DB with migrations.
            let mut db = crate::db::open(&quire.db_path()).expect("db open");
            crate::db::migrate(&mut db).expect("migrate");
            drop(db);

            Self { _dir: dir, quire }
        }

        fn insert_run(
            &self,
            id: &str,
            state: &str,
            sha: &str,
            ref_name: &str,
            queued: i64,
            started: Option<i64>,
            finished: Option<i64>,
        ) {
            let pool = self.quire.db_pool();
            let db = pool.lock().expect("lock");
            db.execute(
                "INSERT INTO runs (id, repo, ref_name, sha, pushed_at_ms, state, failure_kind,
                                  queued_at_ms, started_at_ms, finished_at_ms,
                                  container_id, image_tag, build_started_at_ms, build_finished_at_ms,
                                  container_started_at_ms, container_stopped_at_ms, workspace_path)
                 VALUES (?1, 'example.git', ?2, ?3, ?4, ?5, NULL, ?4, ?6, ?7, NULL, NULL, NULL, NULL, NULL, NULL, '/tmp/ws')",
                rusqlite::params![id, ref_name, sha, queued, state, started, finished],
            ).expect("insert run");
        }

        fn insert_job(
            &self,
            run_id: &str,
            job_id: &str,
            state: &str,
            exit_code: Option<i32>,
            started: Option<i64>,
            finished: Option<i64>,
        ) {
            let pool = self.quire.db_pool();
            let db = pool.lock().expect("lock");
            db.execute(
                "INSERT INTO jobs (run_id, job_id, state, exit_code, started_at_ms, finished_at_ms)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
                rusqlite::params![run_id, job_id, state, exit_code, started, finished],
            )
            .expect("insert job");
        }

        fn app(&self) -> axum::Router {
            super::super::router(self.quire.clone())
        }
    }

    const UUID1: &str = "aaaaaaaa-0000-0000-0000-000000000001";
    const SHA1: &str = "deadbeefdeadbeefdeadbeefdeadbeefdeadbeef";

    #[tokio::test]
    async fn repo_redirect_strips_git_and_redirects() {
        let env = TestEnv::new();
        let app = env.app();
        let req = Request::builder()
            .uri("/example")
            .body(Body::empty())
            .unwrap();
        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::TEMPORARY_REDIRECT);
        let loc = resp.headers().get("location").unwrap().to_str().unwrap();
        assert_eq!(loc, "/example/ci");
    }

    #[tokio::test]
    async fn repo_redirect_strips_git_suffix() {
        let env = TestEnv::new();
        let app = env.app();
        let req = Request::builder()
            .uri("/example.git")
            .body(Body::empty())
            .unwrap();
        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::TEMPORARY_REDIRECT);
        let loc = resp.headers().get("location").unwrap().to_str().unwrap();
        assert_eq!(loc, "/example/ci");
    }

    #[tokio::test]
    async fn run_list_returns_ok_for_known_repo() {
        let env = TestEnv::new();
        env.insert_run(
            UUID1,
            "complete",
            SHA1,
            "refs/heads/main",
            1000,
            Some(2000),
            Some(3000),
        );
        let app = env.app();
        let req = Request::builder()
            .uri("/example/ci")
            .body(Body::empty())
            .unwrap();
        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
    }

    #[tokio::test]
    async fn run_list_returns_404_for_unknown_repo() {
        let env = TestEnv::new();
        let app = env.app();
        let req = Request::builder()
            .uri("/nonexistent/ci")
            .body(Body::empty())
            .unwrap();
        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn run_detail_returns_ok_for_existing_run() {
        let env = TestEnv::new();
        env.insert_run(
            UUID1,
            "complete",
            SHA1,
            "refs/heads/main",
            1000,
            Some(2000),
            Some(3000),
        );
        env.insert_job(UUID1, "build", "complete", Some(0), Some(2000), Some(3000));
        let app = env.app();
        let req = Request::builder()
            .uri(&format!("/example/ci/{UUID1}"))
            .body(Body::empty())
            .unwrap();
        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
    }

    #[tokio::test]
    async fn run_detail_returns_404_for_invalid_id() {
        let env = TestEnv::new();
        let app = env.app();
        let req = Request::builder()
            .uri("/example/ci/not-a-uuid")
            .body(Body::empty())
            .unwrap();
        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn run_detail_returns_404_for_missing_run() {
        let env = TestEnv::new();
        let app = env.app();
        // Valid UUID but no run exists — should return 404, not 500.
        let req = Request::builder()
            .uri(&format!("/example/ci/{UUID1}"))
            .body(Body::empty())
            .unwrap();
        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::NOT_FOUND);
    }
}
