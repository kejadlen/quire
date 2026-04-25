use std::process::Command;

use miette::{IntoDiagnostic, Result, ensure};

use quire::Config;
use quire::repo::Repo;

pub async fn new(config: &Config, name: &str) -> Result<()> {
    let repo = Repo::from_name(name)?;
    ensure!(
        !repo.exists(&config.repos_dir),
        "repository already exists: {}",
        repo.name()
    );

    let repo_dir = repo.path(&config.repos_dir);

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

pub async fn list(config: &Config) -> Result<()> {
    let entries = fs_err::read_dir(&config.repos_dir).into_diagnostic()?;

    let mut repos: Vec<String> = Vec::new();
    for entry in entries {
        let entry = entry.into_diagnostic()?;
        let path = entry.path();

        if !path.is_dir() {
            continue;
        }

        let Ok(relative) = path.strip_prefix(&config.repos_dir) else {
            continue;
        };
        let name = relative.to_string_lossy();

        // Top-level .git directory.
        if name.ends_with(".git") {
            repos.push(name.to_string());
            continue;
        }

        // Group directory — collect .git children.
        let Ok(children) = fs_err::read_dir(&path) else {
            continue;
        };
        for child in children {
            let child = child.into_diagnostic()?;
            let child_name = child.file_name();
            let child_name = child_name.to_string_lossy();
            if child_name.ends_with(".git") && child.path().is_dir() {
                let full = format!("{}/{}", name, child_name);
                repos.push(full);
            }
        }
    }

    repos.sort();
    for repo in &repos {
        println!("{repo}");
    }

    Ok(())
}

pub async fn rm(config: &Config, name: &str) -> Result<()> {
    let repo = Repo::from_name(name)?;
    ensure!(
        repo.exists(&config.repos_dir),
        "repository not found: {}",
        repo.name()
    );

    let repo_dir = repo.path(&config.repos_dir);

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
