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
