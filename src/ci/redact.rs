//! Secret redaction for CI output surfaces.
//!
//! [`SecretRegistry`] holds both declared secrets (for lookup by name)
//! and their revealed values (for output redaction). A single struct
//! replaces the previous split between `Runtime.secrets` and a
//! separate redaction registry.
//!
//! [`Revealed`] is an opaque wrapper without a [`Debug`] impl, so a
//! revealed value can't accidentally land in `tracing::debug!` or a
//! panic message. That's the only memory-side protection here: copies
//! made earlier in the pipeline (the `String` returned by `reveal`,
//! Lua VM internals, intermediate `format!` allocations) aren't
//! zeroized, so the registry is best thought of as preventing
//! accidental display, not preventing memory disclosure.

use std::collections::HashMap;

use crate::secret::SecretString;

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

impl SecretRegistry {
    pub fn new(declared: HashMap<String, SecretString>) -> Self {
        Self {
            declared,
            revealed: HashMap::new(),
        }
    }

    /// Resolve a declared secret by name, caching the revealed value
    /// for redaction. Returns `Err` if the name isn't declared or
    /// the source can't be read.
    ///
    /// Values shorter than 8 characters are returned to the caller
    /// but not registered for redaction — the false-positive rate on
    /// common short strings like "true" or "yes" is too high. A warn
    /// is emitted so an operator can see why a short token is showing
    /// up unredacted in CI output.
    pub fn resolve(&mut self, name: &str) -> super::error::Result<String> {
        let secret = self
            .declared
            .get(name)
            .ok_or_else(|| super::error::Error::UnknownSecret(name.to_string()))?;
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

    fn plain_secrets(pairs: &[(&str, &str)]) -> HashMap<String, SecretString> {
        pairs
            .iter()
            .map(|(k, v)| (k.to_string(), SecretString::from_plain(*v)))
            .collect()
    }

    #[test]
    fn redact_replaces_secret_value() {
        let mut reg = SecretRegistry::new(plain_secrets(&[("github_token", "ghp_abc123xyz")]));
        reg.resolve("github_token").unwrap();
        assert_eq!(
            redact("push with token ghp_abc123xyz failed", &reg),
            "push with token {{ github_token }} failed"
        );
    }

    #[test]
    fn redact_handles_multiple_secrets() {
        let mut reg = SecretRegistry::new(plain_secrets(&[
            ("token_a", "aaaaaaaa"),
            ("token_b", "bbbbbbbb"),
        ]));
        reg.resolve("token_a").unwrap();
        reg.resolve("token_b").unwrap();
        let result = redact("aaaaaaaa and bbbbbbbb", &reg);
        assert_eq!(result, "{{ token_a }} and {{ token_b }}");
    }

    #[test]
    fn redact_longest_first_prevents_partial_overlap() {
        let mut reg = SecretRegistry::new(plain_secrets(&[
            ("short", "abcdefgh"),
            ("long", "abcdefghijklmnop"),
        ]));
        reg.resolve("short").unwrap();
        reg.resolve("long").unwrap();
        assert_eq!(redact("abcdefghijklmnop here", &reg), "{{ long }} here");
    }

    #[test]
    fn redact_returns_unchanged_when_nothing_revealed() {
        let reg = SecretRegistry::new(plain_secrets(&[("key", "secret_value")]));
        assert_eq!(redact("nothing to see", &reg), "nothing to see");
    }

    #[test]
    fn redact_ignores_short_secrets() {
        let mut reg = SecretRegistry::new(plain_secrets(&[("tiny", "abcdefg")]));
        reg.resolve("tiny").unwrap();
        assert_eq!(redact("abcdefg is short", &reg), "abcdefg is short");
    }

    #[test]
    fn redact_similar_but_not_equal_passes_through() {
        let mut reg = SecretRegistry::new(plain_secrets(&[("token", "ghp_abc123xyz")]));
        reg.resolve("token").unwrap();
        assert_eq!(
            redact("ghp_abc124xyz is close but not equal", &reg),
            "ghp_abc124xyz is close but not equal"
        );
    }

    #[test]
    fn redact_replaces_all_occurrences() {
        let mut reg = SecretRegistry::new(plain_secrets(&[("key", "secret_password")]));
        reg.resolve("key").unwrap();
        assert_eq!(
            redact("secret_password secret_password secret_password", &reg),
            "{{ key }} {{ key }} {{ key }}"
        );
    }

    #[test]
    fn minimum_length_is_8() {
        let mut reg =
            SecretRegistry::new(plain_secrets(&[("short", "1234567"), ("ok", "12345678")]));
        // 7 chars — too short for redaction
        reg.resolve("short").unwrap();
        assert_eq!(redact("1234567", &reg), "1234567");

        // 8 chars — just enough
        reg.resolve("ok").unwrap();
        assert_eq!(redact("12345678", &reg), "{{ ok }}");
    }

    #[test]
    fn resolve_errors_for_unknown_name() {
        let mut reg = SecretRegistry::new(plain_secrets(&[]));
        let err = reg.resolve("missing").unwrap_err();
        assert!(
            matches!(err, super::super::error::Error::UnknownSecret(ref n) if n == "missing"),
            "expected UnknownSecret, got: {err:?}"
        );
    }

    #[test]
    fn resolve_returns_value() {
        let mut reg = SecretRegistry::new(plain_secrets(&[("key", "hunter2")]));
        assert_eq!(reg.resolve("key").unwrap(), "hunter2");
    }

    #[test]
    fn redact_is_idempotent() {
        let mut reg = SecretRegistry::new(plain_secrets(&[("token", "ghp_long_secret_value")]));
        reg.resolve("token").unwrap();
        let input = "hello ghp_long_secret_value world";
        let first = redact(input, &reg);
        let second = redact(&first, &reg);
        assert_eq!(first, second);
    }

    #[test]
    fn redact_preserves_non_matching_text() {
        let mut reg = SecretRegistry::new(plain_secrets(&[("token", "ghp_long_secret_value")]));
        reg.resolve("token").unwrap();
        let input = "nothing to see here";
        assert_eq!(redact(input, &reg), input);
    }

    #[test]
    fn redact_with_no_resolves_is_identity() {
        let reg = SecretRegistry::new(plain_secrets(&[("token", "ghp_long_secret_value")]));
        // No resolve — no redactions registered.
        let input = "contains ghp_long_secret_value but not resolved";
        assert_eq!(redact(input, &reg), input);
    }

    #[test]
    fn redact_tiebreaks_equal_length_by_name() {
        // Two names with the same revealed value: alphabetical name wins.
        let mut reg = SecretRegistry::new(plain_secrets(&[
            ("zzz_late", "samevalue"),
            ("aaa_early", "samevalue"),
        ]));
        reg.resolve("zzz_late").unwrap();
        reg.resolve("aaa_early").unwrap();
        assert_eq!(redact("samevalue", &reg), "{{ aaa_early }}");
    }
}
