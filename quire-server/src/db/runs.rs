//! All SQL queries for the `runs`, `jobs`, and `sh` tables.
//!
//! Every method is on [`super::Db`] — callers are responsible for
//! acquiring and releasing the connection. None of these methods open
//! their own connections.

use rusqlite::params;

// ── Types ────────────────────────────────────────────────────────────────────

/// Raw run row returned by list/detail queries.
pub struct RunRow {
    pub id: String,
    pub outcome: Option<String>,
    pub sha: String,
    pub ref_name: String,
    pub created_at: i64,
    pub dispatched_at: Option<i64>,
    pub resolved_at: Option<i64>,
}

/// Raw job row returned by detail queries.
pub struct JobRow {
    pub job_id: String,
    pub state: String,
    pub exit_code: Option<i32>,
    pub started_at_ms: Option<i64>,
    pub finished_at_ms: Option<i64>,
}

/// Raw shell-event row returned by detail queries.
pub struct ShEvent {
    pub job_id: String,
    pub started_at_ms: i64,
    pub finished_at_ms: i64,
    pub exit_code: i32,
    pub cmd: String,
}

/// Aggregated run detail (run + jobs + sh_events).
pub struct RunDetail {
    pub run: RunRow,
    pub jobs: Vec<JobRow>,
    pub sh_events: Vec<ShEvent>,
}

/// Bootstrap data fetched by `GET /api/run/bootstrap`.
pub struct BootstrapData {
    pub sha: String,
    pub ref_name: String,
    pub pushed_at_ms: i64,
    pub git_dir: Option<String>,
    pub traceparent: Option<String>,
    pub dispatched_at: Option<i64>,
    pub repo: String,
}

// ── Parameter structs ─────────────────────────────────────────────────────────

/// Parameters for inserting a new run row.
pub struct NewRun<'a> {
    pub id: &'a str,
    pub repo: &'a str,
    pub ref_name: &'a str,
    pub sha: &'a str,
    pub pushed_at_ms: i64,
    pub created_at: i64,
    pub run_token: Option<&'a str>,
}

/// Parameters for inserting a job row.
pub struct NewJob<'a> {
    pub run_id: &'a str,
    pub job_id: &'a str,
    pub state: &'a str,
    pub exit_code: Option<i32>,
    pub started_at_ms: i64,
    pub finished_at_ms: i64,
}

/// Parameters for inserting a shell-event row.
pub struct NewShEvent<'a> {
    pub run_id: &'a str,
    pub job_id: &'a str,
    pub started_at_ms: i64,
    pub finished_at_ms: i64,
    pub exit_code: i32,
    pub cmd: &'a str,
}

// ── Seeding ───────────────────────────────────────────────────────────────────

/// A run row with all state fields explicit — for dev seeding and test fixtures.
pub struct SeededRun<'a> {
    pub id: &'a str,
    pub repo: &'a str,
    pub ref_name: &'a str,
    pub sha: &'a str,
    pub pushed_at_ms: i64,
    pub created_at: i64,
    pub dispatched_at: Option<i64>,
    pub resolved_at: Option<i64>,
    pub outcome: Option<&'a str>,
}

// ── Methods ──────────────────────────────────────────────────────────────────

impl super::Db {
    // ── Inserts ──────────────────────────────────────────────────────────────

    pub fn insert_run(&self, p: &NewRun<'_>) -> super::Result<()> {
        self.0.execute(
            "INSERT INTO runs (id, repo, ref_name, sha, pushed_at_ms, created_at, run_token)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
            params![
                p.id,
                p.repo,
                p.ref_name,
                p.sha,
                p.pushed_at_ms,
                p.created_at,
                p.run_token
            ],
        )?;
        Ok(())
    }

    pub fn insert_job(&self, p: &NewJob<'_>) -> super::Result<()> {
        self.0.execute(
            "INSERT INTO jobs (run_id, job_id, state, exit_code, started_at_ms, finished_at_ms)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            params![
                p.run_id,
                p.job_id,
                p.state,
                p.exit_code,
                p.started_at_ms,
                p.finished_at_ms
            ],
        )?;
        Ok(())
    }

    pub fn insert_sh_event(&self, p: &NewShEvent<'_>) -> super::Result<()> {
        self.0.execute(
            "INSERT INTO sh (run_id, job_id, started_at_ms, finished_at_ms, exit_code, cmd)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            params![
                p.run_id,
                p.job_id,
                p.started_at_ms,
                p.finished_at_ms,
                p.exit_code,
                p.cmd
            ],
        )?;
        Ok(())
    }

    // ── Selects ──────────────────────────────────────────────────────────────

    /// Return `(sha, ref_name, pushed_at_ms)` for a run.
    pub fn get_run_meta(&self, id: &str) -> super::Result<(String, String, i64)> {
        self.0
            .query_row(
                "SELECT sha, ref_name, pushed_at_ms FROM runs WHERE id = ?1",
                params![id],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
            )
            .map_err(Into::into)
    }

    /// Return `(dispatched_at, resolved_at)` for a run — used to derive lifecycle state.
    pub fn get_run_lifecycle(&self, id: &str) -> super::Result<(Option<i64>, Option<i64>)> {
        self.0
            .query_row(
                "SELECT dispatched_at, resolved_at FROM runs WHERE id = ?1",
                params![id],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .map_err(Into::into)
    }

    /// Return `dispatched_at` for a run.
    pub fn get_run_dispatched_at(&self, id: &str) -> super::Result<Option<i64>> {
        self.0
            .query_row(
                "SELECT dispatched_at FROM runs WHERE id = ?1",
                params![id],
                |row| row.get(0),
            )
            .map_err(Into::into)
    }

    /// Return `resolved_at` for a run.
    pub fn get_run_resolved_at(&self, id: &str) -> super::Result<Option<i64>> {
        self.0
            .query_row(
                "SELECT resolved_at FROM runs WHERE id = ?1",
                params![id],
                |row| row.get(0),
            )
            .map_err(Into::into)
    }

    /// Return the outcome string for a run, or `None` if not yet resolved.
    pub fn get_run_outcome(&self, id: &str) -> super::Result<Option<String>> {
        self.0
            .query_row(
                "SELECT outcome FROM runs WHERE id = ?1",
                params![id],
                |row| row.get(0),
            )
            .map_err(Into::into)
    }

    /// Look up a run by its bearer token. Returns `Err(QueryReturnedNoRows)` if no match.
    pub fn get_run_id_for_token(&self, token: &str) -> super::Result<String> {
        self.0
            .query_row(
                "SELECT id FROM runs WHERE run_token = ?1",
                params![token],
                |row| row.get(0),
            )
            .map_err(Into::into)
    }

    /// Return the bootstrap data for a run, or `None` if no row exists.
    pub fn get_run_bootstrap_data(&self, id: &str) -> super::Result<Option<BootstrapData>> {
        let mut stmt = self.0.prepare(
            "SELECT sha, ref_name, pushed_at_ms, git_dir, traceparent, dispatched_at, repo
             FROM runs WHERE id = ?1",
        )?;
        let mut rows = stmt.query(params![id])?;
        match rows.next()? {
            None => Ok(None),
            Some(row) => Ok(Some(BootstrapData {
                sha: row.get(0)?,
                ref_name: row.get(1)?,
                pushed_at_ms: row.get(2)?,
                git_dir: row.get(3)?,
                traceparent: row.get(4)?,
                dispatched_at: row.get(5)?,
                repo: row.get(6)?,
            })),
        }
    }

    /// All active run IDs for `(repo, ref_name)` — dispatched but not yet resolved.
    pub fn get_active_runs_for_ref(
        &self,
        repo: &str,
        ref_name: &str,
    ) -> super::Result<Vec<String>> {
        self.0
            .prepare(
                "SELECT id FROM runs
             WHERE repo = ?1 AND ref_name = ?2
               AND dispatched_at IS NOT NULL AND resolved_at IS NULL",
            )?
            .query_map(params![repo, ref_name], |row| row.get(0))?
            .collect::<rusqlite::Result<Vec<_>>>()
            .map_err(Into::into)
    }

    /// The 50 most-recent runs for `repo`, ordered newest first.
    pub fn list_runs(&self, repo: &str) -> super::Result<Vec<RunRow>> {
        self.0
            .prepare(
                "SELECT id, outcome, sha, ref_name, created_at, dispatched_at, resolved_at
             FROM runs WHERE repo = ?1
             ORDER BY created_at DESC
             LIMIT 50",
            )?
            .query_map(params![repo], |row| {
                Ok(RunRow {
                    id: row.get(0)?,
                    outcome: row.get(1)?,
                    sha: row.get(2)?,
                    ref_name: row.get(3)?,
                    created_at: row.get(4)?,
                    dispatched_at: row.get(5)?,
                    resolved_at: row.get(6)?,
                })
            })?
            .collect::<rusqlite::Result<Vec<_>>>()
            .map_err(Into::into)
    }

    /// Full detail for a single run: run row + jobs + sh events.
    /// Returns `Err(QueryReturnedNoRows)` if no run matches `(run_id, repo)`.
    pub fn get_run_detail(&self, repo: &str, run_id: &str) -> super::Result<RunDetail> {
        let run = self.0.query_row(
            "SELECT id, outcome, sha, ref_name, created_at, dispatched_at, resolved_at
             FROM runs WHERE id = ?1 AND repo = ?2",
            params![run_id, repo],
            |row| {
                Ok(RunRow {
                    id: row.get(0)?,
                    outcome: row.get(1)?,
                    sha: row.get(2)?,
                    ref_name: row.get(3)?,
                    created_at: row.get(4)?,
                    dispatched_at: row.get(5)?,
                    resolved_at: row.get(6)?,
                })
            },
        )?;

        let jobs = self
            .0
            .prepare(
                "SELECT job_id, state, exit_code, started_at_ms, finished_at_ms
             FROM jobs WHERE run_id = ?1
             ORDER BY started_at_ms IS NULL ASC, started_at_ms ASC, job_id ASC",
            )?
            .query_map(params![run_id], |row| {
                Ok(JobRow {
                    job_id: row.get(0)?,
                    state: row.get(1)?,
                    exit_code: row.get(2)?,
                    started_at_ms: row.get(3)?,
                    finished_at_ms: row.get(4)?,
                })
            })?
            .collect::<rusqlite::Result<Vec<_>>>()
            .map_err(super::DbError::from)?;

        let sh_events = self
            .0
            .prepare(
                "SELECT job_id, started_at_ms, finished_at_ms, exit_code, cmd
             FROM sh WHERE run_id = ?1
             ORDER BY job_id, started_at_ms",
            )?
            .query_map(params![run_id], |row| {
                Ok(ShEvent {
                    job_id: row.get(0)?,
                    started_at_ms: row.get(1)?,
                    finished_at_ms: row.get(2)?,
                    exit_code: row.get(3)?,
                    cmd: row.get(4)?,
                })
            })?
            .collect::<rusqlite::Result<Vec<_>>>()
            .map_err(super::DbError::from)?;

        Ok(RunDetail {
            run,
            jobs,
            sh_events,
        })
    }

    // ── Updates ──────────────────────────────────────────────────────────────

    /// Transition a run to dispatched (active). Stamps `dispatched_at` if not already set.
    pub fn set_run_dispatched(&self, id: &str, now_ms: i64) -> super::Result<()> {
        self.0.execute(
            "UPDATE runs SET dispatched_at = COALESCE(dispatched_at, ?1) WHERE id = ?2",
            params![now_ms, id],
        )?;
        Ok(())
    }

    /// Cancel an active run (dispatched but not resolved): sets `resolved_at`, and `outcome = 'superseded'`.
    pub fn cancel_active_run(&self, id: &str, now_ms: i64) -> super::Result<()> {
        self.0.execute(
            "UPDATE runs SET \
                dispatched_at = COALESCE(dispatched_at, ?1), \
                resolved_at = COALESCE(resolved_at, ?2), \
                outcome = COALESCE(outcome, 'superseded') \
             WHERE id = ?3",
            params![now_ms, now_ms, id],
        )?;
        Ok(())
    }

    /// Cancel all queued (not yet dispatched) runs for `(repo, ref_name)`.
    /// Returns the number of rows updated.
    pub fn cancel_queued_runs_for_ref(
        &self,
        repo: &str,
        ref_name: &str,
        now_ms: i64,
    ) -> super::Result<usize> {
        self.0
            .execute(
                "UPDATE runs SET \
                resolved_at = COALESCE(resolved_at, ?1), \
                outcome = COALESCE(outcome, 'superseded') \
             WHERE repo = ?2 AND ref_name = ?3
               AND dispatched_at IS NULL AND resolved_at IS NULL",
                params![now_ms, repo, ref_name],
            )
            .map_err(Into::into)
    }

    /// Resolve a run with an outcome. Stamps `dispatched_at` and `resolved_at` if not set.
    pub fn resolve_run(&self, id: &str, now_ms: i64, outcome: &str) -> super::Result<()> {
        self.0.execute(
            "UPDATE runs SET \
                dispatched_at = COALESCE(dispatched_at, ?1), \
                resolved_at = COALESCE(resolved_at, ?2), \
                outcome = COALESCE(outcome, ?3) \
             WHERE id = ?4",
            params![now_ms, now_ms, outcome, id],
        )?;
        Ok(())
    }

    /// Persist `git_dir` and `traceparent` on a run for API bootstrap.
    pub fn set_run_bootstrap_data(
        &self,
        id: &str,
        git_dir: &str,
        traceparent: Option<&str>,
    ) -> super::Result<()> {
        self.0.execute(
            "UPDATE runs SET git_dir = ?1, traceparent = ?2 WHERE id = ?3",
            params![git_dir, traceparent, id],
        )?;
        Ok(())
    }

    /// Move every unresolved run to `failed-orphaned`. Returns the number of rows updated.
    pub fn fail_orphaned_runs(&self, now_ms: i64) -> super::Result<usize> {
        self.0
            .execute(
                "UPDATE runs SET \
                dispatched_at = COALESCE(dispatched_at, ?1), \
                resolved_at = COALESCE(resolved_at, ?2), \
                outcome = COALESCE(outcome, 'failed-orphaned') \
             WHERE resolved_at IS NULL",
                params![now_ms, now_ms],
            )
            .map_err(Into::into)
    }

    /// Total number of run rows (used for logging after seeding).
    pub fn count_runs(&self) -> super::Result<i64> {
        self.0
            .query_row("SELECT COUNT(*) FROM runs", [], |row| row.get(0))
            .map_err(Into::into)
    }

    /// Insert a run with all fields pre-set (for dev seeding and test fixtures).
    pub fn insert_seeded_run(&self, r: &SeededRun<'_>) -> super::Result<()> {
        self.0.execute(
            "INSERT INTO runs (id, repo, ref_name, sha, pushed_at_ms,
                               created_at, dispatched_at, resolved_at, outcome)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
            params![
                r.id,
                r.repo,
                r.ref_name,
                r.sha,
                r.pushed_at_ms,
                r.created_at,
                r.dispatched_at,
                r.resolved_at,
                r.outcome,
            ],
        )?;
        Ok(())
    }

    // ── Test helpers ──────────────────────────────────────────────────────────

    #[cfg(test)]
    pub fn get_run_token(&self, id: &str) -> super::Result<Option<String>> {
        self.0
            .query_row(
                "SELECT run_token FROM runs WHERE id = ?1",
                params![id],
                |row| row.get(0),
            )
            .map_err(Into::into)
    }

    #[cfg(test)]
    pub fn get_run_outcome_by_sha(&self, sha: &str) -> super::Result<Option<String>> {
        self.0
            .query_row(
                "SELECT outcome FROM runs WHERE sha = ?1",
                params![sha],
                |row| row.get(0),
            )
            .map_err(Into::into)
    }

    #[cfg(test)]
    pub fn count_unresolved_runs(&self) -> super::Result<i64> {
        self.0
            .query_row(
                "SELECT COUNT(*) FROM runs WHERE resolved_at IS NULL",
                [],
                |row| row.get(0),
            )
            .map_err(Into::into)
    }
}
