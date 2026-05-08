use std::process::Command;

use miette::{IntoDiagnostic, Result, ensure};

use quire::Quire;

pub async fn new(quire: &Quire, name: &str) -> Result<()> {
    let repo = quire.repo(name)?;
    ensure!(!repo.exists(), "repository already exists: {name}");

    // Create parent directory for grouped repos (e.g. work/foo.git).
    if let Some(parent) = repo.path().parent() {
        fs_err::create_dir_all(parent).into_diagnostic()?;
    }

    let status = Command::new("git")
        .args(["init", "--bare", "--initial-branch=main", name])
        .current_dir(quire.repos_dir())
        .status()
        .into_diagnostic()?;

    ensure!(status.success(), "git init failed");

    tracing::info!(%name, "created repository");
    println!("{name}");

    Ok(())
}

pub async fn list(quire: &Quire) -> Result<()> {
    for repo in quire.repos()? {
        println!("{}", repo.name());
    }
    Ok(())
}

pub async fn rm(quire: &Quire, name: &str) -> Result<()> {
    let repo = quire.repo(name)?;
    ensure!(repo.exists(), "repository not found: {name}");

    fs_err::remove_dir_all(repo.path()).into_diagnostic()?;

    // Clean up empty parent directory for grouped repos.
    if let Some(parent) = repo.path().parent()
        && parent != quire.repos_dir()
    {
        let _ = fs_err::remove_dir(parent);
    }

    tracing::info!(%name, "removed repository");

    Ok(())
}
