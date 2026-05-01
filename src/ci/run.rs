//! On-disk storage for CI runs.
//!
//! A run is a directory under `runs/<repo>/<state>/<id>/` containing
//! `meta.yml` (immutable) and `times.yml` (timestamps). The directory's
//! parent name is the authoritative state; transitions are atomic
//! `rename` operations.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::rc::Rc;

use jiff::Timestamp;
use mlua::IntoLua;

use super::lua::{Runtime, RuntimeHandle, ShOutput};
use super::pipeline::Pipeline;
use crate::secret::SecretString;
use crate::{Error, Result};

/// The state of a CI run.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RunState {
    Pending,
    Active,
    Complete,
    Failed,
}

impl RunState {
    /// The directory name used for this state in the run storage layout.
    pub fn dir_name(&self) -> &'static str {
        match self {
            RunState::Pending => "pending",
            RunState::Active => "active",
            RunState::Complete => "complete",
            RunState::Failed => "failed",
        }
    }
}

/// Immutable metadata for a CI run. Written once and never modified.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct RunMeta {
    /// The commit SHA that triggered this run.
    pub sha: String,
    /// The full ref name (e.g. `refs/heads/main`).
    pub r#ref: String,
    /// When the push occurred.
    pub pushed_at: Timestamp,
}

/// Timestamps recorded across the run lifecycle. The directory name is the
/// authoritative state; this file records when transitions happened.
#[derive(Clone, Debug, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct RunTimes {
    /// When the run was picked up (moved to active).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub started_at: Option<Timestamp>,
    /// When the run finished (moved to complete/failed).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub finished_at: Option<Timestamp>,
}

/// Access to CI runs for a single repo.
///
/// Owns the base path (`runs/<repo>/`) and provides run creation
/// and orphan reconciliation. Obtain one via `Ci::runs()`.
#[derive(Debug)]
pub struct Runs {
    base: PathBuf,
}

impl Runs {
    pub fn new(base: PathBuf) -> Self {
        Self { base }
    }

    /// Create a new run record in the `pending` state.
    ///
    /// Writes `meta.yml` and `times.yml` atomically (temp dir + rename).
    pub fn create(&self, meta: &RunMeta) -> Result<Run> {
        let pending_dir = self.base.join(RunState::Pending.dir_name());
        let id = uuid::Uuid::now_v7().to_string();

        fs_err::create_dir_all(&pending_dir)?;

        let tmp_dir = pending_dir.join(format!(".tmp-{id}"));
        fs_err::create_dir_all(&tmp_dir)?;

        write_yaml(&tmp_dir.join("meta.yml"), meta)?;
        write_yaml(&tmp_dir.join("times.yml"), &RunTimes::default())?;

        let final_dir = pending_dir.join(&id);
        fs_err::rename(&tmp_dir, &final_dir)?;

        Run::open(self.base.clone(), RunState::Pending, id)
    }

    /// Scan for orphaned runs in `pending/` and `active/` directories.
    ///
    /// Entries that cannot be opened (missing/unreadable `meta.yml` or
    /// `times.yml`) are quarantined to `failed/` so they don't stay
    /// stuck in pending/active forever.
    ///
    /// The caller decides how to reconcile the returned runs:
    /// - `pending/` entries should be re-enqueued.
    /// - `active/` entries with no live runner should be marked failed.
    pub fn scan_orphans(&self) -> Result<Vec<Run>> {
        let mut orphans = Vec::new();

        for &state in &[RunState::Pending, RunState::Active] {
            let state_path = self.base.join(state.dir_name());
            let entries = match fs_err::read_dir(&state_path) {
                Ok(entries) => entries,
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => continue,
                Err(e) => return Err(e.into()),
            };

            for entry in entries {
                let entry = entry?;
                let name = match entry.file_name().to_str() {
                    Some(n) => n.to_string(),
                    None => continue,
                };

                if name.starts_with('.') {
                    continue;
                }

                match Run::open(self.base.clone(), state, name.clone()) {
                    Ok(run) => orphans.push(run),
                    Err(e) => {
                        tracing::warn!(
                            state = ?state,
                            run_id = %name,
                            %e,
                            "quarantining unreadable run to failed/"
                        );
                        self.quarantine(&state_path.join(&name), &name)?;
                    }
                }
            }
        }

        Ok(orphans)
    }

    /// Move a broken run directory into `failed/` so it stops blocking
    /// pending/active. The contents may be unreadable; we only care
    /// about getting it out of the active state buckets.
    fn quarantine(&self, src: &Path, id: &str) -> Result<()> {
        let failed_dir = self.base.join(RunState::Failed.dir_name());
        fs_err::create_dir_all(&failed_dir)?;
        fs_err::rename(src, failed_dir.join(id))?;
        Ok(())
    }

    /// Reconcile orphaned runs from a previous server instance.
    ///
    /// - `pending/` orphans are moved to `complete/` (will be re-enqueued when
    ///   the runner exists; for now, immediately completed).
    /// - `active/` orphans are moved to `failed/` (no live runner).
    pub fn reconcile_orphans(&self) -> Result<()> {
        let orphans = self.scan_orphans()?;
        for orphan in &orphans {
            tracing::warn!(
                run_id = %orphan.id(),
                state = ?orphan.state(),
                "found orphaned run"
            );
        }

        for mut orphan in orphans {
            match orphan.state() {
                RunState::Pending => {
                    tracing::warn!(
                        run_id = %orphan.id(),
                        "completing orphaned pending run"
                    );
                    if let Err(e) = orphan.transition(RunState::Complete) {
                        tracing::error!(
                            run_id = %orphan.id(),
                            %e,
                            "failed to transition orphaned pending run"
                        );
                    }
                }
                RunState::Active => {
                    tracing::warn!(
                        run_id = %orphan.id(),
                        "marking orphaned active run as failed"
                    );
                    if let Err(e) = orphan.transition(RunState::Failed) {
                        tracing::error!(
                            run_id = %orphan.id(),
                            %e,
                            "failed to transition orphaned active run to failed"
                        );
                    }
                }
                RunState::Complete | RunState::Failed => {
                    unreachable!("scan_orphans only returns pending/active")
                }
            }
        }

        Ok(())
    }
}

/// A CI run on disk.
///
/// Owns the path to the run directory and the in-memory execution
/// state used while driving a pipeline. Tracks current state so that
/// `transition` can move the directory in one call.
pub struct Run {
    base: PathBuf,
    state: RunState,
    id: String,
}

impl Run {
    /// The resolved path to this run's directory on disk.
    pub fn path(&self) -> PathBuf {
        self.base.join(self.state.dir_name()).join(&self.id)
    }

    /// The run's ID.
    pub fn id(&self) -> &str {
        &self.id
    }

    /// The run's current state.
    pub fn state(&self) -> RunState {
        self.state
    }

    /// Open an existing run from disk.
    ///
    /// `state` is the directory the run is expected to be in (e.g.
    /// `pending/`, `active/`). Returns an error if `meta.yml` or
    /// `times.yml` are missing or unreadable.
    pub fn open(base: PathBuf, state: RunState, id: String) -> Result<Self> {
        let run = Self { base, state, id };
        run.read_meta()?;
        run.read_times()?;
        Ok(run)
    }

    /// Drive `pipeline` to completion through this run.
    ///
    /// Consumes the pipeline, taking ownership of its Lua VM. Constructs
    /// a fresh [`Runtime`] with `secrets`, the source outputs
    /// (`:quire/push` from `meta.yml`), and the per-job transitive-input
    /// sets; installs it on the VM, topo-sorts the jobs, transitions
    /// Pending → Active, then invokes each `run_fn` in dependency order
    /// with the runtime handle as its sole argument. Returns a map of
    /// job id → captured `(sh …)` outputs. The run finishes in
    /// `Complete` if every job's `run_fn` returned without error,
    /// otherwise `Failed`.
    ///
    /// Source-ref filtering (e.g. running only `quire/push`-reachable
    /// jobs) is not yet implemented; for now every validated job runs.
    pub fn execute(
        mut self,
        pipeline: Pipeline,
        secrets: HashMap<String, SecretString>,
    ) -> Result<HashMap<String, Vec<ShOutput>>> {
        let meta = self.read_meta()?;

        let runtime = Rc::new(Runtime::new(pipeline, secrets, &meta));

        let lua = runtime.lua();
        let rt_value = RuntimeHandle(runtime.clone())
            .into_lua(lua)
            .expect("install runtime on Lua VM");

        self.transition(RunState::Active)?;

        for job_id in runtime.topo_order() {
            let run_fn = runtime
                .job(job_id)
                .expect("topo_order returned a job id not in pipeline")
                .run_fn
                .clone();

            runtime.enter_job(job_id);
            let result = run_fn.call::<mlua::Value>(rt_value.clone());
            runtime.leave_job();

            if let Err(e) = result {
                lua.remove_app_data::<Rc<Runtime>>();
                self.transition(RunState::Failed)?;
                return Err(Error::JobFailed {
                    job: job_id.to_string(),
                    source: Box::new(e),
                });
            }
        }

        let outputs = runtime.take_outputs();
        lua.remove_app_data::<Rc<Runtime>>();
        self.transition(RunState::Complete)?;
        Ok(outputs)
    }

    /// Transition the run from its current state to a new state.
    ///
    /// Moves the run directory between state parent directories and stamps
    /// `started_at` (entering Active) or `finished_at` (entering Complete or
    /// Failed) on `times.yml`. Each timestamp is set at most once.
    pub fn transition(&mut self, to: RunState) -> Result<()> {
        use RunState::*;
        // Allowed transitions. Pending->Complete is the orphan-reconcile
        // placeholder; everything else is the normal trigger lifecycle.
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

        let src = self.path();
        let dst_parent = self.base.join(to.dir_name());

        if !src.exists() {
            return Err(Error::Io(std::io::Error::new(
                std::io::ErrorKind::NotFound,
                format!("run directory not found: {}", src.display()),
            )));
        }

        fs_err::create_dir_all(&dst_parent)?;
        let dst = dst_parent.join(&self.id);
        fs_err::rename(&src, &dst)?;
        self.state = to;

        let mut times = self.read_times()?;
        let now = Timestamp::now();
        match to {
            RunState::Active if times.started_at.is_none() => times.started_at = Some(now),
            RunState::Complete | RunState::Failed if times.finished_at.is_none() => {
                times.finished_at = Some(now)
            }
            _ => {}
        }
        self.write_times(&times)?;
        Ok(())
    }

    /// Read the timestamps recorded for this run.
    pub fn read_times(&self) -> Result<RunTimes> {
        read_yaml(&self.path().join("times.yml"))
    }

    /// Read the immutable metadata for this run.
    pub fn read_meta(&self) -> Result<RunMeta> {
        read_yaml(&self.path().join("meta.yml"))
    }

    /// Update the timestamps for this run (atomic write).
    pub fn write_times(&self, times: &RunTimes) -> Result<()> {
        write_yaml(&self.path().join("times.yml"), times)
    }
}

/// Write a serializable value to a YAML file atomically (temp file + rename).
pub(crate) fn write_yaml<T: serde::Serialize>(path: &Path, value: &T) -> Result<()> {
    let tmp_path = path.with_extension("yml.tmp");
    let f = fs_err::File::create(&tmp_path)?;
    serde_yaml_ng::to_writer(std::io::BufWriter::new(f), value)?;
    fs_err::rename(&tmp_path, path)?;
    Ok(())
}

/// Read a deserializable value from a YAML file.
fn read_yaml<T: serde::de::DeserializeOwned>(path: &Path) -> Result<T> {
    let f = fs_err::File::open(path)?;
    Ok(serde_yaml_ng::from_reader(std::io::BufReader::new(f))?)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Quire;

    fn tmp_quire() -> (tempfile::TempDir, Quire) {
        let dir = tempfile::tempdir().expect("tempdir");
        let quire = Quire::new(dir.path().to_path_buf());
        (dir, quire)
    }

    fn test_runs(quire: &Quire) -> Runs {
        Runs::new(quire.base_dir().join("runs").join("test.git"))
    }

    fn test_meta() -> RunMeta {
        RunMeta {
            sha: "abc123".to_string(),
            r#ref: "refs/heads/main".to_string(),
            pushed_at: "2026-04-28T12:00:00Z".parse().expect("parse timestamp"),
        }
    }

    #[test]
    fn run_state_dir_name() {
        assert_eq!(RunState::Pending.dir_name(), "pending");
        assert_eq!(RunState::Active.dir_name(), "active");
        assert_eq!(RunState::Complete.dir_name(), "complete");
        assert_eq!(RunState::Failed.dir_name(), "failed");
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
    fn create_writes_files_in_pending() {
        let (_dir, quire) = tmp_quire();
        let runs = test_runs(&quire);
        let run = runs.create(&test_meta()).expect("create");

        let path = run.path();
        assert!(path.exists(), "run directory should exist");
        assert!(path.join("meta.yml").exists());
        assert!(path.join("times.yml").exists());
        assert_eq!(run.state(), RunState::Pending);

        let meta = run.read_meta().expect("read meta");
        assert_eq!(meta.sha, "abc123");

        let state = run.read_times().expect("read state");
        assert!(state.started_at.is_none());
        assert!(state.finished_at.is_none());
    }

    #[test]
    fn transition_moves_directory() {
        let (_dir, quire) = tmp_quire();
        let runs = test_runs(&quire);
        let mut run = runs.create(&test_meta()).expect("create");
        let id = run.id().to_string();

        let old_path = run.path();
        run.transition(RunState::Active).expect("transition");

        assert!(!old_path.exists(), "pending dir should be gone");
        assert_eq!(run.state(), RunState::Active);

        let new_path = run.path();
        assert!(new_path.exists(), "active dir should exist");

        // Meta is byte-identical after move.
        let meta = run.read_meta().expect("read meta");
        assert_eq!(meta.sha, "abc123");
        assert_eq!(run.id(), id);
    }

    #[test]
    fn transition_stamps_started_at_on_active() {
        let (_dir, quire) = tmp_quire();
        let runs = test_runs(&quire);
        let mut run = runs.create(&test_meta()).expect("create");

        run.transition(RunState::Active).expect("to active");
        let times = run.read_times().expect("read state");
        assert!(times.started_at.is_some(), "started_at should be stamped");
        assert!(times.finished_at.is_none());
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
        let times = completed.read_times().expect("read state");
        assert!(times.finished_at.is_some());

        let mut failed = runs.create(&test_meta()).expect("create");
        failed.transition(RunState::Active).expect("to active");
        failed.transition(RunState::Failed).expect("to failed");
        let failed_times = failed.read_times().expect("read state");
        assert!(failed_times.finished_at.is_some());
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
        let active_times = run.read_times().expect("read state");
        let started = active_times.started_at;

        run.transition(RunState::Complete).expect("to complete");
        let complete_times = run.read_times().expect("read state");
        assert_eq!(complete_times.started_at, started, "started_at preserved");
    }

    #[test]
    fn transition_full_lifecycle() {
        let (_dir, quire) = tmp_quire();
        let runs = test_runs(&quire);
        let mut run = runs.create(&test_meta()).expect("create");

        run.transition(RunState::Active).expect("to active");
        run.transition(RunState::Complete).expect("to complete");

        assert_eq!(run.state(), RunState::Complete);
        assert!(run.path().exists());
    }

    #[test]
    fn transition_errors_on_missing_source() {
        let mut run = Run {
            base: PathBuf::from("/tmp/quire-test-runs/test.git"),
            state: RunState::Pending,
            id: uuid::Uuid::now_v7().to_string(),
        };

        let result = run.transition(RunState::Active);
        assert!(result.is_err());
    }

    #[test]
    fn scan_orphans_finds_pending() {
        let (_dir, quire) = tmp_quire();
        let runs = test_runs(&quire);
        let run = runs.create(&test_meta()).expect("create");

        let orphans = runs.scan_orphans().expect("scan");
        assert_eq!(orphans.len(), 1);
        assert_eq!(orphans[0].id(), run.id());
        assert_eq!(orphans[0].state(), RunState::Pending);
    }

    #[test]
    fn scan_orphans_finds_active() {
        let (_dir, quire) = tmp_quire();
        let runs = test_runs(&quire);
        let mut run = runs.create(&test_meta()).expect("create");
        run.transition(RunState::Active).expect("transition");

        let orphans = runs.scan_orphans().expect("scan");
        assert_eq!(orphans.len(), 1);
        assert_eq!(orphans[0].state(), RunState::Active);
    }

    #[test]
    fn scan_orphans_skips_complete() {
        let (_dir, quire) = tmp_quire();
        let runs = test_runs(&quire);
        let mut run = runs.create(&test_meta()).expect("create");
        run.transition(RunState::Complete).expect("transition");

        let orphans = runs.scan_orphans().expect("scan");
        assert!(orphans.is_empty(), "complete runs are not orphans");
    }

    #[test]
    fn scan_orphans_quarantines_unreadable_runs() {
        let (_dir, quire) = tmp_quire();
        let base = quire.base_dir().join("runs").join("test.git");
        let runs = Runs::new(base.clone());

        // Create a run, then break it by removing meta.yml.
        let run = runs.create(&test_meta()).expect("create");
        let id = run.id().to_string();
        fs_err::remove_file(run.path().join("meta.yml")).expect("remove meta");

        let orphans = runs.scan_orphans().expect("scan");
        assert!(orphans.is_empty(), "broken run should not be returned");

        let pending = base.join(RunState::Pending.dir_name()).join(&id);
        assert!(!pending.exists(), "broken run should leave pending/");

        let failed = base.join(RunState::Failed.dir_name()).join(&id);
        assert!(failed.exists(), "broken run should land in failed/");
    }

    #[test]
    fn scan_orphans_empty_when_no_runs_dir() {
        let (_dir, quire) = tmp_quire();
        let runs = test_runs(&quire);
        assert!(runs.scan_orphans().expect("scan").is_empty());
    }

    #[test]
    fn write_times_updates_in_place() {
        let (_dir, quire) = tmp_quire();
        let runs = test_runs(&quire);
        let run = runs.create(&test_meta()).expect("create");

        let started: Timestamp = "2026-04-28T12:00:01Z".parse().expect("parse");
        run.write_times(&RunTimes {
            started_at: Some(started),
            finished_at: None,
        })
        .expect("write state");

        let loaded = run.read_times().expect("read state");
        assert_eq!(loaded.started_at, Some(started));

        // Meta is unchanged.
        let loaded_meta = run.read_meta().expect("read meta");
        assert_eq!(loaded_meta, test_meta());
    }

    #[test]
    fn reconcile_completes_pending_orphans() {
        let (_dir, quire) = tmp_quire();
        let runs = test_runs(&quire);
        let run = runs.create(&test_meta()).expect("create");
        let id = run.id().to_string();

        runs.reconcile_orphans().expect("reconcile");

        // Pending orphan should be moved to complete.
        let completed = runs.base.join(RunState::Complete.dir_name()).join(&id);
        assert!(completed.exists(), "orphan should be in complete/");
        let pending = runs.base.join(RunState::Pending.dir_name()).join(&id);
        assert!(!pending.exists(), "orphan should not be in pending/");
    }

    #[test]
    fn reconcile_fails_active_orphans() {
        let (_dir, quire) = tmp_quire();
        let runs = test_runs(&quire);
        let mut run = runs.create(&test_meta()).expect("create");
        run.transition(RunState::Active).expect("to active");
        let id = run.id().to_string();

        runs.reconcile_orphans().expect("reconcile");

        // Active orphan should be moved to failed.
        let failed = runs.base.join(RunState::Failed.dir_name()).join(&id);
        assert!(failed.exists(), "orphan should be in failed/");
    }

    fn load(source: &str) -> Pipeline {
        super::super::pipeline::Pipeline::load(source, "ci.fnl").expect("load should succeed")
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
        let outputs = run.execute(pipeline, HashMap::new()).expect("execute");

        // Verify the run landed in complete/ on disk.
        let completed = runs.base.join(RunState::Complete.dir_name()).join(&run_id);
        assert!(completed.exists(), "run should be in complete/");

        let a = &outputs["a"];
        let b = &outputs["b"];
        assert_eq!(a.len(), 1);
        assert_eq!(a[0].stdout, "from-a\n");
        assert_eq!(b.len(), 1);
        assert_eq!(b[0].stdout, "from-b\n");
    }

    #[test]
    fn execute_runs_jobs_in_topo_order() {
        // `b` depends on `a`, but the registration order puts `b` first.
        // Topo-sorted execution must run `a` before `b`.
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

        run.execute(pipeline, HashMap::new()).expect("execute");

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
            .execute(pipeline, HashMap::new())
            .expect_err("expected failure");
        assert!(matches!(err, Error::JobFailed { ref job, .. } if job == "a"));

        // Verify the run landed in failed/ on disk.
        let failed = runs.base.join(RunState::Failed.dir_name()).join(&run_id);
        assert!(failed.exists(), "run should be in failed/");
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

        let outputs = run.execute(pipeline, HashMap::new()).expect("execute");

        let grab = &outputs["grab"];
        assert_eq!(grab.len(), 1);
        assert_eq!(grab[0].stdout, "abc123 refs/heads/main\n");
    }

    #[test]
    fn jobs_returns_quire_push_outputs_through_transitive_input() {
        // `b` depends on `a` which depends on `:quire/push`; `b` reads
        // `:quire/push` directly even though it's not a direct input.
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

        let outputs = run.execute(pipeline, HashMap::new()).expect("execute");

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
            .execute(pipeline, HashMap::new())
            .expect_err("expected failure");
        match err {
            Error::JobFailed { job, source } => {
                assert_eq!(job, "grab");
                let msg = source.to_string();
                assert!(
                    msg.contains("not in transitive inputs") && msg.contains("nope"),
                    "expected 'not in transitive inputs' error, got: {msg}"
                );
            }
            other => panic!("expected JobFailed, got: {other:?}"),
        }
    }

    #[test]
    fn jobs_errors_on_non_ancestor_job() {
        // `peer` exists as a job but isn't an ancestor of `grab`.
        let (_dir, quire) = tmp_quire();
        let runs = test_runs(&quire);
        let run = runs.create(&test_meta()).expect("create");

        let pipeline = load(
            r#"(local ci (require :quire.ci))
(ci.job :peer [:quire/push] (fn [_] nil))
(ci.job :grab [:quire/push] (fn [{: jobs}] (jobs :peer)))"#,
        );

        let err = run
            .execute(pipeline, HashMap::new())
            .expect_err("expected failure");
        match err {
            Error::JobFailed { source, .. } => {
                let msg = source.to_string();
                assert!(
                    msg.contains("not in transitive inputs") && msg.contains("peer"),
                    "expected non-ancestor error, got: {msg}"
                );
            }
            other => panic!("expected JobFailed, got: {other:?}"),
        }
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
            .execute(pipeline, HashMap::new())
            .expect_err("expected failure");
        match err {
            Error::JobFailed { source, .. } => {
                let msg = source.to_string();
                assert!(
                    msg.contains("cannot read its own outputs"),
                    "expected self-lookup error, got: {msg}"
                );
            }
            other => panic!("expected JobFailed, got: {other:?}"),
        }
    }

    #[test]
    fn jobs_returns_nil_for_dependency_with_no_outputs() {
        let (_dir, quire) = tmp_quire();
        let runs = test_runs(&quire);
        let run = runs.create(&test_meta()).expect("create");

        // `a` does nothing, `b` reads `a`'s outputs — should get nil.
        let pipeline = load(
            r#"(local ci (require :quire.ci))
(ci.job :a [:quire/push] (fn [_] nil))
(ci.job :b [:a]
  (fn [{: sh : jobs}]
    (let [a-outputs (jobs :a)]
      (sh ["echo" (tostring a-outputs)]))))"#,
        );

        let outputs = run.execute(pipeline, HashMap::new()).expect("execute");
        let b = &outputs["b"];
        assert_eq!(b.len(), 1);
        assert_eq!(b[0].stdout, "nil\n");
    }
}
