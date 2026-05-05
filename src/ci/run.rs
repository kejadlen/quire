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

use super::pipeline::{Pipeline, RunFn};
use super::runtime::{ExecutorRuntime, Runtime, RuntimeHandle, ShOutput};
use crate::display_chain;
use crate::secret::SecretString;
use crate::{Error, Result};

/// The execution mode for a run. Host runs `sh` directly on the host.
/// Docker materializes a container and routes `sh` through `docker exec`.
#[derive(Debug, Clone)]
pub enum Executor {
    Host,
    Docker,
}

/// Owns a [`ContainerSession`](crate::ci::docker::ContainerSession)
/// alongside the run-dir's `container.yml` path so [`Drop`] can stamp
/// `container_stopped_at` *before* the session itself drops and fires
/// `docker stop`.
///
/// Field declaration order matters: `session` is declared first so it
/// drops first after this struct's custom `Drop` body returns. The
/// effect is: write `container_stopped_at` → drop `session` →
/// `docker stop`.
pub(super) struct DockerLifecycle {
    pub(super) session: crate::ci::docker::ContainerSession,
    record_path: PathBuf,
    pub(super) work_dir: String,
}

impl Drop for DockerLifecycle {
    fn drop(&mut self) {
        // Stamp `container_stopped_at` before ContainerSession's Drop
        // (`docker stop`) fires. Errors are logged and swallowed —
        // Drop cannot return Result.
        match read_yaml::<ContainerRecord>(&self.record_path) {
            Ok(mut rec) => {
                rec.container_stopped_at = Some(Timestamp::now());
                if let Err(e) = write_yaml(&self.record_path, &rec) {
                    tracing::error!(
                        error = %display_chain(&e),
                        "failed to write container_stopped_at"
                    );
                }
            }
            Err(e) => tracing::error!(
                error = %display_chain(&e),
                "failed to read container.yml before stop"
            ),
        }
        // After this body returns, fields drop in declaration order:
        // `session` drops first → ContainerSession::Drop → docker stop.
    }
}

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

/// Container metadata for a docker-mode run, persisted to
/// `<run-dir>/container.yml`. Each field is populated incrementally as
/// the lifecycle progresses; absence implies "not yet (or never)
/// reached." Host-mode runs do not write this file.
#[derive(Clone, Debug, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct ContainerRecord {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub image_tag: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub container_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub build_started_at: Option<Timestamp>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub build_finished_at: Option<Timestamp>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub container_started_at: Option<Timestamp>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub container_stopped_at: Option<Timestamp>,
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

        // Set the latest symlink after opening the run so it can do it.
        let run = Run::open(self.base.clone(), RunState::Pending, id)?;
        run.update_latest()?;
        Ok(run)
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
                Err(e) => return Err(e.into()), // cov-excl-line
            };

            for entry in entries {
                let entry = entry?;
                let name = match entry.file_name().to_str() {
                    Some(n) => n.to_string(),
                    None => continue, // cov-excl-line
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
                            error = %display_chain(&e),
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
                run_id = %orphan.id(), // cov-excl-line
                state = ?orphan.state(), // cov-excl-line
                "found orphaned run"
            );
        }

        for mut orphan in orphans {
            match orphan.state() {
                RunState::Pending => {
                    tracing::warn!(
                        run_id = %orphan.id(), // cov-excl-line
                        "completing orphaned pending run"
                    );
                    if let Err(e) = orphan.transition(RunState::Complete) {
                        tracing::error!(
                            run_id = %orphan.id(), // cov-excl-line
                            error = %display_chain(&e),
                            "failed to transition orphaned pending run"
                        );
                    }
                }
                RunState::Active => {
                    tracing::warn!(
                        run_id = %orphan.id(), // cov-excl-line
                        "marking orphaned active run as failed"
                    );
                    if let Err(e) = orphan.transition(RunState::Failed) {
                        tracing::error!(
                            run_id = %orphan.id(), // cov-excl-line
                            error = %display_chain(&e),
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
        executor: Executor,
    ) -> Result<HashMap<String, Vec<ShOutput>>> {
        let meta = self.read_meta()?;

        // Transition to Active *before* building/starting the
        // container. The docker build can take a long time and
        // happens with the run in `active/` so the on-disk state
        // accurately reflects "this run is in progress." It also
        // pins `self.path()` for the lifetime of the run, so the
        // `container.yml` path captured by `DockerLifecycle` stays
        // valid until just before the final Complete/Failed
        // transition (where we explicitly drop the runtime first).
        self.transition(RunState::Active)?;

        let executor_runtime = match self.build_executor_runtime(executor, workspace) {
            Ok(rt) => rt,
            Err(e) => {
                self.transition(RunState::Failed)?;
                return Err(e);
            }
        };

        let runtime = Rc::new(Runtime::new(
            pipeline,
            secrets,
            &meta,
            git_dir,
            workspace.to_path_buf(),
            executor_runtime,
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

            runtime.enter_job(job_id);
            let result: Result<()> = (|| match run_fn {
                RunFn::Lua(f) => {
                    let _: mlua::Value = f.call(rt_value.clone())?;
                    Ok(())
                }
                RunFn::Rust(f) => f(&runtime),
            })();
            runtime.leave_job();

            if let Err(e) = result {
                failed_job = Some((job_id.to_string(), e));
                break;
            }
        }

        // Always drain outputs and write logs, even on failure — the
        // jobs that did run before the failure are useful context.
        let outputs = runtime.take_outputs();
        lua.remove_app_data::<Rc<Runtime>>();

        self.write_all_logs(&outputs)?;

        // Drop the runtime *before* the final transition. In docker
        // mode this fires `DockerLifecycle::drop`, which stamps
        // `container_stopped_at` in `<run-dir>/container.yml`. The
        // path it captured is still valid here (the run is in
        // active/); the subsequent transition moves the file into
        // place with the rest of the run dir.
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

    /// Build the per-run container if `executor` is `Docker`, writing
    /// `container.yml` incrementally as each phase completes. Run must
    /// already be in `active/` so `self.path()` is stable for the
    /// lifetime of the returned [`DockerLifecycle`].
    fn build_executor_runtime(
        &self,
        executor: Executor,
        workspace: &std::path::Path,
    ) -> Result<ExecutorRuntime> {
        match executor {
            Executor::Host => Ok(ExecutorRuntime::Host),
            Executor::Docker => {
                let mut record = ContainerRecord::default();

                // Build phase.
                record.build_started_at = Some(Timestamp::now());
                self.write_container_record(&record)?;

                let dockerfile = workspace.join(".quire/Dockerfile");
                let tag = format!("quire-ci/{}:{}", repo_segment(&self.base), self.id);

                crate::ci::docker::docker_build(&dockerfile, workspace, &tag)?;

                record.image_tag = Some(tag.clone());
                record.build_finished_at = Some(Timestamp::now());
                self.write_container_record(&record)?;

                // Start phase. The bind-mount target inside the
                // container doubles as the working directory for every
                // `(sh …)` invocation routed through `docker exec`.
                const WORK_DIR: &str = "/work";
                let session =
                    crate::ci::docker::ContainerSession::start(&tag, workspace, WORK_DIR)?;

                record.container_id = Some(session.container_id.clone());
                record.container_started_at = Some(session.container_started_at);
                self.write_container_record(&record)?;

                Ok(ExecutorRuntime::Docker(DockerLifecycle {
                    session,
                    record_path: self.path().join("container.yml"),
                    work_dir: WORK_DIR.to_string(),
                }))
            }
        }
    }

    /// Write per-job log files from the captured `(sh …)` outputs.
    ///
    /// Creates `jobs/<job-id>/log.yml` in the run directory for each
    /// job that has outputs. The file contains a YAML list of `ShOutput`
    /// entries — command, exit code, stdout, stderr — one per `(sh …)`
    /// call. Written before the final state transition so logs are
    /// available for both successful and failed runs.
    fn write_all_logs(&self, outputs: &HashMap<String, Vec<ShOutput>>) -> Result<()> {
        for (job_id, sh_outputs) in outputs {
            if sh_outputs.is_empty() {
                continue;
            }
            let job_dir = self.path().join("jobs").join(job_id);
            fs_err::create_dir_all(&job_dir)?;
            write_yaml(&job_dir.join("log.yml"), sh_outputs)?;
        }
        Ok(())
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
            _ => {} // cov-excl-line
        }
        self.write_times(&times)?;
        self.update_latest()?;
        Ok(())
    }

    /// Atomically update the `latest` symlink to point at this run.
    fn update_latest(&self) -> Result<()> {
        let latest = self.base.join("latest");
        let link_target = PathBuf::from(self.state.dir_name()).join(&self.id);
        let tmp_link = self.base.join(".tmp-latest");
        let _ = fs_err::remove_file(&tmp_link);
        std::os::unix::fs::symlink(&link_target, &tmp_link)?;
        let _ = fs_err::remove_file(&latest);
        fs_err::rename(&tmp_link, &latest)?;
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

    /// Read this run's `container.yml` record. Returns the deserialized
    /// `ContainerRecord`. Errors if the file is missing or malformed —
    /// callers should use `path().join("container.yml").exists()` if they
    /// want to handle the absent case as "host mode."
    pub fn read_container_record(&self) -> Result<ContainerRecord> {
        read_yaml(&self.path().join("container.yml"))
    }

    /// Atomically write this run's `container.yml` record (temp file +
    /// rename). Each call replaces the file; partial fields are
    /// represented as `None` and skipped from the output.
    pub fn write_container_record(&self, record: &ContainerRecord) -> Result<()> {
        write_yaml(&self.path().join("container.yml"), record)
    }
}

/// Take the final path component of a runs base (`runs/<repo>/`) for
/// use as the tag segment in `quire-ci/<segment>:<id>`. Falls back to
/// `repo` when the path has no name or it isn't UTF-8.
fn repo_segment(base: &Path) -> String {
    base.file_name()
        .and_then(|s| s.to_str())
        .map(str::to_owned)
        .unwrap_or_else(|| "repo".to_string())
}

/// Materialize a working tree at `sha` into `workspace` via
/// `git archive | tar -x`. Creates the workspace dir if needed.
pub fn materialize_workspace(
    git_dir: &Path,
    sha: &str,
    workspace: &Path,
) -> Result<()> {
    use std::process::{Command, Stdio};

    fs_err::create_dir_all(workspace)?;

    let mut archive = Command::new("git")
        .arg("--git-dir")
        .arg(git_dir)
        .args(["archive", sha])
        .stdout(Stdio::piped())
        .spawn()?;
    let archive_stdout = archive.stdout.take().expect("piped stdout");

    let mut tar = Command::new("tar")
        .args(["-x", "-C"])
        .arg(workspace)
        .stdin(Stdio::from(archive_stdout))
        .spawn()?;

    let tar_status = tar.wait()?;
    let archive_status = archive.wait()?;
    if !archive_status.success() || !tar_status.success() {
        return Err(Error::WorkspaceMaterializationFailed {
            source: std::io::Error::other(format!(
                "git archive exited {archive_status}, tar exited {tar_status}"
            )),
        });
    }
    Ok(())
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
    fn create_symlinks_latest() {
        let (_dir, quire) = tmp_quire();
        let runs = test_runs(&quire);
        let mut run = runs.create(&test_meta()).expect("create");

        let latest = runs.base.join("latest");
        assert!(latest.is_symlink(), "latest should be a symlink");
        let target = fs_err::read_link(&latest).expect("read link");
        assert_eq!(
            target,
            PathBuf::from(RunState::Pending.dir_name()).join(run.id())
        );
        assert!(latest.exists(), "latest should resolve to a real directory");

        // Symlink should follow through transitions.
        run.transition(RunState::Active).expect("to active");
        let target = fs_err::read_link(&latest).expect("read link");
        assert_eq!(
            target,
            PathBuf::from(RunState::Active.dir_name()).join(run.id())
        );
        assert!(latest.exists(), "latest should resolve after transition");
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
    fn transition_keeps_existing_started_at() {
        let (_dir, quire) = tmp_quire();
        let runs = test_runs(&quire);
        let mut run = runs.create(&test_meta()).expect("create");

        // Pre-stamp started_at before transitioning to Active.
        let pre: Timestamp = "2026-04-28T12:00:00Z".parse().unwrap();
        run.write_times(&RunTimes {
            started_at: Some(pre),
            finished_at: None,
        })
        .expect("write times");

        run.transition(RunState::Active).expect("to active");

        let times = run.read_times().expect("read times");
        assert_eq!(
            times.started_at,
            Some(pre),
            "should keep pre-set started_at"
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
    fn scan_orphans_skips_dot_prefixed_entries() {
        let (_dir, quire) = tmp_quire();
        let runs = test_runs(&quire);
        let run = runs.create(&test_meta()).expect("create");

        // Drop a dot-prefixed directory into pending/ alongside the real run.
        let pending_dir = runs.base.join(RunState::Pending.dir_name());
        fs_err::create_dir_all(pending_dir.join(".tmp-stale")).expect("mkdir dot");

        let orphans = runs.scan_orphans().expect("scan");
        assert_eq!(orphans.len(), 1);
        assert_eq!(orphans[0].id(), run.id());
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
                Executor::Host,
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
                Executor::Host,
            )
            .expect("execute");

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

        run.execute(
            pipeline,
            HashMap::new(),
            std::path::Path::new("."),
            &test_workspace(&quire),
            Executor::Host,
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
                Executor::Host,
            )
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

        let outputs = run
            .execute(
                pipeline,
                HashMap::new(),
                std::path::Path::new("."),
                &test_workspace(&quire),
                Executor::Host,
            )
            .expect("execute");

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

        let outputs = run
            .execute(
                pipeline,
                HashMap::new(),
                std::path::Path::new("."),
                &test_workspace(&quire),
                Executor::Host,
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
                Executor::Host,
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
            .execute(
                pipeline,
                HashMap::new(),
                std::path::Path::new("."),
                &test_workspace(&quire),
                Executor::Host,
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
                Executor::Host,
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

        // `a` does nothing, `b` reads `a`'s outputs — should get nil.
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
                Executor::Host,
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
            Executor::Host,
        )
        .expect("execute");

        let log_path = runs
            .base
            .join(RunState::Complete.dir_name())
            .join(&run_id)
            .join("jobs")
            .join("greet")
            .join("log.yml");
        assert!(log_path.exists(), "job log file should exist");

        let entries: Vec<ShOutput> =
            serde_yaml_ng::from_str(&fs_err::read_to_string(&log_path).expect("read log"))
                .expect("parse log");
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].exit, 0);
        assert_eq!(entries[0].stdout, "hello\n");
        assert!(entries[0].stderr.is_empty());
        assert_eq!(entries[0].cmd, "[\"echo\", \"hello\"]");
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
            Executor::Host,
        );

        let failed_dir = runs.base.join(RunState::Failed.dir_name()).join(&run_id);
        assert!(failed_dir.exists(), "run should be in failed/");

        let log_path = failed_dir.join("jobs").join("a").join("log.yml");
        assert!(
            log_path.exists(),
            "job 'a' log should exist even though 'b' failed"
        );

        let entries: Vec<ShOutput> =
            serde_yaml_ng::from_str(&fs_err::read_to_string(&log_path).expect("read log"))
                .expect("parse log");
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].stdout, "from-a\n");
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
                Executor::Host,
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
            Executor::Host,
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
            Err(crate::Error::Git("simulated rust failure".into()))
        })));

        let err = run
            .execute(
                pipeline,
                HashMap::new(),
                std::path::Path::new("."),
                &test_workspace(&quire),
                Executor::Host,
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
    fn container_record_round_trips_through_yaml() {
        let (_dir, quire) = tmp_quire();
        let runs = test_runs(&quire);
        let run = runs.create(&test_meta()).expect("create");

        let now: Timestamp = "2026-05-04T16:20:01Z".parse().expect("parse");
        let later: Timestamp = "2026-05-04T16:21:09Z".parse().expect("parse");
        let record = ContainerRecord {
            image_tag: Some("quire-ci/test:run-id".into()),
            container_id: Some("9f3b8a72c1d4".into()),
            build_started_at: Some(now),
            build_finished_at: Some(later),
            container_started_at: Some(later),
            container_stopped_at: None,
        };
        run.write_container_record(&record).expect("write");

        let read = run.read_container_record().expect("read");
        assert_eq!(read, record);
    }

    #[test]
    fn repo_segment_returns_final_component() {
        assert_eq!(repo_segment(Path::new("runs/test.git")), "test.git");
        assert_eq!(repo_segment(Path::new("/var/lib/quire/runs/repo.git")), "repo.git");
        assert_eq!(repo_segment(Path::new("")), "repo");
    }

    #[test]
    #[ignore = "requires docker"]
    fn execute_docker_mode_runs_jobs_in_container() {
        if !crate::ci::docker::is_available() {
            return;
        }

        // Build a real git repo with a Dockerfile committed at HEAD.
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
        for cmd in [vec!["init", "-b", "main"]] {
            let out = std::process::Command::new("git")
                .args(&cmd)
                .current_dir(&src_repo)
                .envs(env_vars)
                .output()
                .expect("git");
            assert!(out.status.success());
        }
        fs_err::create_dir_all(src_repo.join(".quire")).expect("mkdir .quire");
        fs_err::write(
            src_repo.join(".quire/Dockerfile"),
            "FROM alpine:3.19\n",
        )
        .expect("write Dockerfile");
        for cmd in [vec!["add", "."], vec!["commit", "-m", "initial"]] {
            let out = std::process::Command::new("git")
                .args(&cmd)
                .current_dir(&src_repo)
                .envs(env_vars)
                .output()
                .expect("git");
            assert!(out.status.success());
        }
        let sha = String::from_utf8(
            std::process::Command::new("git")
                .args(["rev-parse", "HEAD"])
                .current_dir(&src_repo)
                .envs(env_vars)
                .output()
                .expect("rev-parse")
                .stdout,
        )
        .unwrap()
        .trim()
        .to_string();

        let workspace = dir.path().join("ws");
        materialize_workspace(&src_repo.join(".git"), &sha, &workspace).expect("materialize");

        let (_qd, quire) = tmp_quire();
        let runs = test_runs(&quire);
        let meta = RunMeta {
            sha,
            r#ref: "refs/heads/main".to_string(),
            pushed_at: "2026-05-04T12:00:00Z".parse().unwrap(),
        };
        let run = runs.create(&meta).expect("create");
        let run_id = run.id().to_string();

        // Run `uname -s` inside the container. On macOS `uname -s`
        // returns `Darwin`; getting `Linux` back proves the command
        // ran inside the alpine container, not on the host.
        let pipeline = load(
            r#"(local ci (require :quire.ci))
(ci.job :probe [:quire/push] (fn [{: sh}] (sh ["uname" "-s"])))"#,
        );

        let outputs = run
            .execute(
                pipeline,
                HashMap::new(),
                &src_repo.join(".git"),
                &workspace,
                Executor::Docker,
            )
            .expect("execute");

        let probe = &outputs["probe"];
        assert_eq!(probe.len(), 1);
        assert_eq!(
            probe[0].stdout.trim(),
            "Linux",
            "expected uname -s to return Linux from inside the container, got: {:?}",
            probe[0].stdout,
        );

        // Verify container.yml was written with all fields.
        let complete = runs.base.join(RunState::Complete.dir_name()).join(&run_id);
        let record_path = complete.join("container.yml");
        assert!(record_path.exists(), "container.yml should exist");
        let record: ContainerRecord =
            serde_yaml_ng::from_str(&fs_err::read_to_string(&record_path).unwrap()).unwrap();
        assert!(record.image_tag.is_some());
        assert!(record.container_id.is_some());
        assert!(record.build_started_at.is_some());
        assert!(record.build_finished_at.is_some());
        assert!(record.container_started_at.is_some());
        assert!(record.container_stopped_at.is_some());

        // Cleanup the image we built.
        if let Some(tag) = record.image_tag {
            let _ = std::process::Command::new("docker")
                .args(["image", "rm", &tag])
                .output();
        }
    }
}
