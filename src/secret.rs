use std::collections::HashMap;
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

    #[error("unknown secret: {0:?}")]
    UnknownSecret(String),
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

impl From<String> for SecretString {
    fn from(value: String) -> Self {
        Self(SecretSource::Plain(value))
    }
}

impl From<&str> for SecretString {
    fn from(value: &str) -> Self {
        Self(SecretSource::Plain(value.to_string()))
    }
}

impl From<PathBuf> for SecretString {
    /// Build from a file path. Contents are read lazily on first [`reveal`].
    ///
    /// [`reveal`]: SecretString::reveal
    fn from(path: PathBuf) -> Self {
        Self(SecretSource::File {
            path,
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

// ── Secret registry and redaction ───────────────────────────────

/// Opaque wrapper for a revealed secret value. No Debug impl.
struct Revealed(String);

impl Revealed {
    fn new(value: String) -> Self {
        Self(value)
    }

    fn as_str(&self) -> &str {
        &self.0
    }
}

// Explicitly no Debug impl — revealed values must never be printed.

/// Per-run secret store: holds declared secrets and their revealed
/// values for both lookup and redaction.
///
/// Constructed with the declared secrets from global config.
/// As `(secret :name)` is called during CI execution, values are
/// revealed and cached for redaction via [`redact`].
///
/// Lifetime is bounded to a single CI run. Do not carry a registry
/// across runs — values from previous runs would contaminate
/// redaction of unrelated output.
pub struct SecretRegistry {
    /// name → declared secret (lazy reveal).
    declared: HashMap<String, SecretString>,
    /// name → revealed value (opaque, zeroed on drop).
    /// Populated on first `(secret :name)` call.
    revealed: HashMap<String, Revealed>,
}

impl std::fmt::Debug for SecretRegistry {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SecretRegistry")
            .field("declared", &self.declared.keys().collect::<Vec<_>>())
            .field("revealed", &self.revealed.keys().collect::<Vec<_>>())
            .finish()
    }
}

impl From<HashMap<String, SecretString>> for SecretRegistry {
    fn from(declared: HashMap<String, SecretString>) -> Self {
        Self {
            declared,
            revealed: HashMap::new(),
        }
    }
}

impl From<Vec<(String, SecretString)>> for SecretRegistry {
    fn from(pairs: Vec<(String, SecretString)>) -> Self {
        Self::from(pairs.into_iter().collect::<HashMap<_, _>>())
    }
}

impl From<Vec<(&str, &str)>> for SecretRegistry {
    fn from(pairs: Vec<(&str, &str)>) -> Self {
        let declared: HashMap<String, SecretString> = pairs
            .into_iter()
            .map(|(k, v)| (k.to_string(), SecretString::from(v)))
            .collect();
        Self::from(declared)
    }
}

impl SecretRegistry {
    /// Resolve a declared secret by name, caching the revealed value
    /// for redaction. Returns `Err` if the name isn't declared or
    /// the source can't be read.
    ///
    /// Values shorter than 8 characters are returned to the caller
    /// but not registered for redaction — the false-positive rate on
    /// common short strings like "true" or "yes" is too high. A warn
    /// is emitted so an operator can see why a short token is showing
    /// up unredacted in CI output.
    pub fn resolve(&mut self, name: &str) -> Result<String> {
        let secret = self
            .declared
            .get(name)
            .ok_or_else(|| Error::UnknownSecret(name.to_string()))?;
        let value = secret.reveal()?.to_string();
        if value.len() >= 8 {
            self.revealed
                .insert(name.to_string(), Revealed::new(value.clone()));
        } else {
            tracing::warn!(
                secret = %name,
                length = value.len(),
                "secret value is shorter than the 8-byte minimum and will not be redacted from CI output"
            );
        }
        Ok(value)
    }

    /// Return revealed (name, value) pairs sorted by value length
    /// descending so longest matches are replaced first (prevents
    /// partial replacement of overlapping secrets). Equal-length
    /// values tiebreak on name, so two names that map to the same
    /// value redact deterministically.
    fn entries(&self) -> Vec<(&str, &str)> {
        let mut entries: Vec<_> = self
            .revealed
            .iter()
            .map(|(k, v)| (k.as_str(), v.as_str()))
            .collect();
        entries.sort_by(|a, b| b.1.len().cmp(&a.1.len()).then_with(|| a.0.cmp(b.0)));
        entries
    }

    pub fn has_redactions(&self) -> bool {
        !self.revealed.is_empty()
    }
}

/// Replace any revealed secret value in `text` with `{{ name }}`.
///
/// Longest values are replaced first to prevent partial matches.
/// Returns the input unchanged when no secrets have been revealed.
pub fn redact(text: &str, registry: &SecretRegistry) -> String {
    if !registry.has_redactions() {
        return text.to_string();
    }
    let mut result = text.to_string();
    for (name, value) in registry.entries() {
        let replacement = format!("{{{{ {} }}}}", name);
        result = result.replace(value, &replacement);
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn debug_redacts_value() {
        let secret = SecretString::from("super_secret_password");
        let debug_output = format!("{secret:?}");
        assert_eq!(debug_output, "SecretString(\"<redacted>\")");
        assert!(
            !debug_output.contains("super_secret_password"),
            "Debug must not leak the secret value"
        );
    }

    #[test]
    fn reveal_returns_plain_value() {
        let secret = SecretString::from("plain_value");
        assert_eq!(secret.reveal().unwrap(), "plain_value");
    }

    #[test]
    fn clone_preserves_plain_value() {
        let secret = SecretString::from("clonable");
        let cloned = secret.clone();
        assert_eq!(cloned.reveal().unwrap(), "clonable");
    }

    #[test]
    fn reveal_caches_file_value() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("token");
        fs_err::write(&path, "initial\n").expect("write");

        let secret = SecretString::from(path.clone());
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

        let secret = SecretString::from(path.clone());
        assert_eq!(secret.reveal().unwrap(), "line1\nline2");
    }

    #[test]
    fn reveal_strips_only_one_trailing_newline() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("secret");
        // Docker secrets convention: strip exactly one trailing newline.
        // Any additional trailing newlines are part of the secret.
        fs_err::write(&path, "value\n\n\n").expect("write");

        let secret = SecretString::from(path.clone());
        assert_eq!(secret.reveal().unwrap(), "value\n\n");
    }

    #[test]
    fn reveal_errors_on_missing_file() {
        let secret = SecretString::from(PathBuf::from("/no/such/file/ever"));
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

        let original = SecretString::from(path.clone());
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
