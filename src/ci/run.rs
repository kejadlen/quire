//! On-disk storage for CI runs.
//!
//! A run is a directory under `runs/<repo>/<state>/<id>/` containing
//! `meta.yml` (immutable) and `times.yml` (timestamps). The directory's
//! parent name is the authoritative state; transitions are atomic
//! `rename` operations.

use std::path::{Path, PathBuf};

use jiff::Timestamp;

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

        Ok(Run {
            base: self.base.clone(),
            state: RunState::Pending,
            id,
        })
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

                // Skip temp files.
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
            return Err(Error::Io(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                format!("invalid run transition: {:?} -> {:?}", self.state, to),
            )));
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
    fn transition_stamps_started_at_on_active() {
        let (_dir, quire) = tmp_quire();
        let runs = Runs::new(quire.base_dir().join("runs").join("test.git"));
        let mut run = runs.create(&test_meta()).expect("create");

        run.transition(RunState::Active).expect("to active");
        let times = run.read_times().expect("read state");
        assert!(times.started_at.is_some(), "started_at should be stamped");
        assert!(times.finished_at.is_none());
    }

    #[test]
    fn transition_stamps_finished_at_on_complete_and_failed() {
        let (_dir, quire) = tmp_quire();
        let runs = Runs::new(quire.base_dir().join("runs").join("test.git"));

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
        let runs = Runs::new(quire.base_dir().join("runs").join("test.git"));

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
        let runs = Runs::new(quire.base_dir().join("runs").join("test.git"));
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
        assert_eq!(orphans[0].id(), run.id());
        assert_eq!(orphans[0].state(), RunState::Pending);
    }

    #[test]
    fn scan_orphans_finds_active() {
        let (_dir, quire) = tmp_quire();
        let runs = Runs::new(quire.base_dir().join("runs").join("test.git"));
        let mut run = runs.create(&test_meta()).expect("create");
        run.transition(RunState::Active).expect("transition");

        let orphans = runs.scan_orphans().expect("scan");
        assert_eq!(orphans.len(), 1);
        assert_eq!(orphans[0].state(), RunState::Active);
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
        let runs = Runs::new(quire.base_dir().join("runs").join("test.git"));
        assert!(runs.scan_orphans().expect("scan").is_empty());
    }

    #[test]
    fn write_times_updates_in_place() {
        let (_dir, quire) = tmp_quire();
        let runs = Runs::new(quire.base_dir().join("runs").join("test.git"));
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
}
