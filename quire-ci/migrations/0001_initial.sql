CREATE TABLE runs (
  id             TEXT    PRIMARY KEY,
  repo           TEXT    NOT NULL,
  ref_name       TEXT    NOT NULL,
  sha            TEXT    NOT NULL,
  created_at     INTEGER NOT NULL,
  dispatched_at  INTEGER,
  resolved_at    INTEGER,
  outcome        TEXT,
  traceparent    TEXT,

  -- timestamps move forward
  CHECK (dispatched_at IS NULL OR dispatched_at >= created_at),
  CHECK (resolved_at   IS NULL OR resolved_at   >= created_at),
  CHECK (resolved_at   IS NULL OR dispatched_at IS NULL
         OR resolved_at >= dispatched_at),

  -- resolved_at and outcome travel together
  CHECK ((resolved_at IS NULL) = (outcome IS NULL)),

  -- outcome enum
  CHECK (outcome IS NULL OR outcome IN (
    'succeeded',
    'failed-pipeline', 'failed-orphaned', 'failed-internal',
    'superseded'
  ))
);

-- Pending work: queue scans only touch unresolved rows.
CREATE INDEX runs_pending ON runs(created_at) WHERE outcome IS NULL;

-- Listing runs per repo, most recent first.
CREATE INDEX runs_repo_created_at ON runs(repo, created_at DESC);
