# CI run store: filesystem to SQLite

Captures the design for `nqrtxvvo` (switch CI db from filesystem to SQLite). Prerequisite for the read-only CI web view milestone (`yqnmmwpw`) — the run-list and run-detail pages want indexed queries that the directory-rename lifecycle makes awkward.

## Scope

In scope:

- A single SQLite database at `<quire-data-root>/quire.db` holding run and job state.
- `runs` and `jobs` tables with state-machine constraints enforced via `CHECK`.
- `rusqlite` plus `rusqlite_migration`; SQL files versioned under `migrations/` at the project root.
- SQLite is the queue: `quire serve` finds the next pending run with a `SELECT`, not by scanning directories.
- Removal of the filesystem state directories (`pending/`, `active/`, `complete/`, `failed/`) and the per-run JSON sidecars (`meta.json`, `state.json`, `times.json`, `container.json`).
- Update to `docs/CI.md` reflecting the new layout.

Out of scope:

- Migration of existing on-disk run state. The operator wipes the old layout manually before first SQLite startup.
- Live log tailing, streaming JSONL persistence, and broadcast channels (`xrupozur`).
- The web view itself — its own design once this lands.
- The supersede invariant (`plpwtqvv`) — schema leaves room but does not enforce.

## Database location

The db lives at `<quire-data-root>/quire.db`. The data root is the same path passed to `Quire::new` today. WAL mode is set on first connection. A new module `src/db.rs` owns the connection, pragmas, and the migration list; `Ci`/`Runs` borrow a connection handle from it rather than opening their own.

The db is project-scoped, not CI-scoped, even though the only tables today are CI tables. Future tables (config snapshots, hook event audit, etc.) live in the same file.

## Schema

```sql
CREATE TABLE runs (
  id              TEXT PRIMARY KEY,
  repo            TEXT NOT NULL,
  ref_name        TEXT NOT NULL,
  sha             TEXT NOT NULL,
  pushed_at_ms    INTEGER NOT NULL,
  state           TEXT NOT NULL,
  failure_kind    TEXT,
  queued_at_ms    INTEGER NOT NULL,
  started_at_ms   INTEGER,
  finished_at_ms  INTEGER,
  container_id    TEXT,
  workspace_path  TEXT NOT NULL,

  CHECK (state IN ('pending', 'active', 'complete', 'failed', 'superseded')),

  CHECK (started_at_ms  IS NULL OR started_at_ms  >= queued_at_ms),
  CHECK (finished_at_ms IS NULL OR finished_at_ms >= queued_at_ms),
  CHECK (finished_at_ms IS NULL OR started_at_ms IS NULL
         OR finished_at_ms >= started_at_ms),

  CHECK (CASE state
    WHEN 'pending'    THEN started_at_ms IS NULL  AND finished_at_ms IS NULL  AND container_id IS NULL
    WHEN 'active'     THEN started_at_ms IS NOT NULL AND finished_at_ms IS NULL
    WHEN 'complete'   THEN started_at_ms IS NOT NULL AND finished_at_ms IS NOT NULL AND container_id IS NULL
    WHEN 'failed'     THEN finished_at_ms IS NOT NULL AND container_id IS NULL
    WHEN 'superseded' THEN finished_at_ms IS NOT NULL AND container_id IS NULL
  END)
);

CREATE INDEX runs_repo_pushed_at ON runs(repo, pushed_at_ms DESC);
CREATE INDEX runs_state          ON runs(state);

CREATE TABLE jobs (
  run_id          TEXT NOT NULL REFERENCES runs(id) ON DELETE CASCADE,
  job_id          TEXT NOT NULL,
  state           TEXT NOT NULL,
  exit_code       INTEGER,
  started_at_ms   INTEGER,
  finished_at_ms  INTEGER,

  CHECK (state IN ('pending', 'active', 'complete', 'failed', 'skipped', 'aborted')),

  CHECK (started_at_ms IS NULL OR finished_at_ms IS NULL
         OR finished_at_ms >= started_at_ms),

  CHECK (CASE state
    WHEN 'pending'  THEN started_at_ms IS NULL  AND finished_at_ms IS NULL
    WHEN 'active'   THEN started_at_ms IS NOT NULL AND finished_at_ms IS NULL
    WHEN 'complete' THEN started_at_ms IS NOT NULL AND finished_at_ms IS NOT NULL
    WHEN 'failed'   THEN started_at_ms IS NOT NULL AND finished_at_ms IS NOT NULL
    WHEN 'skipped'  THEN started_at_ms IS NULL  AND finished_at_ms IS NOT NULL
    WHEN 'aborted'  THEN finished_at_ms IS NOT NULL
  END),

  PRIMARY KEY (run_id, job_id)
);
```

Notes on what the constraints buy:

- `failed` and `superseded` on `runs` allow a null `started_at_ms` because a run can be reconciled out of `pending` without ever starting (`umykvluw`), and a queued run can be superseded before it starts (`plpwtqvv` — schema-ready, not yet enforced).
- `failed` on `jobs` requires `started_at_ms` because a job that never ran transitions to `aborted` (the container died before scheduling reached it, per `zmtuqwly`) or `skipped` (explicit), not `failed`.
- `container_id` is null outside `active`. The runner clears it on transition out.
- Display order for jobs falls out of insert order via `ORDER BY rowid`. No explicit `ord` column.

What the schema deliberately does *not* enforce:

- Single-runner concurrency. That is runner policy; baking a partial unique index on `state = 'active'` would cement the assumption against any future parallel-runs work.
- The supersede invariant (one pending-or-active row per `(repo, ref_name)`). Lands when `plpwtqvv` does, as a partial unique index.

## Job graph and outputs

Out of v1.

The job DAG lives in `ci.fnl`. The web view in scope today (`yqnmmwpw`, scope B from brainstorming) shows a run-list page and a per-run page rendering job status and logs. It does not render the graph. When the web view ever needs to render edges, the source of truth is the compiled `Pipeline`, not a normalized SQL table — re-derive on read rather than store twice.

Job outputs (the table a Lua run-fn returns) are likewise not stored. If a future page wants to display them, add an `outputs JSON` column on `jobs` then.

## Migrations

Migrations live as SQL files under `migrations/` at the project root, included into the binary via `include_str!`:

```rust
use rusqlite_migration::{Migrations, M};

static MIGRATIONS: LazyLock<Migrations<'static>> = LazyLock::new(|| {
    Migrations::new(vec![
        M::up(include_str!("../migrations/0001_initial.sql")),
    ])
});
```

`db::open(path)` opens the connection, sets `journal_mode = WAL` and `foreign_keys = ON`, and runs `MIGRATIONS.to_latest(&mut conn)?` before returning. `rusqlite_migration` tracks `PRAGMA user_version` and applies missing migrations transactionally.

Each future schema change adds a new file (`0002_*.sql`, `0003_*.sql`, …) and a corresponding `M::up` entry. Files are append-only — never edit a migration that has already shipped.

## Queue semantics

The in-memory `mpsc` queue described in `docs/CI.md` is removed. SQLite is the queue.

Enqueue (today's `Runs::create` path):

```sql
INSERT INTO runs (id, repo, ref_name, sha, pushed_at_ms, state,
                  queued_at_ms, workspace_path)
VALUES (?, ?, ?, ?, ?, 'pending', ?, ?);
```

Dequeue (the listener's wakeup picks the next run):

```sql
UPDATE runs
SET state = 'active', started_at_ms = ?, container_id = ?
WHERE id = (SELECT id FROM runs
            WHERE state = 'pending'
            ORDER BY queued_at_ms
            LIMIT 1)
RETURNING *;
```

The wakeup signal stays an in-process `tokio::sync::Notify` (or equivalent) — used only to nudge the runner, not to carry data. Restart-safety falls out for free: on startup the runner picks up whatever is `pending` and reconciles whatever is `active` (orphan from a prior process).

## Orphan reconciliation

`Runs::reconcile_orphans` becomes a single SQL pass on startup:

```sql
UPDATE runs
SET state = 'failed',
    finished_at_ms = ?,
    container_id = NULL,
    failure_kind = 'orphaned'
WHERE state = 'active';
```

For `pending` runs that never started, `umykvluw` lands separately. The current code transitions them to `complete`; the right move once that ticket is in flight is to set them `failed` here too.

In docker mode, the `container_id` of the orphaned active run is the input to a best-effort `docker rm -f` before clearing the column. Already-dead containers are no-ops; that path lives in the runner, not the SQL.

## Filesystem layout after the change

```
<quire-data-root>/
  quire.db                       # the new SQLite db
  repos/<name>.git/              # bare repos, unchanged
  runs/<run-id>/                 # per-run workspace, unchanged
    workspace/                   # materialized checkout
    events.jsonl                 # structural events (xrupozur scope)
    jobs/<job-id>/
      sh-1.log                   # k8s CRI format (xrupozur scope)
      sh-2.log
      ...
```

The `pending/`, `active/`, `complete/`, `failed/` parent directories disappear. Per-run sidecars (`meta.json`, `state.json`, `times.json`, `container.json`) disappear — every field they carried lives in SQL. The run directory keeps the workspace and the log files only.

`workspace_path` on `runs` is the absolute path to that directory. The web view uses it to find logs.

## Documentation updates

`docs/CI.md` needs revisions:

- Drop "No SQLite in v1" from the storage section.
- Replace the directory-rename lifecycle description with the SQL-state-transition description.
- Remove the in-memory `mpsc` queue from the listener architecture; replace with the SQLite-as-queue + `Notify` wakeup.
- Update the on-disk layout description to match the new tree.

The log-format details (`events.jsonl`, k8s CRI per-sh files) are touched as adjacent context but get a proper design doc when `xrupozur` lands.

## Follow-ups

Land separately, in roughly this order:

- `xrupozur` — streaming JSONL events file and per-sh CRI log files. The log layout above is the target; this design assumes it lands before the web view but does not require it before the SQLite migration.
- `umykvluw` — pending orphans transition to `failed`, not `complete`.
- `wqwrqnpw` — move `git-dir` off the `quire/push` table. Independent cleanup.
- `yqnmmwpw` — read-only CI web view, scope B (run list + run detail post-hoc). Own design doc.
- `plpwtqvv` — supersede semantics. Adds the partial unique index on `(repo, ref_name) WHERE state IN ('pending','active')`.
- `zmtuqwly` — distinguish container-died from sh-exit. Populates `runs.failure_kind` with `container-died`.
