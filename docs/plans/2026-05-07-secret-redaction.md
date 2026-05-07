# Secret Redaction Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Prevent resolved secret values from appearing in CI run logs, recorded command strings, database columns, or application tracing output.

**Architecture:** A per-run `SecretRegistry` collects `(name, value)` pairs as `(secret :name)` is called during CI execution. A single `redact(text: &str, registry: &SecretRegistry) -> String` function replaces any registered secret value with `{{ name }}`. The registry is passed through the `Runtime` to each surface that records output: `ShOutput` fields are redacted before persistence, CRI log files are written with redacted content, and a tracing layer redacts event fields before emit.

**Tech Stack:** Rust, existing `SecretString` type, `tracing-subscriber` layer API

---

## File Structure

| File | Responsibility |
|------|---------------|
| `src/ci/redact.rs` (new) | `SecretRegistry`, `redact()` function, rolling buffer for streaming |
| `src/ci/runtime.rs` (modify) | Hold registry, populate on `(secret :name)`, pass to sh output recording |
| `src/ci/logs.rs` (modify) | Accept registry, redact lines before writing CRI log |
| `src/ci/run.rs` (modify) | Redact `output.cmd` and `output.stdout`/`output.stderr` before DB insert |
| `src/bin/quire/main.rs` (modify) | Install tracing redaction layer |

---

### Task 1: Create the SecretRegistry and redact function

**Files:**
- Create: `src/ci/redact.rs`
- Modify: `src/ci/mod.rs`

- [ ] **Step 1: Write the failing tests for SecretRegistry and redact**

In `src/ci/redact.rs`:

```rust
//! Secret redaction for CI output surfaces.
//!
//! Collects resolved secret values into a per-run registry and
//! provides a `redact` function that replaces any registered value
//! with `{{ name }}`.

use std::collections::HashMap;

/// Per-run collection of secret names and their resolved values.
///
/// Populated as `(secret :name)` is called during CI execution.
/// Used by `redact()` to scrub output before persistence.
#[derive(Clone, Default)]
pub struct SecretRegistry {
    /// name → value
    secrets: HashMap<String, String>,
}

impl SecretRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    /// Register a resolved secret value under the given name.
    /// Values shorter than 3 characters are ignored — they're too
    /// short to redact safely (high false-positive rate on common
    /// short strings like "0", "1", "no").
    pub fn register(&mut self, name: impl Into<String>, value: impl AsRef<str>) {
        let name = name.into();
        let value = value.as_ref().to_string();
        if value.len() >= 3 {
            self.secrets.insert(name, value);
        }
    }

    /// Return an iterator over registered (name, value) pairs,
    /// sorted by value length descending so longest matches are
    /// replaced first (prevents partial replacement of overlapping
    /// secrets).
    fn entries(&self) -> Vec<(&str, &str)> {
        let mut entries: Vec<_> = self
            .secrets
            .iter()
            .map(|(k, v)| (k.as_str(), v.as_str()))
            .collect();
        entries.sort_by(|a, b| b.1.len().cmp(&a.1.len()));
        entries
    }
}

/// Replace any registered secret value in `text` with `{{ name }}`.
///
/// Longest values are replaced first to prevent partial matches.
/// Returns the input unchanged when the registry is empty.
pub fn redact(text: &str, registry: &SecretRegistry) -> String {
    if registry.secrets.is_empty() {
        return text.to_string();
    }
    let mut result = text.to_string();
    for (name, value) in registry.entries() {
        result = result.replace(value, &format!("{{{{{name}}}}}"));
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn redact_replaces_secret_value() {
        let mut reg = SecretRegistry::new();
        reg.register("github_token", "ghp_abc123");
        assert_eq!(
            redact("push with token ghp_abc123 failed", &reg),
            "push with token {{ github_token }} failed"
        );
    }

    #[test]
    fn redact_handles_multiple_secrets() {
        let mut reg = SecretRegistry::new();
        reg.register("token_a", "aaa");
        reg.register("token_b", "bbb");
        let result = redact("aaa and bbb", &reg);
        assert_eq!(result, "{{ token_a }} and {{ token_b }}");
    }

    #[test]
    fn redact_longest_first_prevents_partial_overlap() {
        let mut reg = SecretRegistry::new();
        reg.register("short", "abc");
        reg.register("long", "abcdef");
        assert_eq!(
            redact("abcdef here", &reg),
            "{{ long }} here"
        );
    }

    #[test]
    fn redact_returns_unchanged_when_empty() {
        let reg = SecretRegistry::new();
        assert_eq!(redact("nothing to see", &reg), "nothing to see");
    }

    #[test]
    fn redact_ignores_short_secrets() {
        let mut reg = SecretRegistry::new();
        reg.register("tiny", "ab");
        assert_eq!(redact("ab is short", &reg), "ab is short");
    }

    #[test]
    fn redact_similar_but_not_equal_passes_through() {
        let mut reg = SecretRegistry::new();
        reg.register("token", "ghp_abc123");
        assert_eq!(
            redact("ghp_abc124 is close but not equal", &reg),
            "ghp_abc124 is close but not equal"
        );
    }

    #[test]
    fn redact_replaces_all_occurrences() {
        let mut reg = SecretRegistry::new();
        reg.register("key", "secret");
        assert_eq!(
            redact("secret secret secret", &reg),
            "{{ key }} {{ key }} {{ key }}"
        );
    }
}
```

- [ ] **Step 2: Register the module in src/ci/mod.rs**

Add `pub mod redact;` to `src/ci/mod.rs`.

- [ ] **Step 3: Run tests to verify they pass**

Run: `cargo test -p quire redact -q`
Expected: All 7 tests pass.

- [ ] **Step 4: Commit**

```
Add SecretRegistry and redact function for CI output
```

---

### Task 2: Populate the registry on (secret :name) calls

**Files:**
- Modify: `src/ci/runtime.rs`

- [ ] **Step 1: Add SecretRegistry to Runtime**

Add a `registry: RefCell<SecretRegistry>` field to `Runtime`. Initialize it in `Runtime::new`. Expose a public method `register_secret` that delegates to the inner registry.

- [ ] **Step 2: Hook into the (secret :name) Lua binding**

In the `"secret"` Lua binding closure (around `runtime.rs:324-327`), after calling `rt.secret(&name)`, also call `rt.register_secret(&name, &value)` to record the resolved value.

- [ ] **Step 3: Expose a read accessor for the registry**

Add `pub(super) fn registry(&self) -> std::cell::Ref<'_, SecretRegistry>` (and `RefMut` variant) so callers in `logs.rs` and `run.rs` can access it.

- [ ] **Step 4: Write a test**

Test that calling `(secret :github_token)` populates the registry with the resolved value.

- [ ] **Step 5: Run tests**

Run: `cargo test -p quire -q`
Expected: All tests pass.

- [ ] **Step 6: Commit**

```
Populate SecretRegistry when (secret :name) is called
```

---

### Task 3: Redact ShOutput before persistence

**Files:**
- Modify: `src/ci/runtime.rs` (the `sh` method)
- Modify: `src/ci/run.rs` (the DB insert path)

The `Runtime::sh` method records `ShOutput` into `self.outputs`. Redact `stdout`, `stderr`, and `cmd` fields in the clone that gets pushed to `self.outputs`. The original (unredacted) value is returned to the Lua caller so the Fennel script can use it programmatically.

- [ ] **Step 1: Redact output before recording in Runtime::sh**

In `Runtime::sh`, after getting the `ShOutput`, clone it and run `redact` on `stdout`, `stderr`, and `cmd` before pushing to `self.outputs`. The unredacted clone is still returned to the caller.

- [ ] **Step 2: Write tests**

Test that a recorded output has redacted fields while the returned value preserves the original. Use a runtime with a registered secret, call sh with output containing the secret, check both.

- [ ] **Step 3: Run tests**

Run: `cargo test -p quire -q`
Expected: All tests pass.

- [ ] **Step 4: Commit**

```
Redact stdout/stderr/cmd in recorded ShOutput
```

---

### Task 4: Redact CRI log file contents

**Files:**
- Modify: `src/ci/logs.rs`
- Modify: `src/ci/run.rs` (caller of `write_cri_log`)

Since Task 3 already redacts the `ShOutput` before it reaches `write_cri_log` and the DB insert path, CRI log files and the `sh_events.cmd` column are already covered. Verify this by checking that `write_cri_log` receives already-redacted output.

- [ ] **Step 1: Verify CRI logs receive redacted content**

The `write_cri_log` function receives a `&ShOutput` — since Task 3 redacts before recording, the output stored in `self.outputs` is already redacted. The caller in `run.rs` passes the recorded (redacted) output to `write_cri_log`. No changes needed to `logs.rs`.

- [ ] **Step 2: Write an integration-style test**

Create a test that sets up a runtime with a registered secret, runs a shell command that emits the secret, records the output, and verifies the CRI log file contains `{{ name }}` instead of the raw value.

- [ ] **Step 3: Run tests**

Run: `cargo test -p quire -q`
Expected: All tests pass.

- [ ] **Step 4: Commit**

```
Verify CRI log files contain redacted secrets
```

---

### Task 5: Add tracing redaction layer

**Files:**
- Create: `src/bin/quire/tracing_redact.rs` (or inline in main.rs)
- Modify: `src/bin/quire/main.rs`

This is the trickiest surface. A `tracing_subscriber::Layer` that inspects event field values and redacts any registered secret. Since the registry is per-run and tracing is process-global, this needs careful design.

- [ ] **Step 1: Implement a simple string-redacting tracing layer**

Create a `tracing_subscriber::Layer` implementation that wraps the inner fmt layer. On each event, it inspects string-valued fields and runs `redact` against them. Since the registry is per-run (not available at subscriber install time), use a simpler approach: maintain a global `Arc<RwLock<HashSet<String>>>` of known secret values (not names — we don't need the name mapping in logs, just scrubbing). The per-run registry updates this set when secrets are resolved.

- [ ] **Step 2: Wire the layer into init_tracing**

Install the redacting layer in the subscriber stack in `init_tracing`.

- [ ] **Step 3: Update SecretRegistry to sync with the global set**

When `SecretRegistry::register` is called (on the per-run instance), also insert the value into the global set.

- [ ] **Step 4: Write tests**

Test that a tracing event containing a secret value emits the redacted form.

- [ ] **Step 5: Run tests**

Run: `cargo test -p quire -q`
Expected: All tests pass.

- [ ] **Step 6: Commit**

```
Add tracing layer to redact secrets from application logs
```

---

### Task 6: Audit remaining DB columns

**Files:**
- Modify: `src/ci/run.rs` (if needed)

- [ ] **Step 1: Scan schema for text columns that might carry secret-derived values**

Check `migrations/` for the schema. Columns to audit: `runs.failure_reason`, `sh_events.cmd`, and any other text columns. Most columns (sha, ref_name, state, job_id, container_id, workspace_path, image_tag) are system-generated and don't carry user strings.

- [ ] **Step 2: Add redaction at insert points if needed**

If any columns receive user-derived text that could contain secrets, add redaction at the INSERT/UPDATE sites. Given Task 3 already covers `sh_events.cmd`, this is likely a no-op — but verify and document.

- [ ] **Step 3: Commit**

```
Audit DB text columns for secret exposure
```

---

### Task 7: Update docs

**Files:**
- Modify: `docs/PLAN.md` or `README.md` as appropriate

- [ ] **Step 1: Document the redaction behavior**

Add a section noting that secret values are redacted from CI logs, recorded commands, and tracing output. Note the `{{ name }}` format and the minimum secret length (3 chars). Note that base64-encoded forms are not registered.

- [ ] **Step 2: Commit**

```
Document secret redaction in output surfaces
```
