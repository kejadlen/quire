use std::io::{self, BufRead, IsTerminal};
use std::path::PathBuf;

use miette::{Context, Result, ensure, miette};
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
    // post-receive receives updated refs on stdin. We only care that
    // at least one ref was pushed — we don't need to parse them.
    let stdin = io::stdin();
    if stdin.is_terminal() {
        // Not running as a git hook — nothing to do.
        return Ok(());
    }
    let has_refs = stdin.lock().lines().any(|line| line.is_ok());
    if !has_refs {
        return Ok(());
    }

    // GIT_DIR is set by git when running hooks in bare repos.
    let git_dir = std::env::var("GIT_DIR")
        .map(PathBuf::from)
        .map_err(|e| miette!("GIT_DIR not set — hook must run inside a bare repo: {e}"))?;

    let repo = quire
        .repo_from_path(&git_dir)
        .context("hook running in unrecognized repo")?;

    let repo_config = repo.config()?;
    let mirror = match repo_config.mirror {
        Some(m) => m,
        None => return Ok(()),
    };

    let global_config = quire.global_config()?;
    let token = global_config
        .github
        .token
        .reveal()
        .context("failed to resolve GitHub token")?;

    tracing::info!(url = %mirror.url, "pushing to mirror");

    // Token is passed via -c flag — never written to disk or visible in
    // process arguments (git redacts http.extraHeader in trace output).
    let status = repo
        .git(&["push", "--porcelain", &mirror.url, "main"])
        .env("GIT_CONFIG_COUNT", "1")
        .env("GIT_CONFIG_KEY_0", "http.extraHeader")
        .env(
            "GIT_CONFIG_VALUE_0",
            format!("Authorization: Bearer {token}"),
        )
        .stdout(std::process::Stdio::null())
        .status()
        .map_err(quire::Error::Io)
        .context("failed to run git push")?;

    ensure!(status.success(), "git push to mirror failed");

    tracing::info!(url = %mirror.url, "mirror push complete");
    Ok(())
}
