# Split quire-ci from quire-server

Make CI a standalone service. quire-server keeps the git wire protocol,
repo browsing, and push detection. A new `quire-ci serve` mode owns the
webhook receiver, SQLite, runner, run/log web UI, and per-run workspace
materialization.

## Scope

In scope:

- A standalone `quire-ci serve` long-running process.
- A webhook from quire-server to quire-ci on push.
- A fresh `0001` migration for quire-ci's SQLite, post-split shape.
- One Dockerfile with two final-stage targets (`server`, `ci`).
- A staged cutover with quire-server intact through phases 1 and 2.

Out of scope:

- Schema or vocabulary reshape beyond what the split forces (deferred to
  a separate pass once quire-ci is standing on its own).
- A `repos` or `pushes` table. Add when a concrete need lands.
- Per-repo webhook secrets. One shared secret is the right grain.
- The in-pipeline `quire-ci run` subprocess dispatch path. Collapses to
  in-thread execution.

## Service shape

```
                  push                         webhook POST
client (jj/git) ────────► quire-server ────────────────────► quire-ci
                          │                                  │
                          │ git wire, repo browse UI,        │ webhook endpoint,
                          │ post-receive hook,               │ SQLite, runner,
                          │ internal git-http endpoint       │ run/log web UI
                          │                                  │
                          ▼                                  ▼
                    /var/quire/repos          git clone over HTTP from quire-server,
                    (bare repos)              materialize workspace under /var/quire-ci
                                                            │
                                                            ▼
                                                  runner task
                                                  (in-process Fennel VM)
                                                            │
                                       ┌────────────────────┴────────────────────┐
                                       ▼                                         ▼
                                (sh ...) → child process              (sh ...) → docker exec
                                (subprocess phase, now)               (per-run container, later)
```

Push lifecycle:

1. Client pushes. `git-receive-pack` accepts. The `post-receive` hook fires.
2. The hook POSTs JSON to `ci_webhook_url`. Auth via shared secret HMAC
   header. The active span's `traceparent` is propagated as a header.
3. quire-ci's webhook handler inserts a `runs` row, extracts the
   traceparent, and notifies the runner via `tokio::sync::Notify`.
4. The runner picks up the run, clones the repo from quire-server's
   internal git-http endpoint at the SHA in the payload, and materializes
   a workspace under `/var/quire-ci/runs/<id>/workspace/`.
5. The runner calls into the pipeline runtime via
   `tokio::task::spawn_blocking` (Lua is not `Send`-friendly across
   `.await` points).
6. The runtime evaluates `ci.fnl`, walks the DAG, dispatches each
   `(sh ...)`:
   - Subprocess phase (now): spawn a child process directly.
   - Docker phase (later): `docker exec <per-run-container> sh -c "..."`.
7. Events flow back over an in-process `mpsc` channel. The runner
   persists them to SQLite (`jobs`, `sh`) and rebroadcasts via
   `tokio::sync::broadcast` to any web subscribers tailing logs.
8. Secrets stay in-process. The runtime resolves them against quire-ci's
   `SecretRegistry` directly.

What goes away from the current shape:

- `quire-ci run` subprocess dispatch and its JSONL event stream.
- The `/api/run/bootstrap` and `/api/run/secrets/:name` endpoints.
- `run_token` minting and verification.
- `QUIRE__SERVER_URL` / `QUIRE__RUN_TOKEN` env plumbing.
- The musl-static build target. quire-ci is dynamic against glibc.

What stays as a thin CLI:

- `quire-ci validate <path>` and `quire-ci run --local <path>` wrap the
  same in-process pipeline runtime for debugging `ci.fnl` outside the
  orchestrator. One subcommand each, no orchestrator state.

## Repository access

quire-ci clones from quire-server per run. Per-run clone is wasteful but
buys isolation; the clone-and-discard cost is acceptable for v1.

quire-server gains an **internal git-over-HTTP endpoint** for this.
Smart HTTP via `git http-backend`, bound to a port reachable only by
quire-ci. Auth: a shared token in an Authorization header, or network
isolation (docker network, loopback), or both.

Clone URL: configured on quire-ci's side, not in the webhook payload.
One quire-ci binds to one quire-server.

Future optimization (when measured): cache a bare clone at
`/var/quire-ci/repos/<name>.git`, update via `git fetch` on each
webhook, reify per-run workspaces via `git worktree add` or
`jj workspace add`. Avoids the full re-clone per push without coupling
through a shared mount.

## Schema (new quire-ci/migrations/0001)

Lifecycle is derived from column population, not stored. No `state`
column. A row's stage is read off the timestamps and outcome:

| `dispatched_at` | `outcome` | Stage |
|---|---|---|
| NULL | NULL | queued |
| set  | NULL | active |
| set  | set  | resolved (specific kind in `outcome`) |

```sql
CREATE TABLE runs (
  id             TEXT    PRIMARY KEY,
  repo           TEXT    NOT NULL,
  ref_name       TEXT    NOT NULL,
  sha            TEXT    NOT NULL,
  created_at     INTEGER NOT NULL,
  dispatched_at  INTEGER,
  resolved_at    INTEGER,
  outcome        TEXT,
  traceparent    TEXT,

  -- timestamps move forward
  CHECK (dispatched_at IS NULL OR dispatched_at >= created_at),
  CHECK (resolved_at   IS NULL OR resolved_at   >= created_at),
  CHECK (resolved_at   IS NULL OR dispatched_at IS NULL
         OR resolved_at >= dispatched_at),

  -- resolved_at and outcome travel together
  CHECK ((resolved_at IS NULL) = (outcome IS NULL)),

  -- outcome enum
  CHECK (outcome IS NULL OR outcome IN (
    'succeeded',
    'failed-pipeline', 'failed-orphaned', 'failed-internal',
    'superseded'
  ))
);

-- Pending work: queue scans only touch unresolved rows.
CREATE INDEX runs_pending ON runs(created_at) WHERE outcome IS NULL;

-- Listing runs per repo, most recent first.
CREATE INDEX runs_repo_created_at ON runs(repo, created_at DESC);
```

A Rust-side `enum RunStage { Queued, Active, Resolved(Outcome) }` is
computed from the columns rather than stored. The mapping is the table
above.

`jobs` and `sh` follow the same convention (drop state column, encode
lifecycle through timestamp population, add an `outcome` enum where
relevant). Spelling out their CHECK constraints can wait for the
migration itself.

Outcome values:

| Value | Meaning |
|---|---|
| `succeeded` | Pipeline ran, all jobs passed. |
| `failed-pipeline` | Pipeline ran, a job or `(sh ...)` reported failure. |
| `failed-orphaned` | Runner restart found an unresolved row with no live runner — `reconcile_orphans` marked it. |
| `failed-internal` | Runner task panicked or hit an unexpected error before the pipeline could report. |
| `superseded` | A later push for the same `(repo, ref)` displaced this one. |

Dropped from the current schema:

- `runs.state` (derived from timestamps + `outcome` now).
- `runs.failure_kind` (folded into `outcome` as `failed-*` variants).
- `runs.run_token` (no API callback to authenticate).
- `runs.git_dir` (derivable from `repo` plus a known base, and the base
  is per-process anyway).
- `runs.pushed_at_ms` (the receive time on quire-ci's clock is the
  honest field; renamed to `created_at`).
- `runs.started_at_ms` / `finished_at_ms` (renamed to `dispatched_at` /
  `resolved_at` — neutral terms that don't overclaim like "finished"
  does for a superseded run).

Kept: `runs.traceparent`, populated from the webhook header rather than
env vars.

## Vocabulary

Leave `runs`, `jobs`, `sh` as they are. `sh` is honest about being the
host-effect chokepoint and avoids overclaiming "step" when a job's
Fennel logic between `(sh ...)` calls is not recorded as a row.
Generalize the name when a second host primitive lands.

## Webhook contract

`POST {ci_webhook_url}` with body:

```json
{
  "repo": "foo",
  "refs": [
    { "ref_name": "refs/heads/main",
      "old_sha":  "...",
      "new_sha":  "..." }
  ]
}
```

Headers:

- `Authorization: HMAC-SHA256 <hex>` — signature over the raw body
  using `ci_webhook_secret`.
- `traceparent: <w3c>` — propagated from the post-receive span.

One webhook per push. A push can touch multiple refs; the receiver
inserts one `runs` row per ref.

## Binary

One `quire-ci` binary with three subcommands:

- `serve` — the orchestrator (webhook + DB + runner + web UI).
- `validate <path>` — compile-only check of a `ci.fnl`.
- `run --local <path>` — execute a `ci.fnl` against a local checkout
  for debugging, no orchestrator state involved.

The server-dispatched, subprocess form of `quire-ci run` goes away.

## Dockerfile

One Dockerfile with two final-stage targets, sharing the cargo-chef
cook stage:

```dockerfile
FROM debian:trixie-slim AS git-builder
# builds git 2.54 from source; only the server target consumes it
...

FROM rust:1.95-trixie AS chef
RUN cargo install --locked cargo-chef
WORKDIR /build

FROM chef AS planner
COPY . .
RUN cargo chef prepare --recipe-path recipe.json

FROM chef AS builder
COPY --from=planner /build/recipe.json recipe.json
RUN --mount=type=cache,target=/usr/local/cargo/registry \
    --mount=type=cache,target=/build/target \
    cargo chef cook --release --recipe-path recipe.json
COPY . .
RUN --mount=type=cache,target=/usr/local/cargo/registry \
    --mount=type=cache,target=/build/target \
    cargo build --release --bin quire --bin quire-ci && \
    mkdir -p /build/bin && \
    cp target/release/quire    /build/bin/quire && \
    cp target/release/quire-ci /build/bin/quire-ci

FROM debian:trixie-slim AS server
RUN apt-get update && apt-get install -y --no-install-recommends \
        ca-certificates libcurl4 libexpat1 \
    && rm -rf /var/lib/apt/lists/*
COPY --from=git-builder /usr/local/bin/git /usr/local/bin/git
COPY --from=git-builder /usr/local/libexec/git-core/ /usr/local/libexec/git-core/
COPY --from=builder    /build/bin/quire /usr/local/bin/quire
RUN git config --system hook.quire.event   "post-receive" \
 && git config --system hook.quire.command "quire hook post-receive"
RUN mkdir -p /var/quire/repos
WORKDIR /var/quire
EXPOSE 3000
ENTRYPOINT ["quire"]
CMD ["serve"]

FROM debian:trixie-slim AS ci
RUN apt-get update && apt-get install -y --no-install-recommends \
        ca-certificates git \
    && rm -rf /var/lib/apt/lists/*
COPY --from=builder /build/bin/quire-ci /usr/local/bin/quire-ci
RUN mkdir -p /var/quire-ci
WORKDIR /var/quire-ci
EXPOSE 3001
ENTRYPOINT ["quire-ci"]
CMD ["serve"]
```

Build:

```
docker build --target server -t quire-server:$VER .
docker build --target ci     -t quire-ci:$VER     .
```

Notes:

- The `builder` stage builds both binaries on every target. The cook
  stage dominates; per-bin compilation is cheap. Split into per-bin
  builder stages only if it shows up in CI time.
- No `docker:cli` copy. The subprocess-first phase does not need it.
  Add one apt line to the ci target when the docker-exec phase lands.
- `Dockerfile.gitweb` is unrelated and stays as-is.

## Code moves

### Phase 1 — grow quire-ci in parallel

quire-server is **not touched** in this phase. quire-ci grows from a
stub into a working orchestrator alongside the existing in-process
runner.

- `quire-ci/migrations/0001_initial.sql` — the schema above.
- `quire-ci/src/orchestrator/` — webhook handler, `Runs`/`Run`/`Executor`,
  runner task. Crib from `quire-server/src/ci/`, then trim.
- `quire-ci/src/web/` — run/log views. Port templates and Askama setup
  from quire-server.
- `quire-ci/src/db.rs` — DB plumbing, cribbed from quire-server.
- `quire-ci/src/server.rs` (existing stub) — fleshed out into the real
  axum router: `/webhook`, `/runs/*`, `/runs/:id/jobs/:id/logs/stream`,
  static assets.
- `quire-ci/src/quire.rs` — `QuireCi` grows to own the DB pool,
  `SecretRegistry`, and runner handle.
- `quire-core::PushEvent` / `PushRef` — lifted out of
  `quire-server/src/event.rs`, since both processes need them.
- `quire-server`: add a new `webhook_client` module (signs and POSTs the
  event) with tests, but **do not wire it into the hook yet**.
- `quire-server`: add the internal git-http endpoint. quire-ci needs
  something to clone from during parallel development.

At the end of phase 1, both processes work. quire-server still handles
pushes via the in-process runner. quire-ci runs standalone, exercised
via direct webhook calls.

### Phase 2 — cutover

A single small change:

- Switch `quire hook post-receive` from "notify in-process listener" to
  "POST to `ci_webhook_url`".
- Add `ci_webhook_url` and `ci_webhook_secret` to quire-server's config.
- Deploy quire-ci into production alongside quire-server.

### Phase 3 — server cleanup

A separate commit, once production has been on quire-ci for long enough
to trust it:

- Delete `quire-server/src/ci/`, `src/quire/web/api.rs`,
  `src/quire/web/db.rs`, and the run/log handlers and templates.
- Delete all of `quire-server/migrations/` and the `rusqlite*` deps.
- Drop `quire-core::api::SecretResponse`, `ci::bootstrap`,
  `ci::run::{ApiSession, RunMeta}`. They have no consumers after this.

quire-server ends the phase stateless with respect to CI.

## Open questions

- HMAC scheme: SHA-256 is the obvious default; revisit if anything
  pushes against it.
- Whether to fold the existing `/api` mount path into quire-ci's web
  router under the same prefix, or pick a fresh layout. Worth a look
  when porting the templates.
- Health-check shape for quire-ci. The current stub has `/health`; keep
  that and add `/ready` if any deployment infra needs the distinction.

## Sequencing

1. Phase 1 lands as one or more PRs. Tests at the webhook boundary
   (post a synthesized push to quire-ci, assert a `runs` row appears).
2. Phase 2 is one small PR plus a deploy.
3. Phase 3 is the cleanup PR.
