# Replace built-in mirror push with a ci.fnl job

**Goal:** Delete `mirror::push` and the `:mirror` / `:github` config keys. Express mirror-to-GitHub as an ordinary CI job in `.quire/ci.fnl`, using a `(sh ...)` runtime primitive and a `(secret ...)` accessor backed by a new `:secrets` map in global config.

## Motivation

Mirror push is currently a hardcoded second branch in the post-receive flow:

```
hook → /var/quire/server.sock
       └── mirror::push (built-in, reads :mirror from repo config)
       └── ci::trigger  (loads ci.fnl, validates)
```

Two paths, two configs (`:mirror` on the repo, `:github :token` global), and a `git push` shelled out from inside `quire serve` with no visibility in the run UI. Every time we want a new "thing that runs after a push" — re-tag, notify Slack, kick a build elsewhere — the choice is "add another built-in" or "wait for CI." Pulling mirror through the CI engine collapses that choice: there is one path, the engine, and built-in work is just code the user wrote.

## End state

`.quire/ci.fnl` for a repo that mirrors to GitHub:

```fennel
(local ci (require :quire.ci))

(ci:job :mirror [:quire/push]
  (fn [{:inputs {:quire/push push}}]
    (let [token  (secret :github_token)
          header (.. "Authorization: Basic "
                     (base64 (.. "x-access-token:" token)))]
      (sh ["git" "push" "--porcelain"
           "https://github.com/owner/repo.git"
           push.ref]
          {:env  {:GIT_DIR              push.git-dir
                  :GIT_CONFIG_COUNT     "1"
                  :GIT_CONFIG_KEY_0     "http.https://github.com/.extraheader"
                  :GIT_CONFIG_VALUE_0   header}}))))
```

Global `config.fnl`:

```fennel
{:secrets {:github_token {:file "/run/secrets/github_token"}}}
```

`mirror::push`, `MirrorConfig`, `:github`, `:mirror`, and `push_to_mirror` all gone.

## Decisions

### Backend: host `sh` against the bare repo (v0)

Jobs reach the bare repo through `:quire/push.git-dir` and shell out on the host. No container, no clone. Matches the existing threat model — operator-authored `ci.fnl`, unsandboxed eval — and keeps the libgit2-shaped behavior (env-var auth header) verbatim.

This is explicitly the v0 stopgap. The next iteration moves CI jobs into containers, at which point:

- `(sh ...)` stays as a primitive but stops being the typical mirror approach.
- `:git-dir` comes off `:quire/push` outputs (containers don't share the host filesystem; we'll need a different mechanism — bind-mount, push-only remote, or a `(git-push ...)` primitive — to be decided then).
- `(secret ...)` extends to `(container ...)` `:env` the same way it extends to `(sh ...)` today.

The shape of the user-visible `ci.fnl` for a mirror job changes between v0 and the container era. That's acceptable: there's one user (the operator), one repo currently mirroring, and the migration is mechanical.

### Secrets: `:secrets` map in global config

New global config shape:

```fennel
{:secrets {:github_token {:file "/run/secrets/github_token"}
           :slack_webhook "https://hooks.slack.com/..."}}
```

Each value is the existing `SecretString` shape — either a literal or `{:file "..."}`. Resolution is lazy (`SecretString::reveal`), cached, file-form strips trailing newline. Nothing here is new code; it's `SecretString` lifted into a generic map.

`:github :token` and `:mirror` get deleted in the same change. No deprecation window.

Per-repo secrets are out of scope for v0 — every `ci.fnl` can read every global secret. The personal-forge threat model (one operator, one author) makes scoping unnecessary.

### Secrets: `(secret :name)` returns the resolved string

Transparent, not opaque. The opaque-userdata variant was considered and rejected: actual use composes the secret into a string (e.g., the `Authorization: Basic <base64(...)>` header) before it reaches `:env`, so the script unwraps it on the first line anyway. Opaqueness past that point buys no protection.

The leakage class this opens — secrets ending up in run logs via job stdout — is tracked separately as ranger task **`muxqyrlp`** ("Redact secrets from CI run logs"). That fix is log-side: scan output as it streams, replace registered secret bytes with `<redacted>` before the file write and the broadcast channel. No change to the `(secret ...)` API.

### `sh` primitive

```
(sh cmd opts?)
```

- `cmd` — string (passed to `sh -c`) or list (argv, no shell).
- `opts` — table with `:env` (map of `string → string`), `:cwd` (string).
- Returns `{:exit :stdout :stderr :duration}`, same shape as `container`.

Runs in `quire serve`'s process tree. Inherits its env unless `:env` overrides specific keys (merge, not replace). Blocks the Fennel function until exit. No timeout for v0 — matches `container`'s "v1 model is sequential" stance.

`:env` map values are plain strings. The runner doesn't need to know which came from `(secret ...)` because the primitive is host-side and there's no boundary to enforce against; the caller already has the bytes in scope.

### `(secret :name)` accessor

Function in scope inside `run` (the per-job function), alongside `container`, `sh`, etc.:

- `(secret :name)` — resolves the named secret from global config, returns the string. Errors if `:name` is not declared.
- Lookup is case-sensitive. Names are arbitrary keywords; the schema doesn't constrain what they mean.

Implementation: `quire serve` parses `:secrets` once at config load. The CI eval scope binds a Fennel function that, when called, looks up the name and calls `SecretString::reveal`. Errors (unknown name, file unreadable) surface as a Fennel error inside the `run` function — the job fails, error in the log, run marked failed. Same path as any other primitive error.

### `:quire/push` gains `:git-dir`

Add one field to the push source's output table:

```
{:sha             "abc123..."
 :ref             "refs/heads/main"
 :branch          "main"
 :tag             nil
 :pusher          "alice"
 :git-dir         "/var/quire/repos/foo.git"   ; NEW — temporary, host-only
 ...}
```

Marked temporary in CI-FENNEL.md. Removed when in-container jobs land. Flagged in the field's doc comment so the next person to touch it knows the deal.

### Removal sequence

One PR, three commits, in order:

1. **Add the new path.** `:secrets` in global config, `(secret ...)` accessor, `(sh ...)` primitive, `:git-dir` on `:quire/push` outputs. Tests for each. Existing `mirror::push` still wired and runs.
2. **Migrate the operator's mirror job.** Update the one repo currently using `:mirror` to use a `ci.fnl` job. Verify the push works end-to-end against GitHub.
3. **Delete the old path.** Remove `mirror::push`, `MirrorConfig`, `MirrorConfig`'s deserializer, `Repo::push_to_mirror`, `github_auth_header`, the `:mirror` parsing, the `:github` config struct. Update CI.md and `mirror::push` callers. Update tests.

Splitting into three commits makes the deletion bisectable: if step 2 misbehaves, we know the new path is at fault; if step 3 breaks something, we know the deletion was the culprit.

## What's not in this design

- **Per-repo secrets.** Global only. Layer in later if multi-tenant ever happens.
- **Container migration.** Tracked as the next iteration. This design names the seams (`:git-dir`, `sh` vs `container`) but doesn't commit to specifics.
- **Log redaction.** Ranger task `muxqyrlp`.
- **Other built-ins worth folding into CI.** E.g. notification webhooks, post-push tagging. Each can become a `ci.fnl` job once this path proves out; not part of this work.
- **`(base64 ...)` and other helpers.** The example uses `base64` casually; we'll need a small set of host-side helpers (`base64`, `json-encode`, maybe `hash`) for jobs to be self-sufficient. Tracked separately when the second helper is needed.

## Open questions

- **Does `sh` get cancellation?** A superseded run kills `(container ...)` via `docker kill`. For `sh`, the runner would need to track the child PID and `kill` it on supersede. Probably yes — otherwise mirror push during a supersede churns. Cheap to add (already tracking sandbox IDs in run state); confirm during implementation.
- **`sh`'s default `:cwd`.** No CI run has a meaningful default — workspace doesn't exist for the mirror case (no `git archive`). Probably default to the workspace dir if it exists, error if `sh` is called without `:cwd` and no workspace. Decide during implementation.
- **Should `:quire/push.git-dir` be a function call (`(repo-dir)`) instead of a field?** Field is more discoverable (it's right there on the input). Function decouples from the push event (you could call it from a non-push-triggered job). Sticking with field for v0; revisit when a non-push source needs the same access.
