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
