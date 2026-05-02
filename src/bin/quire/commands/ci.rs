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

    let jobs = pipeline.jobs();
    if jobs.is_empty() {
        println!("No jobs registered.");
        return Ok(());
    }

    println!("Jobs:");
    for job in jobs {
        let inputs = job.inputs.join(", ");
        println!("  {} ← [{}]", job.id, inputs);
    }

    println!("\nAll validations passed.");
    Ok(())
}

/// Execute a repo's ci.fnl locally for testing.
///
/// Loads the pipeline at the resolved commit (working-copy `@` by
/// default), creates a transient Run rooted at a tempdir, drives the
/// pipeline through it, and prints each job's `(ci.sh …)` output to
/// stdout. The tempdir is removed when the command exits.
pub async fn run(quire: &Quire, maybe_sha: Option<&str>) -> Result<()> {
    let repo_path = discover_repo()?;
    let commit = resolve_commit(maybe_sha)?;
    let ci = Ci::new(repo_path.clone());

    // Pull secrets from the global config; absence is fine for local
    // testing. A broken-but-present config is a real error. Secrets
    // are passed to `Run::execute` rather than `Ci::pipeline` since they
    // only matter when the run-fns actually fire.
    let secrets = match quire.global_config() {
        Ok(c) => c.secrets,
        Err(quire::Error::ConfigNotFound(_)) => std::collections::HashMap::new(),
        Err(e) => return Err(e).into_diagnostic(),
    };

    let Some(pipeline) = ci.pipeline(&commit)? else {
        println!("No ci.fnl found at {}.", commit.display);
        return Ok(());
    };

    // Tempdir for run artifacts. TODO: switch to an XDG cache dir
    // (e.g. $XDG_CACHE_HOME/quire/local-runs) so logs survive past the
    // command and `tail -f` becomes useful.
    let tmp = tempfile::tempdir().into_diagnostic()?;
    let runs = Runs::new(tmp.path().to_path_buf());

    let meta = RunMeta {
        sha: commit.sha.clone(),
        r#ref: "@".to_string(),
        pushed_at: jiff::Timestamp::now(),
    };

    let run = runs.create(&meta)?;
    println!("Run {}: executing at {}", run.id(), commit.display);

    let exec_result = run.execute(pipeline, secrets, &repo_path.join(".git"));

    match exec_result {
        Ok(outputs) => {
            for (job_id, job_outputs) in &outputs {
                if job_outputs.is_empty() {
                    continue;
                }
                println!("\n==> {}", job_id);
                for o in job_outputs {
                    if !o.stdout.is_empty() {
                        print!("{}", o.stdout);
                    }
                    if !o.stderr.is_empty() {
                        eprint!("{}", o.stderr);
                    }
                    if o.exit != 0 {
                        println!("(exit {})", o.exit);
                    }
                }
            }
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
