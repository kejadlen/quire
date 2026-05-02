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

## See also

- [`fennel.md`](fennel.md) — how Fennel files are loaded into Rust structs.
- `src/quire.rs` — `GlobalConfig` definition.
- `src/secret.rs` — `SecretString` implementation and tests.
