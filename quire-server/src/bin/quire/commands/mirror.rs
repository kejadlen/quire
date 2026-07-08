use miette::{Result, bail};

use quire::Quire;
use quire::mirror;
use quire::quire::RepoName;

/// Push a single ref to the repo's configured mirrors, reporting the outcome.
///
/// Exits non-zero if any mirror rejects the ref, even when others accept it.
pub async fn push(quire: &Quire, repo: &RepoName, ref_name: &str) -> Result<()> {
    let outcome = mirror::push_ref(quire, repo.as_str(), ref_name)?;

    if outcome.pushed.is_empty() && outcome.failed.is_empty() {
        println!("no mirrors configured for {repo}");
        return Ok(());
    }

    for url in &outcome.pushed {
        println!("pushed {ref_name} to {url}");
    }

    if !outcome.failed.is_empty() {
        for failure in &outcome.failed {
            eprintln!(
                "failed to push {ref_name} to {}: {}",
                failure.url, failure.cause
            );
        }
        let total = outcome.pushed.len() + outcome.failed.len();
        bail!(
            "{} of {total} mirror(s) rejected {ref_name}",
            outcome.failed.len()
        );
    }

    Ok(())
}
