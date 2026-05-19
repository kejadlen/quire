//! HTTP API endpoints for CI ↔ server communication.
//!
//! These routes use per-run bearer-token auth (not the Remote-User
//! header auth used by the web UI). Each token is minted when the run
//! is created and stored in `runs.run_token`. The bearer token itself
//! identifies the run — no run ID appears in the path.

use std::path::PathBuf;

use axum::extract::{FromRequestParts, State};
use axum::http::StatusCode;
use axum::middleware::Next;
use axum::response::{IntoResponse, Response, Result};
use axum_extra::TypedHeader;
use axum_extra::headers::Authorization;
use axum_extra::headers::authorization::Bearer;
use axum_extra::routing::{RouterExt, TypedPath};
use quire_core::ci::bootstrap::Bootstrap;
use quire_core::ci::run::RunMeta;

use crate::Quire;

/// Build the API router. Intended to be mounted at `/api` via `Router::nest`.
///
/// All routes are under `/run/…`. [`verify_run_token`] looks the run up by the bearer
/// token and injects the resolved run ID as a request extension before any handler runs.
pub fn router(quire: Quire) -> axum::Router {
    let run_routes = axum::Router::new()
        .route("/bootstrap", axum::routing::get(get_bootstrap))
        .typed_get(get_secret)
        .layer(axum::middleware::from_fn_with_state(
            quire.clone(),
            verify_run_token,
        ));

    axum::Router::new()
        .nest("/run", run_routes)
        .with_state(quire)
}

#[derive(Debug, thiserror::Error, miette::Diagnostic)]
enum ApiError {
    #[error("not found")]
    NotFound,
    #[error("unauthorized")]
    Unauthorized,
    #[error("gone")]
    Gone,
    #[error("internal error")]
    Internal(#[from] tokio::task::JoinError),
    #[error(transparent)]
    Db(rusqlite::Error),
    #[error(transparent)]
    App(#[from] crate::Error),
    #[error(transparent)]
    Secret(#[from] quire_core::secret::Error),
}

impl From<rusqlite::Error> for ApiError {
    fn from(e: rusqlite::Error) -> Self {
        match e {
            rusqlite::Error::QueryReturnedNoRows => ApiError::NotFound,
            _ => ApiError::Db(e),
        }
    }
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        match self {
            ApiError::NotFound => StatusCode::NOT_FOUND.into_response(),
            ApiError::Unauthorized => StatusCode::UNAUTHORIZED.into_response(),
            ApiError::Gone => StatusCode::GONE.into_response(),
            e => {
                tracing::error!(error = %e, "api error");
                StatusCode::INTERNAL_SERVER_ERROR.into_response()
            }
        }
    }
}

/// Middleware that looks up the run by the `Authorization: Bearer <token>` header
/// value against `runs.run_token`. Returns 401 if the header is absent or no run matches.
/// On success, injects the resolved run ID as a request extension so handlers can use it.
async fn verify_run_token(
    State(quire): State<Quire>,
    req: axum::extract::Request,
    next: Next,
) -> Result<Response, ApiError> {
    let (mut parts, body) = req.into_parts();

    let Some(TypedHeader(Authorization(bearer))) =
        <TypedHeader<Authorization<Bearer>> as FromRequestParts<()>>::from_request_parts(
            &mut parts,
            &(),
        )
        .await
        .ok()
    else {
        return Err(ApiError::Unauthorized);
    };
    let token = bearer.token().to_string();

    let run_id = tokio::task::spawn_blocking(move || -> Result<String, ApiError> {
        let db = quire.db_pool();
        match crate::db::runs::get_run_id_for_token(&db, &token) {
            Ok(id) => Ok(id),
            Err(rusqlite::Error::QueryReturnedNoRows) => Err(ApiError::Unauthorized),
            Err(e) => Err(ApiError::Db(e)),
        }
    })
    .await??;

    let mut req = axum::extract::Request::from_parts(parts, body);
    req.extensions_mut().insert(run_id);
    Ok(next.run(req).await)
}

/// `GET /api/run/bootstrap`
///
/// Returns the bootstrap payload for a run. One-shot: the server marks
/// bootstrap as fetched on the first successful read and returns 410 on
/// any subsequent call. Auth is handled by [`verify_run_token`] middleware.
///
/// Returns 404 if the run does not have API bootstrap data (e.g. the run
/// was created with filesystem transport and `store_bootstrap_data` was
/// never called).
async fn get_bootstrap(
    State(quire): State<Quire>,
    axum::Extension(run_id): axum::Extension<String>,
) -> Result<axum::Json<Bootstrap>, ApiError> {
    let bootstrap =
        tokio::task::spawn_blocking(move || -> std::result::Result<Bootstrap, ApiError> {
            let db = crate::db::open(&quire.db_path())?;

            let row =
                crate::db::runs::get_run_bootstrap_data(&db, &run_id)?.ok_or(ApiError::NotFound)?;

            if row.dispatched_at.is_some() {
                return Err(ApiError::Gone);
            }

            let git_dir: PathBuf = row.git_dir.map(PathBuf::from).ok_or(ApiError::NotFound)?;

            let meta = RunMeta {
                sha: row.sha,
                r#ref: row.ref_name,
                pushed_at: jiff::Timestamp::from_millisecond(row.pushed_at_ms)
                    .expect("db stores valid timestamps"),
            };

            let now_ms = jiff::Timestamp::now().as_millisecond();
            crate::db::runs::set_run_dispatched(&db, &run_id, now_ms)?;

            Ok(Bootstrap {
                meta,
                git_dir,
                repo: row.repo,
                run_id,
                traceparent: row.traceparent,
            })
        })
        .await??;

    Ok(axum::Json(bootstrap))
}

/// `GET /api/run/secrets/:name`
///
/// Returns the plain-text value of a named secret from the global config.
/// Auth is handled by [`verify_run_token`] middleware.
/// Returns 404 if the secret is not declared in config.
#[derive(TypedPath, serde::Deserialize)]
#[typed_path("/secrets/{name}")]
struct SecretPath {
    name: String,
}

async fn get_secret(
    SecretPath { name }: SecretPath,
    State(quire): State<Quire>,
) -> Result<axum::Json<serde_json::Value>, ApiError> {
    let value = tokio::task::spawn_blocking(move || -> std::result::Result<String, ApiError> {
        Ok(quire
            .config
            .secrets
            .get(&name)
            .ok_or(ApiError::NotFound)?
            .reveal()?
            .to_string())
    })
    .await??;

    Ok(axum::Json(serde_json::json!({ "value": value })))
}

#[cfg(test)]
mod tests {
    use axum::body::Body;
    use axum::http::{Request, StatusCode};
    use tower::ServiceExt;

    use crate::Quire;
    use crate::ci::{ApiSession, RunMeta, Runs};

    struct TestEnv {
        _dir: tempfile::TempDir,
        quire: Quire,
    }

    impl TestEnv {
        fn new() -> Self {
            let dir = tempfile::tempdir().expect("tempdir");
            let quire = Quire::load(dir.path().to_path_buf()).expect("load");
            let mut db = crate::db::open(&quire.db_path()).expect("db open");
            crate::db::migrate(&mut db).expect("migrate");
            drop(db);
            Self { _dir: dir, quire }
        }

        fn with_config_fnl(content: &str) -> Self {
            let dir = tempfile::tempdir().expect("tempdir");
            fs_err::write(dir.path().join("config.fnl"), content).expect("write config");
            let quire = crate::Quire::load(dir.path().to_path_buf()).expect("load config");
            let mut db = crate::db::open(&quire.db_path()).expect("db open");
            crate::db::migrate(&mut db).expect("migrate");
            drop(db);
            Self { _dir: dir, quire }
        }

        fn runs(&self) -> Runs {
            let base = self.quire.base_dir().join("runs").join("test.git");
            Runs::new(self.quire.db_path(), "test.git".to_string(), base)
        }

        fn meta() -> RunMeta {
            RunMeta {
                sha: "abc1".repeat(10),
                r#ref: "refs/heads/main".to_string(),
                pushed_at: "2026-05-01T00:00:00Z".parse().unwrap(),
            }
        }

        fn app(&self) -> axum::Router {
            super::router(self.quire.clone())
        }
    }

    async fn get(app: axum::Router, uri: &str, token: Option<&str>) -> axum::response::Response {
        let mut builder = Request::builder().uri(uri).method("GET");
        if let Some(t) = token {
            builder = builder.header("Authorization", format!("Bearer {t}"));
        }
        let req = builder.body(Body::empty()).unwrap();
        app.oneshot(req).await.unwrap()
    }

    async fn create_run_with_bootstrap(
        env: &TestEnv,
        session: &ApiSession,
        git_dir: &str,
        traceparent: Option<&str>,
    ) -> String {
        let run = env
            .runs()
            .create(&TestEnv::meta(), Some(session))
            .expect("create run");
        let run_id = run.id().to_string();

        let db = crate::db::open(&env.quire.db_path()).expect("db open");
        crate::db::runs::set_run_bootstrap_data(&db, &run_id, git_dir, traceparent)
            .expect("update bootstrap data");
        run_id
    }

    #[tokio::test]
    async fn bootstrap_returns_401_without_auth() {
        let env = TestEnv::new();
        let session = ApiSession::new(3000);
        create_run_with_bootstrap(&env, &session, "/repos/test.git", None).await;

        let resp = get(env.app(), "/run/bootstrap", None).await;
        assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
    }

    #[tokio::test]
    async fn bootstrap_returns_401_for_unknown_token() {
        let env = TestEnv::new();

        let resp = get(env.app(), "/run/bootstrap", Some("nosuchtoken")).await;
        assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
    }

    #[tokio::test]
    async fn bootstrap_returns_payload_on_first_fetch() {
        let env = TestEnv::new();
        let session = ApiSession::new(3000);
        let run_id = create_run_with_bootstrap(&env, &session, "/repos/test.git", None).await;

        let resp = get(env.app(), "/run/bootstrap", Some(&session.run_token)).await;
        assert_eq!(resp.status(), StatusCode::OK);

        use http_body_util::BodyExt;
        let body = resp.into_body().collect().await.unwrap().to_bytes();
        let parsed: serde_json::Value = serde_json::from_slice(&body).expect("json body");
        assert_eq!(parsed["git_dir"], "/repos/test.git");
        assert_eq!(parsed["repo"], "test.git");
        assert_eq!(parsed["run_id"], run_id);
    }

    #[tokio::test]
    async fn bootstrap_returns_410_on_second_fetch() {
        let env = TestEnv::new();
        let session = ApiSession::new(3000);
        create_run_with_bootstrap(&env, &session, "/repos/test.git", None).await;
        let token = &session.run_token;

        let first = get(env.app(), "/run/bootstrap", Some(token)).await;
        assert_eq!(first.status(), StatusCode::OK);

        let second = get(env.app(), "/run/bootstrap", Some(token)).await;
        assert_eq!(second.status(), StatusCode::GONE);
    }

    #[tokio::test]
    async fn secret_returns_401_without_auth() {
        let env = TestEnv::new();
        let session = ApiSession::new(3000);
        env.runs()
            .create(&TestEnv::meta(), Some(&session))
            .expect("create");

        let resp = get(env.app(), "/run/secrets/my_secret", None).await;
        assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
    }

    #[tokio::test]
    async fn secret_returns_plaintext_value() {
        let env = TestEnv::with_config_fnl(r#"{:secrets {:my_token "hunter2"}}"#);
        let session = ApiSession::new(3000);
        env.runs()
            .create(&TestEnv::meta(), Some(&session))
            .expect("create");

        let resp = get(env.app(), "/run/secrets/my_token", Some(&session.run_token)).await;
        assert_eq!(resp.status(), StatusCode::OK);

        use http_body_util::BodyExt;
        let body = resp.into_body().collect().await.unwrap().to_bytes();
        let parsed: serde_json::Value = serde_json::from_slice(&body).expect("json body");
        assert_eq!(parsed["value"], "hunter2");
    }
}
