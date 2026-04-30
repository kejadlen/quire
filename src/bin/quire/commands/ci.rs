use std::path::PathBuf;

use miette::{IntoDiagnostic, Result};
use quire::ci::{Ci, CommitRef};

/// Validate a repo's ci.fnl without executing any jobs.
///
/// `Ci::load` parses the Fennel source and validates the resulting
/// job graph; this command surfaces the registered jobs and any
/// validation errors via the standard miette diagnostic path.
pub async fn validate(maybe_sha: Option<&str>) -> Result<()> {
    let repo_path = discover_repo()?;
    let commit = resolve_commit(maybe_sha)?;
    let ci = Ci::new(repo_path);

    // Structural validation only — no need to resolve secrets.
    let Some(pipeline) = ci.load(&commit, std::collections::HashMap::new())? else {
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
