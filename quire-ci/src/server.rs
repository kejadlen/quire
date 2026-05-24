use std::net::SocketAddr;

use axum::Router;
use axum::extract::{MatchedPath, State};
use axum::http::{HeaderMap, Request, StatusCode};
use axum::response::IntoResponse;
use axum::routing::{get, post};
use axum_extra::TypedHeader;
use axum_extra::headers::Authorization;
use axum_extra::headers::authorization::Credentials;
use hmac::{Hmac, KeyInit, Mac};
use quire_core::event::PushEvent;
use quire_core::telemetry::{self, FmtMode};
use sha2::Sha256;
use tower_http::trace::TraceLayer;
use tracing::info_span;

use crate::quire::QuireCi;

const VERSION: &str = env!("QUIRE_VERSION");

async fn health() -> &'static str {
    "ok"
}

async fn index() -> String {
    format!("quire-ci {VERSION}\n")
}

#[derive(Debug, thiserror::Error)]
enum WebhookError {
    #[error("missing or malformed Authorization header")]
    MissingSignature,
    #[error("signature mismatch")]
    InvalidSignature,
    #[error(transparent)]
    InvalidPayload(#[from] serde_json::Error),
    #[error(transparent)]
    Db(#[from] rusqlite::Error),
}

impl From<hmac::digest::MacError> for WebhookError {
    fn from(_: hmac::digest::MacError) -> Self {
        Self::InvalidSignature
    }
}

impl IntoResponse for WebhookError {
    fn into_response(self) -> axum::response::Response {
        match self {
            WebhookError::MissingSignature | WebhookError::InvalidSignature => {
                StatusCode::UNAUTHORIZED
            }
            WebhookError::InvalidPayload(_) => StatusCode::BAD_REQUEST,
            WebhookError::Db(_) => StatusCode::INTERNAL_SERVER_ERROR,
        }
        .into_response()
    }
}

struct HmacSha256Sig(Vec<u8>);

impl Credentials for HmacSha256Sig {
    const SCHEME: &'static str = "HMAC-SHA256";

    fn decode(value: &axum::http::HeaderValue) -> Option<Self> {
        let hex_str = value.to_str().ok()?.strip_prefix("HMAC-SHA256 ")?;
        hex::decode(hex_str).ok().map(Self)
    }

    fn encode(&self) -> axum::http::HeaderValue {
        axum::http::HeaderValue::from_str(&format!("HMAC-SHA256 {}", hex::encode(&self.0)))
            .expect("hex is always a valid header value")
    }
}

struct HmacSha256Auth(Vec<u8>);

impl<S: Send + Sync> axum::extract::FromRequestParts<S> for HmacSha256Auth {
    type Rejection = WebhookError;

    async fn from_request_parts(
        parts: &mut axum::http::request::Parts,
        state: &S,
    ) -> std::result::Result<Self, WebhookError> {
        use axum::extract::FromRequestParts;

        let Some(TypedHeader(Authorization(sig))) =
            <TypedHeader<Authorization<HmacSha256Sig>> as FromRequestParts<S>>::from_request_parts(
                parts, state,
            )
            .await
            .ok()
        else {
            return Err(WebhookError::MissingSignature);
        };
        Ok(Self(sig.0))
    }
}

async fn webhook(
    State(quire): State<QuireCi>,
    HmacSha256Auth(provided_bytes): HmacSha256Auth,
    headers: HeaderMap,
    body: axum::body::Bytes,
) -> std::result::Result<StatusCode, WebhookError> {
    let mut mac =
        Hmac::<Sha256>::new_from_slice(quire.hmac_key()).expect("HMAC accepts any key length");
    mac.update(&body);
    mac.verify_slice(&provided_bytes)?;

    let event: PushEvent = serde_json::from_slice(&body)
        .inspect_err(|e| tracing::warn!(error = %e, "invalid webhook payload"))?;

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
        .inspect_err(|e| tracing::error!(error = %e, "database error"))?;
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
        .layer(
            TraceLayer::new_for_http().make_span_with(|request: &Request<_>| {
                let matched_path = request
                    .extensions()
                    .get::<MatchedPath>()
                    .map(MatchedPath::as_str);
                info_span!("http_request", method = ?request.method(), matched_path)
            }),
        )
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
                    "ref": "refs/heads/main",
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
