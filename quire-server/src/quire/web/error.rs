//! Web handler error type, and how 5xx causes reach the request log.
//!
//! Error types never log their own failures. A 5xx response carries its
//! cause as a [`ResponseError`] extension, and the HTTP trace layer logs every
//! 5xx exactly once — with that cause when present, and without it when not,
//! so a 500 can't pass silently or be logged twice.

use std::sync::Arc;

use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};

/// The cause of a 5xx response, carried as a response extension for the
/// trace layer to log.
#[derive(Clone, Debug)]
pub struct ResponseError(pub Arc<dyn std::error::Error + Send + Sync + 'static>);

impl ResponseError {
    /// Build a 500 response carrying `error` as its cause.
    pub fn internal(error: impl std::error::Error + Send + Sync + 'static) -> Response {
        let mut response = StatusCode::INTERNAL_SERVER_ERROR.into_response();
        response
            .extensions_mut()
            .insert(ResponseError(Arc::new(error)));
        response
    }
}

/// Log a 5xx response once, with its cause when one was attached.
///
/// Intended for the trace layer's `on_response`; other statuses are ignored.
pub fn log_server_error(response: &Response) {
    let status = response.status();
    if !status.is_server_error() {
        return;
    }
    match response.extensions().get::<ResponseError>() {
        Some(ResponseError(error)) => tracing::error!(
            status = status.as_u16(),
            error = &**error as &(dyn std::error::Error + 'static),
            "request failed",
        ),
        None => tracing::error!(
            status = status.as_u16(),
            "request failed with no recorded cause",
        ),
    }
}

#[derive(Debug, thiserror::Error)]
pub enum WebError {
    #[error(transparent)]
    Internal(#[from] crate::Error),

    #[error(transparent)]
    TaskPanic(#[from] tokio::task::JoinError),
}

impl IntoResponse for WebError {
    fn into_response(self) -> Response {
        match self {
            Self::Internal(
                crate::Error::RepoNotFound(_)
                | crate::Error::Sql(rusqlite::Error::QueryReturnedNoRows),
            ) => StatusCode::NOT_FOUND.into_response(),
            Self::Internal(_) | Self::TaskPanic(_) => ResponseError::internal(self),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn internal_error_attaches_cause() {
        let err = WebError::Internal(crate::Error::Sql(rusqlite::Error::InvalidQuery));
        let response = err.into_response();
        assert_eq!(response.status(), StatusCode::INTERNAL_SERVER_ERROR);
        let ResponseError(cause) = response
            .extensions()
            .get::<ResponseError>()
            .expect("cause attached");
        assert!(cause.to_string().contains("Query is not read-only"));
    }

    #[test]
    fn not_found_attaches_nothing() {
        let err = WebError::Internal(crate::Error::RepoNotFound("x.git".into()));
        let response = err.into_response();
        assert_eq!(response.status(), StatusCode::NOT_FOUND);
        assert!(response.extensions().get::<ResponseError>().is_none());
    }
}
