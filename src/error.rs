use miette::Diagnostic;

#[derive(Debug, thiserror::Error, Diagnostic)]
pub enum Error {
    #[error("not found: {0}")]
    NotFound(String),

    #[error("io error: {0}")]
    Io(#[from] std::io::Error),

    #[error("secret resolution failed: {0}")]
    SecretResolve(String),

    #[error("config not found: {0}")]
    ConfigNotFound(String),

    #[error(transparent)]
    Fennel(#[from] crate::fennel::FennelError),

    #[error("CI validation failed: {}", .0.iter().map(|e| e.message.clone()).collect::<Vec<_>>().join("; "))]
    Validation(Vec<crate::ci::ValidationError>),

    #[error("lua error: {0}")]
    Lua(String),

    #[error("git error: {0}")]
    Git(String),

    #[allow(dead_code)]
    #[error("event socket error: {0}")]
    EventSocket(String),
}

pub type Result<T> = std::result::Result<T, Error>;

impl From<Vec<crate::ci::ValidationError>> for Error {
    fn from(errs: Vec<crate::ci::ValidationError>) -> Self {
        Error::Validation(errs)
    }
}
