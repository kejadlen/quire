//! Route handlers for the web view.

use askama::Template;
use axum::extract::{Path as AxumPath, State};
use axum::http::{StatusCode, header};
use axum::response::{Html, IntoResponse, Response};

use super::db;
use super::templates::*;
use crate::Quire;
use crate::quire::Repo;

pub async fn repo_home(State(quire): State<Quire>, AxumPath(repo): AxumPath<String>) -> Response {
    let repo_display = repo.trim_end_matches(".git").to_string();
    let repo_name = db::resolve_repo_name(&repo);
    let git_repo = match quire.repo(&repo_name) {
        Ok(r) if r.exists() => r,
        _ => return StatusCode::NOT_FOUND.into_response(),
    };

    // Load recent CI runs from DB.
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

    // Read git data (blocking).
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
        active_section: "readme".to_string(),
    };
    render(&tmpl)
}

type GitData = (
    Option<HeadInfo>,
    Option<String>,
    Vec<BookmarkRow>,
    Vec<TagRow>,
    Vec<ChangeRow>,
);

/// Read summary data from a bare git repository for the repo home page.
fn read_git_data(repo: &Repo) -> GitData {
    let head = read_head_info(repo);
    let readme_html = read_readme(repo);
    let bookmarks = read_bookmarks(repo);
    let tags = read_tags(repo);
    let recent_changes = read_recent_changes(repo);
    (head, readme_html, bookmarks, tags, recent_changes)
}

fn run_git(repo: &Repo, args: &[&str]) -> Option<String> {
    let output = repo.git(args).output().ok()?;
    if !output.status.success() {
        return None;
    }
    let s = String::from_utf8(output.stdout).ok()?;
    let s = s.trim().to_string();
    if s.is_empty() { None } else { Some(s) }
}

fn read_head_info(repo: &Repo) -> Option<HeadInfo> {
    let bookmark =
        run_git(repo, &["symbolic-ref", "--short", "HEAD"]).unwrap_or_else(|| "main".to_string());

    // %H = full sha, %s = subject, %ar = relative age
    let log = run_git(repo, &["log", "-1", "--format=%H%n%s%n%ar"])?;
    let mut lines = log.lines();
    let sha = lines.next()?.to_string();
    let description = lines.next().unwrap_or("").to_string();
    let age = lines.next().unwrap_or("").to_string();

    Some(HeadInfo {
        sha,
        description,
        age,
        bookmark,
    })
}

fn read_readme(repo: &Repo) -> Option<String> {
    // Try common README filenames.
    let candidates = ["HEAD:README.md", "HEAD:readme.md", "HEAD:README"];
    for candidate in &candidates {
        if let Some(raw) = run_git(repo, &["show", candidate]) {
            return Some(render_markdown(&raw));
        }
    }
    None
}

fn render_markdown(markdown: &str) -> String {
    use pulldown_cmark::{Options, Parser, html};
    let opts = Options::ENABLE_TABLES | Options::ENABLE_STRIKETHROUGH;
    let parser = Parser::new_ext(markdown, opts);
    let mut output = String::new();
    html::push_html(&mut output, parser);
    output
}

fn read_bookmarks(repo: &Repo) -> Vec<BookmarkRow> {
    let out = run_git(
        repo,
        &[
            "for-each-ref",
            "--format=%(refname:short)|%(objectname:short)|%(committerdate:relative)",
            "--sort=-committerdate",
            "refs/heads/",
        ],
    )
    .unwrap_or_default();

    out.lines()
        .filter_map(|line| {
            let mut parts = line.splitn(3, '|');
            Some(BookmarkRow {
                name: parts.next()?.to_string(),
                sha_short: parts.next()?.to_string(),
                age: parts.next().unwrap_or("").to_string(),
            })
        })
        .collect()
}

fn read_tags(repo: &Repo) -> Vec<TagRow> {
    let out = run_git(
        repo,
        &[
            "for-each-ref",
            "--format=%(refname:short)|%(committerdate:relative)",
            "--sort=-version:refname",
            "refs/tags/",
        ],
    )
    .unwrap_or_default();

    out.lines()
        .filter_map(|line| {
            let mut parts = line.splitn(2, '|');
            Some(TagRow {
                name: parts.next()?.to_string(),
                age: parts.next().unwrap_or("").to_string(),
            })
        })
        .collect()
}

fn read_recent_changes(repo: &Repo) -> Vec<ChangeRow> {
    let out = run_git(repo, &["log", "-12", "--format=%H|%s|%ar"]).unwrap_or_default();

    out.lines()
        .filter_map(|line| {
            let mut parts = line.splitn(3, '|');
            Some(ChangeRow {
                sha: parts.next()?.to_string(),
                description: parts.next().unwrap_or("").to_string(),
                age: parts.next().unwrap_or("").to_string(),
            })
        })
        .collect()
}

/// Render a template into an HTML response, returning 500 on render failure.
fn render<T: Template>(tmpl: &T) -> Response {
    match tmpl.render() {
        Ok(body) => Html(body).into_response(),
        Err(e) => {
            tracing::error!(
                error = &e as &(dyn std::error::Error + 'static),
                "template render failed"
            );
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
            tracing::warn!(path = %path.display(), error = &e as &(dyn std::error::Error + 'static), "failed to read CRI log");
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
            tracing::error!(
                error = &e as &(dyn std::error::Error + 'static),
                "error template render failed"
            );
            (status, format!("{title}\n\n{detail}\n")).into_response()
        }
    }
}

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
    let refs_handle =
        tokio::task::spawn_blocking(move || (read_bookmarks(&git_repo), read_tags(&git_repo)));

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
    let (bookmarks, tags) = refs_handle.await.unwrap_or_default();

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

    let refs_handle =
        tokio::task::spawn_blocking(move || (read_bookmarks(&git_repo), read_tags(&git_repo)));

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

    // Build a flat list of log paths keyed by (job index, event index)
    // so we can issue all reads concurrently and reassemble in order.
    //
    // tokio::spawn returns JoinHandle; awaiting handles in order preserves
    // spawn order while all tasks run concurrently.
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

    // Await all spawned reads — tasks run concurrently; awaiting handles
    // in spawn order preserves the index mapping.
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
    let (bookmarks, tags) = refs_handle.await.unwrap_or_default();

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

pub async fn config(State(quire): State<Quire>) -> Response {
    let tmpl = ConfigTemplate {
        crumbs: vec![Crumb::new("config")],
        config: quire.config.clone(),
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
            let quire = Quire::load(dir.path().to_path_buf()).expect("load");

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
            outcome: Option<&str>,
            sha: &str,
            ref_name: &str,
            created: i64,
            dispatched: Option<i64>,
            resolved: Option<i64>,
        ) {
            let db = self.quire.db_pool();
            db.execute(
                "INSERT INTO runs (id, repo, ref_name, sha, pushed_at_ms,
                                  created_at, dispatched_at, resolved_at, outcome)
                 VALUES (?1, 'example.git', ?2, ?3, ?4, ?4, ?5, ?6, ?7)",
                rusqlite::params![id, ref_name, sha, created, dispatched, resolved, outcome],
            )
            .expect("insert run");
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
            let db = self.quire.db_pool();
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
    async fn repo_home_returns_ok_for_known_repo() {
        let env = TestEnv::new();
        let app = env.app();
        let req = Request::builder()
            .uri("/example")
            .body(Body::empty())
            .unwrap();
        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
    }

    #[tokio::test]
    async fn repo_home_accepts_git_suffix() {
        let env = TestEnv::new();
        let app = env.app();
        let req = Request::builder()
            .uri("/example.git")
            .body(Body::empty())
            .unwrap();
        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
    }

    #[tokio::test]
    async fn run_list_returns_ok_for_known_repo() {
        let env = TestEnv::new();
        env.insert_run(
            UUID1,
            Some("succeeded"),
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
            Some("succeeded"),
            SHA1,
            "refs/heads/main",
            1000,
            Some(2000),
            Some(3000),
        );
        env.insert_job(UUID1, "build", "succeeded", Some(0), Some(2000), Some(3000));
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
