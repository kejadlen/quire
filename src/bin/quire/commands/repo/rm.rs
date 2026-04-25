use miette::{IntoDiagnostic, Result, ensure};

use quire::Config;
use quire::repo::validate_name;

pub async fn run(config: &Config, name: &str) -> Result<()> {
    let name = validate_name(name)?;
    let repo_dir = config.repos_dir.join(&name);

    ensure!(repo_dir.exists(), "repository not found: {name}");
    ensure!(repo_dir.is_dir(), "not a directory: {name}");

    fs_err::remove_dir_all(&repo_dir).into_diagnostic()?;

    // Clean up empty parent directory for grouped repos.
    if let Some(parent) = repo_dir.parent()
        && parent != config.repos_dir
    {
        let _ = fs_err::remove_dir(parent);
    }

    tracing::info!(%name, "removed repository");

    Ok(())
}
