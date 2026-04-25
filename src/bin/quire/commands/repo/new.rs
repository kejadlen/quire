use std::process::Command;

use miette::{IntoDiagnostic, Result, ensure};

use quire::Config;
use quire::repo::Repo;

pub async fn run(config: &Config, name: &str) -> Result<()> {
    let repo = Repo::from_name(name)?;
    let repo_dir = repo.path(&config.repos_dir);

    ensure!(
        !repo_dir.exists(),
        "repository already exists: {}",
        repo.name()
    );

    // Create parent directory for grouped repos (e.g. work/foo.git).
    if let Some(parent) = repo_dir.parent() {
        fs_err::create_dir_all(parent).into_diagnostic()?;
    }

    let status = Command::new("git")
        .args(["init", "--bare", repo.name()])
        .current_dir(&config.repos_dir)
        .status()
        .into_diagnostic()?;

    ensure!(status.success(), "git init failed");

    tracing::info!(name = %repo.name(), "created repository");
    println!("{}", repo.name());

    Ok(())
}
