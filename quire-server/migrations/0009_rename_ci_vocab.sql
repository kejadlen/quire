-- Rename CI state vocabulary on both runs and jobs.
--
-- runs:  pending → queued, complete → succeeded, superseded → canceled
-- jobs:  complete → succeeded, plus drop the unused pending/skipped/aborted
--        states from the CHECK constraint (no producer writes them today;
--        skipped support is tracked separately and would re-add it with a
--        real producer).
--
-- SQLite can't ALTER CHECK constraints, so both tables are rebuilt. The
-- INSERT…SELECT rewrites the state column inline. Pattern matches
-- migration 0007.

CREATE TABLE runs_new (
  id             TEXT    PRIMARY KEY,
  repo           TEXT    NOT NULL,
  ref_name       TEXT    NOT NULL,
  sha            TEXT    NOT NULL,
  pushed_at_ms   INTEGER NOT NULL,
  state          TEXT    NOT NULL,
  failure_kind   TEXT,
  queued_at_ms   INTEGER NOT NULL,
  started_at_ms  INTEGER,
  finished_at_ms INTEGER,
  run_token      TEXT,
  git_dir        TEXT,
  traceparent    TEXT,

  CHECK (state IN ('queued', 'active', 'succeeded', 'failed', 'canceled')),

  CHECK (started_at_ms  IS NULL OR started_at_ms  >= queued_at_ms),
  CHECK (finished_at_ms IS NULL OR finished_at_ms >= queued_at_ms),
  CHECK (finished_at_ms IS NULL OR started_at_ms  IS NULL
         OR finished_at_ms >= started_at_ms),

  CHECK (CASE state
    WHEN 'queued'    THEN started_at_ms IS NULL     AND finished_at_ms IS NULL
    WHEN 'active'    THEN started_at_ms IS NOT NULL AND finished_at_ms IS NULL
    WHEN 'succeeded' THEN started_at_ms IS NOT NULL AND finished_at_ms IS NOT NULL
    WHEN 'failed'    THEN finished_at_ms IS NOT NULL
    WHEN 'canceled'  THEN finished_at_ms IS NOT NULL
  END)
);

INSERT INTO runs_new
  SELECT id, repo, ref_name, sha, pushed_at_ms,
         CASE state
           WHEN 'pending'    THEN 'queued'
           WHEN 'complete'   THEN 'succeeded'
           WHEN 'superseded' THEN 'canceled'
           ELSE state
         END,
         failure_kind, queued_at_ms, started_at_ms, finished_at_ms,
         run_token, git_dir, traceparent
  FROM runs;

DROP TABLE runs;
ALTER TABLE runs_new RENAME TO runs;

CREATE INDEX runs_repo_pushed_at ON runs(repo, pushed_at_ms DESC);
CREATE INDEX runs_state          ON runs(state);

CREATE TABLE jobs_new (
  run_id          TEXT NOT NULL REFERENCES runs(id) ON DELETE CASCADE,
  job_id          TEXT NOT NULL,
  state           TEXT NOT NULL,
  exit_code       INTEGER,
  started_at_ms   INTEGER,
  finished_at_ms  INTEGER,

  CHECK (state IN ('active', 'succeeded', 'failed')),

  CHECK (started_at_ms IS NULL OR finished_at_ms IS NULL
         OR finished_at_ms >= started_at_ms),

  CHECK (CASE state
    WHEN 'active'    THEN started_at_ms IS NOT NULL AND finished_at_ms IS NULL
    WHEN 'succeeded' THEN started_at_ms IS NOT NULL AND finished_at_ms IS NOT NULL
    WHEN 'failed'    THEN started_at_ms IS NOT NULL AND finished_at_ms IS NOT NULL
  END),

  PRIMARY KEY (run_id, job_id)
);

INSERT INTO jobs_new
  SELECT run_id, job_id,
         CASE state
           WHEN 'complete' THEN 'succeeded'
           ELSE state
         END,
         exit_code, started_at_ms, finished_at_ms
  FROM jobs;

DROP TABLE jobs;
ALTER TABLE jobs_new RENAME TO jobs;
