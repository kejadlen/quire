# Config files

Quire reads a Fennel config file at `/var/quire/config.fnl` on the
bind-mounted volume. It is pure data — a single top-level table —
loaded via the embedding described in [`fennel.md`](fennel.md).

## Global config

Lives at `/var/quire/config.fnl` on the bind-mounted volume.
Operator-created. Re-read on every call (no caching today).

| Key            | Type           | Required | Purpose                                                  |
|----------------|----------------|----------|----------------------------------------------------------|
| `:sentry :dsn` | `SecretString` | no       | Sentry DSN for error reporting. Omit to disable.         |
| `:secrets`     | table          | no       | Named secrets exposed to `ci.fnl` jobs as `(secret :name)`. |

Minimal (no Sentry, no secrets):

```fennel
{}
```

With Sentry, secrets, and the token sourced from a Docker secret:

```fennel
{:sentry {:dsn "https://key@o0.ingest.sentry.io/0"}
 :secrets {:github_token {:file "/run/secrets/github_token"}}}
```

A missing file is a typed error (`Error::ConfigNotFound`). A malformed
file surfaces as a Fennel parse or eval error with source labels.

## Per-repo config

Mirroring is handled by CI jobs defined in `.quire/ci.fnl`. Per-repo
config at `.quire/config.fnl` is reserved for future use (notifications,
visibility settings, etc.).

## SecretString values

Any field typed as `SecretString` accepts two shapes:

- A plain string: `"hunter2"`.
- A file reference: `{:file "/run/secrets/github_token"}`.

File references are resolved on first call to `.reveal()` and cached
for the lifetime of the parsed value. A single trailing newline is
stripped (Docker secrets convention); additional trailing newlines are
preserved.

The `Debug` impl redacts the value, so a config struct slipping into a
`tracing::debug!` call won't leak the secret. Calling `.reveal()` and
logging the result bypasses this — don't.

## Secret redaction in CI output

Resolved secret values are scrubbed from CI output before persistence.
When a job calls `(secret :name)`, the returned value is registered
for the run; later appearances in `(sh ...)` stdout, stderr, or
recorded command strings are replaced with `{{ name }}` in:

- The CRI log files written under each run's workspace.
- The `sh_events.cmd` column.
- Any other `ShOutput`-derived persistence.

Limits worth knowing:

- Values shorter than 8 bytes are not registered. Common short
  strings like `"true"` or `"yes"` would otherwise produce
  unacceptable false-positive replacements. A `WARN`-level trace
  event is emitted when a short value is skipped, so an operator
  can see why a particular token is showing up unredacted.
- Encoded forms (base64, URL-encoded, hex) are not registered. A
  job that emits the secret in a transformed form is on its own.
- The value returned by `(secret :name)` to the Lua caller is the
  raw secret; subsequent `(sh ...)` calls composed from it have
  their *recorded* output redacted at record time.
- Tracing output is not yet redacted (tracked separately).

## See also

- [`fennel.md`](fennel.md) — how Fennel files are loaded into Rust structs.
- `src/quire.rs` — `GlobalConfig` definition.
- `src/secret.rs` — `SecretString` implementation and tests.
