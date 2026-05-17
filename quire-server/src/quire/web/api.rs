//! HTTP API endpoints for CI ↔ server communication.
//!
//! These routes use per-run bearer-token auth (not the Remote-User
//! header auth used by the web UI). Each token is minted when the run
//! is created and scoped to that run's ID.

use std::path::PathBuf;

use axum::extract::{Path as AxumPath, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response, Result};
use axum_extra::TypedHeader;
use axum_extra::headers::Authorization;
use axum_extra::headers::authorization::Bearer;
use jiff::Timestamp;

use crate::Quire;

/// Build the API router. Routes are not wrapped in web-UI auth
/// middleware; each handler verifies its own bearer token.
///
/// Intended to be mounted at `/api` via `Router::nest`.
pub fn router(quire: Quire) -> axum::Router {
    axum::Router::new()
        .route(
            "/runs/{run_id}/bootstrap",
            axum::routing::get(get_bootstrap),
        )
        .route(
            "/runs/{run_id}/secrets/{name}",
            axum::routing::get(get_secret),
        )
        .with_state(quire)
}

#[derive(Debug, thiserror::Error)]
enum ApiError {
    #[error("not found")]
    NotFound,
    #[error("unauthorized")]
    Unauthorized,
    #[error("already fetched")]
    AlreadyFetched,
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
            ApiError::AlreadyFetched => StatusCode::GONE.into_response(),
            e => {
                tracing::error!(error = %e, "api error");
                StatusCode::INTERNAL_SERVER_ERROR.into_response()
            }
        }
    }
}

/// Verify the bearer token against the stored `auth_token` for `run_id`.
/// Returns `Err(NotFound)` if the run doesn't exist, `Err(Unauthorized)` if
/// the token doesn't match (including a null token for filesystem-mode runs).
fn verify_token(db: &rusqlite::Connection, run_id: &str, token: &str) -> Result<(), ApiError> {
    let stored: Option<String> = db
        .query_row(
            "SELECT auth_token FROM runs WHERE id = ?1",
            rusqlite::params![run_id],
            |row| row.get(0),
        )
        .map_err(ApiError::from)?;
    match stored {
        Some(ref t) if t == token => Ok(()),
        _ => Err(ApiError::Unauthorized),
    }
}

/// `GET /api/runs/:run_id/bootstrap`
///
/// Returns the bootstrap payload for the run: push metadata, the bare
/// repo path, and the Sentry handoff (when configured). One-shot: sets
/// `bootstrap_fetched_at_ms` on first successful read; subsequent
/// requests get 410 Gone.
///
/// Auth: `Authorization: Bearer <token>` matching `runs.auth_token`.
async fn get_bootstrap(
    State(quire): State<Quire>,
    AxumPath(run_id): AxumPath<String>,
    bearer: Option<TypedHeader<Authorization<Bearer>>>,
) -> Result<axum::Json<quire_core::ci::bootstrap::Bootstrap>, ApiError> {
    let Some(TypedHeader(Authorization(bearer))) = bearer else {
        return Err(ApiError::Unauthorized);
    };
    let token = bearer.token().to_string();

    let bootstrap = tokio::task::spawn_blocking(move || -> std::result::Result<_, ApiError> {
        let db = quire
            .db_pool()
            .lock()
            .map_err(|_| crate::Error::Io(std::io::Error::other("db mutex poisoned")))?;

        let row: (String, String, String, i64, Option<String>, Option<i64>) = db
            .query_row(
                "SELECT repo, sha, ref_name, pushed_at_ms, auth_token, bootstrap_fetched_at_ms
                     FROM runs WHERE id = ?1",
                rusqlite::params![run_id],
                |row| {
                    Ok((
                        row.get(0)?,
                        row.get(1)?,
                        row.get(2)?,
                        row.get(3)?,
                        row.get(4)?,
                        row.get(5)?,
                    ))
                },
            )
            .map_err(ApiError::from)?;

        let (repo, sha, ref_name, pushed_at_ms, stored_token, fetched_at_ms) = row;

        // Verify bearer token.
        match stored_token {
            Some(ref t) if t == &token => {}
            _ => return Err(ApiError::Unauthorized),
        }

        // One-shot: reject if already fetched.
        if fetched_at_ms.is_some() {
            return Err(ApiError::AlreadyFetched);
        }

        // Atomically claim the dispatch slot.
        let now = Timestamp::now().as_millisecond();
        let updated = db
            .execute(
                "UPDATE runs SET bootstrap_fetched_at_ms = ?1
                     WHERE id = ?2 AND bootstrap_fetched_at_ms IS NULL",
                rusqlite::params![now, run_id],
            )
            .map_err(ApiError::Db)?;

        if updated == 0 {
            return Err(ApiError::AlreadyFetched);
        }

        // Load config for sentry.
        let config = quire.global_config()?;

        let sentry = config.sentry.as_ref().and_then(|s| match s.dsn.reveal() {
            Ok(dsn) => Some(quire_core::ci::bootstrap::SentryHandoff {
                dsn: dsn.to_string(),
                trace_id: String::new(),
            }),
            Err(e) => {
                tracing::warn!(
                    run_id = %run_id,
                    error = %e,
                    "failed to reveal sentry DSN; omitting from bootstrap"
                );
                None
            }
        });

        let git_dir: PathBuf = quire.repos_dir().join(&repo);
        let meta = quire_core::ci::run::RunMeta {
            sha,
            r#ref: ref_name,
            pushed_at: Timestamp::from_millisecond(pushed_at_ms).map_err(|e| {
                ApiError::App(crate::Error::from(std::io::Error::other(e.to_string())))
            })?,
        };

        Ok(quire_core::ci::bootstrap::Bootstrap {
            meta,
            git_dir,
            sentry,
        })
    })
    .await
    .expect("blocking task panicked")?;

    Ok(axum::Json(bootstrap))
}

/// `GET /api/runs/:run_id/secrets/:name`
///
/// Returns the plain-text value of a named secret from the global config.
/// Auth: `Authorization: Bearer <token>` matching `runs.auth_token`.
/// Returns 404 if the run is unknown or the secret is not declared in config.
async fn get_secret(
    State(quire): State<Quire>,
    AxumPath((run_id, name)): AxumPath<(String, String)>,
    bearer: Option<TypedHeader<Authorization<Bearer>>>,
) -> Result<(StatusCode, String), ApiError> {
    let Some(TypedHeader(Authorization(bearer))) = bearer else {
        return Err(ApiError::Unauthorized);
    };
    let token = bearer.token().to_string();

    let value = tokio::task::spawn_blocking(move || -> std::result::Result<String, ApiError> {
        let db = quire
            .db_pool()
            .lock()
            .map_err(|_| crate::Error::Io(std::io::Error::other("db mutex poisoned")))?;
        verify_token(&db, &run_id, &token)?;
        let config = quire.global_config()?;
        match config.secrets.get(&name) {
            Some(s) => Ok(s.reveal()?.to_string()),
            None => Err(ApiError::NotFound),
        }
    })
    .await
    .expect("blocking task panicked")?;

    Ok((StatusCode::OK, value))
}

#[cfg(test)]
mod tests {
    use axum::body::Body;
    use axum::http::{Request, StatusCode};
    use tower::ServiceExt;

    use crate::Quire;
    use crate::ci::{RunMeta, Runs, TransportMode, new_transport};

    struct TestEnv {
        _dir: tempfile::TempDir,
        quire: Quire,
    }

    impl TestEnv {
        fn new() -> Self {
            let dir = tempfile::tempdir().expect("tempdir");
            let quire = Quire::new(dir.path().to_path_buf());
            let mut db = crate::db::open(&quire.db_path()).expect("db open");
            crate::db::migrate(&mut db).expect("migrate");
            drop(db);
            fs_err::write(quire.config_path(), "{}").expect("write config");
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

    #[tokio::test]
    async fn secret_returns_401_without_auth_header() {
        let env = TestEnv::new();
        let transport = new_transport(TransportMode::Api, 3000);
        env.runs()
            .create(&TestEnv::meta(), Some(&transport))
            .expect("create");
        let url = format!("/runs/{}/secrets/my_secret", transport.session.run_id);

        let resp = get(env.app(), &url, None).await;
        assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
    }

    #[tokio::test]
    async fn secret_returns_401_for_wrong_token() {
        let env = TestEnv::new();
        let transport = new_transport(TransportMode::Api, 3000);
        env.runs()
            .create(&TestEnv::meta(), Some(&transport))
            .expect("create");
        let url = format!("/runs/{}/secrets/my_secret", transport.session.run_id);

        let resp = get(env.app(), &url, Some("wrongtoken")).await;
        assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
    }

    #[tokio::test]
    async fn secret_returns_404_for_unknown_run() {
        let env = TestEnv::new();
        let resp = get(
            env.app(),
            "/runs/00000000-0000-0000-0000-000000000001/secrets/my_secret",
            Some("token"),
        )
        .await;
        assert_eq!(resp.status(), StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn secret_returns_404_for_unknown_secret_name() {
        let env = TestEnv::new();
        let transport = new_transport(TransportMode::Api, 3000);
        env.runs()
            .create(&TestEnv::meta(), Some(&transport))
            .expect("create");
        let url = format!("/runs/{}/secrets/no_such_secret", transport.session.run_id);

        let resp = get(env.app(), &url, Some(&transport.session.auth_token)).await;
        assert_eq!(resp.status(), StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn secret_returns_plaintext_value() {
        let env = TestEnv::new();
        // Write config with a secret.
        fs_err::write(
            env.quire.config_path(),
            r#"{:secrets {:my_token "hunter2"}}"#,
        )
        .expect("write config");
        let transport = new_transport(TransportMode::Api, 3000);
        env.runs()
            .create(&TestEnv::meta(), Some(&transport))
            .expect("create");
        let url = format!("/runs/{}/secrets/my_token", transport.session.run_id);

        let resp = get(env.app(), &url, Some(&transport.session.auth_token)).await;
        assert_eq!(resp.status(), StatusCode::OK);

        use http_body_util::BodyExt;
        let body = resp.into_body().collect().await.unwrap().to_bytes();
        assert_eq!(body.as_ref(), b"hunter2");
    }

    #[tokio::test]
    async fn bootstrap_returns_404_for_unknown_run() {
        let env = TestEnv::new();
        let app = env.app();
        let resp = get(
            app,
            "/runs/00000000-0000-0000-0000-000000000001/bootstrap",
            Some("token"),
        )
        .await;
        assert_eq!(resp.status(), StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn bootstrap_returns_401_without_auth_header() {
        let env = TestEnv::new();
        let transport = new_transport(TransportMode::Filesystem, 3000);
        env.runs()
            .create(&TestEnv::meta(), Some(&transport))
            .expect("create");
        let url = format!("/runs/{}/bootstrap", transport.session.run_id);

        let app = env.app();
        let resp = get(app, &url, None).await;
        assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
    }

    #[tokio::test]
    async fn bootstrap_returns_401_for_wrong_token() {
        let env = TestEnv::new();
        let transport = new_transport(TransportMode::Filesystem, 3000);
        env.runs()
            .create(&TestEnv::meta(), Some(&transport))
            .expect("create");
        let url = format!("/runs/{}/bootstrap", transport.session.run_id);

        let app = env.app();
        let resp = get(app, &url, Some("wrongtoken")).await;
        assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
    }

    #[tokio::test]
    async fn bootstrap_returns_json_on_first_fetch() {
        let env = TestEnv::new();
        let transport = new_transport(TransportMode::Filesystem, 3000);
        env.runs()
            .create(&TestEnv::meta(), Some(&transport))
            .expect("create");
        let url = format!("/runs/{}/bootstrap", transport.session.run_id);

        let app = env.app();
        let resp = get(app, &url, Some(&transport.session.auth_token)).await;
        assert_eq!(resp.status(), StatusCode::OK);

        use http_body_util::BodyExt;
        let body = resp.into_body().collect().await.unwrap().to_bytes();
        let bootstrap: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(
            bootstrap["meta"]["sha"],
            "abc1abc1abc1abc1abc1abc1abc1abc1abc1abc1"
        );
        assert_eq!(bootstrap["meta"]["ref"], "refs/heads/main");
    }

    #[tokio::test]
    async fn bootstrap_returns_410_on_second_fetch() {
        let env = TestEnv::new();
        let transport = new_transport(TransportMode::Filesystem, 3000);
        env.runs()
            .create(&TestEnv::meta(), Some(&transport))
            .expect("create");
        let url = format!("/runs/{}/bootstrap", transport.session.run_id);

        let app = env.app();
        let first = get(app.clone(), &url, Some(&transport.session.auth_token)).await;
        assert_eq!(first.status(), StatusCode::OK);

        let second = get(app, &url, Some(&transport.session.auth_token)).await;
        assert_eq!(second.status(), StatusCode::GONE);
    }
}
