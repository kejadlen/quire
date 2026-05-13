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
    fn fennel_eval_display_is_self_contained() {
        // FennelError::Eval's Display should carry both the source
        // name and the underlying lua error text — without needing to
        // walk the `#[source]` chain — so plain `%err` is enough for
        // tracing/Sentry. The chain is still preserved structurally.
        let f = quire_core::fennel::Fennel::new().expect("Fennel::new");
        let result: std::result::Result<i32, _> = f.load_string("(this is not valid", "bad.fnl");
        let fennel_err = result.unwrap_err();

        let plain = fennel_err.to_string();
        assert!(
            plain.starts_with("bad.fnl: "),
            "Display should start with the source name: {plain:?}"
        );
        assert!(
            plain.len() > "bad.fnl: ".len(),
            "Display should include the underlying error detail: {plain:?}"
        );

        // The `#[source]` chain is still walkable.
        let err: &dyn std::error::Error = &fennel_err;
        assert!(err.source().is_some(), "source chain should be preserved");
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
