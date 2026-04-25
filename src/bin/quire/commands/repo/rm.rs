use miette::{IntoDiagnostic, Result, ensure};

use quire::Config;
use quire::repo::Repo;

pub async fn run(config: &Config, name: &str) -> Result<()> {
    let repo = Repo::from_name(name)?;
    let repo_dir = repo.path(&config.repos_dir);

    ensure!(repo_dir.exists(), "repository not found: {}", repo.name());
    ensure!(repo_dir.is_dir(), "not a directory: {}", repo.name());

    fs_err::remove_dir_all(&repo_dir).into_diagnostic()?;

    // Clean up empty parent directory for grouped repos.
    if let Some(parent) = repo_dir.parent()
        && parent != config.repos_dir
    {
        let _ = fs_err::remove_dir(parent);
    }

    tracing::info!(name = %repo.name(), "removed repository");

    Ok(())
}
