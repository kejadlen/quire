# quire-ci events endpoint

Supersedes parts of `2026-05-14-quire-ci-server-api-design.md`. The
original design called for four resource-style state-transition
endpoints (`jobs/:id/start|finish`, `sh/start|finish`) plus a
dedicated `/complete`. This document collapses all five into one
generic events endpoint and explains why.

## What changes

| Original | Replacement |
|---|---|
| `POST /api/runs/:id/jobs/:job_id/start` | `POST /api/runs/:id/events` with `{type: "job_started", ...}` |
| `POST /api/runs/:id/jobs/:job_id/finish` | same endpoint, `{type: "job_finished", ...}` |
| `POST /api/runs/:id/jobs/:job_id/sh/start` | same endpoint, `{type: "sh_started", ...}` |
| `POST /api/runs/:id/jobs/:job_id/sh/finish` | same endpoint, `{type: "sh_finished", ...}` |
| `POST /api/runs/:id/complete` | same endpoint, `{type: "run_finished", ...}` |

Bootstrap (`GET /api/runs/:id/bootstrap`) and log streaming
(`POST /api/runs/:id/jobs/:job_id/sh/logs`) are unchanged.

## Scope

In scope:

- New `POST /api/runs/:id/events` endpoint and handler.
- CI client switches from per-state-transition calls to a single
  events POST, parameterized on the `Event` enum.
- Slice 5's `/complete` folds in as `RunFinished`.

Out of scope:

- Bootstrap and log streaming stay as designed.
- Watchdog behavior unchanged: per-run last-contact timestamp,
  reset on every authenticated request.
- No schema changes — events still project onto `runs`, `jobs`,
  `sh` with the same CHECK constraints.

## Why pivot

Original decision #4 picked resource-style over a generic event
stream on these grounds:

> The event set is small and closed (job start/finish, sh
> start/finish). State-transition endpoints replace the JSONL
> append-log abstraction — each endpoint has one job, idempotency
> falls out naturally, and the server validates ordering against
> the run's current state.

That reasoning held up to scrutiny, but understated the cost: five
routes, five handlers, five sets of tests, five client methods,
and a Rust enum (`Event`) that already encodes exactly the same
information one layer down.

The collapsing argument:

1. **`Event` already exists** in `quire-core::ci::event`. It is
   `#[serde(tag = "type")]` flat, so the wire format is identical
   whether one endpoint or five handle it. The sink trait
   (`EventSink`) is the perfect client abstraction — `HttpSink`
   replaces `JsonlSink` and the rest of CI doesn't notice.

2. **Ordering enforcement doesn't depend on URL shape.** A
   dispatching handler runs the same validation a resource handler
   would, against the same DB state, returning the same 409s.
   Idempotency-via-state-check works identically.

3. **Per-event endpoints multiply with the event set.** Adding a
   `job_skipped` outcome (already on the backlog) means a new
   route and handler in the resource-style world. In the
   events-endpoint world it's one new enum variant and a match arm.

4. **The JSONL path and the API path want the same wire format.**
   Sharing one `Event` enum across both transports means we can't
   diverge by accident during the transition. Resource-style would
   have required a translation layer between JSONL events and
   per-resource request bodies.

Decision #4 is reversed. The rest of the original design's
decisions — bearer-token auth, plain HTTP/JSON, subprocess-local
today, chunked POST for logs — stand unchanged.

## API surface

One endpoint replaces five:

| Method | Path | Body | Purpose |
|---|---|---|---|
| POST | `/api/runs/:id/events` | one `Event` JSON object | Record a state transition. |

Request body is exactly the wire format `quire-core::ci::event::Event`
already emits to JSONL — `at_ms` envelope plus a flattened
`#[serde(tag = "type")]` payload. No batching: one event per
request, mirroring the per-event flush behavior of `JsonlSink`.

Success: 204 No Content.

Errors:

- 401 — missing or invalid bearer token.
- 403 — token does not scope to `:id`.
- 404 — unknown run.
- 409 — event violates run's current state (see ordering).
- 422 — `:job_id` in payload not declared by the run, or event
  payload malformed.
- 5xx — server fault; CI retries with backoff.

## Ordering

Server validates each event before applying it. Rules carry over
from the original resource-style design, restated against the
event variants:

- `job_started`: requires job in declared-but-not-started state.
- `sh_started`: requires the named job started and no sh currently
  open on it.
- `sh_finished`: requires an sh currently open on the named job.
- `job_finished`: requires the named job started with no sh
  currently open.
- `run_finished`: requires every started job already finished.

Violations return 409. CI treats 409 as "already recorded, move
on" — the same idempotency-via-state-check pattern the original
design relied on. Server-side, validation runs before any DB
write; either the projection update and the state check happen in
one transaction, or neither does.

The handler is a single match on `event.kind`. Each arm calls into
a typed validation+projection helper. Adding a variant (e.g.
`job_skipped`) is one match arm and one helper, no new route.

## Client side

`quire-ci` already routes events through the `EventSink` trait
(`quire-ci/src/sink.rs`). The trait stays; a new `HttpSink`
implementation joins `JsonlSink` and `NullSink`:

```rust
pub struct HttpSink {
    client: reqwest::blocking::Client,
    base_url: Url,           // server-url + /api/runs/<run-id>
    token: SecretString,     // QUIRE_CI_TOKEN
}

impl EventSink for HttpSink {
    fn emit(&mut self, event: Event) -> io::Result<()> { ... }
}
```

The sink-selection logic in `quire-ci/src/main.rs` gains a third
arm gated on the existing `--events` / `EventsTarget` mechanism,
so the transport flag plumbed in slice 1 keeps doing its job. No
other code in CI changes — emitters call `sink.emit(event)`
exactly as today.

## Retry policy

Carries over from the original design with one simplification:

- **All events**: retry on 5xx with exponential backoff, up to ~5
  attempts. On final failure, log to stderr and continue. Losing
  a state-transition POST is bad but should not kill the run.
- **`run_finished` specifically**: more attempts than the others,
  because it's the authoritative completion signal. Effectively
  what the original `/complete` retry policy was; now it lives in
  the sink as a per-variant case rather than a separate client
  call site.
- **409**: not retried. Treated as success ("already recorded").
- **4xx other than 409**: not retried. Logged and surfaced as a
  run-level problem.

Server-side, the watchdog keeps the original behavior: per-run
last-contact timestamp updated on every authenticated request
(including ones that 409). A run with no `run_finished` event and
no contact for the timeout window is marked failed by the watchdog.

## Slices

The original plan's slices 3 and 5 collapse into one:

3. **Events endpoint.** `POST /api/runs/:id/events` handler with
   dispatching match on `Event.kind`. `HttpSink` in quire-ci.
   Gated on the existing `transport` flag. `RunFinished` rides the
   same endpoint; no separate `/complete`.

The original plan's slices 1 (auth + transport foundation), 2
(bootstrap), and 4 (log streaming) are unchanged. Slice 6 (retire
filesystem path) is unchanged in shape but now also deletes the
JSONL sink rather than the per-resource client methods.

## Rollout

Same `transport` flag in `conf/config.fnl` from the original
design. While `transport = :filesystem`, CI uses `JsonlSink` and
the server ingests events.jsonl post-mortem. While `transport =
:api`, CI uses `HttpSink` and the server's events handler updates
the projection live. Flag flips per environment as confidence
grows.

After every slice ships and the API path proves out, the
follow-up cleanup commit deletes:

- The `transport` flag and its read sites.
- `JsonlSink` and the events.jsonl write path in quire-ci.
- The post-mortem JSONL ingestion in quire-server.
- The per-sh-log file write/read path (replaced by chunked log
  streaming in slice 4).

## Open questions

None blocking implementation. Worth revisiting later:

- Whether `run_finished` should carry the per-job outcome tallies
  the watchdog currently derives from job rows. Probably not —
  the projection already knows.
- Whether to broadcast applied events on a server-side channel
  (e.g. tokio broadcast) so the live UI can subscribe without
  polling. Out of scope for this slice but the events endpoint is
  the natural producer when that lands.
