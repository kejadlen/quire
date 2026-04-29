//! CI: trigger runs from push events, validate the job graph.

pub mod graph;
pub mod run;

pub use graph::{EvalResult, JobDef, ValidationError, eval_ci, validate};
pub use run::{Run, RunMeta, RunState, RunTimes, Runs};

use std::path::{Path, PathBuf};

use crate::Result;
use crate::event::{PushEvent, PushRef};
use crate::quire::Repo;

/// Path to the CI config within a bare repo, relative to the repo root.
pub const CI_FNL: &str = ".quire/ci.fnl";

/// Access to CI operations for a single repo.
///
/// Provides eval and validation methods scoped to a bare repo.
/// Obtain one via `Repo::ci()`. Run lifecycle is on `Runs`, obtainable
/// via `Repo::runs()`.
pub struct Ci {
    repo_path: PathBuf,
}

impl Ci {
    pub(crate) fn new(repo_path: PathBuf) -> Self {
        Self { repo_path }
    }

    /// Access CI runs for this repo.
    pub fn runs(&self, runs_base: PathBuf) -> Runs {
        Runs::new(runs_base)
    }

    /// Evaluate ci.fnl at a given SHA and return the registration table.
    pub fn eval(&self, sha: &str) -> Result<EvalResult> {
        let source = self.ci_fnl_source(sha)?;
        let fennel = crate::fennel::Fennel::new()?;
        let name = format!("{sha}:{CI_FNL}");
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
        let fennel = crate::fennel::Fennel::new()?;
        let result = eval_ci(&fennel, &source, &name)?;
        validate(&result.jobs)?;
        Ok(result)
    }

    /// Check whether this bare repo has `.quire/ci.fnl` at a given commit SHA.
    fn has_ci_fnl(&self, sha: &str) -> bool {
        self.git(&["show", &format!("{sha}:{CI_FNL}")])
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
            .map(|s| s.success())
            .unwrap_or(false)
    }

    /// Read the contents of `.quire/ci.fnl` at a given commit SHA.
    fn ci_fnl_source(&self, sha: &str) -> Result<String> {
        let output = self
            .git(&["show", &format!("{sha}:{CI_FNL}")])
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .output()?;

        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            return Err(crate::Error::Git(format!(
                "failed to read {CI_FNL} at {sha}: {stderr}"
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

    let mut run = ci.runs(repo.runs_base()).create(&meta)?;

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
