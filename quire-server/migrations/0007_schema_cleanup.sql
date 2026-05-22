-- Drop dead-weight columns from runs: container_id, workspace_path,
-- image_tag, build_started_at_ms, build_finished_at_ms,
-- container_started_at_ms, container_stopped_at_ms, sentry_trace_id.
--
-- container_id appears in CHECK constraints, so we must recreate the table
-- rather than using ALTER TABLE DROP COLUMN.

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

  CHECK (state IN ('pending', 'active', 'complete', 'failed', 'superseded')),

  CHECK (started_at_ms  IS NULL OR started_at_ms  >= queued_at_ms),
  CHECK (finished_at_ms IS NULL OR finished_at_ms >= queued_at_ms),
  CHECK (finished_at_ms IS NULL OR started_at_ms  IS NULL
         OR finished_at_ms >= started_at_ms),

  CHECK (CASE state
    WHEN 'pending'    THEN started_at_ms IS NULL     AND finished_at_ms IS NULL
    WHEN 'active'     THEN started_at_ms IS NOT NULL AND finished_at_ms IS NULL
    WHEN 'complete'   THEN started_at_ms IS NOT NULL AND finished_at_ms IS NOT NULL
    WHEN 'failed'     THEN finished_at_ms IS NOT NULL
    WHEN 'superseded' THEN finished_at_ms IS NOT NULL
  END)
);

INSERT INTO runs_new
  SELECT id, repo, ref_name, sha, pushed_at_ms, state, failure_kind,
         queued_at_ms, started_at_ms, finished_at_ms,
         run_token, git_dir, traceparent
  FROM runs;

DROP TABLE runs;
ALTER TABLE runs_new RENAME TO runs;

CREATE INDEX runs_repo_pushed_at ON runs(repo, pushed_at_ms DESC);
CREATE INDEX runs_state          ON runs(state);
