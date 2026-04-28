# Config files

Quire reads two Fennel config files. Both are pure data — a single
top-level table — loaded via the embedding described in
[`fennel.md`](fennel.md).

## Global config

Lives at `/var/quire/config.fnl` on the bind-mounted volume.
Operator-created. Re-read on every call (no caching today).

| Key              | Type           | Required | Purpose                                                  |
|------------------|----------------|----------|----------------------------------------------------------|
| `:github :token` | `SecretString` | yes      | GitHub PAT used for `http.extraHeader` on mirror pushes. |
| `:sentry :dsn`   | `SecretString` | no       | Sentry DSN for error reporting. Omit to disable.         |

Minimal:

```fennel
{:github {:token "ghp_xxxxxxxxxxxxxxxxxxxxxxxxxxxx"}}
```

With Sentry, and the token sourced from a Docker secret:

```fennel
{:github {:token {:file "/run/secrets/github_token"}}
 :sentry {:dsn "https://key@o0.ingest.sentry.io/0"}}
```

A missing file is a typed error (`Error::ConfigNotFound`). A malformed
file surfaces as a Fennel parse or eval error with source labels.

## Per-repo config

Lives at `.quire/config.fnl` *checked into the repo* — quire reads it
from `HEAD` of the bare repo via `git show HEAD:.quire/config.fnl`.
Repos without the file (or without a given key) get defaults; this is
a no-op, not an error.

The post-receive hook does not read this config directly. Instead it
sends a JSON push event to `quire serve` over `/var/quire/server.sock`.
The server reads the config to find mirror settings and dispatch the
push. See `src/bin/quire/commands/serve.rs` for the full path.

| Key            | Type     | Required | Purpose                                                                       |
|----------------|----------|----------|-------------------------------------------------------------------------------|
| `:mirror :url` | `String` | no       | HTTPS URL of the mirror remote. URLs with embedded `user:pass@` are rejected. |

Example:

```fennel
{:mirror {:url "https://github.com/owner/repo.git"}}
```

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
- `src/quire.rs` — `GlobalConfig`, `RepoConfig`, `MirrorConfig` definitions.
- `src/secret.rs` — `SecretString` implementation and tests.
