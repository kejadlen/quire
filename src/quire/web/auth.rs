//! Auth middleware and identity extractor.

use axum::extract::FromRequestParts;
use axum::http::StatusCode;
use axum::http::request::Parts;
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};

/// Identity extracted from the `Remote-User` header injected by the
/// reverse proxy. Present means authenticated; absent means
/// unauthenticated. Both are valid — individual handlers (or future
/// middleware) decide whether to require auth.
#[derive(Clone, Debug)]
pub struct RemoteUser(pub Option<String>);

impl RemoteUser {
    /// Whether the request carries an authenticated identity.
    pub fn is_authenticated(&self) -> bool {
        self.0.is_some()
    }

    /// The username, if authenticated.
    pub fn username(&self) -> Option<&str> {
        self.0.as_deref()
    }
}

impl<S: Send + Sync> FromRequestParts<S> for RemoteUser {
    type Rejection = std::convert::Infallible;

    async fn from_request_parts(parts: &mut Parts, _state: &S) -> Result<Self, Self::Rejection> {
        let user = parts
            .headers
            .get("Remote-User")
            .and_then(|v| v.to_str().ok())
            .map(|s| s.to_string());
        Ok(RemoteUser(user))
    }
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
