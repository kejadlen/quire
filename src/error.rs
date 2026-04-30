use miette::Diagnostic;

use crate::ci::{LoadError, RunState};
use crate::fennel::FennelError;

#[derive(Debug, thiserror::Error, Diagnostic)]
pub enum Error {
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),

    // Stored as a string because `OnceLock` in `SecretString::reveal` caches
    // the error and `std::io::Error` is not `Clone`. See `secret.rs` for details.
    #[error("secret resolution failed: {0}")]
    SecretResolve(String),

    #[error("unknown secret: {0:?}")]
    UnknownSecret(String),

    #[error("config not found: {0}")]
    ConfigNotFound(String),

    #[error(transparent)]
    #[diagnostic(transparent)]
    Fennel(#[from] Box<FennelError>),

    #[error(transparent)]
    #[diagnostic(transparent)]
    Validation(Box<LoadError>),

    #[error("invalid run transition: {from:?} -> {to:?}")]
    InvalidTransition { from: RunState, to: RunState },

    #[error("job '{job}' failed")]
    JobFailed {
        job: String,
        #[source]
        source: Box<mlua::Error>,
    },

    #[error("git error: {0}")]
    Git(String),

    #[error(transparent)]
    Yaml(#[from] serde_yaml_ng::Error),

    #[error(transparent)]
    Utf8(#[from] std::string::FromUtf8Error),
}

pub type Result<T> = std::result::Result<T, Error>;

impl From<FennelError> for Error {
    fn from(err: FennelError) -> Self {
        Error::Fennel(Box::new(err))
    }
}

impl From<LoadError> for Error {
    fn from(err: LoadError) -> Self {
        Error::Validation(Box::new(err))
    }
}
