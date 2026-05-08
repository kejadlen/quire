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
use mlua::IntoLua;

use super::error::{Error, Result};
use super::pipeline::{Pipeline, RunFn};
use super::runtime::{Runtime, RuntimeHandle, ShOutput};
use quire_core::secret::SecretString;

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

/// Immutable metadata for a CI run. Passed to `Runs::create` at
/// enqueue time; the fields are written to the `runs` row once.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct RunMeta {
    /// The commit SHA that triggered this run.
    pub sha: String,
    /// The full ref name (e.g. `refs/heads/main`).
    pub r#ref: String,
    /// When the push occurred.
    pub pushed_at: Timestamp,
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
        ));

        let lua = runtime.lua();
        let rt_value = RuntimeHandle(runtime.clone())
            .into_lua(lua)
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
                    let _: mlua::Value = f.call(rt_value.clone())?;
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
        lua.remove_app_data::<Rc<Runtime>>();

        self.write_sh_records(&outputs, &timings)?;

        // Drop the runtime *before* the final transition. In docker
        // mode this fires `DockerLifecycle::drop`, which stamps
        // `container_stopped_at` in the database.
        drop(rt_value);
        let _ = lua; // release the Lua borrow tied to `runtime`.
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

    /// Write sh events to the database and per-sh CRI log files to
    /// disk. Written before the final state transition so logs are
    /// available for both successful and failed runs.
    fn write_sh_records(
        &self,
        outputs: &HashMap<String, Vec<ShOutput>>,
        timings: &HashMap<String, super::runtime::ShTimings>,
    ) -> Result<()> {
        if outputs.is_empty() {
            return Ok(());
        }

        let db = crate::db::open(&self.db_path)?;

        for (job_id, sh_outputs) in outputs {
            let job_timings = timings.get(job_id);
            let job_dir = self.path().join("jobs").join(job_id);

            for (i, output) in sh_outputs.iter().enumerate() {
                let (n, started_at, finished_at) = job_timings
                    .and_then(|t| t.get(i))
                    .copied()
                    .unwrap_or_else(|| {
                        // Fallback if timing wasn't captured (shouldn't happen).
                        let n = i + 1;
                        let now = jiff::Timestamp::now();
                        (n, now, now)
                    });

                // Write CRI log file.
                fs_err::create_dir_all(&job_dir)?;
                let sh_path = job_dir.join(format!("sh-{n}.log"));
                super::logs::write_cri_log(&sh_path, output, &started_at.to_string())?;

                // Insert sh event into the database.
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
        super::super::pipeline::compile(source, "ci.fnl").expect("compile should succeed")
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
            r#"(local ci (require :quire.ci))
(ci.job :pwd [:quire/push] (fn [{: sh}] (sh ["ls"])))"#,
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
            r#"(local ci (require :quire.ci))
(ci.job :a [:quire/push] (fn [{: sh}] (sh ["echo" "from-a"])))
(ci.job :b [:a] (fn [{: sh}] (sh ["echo" "from-b"])))"#,
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
            r#"(local ci (require :quire.ci))
(ci.job :b [:a] (fn [{{: sh}}] (sh (.. "echo b >> {log}"))))
(ci.job :a [:quire/push] (fn [{{: sh}}] (sh (.. "echo a >> {log}"))))"#,
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
            r#"(local ci (require :quire.ci))
(ci.job :a [:quire/push] (fn [_] (error "boom")))
(ci.job :b [:a] (fn [{: sh}] (sh ["echo" "should-not-run"])))"#,
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
            r#"(local ci (require :quire.ci))
(ci.job :grab [:quire/push]
  (fn [{: sh : jobs}]
    (let [push (jobs :quire/push)]
      (sh ["echo" push.sha push.ref]))))"#,
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
            r#"(local ci (require :quire.ci))
(ci.job :a [:quire/push] (fn [_] nil))
(ci.job :b [:a]
  (fn [{: sh : jobs}]
    (let [push (jobs :quire/push)]
      (sh ["echo" push.sha]))))"#,
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
            r#"(local ci (require :quire.ci))
(ci.job :grab [:quire/push] (fn [{: jobs}] (jobs :nope)))"#,
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
            r#"(local ci (require :quire.ci))
(ci.job :peer [:quire/push] (fn [_] nil))
(ci.job :grab [:quire/push] (fn [{: jobs}] (jobs :peer)))"#,
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
            r#"(local ci (require :quire.ci))
(ci.job :grab [:quire/push] (fn [{: jobs}] (jobs :grab)))"#,
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
            r#"(local ci (require :quire.ci))
(ci.job :a [:quire/push] (fn [_] nil))
(ci.job :b [:a]
  (fn [{: sh : jobs}]
    (let [a-outputs (jobs :a)]
      (sh ["echo" (tostring a-outputs)]))))"#,
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
            r#"(local ci (require :quire.ci))
(ci.job :greet [:quire/push] (fn [{: sh}] (sh ["echo" "hello"])))"#,
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
            r#"(local ci (require :quire.ci))
(ci.job :a [:quire/push] (fn [{: sh}] (sh ["echo" "from-a"])))
(ci.job :b [:a] (fn [_] (error "boom")))"#,
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
            r#"(local ci (require :quire.ci))
(ci.image "alpine")
(ci.job :bad [:quire/push]
  (fn [_]
    (ci.image "sneaky")))"#,
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
            r#"(local ci (require :quire.ci))
(ci.job :only [:quire/push] (fn [_] nil))"#,
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
            r#"(local ci (require :quire.ci))
(ci.job :boom [:quire/push] (fn [_] nil))"#,
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
}
