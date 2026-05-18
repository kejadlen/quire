//! HTTP API endpoints for CI ↔ server communication.
//!
//! These routes use per-run bearer-token auth (not the Remote-User
//! header auth used by the web UI). Each token is minted when the run
//! is created and scoped to that run's ID.

use std::collections::HashMap;

use axum::extract::{FromRequestParts, Path as AxumPath, State};
use axum::http::StatusCode;
use axum::middleware::Next;
use axum::response::{IntoResponse, Response, Result};
use axum_extra::TypedHeader;
use axum_extra::headers::Authorization;
use axum_extra::headers::authorization::Bearer;

use crate::Quire;

/// Build the API router. Routes under `/runs/{run_id}` are wrapped in
/// [`verify_bearer`] middleware which authenticates the bearer token against
/// the run's stored token before any handler runs.
///
/// Intended to be mounted at `/api` via `Router::nest`.
pub fn router(quire: Quire) -> axum::Router {
    let run_routes = axum::Router::new()
        .route("/secrets/{name}", axum::routing::get(get_secret))
        .layer(axum::middleware::from_fn_with_state(
            quire.clone(),
            verify_bearer,
        ));

    axum::Router::new()
        .nest("/runs/{run_id}", run_routes)
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

/// Middleware that authenticates requests under `/runs/{run_id}` by verifying
/// the `Authorization: Bearer <token>` header against `runs.auth_token` in the
/// DB. Returns 401 if the header is absent or the token doesn't match, 404 if
/// the run doesn't exist.
async fn verify_bearer(
    State(quire): State<Quire>,
    req: axum::extract::Request,
    next: Next,
) -> Response {
    let (mut parts, body) = req.into_parts();

    let token =
        <TypedHeader<Authorization<Bearer>> as FromRequestParts<()>>::from_request_parts(
            &mut parts,
            &(),
        )
        .await
        .ok()
        .map(|TypedHeader(Authorization(bearer))| bearer.token().to_string());

    let run_id =
        <AxumPath<HashMap<String, String>> as FromRequestParts<()>>::from_request_parts(
            &mut parts,
            &(),
        )
        .await
        .ok()
        .and_then(|mut p| p.0.remove("run_id"));

    let req = axum::extract::Request::from_parts(parts, body);

    let Some(token) = token else {
        return StatusCode::UNAUTHORIZED.into_response();
    };

    let Some(run_id) = run_id else {
        return StatusCode::NOT_FOUND.into_response();
    };

    let result = tokio::task::spawn_blocking(move || -> Result<(), ApiError> {
        let db = quire
            .db_pool()
            .lock()
            .map_err(|_| crate::Error::Io(std::io::Error::other("db mutex poisoned")))?;
        let stored: Option<String> = db
            .query_row(
                "SELECT auth_token FROM runs WHERE id = ?1",
                rusqlite::params![run_id],
                |row| row.get(0),
            )
            .map_err(ApiError::from)?;
        match stored {
            Some(ref t) if t == &token => Ok(()),
            _ => Err(ApiError::Unauthorized),
        }
    })
    .await
    .expect("blocking task panicked");

    match result {
        Ok(()) => next.run(req).await,
        Err(e) => e.into_response(),
    }
}

/// `GET /api/runs/:run_id/secrets/:name`
///
/// Returns the plain-text value of a named secret from the global config.
/// Auth is handled by [`verify_bearer`] middleware before this handler runs.
/// Returns 404 if the secret is not declared in config.
async fn get_secret(
    State(quire): State<Quire>,
    AxumPath(params): AxumPath<HashMap<String, String>>,
) -> Result<axum::Json<serde_json::Value>, ApiError> {
    let name = params.get("name").cloned().unwrap_or_default();
    let value = tokio::task::spawn_blocking(move || -> std::result::Result<String, ApiError> {
        let config = quire.global_config()?;
        match config.secrets.get(&name) {
            Some(s) => Ok(s.reveal()?.to_string()),
            None => Err(ApiError::NotFound),
        }
    })
    .await
    .expect("blocking task panicked")?;

    Ok(axum::Json(serde_json::json!({ "value": value })))
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
        let parsed: serde_json::Value = serde_json::from_slice(&body).expect("json body");
        assert_eq!(parsed["value"], "hunter2");
    }
}
