# `(ci.mirror …)` helper

**Status:** Superseded. The registration-time `ci.mirror` primitive was
removed. Mirroring is done via the stdlib `mirror` function inside a
`(ci.job …)` run-fn — see `docs/CI-FENNEL.md` for the current pattern.
This doc is kept for historical context but no longer reflects the
active design.

## Surface

```fennel
(ci.mirror "https://github.com/owner/repo.git"
  {:secret :github_token
   :after  [:build]})
```

Two arguments:

1. **URL** (string) — the remote to push to. Required.
2. **Options** (table):
   - `:secret <name>` — name in the global `:secrets` map. Required.
     Resolved at run time. Auth-less remotes are not yet supported.
   - `:tag <fn>` — required callback that returns the tag name. Called
     at execute time with the push table (`{: sha : ref : pushed-at :
     git-dir}`); the return value is the tag name applied to
     `push.sha` and pushed alongside the refs. Lets the operator
     encode their own tag scheme without the helper baking one in.

     Example:

     ```fennel
     :tag (fn [{: sha}]
            (.. "v" (os.date "!%Y-%m-%d") "-" (string.sub sha 1 8)))
     ```
   - `:refs <list>` — refs to push. Defaults to `[]`, which means "push
     the triggering ref" (`push.ref`). A non-empty list pushes those
     refs verbatim regardless of which ref triggered the run.
   - `:after <list>` — extra job dependencies for sequencing.
     Defaults to `[]`. The mirror always depends on `:quire/push`
     internally; `:after` adds further upstream jobs the mirror should
     wait on (e.g. `[:build]` so a failed build skips the mirror).
   - `:as <id>` — alternate internal-job id. Defaults to
     `quire/mirror`. Reserved for the multi-mirror case; not exercised
     in v0.

The auth flow is hardcoded to GitHub-style HTTP Basic with
`x-access-token` username, base64-encoded into an
`http.extraheader` config. Add a `:auth` knob when a second forge
actually needs a different shape.

## Singleton

Calling `(ci.mirror …)` twice in the same `ci.fnl` is a registration
error: `DefinitionError::DuplicateMirror`. Same shape as
`DuplicateImage` — pipeline-level singleton, span on the duplicate
call. The `:as` opt-out exists for the rare multi-mirror case but is
deferred until that case shows up in practice.

## Desugaring

The helper registers a single internal job at id `quire/mirror`,
inputs `[:quire/push, …after]`, with a Rust-implemented run-fn that:

1. Reads `push.sha`, `push.ref`, and `push.git-dir` from
   `(jobs :quire/push)`.
2. Resolves the secret named by `:secret` from the global secrets map.
3. Calls `:tag` with the push table to get the tag name, then
   `git tag <name> <sha>` locally. Tagging failure is a job error.
4. Builds the auth header (base64 of `x-access-token:<token>` as
   HTTP Basic).
5. Spawns `git push <url> <refspecs…> refs/tags/<tag>` where
   `<refspecs…>` is `:refs` if non-empty, otherwise just `push.ref`.
   `GIT_DIR` and the `http.extraheader` config are set via env.
6. Records the result(s) via the runtime's sh-capture channel so they
   show up in the run log alongside any other shell output.

For v0 the recorded output flows through the existing sh-capture map
(used for log streaming). When the `(output …)` primitive lands as
part of the foundation work, the helper switches to publishing
structured outputs (`:tag-name`, `:tag-result`, `:push-result`) that
downstream jobs can read via `(jobs :quire/mirror)`.

## Failure modes

Registration-time errors land as `DefinitionError`s, rendered with a
span at the call site via miette:

- `DuplicateMirror` — second `(ci.mirror …)` call.
- `InvalidMirrorCall { message }` — opt-shape problems caught at
  registration: missing `:tag`, missing `:secret`, unknown opt key
  (e.g. typo'd `:tagPrefix`), `:tag` not a function. Note: these
  check the call shape, not the contents. Whether the named secret
  exists in the global config is checked at run time and surfaces as
  `Error::UnknownSecret` then.

Run-time failures (network, auth rejection, push rejection) surface
as a non-zero `:exit` in the recorded output, the same as any `(sh
…)` failure. The job is marked failed by the executor's existing
non-zero-exit handling; mirror status is visible in the run log.

## Migration from raw `(sh …)`

The current single mirror in `.quire/ci.fnl` is the twelve-line form
in `docs/plans/2026-04-29-ci-fnl-mirror-design.md` lines 22–35.
After this lands, that becomes:

```fennel
(ci.mirror "https://github.com/owner/repo.git"
  {:secret :github_token
   :tag    (fn [{: sha}]
             (.. "v" (os.date "!%Y-%m-%d") "-" (string.sub sha 1 8)))})
```

No backward-compatibility shim. The repo using the raw form gets
updated by hand in the same change. One operator, one repo, no
stakes.

## What this doesn't cover

- The internal-job mechanism (`:quire/`-namespaced jobs registered
  by quire, exempt from `EmptyInputs`/`ReservedSlash` user-facing
  rules) — separate design.
- The `(output key value)` runtime primitive — same separate design.
- Turning `:quire/push` into a real graph node — same separate
  design.
- Container-era changes. The helper's *implementation* will change
  when CI moves to containers (different git invocation, secret
  injection mechanism), but the surface above stays stable. That's
  the whole point of having a helper.

## Open questions

1. **`--mirror`-style "push everything" semantics.** Listing every
   ref by hand in `:refs` is workable for one or two named refs but
   awkward at scale. If a future use case wants "send all refs and
   delete remote refs that disappeared," add a sentinel (`:all`,
   `:mirror`, or similar) that maps to `git push --mirror`. Not
   needed today; one operator, one repo, named refs.

2. **SSH / non-HTTPS remotes.** A bare `(ci.mirror
   "git@host:foo.git")` could imply ssh-with-host-keys. Probably
   overscope for v0 — require `:secret` and only support HTTPS for
   now. Revisit when a non-HTTPS use case shows up.
