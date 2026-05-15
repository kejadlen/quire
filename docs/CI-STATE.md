# quire — CI state machines

A reader's guide to the two state machines that govern a CI run, paired with what the code actually writes today vs. what the schema and `CI.md` describe. `CI.md` is the architectural design; this doc is the lifecycle inside that design.

There are two machines:

1. **Run state** — the row in `runs`. One per `(repo, ref, push)`.
2. **Job state** — the row in `jobs`. One per job inside a run.

A run owns its jobs; jobs FK on `(run_id, job_id)` and cascade delete.

## Run state machine

### Diagram

```mermaid
stateDiagram-v2
    [*] --> pending : Runs::create

    pending  --> active     : transition(Active, None)
    pending  --> superseded : Runs::create on same (repo, ref)\nsupersede_existing (raw SQL)
    pending  --> failed     : reconcile_orphans\nfailure_kind='orphaned'

    active   --> complete   : transition(Complete, None)
    active   --> failed     : transition(Failed, 'quire-ci-exit')
    active   --> superseded : Runs::create on same (repo, ref)\ndocker kill + supersede_existing
    active   --> failed     : reconcile_orphans\nfailure_kind='orphaned'

    complete   --> [*]
    failed     --> [*]
    superseded --> [*]

    note right of pending
      started_at_ms  IS NULL
      finished_at_ms IS NULL
      container_id   IS NULL
    end note

    note right of active
      started_at_ms stamped on entry
      finished_at_ms still NULL
    end note

    note right of complete
      started_at_ms, finished_at_ms set
      container_id cleared
    end note
```

### Transitions in code

| From → To | Where | When | `failure_kind` |
| --- | --- | --- | --- |
| `[*] → pending` | `Runs::create` (`quire-server/src/ci/run.rs:99`) | A push event arrives and a `runs` row is inserted. | — |
| `pending → active` | `Run::transition`, called from `Run::execute_via_quire_ci` | The executor begins evaluating the pipeline. Stamps `started_at_ms`. | — |
| `active → complete` | `Run::transition` | `quire-ci` subprocess exited 0. Stamps `finished_at_ms`, clears `container_id`. | — |
| `active → failed` | `Run::execute_via_quire_ci` | `quire-ci` subprocess exited non-zero (compile error in `.quire/ci.fnl`, failing job, or panic). | `"quire-ci-exit"` |
| `{pending, active} → superseded` | `Runs::supersede_existing` (`run.rs:144`) via raw SQL, **bypassing `transition`** | A new `Runs::create` for the same `(repo, ref)` arrived. Pending rows are flipped directly; active rows have their container killed (`docker kill`) first. | — |
| `{pending, active} → failed` | `reconcile_orphans` (`run.rs:194`) via raw SQL, **bypassing `transition`** | Startup-time cleanup of rows left behind by a previous `quire serve` instance. | `"orphaned"` |

`Run::transition(to, failure_kind)`'s allowed-transition match:

```
(Pending, Active) | (Pending, Complete) | (Pending, Superseded) |
(Active,  Complete) | (Active,  Failed) | (Active,  Superseded)
```

In practice only `(Pending, Active)`, `(Active, Complete)`, and `(Active, Failed)` are exercised — the supersede edges go through raw SQL (`supersede_existing`), not `transition`. The other edges are gated for defensive consistency, in case a future caller routes supersede through the typed API. Anything else — `Pending → Failed`, `Active → Pending`, or any transition out of a terminal state — returns `InvalidTransition`.

`failure_kind` is recorded only when `to == Failed`; it's ignored for `Active`, `Complete`, and `Superseded`.

### Database invariants

The DB enforces shape per state via a `CHECK` constraint in `migrations/0001_initial.sql`:

| State | started_at | finished_at | container_id |
| --- | --- | --- | --- |
| `pending` | NULL | NULL | NULL |
| `active` | set | NULL | (any) |
| `complete` | set | set | NULL |
| `failed` | (any) | set | NULL |
| `superseded` | (any) | set | NULL |

Plus monotonicity: `started_at >= queued_at`, `finished_at >= started_at`. `started_at_ms`, `finished_at_ms`, and `failure_kind` are stamped at most once each, via `COALESCE` in the `UPDATE`.

### `failure_kind`

Nullable column populated by `Run::transition` when entering `Failed`, plus `reconcile_orphans` (raw SQL). Each transition sets it at most once via `COALESCE`. The values written today:

| Value | Producer |
| --- | --- |
| `"quire-ci-exit"` | `Run::execute_via_quire_ci`: subprocess exited non-zero. Covers both compile errors in `ci.fnl` (caught inside `quire-ci`) and failing user jobs. |
| `"orphaned"` | `reconcile_orphans` on startup. |

Successful and superseded runs leave `failure_kind` NULL. The set is open — UI consumers should not assume it's exhaustive.

## Job state machine

### Diagram

```mermaid
stateDiagram-v2
    [*] --> complete : ingest JobFinished(outcome='complete')
    [*] --> failed   : ingest JobFinished(outcome='failed')

    complete --> [*]
    failed   --> [*]

    pending --> [*] : no producer yet — see Gaps
    active  --> [*] : no producer yet — see Gaps
    skipped --> [*] : no producer yet — see Gaps
    aborted --> [*] : no producer yet — see Gaps
```

### Transitions in code

There is only one writer of `jobs` rows: `Run::ingest_events` (`run.rs:354`). It reads `events.jsonl` after the `quire-ci` subprocess exits and, for each `JobStarted`/`JobFinished` pair, inserts **one row directly in the terminal state**. The intermediate `active` state is held in an in-memory `pending_jobs` map during ingest and never persisted.

| From → To | Where | When |
| --- | --- | --- |
| `[*] → complete` | `Run::ingest_events` (`run.rs:354`) | `JobFinished { outcome: complete }` paired with a buffered `JobStarted`. |
| `[*] → failed` | `Run::ingest_events` | `JobFinished { outcome: failed }` paired with a buffered `JobStarted`. |

Consequence: while `quire-ci` is running, **no `jobs` rows exist for this run**. They all materialize at ingest time. Live progress is visible via `events.jsonl` or per-`sh` log files on disk, not via SQL.

### Database invariants

`migrations/0001_initial.sql` allows six job states (`pending`, `active`, `complete`, `failed`, `skipped`, `aborted`) with these shape rules:

| State | started_at | finished_at |
| --- | --- | --- |
| `pending` | NULL | NULL |
| `active` | set | NULL |
| `complete` | set | set |
| `failed` | set | set |
| `skipped` | NULL | set |
| `aborted` | (any) | set |

`skipped` carries `finished_at` but not `started_at` — the row exists to record "this job never ran" with a timestamp anchoring it to the run.

### Stop-on-first-failure inside `quire-ci`

The subprocess's executor (`quire-ci/src/main.rs`) breaks out of the topo-order loop on the first job error:

```rust
if let Err(e) = result {
    failed_job = Some((job_id.clone(), e));
    break;
}
```

`JobStarted`/`JobFinished` are only emitted for jobs that actually ran. **Jobs downstream of the failure produce no events, so no `jobs` row at all** — not `skipped`, not anything. See Gaps below.

## Event flow: QuireCi executor

`Executor::QuireCi` is the only executor today. The orchestrator shells out to the `quire-ci` binary and ingests events afterward, rather than driving the runtime in-process:

```mermaid
sequenceDiagram
    participant Trigger as ci::trigger_ref
    participant Run as Run (server)
    participant CI as quire-ci subprocess
    participant DB as SQLite

    Trigger->>Run: execute_via_quire_ci()
    Run->>DB: UPDATE runs SET state='active'
    Run->>CI: spawn (--bootstrap, --events, --out-dir)
    CI->>CI: compile .quire/ci.fnl
    loop per job in topo order
      CI->>CI: enter_job / run-fn / leave_job
      CI->>CI: append JobStarted/ShStarted/ShFinished/JobFinished\nto events.jsonl
    end
    CI-->>Run: exit status
    Run->>Run: ingest_events(events.jsonl)
    Run->>DB: INSERT jobs (pass 1)
    Run->>DB: INSERT sh_events (pass 2)
    alt exit 0
        Run->>DB: UPDATE runs SET state='complete'
    else exit nonzero
        Run->>DB: UPDATE runs SET state='failed' (failure_kind='quire-ci-exit')
    end
```

Wire events (`quire-core/src/ci/event.rs`):

* `JobStarted { job_id }`
* `JobFinished { job_id, outcome: complete | failed }` — `JobOutcome` is the closed set, not the full job-state enum.
* `ShStarted { job_id, cmd }` / `ShFinished { job_id, exit_code }`

`Run::ingest_events` reads the file in two passes (jobs first to satisfy the FK on `(run_id, job_id)`, then sh_events). Ingest failures are logged but never demote the run's own outcome — a partial DB write is preferable to losing the pass/fail signal.

## Orchestration today

The lifecycle from push to run start:

```mermaid
sequenceDiagram
    participant Hook as post-receive
    participant Listener as event_listener (tokio)
    participant Trigger as ci::trigger
    participant Exec as Run::execute_via_quire_ci
    participant FS as filesystem
    participant DB as SQLite

    Hook->>Listener: PushEvent JSON over /var/quire/server.sock
    Listener->>Trigger: trigger(quire, &event)
    loop per updated ref
      Trigger->>DB: supersede_existing (Pending|Active → Superseded for same repo/ref)
      Trigger->>DB: INSERT runs (state=pending)
      Trigger->>FS: create run dir + workspace
      Trigger->>FS: git archive | tar -x  (materialize workspace)
      Trigger->>Exec: execute_via_quire_ci
      Exec->>DB: pending → active → complete|failed
    end
```

Two things in `CI.md` that the code does *not* yet implement at this layer:

* **Queue + Notify wakeup.** `CI.md` describes a separate runner task pulled from a SQLite queue via `tokio::sync::Notify`. Today `ci::trigger` is called **synchronously** on the listener's tokio task — one push at a time, no queue, no separate runner. Max-concurrency-1 falls out of this trivially, but it isn't the architecture in `CI.md`.
* **Per-run container.** `CI.md` says `docker run` at run start, `docker exec` per `(sh …)`, `docker stop` at end. `quire-ci` invokes `(sh …)` directly on the host process; the `container_id` / `container_started_at_ms` columns are populated only by the dev fixture and read by `supersede_existing` (which already calls `docker kill` against whatever's there).

## Gaps

States the schema admits — or `CI.md` commits to — that no code path produces today:

| Gap | Schema/spec | Producer needed |
| --- | --- | --- |
| Job `active` rows during execution | Schema-allowed | `ingest_events` inserts one row per job at JobFinished time. While `quire-ci` is running, the `jobs` table has nothing for this run. Live UI of "currently running job" needs an active-row writer — either eager ingest, or a separate writer inside `quire-ci`. |
| Job `pending` rows | Schema-allowed | Useful for "queued jobs in topo order" UI. Today jobs go straight from no-row to a terminal row. |
| Job `skipped` rows for dependents of a failed job | Schema-allowed | `quire-ci`'s loop `break`s on first failure and emits no events for downstream jobs. To populate `skipped`, either `quire-ci` would emit `JobSkipped` events for unrun topo-order jobs, or the ingester would compute them from the pipeline graph + the surviving JobFinished rows. |
| Job `aborted` rows | Schema-allowed | Needed when a run is killed mid-flight — e.g. by `supersede_existing` `docker kill`-ing the container. Today the run row flips to `superseded`, but no `jobs` rows are written for the work that was in flight. |
| `:allow-failure` job flag | Documented in `CI.md` as v1 | Not implemented anywhere in `quire-core`, `quire-ci`, or `quire-server`. The structural validator doesn't recognize the key; the executor treats every job error as fatal. |
| Queue + Notify wakeup | `CI.md` "Communication" section | `trigger` runs synchronously on the listener task. No queue scan, no Notify, no separate runner task. |

## Cross-references

* Architecture and rationale: [`CI.md`](./CI.md).
* Pipeline DSL: [`CI-FENNEL.md`](./CI-FENNEL.md).
* DB shape: [`quire-server/migrations/0001_initial.sql`](../quire-server/migrations/0001_initial.sql).
* Code: `quire-server/src/ci/run.rs`, `quire-server/src/ci/mod.rs`, `quire-core/src/ci/event.rs`, `quire-ci/src/main.rs`.
