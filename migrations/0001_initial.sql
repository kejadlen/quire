CREATE TABLE runs (
  id                    TEXT PRIMARY KEY,
  repo                  TEXT NOT NULL,
  ref_name              TEXT NOT NULL,
  sha                   TEXT NOT NULL,
  pushed_at_ms          INTEGER NOT NULL,
  state                 TEXT NOT NULL,
  failure_kind          TEXT,
  queued_at_ms          INTEGER NOT NULL,
  started_at_ms         INTEGER,
  finished_at_ms        INTEGER,
  container_id          TEXT,
  image_tag             TEXT,
  build_started_at_ms   INTEGER,
  build_finished_at_ms  INTEGER,
  container_started_at_ms INTEGER,
  container_stopped_at_ms INTEGER,
  workspace_path        TEXT NOT NULL,

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
