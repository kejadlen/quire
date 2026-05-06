use std::path::PathBuf;
use std::sync::OnceLock;

#[cfg(test)]
use crate::fennel::Fennel;

/// Errors produced by secret resolution.
#[derive(Debug, Clone, thiserror::Error)]
pub enum Error {
    /// File-backed secret could not be read.
    ///
    /// Stored as a string because `OnceLock` in `SecretString::reveal` caches
    /// the error and `std::io::Error` is not `Clone`. Once `once_cell_try`
    /// stabilizes (allowing `OnceLock::get_or_try_init` with a separate error
    /// type), we can store a structured error instead of a string.
    #[error("secret resolution failed: {0}")]
    Resolve(String),
}

pub type Result<T> = std::result::Result<T, Error>;

/// A string value that deserializes from either a plain literal or a file path.
///
/// Fennel config can provide a secret as:
/// - A plain string: `"s3cret"`
/// - A file reference: `{:file "/run/secrets/my_token"}`
///
/// File contents are resolved lazily on first access to [`SecretString::reveal`]
/// and cached for the lifetime of the instance. Trailing newlines are stripped
/// from file contents (Docker secrets convention).
///
/// The [`std::fmt::Debug`] impl redacts the value.
#[derive(Clone)]
pub struct SecretString(SecretSource);

enum SecretSource {
    Plain(String),
    File {
        path: PathBuf,
        resolved: OnceLock<std::result::Result<String, String>>,
    },
}

impl Clone for SecretSource {
    fn clone(&self) -> Self {
        match self {
            Self::Plain(s) => Self::Plain(s.clone()),
            // File clones get a fresh OnceLock — they re-read from disk on next reveal.
            Self::File { path, .. } => Self::File {
                path: path.clone(),
                resolved: OnceLock::new(),
            },
        }
    }
}

impl SecretString {
    /// The resolved secret value.
    ///
    /// For the file variant, reads from disk on first call and caches the
    /// result. Errors are also cached — subsequent calls return the same error.
    ///
    /// The error is stored as a formatted string inside `OnceLock` because
    /// `std::io::Error` is not `Clone`, and `OnceLock::get_or_init` requires
    /// the closure output to be `Sized` + ownable. Once `once_cell_try`
    /// stabilizes (allowing `OnceLock::get_or_try_init` with a separate error
    /// type), we can store a structured error instead of a string.
    pub fn reveal(&self) -> Result<&str> {
        match &self.0 {
            SecretSource::Plain(s) => Ok(s.as_str()),
            SecretSource::File { path, resolved } => resolved
                .get_or_init(|| {
                    fs_err::read_to_string(path)
                        .map(|s| s.strip_suffix('\n').unwrap_or(&s).to_string())
                        .map_err(|e| format!("{}: {e}", path.display()))
                })
                .as_ref()
                .map(|s| s.as_str())
                .map_err(|msg| Error::Resolve(msg.clone())),
        }
    }
}

impl std::fmt::Debug for SecretString {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_tuple("SecretString").field(&"<redacted>").finish()
    }
}

impl SecretString {
    /// Build from a plain string literal.
    pub fn from_plain(value: impl Into<String>) -> Self {
        Self(SecretSource::Plain(value.into()))
    }

    /// Build from a file path. Contents are read lazily on first [`reveal`].
    ///
    /// [`reveal`]: SecretString::reveal
    pub fn from_file(path: impl Into<PathBuf>) -> Self {
        Self(SecretSource::File {
            path: path.into(),
            resolved: OnceLock::new(),
        })
    }
}

impl<'de> serde::Deserialize<'de> for SecretString {
    fn deserialize<D>(deserializer: D) -> std::result::Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        #[derive(serde::Deserialize)]
        #[serde(untagged)]
        enum Raw {
            Plain(String),
            File { file: PathBuf },
        }

        let raw = Raw::deserialize(deserializer)?;
        let source = match raw {
            Raw::Plain(s) => SecretSource::Plain(s),
            Raw::File { file } => SecretSource::File {
                path: file,
                resolved: OnceLock::new(),
            },
        };

        Ok(Self(source))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn debug_redacts_value() {
        let secret = SecretString::from_plain("super_secret_password");
        let debug_output = format!("{secret:?}");
        assert_eq!(debug_output, "SecretString(\"<redacted>\")");
        assert!(
            !debug_output.contains("super_secret_password"),
            "Debug must not leak the secret value"
        );
    }

    #[test]
    fn reveal_returns_plain_value() {
        let secret = SecretString::from_plain("plain_value");
        assert_eq!(secret.reveal().unwrap(), "plain_value");
    }

    #[test]
    fn clone_preserves_plain_value() {
        let secret = SecretString::from_plain("clonable");
        let cloned = secret.clone();
        assert_eq!(cloned.reveal().unwrap(), "clonable");
    }

    #[test]
    fn reveal_caches_file_value() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("token");
        fs_err::write(&path, "initial\n").expect("write");

        let secret = SecretString::from_file(&path);
        assert_eq!(secret.reveal().unwrap(), "initial");

        // Overwrite the file — cached value should not change.
        fs_err::write(&path, "changed\n").expect("overwrite");
        assert_eq!(secret.reveal().unwrap(), "initial");
    }

    #[test]
    fn reveal_strips_trailing_newline() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("secret");
        fs_err::write(&path, "line1\nline2\n").expect("write");

        let secret = SecretString::from_file(&path);
        assert_eq!(secret.reveal().unwrap(), "line1\nline2");
    }

    #[test]
    fn reveal_strips_only_one_trailing_newline() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("secret");
        // Docker secrets convention: strip exactly one trailing newline.
        // Any additional trailing newlines are part of the secret.
        fs_err::write(&path, "value\n\n\n").expect("write");

        let secret = SecretString::from_file(&path);
        assert_eq!(secret.reveal().unwrap(), "value\n\n");
    }

    #[test]
    fn reveal_errors_on_missing_file() {
        let secret = SecretString::from_file(PathBuf::from("/no/such/file/ever").as_path());
        let err = secret.reveal().unwrap_err();
        assert!(
            matches!(err, Error::Resolve(_)),
            "expected Resolve error, got {err:?}"
        );
    }

    #[test]
    fn clone_resets_cache_and_rereads_from_disk() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("pw");
        fs_err::write(&path, "initial\n").expect("write");

        let original = SecretString::from_file(&path);
        assert_eq!(original.reveal().unwrap(), "initial");

        // Overwrite after the original cached "initial". The clone gets a fresh
        // OnceLock, so it re-reads the current file contents.
        fs_err::write(&path, "changed\n").expect("overwrite");
        let cloned = original.clone();
        assert_eq!(cloned.reveal().unwrap(), "changed");
        // Original's cache is untouched.
        assert_eq!(original.reveal().unwrap(), "initial");
    }

    #[test]
    fn deserialize_plain_string() {
        #[derive(serde::Deserialize)]
        struct Wrapper {
            token: SecretString,
        }

        let json = r#"{"token": "s3cret"}"#;
        let w: Wrapper = serde_json::from_str(json).expect("deserialize plain string");
        assert_eq!(w.token.reveal().unwrap(), "s3cret");
    }

    #[test]
    fn deserialize_file_does_not_touch_disk() {
        #[derive(serde::Deserialize)]
        struct Wrapper {
            token: SecretString,
        }

        let json = r#"{"token": {"file": "/no/such/file/ever"}}"#;
        let w: Wrapper = serde_json::from_str(json).expect("deserialize should not read file");
        // Deserialization succeeded without touching disk.
        assert!(w.token.reveal().is_err());
    }

    #[test]
    fn deserialize_file_resolves_on_reveal() {
        #[derive(serde::Deserialize)]
        struct Wrapper {
            token: SecretString,
        }

        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("token");
        fs_err::write(&path, "from_file\n").expect("write");

        let json = serde_json::json!({
            "token": {"file": path.display().to_string()}
        });
        let w: Wrapper = serde_json::from_value(json).expect("deserialize");
        assert_eq!(w.token.reveal().unwrap(), "from_file");
    }

    #[test]
    fn fennel_round_trip_plain_string() {
        #[derive(serde::Deserialize)]
        struct Config {
            token: SecretString,
        }

        let fennel = Fennel::new().expect("fennel");
        let config: Config = fennel
            .load_string(r#"{:token "hunter2"}"#, "test.fnl")
            .expect("deserialize from fennel");
        assert_eq!(config.token.reveal().unwrap(), "hunter2");
    }

    #[test]
    fn fennel_round_trip_file_ref() {
        #[derive(serde::Deserialize)]
        struct Config {
            token: SecretString,
        }

        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("pw");
        fs_err::write(&path, "secret_from_file\n").expect("write");

        let fennel = Fennel::new().expect("fennel");
        // Fennel table syntax: {:token {:file "/path"}}
        let source = format!("{{:token {{:file \"{}\"}}}}", path.display(),);
        let config: Config = fennel
            .load_string(&source, "test.fnl")
            .expect("deserialize file ref from fennel");
        assert_eq!(config.token.reveal().unwrap(), "secret_from_file");
    }
}
