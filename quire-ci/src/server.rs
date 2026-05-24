use std::net::SocketAddr;

use axum::Router;
use axum::extract::State;
use axum::http::{HeaderMap, StatusCode};
use axum::response::IntoResponse;
use axum::routing::{get, post};
use hmac::{Hmac, KeyInit, Mac};
use quire_core::event::PushEvent;
use quire_core::telemetry::{self, FmtMode};
use sha2::Sha256;

use crate::quire::QuireCi;

const VERSION: &str = env!("QUIRE_VERSION");

async fn health() -> &'static str {
    "ok"
}

async fn index() -> String {
    format!("quire-ci {VERSION}\n")
}

enum WebhookError {
    Unauthorized,
    BadRequest,
    Internal,
}

impl IntoResponse for WebhookError {
    fn into_response(self) -> axum::response::Response {
        match self {
            WebhookError::Unauthorized => StatusCode::UNAUTHORIZED,
            WebhookError::BadRequest => StatusCode::BAD_REQUEST,
            WebhookError::Internal => StatusCode::INTERNAL_SERVER_ERROR,
        }
        .into_response()
    }
}

async fn webhook(
    State(quire): State<QuireCi>,
    headers: HeaderMap,
    body: axum::body::Bytes,
) -> std::result::Result<StatusCode, WebhookError> {
    let secret_bytes = quire
        .config()
        .webhook_secret
        .reveal()
        .map_err(|e| {
            tracing::error!(error = %e, "failed to resolve webhook secret");
            WebhookError::Internal
        })?
        .as_bytes()
        .to_vec();

    let auth_header = headers
        .get("Authorization")
        .and_then(|v| v.to_str().ok())
        .and_then(|s| s.strip_prefix("HMAC-SHA256 "))
        .ok_or(WebhookError::Unauthorized)?
        .to_string();

    let provided_bytes = hex::decode(&auth_header).map_err(|_| WebhookError::Unauthorized)?;

    let mut mac =
        Hmac::<Sha256>::new_from_slice(&secret_bytes).expect("HMAC accepts any key length");
    mac.update(&body);
    mac.verify_slice(&provided_bytes)
        .map_err(|_| WebhookError::Unauthorized)?;

    let event: PushEvent = serde_json::from_slice(&body).map_err(|_| WebhookError::BadRequest)?;

    let traceparent = headers
        .get("traceparent")
        .and_then(|v| v.to_str().ok())
        .map(|s| s.to_string());

    let conn = quire.db().lock();
    let now_ms = jiff::Timestamp::now().as_millisecond();

    for push_ref in event.updated_refs() {
        let id = uuid::Uuid::now_v7().to_string();
        conn.execute(
            r#"INSERT INTO runs (id, repo, "ref", sha, created_at, traceparent)
               VALUES (?1, ?2, ?3, ?4, ?5, ?6)"#,
            rusqlite::params![
                id,
                event.repo,
                push_ref.ref_name,
                push_ref.new_sha,
                now_ms,
                traceparent,
            ],
        )
        .map_err(|e| {
            tracing::error!(error = %e, "failed to insert run");
            WebhookError::Internal
        })?;
    }

    Ok(StatusCode::NO_CONTENT)
}

#[derive(Debug, thiserror::Error, miette::Diagnostic)]
pub enum Error {
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),

    #[error(transparent)]
    Secret(#[from] quire_core::secret::Error),

    #[error(transparent)]
    Telemetry(#[from] quire_core::telemetry::Error),
}

pub type Result<T> = std::result::Result<T, Error>;

pub async fn run(quire: QuireCi) -> Result<()> {
    let port = quire.config().port;

    let miette_layer = telemetry::MietteLayer::new();
    let _guard = telemetry::init_telemetry(
        miette_layer,
        FmtMode::AutoJson,
        quire.config().sentry.as_ref(),
        VERSION,
    )?;

    let app = Router::new()
        .route("/health", get(health))
        .route("/", get(index))
        .route("/webhook", post(webhook))
        .with_state(quire);

    let addr = SocketAddr::from(([0, 0, 0, 0], port));
    tracing::info!(%addr, "starting HTTP server");

    let listener = tokio::net::TcpListener::bind(addr).await?;

    axum::serve(listener, app).await?;

    Ok(())
}

#[cfg(test)]
mod tests {
    use axum::body::Body;
    use axum::http::{Request, StatusCode};
    use hmac::{Hmac, KeyInit, Mac};
    use sha2::Sha256;
    use tower::ServiceExt;

    use crate::quire::QuireCi;

    fn make_app(quire: QuireCi) -> axum::Router {
        axum::Router::new()
            .route("/webhook", axum::routing::post(super::webhook))
            .with_state(quire)
    }

    fn quire_with_secret(secret: &str) -> (tempfile::TempDir, QuireCi) {
        let dir = tempfile::tempdir().expect("tempdir");
        let config_path = dir.path().join("config.fnl");
        fs_err::write(&config_path, format!(r#"{{:webhook-secret "{secret}"}}"#))
            .expect("write config");
        let quire = QuireCi::new(dir.path().to_path_buf()).expect("QuireCi::new");
        (dir, quire)
    }

    fn push_event_body() -> Vec<u8> {
        serde_json::to_vec(&serde_json::json!({
            "type": "push",
            "repo": "test/repo.git",
            "pushed_at": "2026-05-01T00:00:00Z",
            "refs": [
                {
                    "ref_name": "refs/heads/main",
                    "old_sha": "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
                    "new_sha": "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb"
                }
            ]
        }))
        .expect("serialize")
    }

    fn hmac_header(secret: &str, body: &[u8]) -> String {
        let mut mac =
            Hmac::<Sha256>::new_from_slice(secret.as_bytes()).expect("HMAC accepts any key length");
        mac.update(body);
        let result = mac.finalize();
        format!("HMAC-SHA256 {}", hex::encode(result.into_bytes()))
    }

    #[tokio::test]
    async fn valid_hmac_creates_run_row() {
        let secret = "test-secret-key";
        let (_dir, quire) = quire_with_secret(secret);
        let db = quire.db().clone();
        let app = make_app(quire);

        let body = push_event_body();
        let auth = hmac_header(secret, &body);

        let req = Request::builder()
            .method("POST")
            .uri("/webhook")
            .header("Authorization", auth)
            .header("content-type", "application/json")
            .body(Body::from(body))
            .unwrap();

        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::NO_CONTENT);

        let count: i64 = db
            .lock()
            .query_row("SELECT COUNT(*) FROM runs", [], |row| row.get(0))
            .expect("count");
        assert_eq!(count, 1);
    }

    #[tokio::test]
    async fn wrong_hmac_returns_401() {
        let (_dir, quire) = quire_with_secret("correct-secret");
        let app = make_app(quire);

        let body = push_event_body();

        let req = Request::builder()
            .method("POST")
            .uri("/webhook")
            .header("Authorization", "HMAC-SHA256 deadbeefdeadbeef")
            .header("content-type", "application/json")
            .body(Body::from(body))
            .unwrap();

        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
    }
}
