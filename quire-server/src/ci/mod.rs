//! CI: trigger runs from push events, validate the job graph.

use std::collections::HashMap;

pub(crate) mod docker;
pub(crate) mod logs;
mod mirror;
mod pipeline;
mod registration;
mod run;
mod runtime;

pub(crate) mod error;

pub use error::{Error, Result};
pub use pipeline::{DefinitionError, Diagnostic, Job, Pipeline, PipelineError, StructureError};
pub use run::{Executor, Run, RunMeta, RunState, Runs, materialize_workspace, reconcile_orphans};

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

use std::path::{Path, PathBuf};

use crate::display_chain;
use crate::event::{PushEvent, PushRef};
use crate::quire::Repo;

/// Path to the CI config within a bare repo, relative to the repo root.
pub const CI_FNL: &str = ".quire/ci.fnl";

/// Access to CI operations for a single repo.
///
/// Provides pipeline compilation and validation scoped to a bare
/// repo. Obtain one via `Repo::ci()`. Run lifecycle is on `Runs`,
/// obtainable via `Repo::runs()`.
pub struct Ci {
    repo_path: PathBuf,
}

impl Ci {
    pub fn new(repo_path: PathBuf) -> Self {
        Self { repo_path }
    }

    /// Read and compile ci.fnl at a given SHA, returning the validated
    /// pipeline.
    ///
    /// Pure compilation and structural validation. Secrets are not needed
    /// here — they are passed to `Run::execute` since they only matter
    /// when the run-fns actually fire.
    ///
    /// Returns `Ok(None)` if the repo has no ci.fnl at that commit.
    /// Errors if the Fennel source fails to parse/evaluate or if the
    /// resulting job graph violates any structural rule.
    pub fn pipeline(&self, commit: &CommitRef) -> error::Result<Option<Pipeline>> {
        let Some(source) = self.source(&commit.sha)? else {
            return Ok(None);
        };
        Ok(Some(self.compile(&source)?))
    }

    /// Compile `.quire/ci.fnl` source into a validated [`Pipeline`].
    ///
    /// Single chokepoint for compile + structural validation, used by
    /// [`Ci::pipeline`] and `trigger_ref` so the two paths can't drift.
    fn compile(&self, source: &str) -> error::Result<Pipeline> {
        pipeline::compile(source, CI_FNL)
    }

    /// Read the contents of `.quire/ci.fnl` at a given commit SHA.
    ///
    /// Returns `Ok(None)` if the file does not exist at that commit,
    /// `Ok(Some(contents))` if it does, or `Err` for unexpected failures.
    fn source(&self, sha: &str) -> error::Result<Option<String>> {
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
            return Err(error::Error::Git(format!(
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
            tracing::error!(repo = %event.repo, error = format!("{e:#}"), "invalid repo name in event");
            return;
        }
    };

    let secrets = match quire.global_config() {
        Ok(config) => config.secrets,
        Err(e) => {
            tracing::error!(repo = %event.repo, error = %display_chain(&e), "failed to load global config");
            return;
        }
    };

    let db_path = quire.db_path();
    for push_ref in event.updated_refs() {
        if let Err(e) = trigger_ref(
            &repo,
            &db_path,
            event.pushed_at,
            push_ref,
            &secrets,
            run::Executor::Host,
        ) {
            tracing::error!(
                repo = %event.repo,
                sha = %push_ref.new_sha, // cov-excl-line
                error = %display_chain(&e),
                "CI trigger failed"
            );
        }
    }
}

/// Create and run CI for a single updated ref.
fn trigger_ref(
    repo: &Repo,
    db_path: &Path,
    pushed_at: jiff::Timestamp,
    push_ref: &PushRef,
    secrets: &HashMap<String, crate::secret::SecretString>,
    executor: run::Executor,
) -> error::Result<()> {
    let ci = repo.ci();

    let Some(source) = ci.source(&push_ref.new_sha)? else {
        return Ok(());
    };

    let meta = RunMeta {
        sha: push_ref.new_sha.clone(),
        r#ref: push_ref.r#ref.clone(),
        pushed_at,
    };

    let mut run = repo.runs(db_path).create(&meta)?;

    tracing::info!(
        run_id = %run.id(), // cov-excl-line
        sha = %push_ref.new_sha,
        r#ref = %push_ref.r#ref,
        "created CI run"
    );

    let pipeline = match ci.compile(&source) {
        Ok(p) => p,
        Err(e) => {
            run.transition(RunState::Active)?;
            run.transition(RunState::Failed)?;
            return Err(e);
        }
    };

    let workspace = run.path().join("workspace");
    run::materialize_workspace(&repo.path(), &push_ref.new_sha, &workspace)?;
    run.execute(
        pipeline,
        secrets.clone(),
        &repo.path(),
        &workspace,
        executor,
    )?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Quire;
    use crate::event::PushRef;
    use std::path::Path;

    fn git_in(cwd: &Path, args: &[&str]) {
        let output = std::process::Command::new("git")
            .args(args)
            .current_dir(cwd)
            .env("GIT_AUTHOR_NAME", "test")
            .env("GIT_AUTHOR_EMAIL", "test@test")
            .env("GIT_COMMITTER_NAME", "test")
            .env("GIT_COMMITTER_EMAIL", "test@test")
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("GIT_CONFIG_SYSTEM", "/dev/null")
            .output()
            .expect("git command");
        assert!(output.status.success());
    }

    /// Create a bare repo with one commit containing `.quire/ci.fnl`.
    /// Returns the tempdir, the Quire, and the repo name.
    fn bare_repo_with_ci(source: &str) -> (tempfile::TempDir, Quire, String) {
        let dir = tempfile::tempdir().expect("tempdir");
        let work = dir.path().join("work");
        let bare = dir.path().join("repos").join("test.git");

        fs_err::create_dir_all(&work).expect("mkdir work");
        git_in(&work, &["init", "-b", "main"]);
        git_in(&work, &["commit", "--allow-empty", "-m", "initial"]);

        let ci_dir = work.join(".quire");
        fs_err::create_dir_all(&ci_dir).expect("mkdir .quire");
        fs_err::write(ci_dir.join("ci.fnl"), source).expect("write ci.fnl");
        git_in(&work, &["add", "."]);
        git_in(&work, &["commit", "-m", "add ci.fnl"]);

        git_in(
            work.parent().unwrap(),
            &[
                "clone",
                "--bare",
                work.to_str().unwrap(),
                bare.to_str().unwrap(),
            ],
        );

        let quire = Quire::new(dir.path().to_path_buf());
        // Initialize the database.
        let mut db = crate::db::open(&quire.db_path()).expect("init db");
        crate::db::migrate(&mut db).expect("migrate db");
        drop(db);
        (dir, quire, "test.git".to_string())
    }

    fn bare_repo_without_ci() -> (tempfile::TempDir, Quire, String) {
        let dir = tempfile::tempdir().expect("tempdir");
        let work = dir.path().join("work");
        let bare = dir.path().join("repos").join("test.git");

        fs_err::create_dir_all(&work).expect("mkdir work");
        git_in(&work, &["init", "-b", "main"]);
        git_in(&work, &["commit", "--allow-empty", "-m", "initial"]);
        git_in(
            work.parent().unwrap(),
            &[
                "clone",
                "--bare",
                work.to_str().unwrap(),
                bare.to_str().unwrap(),
            ],
        );

        let quire = Quire::new(dir.path().to_path_buf());
        let mut db = crate::db::open(&quire.db_path()).expect("init db");
        crate::db::migrate(&mut db).expect("migrate db");
        drop(db);
        (dir, quire, "test.git".to_string())
    }

    fn head_sha(repo: &crate::quire::Repo) -> String {
        let output = std::process::Command::new("git")
            .args(["-C", repo.path().to_str().unwrap(), "rev-parse", "HEAD"])
            .output()
            .expect("rev-parse");
        String::from_utf8(output.stdout).unwrap().trim().to_string()
    }

    #[test]
    fn ci_pipeline_returns_none_when_no_ci_fnl() {
        let (_dir, quire, name) = bare_repo_without_ci();
        let repo = quire.repo(&name).expect("repo");
        let ci = repo.ci();
        let sha = head_sha(&repo);
        let commit = CommitRef {
            sha: sha.clone(),
            display: sha,
        };
        let result = ci.pipeline(&commit).expect("pipeline should not error");
        assert!(result.is_none(), "no ci.fnl should return None");
    }

    #[test]
    fn ci_pipeline_returns_pipeline_when_ci_fnl_present() {
        let source = r#"(local ci (require :quire.ci))
(ci.job :build [:quire/push] (fn [_] nil))"#;
        let (_dir, quire, name) = bare_repo_with_ci(source);
        let repo = quire.repo(&name).expect("repo");
        let ci = repo.ci();
        let sha = head_sha(&repo);
        let commit = CommitRef {
            sha: sha.clone(),
            display: sha,
        };
        let pipeline = ci
            .pipeline(&commit)
            .expect("pipeline should succeed")
            .expect("should have pipeline");
        assert_eq!(pipeline.jobs().len(), 1);
        assert_eq!(pipeline.jobs()[0].id, "build");
    }

    #[test]
    fn ci_pipeline_errors_on_invalid_fennel() {
        let source = "{:bad {:}";
        let (_dir, quire, name) = bare_repo_with_ci(source);
        let repo = quire.repo(&name).expect("repo");
        let ci = repo.ci();
        let sha = head_sha(&repo);
        let commit = CommitRef {
            sha: sha.clone(),
            display: sha,
        };
        let result = ci.pipeline(&commit);
        assert!(result.is_err(), "bad Fennel should fail");
    }

    #[test]
    fn ci_source_reads_file_at_sha() {
        let source = "(local ci (require :quire.ci))\n(ci.job :x [:quire/push] (fn [_] nil))";
        let (_dir, quire, name) = bare_repo_with_ci(source);
        let repo = quire.repo(&name).expect("repo");
        let ci = repo.ci();
        let sha = head_sha(&repo);
        let content = ci
            .source(&sha)
            .expect("source should succeed")
            .expect("should have content");
        assert!(content.contains(":x"));
    }

    #[test]
    fn trigger_creates_run_and_completes() {
        let source = r#"(local ci (require :quire.ci))
(ci.job :build [:quire/push] (fn [_] nil))"#;
        let (_dir, quire, name) = bare_repo_with_ci(source);
        let repo = quire.repo(&name).expect("repo");
        let sha = head_sha(&repo);
        let pushed_at: jiff::Timestamp = "2026-04-28T12:00:00Z".parse().unwrap();
        let push_ref = PushRef {
            old_sha: "0000000000000000000000000000000000000000".to_string(),
            new_sha: sha.clone(),
            r#ref: "refs/heads/main".to_string(),
        };

        trigger_ref(
            &repo,
            &quire.db_path(),
            pushed_at,
            &push_ref,
            &HashMap::new(),
            run::Executor::Host,
        )
        .expect("trigger_ref should succeed");

        // Verify the run completed (no pending or active rows left behind).
        let conn = crate::db::open(&quire.db_path()).expect("db");
        let count: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM runs WHERE state IN ('pending', 'active')",
                [],
                |row| row.get(0),
            )
            .expect("count");
        assert_eq!(count, 0, "run should be complete, not orphaned");
    }

    #[test]
    fn trigger_skips_when_no_ci_fnl() {
        let (_dir, quire, name) = bare_repo_without_ci();
        let repo = quire.repo(&name).expect("repo");
        let sha = head_sha(&repo);
        let pushed_at: jiff::Timestamp = "2026-04-28T12:00:00Z".parse().unwrap();
        let push_ref = PushRef {
            old_sha: "0000000000000000000000000000000000000000".to_string(),
            new_sha: sha,
            r#ref: "refs/heads/main".to_string(),
        };

        trigger_ref(
            &repo,
            &quire.db_path(),
            pushed_at,
            &push_ref,
            &HashMap::new(),
            run::Executor::Host,
        )
        .expect("should succeed without ci.fnl");
    }

    #[test]
    fn trigger_errors_on_invalid_pipeline() {
        let source = "(local ci (require :quire.ci))\n(ci.job :a [] (fn [_] nil))";
        let (_dir, quire, name) = bare_repo_with_ci(source);
        let repo = quire.repo(&name).expect("repo");
        let sha = head_sha(&repo);
        let pushed_at: jiff::Timestamp = "2026-04-28T12:00:00Z".parse().unwrap();
        let push_ref = PushRef {
            old_sha: "0000000000000000000000000000000000000000".to_string(),
            new_sha: sha,
            r#ref: "refs/heads/main".to_string(),
        };

        let result = trigger_ref(
            &repo,
            &quire.db_path(),
            pushed_at,
            &push_ref,
            &HashMap::new(),
            run::Executor::Host,
        );
        assert!(result.is_err(), "invalid pipeline should fail");
    }

    fn push_event(repo: &str, sha: &str) -> PushEvent {
        PushEvent::new(
            repo.to_string(),
            vec![PushRef {
                old_sha: "0000000000000000000000000000000000000000".to_string(),
                new_sha: sha.to_string(),
                r#ref: "refs/heads/main".to_string(),
            }],
        )
    }

    #[test]
    fn trigger_skips_nonexistent_repo() {
        let dir = tempfile::tempdir().expect("tempdir");
        let quire = Quire::new(dir.path().to_path_buf());
        let event = push_event("no-such.git", "abc123");
        // Should not panic — just logs and returns.
        trigger(&quire, &event);
    }

    #[test]
    fn trigger_skips_repo_not_on_disk() {
        let dir = tempfile::tempdir().expect("tempdir");
        let quire = Quire::new(dir.path().to_path_buf());
        // repo name is valid but directory doesn't exist.
        let event = push_event("missing.git", "abc123");
        trigger(&quire, &event);
    }

    #[test]
    fn trigger_skips_invalid_repo_name() {
        let dir = tempfile::tempdir().expect("tempdir");
        let quire = Quire::new(dir.path().to_path_buf());
        // Repo name with path traversal — quire.repo() returns Err.
        let event = push_event("../evil.git", "abc123");
        trigger(&quire, &event);
    }

    #[test]
    fn trigger_processes_multiple_refs() {
        let source = r#"(local ci (require :quire.ci))
(ci.job :build [:quire/push] (fn [_] nil))"#;
        let (_dir, quire, name) = bare_repo_with_ci(source);
        let repo = quire.repo(&name).expect("repo");
        let sha = head_sha(&repo);
        let _pushed_at: jiff::Timestamp = "2026-04-28T12:00:00Z".parse().unwrap();

        // Two updated refs — both should create runs.
        let event = PushEvent::new(
            name.clone(),
            vec![
                PushRef {
                    old_sha: "0000000000000000000000000000000000000000".to_string(),
                    new_sha: sha.clone(),
                    r#ref: "refs/heads/main".to_string(),
                },
                PushRef {
                    old_sha: "0000000000000000000000000000000000000000".to_string(),
                    new_sha: sha.clone(),
                    r#ref: "refs/tags/v1".to_string(),
                },
            ],
        );

        trigger(&quire, &event);
    }

    #[test]
    fn ci_source_errors_on_invalid_sha() {
        let source = r#"(local ci (require :quire.ci))
(ci.job :build [:quire/push] (fn [_] nil))"#;
        let (_dir, quire, name) = bare_repo_with_ci(source);
        let repo = quire.repo(&name).expect("repo");
        let ci = repo.ci();
        // Use a SHA that doesn't exist — git show will fail with
        // "invalid object name" which doesn't match the does-not-exist filter.
        let result = ci.source("abcdef1234567890");
        let Err(e) = result else { unreachable!() };
        let msg = e.to_string();
        assert!(
            msg.contains("failed to read"),
            "expected git read error, got: {msg}"
        );
    }
}
