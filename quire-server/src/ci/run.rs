//! SQLite-backed storage for CI runs.
//!
//! A run is a row in the `runs` table identified by UUID. State
//! transitions are single `UPDATE` statements inside a transaction.
//! Run directories on disk hold the materialized workspace and per-job
//! log files, but state lives in the database.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use jiff::Timestamp;
use quire_core::ci::transport::ApiSession;
use quire_core::secret::SecretString;
use rand::{Rng, distr::Alphanumeric};

use super::error::{Error, Result};

pub use quire_core::ci::run::RunMeta;

/// How a run dispatches its pipeline.
///
/// `Process` shells out to the `quire-ci` binary, which compiles and
/// runs the pipeline in a separate process. The enum is kept open
/// so a future `Docker` executor can be added without another config
/// migration.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, serde::Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Executor {
    #[default]
    Process,
}

/// Transport for CI ↔ server communication.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, serde::Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum TransportMode {
    #[default]
    Filesystem,
    Api,
}

/// Runtime transport for a single CI run. Built once per run from
/// the config-shape [`TransportMode`] + the server's listen port,
/// then passed to `Runs::create` and `Run::execute_via_quire_ci`.
/// The `Api` variant carries the shared [`ApiSession`] — quire-ci
/// receives a structurally identical value via its CLI flags.
#[derive(Clone, Debug, Default)]
pub enum Transport {
    #[default]
    Filesystem,
    Api(ApiSession),
}

impl Transport {
    /// Build a runtime transport for a new run. For `Api`, mints a
    /// fresh run ID and CSPRNG bearer token, derives the loopback
    /// server URL from `port`, and bundles them into an [`ApiSession`].
    pub fn for_new_run(mode: TransportMode, port: u16) -> Self {
        match mode {
            TransportMode::Filesystem => Transport::Filesystem,
            TransportMode::Api => Transport::Api(ApiSession {
                run_id: uuid::Uuid::now_v7().to_string(),
                server_url: format!("http://127.0.0.1:{port}"),
                auth_token: mint_auth_token(),
            }),
        }
    }
}

/// The state of a CI run.
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
}

impl std::str::FromStr for RunState {
    type Err = ();

    fn from_str(s: &str) -> std::result::Result<Self, Self::Err> {
        match s {
            "pending" => Some(RunState::Pending),
            "active" => Some(RunState::Active),
            "complete" => Some(RunState::Complete),
            "failed" => Some(RunState::Failed),
            "superseded" => Some(RunState::Superseded),
            _ => None,
        }
        .ok_or(())
    }
}

/// Access to CI runs for a single repo.
///
/// Owns the database path, repo name, and base directory for run
/// artifacts (workspace, logs). Each method opens a connection via
/// [`crate::db::open`]. Obtain one via `Ci::runs()`.
#[derive(Debug)]
pub struct Runs {
    db_path: PathBuf,
    repo: String,
    base_dir: PathBuf,
}

impl Runs {
    pub fn new(db_path: PathBuf, repo: String, base_dir: PathBuf) -> Self {
        Self {
            db_path,
            repo,
            base_dir,
        }
    }

    /// Create a new run record in the `pending` state.
    ///
    /// Before inserting, supersedes any existing `pending` or `active`
    /// run for the same `(repo, ref)`. Pending runs are marked
    /// superseded directly; active runs have their container killed
    /// first, then marked superseded.
    ///
    /// Inserts a row into `runs` and creates the run directory for
    /// workspace materialization and log storage.
    ///
    /// `transport` is built by the caller via [`Transport::for_new_run`];
    /// for `Api`, the run's id comes from the `ApiSession` (so quire-ci
    /// and the DB agree on which run a bearer token belongs to) and
    /// the token is persisted in `runs.auth_token`. For `Filesystem`,
    /// a fresh UUID is minted here.
    pub fn create(&self, meta: &RunMeta, transport: &Transport) -> Result<Run> {
        let (id, auth_token_str) = match transport {
            Transport::Filesystem => (uuid::Uuid::now_v7().to_string(), None),
            Transport::Api(api) => (api.run_id.clone(), Some(api.auth_token.as_str())),
        };
        let workspace_path = self.base_dir.join(&id).join("workspace");

        let db = crate::db::open(&self.db_path)?;

        // Supersede any existing pending or active run for (repo, ref).
        // Do this before inserting the new run so the new run is never
        // caught by its own supersede query.
        self.supersede_existing(&db, &meta.r#ref)?;

        db.execute(
            "INSERT INTO runs (id, repo, ref_name, sha, pushed_at_ms, state, queued_at_ms, workspace_path, auth_token)
             VALUES (?1, ?2, ?3, ?4, ?5, 'pending', ?6, ?7, ?8)",
            rusqlite::params![
                &id,
                &self.repo,
                &meta.r#ref,
                &meta.sha,
                meta.pushed_at.as_millisecond(),
                Timestamp::now().as_millisecond(),
                workspace_path.to_str().ok_or_else(|| std::io::Error::new(
                    std::io::ErrorKind::InvalidData,
                    "workspace path is not valid UTF-8",
                ))?,
                auth_token_str,
            ],
        )?;

        // Create run directory for workspace and logs.
        fs_err::create_dir_all(&workspace_path)?;

        Ok(Run {
            db_path: self.db_path.clone(),
            id,
            state: RunState::Pending,
            base_dir: self.base_dir.clone(),
        })
    }

    /// Supersede any existing `pending` or `active` run for
    /// `(repo, ref)`.
    ///
    /// Pending runs are transitioned directly to `superseded`. Active
    /// runs have their container killed via `docker kill` before
    /// transition. Different refs are unaffected.
    fn supersede_existing(&self, db: &rusqlite::Connection, ref_name: &str) -> Result<()> {
        let now = Timestamp::now().as_millisecond();

        // Handle active runs first: kill the container, then mark superseded.
        let active_rows: Vec<(String, Option<String>)> = db
            .prepare(
                "SELECT id, container_id FROM runs
                 WHERE repo = ?1 AND ref_name = ?2 AND state = 'active'",
            )?
            .query_map(rusqlite::params![&self.repo, ref_name], |row| {
                Ok((row.get(0)?, row.get(1)?))
            })?
            .collect::<std::result::Result<_, _>>()?;

        for (run_id, container_id) in &active_rows {
            if let Some(cid) = container_id {
                tracing::info!(run_id = %run_id, container_id = %cid, "killing superseded container");
                let kill_status = std::process::Command::new("docker")
                    .args(["kill", cid])
                    .status();
                if let Err(e) = kill_status {
                    tracing::warn!(run_id = %run_id, error = %e, "docker kill failed");
                }
            }
            db.execute(
                "UPDATE runs SET state = 'superseded', finished_at_ms = ?1, container_id = NULL
                 WHERE id = ?2",
                rusqlite::params![now, run_id],
            )?;
            tracing::info!(run_id = %run_id, "superseded active run");
        }

        // Handle pending runs: just mark superseded.
        let pending_count = db.execute(
            "UPDATE runs SET state = 'superseded', finished_at_ms = ?1
             WHERE repo = ?2 AND ref_name = ?3 AND state = 'pending'",
            rusqlite::params![now, &self.repo, ref_name],
        )?;
        if pending_count > 0 {
            tracing::info!(count = pending_count, "superseded pending run(s)");
        }

        Ok(())
    }
}

/// Move every `pending` or `active` run to `failed` with
/// `failure_kind = 'orphaned'`. Called once at server startup to clean
/// up runs left behind by a prior instance. Operates across all repos —
/// orphans aren't a per-repo concern.
pub fn reconcile_orphans(db_path: &Path) -> Result<()> {
    let now = Timestamp::now().as_millisecond();
    let db = crate::db::open(db_path)?;
    let count = db.execute(
        "UPDATE runs SET state = 'failed', finished_at_ms = ?1,
         container_id = NULL, failure_kind = 'orphaned'
         WHERE state IN ('pending', 'active')",
        rusqlite::params![now],
    )?;
    if count > 0 {
        tracing::warn!(count, "reconciled orphaned runs");
    }
    Ok(())
}

/// A CI run backed by a SQLite row.
///
/// Owns the path to the database and the run's in-memory state cache.
/// Reads and writes go through SQL. The run directory on disk holds
/// the workspace and per-job log files.
pub struct Run {
    db_path: PathBuf,
    id: String,
    state: RunState,
    base_dir: PathBuf,
}

impl Run {
    /// The resolved path to this run's directory on disk.
    pub fn path(&self) -> PathBuf {
        self.base_dir.join(&self.id)
    }

    /// The run's ID.
    pub fn id(&self) -> &str {
        &self.id
    }

    /// The run's current state.
    pub fn state(&self) -> RunState {
        self.state
    }

    /// Open an existing run from the database by ID.
    pub fn open(db_path: PathBuf, id: String, base_dir: PathBuf) -> Result<Self> {
        let db = crate::db::open(&db_path)?;
        let state_str: String = db.query_row(
            "SELECT state FROM runs WHERE id = ?1",
            rusqlite::params![&id],
            |row| row.get(0),
        )?;
        let state: RunState = state_str.parse().map_err(|_| {
            std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                format!("invalid state in db: {state_str}"),
            )
        })?;
        Ok(Self {
            db_path,
            id,
            state,
            base_dir,
        })
    }

    /// Run the pipeline by shelling out to the `quire-ci` binary.
    ///
    /// Layout under the run dir on disk:
    /// * `quire-ci.log` — combined stdout+stderr of the subprocess.
    /// * `events.jsonl` — structured event stream (one JSON object per
    ///   line). Ingested into `jobs` and `sh_events` after the
    ///   subprocess exits.
    /// * `jobs/<job>/sh-<n>.log` — per-sh CRI logs, written by quire-ci
    ///   via `--out-dir`.
    ///
    /// When `transport` is `Api`, passes `--run-id`, `--server-url`,
    /// and `QUIRE_CI_TOKEN` to the subprocess instead of the
    /// filesystem-based flags.
    ///
    /// Run finishes `Complete` on exit 0, `Failed` otherwise. The DB
    /// rows are written even on failure so the web UI can render
    /// partial progress.
    pub fn execute_via_quire_ci(
        mut self,
        git_dir: &Path,
        workspace: &Path,
        meta: &RunMeta,
        secrets: &HashMap<String, SecretString>,
        sentry: Option<&quire_core::ci::dispatch::SentryHandoff>,
        transport: &Transport,
    ) -> Result<()> {
        self.transition(RunState::Active, None)?;

        let run_dir = self.path();
        let log_path = run_dir.join("quire-ci.log");
        let events_path = run_dir.join("events.jsonl");
        let dispatch_path = run_dir.join("dispatch.json");
        // fs_err for the path-bearing IO error; unwrap to std::fs::File so
        // it's convertible into Stdio.
        let log = fs_err::File::create(&log_path)?.into_parts().0;
        let log_clone = log.try_clone()?;

        write_dispatch(&dispatch_path, git_dir, meta, secrets, sentry)?;

        tracing::info!(
            run_id = %self.id,
            log = %log_path.display(),
            events = %events_path.display(),
            "dispatching run to quire-ci",
        );

        let mut cmd = std::process::Command::new("quire-ci");
        cmd.arg("run").arg("--workspace").arg(workspace);

        match transport {
            Transport::Filesystem => {
                cmd.arg("--out-dir")
                    .arg(&run_dir)
                    .arg("--events")
                    .arg(&events_path)
                    .arg("--dispatch")
                    .arg(&dispatch_path);
            }
            Transport::Api(api) => {
                cmd.arg("--run-id")
                    .arg(&self.id)
                    .arg("--server-url")
                    .arg(&api.server_url);
                cmd.env("QUIRE_CI_TOKEN", &api.auth_token);
            }
        }

        let status = cmd
            .stdout(std::process::Stdio::from(log))
            .stderr(std::process::Stdio::from(log_clone))
            .status()
            .map_err(|source| Error::CommandSpawnFailed {
                program: "quire-ci".to_string(),
                cwd: workspace.to_path_buf(),
                source,
            })?;

        // quire-ci unlinks the dispatch file after `load_dispatch`;
        // this is a best-effort safety net for paths where it didn't
        // get that far (spawn failed mid-exec, arg parsing rejected
        // input, panic before read). `NotFound` is the expected case.
        if let Err(e) = fs_err::remove_file(&dispatch_path)
            && e.kind() != std::io::ErrorKind::NotFound
        {
            tracing::warn!(
                run_id = %self.id,
                path = %dispatch_path.display(),
                error = %e,
                "failed to remove dispatch file after run"
            );
        }

        // Ingest events before checking outcome — partial results from a
        // crashed run are still useful in the UI. A parse failure is
        // logged but doesn't mask the run outcome.
        let run_outcome = match self.ingest_events(&events_path) {
            Ok(outcome) => outcome,
            Err(e) => {
                tracing::warn!(
                    run_id = %self.id,
                    error = %e,
                    "failed to ingest quire-ci events; jobs/sh_events rows may be incomplete"
                );
                None
            }
        };

        // RunFinished is the sole signal for outcome: present means quire-ci
        // reached the end cleanly; absent means it crashed or was killed.
        // The exit code is kept in the error for diagnostics only.
        match run_outcome {
            Some(quire_core::ci::event::RunOutcome::Success) => {
                self.transition(RunState::Complete, None)?;
            }
            Some(quire_core::ci::event::RunOutcome::PipelineFailure) => {
                self.transition(RunState::Failed, Some("pipeline-failure"))?;
            }
            None => {
                self.transition(RunState::Failed, Some("process-crashed"))?;
                return Err(Error::ProcessFailed {
                    exit: status.code(),
                });
            }
        }
        Ok(())
    }

    /// Read `events.jsonl` and replay it into the database.
    ///
    /// Done in two passes because `sh_events` has a foreign key on
    /// `(run_id, job_id)` in `jobs`, and the wire format interleaves
    /// sh events with their owning job. Pass 1 inserts every job row
    /// (paired by `job_id`); pass 2 inserts sh events.
    fn ingest_events(&self, path: &Path) -> Result<Option<quire_core::ci::event::RunOutcome>> {
        use quire_core::ci::event::{Event, EventKind, JobOutcome, RunOutcome};

        let bytes = match fs_err::read(path) {
            Ok(b) => b,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(e) => return Err(e.into()),
        };
        let events: Vec<Event> = bytes
            .split(|b| *b == b'\n')
            .filter(|line| !line.is_empty())
            .map(serde_json::from_slice)
            .collect::<std::result::Result<_, _>>()
            .map_err(|e| Error::EventStreamParse {
                path: path.to_path_buf(),
                source: e,
            })?;

        let db = crate::db::open(&self.db_path)?;

        // Pass 1: jobs rows. Pair JobStarted with JobFinished by job_id.
        let mut pending_jobs: HashMap<&str, i64> = HashMap::new();
        let mut run_outcome: Option<RunOutcome> = None;
        for event in &events {
            match &event.kind {
                EventKind::JobStarted { job_id } => {
                    pending_jobs.insert(job_id.as_str(), event.at_ms);
                }
                EventKind::JobFinished { job_id, outcome } => {
                    let started_at = pending_jobs.remove(job_id.as_str()).unwrap_or(event.at_ms);
                    let state = match outcome {
                        JobOutcome::Complete => "complete",
                        JobOutcome::Failed => "failed",
                    };
                    db.execute(
                        "INSERT INTO jobs (run_id, job_id, state, started_at_ms, finished_at_ms) \
                         VALUES (?1, ?2, ?3, ?4, ?5)",
                        rusqlite::params![&self.id, job_id, state, started_at, event.at_ms],
                    )?;
                }
                EventKind::RunFinished { outcome } => {
                    run_outcome = Some(*outcome);
                }
                EventKind::ShStarted { .. } | EventKind::ShFinished { .. } => {}
            }
        }

        // Pass 2: sh_events rows. Pair ShStarted with ShFinished by job_id
        // (sequential within a run-fn, so a single buffer slot per job
        // is enough).
        let mut pending_sh: HashMap<&str, (i64, &str)> = HashMap::new();
        for event in &events {
            match &event.kind {
                EventKind::ShStarted { job_id, cmd } => {
                    pending_sh.insert(job_id.as_str(), (event.at_ms, cmd.as_str()));
                }
                EventKind::ShFinished { job_id, exit_code } => {
                    let Some((started_at, cmd)) = pending_sh.remove(job_id.as_str()) else {
                        continue;
                    };
                    db.execute(
                        "INSERT INTO sh_events (run_id, job_id, started_at_ms, finished_at_ms, exit_code, cmd) \
                         VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
                        rusqlite::params![&self.id, job_id, started_at, event.at_ms, exit_code, cmd],
                    )?;
                }
                EventKind::JobStarted { .. }
                | EventKind::JobFinished { .. }
                | EventKind::RunFinished { .. } => {}
            }
        }

        Ok(run_outcome)
    }

    /// Transition the run from its current state to a new state.
    ///
    /// Allowed edges (see `docs/CI-STATE.md`):
    ///
    /// * `Pending → Active`
    /// * `Pending → Complete`
    /// * `Pending → Superseded`
    /// * `Active  → Complete`
    /// * `Active  → Failed`
    /// * `Active  → Superseded`
    ///
    /// `failure_kind` is recorded only when transitioning to
    /// `Failed`; it is ignored for other targets. Pass a short tag
    /// (`"quire-ci-exit"`) so the UI can distinguish job-pipeline
    /// failures from `reconcile_orphans`'s `"orphaned"`. Each
    /// timestamp and `failure_kind` is set at most once (via
    /// `COALESCE`).
    pub fn transition(&mut self, to: RunState, failure_kind: Option<&str>) -> Result<()> {
        use RunState::*;
        let allowed = matches!(
            (self.state, to),
            (Pending, Active)
                | (Pending, Complete)
                | (Pending, Superseded)
                | (Active, Complete)
                | (Active, Failed)
                | (Active, Superseded)
        );
        if !allowed {
            return Err(Error::InvalidTransition {
                from: self.state,
                to,
            });
        }

        let now = Timestamp::now().as_millisecond();
        let db = crate::db::open(&self.db_path)?;

        match to {
            Active => {
                db.execute(
                    "UPDATE runs SET state = 'active', started_at_ms = COALESCE(started_at_ms, ?1)
                     WHERE id = ?2",
                    rusqlite::params![now, &self.id],
                )?;
            }
            Complete | Superseded => {
                db.execute(
                    "UPDATE runs SET state = ?1, \
                        started_at_ms = COALESCE(started_at_ms, ?2), \
                        finished_at_ms = COALESCE(finished_at_ms, ?3), \
                        container_id = NULL \
                     WHERE id = ?4",
                    rusqlite::params![to.as_str(), now, now, &self.id],
                )?;
            }
            Failed => {
                db.execute(
                    "UPDATE runs SET state = 'failed', \
                        started_at_ms = COALESCE(started_at_ms, ?1), \
                        finished_at_ms = COALESCE(finished_at_ms, ?2), \
                        container_id = NULL, \
                        failure_kind = COALESCE(failure_kind, ?3) \
                     WHERE id = ?4",
                    rusqlite::params![now, now, failure_kind, &self.id],
                )?;
            }
            Pending => unreachable!("transition to Pending is not valid"),
        }

        self.state = to;
        Ok(())
    }

    /// Read the immutable metadata for this run.
    pub fn read_meta(&self) -> Result<RunMeta> {
        let db = crate::db::open(&self.db_path)?;
        let (sha, ref_name, pushed_at_ms) = db.query_row(
            "SELECT sha, ref_name, pushed_at_ms FROM runs WHERE id = ?1",
            rusqlite::params![&self.id],
            |row| {
                let sha: String = row.get(0)?;
                let ref_name: String = row.get(1)?;
                let pushed_at_ms: i64 = row.get(2)?;
                Ok((sha, ref_name, pushed_at_ms))
            },
        )?;
        Ok(RunMeta {
            sha,
            r#ref: ref_name,
            pushed_at: Timestamp::from_millisecond(pushed_at_ms)
                .expect("db stores valid timestamps"),
        })
    }

    /// Read the `started_at` timestamp for this run, if set.
    pub fn read_started_at(&self) -> Result<Option<Timestamp>> {
        let db = crate::db::open(&self.db_path)?;
        let ms: Option<i64> = db.query_row(
            "SELECT started_at_ms FROM runs WHERE id = ?1",
            rusqlite::params![&self.id],
            |row| row.get(0),
        )?;
        Ok(ms.map(|m| Timestamp::from_millisecond(m).expect("valid timestamp")))
    }

    /// Read the `finished_at` timestamp for this run, if set.
    pub fn read_finished_at(&self) -> Result<Option<Timestamp>> {
        let db = crate::db::open(&self.db_path)?;
        let ms: Option<i64> = db.query_row(
            "SELECT finished_at_ms FROM runs WHERE id = ?1",
            rusqlite::params![&self.id],
            |row| row.get(0),
        )?;
        Ok(ms.map(|m| Timestamp::from_millisecond(m).expect("valid timestamp")))
    }
}

/// Serialize the dispatch payload as JSON and write it to `path` with
/// owner-only permissions on Unix. Secrets cross as plaintext so the
/// 0600 mode is the line of defense against other local users; failure
/// to set the mode aborts the dispatch (better than leaking).
fn write_dispatch(
    path: &Path,
    git_dir: &Path,
    meta: &RunMeta,
    secrets: &HashMap<String, SecretString>,
    sentry: Option<&quire_core::ci::dispatch::SentryHandoff>,
) -> Result<()> {
    use quire_core::ci::dispatch::{Dispatch, SentryHandoff};

    let mut revealed: HashMap<String, String> = HashMap::with_capacity(secrets.len());
    for (name, value) in secrets {
        revealed.insert(
            name.clone(),
            value.reveal().map_err(Error::Secret)?.to_string(),
        );
    }
    let dispatch = Dispatch {
        meta: meta.clone(),
        git_dir: git_dir.to_path_buf(),
        secrets: revealed,
        sentry: sentry.map(|s| SentryHandoff {
            dsn: s.dsn.clone(),
            trace_id: s.trace_id.clone(),
        }),
    };
    let json = serde_json::to_vec_pretty(&dispatch).map_err(std::io::Error::other)?;

    // Open with mode 0600 from the start so there's no window where
    // the file is world-readable.
    use fs_err::os::unix::fs::OpenOptionsExt;
    use std::io::Write;
    let mut file = fs_err::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(0o600)
        .open(path)?;
    file.write_all(&json)?;
    Ok(())
}

/// Take the final path component of a runs base (`runs/<repo>/`) and
/// sanitize it for use as the tag segment in `quire-ci/<segment>:<id>`.
/// Materialize a working tree at `sha` into `workspace` via
/// `git archive | tar -x`. Creates the workspace dir if needed.
pub fn materialize_workspace(git_dir: &Path, sha: &str, workspace: &Path) -> Result<()> {
    use std::process::{Command, Stdio};

    fs_err::create_dir_all(workspace)?;

    let mut archive = Command::new("git")
        .arg("--git-dir")
        .arg(git_dir)
        .args(["archive", sha])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()?;
    let archive_stdout = archive.stdout.take().expect("piped stdout");

    let tar = Command::new("tar")
        .args(["-x", "-C"])
        .arg(workspace)
        .stdin(Stdio::from(archive_stdout))
        .stderr(Stdio::piped())
        .spawn()?;

    let tar_output = tar.wait_with_output()?;
    let archive_output = archive.wait_with_output()?;
    if !archive_output.status.success() || !tar_output.status.success() {
        return Err(Error::WorkspaceMaterializationFailed {
            source: std::io::Error::other(format!(
                "git archive exited {}, tar exited {}",
                archive_output.status, tar_output.status
            )),
        });
    }
    Ok(())
}

/// Mint a 32-character alphanumeric bearer token from the OS CSPRNG.
///
/// ~190 bits of entropy, opaque to the holder. Used as the per-run
/// auth secret for the API transport; stored in the `runs.auth_token`
/// column and passed to quire-ci via `QUIRE_CI_TOKEN`.
fn mint_auth_token() -> String {
    rand::rng()
        .sample_iter(&Alphanumeric)
        .take(32)
        .map(char::from)
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Quire;

    fn tmp_quire() -> (tempfile::TempDir, Quire) {
        let dir = tempfile::tempdir().expect("tempdir");
        let quire = Quire::new(dir.path().to_path_buf());
        // Initialize the database.
        let mut db = crate::db::open(&quire.db_path()).expect("init db");
        crate::db::migrate(&mut db).expect("migrate db");
        drop(db);
        (dir, quire)
    }

    fn test_runs(quire: &Quire) -> Runs {
        let base_dir = quire.base_dir().join("runs").join("test.git");
        Runs::new(quire.db_path(), "test.git".to_string(), base_dir)
    }

    fn test_meta() -> RunMeta {
        RunMeta {
            sha: "abc123".to_string(),
            r#ref: "refs/heads/main".to_string(),
            pushed_at: "2026-04-28T12:00:00Z".parse().expect("parse timestamp"),
        }
    }

    #[test]
    fn materialize_workspace_extracts_archive_at_sha() {
        let dir = tempfile::tempdir().expect("tempdir");
        let src_repo = dir.path().join("src");
        fs_err::create_dir_all(&src_repo).expect("mkdir src");

        let env_vars: [(&str, &str); 6] = [
            ("GIT_AUTHOR_NAME", "test"),
            ("GIT_AUTHOR_EMAIL", "test@test"),
            ("GIT_COMMITTER_NAME", "test"),
            ("GIT_COMMITTER_EMAIL", "test@test"),
            ("GIT_CONFIG_GLOBAL", "/dev/null"),
            ("GIT_CONFIG_SYSTEM", "/dev/null"),
        ];

        let output = std::process::Command::new("git")
            .args(["init", "-b", "main"])
            .current_dir(&src_repo)
            .envs(env_vars)
            .output()
            .expect("git init");
        assert!(output.status.success());

        let output = std::process::Command::new("git")
            .args(["commit", "--allow-empty", "-m", "initial"])
            .current_dir(&src_repo)
            .envs(env_vars)
            .output()
            .expect("git commit initial");
        assert!(output.status.success());

        fs_err::write(src_repo.join("hello.txt"), "hi\n").expect("write hello.txt");

        let output = std::process::Command::new("git")
            .args(["add", "."])
            .current_dir(&src_repo)
            .envs(env_vars)
            .output()
            .expect("git add");
        assert!(output.status.success());

        let output = std::process::Command::new("git")
            .args(["commit", "-m", "add file"])
            .current_dir(&src_repo)
            .envs(env_vars)
            .output()
            .expect("git commit");
        assert!(output.status.success());

        let sha_output = std::process::Command::new("git")
            .args(["rev-parse", "HEAD"])
            .current_dir(&src_repo)
            .envs(env_vars)
            .output()
            .expect("git rev-parse");
        assert!(sha_output.status.success());
        let sha = String::from_utf8(sha_output.stdout)
            .expect("utf8")
            .trim()
            .to_string();

        let workspace = dir.path().join("ws");
        materialize_workspace(&src_repo.join(".git"), &sha, &workspace).expect("materialize");
        assert_eq!(
            fs_err::read_to_string(workspace.join("hello.txt")).unwrap(),
            "hi\n"
        );
    }

    #[test]
    fn materialize_workspace_errors_on_unknown_sha() {
        let dir = tempfile::tempdir().expect("tempdir");
        let src_repo = dir.path().join("src");
        fs_err::create_dir_all(&src_repo).expect("mkdir src");

        let env_vars: [(&str, &str); 6] = [
            ("GIT_AUTHOR_NAME", "test"),
            ("GIT_AUTHOR_EMAIL", "test@test"),
            ("GIT_COMMITTER_NAME", "test"),
            ("GIT_COMMITTER_EMAIL", "test@test"),
            ("GIT_CONFIG_GLOBAL", "/dev/null"),
            ("GIT_CONFIG_SYSTEM", "/dev/null"),
        ];
        let out = std::process::Command::new("git")
            .args(["init", "-b", "main"])
            .current_dir(&src_repo)
            .envs(env_vars)
            .output()
            .expect("git init");
        assert!(out.status.success());

        let workspace = dir.path().join("ws");
        let err = materialize_workspace(
            &src_repo.join(".git"),
            "deadbeefdeadbeefdeadbeefdeadbeefdeadbeef",
            &workspace,
        )
        .expect_err("expected failure on unknown SHA");
        assert!(
            matches!(err, Error::WorkspaceMaterializationFailed { .. }),
            "expected WorkspaceMaterializationFailed, got: {err:?}"
        );
    }

    #[test]
    fn write_dispatch_records_git_dir_for_quire_ci() {
        use quire_core::ci::dispatch::Dispatch;

        let dir = tempfile::tempdir().expect("tempdir");
        let dispatch_path = dir.path().join("dispatch.json");
        let git_dir = dir.path().join("repos").join("test.git");

        write_dispatch(
            &dispatch_path,
            &git_dir,
            &test_meta(),
            &HashMap::new(),
            None,
        )
        .expect("write_dispatch");

        let bytes = fs_err::read(&dispatch_path).expect("read dispatch");
        let dispatch: Dispatch = serde_json::from_slice(&bytes).expect("parse dispatch");
        assert_eq!(
            dispatch.git_dir, git_dir,
            "quire-ci needs the bare repo path to set GIT_DIR for the mirror job"
        );
    }

    #[test]
    fn run_state_round_trips() {
        for state in [
            RunState::Pending,
            RunState::Active,
            RunState::Complete,
            RunState::Failed,
            RunState::Superseded,
        ] {
            assert!(state.as_str().parse::<RunState>().is_ok());
        }
        assert!("unknown".parse::<RunState>().is_err());
    }

    #[test]
    fn create_generates_uuidv7_id() {
        let (_dir, quire) = tmp_quire();
        let runs = test_runs(&quire);
        let run = runs
            .create(&test_meta(), &Transport::Filesystem)
            .expect("create");
        let parsed = uuid::Uuid::parse_str(run.id()).expect("should be valid UUID");
        assert_eq!(parsed.get_version(), Some(uuid::Version::SortRand));
    }

    #[test]
    fn create_with_filesystem_leaves_auth_token_null() {
        let (_dir, quire) = tmp_quire();
        let runs = test_runs(&quire);
        let run = runs
            .create(&test_meta(), &Transport::Filesystem)
            .expect("create");

        let conn = crate::db::open(&quire.db_path()).expect("db");
        let token: Option<String> = conn
            .query_row(
                "SELECT auth_token FROM runs WHERE id = ?1",
                rusqlite::params![run.id()],
                |row| row.get(0),
            )
            .expect("row");
        assert!(
            token.is_none(),
            "filesystem transport should not mint a token"
        );
    }

    #[test]
    fn create_with_api_persists_minted_auth_token() {
        let (_dir, quire) = tmp_quire();
        let runs = test_runs(&quire);
        let transport = Transport::for_new_run(TransportMode::Api, 3000);
        let run = runs.create(&test_meta(), &transport).expect("create");

        let Transport::Api(api) = &transport else {
            panic!("expected Api transport");
        };

        // Run id and ApiSession.run_id are the same value — quire-ci
        // and the orchestrator agree on which run a token belongs to.
        assert_eq!(run.id(), api.run_id);

        let conn = crate::db::open(&quire.db_path()).expect("db");
        let stored: Option<String> = conn
            .query_row(
                "SELECT auth_token FROM runs WHERE id = ?1",
                rusqlite::params![run.id()],
                |row| row.get(0),
            )
            .expect("row");
        assert_eq!(stored.as_deref(), Some(api.auth_token.as_str()));
    }

    #[test]
    fn for_new_run_mints_alphanumeric_token() {
        let transport = Transport::for_new_run(TransportMode::Api, 4000);
        let Transport::Api(api) = transport else {
            panic!("expected Api transport");
        };
        assert_eq!(api.server_url, "http://127.0.0.1:4000");
        assert_eq!(api.auth_token.len(), 32);
        assert!(
            api.auth_token.chars().all(|c| c.is_ascii_alphanumeric()),
            "token should be alphanumeric, got {:?}",
            api.auth_token
        );
        assert!(
            uuid::Uuid::parse_str(&api.run_id).is_ok(),
            "run_id should be a UUID, got {:?}",
            api.run_id
        );
    }

    #[test]
    fn mint_auth_token_returns_unique_values() {
        let a = mint_auth_token();
        let b = mint_auth_token();
        assert_ne!(a, b, "two mints should not collide");
    }

    #[test]
    fn create_writes_row_in_pending_state() {
        let (_dir, quire) = tmp_quire();
        let runs = test_runs(&quire);
        let run = runs
            .create(&test_meta(), &Transport::Filesystem)
            .expect("create");

        assert_eq!(run.state(), RunState::Pending);

        // Verify workspace directory was created.
        let workspace = run.path().join("workspace");
        assert!(workspace.exists(), "workspace directory should exist");

        // Verify metadata round-trips through the DB.
        let meta = run.read_meta().expect("read meta");
        assert_eq!(meta.sha, "abc123");

        // No started_at yet.
        let started = run.read_started_at().expect("read started_at");
        assert!(started.is_none());
        let finished = run.read_finished_at().expect("read finished_at");
        assert!(finished.is_none());
    }

    #[test]
    fn transition_updates_state_in_db() {
        let (_dir, quire) = tmp_quire();
        let runs = test_runs(&quire);
        let mut run = runs
            .create(&test_meta(), &Transport::Filesystem)
            .expect("create");
        let id = run.id().to_string();

        run.transition(RunState::Active, None).expect("transition");
        assert_eq!(run.state(), RunState::Active);

        // Verify started_at was stamped.
        let started = run.read_started_at().expect("read started_at");
        assert!(started.is_some(), "started_at should be stamped");

        // Re-open the run and verify state persists.
        let reopened =
            Run::open(quire.db_path(), id.clone(), runs.base_dir.clone()).expect("reopen");
        assert_eq!(reopened.state(), RunState::Active);
        assert_eq!(reopened.id(), id);
    }

    #[test]
    fn transition_stamps_started_at_on_active() {
        let (_dir, quire) = tmp_quire();
        let runs = test_runs(&quire);
        let mut run = runs
            .create(&test_meta(), &Transport::Filesystem)
            .expect("create");

        run.transition(RunState::Active, None).expect("to active");
        let started = run.read_started_at().expect("read started_at");
        assert!(started.is_some(), "started_at should be stamped");
        assert!(run.read_finished_at().expect("read").is_none());
    }

    #[test]
    fn transition_stamps_finished_at_on_complete_and_failed() {
        let (_dir, quire) = tmp_quire();
        let runs = test_runs(&quire);

        let mut completed = runs
            .create(&test_meta(), &Transport::Filesystem)
            .expect("create");
        completed
            .transition(RunState::Active, None)
            .expect("to active");
        completed
            .transition(RunState::Complete, None)
            .expect("to complete");
        assert!(completed.read_finished_at().expect("read").is_some());

        let mut failed = runs
            .create(&test_meta(), &Transport::Filesystem)
            .expect("create");
        failed
            .transition(RunState::Active, None)
            .expect("to active");
        failed
            .transition(RunState::Failed, Some("job-error"))
            .expect("to failed");
        assert!(failed.read_finished_at().expect("read").is_some());
    }

    #[test]
    fn transition_records_failure_kind_on_failed() {
        let (_dir, quire) = tmp_quire();
        let runs = test_runs(&quire);
        let mut run = runs
            .create(&test_meta(), &Transport::Filesystem)
            .expect("create");
        let id = run.id().to_string();

        run.transition(RunState::Active, None).expect("to active");
        run.transition(RunState::Failed, Some("job-error"))
            .expect("to failed");

        let db = crate::db::open(&quire.db_path()).expect("open db");
        let kind: Option<String> = db
            .query_row(
                "SELECT failure_kind FROM runs WHERE id = ?1",
                rusqlite::params![&id],
                |row| row.get(0),
            )
            .expect("query");
        assert_eq!(kind.as_deref(), Some("job-error"));
    }

    #[test]
    fn transition_skips_failure_kind_when_none() {
        let (_dir, quire) = tmp_quire();
        let runs = test_runs(&quire);
        let mut run = runs
            .create(&test_meta(), &Transport::Filesystem)
            .expect("create");
        let id = run.id().to_string();

        run.transition(RunState::Active, None).expect("to active");
        run.transition(RunState::Failed, None).expect("to failed");

        let db = crate::db::open(&quire.db_path()).expect("open db");
        let kind: Option<String> = db
            .query_row(
                "SELECT failure_kind FROM runs WHERE id = ?1",
                rusqlite::params![&id],
                |row| row.get(0),
            )
            .expect("query");
        assert!(kind.is_none());
    }

    #[test]
    fn transition_rejects_invalid_transitions() {
        let (_dir, quire) = tmp_quire();
        let runs = test_runs(&quire);

        // Pending -> Failed is not allowed (must go via Active).
        let mut run = runs
            .create(&test_meta(), &Transport::Filesystem)
            .expect("create");
        assert!(run.transition(RunState::Failed, None).is_err());

        // Terminal -> anything is not allowed.
        let mut completed = runs
            .create(&test_meta(), &Transport::Filesystem)
            .expect("create");
        completed
            .transition(RunState::Active, None)
            .expect("to active");
        completed
            .transition(RunState::Complete, None)
            .expect("to complete");
        assert!(completed.transition(RunState::Active, None).is_err());
        assert!(completed.transition(RunState::Failed, None).is_err());
    }

    #[test]
    fn transition_preserves_started_at_through_completion() {
        let (_dir, quire) = tmp_quire();
        let runs = test_runs(&quire);
        let mut run = runs
            .create(&test_meta(), &Transport::Filesystem)
            .expect("create");

        run.transition(RunState::Active, None).expect("to active");
        let started = run.read_started_at().expect("read started_at");

        run.transition(RunState::Complete, None)
            .expect("to complete");
        assert_eq!(
            run.read_started_at().expect("read"),
            started,
            "started_at preserved"
        );
    }

    #[test]
    fn transition_full_lifecycle() {
        let (_dir, quire) = tmp_quire();
        let runs = test_runs(&quire);
        let mut run = runs
            .create(&test_meta(), &Transport::Filesystem)
            .expect("create");

        run.transition(RunState::Active, None).expect("to active");
        run.transition(RunState::Complete, None)
            .expect("to complete");

        assert_eq!(run.state(), RunState::Complete);
    }

    #[test]
    fn reconcile_fails_pending_orphans() {
        let (_dir, quire) = tmp_quire();
        let runs = test_runs(&quire);
        let run = runs
            .create(&test_meta(), &Transport::Filesystem)
            .expect("create");
        let id = run.id().to_string();

        reconcile_orphans(&quire.db_path()).expect("reconcile");

        let reopened = Run::open(quire.db_path(), id, runs.base_dir.clone()).expect("reopen");
        assert_eq!(reopened.state(), RunState::Failed);
    }

    #[test]
    fn reconcile_fails_active_orphans() {
        let (_dir, quire) = tmp_quire();
        let runs = test_runs(&quire);
        let mut run = runs
            .create(&test_meta(), &Transport::Filesystem)
            .expect("create");
        run.transition(RunState::Active, None).expect("to active");
        let id = run.id().to_string();

        reconcile_orphans(&quire.db_path()).expect("reconcile");

        let reopened = Run::open(quire.db_path(), id, runs.base_dir.clone()).expect("reopen");
        assert_eq!(reopened.state(), RunState::Failed);
    }

    #[test]
    fn reconcile_leaves_complete_runs_alone() {
        let (_dir, quire) = tmp_quire();
        let runs = test_runs(&quire);
        let mut run = runs
            .create(&test_meta(), &Transport::Filesystem)
            .expect("create");
        run.transition(RunState::Active, None).expect("to active");
        run.transition(RunState::Complete, None)
            .expect("to complete");
        let id = run.id().to_string();

        reconcile_orphans(&quire.db_path()).expect("reconcile");

        let reopened = Run::open(quire.db_path(), id, runs.base_dir.clone()).expect("reopen");
        assert_eq!(reopened.state(), RunState::Complete);
    }

    #[test]
    fn reconcile_is_a_noop_when_no_runs() {
        let (_dir, quire) = tmp_quire();
        reconcile_orphans(&quire.db_path()).expect("reconcile");
    }

    #[test]
    fn ingest_events_writes_jobs_and_sh_events_rows() {
        use quire_core::ci::event::{Event, EventKind, JobOutcome, RunOutcome};

        let (_dir, quire) = tmp_quire();
        let runs = test_runs(&quire);
        let run = runs
            .create(&test_meta(), &Transport::Filesystem)
            .expect("create");
        let run_id = run.id().to_string();

        let events = vec![
            Event {
                at_ms: 100,
                kind: EventKind::JobStarted {
                    job_id: "build".into(),
                },
            },
            Event {
                at_ms: 110,
                kind: EventKind::ShStarted {
                    job_id: "build".into(),
                    cmd: "echo hi".into(),
                },
            },
            Event {
                at_ms: 190,
                kind: EventKind::ShFinished {
                    job_id: "build".into(),
                    exit_code: 0,
                },
            },
            Event {
                at_ms: 200,
                kind: EventKind::JobFinished {
                    job_id: "build".into(),
                    outcome: JobOutcome::Complete,
                },
            },
            Event {
                at_ms: 210,
                kind: EventKind::JobStarted {
                    job_id: "test".into(),
                },
            },
            Event {
                at_ms: 220,
                kind: EventKind::JobFinished {
                    job_id: "test".into(),
                    outcome: JobOutcome::Failed,
                },
            },
            Event {
                at_ms: 230,
                kind: EventKind::RunFinished {
                    outcome: RunOutcome::PipelineFailure,
                },
            },
        ];

        let events_path = run.path().join("events.jsonl");
        let mut bytes = Vec::new();
        for ev in &events {
            bytes.extend(serde_json::to_vec(ev).unwrap());
            bytes.push(b'\n');
        }
        fs_err::write(&events_path, bytes).expect("write events.jsonl");

        let outcome = run.ingest_events(&events_path).expect("ingest");
        assert_eq!(outcome, Some(RunOutcome::PipelineFailure));

        let db = crate::db::open(&quire.db_path()).expect("open db");
        let jobs: Vec<(String, String, i64, i64)> = db
            .prepare(
                "SELECT job_id, state, started_at_ms, finished_at_ms FROM jobs \
                 WHERE run_id = ?1 ORDER BY started_at_ms",
            )
            .unwrap()
            .query_map([&run_id], |row| {
                Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?))
            })
            .unwrap()
            .collect::<std::result::Result<_, _>>()
            .unwrap();
        assert_eq!(
            jobs,
            vec![
                ("build".to_string(), "complete".to_string(), 100, 200),
                ("test".to_string(), "failed".to_string(), 210, 220),
            ]
        );

        let sh_events: Vec<(String, i64, i64, i32, String)> = db
            .prepare(
                "SELECT job_id, started_at_ms, finished_at_ms, exit_code, cmd FROM sh_events \
                 WHERE run_id = ?1 ORDER BY started_at_ms",
            )
            .unwrap()
            .query_map([&run_id], |row| {
                Ok((
                    row.get(0)?,
                    row.get(1)?,
                    row.get(2)?,
                    row.get(3)?,
                    row.get(4)?,
                ))
            })
            .unwrap()
            .collect::<std::result::Result<_, _>>()
            .unwrap();
        assert_eq!(
            sh_events,
            vec![("build".to_string(), 110, 190, 0, "echo hi".to_string())]
        );
    }

    #[test]
    fn ingest_events_treats_missing_file_as_empty() {
        let (_dir, quire) = tmp_quire();
        let runs = test_runs(&quire);
        let run = runs
            .create(&test_meta(), &Transport::Filesystem)
            .expect("create");

        let missing = run.path().join("events.jsonl");
        let outcome = run
            .ingest_events(&missing)
            .expect("missing file should not error");
        assert!(outcome.is_none(), "missing file yields no outcome");

        let db = crate::db::open(&quire.db_path()).expect("open db");
        let count: i64 = db
            .query_row(
                "SELECT COUNT(*) FROM jobs WHERE run_id = ?1",
                [run.id()],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(count, 0);
    }

    #[test]
    fn create_supersedes_pending_run_on_same_ref() {
        let (_dir, quire) = tmp_quire();
        let runs = test_runs(&quire);

        // Create first run.
        let run1 = runs
            .create(&test_meta(), &Transport::Filesystem)
            .expect("create run1");
        let run1_id = run1.id().to_string();
        assert_eq!(run1.state(), RunState::Pending);

        // Create second run for same (repo, ref) — should supersede the first.
        let meta2 = RunMeta {
            sha: "def456".to_string(),
            r#ref: "refs/heads/main".to_string(),
            pushed_at: "2026-04-28T13:00:00Z".parse().unwrap(),
        };
        let run2 = runs
            .create(&meta2, &Transport::Filesystem)
            .expect("create run2");
        assert_eq!(run2.state(), RunState::Pending);

        // First run should now be superseded.
        let reopened = Run::open(quire.db_path(), run1_id, runs.base_dir.clone()).expect("reopen");
        assert_eq!(reopened.state(), RunState::Superseded);
        assert!(
            reopened.read_finished_at().expect("read").is_some(),
            "superseded run should have finished_at"
        );
    }

    #[test]
    fn create_supersedes_active_run_on_same_ref() {
        let (_dir, quire) = tmp_quire();
        let runs = test_runs(&quire);

        // Create and activate first run.
        let mut run1 = runs
            .create(&test_meta(), &Transport::Filesystem)
            .expect("create run1");
        let run1_id = run1.id().to_string();
        run1.transition(RunState::Active, None).expect("to active");

        // Create second run for same (repo, ref).
        let meta2 = RunMeta {
            sha: "def456".to_string(),
            r#ref: "refs/heads/main".to_string(),
            pushed_at: "2026-04-28T13:00:00Z".parse().unwrap(),
        };
        let run2 = runs
            .create(&meta2, &Transport::Filesystem)
            .expect("create run2");
        assert_eq!(run2.state(), RunState::Pending);

        // First run should be superseded.
        let reopened = Run::open(quire.db_path(), run1_id, runs.base_dir.clone()).expect("reopen");
        assert_eq!(reopened.state(), RunState::Superseded);
        assert!(
            reopened.read_finished_at().expect("read").is_some(),
            "superseded run should have finished_at"
        );
    }

    #[test]
    fn create_does_not_supersede_different_ref() {
        let (_dir, quire) = tmp_quire();
        let runs = test_runs(&quire);

        // Create run for main.
        let run1 = runs
            .create(&test_meta(), &Transport::Filesystem)
            .expect("create run1");
        let run1_id = run1.id().to_string();

        // Create run for a different ref.
        let meta2 = RunMeta {
            sha: "def456".to_string(),
            r#ref: "refs/heads/feature".to_string(),
            pushed_at: "2026-04-28T13:00:00Z".parse().unwrap(),
        };
        let _run2 = runs
            .create(&meta2, &Transport::Filesystem)
            .expect("create run2");

        // First run should still be pending.
        let reopened = Run::open(quire.db_path(), run1_id, runs.base_dir.clone()).expect("reopen");
        assert_eq!(reopened.state(), RunState::Pending);
    }

    #[test]
    fn create_does_not_supersede_complete_or_failed_runs() {
        let (_dir, quire) = tmp_quire();
        let runs = test_runs(&quire);

        // Create and complete first run.
        let mut run1 = runs
            .create(&test_meta(), &Transport::Filesystem)
            .expect("create run1");
        let run1_id = run1.id().to_string();
        run1.transition(RunState::Active, None).expect("to active");
        run1.transition(RunState::Complete, None)
            .expect("to complete");

        // Create second run for same (repo, ref).
        let meta2 = RunMeta {
            sha: "def456".to_string(),
            r#ref: "refs/heads/main".to_string(),
            pushed_at: "2026-04-28T13:00:00Z".parse().unwrap(),
        };
        let _run2 = runs
            .create(&meta2, &Transport::Filesystem)
            .expect("create run2");

        // First run should still be complete.
        let reopened = Run::open(quire.db_path(), run1_id, runs.base_dir.clone()).expect("reopen");
        assert_eq!(reopened.state(), RunState::Complete);
    }

    #[test]
    fn transition_allows_pending_to_superseded() {
        let (_dir, quire) = tmp_quire();
        let runs = test_runs(&quire);
        let mut run = runs
            .create(&test_meta(), &Transport::Filesystem)
            .expect("create");
        run.transition(RunState::Superseded, None)
            .expect("to superseded");
        assert_eq!(run.state(), RunState::Superseded);
    }

    #[test]
    fn transition_allows_active_to_superseded() {
        let (_dir, quire) = tmp_quire();
        let runs = test_runs(&quire);
        let mut run = runs
            .create(&test_meta(), &Transport::Filesystem)
            .expect("create");
        run.transition(RunState::Active, None).expect("to active");
        run.transition(RunState::Superseded, None)
            .expect("to superseded");
        assert_eq!(run.state(), RunState::Superseded);
    }

    #[test]
    fn supersede_sets_finished_at() {
        let (_dir, quire) = tmp_quire();
        let runs = test_runs(&quire);
        let mut run = runs
            .create(&test_meta(), &Transport::Filesystem)
            .expect("create");
        run.transition(RunState::Active, None).expect("to active");

        assert!(
            run.read_finished_at().expect("read").is_none(),
            "should not have finished_at before supersede"
        );

        run.transition(RunState::Superseded, None)
            .expect("to superseded");
        assert!(
            run.read_finished_at().expect("read").is_some(),
            "superseded run should have finished_at"
        );
    }
}
