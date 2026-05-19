CREATE TABLE run_transitions (
  run_id       TEXT NOT NULL REFERENCES runs(id) ON DELETE CASCADE,
  state        TEXT NOT NULL CHECK (state IN ('pending', 'active', 'complete', 'failed', 'superseded')),
  at_ms        INTEGER NOT NULL,
  failure_kind TEXT
);
CREATE INDEX run_transitions_run_id ON run_transitions(run_id, at_ms);

-- Migrate existing state data from the legacy runs columns.
-- Every run started as pending at queued_at_ms.
INSERT INTO run_transitions (run_id, state, at_ms)
  SELECT id, 'pending', queued_at_ms FROM runs;

-- Runs that became active.
INSERT INTO run_transitions (run_id, state, at_ms)
  SELECT id, 'active', started_at_ms FROM runs
  WHERE started_at_ms IS NOT NULL;

-- Terminal transitions (complete, failed, superseded).
INSERT INTO run_transitions (run_id, state, at_ms, failure_kind)
  SELECT id, state, finished_at_ms, failure_kind FROM runs
  WHERE finished_at_ms IS NOT NULL;
