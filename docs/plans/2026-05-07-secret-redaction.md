# Secret Redaction Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Prevent resolved secret values from appearing in CI run logs, recorded command strings, and database columns.

**Architecture:** A per-run `SecretRegistry` collects `(name, value)` pairs as `(secret :name)` is called during CI execution. A single `redact(text: &str, registry: &SecretRegistry) -> String` function replaces any registered secret value with `{{ name }}`. The registry is threaded through `Runtime` and applied to `ShOutput` before persistence.

**Tech Stack:** Rust, existing `SecretString` type

---

## File Structure

| File | Responsibility |
|------|---------------|
| `src/ci/redact.rs` | `SecretRegistry`, `redact()` function |
| `src/ci/runtime.rs` | Hold registry, populate on `(secret :name)`, redact on recording |
| `src/ci/run.rs` | Receives already-redacted output for DB insert and CRI log |

---

### Task 1: Create the SecretRegistry and redact function

**Files:**
- Create: `src/ci/redact.rs`
- Modify: `src/ci/mod.rs`

- [x] **Step 1: Write SecretRegistry and redact with tests**
- [x] **Step 2: Register the module in src/ci/mod.rs**
- [x] **Step 3: Run tests**
- [x] **Step 4: Commit**

---

### Task 2: Populate the registry on (secret :name) calls

**Files:**
- Modify: `src/ci/runtime.rs`

- [x] **Step 1: Add SecretRegistry to Runtime**
- [x] **Step 2: Hook into the (secret :name) Lua binding**
- [x] **Step 3: Expose a read accessor for the registry**
- [x] **Step 4: Run tests**
- [x] **Step 5: Commit**

---

### Task 3: Redact ShOutput before persistence

**Files:**
- Modify: `src/ci/runtime.rs` (the `sh` method)

The `Runtime::sh` method records `ShOutput` into `self.outputs`. Redact `stdout`, `stderr`, and `cmd` fields in the clone that gets pushed to `self.outputs`. The original (unredacted) value is returned to the Lua caller so the Fennel script can use it programmatically.

- [x] **Step 1: Redact output before recording in Runtime::sh**
- [x] **Step 2: Run tests**
- [x] **Step 3: Commit**

---

### Task 4: Verify CRI log files and DB columns are covered

No code changes needed. Since Task 3 redacts the `ShOutput` before it reaches `write_cri_log` and the DB insert path, both surfaces are covered.

Schema audit — text columns that could carry user-derived text:
- `sh_events.cmd` — covered by Task 3 (redacted before insert)
- `runs.repo` — git repo name, system-generated
- `runs.ref_name` — git ref, system-generated
- `runs.sha` — commit hash, system-generated
- `runs.failure_kind` — enum tag, not user text
- `runs.container_id`, `image_tag`, `workspace_path` — system-generated

No additional redaction sites needed beyond `sh_events.cmd`.

- [x] **Step 1: Verify CRI logs and DB receive redacted content**
- [x] **Step 2: Commit**

---

### Task 5: Tracing redaction — DEFERRED

Not included in v1. The current approach is to audit existing trace call sites
for fields that could carry secret-derived values and remove them at the source.
A tracing-subscriber layer is not feasible with the current architecture:
`Layer` cannot rewrite field values mid-event (fields are `dyn Value` consumed
by the formatter via `Visit`). A future approach would use a custom `MakeWriter`
for post-format byte rewriting, but this adds significant complexity for
uncertain benefit. Revisit if secret leakage through application logs becomes
a practical problem.

---

### Task 6: Update docs

**Files:**
- Modify: `docs/PLAN.md` or `README.md` as appropriate

- [ ] **Step 1: Document the redaction behavior**

Add a section noting that secret values are redacted from CI logs, recorded commands, and database columns. Note the `{{ name }}` format and the minimum secret length (8 chars). Note that base64-encoded forms are not registered.

- [ ] **Step 2: Commit**
