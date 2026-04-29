use std::path::{Path, PathBuf};

use crate::event::{PushEvent, PushRef};
use crate::fennel::{Fennel, FennelError};
use crate::quire::Repo;
use crate::{Error, Result};

/// The state of a CI run.
#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
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
    /// ISO 8601 timestamp of when the push occurred.
    pub pushed_at: String,
}

/// Mutable state for a CI run. Updated throughout the run lifecycle.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct RunStateFile {
    /// Current status of the run.
    pub status: RunState,
    /// ISO 8601 timestamp of when the run was picked up (moved to active).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub started_at: Option<String>,
    /// ISO 8601 timestamp of when the run finished (moved to complete/failed).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub finished_at: Option<String>,
}

/// Access to CI runs for a single repo.
///
/// Owns the base path (`runs/<repo>/`) and provides run creation.
/// Obtain one via `Repo::runs()`.
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
    /// Writes `meta.yml` and `state.yml` atomically (temp dir + rename).
    pub fn create(&self, meta: &RunMeta) -> Result<Run> {
        let pending_dir = self.base.join(RunState::Pending.dir_name());
        let id = uuid::Uuid::now_v7().to_string();

        fs_err::create_dir_all(&pending_dir)?;

        let tmp_dir = pending_dir.join(format!(".tmp-{id}"));
        fs_err::create_dir_all(&tmp_dir)?;

        write_yaml(&tmp_dir.join("meta.yml"), meta)?;
        write_yaml(
            &tmp_dir.join("state.yml"),
            &RunStateFile {
                status: RunState::Pending,
                started_at: None,
                finished_at: None,
            },
        )?;

        let final_dir = pending_dir.join(&id);
        fs_err::rename(&tmp_dir, &final_dir)?;

        Ok(Run {
            base: self.base.clone(),
            state: RunState::Pending,
            id,
        })
    }

    /// Scan for orphaned runs in `pending/` and `active/` directories.
    ///
    /// The caller decides how to reconcile them:
    /// - `pending/` entries should be re-enqueued.
    /// - `active/` entries with no live runner should be marked failed.
    pub fn scan_orphans(&self) -> Result<Vec<OpenedRun>> {
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

                // Skip temp files.
                if name.starts_with('.') {
                    continue;
                }

                match Run::open(self.base.clone(), state, name) {
                    Ok(opened) => orphans.push(opened),
                    Err(e) => {
                        tracing::warn!(
                            state = ?state,
                            %e,
                            "skipping orphaned run"
                        );
                    }
                }
            }
        }

        Ok(orphans)
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
                run_id = %orphan.run.id(),
                state = ?orphan.run.state(),
                "found orphaned run"
            );
        }

        for mut orphan in orphans {
            match orphan.run.state() {
                RunState::Pending => {
                    tracing::warn!(
                        run_id = %orphan.run.id(),
                        "completing orphaned pending run"
                    );
                    if let Err(e) = orphan.run.transition(RunState::Complete) {
                        tracing::error!(
                            run_id = %orphan.run.id(),
                            %e,
                            "failed to transition orphaned pending run"
                        );
                    }
                }
                RunState::Active => {
                    tracing::warn!(
                        run_id = %orphan.run.id(),
                        "marking orphaned active run as failed"
                    );
                    if let Err(e) = orphan.run.transition(RunState::Failed) {
                        tracing::error!(
                            run_id = %orphan.run.id(),
                            %e,
                            "failed to transition orphaned active run to failed"
                        );
                        continue;
                    }
                    if let Err(e) = orphan.run.write_state(&RunStateFile {
                        status: RunState::Failed,
                        started_at: orphan.state.started_at.clone(),
                        finished_at: Some(jiff::Zoned::now().to_string()),
                    }) {
                        tracing::error!(
                            run_id = %orphan.run.id(),
                            %e,
                            "failed to write state for failed run"
                        );
                    }
                }
                _ => unreachable!("scan_orphans only returns pending/active"),
            }
        }

        Ok(())
    }
}

/// A CI run on disk.
///
/// Owns the path to the run directory. Tracks current state so that
/// `transition` can move the directory in one call.
#[derive(Debug)]
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

    /// Open an existing run from disk, reading its metadata and state.
    ///
    /// `state` is the directory the run is expected to be in (e.g.
    /// `pending/`, `active/`). Returns an error if the run directory or
    /// its files are missing or unreadable.
    pub fn open(base: PathBuf, state: RunState, id: String) -> Result<OpenedRun> {
        let run = Self { base, state, id };
        let meta = run.read_meta()?;
        let run_state = run.read_state()?;
        Ok(OpenedRun {
            run,
            meta,
            state: run_state,
        })
    }

    /// Transition the run from its current state to a new state.
    ///
    /// Moves the run directory between state parent directories and updates
    /// the tracked state.
    pub fn transition(&mut self, to: RunState) -> Result<()> {
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
        Ok(())
    }

    /// Read the mutable state file for this run.
    pub fn read_state(&self) -> Result<RunStateFile> {
        read_yaml(&self.path().join("state.yml"))
    }

    /// Read the immutable metadata for this run.
    pub fn read_meta(&self) -> Result<RunMeta> {
        read_yaml(&self.path().join("meta.yml"))
    }

    /// Update the state file for this run (atomic write).
    pub fn write_state(&self, state: &RunStateFile) -> Result<()> {
        write_yaml(&self.path().join("state.yml"), state)
    }
}

/// A run loaded from disk with its metadata and state.
#[derive(Debug)]
pub struct OpenedRun {
    pub run: Run,
    pub meta: RunMeta,
    pub state: RunStateFile,
}

/// A registered job definition extracted from ci.fnl.
pub struct JobDef {
    pub id: String,
    pub inputs: Vec<String>,
}

/// The result of evaluating a ci.fnl file.
pub struct EvalResult {
    pub jobs: Vec<JobDef>,
}

/// Evaluate a ci.fnl source string, registering jobs via the `job` macro.
///
/// Injects a `job` global that accumulates into a registration table,
/// evaluates the source, and extracts the registered jobs.
pub fn eval_ci(fennel: &Fennel, source: &str, name: &str) -> Result<EvalResult> {
    fennel.eval_raw(source, name, |lua| {
        // Create a registration table. `job` will push into this.
        let registry: mlua::Table = lua.create_table()?;
        lua.globals().set("_quire_jobs", registry)?;

        // Define the `job` global: (job id inputs run-fn)
        let job_fn = lua.create_function(
            |lua, (id, inputs, run_fn): (mlua::String, mlua::Table, mlua::Function)| {
                let registry: mlua::Table = lua.globals().get("_quire_jobs")?;
                let entry = lua.create_table()?;
                entry.set("id", id)?;
                entry.set("inputs", inputs)?;
                entry.set("run", run_fn)?;
                registry.push(entry)?;
                Ok(())
            },
        )?;
        lua.globals().set("job", job_fn)?;

        Ok(())
    })?;

    // Extract the registration table.
    let lua_err = |e: mlua::Error| FennelError::from_lua(source, name, e);
    let registry: mlua::Table = fennel.lua().globals().get("_quire_jobs").map_err(lua_err)?;
    let mut jobs = Vec::new();
    for entry in registry.sequence_values::<mlua::Table>() {
        let entry = entry.map_err(lua_err)?;
        let id: String = entry.get("id").map_err(lua_err)?;
        let inputs_table: mlua::Table = entry.get("inputs").map_err(lua_err)?;
        let mut inputs = Vec::new();
        for input in inputs_table.sequence_values::<String>() {
            inputs.push(input.map_err(lua_err)?);
        }
        jobs.push(JobDef { id, inputs });
    }

    Ok(EvalResult { jobs })
}

/// A validation error found in the job graph.
#[derive(Debug, thiserror::Error, miette::Diagnostic)]
pub enum ValidationError {
    #[error("Cycle detected among jobs: {}", cycle_jobs.join(", "))]
    Cycle { cycle_jobs: Vec<String> },

    #[error(
        "Job '{job_id}' has empty inputs. Pass [:quire/push] (or another input) so it has something to fire it."
    )]
    EmptyInputs { job_id: String },

    #[error("Job '{job_id}' is not reachable from any source ref (e.g. :quire/push).")]
    Unreachable { job_id: String },

    #[error("Job id '{job_id}' contains '/', which is reserved for the 'quire/' source namespace.")]
    ReservedSlash { job_id: String },
}

/// Validate the structural rules of a job graph.
///
/// Returns `Ok(())` if all four rules pass, or `Err` with all violations found.
pub fn validate(jobs: &[JobDef]) -> std::result::Result<(), Vec<ValidationError>> {
    let mut errors = Vec::new();

    // Rule 4: no '/' in user job ids.
    for job in jobs {
        if job.id.contains('/') {
            errors.push(ValidationError::ReservedSlash {
                job_id: job.id.clone(),
            });
        }
    }

    // Rule 2: non-empty inputs.
    for job in jobs {
        if job.inputs.is_empty() {
            errors.push(ValidationError::EmptyInputs {
                job_id: job.id.clone(),
            });
        }
    }

    // Rule 1: acyclic.
    //
    // Build a directed graph where edges point from dependency to
    // dependent. Source refs (e.g. "quire/push") are not nodes.
    let mut graph: petgraph::Graph<&str, ()> = petgraph::Graph::new();
    let mut node_map: std::collections::HashMap<&str, petgraph::graph::NodeIndex> =
        std::collections::HashMap::new();

    for job in jobs {
        let idx = graph.add_node(job.id.as_str());
        node_map.insert(job.id.as_str(), idx);
    }

    for job in jobs {
        let dependent = node_map[job.id.as_str()];
        for input in &job.inputs {
            if let Some(&dependency) = node_map.get(input.as_str()) {
                graph.add_edge(dependency, dependent, ());
            }
        }
    }

    // Each non-trivial strongly connected component is a distinct cycle.
    // A single-node SCC is only a cycle if it has a self-edge.
    for scc in petgraph::algo::tarjan_scc(&graph) {
        let is_cycle = scc.len() > 1 || (scc.len() == 1 && graph.contains_edge(scc[0], scc[0]));
        if !is_cycle {
            continue;
        }
        let mut cycle_jobs: Vec<String> = scc.iter().map(|&idx| graph[idx].to_string()).collect();
        cycle_jobs.sort();
        errors.push(ValidationError::Cycle { cycle_jobs });
    }

    // Rule 3: reachability — every job's transitive inputs must include a source ref.
    let is_source = |name: &str| name.starts_with("quire/");

    for job in jobs {
        let mut visited = std::collections::HashSet::new();
        let mut stack: Vec<&str> = job.inputs.iter().map(|s| s.as_str()).collect();
        let mut found_source = false;

        while let Some(name) = stack.pop() {
            if !visited.insert(name) {
                continue;
            }
            if is_source(name) {
                found_source = true;
                break;
            }
            if let Some(upstream) = jobs.iter().find(|j| j.id == name) {
                for input in &upstream.inputs {
                    stack.push(input.as_str());
                }
            }
        }

        if !found_source {
            errors.push(ValidationError::Unreachable {
                job_id: job.id.clone(),
            });
        }
    }

    if errors.is_empty() {
        Ok(())
    } else {
        Err(errors)
    }
}

/// Dispatch a push event: CI gating and mirror push.
pub async fn dispatch_push(quire: &crate::Quire, event: &PushEvent) {
    let repo = match quire.repo(&event.repo) {
        Ok(r) if r.exists() => r,
        Ok(_) => {
            tracing::error!(repo = %event.repo, "repo not found on disk");
            return;
        }
        Err(e) => {
            tracing::error!(repo = %event.repo, %e, "invalid repo name in event");
            return;
        }
    };

    dispatch_ci(&repo, event);
    dispatch_mirror(quire, repo, event).await;
}

/// Check each updated ref for .quire/ci.fnl, create runs, and eval + validate.
fn dispatch_ci(repo: &Repo, event: &PushEvent) {
    for push_ref in event.updated_refs() {
        if let Err(e) = dispatch_ci_ref(repo, &event.pushed_at, push_ref) {
            tracing::error!(
                repo = %event.repo,
                sha = %push_ref.new_sha,
                %e,
                "CI dispatch failed"
            );
        }
    }
}

/// Create and run CI for a single updated ref.
///
/// Returns `Ok(())` if CI ran (regardless of whether the run succeeded
/// or failed), or `Err` if dispatch itself failed.
fn dispatch_ci_ref(repo: &Repo, pushed_at: &str, push_ref: &PushRef) -> Result<()> {
    if !repo.has_ci_fnl(&push_ref.new_sha) {
        return Ok(());
    }

    let meta = RunMeta {
        sha: push_ref.new_sha.clone(),
        r#ref: push_ref.r#ref.clone(),
        pushed_at: pushed_at.to_string(),
    };

    let mut run = repo.runs().create(&meta)?;

    tracing::info!(
        run_id = %run.id(),
        sha = %push_ref.new_sha,
        r#ref = %push_ref.r#ref,
        "created CI run"
    );

    run.transition(RunState::Active)?;

    let result = eval_and_validate(repo, &push_ref.new_sha);
    match result {
        Ok(()) => {
            run.transition(RunState::Complete)?;
        }
        Err(e) => {
            run.transition(RunState::Failed)?;
            run.write_state(&RunStateFile {
                status: RunState::Failed,
                started_at: None,
                finished_at: Some(jiff::Zoned::now().to_string()),
            })?;
            // Return the eval/validation error as the dispatch error.
            Err(e)?;
        }
    }

    Ok(())
}

/// Evaluate ci.fnl at a given SHA and validate the job graph.
fn eval_and_validate(repo: &Repo, sha: &str) -> Result<()> {
    let source = repo.ci_fnl_source(sha)?;
    let fennel = Fennel::new()?;
    let eval_result = eval_ci(&fennel, &source, &format!("{sha}:.quire/ci.fnl"))?;
    validate(&eval_result.jobs)?;
    Ok(())
}

/// Push updated refs to the configured mirror.
async fn dispatch_mirror(quire: &crate::Quire, repo: Repo, event: &PushEvent) {
    let config = match repo.config() {
        Ok(c) => c,
        Err(e) => {
            tracing::error!(repo = %event.repo, %e, "failed to load repo config");
            return;
        }
    };

    let Some(mirror) = config.mirror else {
        tracing::debug!(repo = %event.repo, "no mirror configured, skipping");
        return;
    };

    let global_config = match quire.global_config() {
        Ok(c) => c,
        Err(e) => {
            tracing::error!(%e, "failed to load global config for mirror push");
            return;
        }
    };

    let token = match global_config.github.token.reveal() {
        Ok(t) => t.to_string(),
        Err(e) => {
            tracing::error!(%e, "failed to resolve GitHub token");
            return;
        }
    };

    // Only push refs that were actually updated (non-zero new sha).
    let refs: Vec<String> = event
        .updated_refs()
        .iter()
        .map(|r| r.r#ref.clone())
        .collect();

    if refs.is_empty() {
        return;
    }

    let mirror_url = mirror.url.clone();
    tracing::info!(url = %mirror.url, refs = ?refs, "pushing to mirror");

    let result = tokio::task::spawn_blocking(move || {
        let ref_slices: Vec<&str> = refs.iter().map(|s| s.as_str()).collect();
        repo.push_to_mirror(&mirror, &token, &ref_slices)
    })
    .await;

    match result {
        Ok(Ok(())) => tracing::info!(url = %mirror_url, "mirror push complete"),
        Ok(Err(e)) => tracing::error!(url = %mirror_url, %e, "mirror push failed"),
        Err(e) => tracing::error!(url = %mirror_url, %e, "mirror push task panicked"),
    }
}

/// Write a serializable value to a YAML file atomically (temp file + rename).
fn write_yaml<T: serde::Serialize>(path: &Path, value: &T) -> Result<()> {
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

    fn test_meta() -> RunMeta {
        RunMeta {
            sha: "abc123".to_string(),
            r#ref: "refs/heads/main".to_string(),
            pushed_at: "2026-04-28T12:00:00Z".to_string(),
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
        let runs = Runs::new(quire.base_dir().join("runs").join("test.git"));
        let run = runs.create(&test_meta()).expect("create");
        let parsed = uuid::Uuid::parse_str(run.id()).expect("should be valid UUID");
        assert_eq!(parsed.get_version(), Some(uuid::Version::SortRand));
    }

    #[test]
    fn create_writes_files_in_pending() {
        let (_dir, quire) = tmp_quire();
        let runs = Runs::new(quire.base_dir().join("runs").join("test.git"));
        let run = runs.create(&test_meta()).expect("create");

        let path = run.path();
        assert!(path.exists(), "run directory should exist");
        assert!(path.join("meta.yml").exists());
        assert!(path.join("state.yml").exists());
        assert_eq!(run.state(), RunState::Pending);

        let meta = run.read_meta().expect("read meta");
        assert_eq!(meta.sha, "abc123");

        let state = run.read_state().expect("read state");
        assert_eq!(state.status, RunState::Pending);
        assert!(state.started_at.is_none());
    }

    #[test]
    fn transition_moves_directory() {
        let (_dir, quire) = tmp_quire();
        let runs = Runs::new(quire.base_dir().join("runs").join("test.git"));
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
    fn transition_full_lifecycle() {
        let (_dir, quire) = tmp_quire();
        let runs = Runs::new(quire.base_dir().join("runs").join("test.git"));
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
        let runs = Runs::new(quire.base_dir().join("runs").join("test.git"));
        let run = runs.create(&test_meta()).expect("create");

        let orphans = runs.scan_orphans().expect("scan");
        assert_eq!(orphans.len(), 1);
        assert_eq!(orphans[0].run.id(), run.id());
        assert_eq!(orphans[0].run.state(), RunState::Pending);
    }

    #[test]
    fn scan_orphans_finds_active() {
        let (_dir, quire) = tmp_quire();
        let runs = Runs::new(quire.base_dir().join("runs").join("test.git"));
        let mut run = runs.create(&test_meta()).expect("create");
        run.transition(RunState::Active).expect("transition");

        let orphans = runs.scan_orphans().expect("scan");
        assert_eq!(orphans.len(), 1);
        assert_eq!(orphans[0].run.state(), RunState::Active);
    }

    #[test]
    fn scan_orphans_skips_complete() {
        let (_dir, quire) = tmp_quire();
        let runs = Runs::new(quire.base_dir().join("runs").join("test.git"));
        let mut run = runs.create(&test_meta()).expect("create");
        run.transition(RunState::Complete).expect("transition");

        let orphans = runs.scan_orphans().expect("scan");
        assert!(orphans.is_empty(), "complete runs are not orphans");
    }

    #[test]
    fn scan_orphans_empty_when_no_runs_dir() {
        let (_dir, quire) = tmp_quire();
        let runs = Runs::new(quire.base_dir().join("runs").join("test.git"));
        assert!(runs.scan_orphans().expect("scan").is_empty());
    }

    #[test]
    fn write_state_updates_in_place() {
        let (_dir, quire) = tmp_quire();
        let runs = Runs::new(quire.base_dir().join("runs").join("test.git"));
        let run = runs.create(&test_meta()).expect("create");

        run.write_state(&RunStateFile {
            status: RunState::Active,
            started_at: Some("2026-04-28T12:00:01Z".to_string()),
            finished_at: None,
        })
        .expect("write state");

        let loaded = run.read_state().expect("read state");
        assert_eq!(loaded.status, RunState::Active);
        assert_eq!(loaded.started_at.as_deref(), Some("2026-04-28T12:00:01Z"));

        // Meta is unchanged.
        let loaded_meta = run.read_meta().expect("read meta");
        assert_eq!(loaded_meta, test_meta());
    }

    // --- eval_ci tests ---

    fn fennel() -> Fennel {
        Fennel::new().expect("Fennel::new() should succeed")
    }

    #[test]
    fn eval_ci_registers_a_job() {
        let f = fennel();
        let source = r#"(job :test [:quire/push] (fn [_] nil))"#;
        let result = eval_ci(&f, source, "ci.fnl").expect("eval should succeed");
        assert_eq!(result.jobs.len(), 1);
        assert_eq!(result.jobs[0].id, "test");
        assert_eq!(result.jobs[0].inputs, vec!["quire/push"]);
    }

    #[test]
    fn eval_ci_registers_multiple_jobs() {
        let f = fennel();
        let source = r#"
(job :build [:quire/push] (fn [_] nil))
(job :test [:build] (fn [_] nil))
"#;
        let result = eval_ci(&f, source, "ci.fnl").expect("eval should succeed");
        assert_eq!(result.jobs.len(), 2);
        assert_eq!(result.jobs[0].id, "build");
        assert_eq!(result.jobs[0].inputs, vec!["quire/push"]);
        assert_eq!(result.jobs[1].id, "test");
        assert_eq!(result.jobs[1].inputs, vec!["build"]);
    }

    #[test]
    fn eval_ci_errors_on_bad_fennel() {
        let f = fennel();
        let result = eval_ci(&f, "{:bad {:}", "ci.fnl");
        assert!(result.is_err(), "malformed Fennel should fail");
    }

    // --- validate tests ---

    #[test]
    fn validate_accepts_valid_config() {
        let jobs = vec![
            JobDef {
                id: "build".into(),
                inputs: vec!["quire/push".into()],
            },
            JobDef {
                id: "test".into(),
                inputs: vec!["build".into(), "quire/push".into()],
            },
        ];
        assert!(validate(&jobs).is_ok());
    }

    #[test]
    fn validate_rejects_cycle() {
        let jobs = vec![
            JobDef {
                id: "a".into(),
                inputs: vec!["b".into()],
            },
            JobDef {
                id: "b".into(),
                inputs: vec!["a".into()],
            },
        ];
        let errs = validate(&jobs).unwrap_err();
        assert!(
            errs.iter().any(|e| matches!(e, ValidationError::Cycle { cycle_jobs } if cycle_jobs.contains(&"a".to_string()) && cycle_jobs.contains(&"b".to_string()))),
            "should report a cycle involving a and b: {errs:?}"
        );
    }

    #[test]
    fn validate_cycle_only_reports_cycle_members() {
        // `clean` is acyclic; `a` and `b` form a cycle. Only a/b should be
        // flagged, and `clean` must not appear in any Cycle error.
        let jobs = vec![
            JobDef {
                id: "a".into(),
                inputs: vec!["b".into(), "quire/push".into()],
            },
            JobDef {
                id: "b".into(),
                inputs: vec!["a".into(), "quire/push".into()],
            },
            JobDef {
                id: "clean".into(),
                inputs: vec!["quire/push".into()],
            },
        ];
        let errs = validate(&jobs).unwrap_err();
        let cycle_errs: Vec<&Vec<String>> = errs
            .iter()
            .filter_map(|e| match e {
                ValidationError::Cycle { cycle_jobs } => Some(cycle_jobs),
                _ => None,
            })
            .collect();
        assert_eq!(
            cycle_errs.len(),
            1,
            "expected exactly one cycle error: {errs:?}"
        );
        assert_eq!(cycle_errs[0], &vec!["a".to_string(), "b".to_string()]);
    }

    #[test]
    fn validate_reports_disjoint_cycles_separately() {
        // Two independent cycles: (a <-> b) and (c <-> d).
        let jobs = vec![
            JobDef {
                id: "a".into(),
                inputs: vec!["b".into(), "quire/push".into()],
            },
            JobDef {
                id: "b".into(),
                inputs: vec!["a".into(), "quire/push".into()],
            },
            JobDef {
                id: "c".into(),
                inputs: vec!["d".into(), "quire/push".into()],
            },
            JobDef {
                id: "d".into(),
                inputs: vec!["c".into(), "quire/push".into()],
            },
        ];
        let errs = validate(&jobs).unwrap_err();
        let cycle_count = errs
            .iter()
            .filter(|e| matches!(e, ValidationError::Cycle { .. }))
            .count();
        assert_eq!(cycle_count, 2, "expected two cycle errors: {errs:?}");
    }

    #[test]
    fn validate_rejects_empty_inputs() {
        let jobs = vec![JobDef {
            id: "setup".into(),
            inputs: vec![],
        }];
        let errs = validate(&jobs).unwrap_err();
        assert!(
            errs.iter()
                .any(|e| matches!(e, ValidationError::EmptyInputs { job_id } if job_id == "setup")),
            "should report empty inputs for 'setup': {errs:?}"
        );
    }

    #[test]
    fn validate_rejects_unreachable_jobs() {
        let jobs = vec![JobDef {
            id: "orphan".into(),
            inputs: vec!["orphan".into()],
        }];
        let errs = validate(&jobs).unwrap_err();
        assert!(
            errs.iter().any(
                |e| matches!(e, ValidationError::Unreachable { job_id } if job_id == "orphan")
            ),
            "should report unreachable job 'orphan': {errs:?}"
        );
    }

    #[test]
    fn validate_rejects_slash_in_job_id() {
        let jobs = vec![JobDef {
            id: "foo/bar".into(),
            inputs: vec!["quire/push".into()],
        }];
        let errs = validate(&jobs).unwrap_err();
        assert!(
            errs.iter().any(
                |e| matches!(e, ValidationError::ReservedSlash { job_id } if job_id == "foo/bar")
            ),
            "should report slash in job id: {errs:?}"
        );
    }
}
