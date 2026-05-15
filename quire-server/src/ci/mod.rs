//! CI: trigger runs from push events, validate the job graph.

use std::collections::HashMap;

mod run;

pub(crate) mod error;

pub use error::{Error, Result};
pub use quire_core::ci::pipeline::{
    DefinitionError, Diagnostic, Job, Pipeline, PipelineError, StructureError,
};
pub use quire_core::ci::run::RunMeta;
pub use quire_core::ci::transport::ApiSession;
pub use quire_core::ci::{pipeline, registration, runtime};
pub use run::{
    Executor, Run, RunState, Runs, Transport, TransportMode, materialize_workspace,
    reconcile_orphans,
};

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
    /// here — they are passed to `run.execute_via_quire_ci` since they only matter
    /// when the run-fns actually fire.
    ///
    /// Returns `Ok(None)` if the repo has no ci.fnl at that commit.
    /// Errors if the Fennel source fails to parse/evaluate or if the
    /// resulting job graph violates any structural rule.
    pub fn pipeline(&self, commit: &CommitRef) -> error::Result<Option<Pipeline>> {
        let Some(source) = self.source(&commit.sha)? else {
            return Ok(None);
        };
        Ok(Some(pipeline::compile(&source, CI_FNL)?))
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

/// Everything needed to run CI for refs in a single push event, constant
/// across all refs.
struct TriggerContext<'a> {
    run: RunContext<'a>,
    event_repo: &'a str,
    transport_mode: TransportMode,
    port: u16,
    sentry_dsn: Option<String>,
}

/// Repo-level context passed into the inner execution function.
struct RunContext<'a> {
    repo: &'a Repo,
    db_path: &'a Path,
    secrets: &'a HashMap<String, quire_core::secret::SecretString>,
    executor: Executor,
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
            tracing::error!(repo = %event.repo, error = %e, "invalid repo name in event");
            return;
        }
    };

    let config = match quire.global_config() {
        Ok(config) => config,
        Err(e) => {
            tracing::error!(repo = %event.repo, error = &e as &(dyn std::error::Error + 'static), "failed to load global config");
            return;
        }
    };

    let sentry_dsn = config.sentry.as_ref().and_then(|s| match s.dsn.reveal() {
        Ok(dsn) => Some(dsn.to_string()),
        Err(e) => {
            tracing::warn!(
                error = &e as &(dyn std::error::Error + 'static),
                "failed to resolve Sentry DSN, quire-ci runs will skip Sentry",
            );
            None
        }
    });

    let db_path = quire.db_path();
    let ctx = TriggerContext {
        run: RunContext {
            repo: &repo,
            db_path: &db_path,
            secrets: &config.secrets,
            executor: config.ci.executor,
        },
        event_repo: &event.repo,
        transport_mode: config.ci.transport,
        port: config.port,
        sentry_dsn,
    };

    for push_ref in event.updated_refs() {
        // One trace per push_ref. The trace context is set on the
        // orchestrator's scope for the duration of this iteration and
        // propagated to quire-ci through the dispatch file, so a
        // quire-ci panic and the orchestrator-side "CI trigger
        // failed" event end up on the same trace in Sentry. DSN and
        // trace_id travel together — no DSN, no handoff, no trace
        // tagging is observable.
        let trace_id = sentry::protocol::TraceId::default();
        let span_id = sentry::protocol::SpanId::default();
        run_ref(&ctx, event.pushed_at, push_ref, trace_id, span_id);
    }
}

/// Set up Sentry trace scope and run CI for a single ref.
fn run_ref(
    ctx: &TriggerContext<'_>,
    pushed_at: jiff::Timestamp,
    push_ref: &PushRef,
    trace_id: sentry::protocol::TraceId,
    span_id: sentry::protocol::SpanId,
) {
    let transport = Transport::for_new_run(ctx.transport_mode, ctx.port);
    let sentry_handoff =
        ctx.sentry_dsn
            .as_ref()
            .map(|dsn| quire_core::ci::bootstrap::SentryHandoff {
                dsn: dsn.clone(),
                trace_id: trace_id.to_string(),
            });
    sentry::with_scope(
        |scope| {
            scope.set_context(
                "trace",
                sentry::protocol::Context::Trace(Box::new(sentry::protocol::TraceContext {
                    trace_id,
                    span_id,
                    op: Some("quire.ci.run".into()),
                    ..Default::default()
                })),
            );
        },
        || {
            if let Err(e) = run_ref_inner(
                &ctx.run,
                pushed_at,
                push_ref,
                &transport,
                sentry_handoff.as_ref(),
            ) {
                tracing::error!(
                    repo = %ctx.event_repo,
                    sha = %push_ref.new_sha, // cov-excl-line
                    error = &e as &(dyn std::error::Error + 'static),
                    "CI trigger failed",
                );
            }
        },
    );
}

/// Create and run CI for a single updated ref.
fn run_ref_inner(
    ctx: &RunContext<'_>,
    pushed_at: jiff::Timestamp,
    push_ref: &PushRef,
    transport: &Transport,
    sentry: Option<&quire_core::ci::bootstrap::SentryHandoff>,
) -> error::Result<()> {
    let ci = ctx.repo.ci();

    if ci.source(&push_ref.new_sha)?.is_none() {
        return Ok(());
    }

    let meta = RunMeta {
        sha: push_ref.new_sha.clone(),
        r#ref: push_ref.r#ref.clone(),
        pushed_at,
    };

    let run = ctx.repo.runs(ctx.db_path).create(&meta, transport)?;

    tracing::info!(
        run_id = %run.id(), // cov-excl-line
        sha = %push_ref.new_sha,
        r#ref = %push_ref.r#ref,
        "created CI run"
    );

    let workspace = run.path().join("workspace");
    run::materialize_workspace(&ctx.repo.path(), &push_ref.new_sha, &workspace)?;
    match ctx.executor {
        Executor::Process => {
            // Compilation happens inside quire-ci so a malformed ci.fnl is
            // reported once, with the worker's trace context.
            run.execute_via_quire_ci(
                &ctx.repo.path(),
                &workspace,
                &meta,
                ctx.secrets,
                sentry,
                transport,
            )?;
        }
    }
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
(ci.job :build [:quire/push] (fn [] nil))"#;
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
        // 2 = 1 user job (`build`) + the built-in `quire/push` job
        // that `compile` appends to every pipeline.
        assert_eq!(pipeline.job_count(), 2);
        assert!(pipeline.job("build").is_some());
        assert!(pipeline.job("quire/push").is_some());
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

    fn run_ctx<'a>(
        repo: &'a crate::quire::Repo,
        db_path: &'a std::path::Path,
        secrets: &'a HashMap<String, quire_core::secret::SecretString>,
    ) -> RunContext<'a> {
        RunContext {
            repo,
            db_path,
            secrets,
            executor: Executor::Process,
        }
    }

    /// Serialize PATH mutations so concurrent tests don't observe each
    /// other's fake binaries.
    static PATH_MUTEX: std::sync::Mutex<()> = std::sync::Mutex::new(());

    /// Create a temp directory containing a fake `quire-ci` that exits
    /// with the given code. Returns the temp dir (for lifetime) and the
    /// new PATH value.
    fn fake_quire_ci(exit_code: i32) -> (tempfile::TempDir, std::ffi::OsString) {
        let dir = tempfile::tempdir().expect("tempdir for fake quire-ci");
        if cfg!(unix) {
            let path = dir.path().join("quire-ci");
            // For a clean exit, write RunFinished(success) to the --events
            // file so the server can determine the outcome from events alone.
            let script = if exit_code == 0 {
                r#"#!/bin/sh
events=
while [ $# -gt 0 ]; do
  case "$1" in
    --events) events="$2"; shift 2 ;;
    *) shift ;;
  esac
done
if [ -n "$events" ] && [ "$events" != "null" ]; then
  printf '{"at_ms":0,"type":"run_finished","outcome":"success"}\n' > "$events"
fi
exit 0
"#
                .to_string()
            } else {
                format!("#!/bin/sh\nexit {exit_code}\n")
            };
            fs_err::write(&path, script).expect("write fake quire-ci");
            use std::os::unix::fs::PermissionsExt;
            fs_err::set_permissions(&path, std::fs::Permissions::from_mode(0o755))
                .expect("chmod fake quire-ci");
        } else {
            let path = dir.path().join("quire-ci.bat");
            fs_err::write(&path, format!("@echo off\nexit /b {exit_code}\n"))
                .expect("write fake quire-ci");
        }
        let old_path = std::env::var_os("PATH").unwrap_or_default();
        let mut new_path = dir.path().as_os_str().to_owned();
        new_path.push(std::ffi::OsString::from(if cfg!(windows) {
            ";"
        } else {
            ":"
        }));
        new_path.push(&old_path);
        (dir, new_path)
    }

    /// Run a closure with a modified PATH, restoring it afterward.
    /// Acquires a global mutex so concurrent tests don't race on PATH.
    fn with_path<F, R>(new_path: &std::ffi::OsString, f: F) -> R
    where
        F: FnOnce() -> R,
    {
        let _guard = PATH_MUTEX.lock().unwrap();
        let old = std::env::var_os("PATH").unwrap_or_default();
        // SAFETY: the mutex guarantees no other thread is reading or
        // writing PATH during this scope.
        unsafe {
            std::env::set_var("PATH", new_path);
        }
        let result = f();
        unsafe {
            std::env::set_var("PATH", &old);
        }
        result
    }

    #[test]
    fn run_ref_inner_drives_run_to_complete_with_fake_quire_ci() {
        let source = r#"(local ci (require :quire.ci))
(ci.job :build [:quire/push] (fn [] nil))"#;
        let (_dir, quire, name) = bare_repo_with_ci(source);
        let repo = quire.repo(&name).expect("repo");
        let sha = head_sha(&repo);
        let pushed_at: jiff::Timestamp = "2026-04-28T12:00:00Z".parse().unwrap();
        let push_ref = PushRef {
            old_sha: "0000000000000000000000000000000000000000".to_string(),
            new_sha: sha.clone(),
            r#ref: "refs/heads/main".to_string(),
        };

        let (_fake_dir, fake_path) = fake_quire_ci(0);
        let db_path = quire.db_path();
        let secrets = HashMap::new();
        let ctx = run_ctx(&repo, &db_path, &secrets);
        let trigger_result = with_path(&fake_path, || {
            run_ref_inner(&ctx, pushed_at, &push_ref, &Transport::Filesystem, None)
        });

        trigger_result.expect("trigger_ref should succeed with fake quire-ci");

        // The run should have reached complete.
        let conn = crate::db::open(&quire.db_path()).expect("db");
        let state: String = conn
            .query_row(
                "SELECT state FROM runs WHERE sha = ?1",
                rusqlite::params![&sha],
                |row| row.get(0),
            )
            .expect("should have a run");
        assert_eq!(
            state, "complete",
            "run should be complete after fake quire-ci exits 0"
        );

        // No pending or active rows left behind.
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
    fn run_ref_inner_transitions_to_failed_when_process_crashes() {
        let source = r#"(local ci (require :quire.ci))
(ci.job :build [:quire/push] (fn [] nil))"#;
        let (_dir, quire, name) = bare_repo_with_ci(source);
        let repo = quire.repo(&name).expect("repo");
        let sha = head_sha(&repo);
        let pushed_at: jiff::Timestamp = "2026-04-28T12:00:00Z".parse().unwrap();
        let push_ref = PushRef {
            old_sha: "0000000000000000000000000000000000000000".to_string(),
            new_sha: sha.clone(),
            r#ref: "refs/heads/main".to_string(),
        };

        let (_fake_dir, fake_path) = fake_quire_ci(1);
        let db_path = quire.db_path();
        let secrets = HashMap::new();
        let ctx = run_ctx(&repo, &db_path, &secrets);
        let trigger_result = with_path(&fake_path, || {
            run_ref_inner(&ctx, pushed_at, &push_ref, &Transport::Filesystem, None)
        });

        let err = trigger_result.expect_err("should fail when quire-ci exits nonzero");
        assert!(
            err.to_string().contains("quire-ci exited"),
            "expected ProcessFailed error, got: {err}"
        );

        let conn = crate::db::open(&quire.db_path()).expect("db");
        let state: String = conn
            .query_row(
                "SELECT state FROM runs WHERE sha = ?1",
                rusqlite::params![&sha],
                |row| row.get(0),
            )
            .expect("should have a run");
        assert_eq!(
            state, "failed",
            "run should be failed after quire-ci exits 1"
        );
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

        let db_path = quire.db_path();
        let secrets = HashMap::new();
        let ctx = run_ctx(&repo, &db_path, &secrets);
        run_ref_inner(&ctx, pushed_at, &push_ref, &Transport::Filesystem, None)
            .expect("should succeed without ci.fnl");
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
(ci.job :build [:quire/push] (fn [] nil))"#;
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
(ci.job :build [:quire/push] (fn [] nil))"#;
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
