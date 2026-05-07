//! Secret redaction for CI output surfaces.
//!
//! Collects resolved secret values into a per-run registry and
//! provides a [`redact`] function that replaces any registered value
//! with `{{ name }}`.
//!
//! Values are stored opaquely (no [`Debug`] impl, manual [`Drop`]
//! that overwrites bytes) to avoid re-introducing the secret in
//! debug output or core dumps. This mirrors the protections in
//! `crate::secret::SecretString`.

use std::collections::HashMap;

/// Opaque wrapper for a secret value stored in the registry.
/// Zeroes its heap buffer on drop. No Debug impl.
struct Secret(Vec<u8>);

impl Secret {
    fn new(value: String) -> Self {
        Self(value.into_bytes())
    }

    fn as_str(&self) -> &str {
        // Values were constructed from valid UTF-8 strings.
        std::str::from_utf8(&self.0).unwrap_or("")
    }
}

impl Drop for Secret {
    fn drop(&mut self) {
        for byte in self.0.iter_mut() {
            *byte = 0;
        }
    }
}

// Explicitly no Debug impl — the registry must never print secret values.

/// Per-run collection of secret names and their resolved values.
///
/// Populated as `(secret :name)` is called during CI execution.
/// Used by [`redact`] to scrub output before persistence.
///
/// Lifetime is bounded to a single CI run. Do not carry a registry
/// across runs — values from previous runs would contaminate
/// redaction of unrelated output.
pub struct SecretRegistry {
    /// name → value (opaque, zeroed on drop)
    secrets: HashMap<String, Secret>,
}

impl Default for SecretRegistry {
    fn default() -> Self {
        Self::new()
    }
}

impl SecretRegistry {
    pub fn new() -> Self {
        Self {
            secrets: HashMap::new(),
        }
    }

    /// Register a resolved secret value under the given name.
    /// Values shorter than 8 characters are ignored — they're too
    /// short to redact safely (high false-positive rate on common
    /// short strings like "set", "yes", "true", "no").
    pub fn register(&mut self, name: impl Into<String>, value: impl AsRef<str>) {
        let name = name.into();
        let value = value.as_ref().to_string();
        if value.len() >= 8 {
            self.secrets.insert(name, Secret::new(value));
        }
    }

    /// Return registered (name, value) pairs sorted by value length
    /// descending so longest matches are replaced first (prevents
    /// partial replacement of overlapping secrets).
    fn entries(&self) -> Vec<(&str, &str)> {
        let mut entries: Vec<_> = self
            .secrets
            .iter()
            .map(|(k, v)| (k.as_str(), v.as_str()))
            .collect();
        entries.sort_by_key(|b| std::cmp::Reverse(b.1.len()));
        entries
    }

    pub fn is_empty(&self) -> bool {
        self.secrets.is_empty()
    }
}

/// Replace any registered secret value in `text` with `{{ name }}`.
///
/// Longest values are replaced first to prevent partial matches.
/// Returns the input unchanged when the registry is empty.
pub fn redact(text: &str, registry: &SecretRegistry) -> String {
    if registry.is_empty() {
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
    fn redact_replaces_secret_value() {
        let mut reg = SecretRegistry::new();
        reg.register("github_token", "ghp_abc123xyz");
        assert_eq!(
            redact("push with token ghp_abc123xyz failed", &reg),
            "push with token {{ github_token }} failed"
        );
    }

    #[test]
    fn redact_handles_multiple_secrets() {
        let mut reg = SecretRegistry::new();
        reg.register("token_a", "aaaaaaaa");
        reg.register("token_b", "bbbbbbbb");
        let result = redact("aaaaaaaa and bbbbbbbb", &reg);
        assert_eq!(result, "{{ token_a }} and {{ token_b }}");
    }

    #[test]
    fn redact_longest_first_prevents_partial_overlap() {
        let mut reg = SecretRegistry::new();
        reg.register("short", "abcdefgh");
        reg.register("long", "abcdefghijklmnop");
        assert_eq!(redact("abcdefghijklmnop here", &reg), "{{ long }} here");
    }

    #[test]
    fn redact_returns_unchanged_when_empty() {
        let reg = SecretRegistry::new();
        assert_eq!(redact("nothing to see", &reg), "nothing to see");
    }

    #[test]
    fn redact_ignores_short_secrets() {
        let mut reg = SecretRegistry::new();
        reg.register("tiny", "abcdefg");
        assert_eq!(redact("abcdefg is short", &reg), "abcdefg is short");
    }

    #[test]
    fn redact_similar_but_not_equal_passes_through() {
        let mut reg = SecretRegistry::new();
        reg.register("token", "ghp_abc123xyz");
        assert_eq!(
            redact("ghp_abc124xyz is close but not equal", &reg),
            "ghp_abc124xyz is close but not equal"
        );
    }

    #[test]
    fn redact_replaces_all_occurrences() {
        let mut reg = SecretRegistry::new();
        reg.register("key", "secret_password");
        assert_eq!(
            redact("secret_password secret_password secret_password", &reg),
            "{{ key }} {{ key }} {{ key }}"
        );
    }

    #[test]
    fn minimum_length_is_8() {
        let mut reg = SecretRegistry::new();
        // 7 chars — too short
        reg.register("short", "1234567");
        assert_eq!(redact("1234567", &reg), "1234567");

        // 8 chars — just enough
        reg.register("ok", "12345678");
        assert_eq!(redact("12345678", &reg), "{{ ok }}");
    }
}
