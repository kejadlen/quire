use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Arc, Mutex, OnceLock};

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

/// Signature for a secret fallback fetcher: given a name, return the
/// revealed value or an error.
type SecretFetcher = Box<dyn Fn(&str) -> Result<String>>;

/// Per-run secret store. Resolves secret names to values via a fetcher
/// closure, caching each result so the fetcher is called at most once
/// per name. Revealed values are registered for redaction.
///
/// The normal construction path installs an API fetcher and starts with
/// an empty cache. The filesystem source pre-warms the cache via
/// [`SecretRegistry::seed`] so no fetches are needed at run time.
///
/// Lifetime is bounded to a single CI run. Do not carry a registry
/// across runs — revealed values from previous runs would contaminate
/// redaction of unrelated output.
pub struct SecretRegistry {
    /// Pull-through cache: name → secret. Pre-seeded for the filesystem
    /// source; populated lazily for the API source.
    cache: HashMap<String, SecretString>,
    /// name → revealed value (opaque). Populated on first `(secret :name)` call.
    revealed: HashMap<String, Revealed>,
    /// Called when a name is absent from the cache. Always present — the
    /// normal case is an API fetcher; tests and the filesystem source use
    /// a closure that returns [`Error::UnknownSecret`].
    fetcher: SecretFetcher,
    /// Optional tracing-layer handle. When set, every revealed value (≥ 8 chars)
    /// is pushed here so the fmt writer can redact it from log output.
    redact_handle: Option<RedactHandle>,
}

impl std::fmt::Debug for SecretRegistry {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SecretRegistry")
            .field("cache", &self.cache.keys().collect::<Vec<_>>())
            .field("revealed", &self.revealed.keys().collect::<Vec<_>>())
            .field("fetcher", &"<fn>")
            .field("redact_handle", &self.redact_handle.is_some())
            .finish()
    }
}

fn not_found_fetcher(name: &str) -> Result<String> {
    Err(Error::UnknownSecret(name.to_string()))
}

impl From<HashMap<String, SecretString>> for SecretRegistry {
    fn from(secrets: HashMap<String, SecretString>) -> Self {
        Self::new(not_found_fetcher).seed(secrets)
    }
}

impl From<Vec<(String, SecretString)>> for SecretRegistry {
    fn from(pairs: Vec<(String, SecretString)>) -> Self {
        Self::from(pairs.into_iter().collect::<HashMap<_, _>>())
    }
}

impl From<Vec<(&str, &str)>> for SecretRegistry {
    fn from(pairs: Vec<(&str, &str)>) -> Self {
        let cache: HashMap<String, SecretString> = pairs
            .into_iter()
            .map(|(k, v)| (k.to_string(), SecretString::from(v)))
            .collect();
        Self::from(cache)
    }
}

impl SecretRegistry {
    /// Create a registry backed by `fetcher`. The cache starts empty;
    /// use [`SecretRegistry::seed`] to pre-warm it.
    ///
    /// `fetcher` is called at most once per name — results are cached
    /// back into the registry so subsequent lookups are local. Values
    /// fetched through either path are registered for redaction.
    pub fn new<F>(fetcher: F) -> Self
    where
        F: Fn(&str) -> Result<String> + 'static,
    {
        Self {
            cache: HashMap::new(),
            revealed: HashMap::new(),
            fetcher: Box::new(fetcher),
            redact_handle: None,
        }
    }

    /// Attach a [`RedactHandle`] so that every subsequently revealed secret
    /// (≥ 8 chars) is forwarded to the tracing layer for log redaction.
    ///
    /// Call this once, right after constructing the registry and before the
    /// first `resolve()`. The handle should be the same one passed to
    /// [`telemetry::init_tracing`].
    pub fn attach_redact_handle(&mut self, handle: RedactHandle) {
        self.redact_handle = Some(handle);
    }

    /// Pre-warm the cache with an existing set of secrets. Intended for
    /// the filesystem source, which receives all secrets up-front in the
    /// bootstrap file. Pre-seeded names are served from the cache without
    /// invoking the fetcher.
    pub fn seed(mut self, secrets: HashMap<String, SecretString>) -> Self {
        self.cache = secrets;
        self
    }

    /// Resolve a secret by name, caching the revealed value for
    /// redaction. Checks the cache first; on a miss, calls the fetcher
    /// and stores the result. Returns `Err` if the name is unknown or
    /// the source can't be read.
    ///
    /// Values shorter than 8 characters are returned to the caller
    /// but not registered for redaction — the false-positive rate on
    /// common short strings like "true" or "yes" is too high. A warn
    /// is emitted so an operator can see why a short token is showing
    /// up unredacted in CI output.
    ///
    /// The returned `String` is the plain, revealed value. Do not pass
    /// it to `tracing` or any other log sink — the global tracing
    /// subscriber has no redaction layer, so a leaked value would
    /// reach stderr and Sentry. Route it into a surface that goes
    /// through [`redact`] (e.g. `sh` command args, ShOutput) or wrap
    /// it in a type whose `Debug`/`Display` impl redacts.
    pub fn resolve(&mut self, name: &str) -> Result<String> {
        let value = if let Some(secret) = self.cache.get(name) {
            secret.reveal()?.to_string()
        } else {
            let fetched = (self.fetcher)(name)?;
            self.cache
                .insert(name.to_string(), SecretString::from(fetched.clone()));
            fetched
        };
        if value.len() >= 8 {
            self.revealed
                .insert(name.to_string(), Revealed::new(value.clone()));
            if let Some(h) = &self.redact_handle {
                h.push(name.to_string(), value.clone());
            }
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

// ── Tracing-level redaction ─────────────────────────────────────────────────

/// A thread-safe, cheaply cloneable handle to the set of revealed secrets for
/// use by the tracing fmt layer.
///
/// `SecretRegistry::attach_redact_handle` registers this handle; the registry
/// pushes each newly revealed secret (≥ 8 chars) into it via `push`. The
/// tracing layer calls `redact` on every formatted log line before writing to
/// stderr and Sentry.
#[derive(Clone, Default)]
pub struct RedactHandle(Arc<Mutex<Vec<(String, String)>>>);

impl RedactHandle {
    pub fn new() -> Self {
        Self(Arc::new(Mutex::new(Vec::new())))
    }

    /// Register a (name, value) pair. Called by the registry on each resolve.
    pub(crate) fn push(&self, name: String, value: String) {
        self.0.lock().unwrap().push((name, value));
    }

    /// Replace every known secret value in `text` with `{{ name }}`.
    /// Returns the input unchanged when no secrets have been registered yet.
    pub fn redact(&self, text: &str) -> String {
        let entries = self.0.lock().unwrap();
        if entries.is_empty() {
            return text.to_string();
        }
        let mut sorted: Vec<(&str, &str)> = entries
            .iter()
            .map(|(n, v)| (n.as_str(), v.as_str()))
            .collect();
        // Longest first to prevent partial matches, then by name for stability.
        sorted.sort_by(|a, b| b.1.len().cmp(&a.1.len()).then_with(|| a.0.cmp(b.0)));
        let mut result = text.to_string();
        for (name, value) in sorted {
            result = result.replace(value, &format!("{{{{ {} }}}}", name));
        }
        result
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fennel::Fennel;

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
    fn registry_debug_does_not_leak_revealed_values() {
        let mut registry: SecretRegistry =
            vec![("github_token", "abcdefghijklmnop_long_enough")].into();
        let _ = registry.resolve("github_token").unwrap();
        let debug_output = format!("{registry:?}");
        assert!(
            !debug_output.contains("abcdefghijklmnop_long_enough"),
            "SecretRegistry Debug must not leak revealed values: {debug_output}"
        );
        assert!(
            debug_output.contains("github_token"),
            "SecretRegistry Debug should still surface cached names: {debug_output}"
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
    fn redact_handle_replaces_known_values() {
        let handle = RedactHandle::new();
        handle.push("token".into(), "supersecrettoken".into());
        handle.push("password".into(), "hunter2password".into());
        let out = handle.redact("token is supersecrettoken and password is hunter2password");
        assert_eq!(out, "token is {{ token }} and password is {{ password }}");
    }

    #[test]
    fn redact_handle_longest_first() {
        let handle = RedactHandle::new();
        // "longprefix_suffix" contains "longprefix" as a prefix; longest must go first.
        handle.push("short".into(), "longprefix".into());
        handle.push("long".into(), "longprefix_suffix".into());
        let out = handle.redact("value: longprefix_suffix");
        // The longer value "longprefix_suffix" should be replaced entirely,
        // not partially replaced as "{{ short }}_suffix".
        assert_eq!(out, "value: {{ long }}");
    }

    #[test]
    fn redact_handle_empty_returns_input() {
        let handle = RedactHandle::new();
        let text = "no secrets here";
        assert_eq!(handle.redact(text), text);
    }

    #[test]
    fn attach_redact_handle_receives_resolved_secrets() {
        let handle = RedactHandle::new();
        let mut registry: SecretRegistry = vec![("api_key", "abcdefghijklmnop_secret")].into();
        registry.attach_redact_handle(handle.clone());
        let _ = registry.resolve("api_key").unwrap();
        let out = handle.redact("key=abcdefghijklmnop_secret");
        assert_eq!(out, "key={{ api_key }}");
    }

    #[test]
    fn fetcher_result_is_cached() {
        use std::sync::Arc;
        use std::sync::atomic::{AtomicUsize, Ordering};

        let call_count = Arc::new(AtomicUsize::new(0));
        let counter = call_count.clone();

        let mut registry = SecretRegistry::new(move |name| {
            counter.fetch_add(1, Ordering::SeqCst);
            Ok(format!("fetched_{name}_abcdefgh"))
        });

        let first = registry.resolve("token").unwrap();
        let second = registry.resolve("token").unwrap();

        assert_eq!(first, "fetched_token_abcdefgh");
        assert_eq!(second, "fetched_token_abcdefgh");
        assert_eq!(
            call_count.load(Ordering::SeqCst),
            1,
            "fallback should be called exactly once"
        );
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
