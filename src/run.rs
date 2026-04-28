use std::path::{Path, PathBuf};

use crate::Result;

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
    pub fn scan_orphans(&self) -> Vec<OrphanedRun> {
        let mut orphans = Vec::new();

        for &state in &[RunState::Pending, RunState::Active] {
            let state_path = self.base.join(state.dir_name());
            let Ok(entries) = fs_err::read_dir(&state_path) else {
                continue;
            };

            for entry in entries.flatten() {
                let name = match entry.file_name().to_str() {
                    Some(n) => n.to_string(),
                    None => continue,
                };

                // Skip temp files.
                if name.starts_with('.') {
                    continue;
                }

                let path = entry.path();

                let meta = match read_yaml::<RunMeta>(&path.join("meta.yml")) {
                    Ok(m) => m,
                    Err(e) => {
                        tracing::warn!(
                            path = %path.display(),
                            %e,
                            "skipping orphaned run: cannot read meta"
                        );
                        continue;
                    }
                };

                let state_file = match read_yaml::<RunStateFile>(&path.join("state.yml")) {
                    Ok(s) => s,
                    Err(e) => {
                        tracing::warn!(
                            path = %path.display(),
                            %e,
                            "skipping orphaned run: cannot read state"
                        );
                        continue;
                    }
                };

                orphans.push(OrphanedRun {
                    run: Run::open(self.base.clone(), state, name),
                    meta,
                    state: state_file,
                });
            }
        }

        orphans
    }

    /// Reconcile orphaned runs from a previous server instance.
    ///
    /// - `pending/` orphans are moved to `complete/` (will be re-enqueued when
    ///   the runner exists; for now, immediately completed).
    /// - `active/` orphans are moved to `failed/` (no live runner).
    pub fn reconcile_orphans(&self) {
        let orphans = self.scan_orphans();
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

    /// Open an existing run at a known state.
    ///
    /// Does not verify the directory exists — used by the orphan scanner
    /// which already read the directory listing.
    fn open(base: PathBuf, state: RunState, id: String) -> Self {
        Self { base, state, id }
    }

    /// Transition the run from its current state to a new state.
    ///
    /// Moves the run directory between state parent directories and updates
    /// the tracked state.
    pub fn transition(&mut self, to: RunState) -> Result<()> {
        let src = self.path();
        let dst_parent = self.base.join(to.dir_name());

        if !src.exists() {
            return Err(crate::Error::Io(std::io::Error::new(
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

/// An orphaned run found during startup scan.
#[derive(Debug)]
pub struct OrphanedRun {
    pub run: Run,
    pub meta: RunMeta,
    pub state: RunStateFile,
}

/// Write a value to a YAML file atomically.
fn write_yaml<T: serde::Serialize>(path: &Path, value: &T) -> Result<()> {
    let tmp_path = path.with_extension("yml.tmp");
    let f = fs_err::File::create(&tmp_path)?;
    serde_yaml_ng::to_writer(std::io::BufWriter::new(f), value)
        .map_err(|e| crate::Error::Io(std::io::Error::other(format!("yaml write error: {e}"))))?;
    fs_err::rename(&tmp_path, path)?;
    Ok(())
}

/// Read a value from a YAML file.
fn read_yaml<T: serde::de::DeserializeOwned>(path: &Path) -> Result<T> {
    let f = fs_err::File::open(path)?;
    serde_yaml_ng::from_reader(std::io::BufReader::new(f))
        .map_err(|e| crate::Error::Io(std::io::Error::other(format!("yaml read error: {e}"))))
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

        let orphans = runs.scan_orphans();
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

        let orphans = runs.scan_orphans();
        assert_eq!(orphans.len(), 1);
        assert_eq!(orphans[0].run.state(), RunState::Active);
    }

    #[test]
    fn scan_orphans_skips_complete() {
        let (_dir, quire) = tmp_quire();
        let runs = Runs::new(quire.base_dir().join("runs").join("test.git"));
        let mut run = runs.create(&test_meta()).expect("create");
        run.transition(RunState::Complete).expect("transition");

        let orphans = runs.scan_orphans();
        assert!(orphans.is_empty(), "complete runs are not orphans");
    }

    #[test]
    fn scan_orphans_empty_when_no_runs_dir() {
        let (_dir, quire) = tmp_quire();
        let runs = Runs::new(quire.base_dir().join("runs").join("test.git"));
        assert!(runs.scan_orphans().is_empty());
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
}
