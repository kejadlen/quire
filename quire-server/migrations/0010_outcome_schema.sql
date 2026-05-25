-- Replace state/failure_kind columns with timestamp-based lifecycle and
-- outcome enum. Column renames:
--   queued_at_ms   → created_at
--   started_at_ms  → dispatched_at
--   finished_at_ms → resolved_at
--
-- Lifecycle is now derived from whether timestamps are set:
--   queued:   dispatched_at IS NULL AND resolved_at IS NULL
--   active:   dispatched_at IS NOT NULL AND resolved_at IS NULL
--   resolved: resolved_at IS NOT NULL (outcome IS NOT NULL)
--
-- outcome replaces state+failure_kind for resolved runs:
--   state='succeeded'                          → 'succeeded'
--   state='failed', failure_kind='orphaned'    → 'failed-orphaned'
--   state='failed', failure_kind='process-crashed' → 'failed-internal'
--   state='failed' (other)                     → 'failed-pipeline'
--   state='canceled'                           → 'superseded'

CREATE TABLE runs_new (
  id             TEXT    PRIMARY KEY,
  repo           TEXT    NOT NULL,
  ref_name       TEXT    NOT NULL,
  sha            TEXT    NOT NULL,
  pushed_at_ms   INTEGER NOT NULL,
  created_at     INTEGER NOT NULL,
  dispatched_at  INTEGER,
  resolved_at    INTEGER,
  outcome        TEXT,
  run_token      TEXT,
  git_dir        TEXT,
  traceparent    TEXT,

  CHECK (dispatched_at IS NULL OR dispatched_at >= created_at),
  CHECK (resolved_at   IS NULL OR resolved_at   >= created_at),
  CHECK (resolved_at   IS NULL OR dispatched_at IS NULL
         OR resolved_at >= dispatched_at),

  CHECK ((resolved_at IS NULL) = (outcome IS NULL)),

  CHECK (outcome IS NULL OR outcome IN (
    'succeeded',
    'failed-pipeline', 'failed-orphaned', 'failed-internal',
    'superseded'
  ))
);

INSERT INTO runs_new
  SELECT
    id, repo, ref_name, sha, pushed_at_ms,
    queued_at_ms,
    started_at_ms,
    finished_at_ms,
    CASE
      WHEN state IN ('queued', 'active') THEN NULL
      WHEN state = 'succeeded' THEN 'succeeded'
      WHEN state = 'failed' THEN
        CASE failure_kind
          WHEN 'orphaned'        THEN 'failed-orphaned'
          WHEN 'process-crashed' THEN 'failed-internal'
          ELSE                        'failed-pipeline'
        END
      WHEN state = 'canceled' THEN 'superseded'
      ELSE NULL
    END,
    run_token, git_dir, traceparent
  FROM runs;

DROP TABLE runs;
ALTER TABLE runs_new RENAME TO runs;

CREATE INDEX runs_repo_pushed_at ON runs(repo, pushed_at_ms DESC);
CREATE INDEX runs_pending ON runs(created_at) WHERE outcome IS NULL;
