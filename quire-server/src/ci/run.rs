//! SQLite-backed storage for CI runs.
//!
//! A run is a row in the `runs` table identified by UUID. State
//! transitions are single `UPDATE` statements inside a transaction.
//! Run directories on disk hold the materialized workspace and per-job
//! log files, but state lives in the database.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::rc::Rc;

use jiff::Timestamp;
use quire_core::ci::pipeline::{Pipeline, RunFn};
use quire_core::ci::runtime::{Runtime, RuntimeHandle, ShOutput};
use quire_core::secret::SecretString;

use super::error::{Error, Result};

pub use quire_core::ci::run::RunMeta;

/// How a run dispatches its pipeline.
///
/// `Host` evaluates the Lua/Fennel pipeline in-process on the
/// orchestrator. `QuireCi` shells out to the `quire-ci` binary,
/// which compiles and runs the pipeline in a separate process.
/// Selected by the `:executor` key in the global config.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, serde::Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Executor {
    #[default]
    Host,
    QuireCi,
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
    /// Inserts a row into `runs` and creates the run directory for
    /// workspace materialization and log storage.
    pub fn create(&self, meta: &RunMeta) -> Result<Run> {
        let id = uuid::Uuid::now_v7().to_string();
        let workspace_path = self.base_dir.join(&id).join("workspace");

        let db = crate::db::open(&self.db_path)?;
        db.execute(
            "INSERT INTO runs (id, repo, ref_name, sha, pushed_at_ms, state, queued_at_ms, workspace_path)
             VALUES (?1, ?2, ?3, ?4, ?5, 'pending', ?6, ?7)",
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

    /// Drive `pipeline` to completion through this run.
    ///
    /// Consumes the pipeline, taking ownership of its Lua VM. Constructs
    /// a fresh [`Runtime`] with `secrets`, the source outputs
    /// (`:quire/push` from metadata), and the per-job transitive-input
    /// sets; installs it on the VM, topo-sorts the jobs, transitions
    /// Pending → Active, then invokes each `run_fn` in dependency order
    /// with the runtime handle as its sole argument. Returns a map of
    /// job id → captured `(sh …)` outputs. The run finishes in
    /// `Complete` if every job's `run_fn` returned without error,
    /// otherwise `Failed`.
    ///
    /// Per-job logs are written to `jobs/<job-id>/log` inside the run
    /// directory before the final state transition, so logs are
    /// available for both successful and failed runs.
    pub fn execute(
        mut self,
        pipeline: Pipeline,
        secrets: HashMap<String, SecretString>,
        git_dir: &std::path::Path,
        workspace: &std::path::Path,
    ) -> Result<HashMap<String, Vec<ShOutput>>> {
        let meta = self.read_meta()?;

        self.transition(RunState::Active)?;

        let runtime = Rc::new(Runtime::new(
            pipeline,
            secrets,
            &meta,
            git_dir,
            workspace.to_path_buf(),
            self.path(),
        ));

        let runtime_guard = RuntimeHandle::install(runtime.clone(), runtime.lua())
            .expect("install runtime on Lua VM");

        let mut failed_job: Option<(String, Error)> = None;
        for job_id in runtime.topo_order() {
            let run_fn = runtime
                .job(job_id)
                .expect("topo_order returned a job id not in pipeline")
                .run_fn
                .clone();

            // Insert job row in 'active' state.
            let job_started = Timestamp::now().as_millisecond();
            {
                let db = crate::db::open(&self.db_path)?;
                db.execute(
                    "INSERT INTO jobs (run_id, job_id, state, started_at_ms) VALUES (?1, ?2, 'active', ?3)",
                    rusqlite::params![&self.id, job_id, job_started],
                )?;
            }

            runtime.enter_job(job_id);
            let result: Result<()> = (|| match run_fn {
                RunFn::Lua(f) => {
                    f.call::<mlua::Value>(())?;
                    Ok(())
                }
                RunFn::Rust(f) => f(&runtime).map_err(Into::into),
            })();
            runtime.leave_job();

            // Update job row to terminal state.
            let job_finished = Timestamp::now().as_millisecond();
            let (job_state, exit_code) = match &result {
                Ok(()) => ("complete", None::<i32>),
                Err(_) => ("failed", None::<i32>),
            };
            {
                let db = crate::db::open(&self.db_path)?;
                db.execute(
                    "UPDATE jobs SET state = ?1, exit_code = ?2, finished_at_ms = ?3 WHERE run_id = ?4 AND job_id = ?5",
                    rusqlite::params![job_state, exit_code, job_finished, &self.id, job_id],
                )?;
            }

            if let Err(e) = result {
                failed_job = Some((job_id.to_string(), e));
                break;
            }
        }

        // Always drain outputs and write logs, even on failure — the
        // jobs that did run before the failure are useful context.
        let outputs = runtime.take_outputs();
        let timings = runtime.take_sh_timings();

        // Drop the guard first so the runtime app data and stub
        // entries are released before we drop the `Rc<Runtime>` that
        // owns the Lua VM behind them.
        drop(runtime_guard);

        self.write_sh_records(&outputs, &timings)?;

        // Drop the runtime *before* the final transition. In docker
        // mode this fires `DockerLifecycle::drop`, which stamps
        // `container_stopped_at` in the database.
        drop(runtime);

        if let Some((job, source)) = failed_job {
            self.transition(RunState::Failed)?;
            return Err(Error::JobFailed {
                job,
                source: Box::new(source),
            });
        }

        self.transition(RunState::Complete)?;
        Ok(outputs)
    }

    /// Run the pipeline by shelling out to the `quire-ci` binary.
    ///
    /// Layout under the run dir on disk:
    /// * `quire-ci.log` — combined stdout+stderr of the subprocess.
    /// * `events.jsonl` — structured event stream (one JSON object per
    ///   line). Ingested into `jobs` and `sh_events` after the
    ///   subprocess exits.
    /// * `jobs/<job>/sh-<n>.log` — per-sh CRI logs, written by quire-ci
    ///   via `--out-dir`. Same layout the Host executor produces.
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
    ) -> Result<()> {
        self.transition(RunState::Active)?;

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

        let status = std::process::Command::new("quire-ci")
            .arg("run")
            .arg("--workspace")
            .arg(workspace)
            .arg("--out-dir")
            .arg(&run_dir)
            .arg("--events")
            .arg(&events_path)
            .arg("--dispatch")
            .arg(&dispatch_path)
            .stdout(std::process::Stdio::from(log))
            .stderr(std::process::Stdio::from(log_clone))
            .status()
            .map_err(|source| Error::CommandSpawnFailed {
                program: "quire-ci".to_string(),
                cwd: workspace.to_path_buf(),
                source,
            })?;

        // Ingest events whether or not the run succeeded — partial
        // results are still useful in the UI. A failure to read or
        // parse the file goes to the log but doesn't mask the run's
        // own pass/fail outcome.
        if let Err(e) = self.ingest_events(&events_path) {
            tracing::warn!(
                run_id = %self.id,
                error = %e,
                "failed to ingest quire-ci events; jobs/sh_events rows may be incomplete"
            );
        }

        if !status.success() {
            self.transition(RunState::Failed)?;
            return Err(Error::QuireCiExit {
                exit: status.code(),
            });
        }

        self.transition(RunState::Complete)?;
        Ok(())
    }

    /// Read `events.jsonl` and replay it into the database.
    ///
    /// Done in two passes because `sh_events` has a foreign key on
    /// `(run_id, job_id)` in `jobs`, and the wire format interleaves
    /// sh events with their owning job. Pass 1 inserts every job row
    /// (paired by `job_id`); pass 2 inserts sh events.
    fn ingest_events(&self, path: &Path) -> Result<()> {
        use quire_core::ci::event::{Event, EventKind, JobOutcome};

        let bytes = match fs_err::read(path) {
            Ok(b) => b,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(()),
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
                EventKind::JobStarted { .. } | EventKind::JobFinished { .. } => {}
            }
        }

        Ok(())
    }

    /// Insert sh_events DB rows from the runtime's captured outputs and
    /// timings. Written before the final state transition so events are
    /// available for both successful and failed runs.
    ///
    /// Per-sh CRI log files are written by [`Runtime::sh`] inline as
    /// the run progresses (see `Runtime::log_dir`), so this is purely
    /// a database concern.
    fn write_sh_records(
        &self,
        outputs: &HashMap<String, Vec<ShOutput>>,
        timings: &HashMap<String, quire_core::ci::runtime::ShTimings>,
    ) -> Result<()> {
        if outputs.is_empty() {
            return Ok(());
        }

        let db = crate::db::open(&self.db_path)?;

        for (job_id, sh_outputs) in outputs {
            let job_timings = timings.get(job_id);

            for (i, output) in sh_outputs.iter().enumerate() {
                let (started_at, finished_at) = job_timings
                    .and_then(|t| t.get(i))
                    .copied()
                    .unwrap_or_else(|| {
                        let now = jiff::Timestamp::now();
                        (now, now)
                    });

                db.execute(
                    "INSERT INTO sh_events (run_id, job_id, started_at_ms, finished_at_ms, exit_code, cmd)
                     VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
                    rusqlite::params![
                        &self.id,
                        job_id,
                        started_at.as_millisecond(),
                        finished_at.as_millisecond(),
                        output.exit,
                        &output.cmd,
                    ],
                )?;
            }
        }

        Ok(())
    }

    /// Transition the run from its current state to a new state.
    ///
    /// Executes a single `UPDATE` in the database, stamping
    /// `started_at` (entering Active) or `finished_at` (entering
    /// Complete or Failed) and clearing `container_id` on terminal
    /// states. Each timestamp is set at most once.
    pub fn transition(&mut self, to: RunState) -> Result<()> {
        use RunState::*;
        let allowed = matches!(
            (self.state, to),
            (Pending, Active) | (Pending, Complete) | (Active, Complete) | (Active, Failed)
        );
        if !allowed {
            return Err(Error::InvalidTransition {
                from: self.state,
                to,
            });
        }

        let now = Timestamp::now().as_millisecond();
        let db = crate::db::open(&self.db_path)?;

        // Build the SET clause dynamically based on the target state.
        match to {
            Active => {
                db.execute(
                    "UPDATE runs SET state = 'active', started_at_ms = COALESCE(started_at_ms, ?1)
                     WHERE id = ?2",
                    rusqlite::params![now, &self.id],
                )?;
            }
            Complete | Failed => {
                db.execute(
                    "UPDATE runs SET state = ?1, \
                        started_at_ms = COALESCE(started_at_ms, ?2), \
                        finished_at_ms = COALESCE(finished_at_ms, ?3), \
                        container_id = NULL \
                     WHERE id = ?4",
                    rusqlite::params![to.as_str(), now, now, &self.id],
                )?;
            }
            _ => unreachable!("checked by allowed match above"),
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

    /// Materialize a workspace directory under the test Quire's base dir.
    /// Used by `Run::execute` call sites to satisfy the workspace param.
    fn test_workspace(quire: &Quire) -> PathBuf {
        let workspace = quire.base_dir().join("ws");
        fs_err::create_dir_all(&workspace).expect("mkdir workspace");
        workspace
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
        let run = runs.create(&test_meta()).expect("create");
        let parsed = uuid::Uuid::parse_str(run.id()).expect("should be valid UUID");
        assert_eq!(parsed.get_version(), Some(uuid::Version::SortRand));
    }

    #[test]
    fn create_writes_row_in_pending_state() {
        let (_dir, quire) = tmp_quire();
        let runs = test_runs(&quire);
        let run = runs.create(&test_meta()).expect("create");

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
        let mut run = runs.create(&test_meta()).expect("create");
        let id = run.id().to_string();

        run.transition(RunState::Active).expect("transition");
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
        let mut run = runs.create(&test_meta()).expect("create");

        run.transition(RunState::Active).expect("to active");
        let started = run.read_started_at().expect("read started_at");
        assert!(started.is_some(), "started_at should be stamped");
        assert!(run.read_finished_at().expect("read").is_none());
    }

    #[test]
    fn transition_stamps_finished_at_on_complete_and_failed() {
        let (_dir, quire) = tmp_quire();
        let runs = test_runs(&quire);

        let mut completed = runs.create(&test_meta()).expect("create");
        completed.transition(RunState::Active).expect("to active");
        completed
            .transition(RunState::Complete)
            .expect("to complete");
        assert!(completed.read_finished_at().expect("read").is_some());

        let mut failed = runs.create(&test_meta()).expect("create");
        failed.transition(RunState::Active).expect("to active");
        failed.transition(RunState::Failed).expect("to failed");
        assert!(failed.read_finished_at().expect("read").is_some());
    }

    #[test]
    fn transition_rejects_invalid_transitions() {
        let (_dir, quire) = tmp_quire();
        let runs = test_runs(&quire);

        // Pending -> Failed is not allowed (must go via Active).
        let mut run = runs.create(&test_meta()).expect("create");
        assert!(run.transition(RunState::Failed).is_err());

        // Terminal -> anything is not allowed.
        let mut completed = runs.create(&test_meta()).expect("create");
        completed.transition(RunState::Active).expect("to active");
        completed
            .transition(RunState::Complete)
            .expect("to complete");
        assert!(completed.transition(RunState::Active).is_err());
        assert!(completed.transition(RunState::Failed).is_err());
    }

    #[test]
    fn transition_preserves_started_at_through_completion() {
        let (_dir, quire) = tmp_quire();
        let runs = test_runs(&quire);
        let mut run = runs.create(&test_meta()).expect("create");

        run.transition(RunState::Active).expect("to active");
        let started = run.read_started_at().expect("read started_at");

        run.transition(RunState::Complete).expect("to complete");
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
        let mut run = runs.create(&test_meta()).expect("create");

        run.transition(RunState::Active).expect("to active");
        run.transition(RunState::Complete).expect("to complete");

        assert_eq!(run.state(), RunState::Complete);
    }

    #[test]
    fn reconcile_fails_pending_orphans() {
        let (_dir, quire) = tmp_quire();
        let runs = test_runs(&quire);
        let run = runs.create(&test_meta()).expect("create");
        let id = run.id().to_string();

        reconcile_orphans(&quire.db_path()).expect("reconcile");

        let reopened = Run::open(quire.db_path(), id, runs.base_dir.clone()).expect("reopen");
        assert_eq!(reopened.state(), RunState::Failed);
    }

    #[test]
    fn reconcile_fails_active_orphans() {
        let (_dir, quire) = tmp_quire();
        let runs = test_runs(&quire);
        let mut run = runs.create(&test_meta()).expect("create");
        run.transition(RunState::Active).expect("to active");
        let id = run.id().to_string();

        reconcile_orphans(&quire.db_path()).expect("reconcile");

        let reopened = Run::open(quire.db_path(), id, runs.base_dir.clone()).expect("reopen");
        assert_eq!(reopened.state(), RunState::Failed);
    }

    #[test]
    fn reconcile_leaves_complete_runs_alone() {
        let (_dir, quire) = tmp_quire();
        let runs = test_runs(&quire);
        let mut run = runs.create(&test_meta()).expect("create");
        run.transition(RunState::Active).expect("to active");
        run.transition(RunState::Complete).expect("to complete");
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

    fn load(source: &str) -> Pipeline {
        quire_core::ci::pipeline::compile(source, "ci.fnl").expect("compile should succeed")
    }

    #[test]
    fn host_mode_runs_sh_in_workspace() {
        let (_dir, quire) = tmp_quire();
        let runs = test_runs(&quire);
        let run = runs.create(&test_meta()).expect("create");

        let workspace = quire.base_dir().join("ws");
        fs_err::create_dir_all(&workspace).expect("mkdir ws");
        fs_err::write(workspace.join("marker"), "x").expect("write marker");

        let pipeline = load(
            r#"(local {: job : runtime} (require :quire.ci))
(job :pwd [:quire/push] (fn [] (runtime.sh ["ls"])))"#,
        );

        let outputs = run
            .execute(
                pipeline,
                HashMap::new(),
                std::path::Path::new("."),
                &workspace,
            )
            .expect("execute");
        let pwd = &outputs["pwd"];
        assert_eq!(pwd.len(), 1);
        assert!(
            pwd[0].stdout.contains("marker"),
            "expected workspace ls to include marker, got: {:?}",
            pwd[0].stdout,
        );
    }

    #[test]
    fn execute_records_outputs_per_job() {
        let (_dir, quire) = tmp_quire();
        let runs = test_runs(&quire);
        let run = runs.create(&test_meta()).expect("create");

        let pipeline = load(
            r#"(local {: job : runtime} (require :quire.ci))
(job :a [:quire/push] (fn [] (runtime.sh ["echo" "from-a"])))
(job :b [:a] (fn [] (runtime.sh ["echo" "from-b"])))"#,
        );

        let run_id = run.id().to_string();
        let outputs = run
            .execute(
                pipeline,
                HashMap::new(),
                std::path::Path::new("."),
                &test_workspace(&quire),
            )
            .expect("execute");

        // Verify the run landed in complete in the DB.
        let reopened = Run::open(quire.db_path(), run_id, runs.base_dir.clone()).expect("reopen");
        assert_eq!(reopened.state(), RunState::Complete);

        let a = &outputs["a"];
        let b = &outputs["b"];
        assert_eq!(a.len(), 1);
        assert_eq!(a[0].stdout, "from-a\n");
        assert_eq!(b.len(), 1);
        assert_eq!(b[0].stdout, "from-b\n");
    }

    #[test]
    fn execute_runs_jobs_in_topo_order() {
        let (_dir, quire) = tmp_quire();
        let runs = test_runs(&quire);
        let run = runs.create(&test_meta()).expect("create");

        let log = quire.base_dir().join("order.log");
        let log_str = log.to_string_lossy();
        let source = format!(
            r#"(local {{: job : runtime}} (require :quire.ci))
(job :b [:a] (fn [] (runtime.sh (.. "echo b >> {log}"))))
(job :a [:quire/push] (fn [] (runtime.sh (.. "echo a >> {log}"))))"#,
            log = log_str
        );
        let pipeline = load(&source);

        run.execute(
            pipeline,
            HashMap::new(),
            std::path::Path::new("."),
            &test_workspace(&quire),
        )
        .expect("execute");

        let contents = fs_err::read_to_string(&log).expect("read log");
        assert_eq!(contents, "a\nb\n");
    }

    #[test]
    fn execute_stops_on_first_failure_and_transitions_failed() {
        let (_dir, quire) = tmp_quire();
        let runs = test_runs(&quire);
        let run = runs.create(&test_meta()).expect("create");

        let pipeline = load(
            r#"(local {: job : runtime} (require :quire.ci))
(job :a [:quire/push] (fn [] (error "boom")))
(job :b [:a] (fn [] (runtime.sh ["echo" "should-not-run"])))"#,
        );

        let run_id = run.id().to_string();
        let err = run
            .execute(
                pipeline,
                HashMap::new(),
                std::path::Path::new("."),
                &test_workspace(&quire),
            )
            .expect_err("expected failure");
        assert!(matches!(err, Error::JobFailed { ref job, .. } if job == "a"));

        // Verify the run is failed in the DB.
        let reopened = Run::open(quire.db_path(), run_id, runs.base_dir.clone()).expect("reopen");
        assert_eq!(reopened.state(), RunState::Failed);
    }

    #[test]
    fn jobs_returns_quire_push_outputs_for_direct_input() {
        let (_dir, quire) = tmp_quire();
        let runs = test_runs(&quire);
        let run = runs.create(&test_meta()).expect("create");

        let pipeline = load(
            r#"(local {: job : runtime} (require :quire.ci))
(job :grab [:quire/push]
  (fn []
    (let [push (runtime.jobs :quire/push)]
      (runtime.sh ["echo" push.sha push.ref]))))"#,
        );

        let outputs = run
            .execute(
                pipeline,
                HashMap::new(),
                std::path::Path::new("."),
                &test_workspace(&quire),
            )
            .expect("execute");

        let grab = &outputs["grab"];
        assert_eq!(grab.len(), 1);
        assert_eq!(grab[0].stdout, "abc123 refs/heads/main\n");
    }

    #[test]
    fn jobs_returns_quire_push_outputs_through_transitive_input() {
        let (_dir, quire) = tmp_quire();
        let runs = test_runs(&quire);
        let run = runs.create(&test_meta()).expect("create");

        let pipeline = load(
            r#"(local {: job : runtime} (require :quire.ci))
(job :a [:quire/push] (fn [] nil))
(job :b [:a]
  (fn []
    (let [push (runtime.jobs :quire/push)]
      (runtime.sh ["echo" push.sha]))))"#,
        );

        let outputs = run
            .execute(
                pipeline,
                HashMap::new(),
                std::path::Path::new("."),
                &test_workspace(&quire),
            )
            .expect("execute");

        let b = &outputs["b"];
        assert_eq!(b.len(), 1);
        assert_eq!(b[0].stdout, "abc123\n");
    }

    #[test]
    fn jobs_errors_on_unknown_name() {
        let (_dir, quire) = tmp_quire();
        let runs = test_runs(&quire);
        let run = runs.create(&test_meta()).expect("create");

        let pipeline = load(
            r#"(local {: job : runtime} (require :quire.ci))
(job :grab [:quire/push] (fn [] (runtime.jobs :nope)))"#,
        );

        let err = run
            .execute(
                pipeline,
                HashMap::new(),
                std::path::Path::new("."),
                &test_workspace(&quire),
            )
            .expect_err("expected failure");
        let Error::JobFailed { job, source } = err else {
            unreachable!()
        };
        assert_eq!(job, "grab");
        let msg = source.to_string();
        assert!(
            msg.contains("not in transitive inputs") && msg.contains("nope"),
            "expected 'not in transitive inputs' error, got: {msg}"
        );
    }

    #[test]
    fn jobs_errors_on_non_ancestor_job() {
        let (_dir, quire) = tmp_quire();
        let runs = test_runs(&quire);
        let run = runs.create(&test_meta()).expect("create");

        let pipeline = load(
            r#"(local {: job : runtime} (require :quire.ci))
(job :peer [:quire/push] (fn [] nil))
(job :grab [:quire/push] (fn [] (runtime.jobs :peer)))"#,
        );

        let err = run
            .execute(
                pipeline,
                HashMap::new(),
                std::path::Path::new("."),
                &test_workspace(&quire),
            )
            .expect_err("expected failure");
        let Error::JobFailed { source, .. } = err else {
            unreachable!()
        };
        let msg = source.to_string();
        assert!(
            msg.contains("not in transitive inputs") && msg.contains("peer"),
            "expected non-ancestor error, got: {msg}"
        );
    }

    #[test]
    fn jobs_errors_on_self_lookup() {
        let (_dir, quire) = tmp_quire();
        let runs = test_runs(&quire);
        let run = runs.create(&test_meta()).expect("create");

        let pipeline = load(
            r#"(local {: job : runtime} (require :quire.ci))
(job :grab [:quire/push] (fn [] (runtime.jobs :grab)))"#,
        );

        let err = run
            .execute(
                pipeline,
                HashMap::new(),
                std::path::Path::new("."),
                &test_workspace(&quire),
            )
            .expect_err("expected failure");
        let Error::JobFailed { source, .. } = err else {
            unreachable!()
        };
        let msg = source.to_string();
        assert!(
            msg.contains("cannot read its own outputs"),
            "expected self-lookup error, got: {msg}"
        );
    }

    #[test]
    fn jobs_returns_nil_for_dependency_with_no_outputs() {
        let (_dir, quire) = tmp_quire();
        let runs = test_runs(&quire);
        let run = runs.create(&test_meta()).expect("create");

        let pipeline = load(
            r#"(local {: job : runtime} (require :quire.ci))
(job :a [:quire/push] (fn [] nil))
(job :b [:a]
  (fn []
    (let [a-outputs (runtime.jobs :a)]
      (runtime.sh ["echo" (tostring a-outputs)]))))"#,
        );

        let outputs = run
            .execute(
                pipeline,
                HashMap::new(),
                std::path::Path::new("."),
                &test_workspace(&quire),
            )
            .expect("execute");
        let b = &outputs["b"];
        assert_eq!(b.len(), 1);
        assert_eq!(b[0].stdout, "nil\n");
    }

    #[test]
    fn execute_writes_job_logs_to_disk() {
        let (_dir, quire) = tmp_quire();
        let runs = test_runs(&quire);
        let run = runs.create(&test_meta()).expect("create");

        let pipeline = load(
            r#"(local {: job : runtime} (require :quire.ci))
(job :greet [:quire/push] (fn [] (runtime.sh ["echo" "hello"])))"#,
        );

        let run_id = run.id().to_string();
        run.execute(
            pipeline,
            HashMap::new(),
            std::path::Path::new("."),
            &test_workspace(&quire),
        )
        .expect("execute");

        // CRI log file should exist.
        let log_path = runs
            .base_dir
            .join(&run_id)
            .join("jobs")
            .join("greet")
            .join("sh-1.log");
        assert!(log_path.exists(), "sh-1.log should exist");

        let contents = fs_err::read_to_string(&log_path).expect("read log");
        assert!(contents.contains("stdout F hello"));

        // sh_events table should have one row.
        let db = crate::db::open(&quire.db_path()).expect("db");
        let count: i64 = db
            .query_row(
                "SELECT COUNT(*) FROM sh_events WHERE run_id = ?1 AND job_id = 'greet'",
                rusqlite::params![&run_id],
                |row| row.get(0),
            )
            .expect("query");
        assert_eq!(count, 1);
    }

    #[test]
    fn execute_writes_logs_for_failed_run() {
        let (_dir, quire) = tmp_quire();
        let runs = test_runs(&quire);
        let run = runs.create(&test_meta()).expect("create");

        // `a` succeeds, `b` fails — log for `a` should still be written.
        let pipeline = load(
            r#"(local {: job : runtime} (require :quire.ci))
(job :a [:quire/push] (fn [] (runtime.sh ["echo" "from-a"])))
(job :b [:a] (fn [] (error "boom")))"#,
        );

        let run_id = run.id().to_string();
        let _ = run.execute(
            pipeline,
            HashMap::new(),
            std::path::Path::new("."),
            &test_workspace(&quire),
        );

        let failed_dir = runs.base_dir.join(&run_id);
        assert!(failed_dir.exists(), "run directory should exist");

        let log_path = failed_dir.join("jobs").join("a").join("sh-1.log");
        assert!(
            log_path.exists(),
            "job 'a' sh-1.log should exist even though 'b' failed"
        );

        let contents = fs_err::read_to_string(&log_path).expect("read log");
        assert!(contents.contains("stdout F from-a"));
    }

    #[test]
    fn execute_errors_when_image_called_in_run_fn() {
        let (_dir, quire) = tmp_quire();
        let runs = test_runs(&quire);
        let run = runs.create(&test_meta()).expect("create");

        let pipeline = load(
            r#"(local {: job : image} (require :quire.ci))
(image "alpine")
(job :bad [:quire/push]
  (fn []
    (image "sneaky")))"#,
        );

        let err = run
            .execute(
                pipeline,
                HashMap::new(),
                std::path::Path::new("."),
                &test_workspace(&quire),
            )
            .expect_err("expected failure");
        let Error::JobFailed { job, source } = err else {
            panic!("expected JobFailed, got: {err:?}")
        };
        assert_eq!(job, "bad");
        let msg = source.to_string();
        assert!(
            msg.contains("registration not installed"),
            "expected registration error, got: {msg}"
        );
    }

    #[test]
    fn rust_run_fn_is_invoked_by_executor() {
        use std::cell::Cell;

        let (_dir, quire) = tmp_quire();
        let runs = test_runs(&quire);
        let run = runs.create(&test_meta()).expect("create");

        let mut pipeline = load(
            r#"(local {: job : runtime} (require :quire.ci))
(job :only [:quire/push] (fn [] nil))"#,
        );

        let called = Rc::new(Cell::new(false));
        let called_clone = called.clone();
        pipeline.replace_first_run_fn(RunFn::Rust(Rc::new(move |_rt| {
            called_clone.set(true);
            Ok(())
        })));

        run.execute(
            pipeline,
            HashMap::new(),
            std::path::Path::new("."),
            &test_workspace(&quire),
        )
        .expect("execute should succeed");
        assert!(called.get(), "rust run-fn should have been called");
    }

    #[test]
    fn rust_run_fn_errors_surface_as_job_failed() {
        let (_dir, quire) = tmp_quire();
        let runs = test_runs(&quire);
        let run = runs.create(&test_meta()).expect("create");

        let mut pipeline = load(
            r#"(local {: job : runtime} (require :quire.ci))
(job :boom [:quire/push] (fn [] nil))"#,
        );

        pipeline.replace_first_run_fn(RunFn::Rust(Rc::new(|_rt| {
            Err(crate::ci::runtime::RuntimeError::Git(
                "simulated rust failure".into(),
            ))
        })));

        let err = run
            .execute(
                pipeline,
                HashMap::new(),
                std::path::Path::new("."),
                &test_workspace(&quire),
            )
            .expect_err("expected failure");
        let Error::JobFailed { job, source } = err else {
            panic!("expected JobFailed, got: {err:?}");
        };
        assert_eq!(job, "boom");
        assert!(
            source.to_string().contains("simulated rust failure"),
            "expected source to surface rust error, got: {source}"
        );
    }

    #[test]
    fn ingest_events_writes_jobs_and_sh_events_rows() {
        use quire_core::ci::event::{Event, EventKind, JobOutcome};

        let (_dir, quire) = tmp_quire();
        let runs = test_runs(&quire);
        let run = runs.create(&test_meta()).expect("create");
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
        ];

        let events_path = run.path().join("events.jsonl");
        let mut bytes = Vec::new();
        for ev in &events {
            bytes.extend(serde_json::to_vec(ev).unwrap());
            bytes.push(b'\n');
        }
        fs_err::write(&events_path, bytes).expect("write events.jsonl");

        run.ingest_events(&events_path).expect("ingest");

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
        let run = runs.create(&test_meta()).expect("create");

        let missing = run.path().join("events.jsonl");
        run.ingest_events(&missing)
            .expect("missing file should not error");

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
}
