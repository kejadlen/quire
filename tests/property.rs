use std::collections::HashMap;

use hegel::TestCase;
use hegel::generators::{integers, just, text, vecs};
use hegel::one_of;
use quire::ci::{SecretRegistry, redact};
use quire::event::{PushEvent, PushRef};
use quire::secret::SecretString;

const ZERO_SHA: &str = "0000000000000000000000000000000000000000";

const MIN_REDACT_LEN: usize = 8;

#[hegel::composite]
fn push_ref(tc: TestCase) -> PushRef {
    PushRef {
        r#ref: tc.draw(text()),
        old_sha: tc.draw(text()),
        // Mix in zero-shas so updated_refs() actually exercises its filter.
        new_sha: tc.draw(one_of![text(), just(ZERO_SHA.to_string())]),
    }
}

#[hegel::composite]
fn push_event(tc: TestCase) -> PushEvent {
    // jiff::Timestamp range: -377705023201..=253402207200 seconds.
    let secs = tc.draw(
        integers::<i64>()
            .min_value(-377_705_023_201)
            .max_value(253_402_207_200),
    );
    PushEvent {
        r#type: tc.draw(text()),
        repo: tc.draw(text()),
        pushed_at: jiff::Timestamp::from_second(secs).expect("timestamp in range"),
        refs: tc.draw(vecs(push_ref()).max_size(8)),
    }
}

#[hegel::test]
fn push_event_round_trips_json(tc: TestCase) {
    let event = tc.draw(push_event());
    let json = serde_json::to_string(&event).expect("serialize");
    let parsed: PushEvent = serde_json::from_str(&json).expect("deserialize");
    assert_eq!(event, parsed);
}

#[hegel::test]
fn updated_refs_excludes_only_zero_sha_deletions(tc: TestCase) {
    let event = tc.draw(push_event());
    let kept = event.updated_refs();

    let expected: Vec<&PushRef> = event
        .refs
        .iter()
        .filter(|r| r.new_sha != ZERO_SHA)
        .collect();
    assert_eq!(kept, expected);

    let deleted = event.refs.iter().filter(|r| r.new_sha == ZERO_SHA).count();
    assert_eq!(kept.len() + deleted, event.refs.len());
}

#[hegel::test]
fn secret_string_debug_never_leaks_plain_value(tc: TestCase) {
    let value = tc.draw(text());
    let secret = SecretString::from(value.clone());
    let debug = format!("{secret:?}");
    assert_eq!(debug, "SecretString(\"<redacted>\")");
}

#[hegel::test]
fn secret_string_plain_json_round_trips(tc: TestCase) {
    let value = tc.draw(text());
    let json = serde_json::to_string(&value).expect("serialize string");
    let secret: SecretString = serde_json::from_str(&json).expect("deserialize SecretString");
    assert_eq!(secret.reveal().expect("plain reveal"), value);
}

#[hegel::test]
fn secret_string_from_file_strips_one_trailing_newline(tc: TestCase) {
    let content = tc.draw(text());
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("secret");
    fs_err::write(&path, &content).expect("write");

    let revealed = SecretString::from(path.clone())
        .reveal()
        .expect("reveal")
        .to_string();
    let expected = content.strip_suffix('\n').unwrap_or(&content).to_string();
    assert_eq!(revealed, expected);
}

#[hegel::test]
fn push_event_repo_round_trips_json(tc: TestCase) {
    // Verify that arbitrary repo names survive JSON serialization.
    let mut event = tc.draw(push_event());
    event.repo = tc.draw(text());
    let json = serde_json::to_string(&event).expect("serialize");
    let parsed: PushEvent = serde_json::from_str(&json).expect("deserialize");
    assert_eq!(event, parsed);
}

#[hegel::test]
fn push_event_updated_refs_is_subtractive(tc: TestCase) {
    // updated_refs() can only remove refs (zero-sha deletions),
    // never add or reorder them.
    let event = tc.draw(push_event());
    let kept = event.updated_refs();
    assert!(kept.len() <= event.refs.len());
    for kept_ref in &kept {
        assert!(
            event
                .refs
                .iter()
                .any(|r| r.r#ref == kept_ref.r#ref && r.new_sha == kept_ref.new_sha)
        );
    }
}

// ── Secret registry helpers ──────────────────────────────────────

/// Generate a secret name from an index.
fn secret_name(i: usize) -> String {
    format!("secret_{i}")
}

#[hegel::composite]
fn unique_secret_entries(tc: TestCase) -> Vec<(String, String)> {
    let count = tc.draw(integers::<usize>().min_value(1).max_value(8));
    let mut seen = std::collections::HashSet::new();
    let mut entries = Vec::new();
    for i in 0..count {
        let value = tc.draw(text().alphabet("abcdefghijklmnopqrstuvwxyz0123456789"));
        let name = secret_name(i);
        if seen.insert(name.clone()) {
            entries.push((name, value));
        }
    }
    entries
}

#[hegel::composite]
fn resolved_registry(tc: TestCase) -> SecretRegistry {
    let entries = tc.draw(unique_secret_entries());
    let mut map = HashMap::new();
    for (name, value) in &entries {
        map.insert(name.clone(), SecretString::from(value.clone()));
    }
    let mut reg = SecretRegistry::from(map);
    // Resolve all secrets so they're registered for redaction.
    for (name, _) in &entries {
        let _ = reg.resolve(name);
    }
    reg
}

#[hegel::composite]
fn text_with_secrets(tc: TestCase) -> (SecretRegistry, String) {
    let entries = tc.draw(unique_secret_entries());
    let mut map = HashMap::new();
    let mut long_values: Vec<String> = Vec::new();
    for (name, value) in &entries {
        map.insert(name.clone(), SecretString::from(value.clone()));
        if value.len() >= MIN_REDACT_LEN {
            long_values.push(value.clone());
        }
    }
    let mut reg = SecretRegistry::from(map);
    for (name, _) in &entries {
        let _ = reg.resolve(name);
    }

    // Build text that intersperses random noise with secret values.
    let mut body = tc.draw(text());
    for (_, value) in &entries {
        if value.len() >= MIN_REDACT_LEN {
            body.push_str(value);
            body.push_str(&tc.draw(text()));
        }
    }
    (reg, body)
}

// ── Secret registry property tests ────────────────────────────────

#[hegel::test]
fn redact_never_contains_revealed_long_values(tc: TestCase) {
    let entries = tc.draw(unique_secret_entries());
    let mut map = HashMap::new();
    let mut long_values: Vec<String> = Vec::new();
    for (name, value) in &entries {
        map.insert(name.clone(), SecretString::from(value.clone()));
        if value.len() >= MIN_REDACT_LEN {
            long_values.push(value.clone());
        }
    }
    let mut reg = SecretRegistry::from(map);
    for (name, _) in &entries {
        let _ = reg.resolve(name);
    }

    // Build text containing all the long values.
    let mut body = tc.draw(text());
    for value in &long_values {
        body.push_str(value);
        body.push_str(&tc.draw(text()));
    }
    let result = redact(&body, &reg);
    for value in &long_values {
        assert!(
            !result.contains(value),
            "redacted text still contains secret value: {value}"
        );
    }
}

#[hegel::test]
fn redact_is_idempotent(tc: TestCase) {
    let (reg, text) = tc.draw(text_with_secrets());
    let first = redact(&text, &reg);
    let second = redact(&first, &reg);
    assert_eq!(first, second);
}

#[hegel::test]
fn redact_preserves_text_without_secrets(tc: TestCase) {
    let reg = tc.draw(resolved_registry());
    let text = tc.draw(text());
    let result = redact(&text, &reg);
    // If no secret value happens to appear in the random text, output is
    // identical. This won't always hold (random text might contain a secret),
    // so only assert when no redaction actually occurred.
    if !reg.has_redactions() {
        assert_eq!(result, text);
    }
}

#[hegel::test]
fn redact_empty_registry_is_identity(tc: TestCase) {
    let reg = SecretRegistry::from(HashMap::new());
    let text = tc.draw(text());
    assert_eq!(redact(&text, &reg), text);
}

#[hegel::test]
fn redact_unresolved_registry_is_identity(tc: TestCase) {
    let entries = tc.draw(unique_secret_entries());
    let map: HashMap<String, SecretString> = entries
        .into_iter()
        .map(|(k, v)| (k, SecretString::from(v)))
        .collect();
    let reg = SecretRegistry::from(map);
    let text = tc.draw(text());
    assert_eq!(redact(&text, &reg), text);
}

#[hegel::test]
fn resolve_returns_consistent_value(tc: TestCase) {
    let entries = tc.draw(unique_secret_entries());
    let (name, value) = entries.into_iter().next().unwrap();
    let mut reg = SecretRegistry::from(vec![(name.as_str(), value.as_str())]);
    let first = reg.resolve(&name).unwrap();
    let second = reg.resolve(&name).unwrap();
    assert_eq!(first, second);
    assert_eq!(first, value);
}

#[hegel::test]
fn resolve_unknown_name_errors(tc: TestCase) {
    let name = tc.draw(text());
    let mut reg = SecretRegistry::from(HashMap::new());
    assert!(reg.resolve(&name).is_err());
}

#[hegel::test]
fn redact_output_never_shows_long_secret_values(tc: TestCase) {
    let entries = tc.draw(unique_secret_entries());
    let mut map = HashMap::new();
    for (name, value) in &entries {
        map.insert(name.clone(), SecretString::from(value.clone()));
    }
    let mut reg = SecretRegistry::from(map);
    for (name, _) in &entries {
        let _ = reg.resolve(name);
    }

    // Concatenate all secret values into one string.
    let mut body = String::new();
    for (_, value) in &entries {
        body.push_str(value);
        body.push(' ');
    }
    let result = redact(&body, &reg);

    // No secret value >= 8 chars should survive.
    for (_, value) in &entries {
        if value.len() >= MIN_REDACT_LEN {
            assert!(!result.contains(value), "long secret leaked: {value}");
        }
    }
}

#[hegel::test]
fn short_secrets_are_never_redacted(tc: TestCase) {
    // Generate values of 1-7 bytes. With ASCII alphabet, char count == byte count.
    let value = tc.draw(text().alphabet("abcdefgh").max_size(7));
    if value.is_empty() {
        return;
    }
    let mut reg = SecretRegistry::from(vec![("short", value.as_str())]);
    let _ = reg.resolve("short");
    let body = value.clone();
    let result = redact(&body, &reg);
    // Values < 8 bytes are never registered for redaction.
    assert_eq!(result, body, "short secret was incorrectly redacted");
}
