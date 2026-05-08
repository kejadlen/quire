use miette::Diagnostic;

use crate::ci::Error as CiError;
use quire_core::fennel::FennelError;
use quire_core::secret;

#[derive(Debug, thiserror::Error, Diagnostic)]
pub enum Error {
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),

    #[error("config not found: {0}")]
    ConfigNotFound(String),

    #[error(transparent)]
    #[diagnostic(transparent)]
    Fennel(#[from] Box<FennelError>),

    #[error(transparent)]
    #[diagnostic(transparent)]
    Ci(#[from] CiError),

    #[error(transparent)]
    Secret(#[from] secret::Error),

    #[error(transparent)]
    Sql(#[from] rusqlite::Error),

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

#[cfg(test)]
mod tests {
    use super::*;
    use quire_core::fennel::FennelError;

    #[test]
    fn from_fennel_error() {
        let fennel_err = FennelError::Io(std::io::Error::new(
            std::io::ErrorKind::NotFound,
            "test.fnl",
        ));
        let err: Error = fennel_err.into();
        assert!(err.to_string().contains("test.fnl"));
    }

    #[test]
    fn display_chain_walks_source_chain() {
        // FennelError::Eval has a top-level message of just the
        // filename and an mlua::Error in its source — the exact case
        // the helper is meant to fix.
        let f = quire_core::fennel::Fennel::new().expect("Fennel::new");
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
        let err = Error::ConfigNotFound("missing.yml".to_string());
        assert_eq!(
            display_chain(&err).to_string(),
            "config not found: missing.yml"
        );
    }

    #[test]
    fn from_pipeline_error() {
        let source = "(ci.job :a [] (fn [_] nil))";
        let pipeline_err = crate::ci::PipelineError {
            src: miette::NamedSource::new("ci.fnl", source.to_string()),
            diagnostics: vec![crate::ci::Diagnostic::Definition(
                crate::ci::DefinitionError::EmptyInputs {
                    job_id: "a".to_string(),
                    span: miette::SourceSpan::from((0, 0)),
                },
            )],
        };
        let ci_err = crate::ci::Error::from(pipeline_err);
        let err: Error = ci_err.into();
        assert!(err.to_string().contains("ci.fnl has errors"));
    }
}
