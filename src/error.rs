#[derive(Debug, thiserror::Error, miette::Diagnostic)]
pub enum Error {
    #[error("not found: {0}")]
    NotFound(String),

    #[error("io error: {0}")]
    Io(#[from] std::io::Error),

    #[error("secret resolution failed: {0}")]
    SecretResolve(String),
}

pub type Result<T> = std::result::Result<T, Error>;
