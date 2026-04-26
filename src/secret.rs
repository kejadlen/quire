use std::path::PathBuf;
use std::sync::OnceLock;

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
pub struct SecretString {
    source: SecretSource,
    resolved: OnceLock<std::result::Result<String, String>>,
}

impl Clone for SecretString {
    fn clone(&self) -> Self {
        Self {
            source: self.source.clone(),
            resolved: OnceLock::new(),
        }
    }
}

#[derive(Clone)]
enum SecretSource {
    Plain(String),
    File(PathBuf),
}

impl SecretString {
    /// The resolved secret value.
    ///
    /// For the file variant, reads from disk on first call and caches the
    /// result. Returns a typed error if the file is missing or unreadable.
    /// Errors are also cached — subsequent calls return the same error.
    pub fn reveal(&self) -> crate::Result<&str> {
        self.resolved
            .get_or_init(|| match &self.source {
                SecretSource::Plain(s) => Ok(s.clone()),
                SecretSource::File(path) => fs_err::read_to_string(path)
                    .map(|s| s.trim_end_matches('\n').to_string())
                    .map_err(|e| format!("{}: {e}", path.display())),
            })
            .as_ref()
            .map(|s| s.as_str())
            .map_err(|msg| crate::Error::SecretResolve(msg.clone()))
    }
}

impl std::fmt::Debug for SecretString {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_tuple("SecretString").field(&"<redacted>").finish()
    }
}

impl<'de> serde::Deserialize<'de> for SecretString {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
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
            Raw::File { file } => SecretSource::File(file),
        };

        Ok(Self {
            source,
            resolved: OnceLock::new(),
        })
    }
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use super::*;

    impl SecretString {
        fn from_plain(value: &str) -> Self {
            Self {
                source: SecretSource::Plain(value.to_string()),
                resolved: OnceLock::new(),
            }
        }

        fn from_file(path: &Path) -> Self {
            Self {
                source: SecretSource::File(path.to_path_buf()),
                resolved: OnceLock::new(),
            }
        }
    }

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
    fn reveal_errors_on_missing_file() {
        let secret = SecretString::from_file(PathBuf::from("/no/such/file/ever").as_path());
        let err = secret.reveal().unwrap_err();
        assert!(
            matches!(err, crate::Error::SecretResolve(_)),
            "expected SecretResolve error, got {err:?}"
        );
    }

    #[test]
    fn clone_resolves_independently() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("pw");
        fs_err::write(&path, "secret\n").expect("write");

        let original = SecretString::from_file(&path);
        assert_eq!(original.reveal().unwrap(), "secret");

        // Clone gets a fresh OnceLock — it re-reads from disk.
        let cloned = original.clone();
        assert_eq!(cloned.reveal().unwrap(), "secret");
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

        let fennel = crate::fennel::Fennel::new().expect("fennel");
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

        let fennel = crate::fennel::Fennel::new().expect("fennel");
        // Fennel table syntax: {:token {:file "/path"}}
        let source = format!("{{:token {{:file \"{}\"}}}}", path.display(),);
        let config: Config = fennel
            .load_string(&source, "test.fnl")
            .expect("deserialize file ref from fennel");
        assert_eq!(config.token.reveal().unwrap(), "secret_from_file");
    }
}
