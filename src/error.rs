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

    #[error("git error: {0}")]
    Git(String),
}

pub type Result<T> = std::result::Result<T, Error>;
