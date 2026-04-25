use std::process::Command;

use miette::{IntoDiagnostic, Result, ensure};

use quire::Config;
use quire::repo::validate_name;

pub async fn run(config: &Config, name: &str) -> Result<()> {
    let name = validate_name(name)?;
    let repo_dir = config.repos_dir.join(&name);

    ensure!(
        !repo_dir.exists(),
        "repository already exists: {name}"
    );

    // Create parent directory for grouped repos (e.g. work/foo.git).
    if let Some(parent) = repo_dir.parent() {
        fs_err::create_dir_all(parent).into_diagnostic()?;
    }

    let status = Command::new("git")
        .args(["init", "--bare", &name])
        .current_dir(&config.repos_dir)
        .status()
        .into_diagnostic()?;

    ensure!(status.success(), "git init failed");

    tracing::info!(%name, "created repository");
    println!("{name}");

    Ok(())
}
