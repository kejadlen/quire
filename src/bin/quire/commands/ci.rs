use miette::Result;

use quire::ci::{Ci, ValidationError};

/// Validate a ci.fnl file without executing any jobs.
///
/// Evaluates the Fennel source to extract the job registration table,
/// then runs the four structural validations. Prints each job found
/// and any validation errors.
pub async fn validate(path: &std::path::Path) -> Result<()> {
    let result = Ci::validate_file(path)?;

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
                    ValidationError::Cycle { cycle_jobs } => {
                        format!("cycle: {}", cycle_jobs.join(" → "))
                    }
                    ValidationError::EmptyInputs { job_id } => {
                        format!("{job_id}: empty inputs")
                    }
                    ValidationError::Unreachable { job_id } => {
                        format!("{job_id}: unreachable from any source ref")
                    }
                    ValidationError::ReservedSlash { job_id } => {
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
