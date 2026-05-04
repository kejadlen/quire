use miette::Diagnostic;

use crate::ci::{PipelineError, RunState};
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
}

pub type Result<T> = std::result::Result<T, Error>;

/// Display wrapper that walks an error's `source()` chain.
///
/// `tracing`'s `%field` formatter only calls `Display` on the top
/// error, which discards information for layered errors (e.g.,
/// `FennelError::Eval` whose top message is just the filename, with
/// the real diagnostic carried by its `source`). This wrapper joins
/// each layer with `": "` so structured logs carry the whole chain.
///
/// Construct via [`display_chain`] and use as a tracing field:
///
/// ```ignore
/// tracing::error!(error = %display_chain(&e), "operation failed");
/// ```
pub struct DisplayChain<'a>(&'a (dyn std::error::Error + 'static));

impl std::fmt::Display for DisplayChain<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.0)?;
        let mut cur = self.0.source();
        while let Some(err) = cur {
            write!(f, ": {err}")?;
            cur = err.source();
        }
        Ok(())
    }
}

/// Wrap an error reference for chained `Display` rendering. See
/// [`DisplayChain`].
pub fn display_chain<E: std::error::Error + 'static>(err: &E) -> DisplayChain<'_> {
    DisplayChain(err)
}

impl From<FennelError> for Error {
    fn from(err: FennelError) -> Self {
        Error::Fennel(Box::new(err))
    }
}

impl From<PipelineError> for Error {
    fn from(err: PipelineError) -> Self {
        Error::Pipeline(Box::new(err))
    }
}

impl From<mlua::Error> for Error {
    fn from(err: mlua::Error) -> Self {
        Error::Lua(Box::new(err))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fennel::FennelError;

    #[test]
    fn from_fennel_error() {
        let fennel_err = FennelError::Empty {
            name: "test.fnl".to_string(),
        };
        let err: Error = fennel_err.into();
        assert!(err.to_string().contains("test.fnl"));
    }

    #[test]
    fn display_chain_walks_source_chain() {
        // FennelError::Eval has a top-level message of just the
        // filename and an mlua::Error in its source — the exact case
        // the helper is meant to fix.
        let f = crate::fennel::Fennel::new().expect("Fennel::new");
        let result: std::result::Result<i32, _> = f.load_string("(this is not valid", "bad.fnl");
        let fennel_err = result.unwrap_err();

        let plain = fennel_err.to_string();
        let chained = display_chain(&fennel_err).to_string();

        assert!(
            chained.starts_with(&plain),
            "chained output should begin with the top message"
        );
        assert!(
            chained.len() > plain.len(),
            "chained output should add source info: top={plain:?} chained={chained:?}"
        );
    }

    #[test]
    fn display_chain_handles_no_source() {
        let err = Error::Git("boom".to_string());
        assert_eq!(display_chain(&err).to_string(), "git error: boom");
    }

    #[test]
    fn from_pipeline_error() {
        let source = "(ci.job :a [] (fn [_] nil))";
        let pipeline_err = PipelineError {
            src: miette::NamedSource::new("ci.fnl", source.to_string()),
            diagnostics: vec![crate::ci::Diagnostic::Definition(
                crate::ci::DefinitionError::EmptyInputs {
                    job_id: "a".to_string(),
                    span: miette::SourceSpan::from((0, 0)),
                },
            )],
        };
        let err: Error = pipeline_err.into();
        assert!(err.to_string().contains("ci.fnl has errors"));
    }
}
