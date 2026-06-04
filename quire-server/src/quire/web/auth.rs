//! Auth middleware and extractor for the web view.

use std::convert::Infallible;

use axum::extract::FromRequestParts;
use axum::http::StatusCode;
use axum::http::request::Parts;
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};

/// Extractor that resolves to `true` when the `Remote-User` header is present.
pub struct Auth(pub bool);

impl<S: Send + Sync> FromRequestParts<S> for Auth {
    type Rejection = Infallible;

    async fn from_request_parts(parts: &mut Parts, _state: &S) -> Result<Self, Self::Rejection> {
        Ok(Auth(parts.headers.contains_key("Remote-User")))
    }
}

/// Dev-only middleware that injects a synthetic `Remote-User` header so the
/// `Auth` extractor behaves as if a real user is present.
#[cfg(feature = "dev")]
pub async fn inject_dev_user(mut request: axum::extract::Request, next: Next) -> Response {
    request
        .headers_mut()
        .insert("Remote-User", axum::http::HeaderValue::from_static("dev"));
    next.run(request).await
}

/// Middleware that rejects unauthenticated requests.
///
/// CI routes require auth per the access matrix in PLAN.md.
/// Returns 401 so the client knows auth is required.
pub async fn require_auth(request: axum::extract::Request, next: Next) -> Response {
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
    use axum::body::Body;
    use axum::http::{Request, StatusCode};
    use axum::routing::get;
    use tower::ServiceExt;

    use super::require_auth;

    async fn ok_handler() -> &'static str {
        "ok"
    }

    fn test_app() -> axum::Router {
        axum::Router::new()
            .route("/", get(ok_handler))
            .layer(axum::middleware::from_fn(require_auth))
    }

    #[tokio::test]
    async fn require_auth_rejects_missing_header() {
        let app = test_app();
        let req = Request::builder().uri("/").body(Body::empty()).unwrap();
        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
    }

    #[tokio::test]
    async fn require_auth_allows_valid_header() {
        let app = test_app();
        let req = Request::builder()
            .uri("/")
            .header("Remote-User", "alice")
            .body(Body::empty())
            .unwrap();
        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
    }
}
