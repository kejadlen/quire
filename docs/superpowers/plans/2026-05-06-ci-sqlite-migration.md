# CI SQLite Migration Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Replace the filesystem-backed CI run store with a single SQLite database, preserving the existing CI lifecycle (trigger → execute → complete/fail).

**Architecture:** A `src/db.rs` module owns the SQLite connection, migrations, and schema. `Runs` and `Run` structs query and mutate the DB instead of managing directories. State directories (`pending/`, `active/`, `complete/`, `failed/`) and per-run YAML sidecars are removed. Run directories persist only for workspace materialization and log storage.

**Tech Stack:** `rusqlite`, `rusqlite_migration`, `include_str!` for embedded SQL migrations.

**Design doc:** `docs/plans/2026-05-06-ci-sqlite-migration-design.md`

---

## File structure

| Action | File | Responsibility |
|--------|------|---------------|
| Create | `migrations/0001_initial.sql` | Schema DDL for `runs` and `jobs` tables |
| Create | `src/db.rs` | Connection management, WAL mode, migration runner |
| Modify | `Cargo.toml` | Add `rusqlite`, `rusqlite_migration` dependencies |
| Modify | `src/lib.rs` | Export `db` module |
| Modify | `src/ci/run.rs` | Rewrite `Runs` and `Run` to use SQLite; remove `write_yaml`/`read_yaml` helpers |
| Modify | `src/ci/mod.rs` | Update `trigger_ref` to pass DB conn; remove old filesystem paths |
| Modify | `src/ci/error.rs` | Add `Sql` error variant, remove `Yaml` variant |
| Modify | `src/ci/runtime.rs` | Update `DockerLifecycle` to use DB for container record writes |
| Modify | `src/ci/docker.rs` | No changes expected (shell-out layer) |
| Modify | `src/quire.rs` | Add `db()` method returning a DB handle; remove `Runs` convenience methods that are now DB-scoped |
| Modify | `src/bin/quire/server.rs` | Open DB on startup; pass to orphan reconciliation |
| Modify | `src/bin/quire/commands/ci.rs` | Use DB for `ci run` command |
| Modify | `docs/CI.md` | Update storage section, layout, lifecycle description |

---

## Task 1: Add dependencies and create migration file

**Files:**
- Create: `migrations/0001_initial.sql`
- Modify: `Cargo.toml`

- [ ] **Step 1: Add rusqlite and rusqlite_migration to Cargo.toml**

Add to `[dependencies]` in `Cargo.toml`:

```toml
rusqlite = { version = "*", features = ["bundled"] }
rusqlite_migration = "*"
```

- [ ] **Step 2: Create the initial migration file**

Create `migrations/0001_initial.sql` with the schema from the design doc:

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

- [ ] **Step 3: Verify it compiles**

Run: `cargo check --workspace`
Expected: compiles (dependencies resolve; no code uses them yet)

- [ ] **Step 4: Commit**

```
Add rusqlite dependencies and initial schema migration
```

---

## Task 2: Create `src/db.rs` — connection management and migration runner

**Files:**
- Create: `src/db.rs`
- Modify: `src/lib.rs`

- [ ] **Step 1: Create `src/db.rs`**

```rust
//! Database connection management and migration runner.
//!
//! Owns the SQLite connection, WAL mode pragma, foreign key enforcement,
//! and the ordered list of migrations. Callers borrow a connection handle
//! from [`open`] rather than opening their own.

use std::path::Path;
use std::sync::LazyLock;

use rusqlite::Connection;
use rusqlite_migration::{Migrations, M};

use crate::error::Error;

/// The ordered set of schema migrations. Append-only — never edit
/// a migration that has already shipped.
static MIGRATIONS: LazyLock<Migrations<'static>> = LazyLock::new(|| {
    Migrations::new(vec![
        M::up(include_str!("../migrations/0001_initial.sql")),
    ])
});

/// Open the database at `path`, enable WAL mode and foreign keys,
/// and run any pending migrations. Creates the file if it doesn't
/// exist.
pub fn open(path: &Path) -> Result<Connection, Error> {
    let mut conn = Connection::open(path)?;
    conn.execute_batch("PRAGMA journal_mode = WAL; PRAGMA foreign_keys = ON;")?;
    MIGRATIONS.to_latest(&mut conn)?;
    Ok(conn)
}

/// Open an in-memory database (for tests). Same pragmas and
/// migrations as the on-disk version.
#[cfg(test)]
pub fn open_in_memory() -> Result<Connection, Error> {
    let mut conn = Connection::open_in_memory()?;
    conn.execute_batch("PRAGMA foreign_keys = ON;")?;
    MIGRATIONS.to_latest(&mut conn)?;
    Ok(conn)
}
```

- [ ] **Step 2: Export the module from `src/lib.rs`**

Add `pub mod db;` to `src/lib.rs`.

- [ ] **Step 3: Add `Sql` error variant to `src/error.rs`**

Add `rusqlite` error conversion. In `src/error.rs`:

```rust
#[error(transparent)]
Sql(#[from] rusqlite::Error),
```

And in `src/ci/error.rs`:

```rust
#[error(transparent)]
Sql(#[from] rusqlite::Error),
```

Also remove the `Yaml` and `Utf8` variants from `src/ci/error.rs` once nothing uses them (will clean up in Task 3).

- [ ] **Step 4: Verify it compiles**

Run: `cargo check --workspace`

- [ ] **Step 5: Commit**

```
Add db module with SQLite connection management and migrations
```

---

## Task 3: Rewrite `Runs` and `Run` to use SQLite

This is the core of the migration. The `Runs` struct owns a DB connection (or a path to the DB file) and a base path for run directories (workspace + logs). `Run` owns a connection and a run ID.

**Files:**
- Modify: `src/ci/run.rs`
- Modify: `src/ci/error.rs`

### Key changes to `src/ci/run.rs`

**Struct changes:**

- `Runs` now holds: `db: rusqlite::Connection`, `repo: String`, `base_dir: PathBuf` (for run directories)
- `Run` now holds: `db: rusqlite::Connection`, `id: String`, `repo: String`, `base_dir: PathBuf`
- `RunState` gains `Superseded` variant
- Remove `RunMeta`, `RunTimes`, `ContainerRecord` as persistence types — their fields map to columns
- Keep `RunMeta` as an in-memory input type for `Runs::create` (callers still pass sha/ref/pushed_at)
- Remove `write_yaml` / `read_yaml` helpers

**`Runs::create` changes:**

```sql
INSERT INTO runs (id, repo, ref_name, sha, pushed_at_ms, state,
                  queued_at_ms, workspace_path)
VALUES (?, ?, ?, ?, ?, 'pending', ?, ?);
```

The `workspace_path` is `<base_dir>/<id>/workspace`. The run directory is created at create time.

**`Run::transition` changes:**

```sql
UPDATE runs SET state = ?, started_at_ms = ?, finished_at_ms = ?, container_id = NULL
WHERE id = ?;
```

Single UPDATE in a transaction. No directory renames. Timestamps stamped as in the current code.

**`Run::read_meta` / `read_times` / `write_times` changes:**

Replaced by direct column reads from the `runs` row. Expose accessor methods instead of returning structs:

- `Run::sha()`, `Run::ref_name()`, `Run::pushed_at_ms()` — read from DB
- `Run::started_at_ms()`, `Run::finished_at_ms()` — read from DB
- `Run::state()` — cached from last query or read fresh

**`Run::read_container_record` / `write_container_record` changes:**

`container_id` is a column on `runs`. The container timestamps (build_started_at, etc.) are not in the current schema — they can be added to the `runs` table in a follow-up migration. For now, keep writing `container.yml` as a file in the run directory for the container lifecycle timestamps, and only track `container_id` in the DB. This is consistent with the design doc's schema which only has `container_id`.

**`DockerLifecycle` changes:**

`record_path` still points to `<run-dir>/container.yml` for the container timestamps. The `container_id` is also written to the DB when it's set. The `Drop` impl continues to write `container_stopped_at` to the YAML file.

**`Runs::scan_orphans` changes:**

```sql
SELECT id, state FROM runs WHERE state IN ('pending', 'active') AND repo = ?;
```

No more directory scanning. Quarantine concept goes away (unreadable runs were a filesystem artifact).

**`Runs::reconcile_orphans` changes:**

```sql
UPDATE runs SET state = 'failed', finished_at_ms = ?, container_id = NULL, failure_kind = 'orphaned'
WHERE state = 'active' AND repo = ?;
```

For pending orphans, the design doc says `umykvluw` lands separately. Current behavior transitions them to `complete`. Keep that behavior for now but use the DB:

```sql
UPDATE runs SET state = 'complete', finished_at_ms = ?, container_id = NULL
WHERE state = 'pending' AND repo = ?;
```

**`Run::path` changes:**

Returns `<base_dir>/<id>/` — the run directory for workspace and logs. No state subdirectory.

**`Run::update_latest` changes:**

Removed entirely. The `latest` symlink was a filesystem workaround; the DB query `SELECT id FROM runs WHERE repo = ? ORDER BY queued_at_ms DESC LIMIT 1` replaces it.

**`Run::write_all_logs` changes:**

Stays the same — writes YAML log files under `<run-dir>/jobs/<job-id>/log.yml`. Logs live on disk per the design doc.

- [ ] **Step 1: Write the new `RunState` with `Superseded` variant and accessor methods**

Update `RunState` to include `Superseded`:

```rust
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RunState {
    Pending,
    Active,
    Complete,
    Failed,
    Superseded,
}

impl RunState {
    pub fn as_str(&self) -> &'static str {
        match self {
            RunState::Pending => "pending",
            RunState::Active => "active",
            RunState::Complete => "complete",
            RunState::Failed => "failed",
            RunState::Superseded => "superseded",
        }
    }

    pub fn from_str(s: &str) -> Option<Self> {
        match s {
            "pending" => Some(RunState::Pending),
            "active" => Some(RunState::Active),
            "complete" => Some(RunState::Complete),
            "failed" => Some(RunState::Failed),
            "superseded" => Some(RunState::Superseded),
            _ => None,
        }
    }
}
```

Remove `dir_name()`.

- [ ] **Step 2: Rewrite `Runs` struct**

```rust
pub struct Runs {
    db: rusqlite::Connection,
    repo: String,
    base_dir: PathBuf,
}
```

`base_dir` is `<quire-root>/runs/<repo>/`. Run directories live at `<base_dir>/<id>/`.

Update constructor:

```rust
impl Runs {
    pub fn new(db: rusqlite::Connection, repo: String, base_dir: PathBuf) -> Self {
        Self { db, repo, base_dir }
    }
}
```

- [ ] **Step 3: Rewrite `Runs::create`**

```rust
pub fn create(&self, meta: &RunMeta) -> Result<Run> {
    let id = uuid::Uuid::now_v7().to_string();
    let workspace_path = self.base_dir.join(&id).join("workspace");

    self.db.execute(
        "INSERT INTO runs (id, repo, ref_name, sha, pushed_at_ms, state, queued_at_ms, workspace_path)
         VALUES (?1, ?2, ?3, ?4, ?5, 'pending', ?6, ?7)",
        rusqlite::params![
            &id,
            &self.repo,
            &meta.r#ref,
            &meta.sha,
            meta.pushed_at.as_millisecond(),
            jiff::Timestamp::now().as_millisecond(),
            workspace_path.to_str().ok_or_else(|| std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                "workspace path is not valid UTF-8",
            ))?,
        ],
    )?;

    // Create run directory for workspace and logs.
    fs_err::create_dir_all(&workspace_path)?;

    Ok(Run {
        id,
        repo: self.repo.clone(),
        base_dir: self.base_dir.clone(),
        state: RunState::Pending,
    })
}
```

Note: `Run` caches `state` in memory to avoid a round-trip after creation. It also needs a way to execute DB statements. Two options:
- (A) `Run` clones the connection or holds a reference
- (B) `Run` takes `&Connection` on each method call

Option (B) is cleaner for borrowing but makes `Run::execute` harder since it consumes `self`. Go with option (A): `Run` holds its own `rusqlite::Connection`. SQLite allows multiple connections to the same file in WAL mode. Alternatively, since `rusqlite::Connection` is not `Clone`, `Run` can take ownership of the connection from `Runs`.

Actually, the simplest approach: `Runs` holds the DB path (not a connection), and each method opens a short-lived connection. Or better: pass `&Connection` to each method. Since `Run::execute` needs to do many operations, it should hold its own connection.

Let me reconsider: `Runs` creates `Run` objects. The caller (trigger_ref, server startup) has a connection. Let's make `Runs` hold a `&Connection` lifetime... but that gets messy with ownership.

Simplest correct approach: `Runs` owns a `Connection`. `Run` borrows `&Connection`. But `Run::execute` consumes `self` and the pipeline, and the runtime needs its own state...

Final decision: `Run` holds its own `rusqlite::Connection`. It opens a new connection to the same DB file. This is standard SQLite practice — multiple connections in WAL mode are fine. `Runs` holds the DB path and opens connections as needed.

Update:

```rust
pub struct Runs {
    db_path: PathBuf,
    repo: String,
    base_dir: PathBuf,
}

impl Runs {
    pub fn new(db_path: PathBuf, repo: String, base_dir: PathBuf) -> Self {
        Self { db_path, repo, base_dir }
    }

    fn conn(&self) -> Result<rusqlite::Connection> {
        let conn = rusqlite::Connection::open(&self.db_path)?;
        conn.execute_batch("PRAGMA journal_mode = WAL; PRAGMA foreign_keys = ON;")?;
        Ok(conn)
    }

    pub fn create(&self, meta: &RunMeta) -> Result<Run> {
        let conn = self.conn()?;
        // ... insert ...
        Run::open(conn, self.repo.clone(), self.base_dir.clone(), &id)
    }
}
```

Hmm, but opening a new connection per operation is wasteful. Let me think about this differently.

Better approach: `Runs` holds a `rusqlite::Connection`. `Run::execute` is where the long-lived operation happens. Before `execute`, `Run` can get its own connection (or we pass one in). For the simpler methods (transition, read_meta, etc.), `Run` can borrow from... somewhere.

Actually, the cleanest solution: make `Run` hold a `rusqlite::Connection` that it receives at construction. `Runs` opens a connection in `create` and transfers it to the new `Run`. For methods that don't consume `self` (like `read_meta`, `transition`), `Run` uses its owned connection. For `execute`, it already owns one.

But then `Runs` needs a new connection for each `create` call. Unless `Runs` doesn't hold a connection at all — it holds the db path.

Let me look at how `Runs` is used:

1. `Runs::new(base)` — constructed with a path
2. `runs.create(&meta)` — returns a `Run`
3. `runs.scan_orphans()` — returns `Vec<Run>`
4. `runs.reconcile_orphans()` — internally iterates

And `Run` is used:
1. `run.id()`, `run.state()`, `run.path()`
2. `run.transition(RunState::Active)`
3. `run.read_meta()`, `run.read_times()`, `run.write_times()`
4. `run.read_container_record()`, `run.write_container_record()`
5. `run.execute(pipeline, secrets, git_dir, workspace, executor)` — consumes self

The pattern is: `Runs` creates `Run` objects, and callers work with `Run` objects. Both need DB access.

Simplest clean approach: both `Runs` and `Run` hold `rusqlite::Connection`. Since SQLite supports multiple connections in WAL mode, `Runs::create` opens a new connection for the new `Run`. For `Runs` methods like `scan_orphans` and `reconcile_orphans`, it uses its own connection.

- [ ] **Step 4: Rewrite `Run` struct**

```rust
pub struct Run {
    db: rusqlite::Connection,
    id: String,
    state: RunState,
    base_dir: PathBuf,
}
```

Methods use `self.db` for all reads/writes. No more filesystem state management.

- [ ] **Step 5: Rewrite `Run::transition`**

```rust
pub fn transition(&mut self, to: RunState) -> Result<()> {
    let allowed = matches!(
        (self.state, to),
        (RunState::Pending, RunState::Active)
        | (RunState::Pending, RunState::Complete)
        | (RunState::Active, RunState::Complete)
        | (RunState::Active, RunState::Failed)
    );
    if !allowed {
        return Err(Error::InvalidTransition { from: self.state, to });
    }

    let now = jiff::Timestamp::now().as_millisecond();

    self.db.execute(
        "UPDATE runs SET state = ?1,
            started_at_ms = CASE WHEN ?2 = 'active' AND started_at_ms IS NULL THEN ?3 ELSE started_at_ms END,
            finished_at_ms = CASE WHEN ?2 IN ('complete', 'failed') AND finished_at_ms IS NULL THEN ?3 ELSE finished_at_ms END,
            container_id = CASE WHEN ?2 IN ('complete', 'failed') THEN NULL ELSE container_id END
         WHERE id = ?4",
        rusqlite::params![to.as_str(), to.as_str(), now, &self.id],
    )?;

    self.state = to;
    Ok(())
}
```

- [ ] **Step 6: Rewrite `Run::read_meta` as column accessors**

```rust
pub fn read_meta(&self) -> Result<RunMeta> {
    let (sha, ref_name, pushed_at_ms) = self.db.query_row(
        "SELECT sha, ref_name, pushed_at_ms FROM runs WHERE id = ?1",
        rusqlite::params![&self.id],
        |row| Ok((row.get(0)?, row.get(1)?, row.get::<_, i64>(2)?)),
    )?;
    Ok(RunMeta {
        sha,
        r#ref: ref_name,
        pushed_at: jiff::Timestamp::from_millisecond(pushed_at_ms).expect("valid timestamp"),
    })
}
```

- [ ] **Step 7: Remove `write_yaml` / `read_yaml`, `RunTimes`, `ContainerRecord` persistence**

`RunTimes` and `ContainerRecord` as separate YAML-backed types go away. Timestamps are columns on `runs`. `container_id` is a column. Container lifecycle timestamps (build_started_at etc.) can stay as a `container.yml` file in the run dir for now, since the schema only tracks `container_id`.

Keep `RunMeta` as an in-memory struct passed to `Runs::create`. Remove `RunTimes` as a public type — callers use `run.started_at()` etc.

Actually, keep `ContainerRecord` around for the file-based container timestamps, since the DB schema only has `container_id`. The `DockerLifecycle` still writes `container.yml`.

- [ ] **Step 8: Rewrite `Run::path`**

```rust
pub fn path(&self) -> PathBuf {
    self.base_dir.join(&self.id)
}
```

No state subdirectory. The run dir is always `<base_dir>/<id>/`.

- [ ] **Step 9: Remove `update_latest`**

No more symlink. Remove the method entirely.

- [ ] **Step 10: Rewrite `Runs::scan_orphans` and `reconcile_orphans`**

```rust
pub fn scan_orphans(&self) -> Result<Vec<Run>> {
    let mut stmt = self.db.prepare(
        "SELECT id, state FROM runs WHERE state IN ('pending', 'active') AND repo = ?1"
    )?;
    let rows = stmt.query_map(rusqlite::params![&self.repo], |row| {
        let id: String = row.get(0)?;
        let state_str: String = row.get(1)?;
        let state = RunState::from_str(&state_str).expect("DB enforces valid states");
        Ok((id, state))
    })?;

    let mut orphans = Vec::new();
    for row in rows {
        let (id, state) = row?;
        let db = self.conn()?;  // each Run gets its own connection
        orphans.push(Run { db, id, state, base_dir: self.base_dir.clone() });
    }
    Ok(orphans)
}
```

```rust
pub fn reconcile_orphans(&self) -> Result<()> {
    let now = jiff::Timestamp::now().as_millisecond();

    // Active orphans → failed
    self.db.execute(
        "UPDATE runs SET state = 'failed', finished_at_ms = ?1, container_id = NULL, failure_kind = 'orphaned'
         WHERE state = 'active' AND repo = ?2",
        rusqlite::params![now, &self.repo],
    )?;

    // Pending orphans → complete (matching current behavior; umykvluw changes this to failed)
    self.db.execute(
        "UPDATE runs SET state = 'complete', finished_at_ms = ?1, container_id = NULL
         WHERE state = 'pending' AND repo = ?2",
        rusqlite::params![now, &self.repo],
    )?;

    Ok(())
}
```

- [ ] **Step 11: Rewrite `Run::execute` to use DB**

The execute method is the most complex. Key changes:
- `self.transition(RunState::Active)` works as before (now DB-backed)
- `build_executor_runtime` writes `container_id` to the DB instead of (or in addition to) `container.yml`
- `write_all_logs` stays file-based (logs on disk per design doc)
- The `DockerLifecycle.record_path` stays for container timestamps, but `container_id` is tracked in the DB

- [ ] **Step 12: Update `Run::build_executor_runtime`**

After building the container and getting the session, write `container_id` to the DB:

```rust
self.db.execute(
    "UPDATE runs SET container_id = ?1 WHERE id = ?2",
    rusqlite::params![&session.container_id, &self.id],
)?;
```

Keep writing `container.yml` for the build/container timestamps.

- [ ] **Step 13: Remove unused error variants from `src/ci/error.rs`**

Remove `Yaml` and `Utf8` variants if nothing uses them. Add `Sql` variant.

- [ ] **Step 14: Run tests and fix compilation errors**

Run: `cargo check --workspace`
Then: `cargo test --workspace -q`

Fix any compilation errors. The tests in `run.rs` will need updating since they construct `Runs` with the old API.

- [ ] **Step 15: Update tests in `src/ci/run.rs`**

Key test changes:
- `tmp_quire()` helpers create an in-memory DB via `db::open_in_memory()` or open a temp file DB
- `test_runs()` creates `Runs::new(db_path, "test.git".to_string(), base_dir)`
- Tests no longer check for state directories (`pending/`, `active/`, etc.)
- Tests check run directories at `<base_dir>/<id>/`
- `scan_orphans` tests verify DB queries instead of directory scans
- Remove `create_symlinks_latest` test (no more symlink)
- Remove `scan_orphans_quarantines_unreadable_runs` test (no more quarantine — that was a filesystem artifact)
- Update `transition_errors_on_missing_source` — no more missing directory, but could test with a run ID that doesn't exist in the DB

- [ ] **Step 16: Run full test suite**

Run: `cargo test --workspace -q`

- [ ] **Step 17: Commit**

```
Rewrite Runs and Run to use SQLite for state storage
```

---

## Task 4: Update `src/ci/mod.rs` — trigger path

**Files:**
- Modify: `src/ci/mod.rs`

The `trigger_ref` function currently calls `ci.runs(repo.runs_base()).create(&meta)`. After the migration, it needs to pass the DB connection/path.

- [ ] **Step 1: Update `Ci::runs` signature**

Change from:

```rust
pub fn runs(&self, runs_base: PathBuf) -> Runs
```

To:

```rust
pub fn runs(&self, db_path: &Path, repo: &str, runs_base: PathBuf) -> Runs
```

Or, better: pass through the Quire-level DB path. The `trigger` function has access to `quire`, so it can pass `quire.db_path()`.

- [ ] **Step 2: Update `trigger` and `trigger_ref` functions**

```rust
pub fn trigger(quire: &crate::Quire, event: &PushEvent) {
    // ... existing repo resolution ...
    let db_path = quire.db_path();
    for push_ref in event.updated_refs() {
        if let Err(e) = trigger_ref(&repo, &db_path, event.pushed_at, push_ref, &secrets) {
            // ... error handling ...
        }
    }
}

fn trigger_ref(
    repo: &Repo,
    db_path: &Path,
    pushed_at: jiff::Timestamp,
    push_ref: &PushRef,
    secrets: &HashMap<String, crate::secret::SecretString>,
) -> error::Result<()> {
    // ... existing code ...
    let mut run = ci.runs(db_path, repo.name(), repo.runs_base()).create(&meta)?;
    // ... rest stays largely the same ...
}
```

- [ ] **Step 3: Update tests in `src/ci/mod.rs`**

Tests that create `Runs` need the new signature. Use `db::open_in_memory()` or a temp file for the DB.

- [ ] **Step 4: Run tests**

Run: `cargo test --workspace -q`

- [ ] **Step 5: Commit**

```
Update CI trigger path to use SQLite
```

---

## Task 5: Update `src/quire.rs` — DB path accessor

**Files:**
- Modify: `src/quire.rs`

- [ ] **Step 1: Add `db_path` method to `Quire`**

```rust
pub fn db_path(&self) -> PathBuf {
    self.base_dir.join("quire.db")
}
```

- [ ] **Step 2: Update `Repo::runs` and `Repo::runs_base`**

`Repo::runs` currently returns `Runs::new(self.runs_base())`. Update to pass the DB path:

```rust
pub fn runs(&self, db_path: &Path) -> Runs {
    Runs::new(
        db_path.to_path_buf(),
        self.name().to_string(),
        self.runs_base(),
    )
}
```

Or remove the convenience method and let callers construct `Runs` directly with the right params.

- [ ] **Step 3: Run tests**

Run: `cargo test --workspace -q`

- [ ] **Step 4: Commit**

```
Add DB path accessor to Quire and update Repo::runs
```

---

## Task 6: Update server startup — open DB and reconcile orphans

**Files:**
- Modify: `src/bin/quire/server.rs`

- [ ] **Step 1: Open the database on startup**

Add after the socket setup, before orphan reconciliation:

```rust
let db_path = quire.db_path();
tracing::info!(path = %db_path.display(), "opening database");
let db = crate::db::open(&db_path)?;
```

- [ ] **Step 2: Update orphan reconciliation to use DB**

```rust
for repo in quire.repos().context("failed to list repos")? {
    let runs = repo.runs(&db_path);
    runs.reconcile_orphans()?;
}
```

Note: if `Runs` holds a connection (not a path), the server would pass a reference or the path. If `Runs` takes a path and opens its own connection, this is straightforward.

- [ ] **Step 3: Run tests**

Run: `cargo test --workspace -q`

- [ ] **Step 4: Commit**

```
Open SQLite database on server startup for orphan reconciliation
```

---

## Task 7: Update `ci run` CLI command

**Files:**
- Modify: `src/bin/quire/commands/ci.rs`

- [ ] **Step 1: Update `ci::run` to use DB**

The `run` function creates a `Runs` with a tempdir. Now it needs a DB path. Use a temp file for the DB:

```rust
let db_path = tmp.path().join("quire.db");
let db = quire::db::open(&db_path)?;
let runs = Runs::new(db_path, "local".to_string(), tmp.path().to_path_buf());
```

- [ ] **Step 2: Run tests**

Run: `cargo test --workspace -q`

- [ ] **Step 3: Commit**

```
Update ci run command to use SQLite
```

---

## Task 8: Update `src/ci/runtime.rs` — DockerLifecycle DB writes

**Files:**
- Modify: `src/ci/runtime.rs`

The `DockerLifecycle` currently writes `container_stopped_at` to a YAML file. After the migration:
- `container_id` is tracked in the DB
- Container lifecycle timestamps can stay in `container.yml` for now (the DB schema only has `container_id`)
- The `Drop` impl for `DockerLifecycle` needs a DB connection to clear `container_id`

The challenge: `DockerLifecycle` needs DB access in its `Drop` impl. Options:
- (A) Give `DockerLifecycle` the DB path so it can open a connection in `Drop`
- (B) Keep writing container timestamps to `container.yml` only; the DB `container_id` is managed by `Run::build_executor_runtime` and `Run::transition` (which already clears it)

Go with (B): `Run::transition` already sets `container_id = NULL` when transitioning to Complete/Failed. The `DockerLifecycle` only needs to write `container_stopped_at` to the YAML file. No changes to `DockerLifecycle` needed beyond what's already handled by the transition logic.

- [ ] **Step 1: Verify `DockerLifecycle` Drop still works**

The `Drop` impl writes to `container.yml` at `self.record_path`. This path is still valid since run directories still exist at `<base_dir>/<id>/`. No changes needed.

- [ ] **Step 2: Verify container_id is cleared on state transition**

The `Run::transition` SQL already sets `container_id = NULL` for complete/failed states. Confirm this is working.

- [ ] **Step 3: Commit (if changes needed, otherwise skip)**

---

## Task 9: Update `docs/CI.md`

**Files:**
- Modify: `docs/CI.md`

- [ ] **Step 1: Update the "Storage" section**

Remove "No SQLite in v1" and the secondary-index-only commitment. Replace with the SQLite-as-primary-store description from the design doc.

- [ ] **Step 2: Update the volume layout**

Replace the directory-based run layout with:

```
/var/quire/
  quire.db                       # SQLite database
  repos/<name>.git/              # bare repos, unchanged
  runs/<repo>/<run-id>/          # per-run workspace
    workspace/                   # materialized checkout
    jobs/<job-id>/
      log.yml                    # per-job sh output logs
```

- [ ] **Step 3: Update the lifecycle description**

Replace the directory-rename lifecycle with SQL state transitions.

- [ ] **Step 4: Remove the in-memory queue / mpsc references**

The design doc says SQLite is the queue. The `mpsc` references in CI.md should be updated (or noted as "replaced by DB queries" — the actual queue replacement is a follow-up since the runner isn't built yet).

- [ ] **Step 5: Commit**

```
Update CI docs for SQLite migration
```

---

## Task 10: Clean up and final verification

- [ ] **Step 1: Remove dead code**

- Remove `write_yaml` and `read_yaml` helpers from `run.rs`
- Remove `Yaml` error variant from `ci/error.rs` if unused
- Remove `repo_segment` function if unused (was for Docker image tags from path — check if still needed)
- Remove any `serde_yaml_ng` usage in `run.rs` that's no longer needed

- [ ] **Step 2: Run `just all`**

Run: `just all`
Expected: all checks pass (fmt, clippy, test)

- [ ] **Step 3: Run coverage**

Run: `just coverage`
Expected: 100% coverage maintained

- [ ] **Step 4: Commit**

```
Clean up dead code from filesystem run store
```
