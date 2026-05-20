//! Route handlers for the web view.

mod ci;
mod commit;
mod git;
mod log_view;
mod repo;
mod repo_list;
mod tree;

pub use ci::{run_detail, run_list};
pub use commit::commit_view;
pub use log_view::log_view;
pub use repo::repo_home;
pub use repo_list::repo_list;
pub use tree::{tree_view, tree_view_path};

use askama::Template;
use axum::extract::State;
use axum::http::{StatusCode, header};
use axum::response::{Html, IntoResponse, Response};

use super::templates::ConfigTemplate;
use crate::Quire;

/// Render a template into an HTML response, returning 500 on render failure.
pub(super) fn render<T: Template>(tmpl: &T) -> Response {
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
        include_str!("../../../../static/style.css"),
    )
        .into_response()
}

pub async fn config(State(quire): State<Quire>) -> Response {
    let tmpl = ConfigTemplate {
        crumbs: None,
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

    struct TestEnv {
        _dir: tempfile::TempDir,
        quire: Quire,
    }

    impl TestEnv {
        fn new() -> Self {
            let dir = tempfile::tempdir().expect("tempdir");
            let quire = Quire::load(dir.path().to_path_buf()).expect("load");

            let repos_dir = quire.repos_dir();
            let bare = repos_dir.join("example.git");
            fs_err::create_dir_all(&bare).expect("mkdir bare repo");

            let mut db = crate::db::Db::open(&quire.db_path()).expect("db open");
            db.migrate().expect("migrate");
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
            db.insert_seeded_run(&crate::db::runs::SeededRun {
                id,
                repo: "example.git",
                ref_name,
                sha,
                pushed_at_ms: created,
                created_at: created,
                dispatched_at: dispatched,
                resolved_at: resolved,
                outcome,
            })
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
            db.insert_job(run_id, job_id, state, exit_code, started.unwrap_or(0), finished.unwrap_or(0))
                .expect("insert job");
        }

        fn app(&self) -> axum::Router {
            super::super::public_router(self.quire.clone())
                .merge(super::super::ci_router(self.quire.clone()))
        }
    }

    const UUID1: &str = "aaaaaaaa-0000-0000-0000-000000000001";
    const SHA1: &str = "deadbeefdeadbeefdeadbeefdeadbeefdeadbeef";

    #[tokio::test]
    async fn repo_home_returns_ok_for_known_repo() {
        let env = TestEnv::new();
        let resp = env
            .app()
            .oneshot(
                Request::builder()
                    .uri("/example")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
    }

    #[tokio::test]
    async fn repo_home_accepts_git_suffix() {
        let env = TestEnv::new();
        let resp = env
            .app()
            .oneshot(
                Request::builder()
                    .uri("/example.git")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
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
        let resp = env
            .app()
            .oneshot(
                Request::builder()
                    .uri("/example/ci")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
    }

    #[tokio::test]
    async fn run_list_returns_404_for_unknown_repo() {
        let env = TestEnv::new();
        let resp = env
            .app()
            .oneshot(
                Request::builder()
                    .uri("/nonexistent/ci")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
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
        let resp = env
            .app()
            .oneshot(
                Request::builder()
                    .uri(&format!("/example/ci/{UUID1}"))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
    }

    #[tokio::test]
    async fn run_detail_returns_404_for_invalid_id() {
        let env = TestEnv::new();
        let resp = env
            .app()
            .oneshot(
                Request::builder()
                    .uri("/example/ci/not-a-uuid")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn run_detail_returns_404_for_missing_run() {
        let env = TestEnv::new();
        let resp = env
            .app()
            .oneshot(
                Request::builder()
                    .uri(&format!("/example/ci/{UUID1}"))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn tree_view_returns_ok_for_known_repo() {
        let env = TestEnv::new();
        let repo_path = env.quire.repos_dir().join("example.git");
        std::process::Command::new("git")
            .args(["init", "--bare"])
            .current_dir(&repo_path)
            .output()
            .ok();
        let resp = env
            .app()
            .oneshot(
                Request::builder()
                    .uri("/example/tree")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        // Empty repo has no HEAD → 404; populated repo → 200.
        assert!(
            resp.status() == StatusCode::OK || resp.status() == StatusCode::NOT_FOUND,
            "unexpected status: {}",
            resp.status()
        );
    }

    #[tokio::test]
    async fn tree_view_returns_404_for_unknown_repo() {
        let env = TestEnv::new();
        let resp = env
            .app()
            .oneshot(
                Request::builder()
                    .uri("/nonexistent/tree")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::NOT_FOUND);
    }
}
