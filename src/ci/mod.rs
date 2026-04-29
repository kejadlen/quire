//! CI: trigger runs from push events, validate the job graph.

pub mod graph;
pub mod run;

pub use graph::{EvalResult, JobDef, ValidationError, eval_ci, validate};
pub use run::{Run, RunMeta, RunState, RunTimes};

use std::path::{Path, PathBuf};

use crate::Result;
use crate::event::{PushEvent, PushRef};
use crate::fennel::Fennel;
use crate::quire::Repo;

/// Access to CI operations for a single repo.
///
/// Owns the base path for runs (`runs/<repo>/`) and provides eval,
/// validation, and run lifecycle methods. Obtain one via `Repo::ci()`.
pub struct Ci {
    repo_path: PathBuf,
    runs_base: PathBuf,
}

impl Ci {
    pub(crate) fn new(repo_path: PathBuf, runs_base: PathBuf) -> Self {
        Self {
            repo_path,
            runs_base,
        }
    }

    // --- Eval & validation ---

    /// Evaluate ci.fnl at a given SHA and return the registration table.
    pub fn eval(&self, sha: &str) -> Result<EvalResult> {
        let source = self.ci_fnl_source(sha)?;
        let fennel = Fennel::new()?;
        let name = format!("{sha}:.quire/ci.fnl");
        let result = eval_ci(&fennel, &source, &name)?;
        Ok(result)
    }

    /// Evaluate ci.fnl at a given SHA and validate the job graph.
    pub fn validate_at(&self, sha: &str) -> Result<EvalResult> {
        let result = self.eval(sha)?;
        validate(&result.jobs)?;
        Ok(result)
    }

    /// Evaluate a ci.fnl file from disk and validate the job graph.
    pub fn validate_file(path: &Path) -> Result<EvalResult> {
        let source = fs_err::read_to_string(path)?;
        let name = path.display().to_string();
        let fennel = Fennel::new()?;
        let result = eval_ci(&fennel, &source, &name)?;
        validate(&result.jobs)?;
        Ok(result)
    }

    /// Check whether this bare repo has `.quire/ci.fnl` at a given commit SHA.
    fn has_ci_fnl(&self, sha: &str) -> bool {
        self.git(&["show", &format!("{sha}:.quire/ci.fnl")])
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
            .map(|s| s.success())
            .unwrap_or(false)
    }

    /// Read the contents of `.quire/ci.fnl` at a given commit SHA.
    fn ci_fnl_source(&self, sha: &str) -> Result<String> {
        let output = self
            .git(&["show", &format!("{sha}:.quire/ci.fnl")])
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .output()?;

        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            return Err(crate::Error::Git(format!(
                "failed to read ci.fnl at {sha}: {stderr}"
            )));
        }

        Ok(String::from_utf8(output.stdout)?)
    }

    /// Start a git command rooted in this repo.
    fn git(&self, args: &[&str]) -> std::process::Command {
        let mut cmd = std::process::Command::new("git");
        cmd.args(args).current_dir(&self.repo_path);
        cmd
    }

    // --- Run lifecycle ---

    /// Create a new run record in the `pending` state.
    ///
    /// Writes `meta.yml` and `times.yml` atomically (temp dir + rename).
    pub fn create_run(&self, meta: &RunMeta) -> Result<Run> {
        let pending_dir = self.runs_base.join(RunState::Pending.dir_name());
        let id = uuid::Uuid::now_v7().to_string();

        fs_err::create_dir_all(&pending_dir)?;

        let tmp_dir = pending_dir.join(format!(".tmp-{id}"));
        fs_err::create_dir_all(&tmp_dir)?;

        run::write_yaml(&tmp_dir.join("meta.yml"), meta)?;
        run::write_yaml(&tmp_dir.join("times.yml"), &RunTimes::default())?;

        let final_dir = pending_dir.join(&id);
        fs_err::rename(&tmp_dir, &final_dir)?;

        Run::open(self.runs_base.clone(), RunState::Pending, id)
    }

    /// Scan for orphaned runs in `pending/` and `active/` directories.
    pub fn scan_orphans(&self) -> Result<Vec<Run>> {
        let mut orphans = Vec::new();

        for &state in &[RunState::Pending, RunState::Active] {
            let state_path = self.runs_base.join(state.dir_name());
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

                match Run::open(self.runs_base.clone(), state, name.clone()) {
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

    /// Move a broken run directory into `failed/`.
    fn quarantine(&self, src: &Path, id: &str) -> Result<()> {
        let failed_dir = self.runs_base.join(RunState::Failed.dir_name());
        fs_err::create_dir_all(&failed_dir)?;
        fs_err::rename(src, failed_dir.join(id))?;
        Ok(())
    }

    /// Reconcile orphaned runs from a previous server instance.
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

/// Trigger CI for a push event: scan each updated ref for `.quire/ci.fnl`,
/// create a run, and evaluate + validate it.
pub fn trigger(quire: &crate::Quire, event: &PushEvent) {
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

    for push_ref in event.updated_refs() {
        if let Err(e) = trigger_ref(&repo, event.pushed_at, push_ref) {
            tracing::error!(
                repo = %event.repo,
                sha = %push_ref.new_sha,
                %e,
                "CI trigger failed"
            );
        }
    }
}

/// Create and run CI for a single updated ref.
fn trigger_ref(repo: &Repo, pushed_at: jiff::Timestamp, push_ref: &PushRef) -> Result<()> {
    let ci = repo.ci();

    if !ci.has_ci_fnl(&push_ref.new_sha) {
        return Ok(());
    }

    let meta = RunMeta {
        sha: push_ref.new_sha.clone(),
        r#ref: push_ref.r#ref.clone(),
        pushed_at,
    };

    let mut run = ci.create_run(&meta)?;

    tracing::info!(
        run_id = %run.id(),
        sha = %push_ref.new_sha,
        r#ref = %push_ref.r#ref,
        "created CI run"
    );

    run.transition(RunState::Active)?;

    let result = ci.validate_at(&push_ref.new_sha);
    match result {
        Ok(_) => {
            run.transition(RunState::Complete)?;
        }
        Err(e) => {
            run.transition(RunState::Failed)?;
            Err(e)?;
        }
    }

    Ok(())
}
