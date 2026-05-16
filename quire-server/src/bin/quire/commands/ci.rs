use std::path::PathBuf;

use miette::{IntoDiagnostic, Result};
use quire::Quire;
use quire::ci::{Ci, CommitRef, RunMeta, Runs};

/// Validate a repo's ci.fnl without executing any jobs.
///
/// `Ci::pipeline` parses the Fennel source and validates the resulting
/// job graph; this command surfaces the registered jobs and any
/// validation errors via the standard miette diagnostic path.
pub async fn validate(maybe_sha: Option<&str>) -> Result<()> {
    let repo_path = discover_repo()?;
    let commit = resolve_commit(maybe_sha)?;
    let ci = Ci::new(repo_path);

    let Some(pipeline) = ci.pipeline(&commit)? else {
        println!("No ci.fnl found at {}.", commit.display);
        return Ok(());
    };

    if pipeline.job_count() == 0 {
        println!("No jobs registered.");
        return Ok(());
    }

    println!("Jobs:");
    for job in pipeline.jobs() {
        let inputs = job.inputs.join(", ");
        println!("  {} ← [{}]", job.id, inputs);
    }

    println!("\nAll validations passed.");
    Ok(())
}

/// Execute a repo's ci.fnl locally for testing.
///
/// Loads the pipeline at the resolved commit (working-copy `@` by
/// default), creates a transient Run rooted at a tempdir, dispatches
/// to `quire-ci` via `execute`, and prints the combined
/// log to stdout. The tempdir is removed when the command exits.
pub async fn run(quire: &Quire, maybe_sha: Option<&str>) -> Result<()> {
    let repo_path = discover_repo()?;
    let commit = resolve_commit(maybe_sha)?;
    let ci = Ci::new(repo_path.clone());

    // Pull secrets from the global config; absence is fine for local
    // testing. A broken-but-present config is a real error.
    let secrets = match quire.global_config() {
        Ok(c) => c.secrets,
        Err(quire::Error::ConfigNotFound(_)) => std::collections::HashMap::new(),
        Err(e) => return Err(e).into_diagnostic(),
    };

    let Some(_pipeline) = ci.pipeline(&commit)? else {
        println!("No ci.fnl found at {}.", commit.display);
        return Ok(());
    };

    // Tempdir for run artifacts. TODO: switch to an XDG cache dir
    // (e.g. $XDG_CACHE_HOME/quire/local-runs) so logs survive past the
    // command and `tail -f` becomes useful.
    let tmp = tempfile::tempdir().into_diagnostic()?;
    let db_path = tmp.path().join("quire.db");
    let mut db = quire::db::open(&db_path).into_diagnostic()?;
    quire::db::migrate(&mut db).into_diagnostic()?;
    drop(db);
    let runs = Runs::new(db_path, "local".to_string(), tmp.path().to_path_buf());

    let meta = RunMeta {
        sha: commit.sha.clone(),
        r#ref: "@".to_string(),
        pushed_at: jiff::Timestamp::now(),
    };

    let run = runs.create(&meta, None)?;
    let run_id = run.id().to_string();
    println!(
        "Run {}: executing at {} ({})",
        run_id,
        commit.display,
        &commit.sha[..commit.sha.len().min(12)],
    );

    let workspace = tmp.path().join("workspace");
    quire::ci::materialize_workspace(&repo_path.join(".git"), &commit.sha, &workspace)
        .into_diagnostic()?;
    let exec_result = run.execute(
        &repo_path.join(".git"),
        &workspace,
        &meta,
        &secrets,
        None,
        None,
    );

    // Print the combined quire-ci log regardless of outcome.
    let log_path = tmp.path().join(&run_id).join("quire-ci.log");
    match fs_err::read_to_string(&log_path) {
        Ok(log) => print!("{log}"),
        Err(e) => tracing::debug!(
            path = %log_path.display(),
            error = %e,
            "quire-ci log not found (binary may have failed to start)",
        ),
    }

    match exec_result {
        Ok(()) => {
            println!("\nRun complete.");
            Ok(())
        }
        Err(e) => {
            println!("\nRun failed.");
            Err(e).into_diagnostic()
        }
    }
}

/// Find the repo root from the current working directory using jj.
fn resolve_commit(maybe_sha: Option<&str>) -> Result<CommitRef> {
    match maybe_sha {
        Some(s) => Ok(CommitRef {
            sha: s.to_string(),
            display: s.to_string(),
        }),
        None => {
            let sha = current_commit()?;
            Ok(CommitRef {
                sha,
                display: "@".to_string(),
            })
        }
    }
}

fn discover_repo() -> Result<PathBuf> {
    let output = std::process::Command::new("jj")
        .args(["root"])
        .output()
        .into_diagnostic()?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        miette::bail!("not in a jj repo: {stderr}");
    }

    let path = String::from_utf8(output.stdout).into_diagnostic()?;
    Ok(PathBuf::from(path.trim()))
}

/// Get the git commit ID of the working copy revision via jj.
///
/// Resolves `@`, which jj snapshots into a real commit on every
/// invocation, so the SHA reflects uncommitted edits.
fn current_commit() -> Result<String> {
    let output = std::process::Command::new("jj")
        .args([
            "log",
            "--limit",
            "1",
            "--no-graph",
            "--revisions",
            "@",
            "--template",
            "commit_id",
        ])
        .output()
        .into_diagnostic()?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        miette::bail!("failed to get current commit: {stderr}");
    }

    let sha = String::from_utf8(output.stdout).into_diagnostic()?;
    Ok(sha.trim().to_string())
}
