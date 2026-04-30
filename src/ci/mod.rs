//! CI: trigger runs from push events, validate the job graph.

mod lua;
pub mod pipeline;
pub mod run;

pub use pipeline::{Job, LoadError, Pipeline, ValidationError};
pub use run::{Run, RunMeta, RunState, RunTimes, Runs};

/// A resolved commit reference.
///
/// Carries both the full SHA (for git operations) and a short display
/// form (for error messages and user-facing output).
pub struct CommitRef {
    /// Full commit SHA for git operations.
    pub sha: String,
    /// Short or human-readable form for display.
    pub display: String,
}

use std::collections::HashMap;
use std::path::PathBuf;

use crate::Result;
use crate::event::{PushEvent, PushRef};
use crate::quire::Repo;
use crate::secret::SecretString;

/// Path to the CI config within a bare repo, relative to the repo root.
pub const CI_FNL: &str = ".quire/ci.fnl";

/// Access to CI operations for a single repo.
///
/// Provides load and validation methods scoped to a bare repo.
/// Obtain one via `Repo::ci()`. Run lifecycle is on `Runs`, obtainable
/// via `Repo::runs()`.
pub struct Ci {
    repo_path: PathBuf,
}

impl Ci {
    pub fn new(repo_path: PathBuf) -> Self {
        Self { repo_path }
    }

    /// Access CI runs for this repo.
    pub fn runs(&self, runs_base: PathBuf) -> Runs {
        Runs::new(runs_base)
    }

    /// Load ci.fnl at a given SHA and return the validated pipeline.
    ///
    /// `secrets` is the map of named secrets exposed to the script via
    /// `(ci:secret …)`; pass an empty map for structural validation that
    /// does not need to resolve secrets at registration time.
    ///
    /// Returns `Ok(None)` if the repo has no ci.fnl at that commit.
    /// Errors if the Fennel source fails to parse/evaluate or if the
    /// resulting job graph violates any structural rule.
    pub fn load(
        &self,
        commit: &CommitRef,
        secrets: HashMap<String, SecretString>,
    ) -> Result<Option<Pipeline>> {
        let Some(source) = self.source(&commit.sha)? else {
            return Ok(None);
        };
        let fennel = crate::fennel::Fennel::new()?;
        let name = CI_FNL.to_string();
        let lua_name = format!("{}:{CI_FNL}", commit.sha);
        let pipeline = pipeline::load(&fennel, &source, &lua_name, &name, secrets)?;
        Ok(Some(pipeline))
    }

    /// Read the contents of `.quire/ci.fnl` at a given commit SHA.
    ///
    /// Returns `Ok(None)` if the file does not exist at that commit,
    /// `Ok(Some(contents))` if it does, or `Err` for unexpected failures.
    fn source(&self, sha: &str) -> Result<Option<String>> {
        let output = self
            .git(&["show", &format!("{sha}:{CI_FNL}")])
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .output()?;

        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            if stderr.contains("does not exist") || stderr.contains("not found") {
                return Ok(None);
            }
            return Err(crate::Error::Git(format!(
                "failed to read {CI_FNL} at {sha}: {stderr}"
            )));
        }

        Ok(Some(String::from_utf8(output.stdout)?))
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

    // Pull the secrets map up front; missing global config means no
    // secrets are available, but a present-but-broken config is a real
    // error and aborts the trigger.
    let secrets = match quire.global_config() {
        Ok(c) => c.secrets,
        Err(crate::Error::ConfigNotFound(_)) => HashMap::new(),
        Err(e) => {
            tracing::error!(repo = %event.repo, %e, "failed to load global config");
            return;
        }
    };

    for push_ref in event.updated_refs() {
        if let Err(e) = trigger_ref(&repo, event.pushed_at, push_ref, secrets.clone()) {
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
fn trigger_ref(
    repo: &Repo,
    pushed_at: jiff::Timestamp,
    push_ref: &PushRef,
    secrets: HashMap<String, SecretString>,
) -> Result<()> {
    let ci = repo.ci();

    let Some(source) = ci.source(&push_ref.new_sha)? else {
        return Ok(());
    };

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

    let fennel = crate::fennel::Fennel::new()?;
    let name = CI_FNL.to_string();
    let lua_name = format!("{}:{CI_FNL}", push_ref.new_sha);

    match pipeline::load(&fennel, &source, &lua_name, &name, secrets) {
        Ok(_pipeline) => run.transition(RunState::Complete)?,
        Err(e) => {
            run.transition(RunState::Failed)?;
            return Err(e);
        }
    }

    Ok(())
}
