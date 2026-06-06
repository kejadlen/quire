//! Web handler error type.

use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};

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
            Self::Internal(_) | Self::TaskPanic(_) => {
                StatusCode::INTERNAL_SERVER_ERROR.into_response()
            }
        }
    }
}
