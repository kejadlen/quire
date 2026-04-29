//! CI: trigger runs from push events, validate the job graph.

pub mod graph;
pub mod run;

pub use graph::{EvalResult, JobDef, ValidationError, eval_ci, validate};
pub use run::{Run, RunMeta, RunState, RunTimes, Runs};

use crate::Result;
use crate::event::{PushEvent, PushRef};
use crate::fennel::Fennel;
use crate::quire::Repo;

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
///
/// Returns `Ok(())` if CI ran (regardless of whether the run succeeded
/// or failed), or `Err` if the trigger itself failed.
fn trigger_ref(repo: &Repo, pushed_at: jiff::Timestamp, push_ref: &PushRef) -> Result<()> {
    if !repo.has_ci_fnl(&push_ref.new_sha) {
        return Ok(());
    }

    let meta = RunMeta {
        sha: push_ref.new_sha.clone(),
        r#ref: push_ref.r#ref.clone(),
        pushed_at,
    };

    let mut run = repo.runs().create(&meta)?;

    tracing::info!(
        run_id = %run.id(),
        sha = %push_ref.new_sha,
        r#ref = %push_ref.r#ref,
        "created CI run"
    );

    run.transition(RunState::Active)?;

    let result = eval_and_validate(repo, &push_ref.new_sha);
    match result {
        Ok(()) => {
            run.transition(RunState::Complete)?;
        }
        Err(e) => {
            run.transition(RunState::Failed)?;
            // Return the eval/validation error as the dispatch error.
            Err(e)?;
        }
    }

    Ok(())
}

/// Evaluate ci.fnl at a given SHA and validate the job graph.
fn eval_and_validate(repo: &Repo, sha: &str) -> Result<()> {
    let source = repo.ci_fnl_source(sha)?;
    let fennel = Fennel::new()?;
    let eval_result = eval_ci(&fennel, &source, &format!("{sha}:.quire/ci.fnl"))?;
    validate(&eval_result.jobs)?;
    Ok(())
}
