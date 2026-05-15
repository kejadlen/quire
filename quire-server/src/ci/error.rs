//! CI error types.

use miette::Diagnostic;

use super::pipeline::{CompileError, PipelineError};
use super::run::RunState;
use super::runtime::RuntimeError;
use quire_core::fennel::FennelError;
use quire_core::secret;

/// Errors produced by CI operations.
#[derive(Debug, thiserror::Error, Diagnostic)]
pub enum Error {
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),

    #[error(transparent)]
    #[diagnostic(transparent)]
    Fennel(#[from] Box<FennelError>),

    #[error(transparent)]
    #[diagnostic(transparent)]
    Pipeline(Box<PipelineError>),

    #[error("invalid run transition: {from:?} -> {to:?}")]
    InvalidTransition { from: RunState, to: RunState },

    #[error(transparent)]
    Lua(Box<mlua::Error>),

    #[error("workspace materialization failed: {source}")]
    WorkspaceMaterializationFailed {
        #[source]
        source: std::io::Error,
    },

    #[error("git error: {0}")]
    Git(String),

    #[error(transparent)]
    Yaml(#[from] serde_yaml_ng::Error),

    #[error(transparent)]
    Utf8(#[from] std::string::FromUtf8Error),

    #[error(transparent)]
    Sql(#[from] rusqlite::Error),

    #[error(transparent)]
    Secret(#[from] secret::Error),

    #[error("command spawn failed: {program} in {}: {source}", cwd.display())]
    CommandSpawnFailed {
        program: String,
        cwd: std::path::PathBuf,
        #[source]
        source: std::io::Error,
    },

    #[error("quire-ci exited with status {exit:?}")]
    QuireCiExit { exit: Option<i32> },

    #[error("ci.transport=api requires ci.server-url to be set in config.fnl")]
    ApiTransportMissingServerUrl,

    #[error("failed to parse quire-ci event stream at {}: {source}", path.display())]
    EventStreamParse {
        path: std::path::PathBuf,
        #[source]
        source: serde_json::Error,
    },
}

pub type Result<T> = std::result::Result<T, Error>;

impl From<PipelineError> for Error {
    fn from(err: PipelineError) -> Self {
        Error::Pipeline(Box::new(err))
    }
}

impl From<CompileError> for Error {
    fn from(err: CompileError) -> Self {
        match err {
            CompileError::Fennel(e) => Error::Fennel(e),
            CompileError::Pipeline(e) => Error::Pipeline(e),
        }
    }
}

impl From<RuntimeError> for Error {
    fn from(err: RuntimeError) -> Self {
        match err {
            RuntimeError::Secret(e) => Error::Secret(e),
            RuntimeError::Lua(e) => Error::Lua(e),
            RuntimeError::CommandSpawnFailed {
                program,
                cwd,
                source,
            } => Error::CommandSpawnFailed {
                program,
                cwd,
                source,
            },
            RuntimeError::Git(s) => Error::Git(s),
            RuntimeError::LogWriteFailed { path, source } => Error::CommandSpawnFailed {
                program: "write-cri-log".to_string(),
                cwd: path,
                source,
            },
        }
    }
}

impl From<FennelError> for Error {
    fn from(err: FennelError) -> Self {
        Error::Fennel(Box::new(err))
    }
}

impl From<mlua::Error> for Error {
    fn from(err: mlua::Error) -> Self {
        Error::Lua(Box::new(err))
    }
}
