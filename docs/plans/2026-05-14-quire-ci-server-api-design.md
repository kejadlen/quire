# quire-ci ↔ quire-server API

Captures the design for replacing filesystem-based communication between `quire-ci` and `quire-server` with an HTTP API. The endgame is containerized runs with live log streaming; this design lays the groundwork.

## Scope

In scope:

- HTTP/JSON API for all CI→server data flow currently routed through the filesystem.
- Per-run bearer-token auth, scoped so a run's token can only write to that run.
- Bootstrap via CLI args + env var; no shared filesystem paths for comms.
- Chunked POST for log streaming, sized for the eventual container case.
- A server-side config flag that keeps the filesystem path working during build-out.

Out of scope (deliberately deferred to v2):

- Log stream resume after dropped connections (`Resume-From` semantics).
- Server→CI messages such as cancel or pause.
- WebSocket transport.
- Versioned API prefix (`/api/v1/...`).
- Workspace materialization over HTTP — the git archive still lands on disk for now.
- Per-repo (ci.fnl) opt-in for the transport flag.

## Background

`quire-server` and `quire-ci` currently communicate entirely through the filesystem (see `docs/CI.md` and the implementation in `quire-server/src/ci/run.rs` and `quire-ci/src/main.rs`). Server spawns CI as a subprocess; both halves share a run directory. The current touchpoints:

| Artifact | Path | Writer | Reader | Purpose |
|---|---|---|---|---|
| Bootstrap | `<run-dir>/bootstrap.json` | server | CI | Secrets, git_dir, push meta, Sentry handoff |
| Events | `<run-dir>/events.jsonl` | CI | server | Job/sh state transitions |
| Subprocess log | `<run-dir>/quire-ci.log` | CI (stdio) | server UI | Debug subprocess output |
| Per-sh log | `<run-dir>/jobs/<job>/sh-<n>.log` | CI | server UI | Rendered on job detail page |

This breaks down once CI runs inside a container: the container's filesystem is opaque to the host, and logs need to surface live, not after process exit.

## Design decisions

The full alternatives were explored in conversation; the load-bearing decisions:

1. **Subprocess-local today, remote-capable tomorrow.** Server still spawns CI as a subprocess. The API is designed so colocation is incidental — no assumptions about a shared filesystem or `localhost`.

2. **Plain HTTP/JSON.** Axum on the server, reqwest on the client. Same router and auth middleware as the existing web UI. WebSockets are deferred until a concrete need (multiplexing many parallel jobs, server→CI cancel) materializes.

3. **Per-run bearer token.** Server mints a short-lived token when the run is created; CI sends `Authorization: Bearer <token>` on every call. Middleware verifies the token matches the run-id in the URL. Token's lifetime equals the run's lifetime.

4. **Resource-style endpoints, not a generic event stream.** The event set is small and closed (job start/finish, sh start/finish). State-transition endpoints replace the JSONL append-log abstraction — each endpoint has one job, idempotency falls out naturally, and the server validates ordering against the run's current state.

5. **API-authoritative completion.** `POST /complete` is the source of truth for run termination. Subprocess exit becomes a backstop signal only. A server-side watchdog catches crashed CIs that never post completion.

6. **Chunked POST per sh for logs.** One streaming request per `(sh ...)` invocation. Request close = stream complete. Plays through reverse proxies and shares the auth/middleware stack.

## API surface

Bootstrap (server passes to CI at spawn time):

- CLI args: `--run-id <uuid>`, `--server-url <url>`
- Env var: `QUIRE_CI_TOKEN=<bearer>` (kept out of `ps` output)

Endpoints (CI calls server):

| Method | Path | Body | Purpose |
|---|---|---|---|
| GET    | `/api/runs/:id/bootstrap` | — | Fetch bootstrap payload. One-shot: server invalidates after first successful read. |
| POST   | `/api/runs/:id/jobs/:job_id/start` | `{}` | Job entered execution. Server timestamps. |
| POST   | `/api/runs/:id/jobs/:job_id/finish` | `{ outcome: "complete" \| "failed" }` | Job done. |
| POST   | `/api/runs/:id/jobs/:job_id/sh/start` | `{ cmd: string }` | Start the next sh in this job. Server assigns the sh index. |
| POST   | `/api/runs/:id/jobs/:job_id/sh/logs` | chunked CRI lines | Stream stdout/stderr for the currently-active sh. |
| POST   | `/api/runs/:id/jobs/:job_id/sh/finish` | `{ exit_code: i32 }` | Close the currently-active sh. |
| POST   | `/api/runs/:id/complete` | `{ outcome, exit_code }` | Run terminal. Authoritative completion signal. |

`:job_id` is the job name from `.quire/ci.fnl`. `:sh_n` does not appear in the URL because sh's are sequential within a job — the server tracks the current sh index per active job and surfaces it in read endpoints (out of scope here).

Ordering rules enforced server-side, returning 409 on violation:

- `sh/start` requires the job started and no sh currently open.
- `sh/logs` and `sh/finish` require an sh currently open.
- `jobs/:id/finish` requires no sh currently open.
- `complete` requires every started job to have finished.

Other error codes: 401 for missing/invalid token, 403 for token/run mismatch, 404 for unknown run, 410 if `bootstrap` was already fetched, 422 if a path segment doesn't match a known job, 5xx for server errors (CI retries with backoff).

## Lifecycle

1. **Push arrives** at quire-server. Server creates a run row (UUIDv7), mints a bearer token, materializes the workspace to disk, and sets the run to `pending`.

2. **Server spawns quire-ci:**

   ```
   QUIRE_CI_TOKEN=<token> quire-ci run \
     --run-id <uuid> \
     --server-url <url> \
     --workspace <path>
   ```

   No `--bootstrap`, no `--events`, no log path. The workspace path remains because that's the checkout CI runs *in*, not a comms channel.

3. **quire-ci bootstraps.** First call is `GET /api/runs/:id/bootstrap`. Server flips the run to `active`, returns the bootstrap payload (secrets, git_dir, push meta, Sentry handoff), and invalidates the bootstrap resource. Watchdog timer starts.

4. **quire-ci executes the DAG.** For each job:
   - `POST /jobs/:job_id/start`
   - For each `(sh ...)`:
     - `POST /jobs/:job_id/sh/start` with the command string
     - Open chunked `POST /jobs/:job_id/sh/logs`, stream CRI-formatted lines as the shell runs
     - On sh exit, close the logs request, then `POST /jobs/:job_id/sh/finish` with the exit code
   - `POST /jobs/:job_id/finish` with the outcome

5. **quire-ci finishes.** `POST /complete` with overall outcome and exit code. Server marks run terminal, stops the watchdog. Subprocess exits shortly after.

6. **Failure paths.**
   - CI crashes before `/complete` → watchdog times out → server marks run failed.
   - Network blip mid-log-stream → request closes early → v1 logs to stderr and continues the run; the sh's logs are truncated server-side. Retry/resume is v2.
   - Server returns 5xx → CI retries with exponential backoff for events and `/complete`; log streams do not retry in v1.

## Error handling

Server-side validation runs before any DB work:

- Token present, valid, and scoped to `:id`.
- Run exists and is in a state that accepts this request.
- Path segments (`:job_id`) correspond to declarations the run has already made.

quire-ci retry policy:

- **Events (start/finish endpoints):** retry on 5xx with exponential backoff, up to ~5 attempts. On final failure, log to stderr and continue. Losing a state-transition POST is bad but should not kill the run.
- **Bootstrap:** retry on 5xx, fail fast on 4xx. If bootstrap cannot be fetched, the run is unrunnable; CI exits nonzero and the watchdog catches it.
- **Complete:** the most important POST in the run. Retry with backoff and more attempts than the others.
- **Log streams:** v1 does not retry. A broken stream logs a stderr warning and execution continues.

Server-side robustness:

- All state-transition endpoints are idempotent by virtue of state checks: a retried `/start` after a successful first call gets 409, and CI treats that as "already recorded, move on."
- Watchdog: per-run last-contact timestamp, reset on every authenticated request. Configurable timeout (default tuned long enough for slow `npm install`-style steps, short enough that crashes don't strand runs).
- Sentry handoff is still passed via the bootstrap payload, so CI traces continue to link to server traces.

## Rollout

A server-side config flag keeps the filesystem path working while the API is built out:

```fennel
;; conf/config.fnl
{:ci {:transport :filesystem}  ; or :api
 ...}
```

Default is `:filesystem` during build-out. The server reads this at startup and, when spawning quire-ci, passes the choice through (CLI flag or env var — implementation detail). quire-ci implements both paths and dispatches at startup. Each slice (bootstrap, jobs/sh state, logs, complete) checks the flag and takes either path.

The flag lives in server config rather than per-repo `ci.fnl` because:

- `ci.fnl` lives inside the workspace, which is exactly the materialization step we eventually want to move off the filesystem. A flag inside the workspace forces the server to read it through the filesystem in every case.
- The transport choice is a property of how the server orchestrates CI, not of what a repo's CI does.
- Staged rollout (staging, then prod) maps naturally to a server config change; per-repo opt-in would require chasing every repo owner for a flag we plan to delete.

Single flag covers all four artifacts. Per-artifact flags would let us land slices independently without coordination, but the slices each gate on `transport == :api` at their own seam, so a single flag with code-level coexistence is enough.

After every slice ships and the API path proves out, delete the filesystem path and the flag in a single follow-up.

## Implementation slices

These will be filed as separate ranger tasks:

1. **Auth + transport foundation.** Bearer-token middleware on server. HTTP client in quire-ci. CLI args (`--run-id`, `--server-url`) and `QUIRE_CI_TOKEN` env var threaded through. Config flag wired up. No behavior change yet — both halves still take the filesystem path.
2. **Bootstrap over the API.** `GET /api/runs/:id/bootstrap` and corresponding client call, gated by the transport flag.
3. **Job and sh state transitions over the API.** The four `/start` and `/finish` endpoints, plus their clients.
4. **Log streaming over the API.** Chunked `POST /sh/logs` with CRI-line bodies.
5. **`/complete` and watchdog.** Authoritative completion, server-side timeout backstop.
6. **Retire the filesystem path.** Delete the flag, the JSONL ingestion, and the per-sh log file reads. Single cleanup commit after all slices ship.
