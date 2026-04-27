use std::io::{self, IsTerminal};
use std::path::PathBuf;

use miette::{Context, Result, bail, ensure, miette};
use quire::Quire;

#[derive(Clone, Copy, Debug, clap::ValueEnum)]
pub enum HookName {
    PostReceive,
}

impl std::fmt::Display for HookName {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let name = match self {
            HookName::PostReceive => "post-receive",
        };
        f.write_str(name)
    }
}

pub async fn run(quire: &Quire, hook_name: HookName) -> Result<()> {
    match hook_name {
        HookName::PostReceive => post_receive(quire),
    }
}

fn post_receive(quire: &Quire) -> Result<()> {
    // git invokes hooks with refs piped on stdin. A terminal here means
    // a human typed `quire hook post-receive` directly — that's a misuse,
    // not a no-op.
    if io::stdin().is_terminal() {
        bail!("quire hook is for git to invoke, not for direct CLI use");
    }

    // GIT_DIR is set by git when running hooks in bare repos.
    let git_dir = std::env::var("GIT_DIR")
        .map(PathBuf::from)
        .map_err(|e| miette!("GIT_DIR not set — hook must run inside a bare repo: {e}"))?;

    let repo = quire
        .repo_from_path(&git_dir)
        .context("hook running in unrecognized repo")?;
    ensure!(
        repo.exists(),
        "GIT_DIR points to a non-existent repo: {}",
        git_dir.display()
    );

    let repo_config = repo.config()?;
    let Some(mirror) = repo_config.mirror else {
        return Ok(());
    };

    let global_config = quire.global_config()?;
    let token = global_config
        .github
        .token
        .reveal()
        .context("failed to resolve GitHub token")?;

    // Parse pushed refs from stdin. Each line is:
    //   <old-sha> <new-sha> <refname>
    // Only push refs that were actually updated (new sha is not all zeros).
    let stdin = io::stdin();
    let mut refs: Vec<String> = Vec::new();
    for line in stdin.lines() {
        let line = line.map_err(|e| miette!("failed to read hook stdin: {e}"))?;
        let parts: Vec<&str> = line.split_whitespace().collect();
        if parts.len() != 3 {
            continue;
        }
        let new_sha = parts[1];
        if new_sha == "0000000000000000000000000000000000000000" {
            continue;
        }
        refs.push(parts[2].to_string());
    }

    if refs.is_empty() {
        return Ok(());
    }

    let ref_slices: Vec<&str> = refs.iter().map(|s| s.as_str()).collect();
    tracing::info!(url = %mirror.url, refs = ?ref_slices, "pushing to mirror");
    repo.push_to_mirror(&mirror, token, &ref_slices)?;
    tracing::info!(url = %mirror.url, "mirror push complete");
    Ok(())
}
