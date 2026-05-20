//! SQLite-backed storage for CI runs.
//!
//! A run is a row in the `runs` table identified by UUID. State
//! transitions are single `UPDATE` statements inside a transaction.
//! Run directories on disk hold the materialized workspace and per-job
//! log files, but state lives in the database.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};


use super::error::{Error, Result};
use jiff::Timestamp;
use quire_core::ci::event::{Event, EventKind, JobOutcome, RunOutcome};
use quire_core::ci::run::ApiSession;

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

impl std::fmt::Display for Executor {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Process => f.write_str("process"),
        }
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

    /// Create a new run record in the queued state.
    ///
    /// Before inserting, cancels any existing queued or active
    /// run for the same `(repo, ref)`.
    ///
    /// Inserts a row into `runs` and creates the run directory for
    /// workspace materialization and log storage.
    ///
    /// `session` is `Some` for orchestrator-dispatched runs — the run's id
    /// and bearer token come from the session and are persisted so quire-ci
    /// and the DB agree. Pass `None` for local runs; a fresh UUID is minted
    /// and no auth token is stored.
    pub fn create(&self, meta: &RunMeta, session: Option<&ApiSession>) -> Result<Run> {
        let (id, run_token_str) = match session {
            None => (uuid::Uuid::now_v7().to_string(), None),
            Some(s) => {
                let id = uuid::Uuid::now_v7().to_string();
                (id, Some(s.run_token.as_str()))
            }
        };

        let db = crate::db::Db::open(&self.db_path)?;

        // Cancel any existing queued or active run for (repo, ref).
        // Do this before inserting the new run so the new run is never
        // caught by its own cancel query.
        self.cancel_existing(&db, &meta.r#ref)?;

        db.insert_run(&crate::db::runs::NewRun {
            id: &id,
            repo: &self.repo,
            ref_name: &meta.r#ref,
            sha: &meta.sha,
            pushed_at_ms: meta.pushed_at.as_millisecond(),
            created_at: Timestamp::now().as_millisecond(),
            run_token: run_token_str,
        })?;

        // Create run directory for workspace and logs.
        let workspace_path = self.base_dir.join(&id).join("workspace");
        fs_err::create_dir_all(&workspace_path)?;

        Ok(Run {
            db_path: self.db_path.clone(),
            id,
            dispatched: false,
            resolved: false,
            base_dir: self.base_dir.clone(),
        })
    }

    /// Cancel any existing queued or active run for
    /// `(repo, ref)`. Different refs are unaffected.
    fn cancel_existing(&self, db: &crate::db::Db, ref_name: &str) -> Result<()> {
        let now = Timestamp::now().as_millisecond();

        let active_ids = db.get_active_runs_for_ref(&self.repo, ref_name)?;

        for run_id in &active_ids {
            db.cancel_active_run(run_id, now)?;
            tracing::info!(run_id = %run_id, "canceled active run");
        }

        let queued_count = db.cancel_queued_runs_for_ref(&self.repo, ref_name, now)?;
        if queued_count > 0 {
            tracing::info!(count = queued_count, "canceled queued run(s)");
        }

        Ok(())
    }
}

/// Move every queued or active run to `failed-orphaned`. Called once at
/// server startup to clean up runs left behind by a prior instance.
/// Operates across all repos — orphans aren't a per-repo concern.
pub fn reconcile_orphans(db_path: &Path) -> Result<()> {
    let now = Timestamp::now().as_millisecond();
    let db = crate::db::Db::open(db_path)?;
    let count = db.fail_orphaned_runs(now)?;
    if count > 0 {
        tracing::warn!(count, "reconciled orphaned runs");
    }
    Ok(())
}

/// A CI run backed by a SQLite row.
///
/// Owns the path to the database and the run's in-memory lifecycle flags.
/// Reads and writes go through SQL. The run directory on disk holds
/// the workspace and per-job log files.
pub struct Run {
    db_path: PathBuf,
    id: String,
    /// Whether `dispatched_at` has been set (run is active or resolved).
    dispatched: bool,
    /// Whether `resolved_at` has been set (run is terminal).
    resolved: bool,
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

    /// Open an existing run from the database by ID.
    pub fn open(db_path: PathBuf, id: String, base_dir: PathBuf) -> Result<Self> {
        let db = crate::db::Db::open(&db_path)?;
        let (dispatched_at, resolved_at) = db.get_run_lifecycle(&id)?;
        Ok(Self {
            db_path,
            id,
            dispatched: dispatched_at.is_some(),
            resolved: resolved_at.is_some(),
            base_dir,
        })
    }

    /// Run the pipeline by shelling out to the `quire-ci` binary.
    ///
    /// Layout under the run dir on disk:
    /// * `quire-ci.log` — combined stdout+stderr of the subprocess.
    /// * `events.jsonl` — structured event stream (one JSON object per
    ///   line). Ingested into `jobs` and `sh` after the
    ///   subprocess exits.
    /// * `jobs/<job>/sh-<n>.log` — per-sh CRI logs, written by quire-ci
    ///   via `--out-dir`.
    ///
    /// Run finishes `succeeded` on exit 0, `failed-*` otherwise. The DB
    /// rows are written even on failure so the web UI can render
    /// partial progress.
    pub fn execute(
        mut self,
        git_dir: &Path,
        workspace: &Path,
        traceparent: Option<&str>,
        sentry_dsn: Option<&str>,
        session: Option<&ApiSession>,
    ) -> Result<()> {
        // For API runs the GET /api/run/bootstrap endpoint owns the
        // dispatch (it sets dispatched_at when quire-ci fetches the payload).
        // Calling dispatch() here would set dispatched_at in the DB before
        // quire-ci connects, causing the endpoint to return 410 Gone.
        // Update local flag only so the later resolve() call skips the
        // already-dispatched guard.
        if session.is_some() {
            self.dispatched = true;
        } else {
            self.dispatch()?;
        }

        let run_dir = self.path();
        let log_path = run_dir.join("quire-ci.log");
        let events_path = run_dir.join("events.jsonl");
        // fs_err for the path-bearing IO error; unwrap to std::fs::File so
        // it's convertible into Stdio.
        let log = fs_err::File::create(&log_path)?.into_parts().0;
        let log_clone = log.try_clone()?;

        tracing::info!(
            run_id = %self.id,
            log = %log_path.display(),
            events = %events_path.display(),
            "dispatching run to quire-ci",
        );

        let mut cmd = std::process::Command::new("quire-ci");
        cmd.arg("run")
            .arg("--workspace")
            .arg(workspace)
            .arg("--out-dir")
            .arg(&run_dir)
            .arg("--events")
            .arg(&events_path);

        match session {
            None => {
                cmd.arg("--local").arg("--git-dir").arg(git_dir);
            }
            Some(s) => {
                self.store_bootstrap_data(git_dir, traceparent)?;
                cmd.env("QUIRE__SERVER_URL", &s.server_url);
                cmd.env("QUIRE__RUN_TOKEN", &s.run_token);
            }
        }
        if let Some(dsn) = sentry_dsn {
            cmd.env("QUIRE__SENTRY_DSN", dsn);
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

        // Ingest events before checking outcome — partial results from a
        // crashed run are still useful in the UI. A parse failure is
        // logged but doesn't mask the run outcome.
        let run_outcome = match self.ingest_events(&events_path) {
            Ok(outcome) => outcome,
            Err(e) => {
                tracing::warn!(
                    run_id = %self.id,
                    error = %e,
                    "failed to ingest quire-ci events; jobs/sh rows may be incomplete"
                );
                None
            }
        };

        if !status.success() {
            self.resolve("failed-internal")?;
            return Err(Error::ProcessFailed {
                exit: status.code(),
            });
        }

        // Exit 0: RunFinished determines the pipeline outcome. Absent means
        // quire-ci exited cleanly but never reached the terminal event —
        // treat that as a crash too.
        match run_outcome {
            Some(quire_core::ci::event::RunOutcome::Succeeded) => {
                self.resolve("succeeded")?;
            }
            Some(quire_core::ci::event::RunOutcome::PipelineFailure) => {
                self.resolve("failed-pipeline")?;
            }
            None => {
                self.resolve("failed-internal")?;
                return Err(Error::ProcessFailed {
                    exit: status.code(),
                });
            }
        }
        Ok(())
    }

    /// Read `events.jsonl` and replay it into the database.
    ///
    /// Done in two passes because `sh` has a foreign key on
    /// `(run_id, job_id)` in `jobs`, and the wire format interleaves
    /// sh events with their owning job. Pass 1 inserts every job row
    /// (paired by `job_id`); pass 2 inserts sh events.
    fn ingest_events(&self, path: &Path) -> Result<Option<RunOutcome>> {
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

        let db = crate::db::Db::open(&self.db_path)?;

        // Pass 1: jobs rows. Pair JobStarted with JobFinished by job_id.
        let mut inflight_jobs: HashMap<&str, i64> = HashMap::new();
        let mut run_outcome: Option<RunOutcome> = None;
        for event in &events {
            match &event.kind {
                EventKind::JobStarted { job_id } => {
                    inflight_jobs.insert(job_id.as_str(), event.at_ms);
                }
                EventKind::JobFinished { job_id, outcome } => {
                    let started_at = inflight_jobs.remove(job_id.as_str()).unwrap_or(event.at_ms);
                    let state = match outcome {
                        JobOutcome::Succeeded => "succeeded",
                        JobOutcome::Failed => "failed",
                    };
                    db.insert_job(&self.id, job_id, state, None, started_at, event.at_ms)?;
                }
                EventKind::RunFinished { outcome } => {
                    run_outcome = Some(*outcome);
                }
                EventKind::ShStarted { .. } | EventKind::ShFinished { .. } => {}
            }
        }

        // Pass 2: sh rows. Pair ShStarted with ShFinished by job_id
        // (sequential within a run-fn, so a single buffer slot per job
        // is enough).
        let mut inflight_sh: HashMap<&str, (i64, &str)> = HashMap::new();
        for event in &events {
            match &event.kind {
                EventKind::ShStarted { job_id, cmd } => {
                    inflight_sh.insert(job_id.as_str(), (event.at_ms, cmd.as_str()));
                }
                EventKind::ShFinished { job_id, exit_code } => {
                    let Some((started_at, cmd)) = inflight_sh.remove(job_id.as_str()) else {
                        continue;
                    };
                    db.insert_sh_event(&self.id, job_id, started_at, event.at_ms, *exit_code, cmd)?;
                }
                EventKind::JobStarted { .. }
                | EventKind::JobFinished { .. }
                | EventKind::RunFinished { .. } => {}
            }
        }

        Ok(run_outcome)
    }

    /// Persist bootstrap data in the DB so the API endpoint can serve it.
    ///
    /// Called by `execute` when the API transport is active, before spawning
    /// quire-ci. quire-ci fetches this via `GET /api/runs/:id/bootstrap`
    /// instead of reading a file.
    fn store_bootstrap_data(&self, git_dir: &Path, traceparent: Option<&str>) -> Result<()> {
        let git_dir_str = git_dir.to_str().ok_or_else(|| {
            std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                "git_dir path is not valid UTF-8",
            )
        })?;
        let db = crate::db::Db::open(&self.db_path)?;
        db.set_run_bootstrap_data(&self.id, git_dir_str, traceparent)?;
        Ok(())
    }

    /// Mark the run as dispatched (queued → active). Sets `dispatched_at`.
    pub fn dispatch(&mut self) -> Result<()> {
        if self.dispatched || self.resolved {
            return Err(Error::AlreadyDispatched);
        }
        let now = Timestamp::now().as_millisecond();
        let db = crate::db::Db::open(&self.db_path)?;
        db.set_run_dispatched(&self.id, now)?;
        self.dispatched = true;
        Ok(())
    }

    /// Resolve the run with an outcome. Sets `resolved_at` and `outcome`.
    pub fn resolve(&mut self, outcome: &str) -> Result<()> {
        if self.resolved {
            return Err(Error::AlreadyResolved);
        }
        let now = Timestamp::now().as_millisecond();
        let db = crate::db::Db::open(&self.db_path)?;
        db.resolve_run(&self.id, now, outcome)?;
        self.dispatched = true;
        self.resolved = true;
        Ok(())
    }

    /// Read the immutable metadata for this run.
    pub fn read_meta(&self) -> Result<RunMeta> {
        let db = crate::db::Db::open(&self.db_path)?;
        let (sha, ref_name, pushed_at_ms) = db.get_run_meta(&self.id)?;
        Ok(RunMeta {
            sha,
            r#ref: ref_name,
            pushed_at: Timestamp::from_millisecond(pushed_at_ms)
                .expect("db stores valid timestamps"),
        })
    }

    /// Read the `dispatched_at` timestamp for this run, if set.
    pub fn read_dispatched_at(&self) -> Result<Option<Timestamp>> {
        let db = crate::db::Db::open(&self.db_path)?;
        let ms = db.get_run_dispatched_at(&self.id)?;
        Ok(ms.map(|m| Timestamp::from_millisecond(m).expect("valid timestamp")))
    }

    /// Read the `resolved_at` timestamp for this run, if set.
    pub fn read_resolved_at(&self) -> Result<Option<Timestamp>> {
        let db = crate::db::Db::open(&self.db_path)?;
        let ms = db.get_run_resolved_at(&self.id)?;
        Ok(ms.map(|m| Timestamp::from_millisecond(m).expect("valid timestamp")))
    }

    /// Read the `outcome` string for this run, if set.
    pub fn read_outcome(&self) -> Result<Option<String>> {
        let db = crate::db::Db::open(&self.db_path)?;
        Ok(db.get_run_outcome(&self.id)?)
    }
}

/// Take the final path component of a runs base (`runs/<repo>/`) and
/// sanitize it for use as the tag segment in `quire-ci/<segment>:<id>`.
/// Materialize a working tree at `sha` into `workspace` via
/// `git archive | tar -x`. Creates the workspace dir if needed.
pub fn materialize_workspace(git_dir: &Path, sha: &str, workspace: &Path) -> Result<()> {
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
        let quire = Quire::load(dir.path().to_path_buf()).expect("load");
        // Initialize the database.
        let mut db = crate::db::Db::open(&quire.db_path()).expect("init db");
        db.migrate().expect("migrate db");
        drop(db);
        (dir, quire)
    }

    fn test_runs(quire: &Quire) -> Runs {
        let base_dir = quire.base_dir().join("runs").join("test.git");
        Runs::new(quire.db_path(), "test.git".to_string(), base_dir)
    }

    fn test_session() -> ApiSession {
        ApiSession::new(3000)
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
    fn create_generates_uuidv7_id() {
        let (_dir, quire) = tmp_quire();
        let runs = test_runs(&quire);
        let run = runs
            .create(&test_meta(), Some(&test_session()))
            .expect("create");
        let parsed = uuid::Uuid::parse_str(run.id()).expect("should be valid UUID");
        assert_eq!(parsed.get_version(), Some(uuid::Version::SortRand));
    }

    #[test]
    fn create_persists_minted_run_token() {
        let (_dir, quire) = tmp_quire();
        let runs = test_runs(&quire);
        let session = ApiSession::new(3000);
        let run = runs.create(&test_meta(), Some(&session)).expect("create");

        // Run ID is minted by the server, not taken from the session.
        assert!(uuid::Uuid::parse_str(run.id()).is_ok());

        let db = crate::db::Db::open(&quire.db_path()).expect("db");
        let stored = db.get_run_token(run.id()).expect("row");
        assert_eq!(stored.as_deref(), Some(session.run_token.as_str()));
    }

    #[test]
    fn new_session_mints_alphanumeric_token() {
        let session = ApiSession::new(3000);
        assert_eq!(session.server_url, "http://127.0.0.1:3000");
        assert_eq!(session.run_token.len(), 32);
        assert!(
            session
                .run_token
                .chars()
                .all(|c: char| c.is_ascii_alphanumeric()),
            "token should be alphanumeric, got {:?}",
            session.run_token
        );
    }

    #[test]
    fn new_session_tokens_are_unique() {
        let a = ApiSession::new(3000).run_token;
        let b = ApiSession::new(3000).run_token;
        assert_ne!(a, b, "two mints should not collide");
    }

    #[test]
    fn create_writes_row_in_queued_state() {
        let (_dir, quire) = tmp_quire();
        let runs = test_runs(&quire);
        let run = runs
            .create(&test_meta(), Some(&test_session()))
            .expect("create");

        // New run is not dispatched or resolved.
        assert!(!run.dispatched);
        assert!(!run.resolved);

        // Verify workspace directory was created.
        let workspace = run.path().join("workspace");
        assert!(workspace.exists(), "workspace directory should exist");

        // Verify metadata round-trips through the DB.
        let meta = run.read_meta().expect("read meta");
        assert_eq!(meta.sha, "abc123");

        // No dispatched_at or resolved_at yet.
        let dispatched = run.read_dispatched_at().expect("read dispatched_at");
        assert!(dispatched.is_none());
        let resolved = run.read_resolved_at().expect("read resolved_at");
        assert!(resolved.is_none());
    }

    #[test]
    fn dispatch_updates_db() {
        let (_dir, quire) = tmp_quire();
        let runs = test_runs(&quire);
        let mut run = runs
            .create(&test_meta(), Some(&test_session()))
            .expect("create");
        let id = run.id().to_string();

        run.dispatch().expect("dispatch");
        assert!(run.dispatched);
        assert!(!run.resolved);

        // Verify dispatched_at was stamped.
        let dispatched = run.read_dispatched_at().expect("read dispatched_at");
        assert!(dispatched.is_some(), "dispatched_at should be stamped");

        // Re-open the run and verify state persists.
        let reopened =
            Run::open(quire.db_path(), id.clone(), runs.base_dir.clone()).expect("reopen");
        assert!(reopened.dispatched);
        assert!(!reopened.resolved);
        assert_eq!(reopened.id(), id);
    }

    #[test]
    fn dispatch_stamps_dispatched_at() {
        let (_dir, quire) = tmp_quire();
        let runs = test_runs(&quire);
        let mut run = runs
            .create(&test_meta(), Some(&test_session()))
            .expect("create");

        run.dispatch().expect("dispatch");
        let dispatched = run.read_dispatched_at().expect("read dispatched_at");
        assert!(dispatched.is_some(), "dispatched_at should be stamped");
        assert!(run.read_resolved_at().expect("read").is_none());
    }

    #[test]
    fn resolve_stamps_resolved_at_on_succeeded_and_failed() {
        let (_dir, quire) = tmp_quire();
        let runs = test_runs(&quire);

        let mut completed = runs
            .create(&test_meta(), Some(&test_session()))
            .expect("create");
        completed.dispatch().expect("dispatch");
        completed.resolve("succeeded").expect("resolve succeeded");
        assert!(completed.read_resolved_at().expect("read").is_some());
        assert_eq!(
            completed.read_outcome().expect("read outcome").as_deref(),
            Some("succeeded")
        );

        let mut failed = runs
            .create(&test_meta(), Some(&test_session()))
            .expect("create");
        failed.dispatch().expect("dispatch");
        failed.resolve("failed-pipeline").expect("resolve failed");
        assert!(failed.read_resolved_at().expect("read").is_some());
        assert_eq!(
            failed.read_outcome().expect("read outcome").as_deref(),
            Some("failed-pipeline")
        );
    }

    #[test]
    fn resolve_records_outcome() {
        let (_dir, quire) = tmp_quire();
        let runs = test_runs(&quire);
        let mut run = runs
            .create(&test_meta(), Some(&test_session()))
            .expect("create");

        run.dispatch().expect("dispatch");
        run.resolve("failed-internal").expect("resolve");

        let outcome = run.read_outcome().expect("read outcome");
        assert_eq!(outcome.as_deref(), Some("failed-internal"));
    }

    #[test]
    fn resolve_rejects_double_resolve() {
        let (_dir, quire) = tmp_quire();
        let runs = test_runs(&quire);

        // Already resolved -> error.
        let mut completed = runs
            .create(&test_meta(), Some(&test_session()))
            .expect("create");
        completed.dispatch().expect("dispatch");
        completed.resolve("succeeded").expect("to succeed");
        assert!(completed.resolve("succeeded").is_err());
        assert!(completed.resolve("failed-pipeline").is_err());
    }

    #[test]
    fn dispatch_rejects_double_dispatch() {
        let (_dir, quire) = tmp_quire();
        let runs = test_runs(&quire);

        let mut run = runs
            .create(&test_meta(), Some(&test_session()))
            .expect("create");
        run.dispatch().expect("dispatch");
        assert!(run.dispatch().is_err());
    }

    #[test]
    fn resolve_preserves_dispatched_at_through_completion() {
        let (_dir, quire) = tmp_quire();
        let runs = test_runs(&quire);
        let mut run = runs
            .create(&test_meta(), Some(&test_session()))
            .expect("create");

        run.dispatch().expect("dispatch");
        let dispatched = run.read_dispatched_at().expect("read dispatched_at");

        run.resolve("succeeded").expect("resolve");
        assert_eq!(
            run.read_dispatched_at().expect("read"),
            dispatched,
            "dispatched_at preserved"
        );
    }

    #[test]
    fn full_lifecycle_dispatch_then_resolve() {
        let (_dir, quire) = tmp_quire();
        let runs = test_runs(&quire);
        let mut run = runs
            .create(&test_meta(), Some(&test_session()))
            .expect("create");

        run.dispatch().expect("dispatch");
        run.resolve("succeeded").expect("resolve");

        assert!(run.dispatched);
        assert!(run.resolved);
        assert_eq!(
            run.read_outcome().expect("outcome").as_deref(),
            Some("succeeded")
        );
    }

    #[test]
    fn reconcile_fails_queued_orphans() {
        let (_dir, quire) = tmp_quire();
        let runs = test_runs(&quire);
        let run = runs
            .create(&test_meta(), Some(&test_session()))
            .expect("create");
        let id = run.id().to_string();

        reconcile_orphans(&quire.db_path()).expect("reconcile");

        let reopened = Run::open(quire.db_path(), id, runs.base_dir.clone()).expect("reopen");
        assert!(reopened.resolved, "orphaned run should be resolved");
        assert_eq!(
            reopened.read_outcome().expect("outcome").as_deref(),
            Some("failed-orphaned")
        );
    }

    #[test]
    fn reconcile_fails_active_orphans() {
        let (_dir, quire) = tmp_quire();
        let runs = test_runs(&quire);
        let mut run = runs
            .create(&test_meta(), Some(&test_session()))
            .expect("create");
        run.dispatch().expect("dispatch");
        let id = run.id().to_string();

        reconcile_orphans(&quire.db_path()).expect("reconcile");

        let reopened = Run::open(quire.db_path(), id, runs.base_dir.clone()).expect("reopen");
        assert!(reopened.resolved, "orphaned active run should be resolved");
        assert_eq!(
            reopened.read_outcome().expect("outcome").as_deref(),
            Some("failed-orphaned")
        );
    }

    #[test]
    fn reconcile_leaves_succeeded_runs_alone() {
        let (_dir, quire) = tmp_quire();
        let runs = test_runs(&quire);
        let mut run = runs
            .create(&test_meta(), Some(&test_session()))
            .expect("create");
        run.dispatch().expect("dispatch");
        run.resolve("succeeded").expect("resolve");
        let id = run.id().to_string();

        reconcile_orphans(&quire.db_path()).expect("reconcile");

        let reopened = Run::open(quire.db_path(), id, runs.base_dir.clone()).expect("reopen");
        assert_eq!(
            reopened.read_outcome().expect("outcome").as_deref(),
            Some("succeeded")
        );
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
            .create(&test_meta(), Some(&test_session()))
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
                    outcome: JobOutcome::Succeeded,
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

        let conn = rusqlite::Connection::open(quire.db_path()).expect("open db");
        let jobs: Vec<(String, String, i64, i64)> = conn
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
                ("build".to_string(), "succeeded".to_string(), 100, 200),
                ("test".to_string(), "failed".to_string(), 210, 220),
            ]
        );

        let sh_events: Vec<(String, i64, i64, i32, String)> = conn
            .prepare(
                "SELECT job_id, started_at_ms, finished_at_ms, exit_code, cmd FROM sh \
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
            .create(&test_meta(), Some(&test_session()))
            .expect("create");

        let missing = run.path().join("events.jsonl");
        let outcome = run
            .ingest_events(&missing)
            .expect("missing file should not error");
        assert!(outcome.is_none(), "missing file yields no outcome");

        let conn = rusqlite::Connection::open(quire.db_path()).expect("open db");
        let count: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM jobs WHERE run_id = ?1",
                [run.id()],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(count, 0);
    }

    #[test]
    fn create_cancels_queued_run_on_same_ref() {
        let (_dir, quire) = tmp_quire();
        let runs = test_runs(&quire);

        // Create first run.
        let run1 = runs
            .create(&test_meta(), Some(&test_session()))
            .expect("create run1");
        let run1_id = run1.id().to_string();
        assert!(!run1.dispatched && !run1.resolved);

        // Create second run for same (repo, ref) — should cancel the first.
        let meta2 = RunMeta {
            sha: "def456".to_string(),
            r#ref: "refs/heads/main".to_string(),
            pushed_at: "2026-04-28T13:00:00Z".parse().unwrap(),
        };
        let run2 = runs
            .create(&meta2, Some(&test_session()))
            .expect("create run2");
        assert!(!run2.dispatched && !run2.resolved);

        // First run should now be superseded (resolved).
        let reopened = Run::open(quire.db_path(), run1_id, runs.base_dir.clone()).expect("reopen");
        assert!(reopened.resolved, "canceled run should be resolved");
        assert_eq!(
            reopened.read_outcome().expect("outcome").as_deref(),
            Some("superseded")
        );
        assert!(
            reopened.read_resolved_at().expect("read").is_some(),
            "canceled run should have resolved_at"
        );
    }

    #[test]
    fn create_cancels_active_run_on_same_ref() {
        let (_dir, quire) = tmp_quire();
        let runs = test_runs(&quire);

        // Create and dispatch first run.
        let mut run1 = runs
            .create(&test_meta(), Some(&test_session()))
            .expect("create run1");
        let run1_id = run1.id().to_string();
        run1.dispatch().expect("dispatch");

        // Create second run for same (repo, ref).
        let meta2 = RunMeta {
            sha: "def456".to_string(),
            r#ref: "refs/heads/main".to_string(),
            pushed_at: "2026-04-28T13:00:00Z".parse().unwrap(),
        };
        let run2 = runs
            .create(&meta2, Some(&test_session()))
            .expect("create run2");
        assert!(!run2.dispatched && !run2.resolved);

        // First run should be superseded.
        let reopened = Run::open(quire.db_path(), run1_id, runs.base_dir.clone()).expect("reopen");
        assert!(reopened.resolved, "canceled run should be resolved");
        assert_eq!(
            reopened.read_outcome().expect("outcome").as_deref(),
            Some("superseded")
        );
        assert!(
            reopened.read_resolved_at().expect("read").is_some(),
            "canceled run should have resolved_at"
        );
    }

    #[test]
    fn create_does_not_cancel_different_ref() {
        let (_dir, quire) = tmp_quire();
        let runs = test_runs(&quire);

        // Create run for main.
        let run1 = runs
            .create(&test_meta(), Some(&test_session()))
            .expect("create run1");
        let run1_id = run1.id().to_string();

        // Create run for a different ref.
        let meta2 = RunMeta {
            sha: "def456".to_string(),
            r#ref: "refs/heads/feature".to_string(),
            pushed_at: "2026-04-28T13:00:00Z".parse().unwrap(),
        };
        let _run2 = runs
            .create(&meta2, Some(&test_session()))
            .expect("create run2");

        // First run should still be queued (not dispatched, not resolved).
        let reopened = Run::open(quire.db_path(), run1_id, runs.base_dir.clone()).expect("reopen");
        assert!(!reopened.dispatched && !reopened.resolved);
    }

    #[test]
    fn create_does_not_cancel_succeeded_or_failed_runs() {
        let (_dir, quire) = tmp_quire();
        let runs = test_runs(&quire);

        // Drive first run to succeeded.
        let mut run1 = runs
            .create(&test_meta(), Some(&test_session()))
            .expect("create run1");
        let run1_id = run1.id().to_string();
        run1.dispatch().expect("dispatch");
        run1.resolve("succeeded").expect("resolve");

        // Create second run for same (repo, ref).
        let meta2 = RunMeta {
            sha: "def456".to_string(),
            r#ref: "refs/heads/main".to_string(),
            pushed_at: "2026-04-28T13:00:00Z".parse().unwrap(),
        };
        let _run2 = runs
            .create(&meta2, Some(&test_session()))
            .expect("create run2");

        // First run should still be succeeded.
        let reopened = Run::open(quire.db_path(), run1_id, runs.base_dir.clone()).expect("reopen");
        assert_eq!(
            reopened.read_outcome().expect("outcome").as_deref(),
            Some("succeeded")
        );
    }

    #[test]
    fn resolve_sets_resolved_at() {
        let (_dir, quire) = tmp_quire();
        let runs = test_runs(&quire);
        let mut run = runs
            .create(&test_meta(), Some(&test_session()))
            .expect("create");
        run.dispatch().expect("dispatch");

        assert!(
            run.read_resolved_at().expect("read").is_none(),
            "should not have resolved_at before resolve"
        );

        run.resolve("superseded").expect("resolve");
        assert!(
            run.read_resolved_at().expect("read").is_some(),
            "resolved run should have resolved_at"
        );
    }
}
