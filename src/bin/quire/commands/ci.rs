use std::path::PathBuf;

use miette::{IntoDiagnostic, Result};
use quire::ci::{Ci, CommitRef};

/// Validate a repo's ci.fnl without executing any jobs.
///
/// Loads the Fennel source at the given SHA (or HEAD) to extract
/// the job registration table, then runs the four structural validations.
/// Prints each job found and any validation errors.
pub async fn validate(maybe_sha: Option<&str>) -> Result<()> {
    let repo_path = discover_repo()?;
    let commit = resolve_commit(maybe_sha)?;
    let ci = Ci::new(repo_path);

    let Some(pipeline) = ci.load(&commit)? else {
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

    match pipeline.validate() {
        Ok(()) => {
            println!("\nAll validations passed.");
        }
        Err(errors) => {
            println!("\nValidation errors:");
            for err in &errors {
                let label = match err {
                    quire::ci::ValidationError::Cycle { cycle_jobs } => {
                        format!("cycle: {}", cycle_jobs.join(" → "))
                    }
                    quire::ci::ValidationError::EmptyInputs { job_id } => {
                        format!("{job_id}: empty inputs")
                    }
                    quire::ci::ValidationError::Unreachable { job_id } => {
                        format!("{job_id}: unreachable from any source ref")
                    }
                    quire::ci::ValidationError::ReservedSlash { job_id } => {
                        format!("{job_id}: '/' in job id")
                    }
                };
                println!("  ✗ {label}");
            }
            std::process::exit(1);
        }
    }

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

/// Get the git commit ID of the latest committed revision via jj.
fn current_commit() -> Result<String> {
    let output = std::process::Command::new("jj")
        .args([
            "log",
            "--limit",
            "1",
            "--no-graph",
            "-r",
            "@-",
            "-T",
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
