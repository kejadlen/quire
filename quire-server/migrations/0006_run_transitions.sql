-- Replace state columns on `runs` with a `run_transitions` log table.
--
-- The old `runs` table has multi-column CHECK constraints that reference state,
-- started_at_ms, and finished_at_ms, so we cannot use ALTER TABLE DROP COLUMN.
-- Instead we use the SQLite rename-and-recreate pattern:
--
--   1. Rename `runs` to `_runs_old`.
--   2. CREATE new `runs` without the state columns.
--   3. INSERT old data into new `runs`.
--   4. Recreate `jobs` and `sh_events` with FK references fixed.
--      (Renaming `runs` causes SQLite to rewrite their FK text to `_runs_old`;
--       recreating them restores the FK to the new `runs`.)
--
-- PRAGMA defer_foreign_keys = ON is transaction-scoped and defers FK checks
-- to commit time so the intermediate steps (pointing at `_runs_old`) don't
-- violate FK constraints before the old table is dropped.

PRAGMA defer_foreign_keys = ON;

-- Save state data before dismantling the old runs table.
CREATE TABLE _state AS
  SELECT id, state, queued_at_ms, started_at_ms, finished_at_ms, failure_kind FROM runs;

-- Rename old runs (SQLite will rewrite dependent FK references to _runs_old).
ALTER TABLE runs RENAME TO _runs_old;

-- Recreate runs without the state columns.
CREATE TABLE runs (
  id                      TEXT PRIMARY KEY,
  repo                    TEXT NOT NULL,
  ref_name                TEXT NOT NULL,
  sha                     TEXT NOT NULL,
  pushed_at_ms            INTEGER NOT NULL,
  container_id            TEXT,
  image_tag               TEXT,
  build_started_at_ms     INTEGER,
  build_finished_at_ms    INTEGER,
  container_started_at_ms INTEGER,
  container_stopped_at_ms INTEGER,
  workspace_path          TEXT NOT NULL,
  run_token               TEXT,
  git_dir                 TEXT,
  sentry_trace_id         TEXT
);

INSERT INTO runs (id, repo, ref_name, sha, pushed_at_ms, container_id, image_tag,
  build_started_at_ms, build_finished_at_ms, container_started_at_ms, container_stopped_at_ms,
  workspace_path, run_token, git_dir, sentry_trace_id)
SELECT id, repo, ref_name, sha, pushed_at_ms, container_id, image_tag,
  build_started_at_ms, build_finished_at_ms, container_started_at_ms, container_stopped_at_ms,
  workspace_path, run_token, git_dir, sentry_trace_id
FROM _runs_old;

DROP TABLE _runs_old;

-- Rebuild indexes for the new runs table.
DROP INDEX IF EXISTS runs_repo_pushed_at;
DROP INDEX IF EXISTS runs_state;
CREATE INDEX runs_repo_pushed_at ON runs(repo, pushed_at_ms DESC);

-- Create the run_transitions table (FK now correctly references the new runs).
CREATE TABLE run_transitions (
  run_id       TEXT NOT NULL REFERENCES runs(id) ON DELETE CASCADE,
  state        TEXT NOT NULL CHECK (state IN ('pending', 'active', 'complete', 'failed', 'superseded')),
  at_ms        INTEGER NOT NULL,
  failure_kind TEXT
);
CREATE INDEX run_transitions_run_id ON run_transitions(run_id, at_ms);

-- Populate run_transitions from the saved state data.
-- Every run started as pending.
INSERT INTO run_transitions (run_id, state, at_ms)
  SELECT id, 'pending', queued_at_ms FROM _state;

-- Runs that became active.
INSERT INTO run_transitions (run_id, state, at_ms)
  SELECT id, 'active', started_at_ms FROM _state
  WHERE started_at_ms IS NOT NULL;

-- Terminal transitions (complete, failed, superseded).
INSERT INTO run_transitions (run_id, state, at_ms, failure_kind)
  SELECT id, state, finished_at_ms, failure_kind FROM _state
  WHERE finished_at_ms IS NOT NULL;

DROP TABLE _state;

-- Recreate jobs with FK reference fixed to point to the new runs.
-- (After the runs rename, SQLite rewrote jobs' FK text to _runs_old.)
ALTER TABLE jobs RENAME TO _jobs_old;

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

INSERT INTO jobs SELECT * FROM _jobs_old;
DROP TABLE _jobs_old;

-- Recreate sh_events with FK reference fixed to point to the new jobs.
-- (After the jobs rename, SQLite rewrote sh_events' FK text to _jobs_old.)
ALTER TABLE sh_events RENAME TO _sh_events_old;

CREATE TABLE sh_events (
  run_id         TEXT NOT NULL,
  job_id         TEXT NOT NULL,
  started_at_ms  INTEGER NOT NULL,
  finished_at_ms INTEGER NOT NULL,
  exit_code      INTEGER NOT NULL,
  cmd            TEXT NOT NULL,
  PRIMARY KEY (run_id, job_id, started_at_ms),
  FOREIGN KEY (run_id, job_id) REFERENCES jobs(run_id, job_id) ON DELETE CASCADE
);

INSERT INTO sh_events SELECT * FROM _sh_events_old;
DROP TABLE _sh_events_old;
