//! Auth middleware for the web view.

use axum::http::StatusCode;
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};

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
