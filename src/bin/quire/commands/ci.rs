use std::path::PathBuf;

use miette::{IntoDiagnostic, Result};
use quire::ci::Ci;

/// Validate a repo's ci.fnl without executing any jobs.
///
/// Evaluates the Fennel source at the given SHA (or HEAD) to extract
/// the job registration table, then runs the four structural validations.
/// Prints each job found and any validation errors.
pub async fn validate(sha: Option<&str>) -> Result<()> {
    let repo_path = discover_repo()?;
    let sha = sha.unwrap_or("HEAD");
    let ci = Ci::new(repo_path);

    let Some(result) = ci.eval(sha)? else {
        println!("No ci.fnl found at {sha}.");
        return Ok(());
    };

    if result.jobs.is_empty() {
        println!("No jobs registered.");
        return Ok(());
    }

    println!("Jobs:");
    for job in &result.jobs {
        let inputs = job.inputs.join(", ");
        println!("  {} ← [{}]", job.id, inputs);
    }

    match quire::ci::validate(&result.jobs) {
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

/// Find the git repo root from the current working directory.
fn discover_repo() -> Result<PathBuf> {
    let output = std::process::Command::new("git")
        .args(["rev-parse", "--show-toplevel"])
        .output()
        .into_diagnostic()?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        miette::bail!("not in a git repository: {stderr}");
    }

    let path = String::from_utf8(output.stdout).into_diagnostic()?;
    Ok(PathBuf::from(path.trim()))
}
