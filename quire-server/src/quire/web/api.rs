//! HTTP API endpoints for CI ↔ server communication.
//!
//! These routes use per-run bearer-token auth (not the Remote-User
//! header auth used by the web UI). Each token is minted when the run
//! is created and scoped to that run's ID.

use axum::extract::{Path as AxumPath, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum_extra::TypedHeader;
use axum_extra::headers::Authorization;
use axum_extra::headers::authorization::Bearer;

use crate::Quire;

/// Build the API router. Routes are not wrapped in web-UI auth
/// middleware; each handler verifies its own bearer token.
pub fn router(quire: Quire) -> axum::Router {
    axum::Router::new()
        .route(
            "/api/runs/{run_id}/secrets/{name}",
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

/// `GET /api/runs/:run_id/secrets/:name`
///
/// Returns the plain-text value of a named secret from the global config.
/// Auth: `Authorization: Bearer <token>` matching `runs.auth_token`.
/// Returns 404 if the run is unknown or the secret is not declared in config.
async fn get_secret(
    State(quire): State<Quire>,
    AxumPath((run_id, name)): AxumPath<(String, String)>,
    bearer: Option<TypedHeader<Authorization<Bearer>>>,
) -> Response {
    let Some(TypedHeader(Authorization(bearer))) = bearer else {
        return ApiError::Unauthorized.into_response();
    };
    let token = bearer.token().to_string();

    let result = tokio::task::spawn_blocking(move || -> Result<String, ApiError> {
        let db = crate::db::open(&quire.db_path())?;
        verify_token(&db, &run_id, &token)?;
        let config = quire.global_config()?;
        match config.secrets.get(&name) {
            Some(s) => Ok(s.reveal()?.to_string()),
            None => Err(ApiError::NotFound),
        }
    })
    .await;

    match result {
        Ok(Ok(value)) => (StatusCode::OK, value).into_response(),
        Ok(Err(e)) => e.into_response(),
        Err(_) => StatusCode::INTERNAL_SERVER_ERROR.into_response(),
    }
}

#[cfg(test)]
mod tests {
    use axum::body::Body;
    use axum::http::{Request, StatusCode};
    use tower::ServiceExt;

    use crate::Quire;
    use crate::ci::{RunMeta, Runs, Transport, TransportMode};

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
        let transport = Transport::for_new_run(TransportMode::Api, 3000);
        env.runs()
            .create(&TestEnv::meta(), &transport)
            .expect("create");
        let Transport::Api(ref session) = transport else {
            panic!("expected Api transport");
        };
        let url = format!("/api/runs/{}/secrets/my_secret", session.run_id);

        let resp = get(env.app(), &url, None).await;
        assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
    }

    #[tokio::test]
    async fn secret_returns_401_for_wrong_token() {
        let env = TestEnv::new();
        let transport = Transport::for_new_run(TransportMode::Api, 3000);
        env.runs()
            .create(&TestEnv::meta(), &transport)
            .expect("create");
        let Transport::Api(ref session) = transport else {
            panic!("expected Api transport");
        };
        let url = format!("/api/runs/{}/secrets/my_secret", session.run_id);

        let resp = get(env.app(), &url, Some("wrongtoken")).await;
        assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
    }

    #[tokio::test]
    async fn secret_returns_404_for_unknown_run() {
        let env = TestEnv::new();
        let resp = get(
            env.app(),
            "/api/runs/00000000-0000-0000-0000-000000000001/secrets/my_secret",
            Some("token"),
        )
        .await;
        assert_eq!(resp.status(), StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn secret_returns_404_for_unknown_secret_name() {
        let env = TestEnv::new();
        let transport = Transport::for_new_run(TransportMode::Api, 3000);
        env.runs()
            .create(&TestEnv::meta(), &transport)
            .expect("create");
        let Transport::Api(ref session) = transport else {
            panic!("expected Api transport");
        };
        let url = format!("/api/runs/{}/secrets/no_such_secret", session.run_id);

        let resp = get(env.app(), &url, Some(&session.auth_token)).await;
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
        let transport = Transport::for_new_run(TransportMode::Api, 3000);
        env.runs()
            .create(&TestEnv::meta(), &transport)
            .expect("create");
        let Transport::Api(ref session) = transport else {
            panic!("expected Api transport");
        };
        let url = format!("/api/runs/{}/secrets/my_token", session.run_id);

        let resp = get(env.app(), &url, Some(&session.auth_token)).await;
        assert_eq!(resp.status(), StatusCode::OK);

        use http_body_util::BodyExt;
        let body = resp.into_body().collect().await.unwrap().to_bytes();
        assert_eq!(body.as_ref(), b"hunter2");
    }
}
