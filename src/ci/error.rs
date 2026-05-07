//! CI error types.

use miette::Diagnostic;

use super::pipeline::PipelineError;
use super::run::RunState;
use crate::fennel::FennelError;
use crate::secret;

/// Errors produced by CI operations.
#[derive(Debug, thiserror::Error, Diagnostic)]
pub enum Error {
    #[error("io error")]
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

    #[error("job '{job}' failed")]
    JobFailed {
        job: String,
        #[source]
        source: Box<Error>,
    },

    #[error("docker is not available — install docker and ensure the daemon is running")]
    DockerUnavailable,

    #[error("missing .quire/Dockerfile")]
    DockerfileMissing,

    #[error("workspace materialization failed")]
    WorkspaceMaterializationFailed {
        #[source]
        source: std::io::Error,
    },

    #[error("image build failed")]
    ImageBuildFailed {
        #[source]
        source: std::io::Error,
    },

    #[error("container start failed")]
    ContainerStartFailed {
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

    #[error("command spawn failed: {program} in {cwd}")]
    CommandSpawnFailed {
        program: String,
        cwd: std::path::PathBuf,
        #[source]
        source: std::io::Error,
    },
}

pub type Result<T> = std::result::Result<T, Error>;

impl From<PipelineError> for Error {
    fn from(err: PipelineError) -> Self {
        Error::Pipeline(Box::new(err))
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
