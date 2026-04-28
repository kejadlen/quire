use std::io::{self, IsTerminal};

use miette::{Context, IntoDiagnostic, Result, bail, ensure};
use quire::Quire;

const ZERO_SHA: &str = "0000000000000000000000000000000000000000";

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

    // GIT_DIR is set by git when running hooks in bare repos. When hooks
    // are invoked via hook.<name>.command, GIT_DIR may be relative (e.g.
    // "."), so canonicalize before resolving.
    let git_dir = std::env::var("GIT_DIR")
        .into_diagnostic()
        .context("GIT_DIR not set — hook must run inside a bare repo")
        .and_then(|git_dir| {
            std::path::Path::new(&git_dir)
                .canonicalize()
                .into_diagnostic()
                .context("failed to resolve GIT_DIR")
        })?;

    let repo = quire
        .repo_from_path(&git_dir)
        .context("hook running in unrecognized repo")?;
    ensure!(
        repo.exists(),
        "GIT_DIR points to a non-existent repo: {}",
        git_dir.display()
    );

    // Parse pushed refs from stdin. Each line is:
    //   <old-sha> <new-sha> <refname>
    let stdin = io::stdin();
    let mut refs: Vec<quire::event::PushRef> = Vec::new();
    for line in stdin.lines() {
        let line = line
            .into_diagnostic()
            .context("failed to read hook stdin")?;
        let parts: Vec<&str> = line.split_whitespace().collect();
        if parts.len() != 3 {
            continue;
        }
        refs.push(quire::event::PushRef {
            old_sha: parts[0].to_string(),
            new_sha: parts[1].to_string(),
            r#ref: parts[2].to_string(),
        });
    }

    // Only send an event when at least one ref was actually updated
    // (new sha is not all zeros). Deletions are included in the event
    // payload but don't count as updates on their own.
    let has_updates = refs.iter().any(|r| r.new_sha != ZERO_SHA);
    if !has_updates {
        return Ok(());
    }

    // Resolve repo name relative to repos dir for the event payload.
    let repo_name = repo
        .path()
        .strip_prefix(quire.repos_dir())
        .into_diagnostic()
        .context("repo path not under repos dir")?
        .to_string_lossy()
        .to_string();

    let event = quire::event::build_push_event(repo_name, refs);
    let mut line = serde_json::to_string(&event)
        .into_diagnostic()
        .context("failed to serialize push event")?;
    line.push('\n');

    let socket_path = quire.socket_path();
    if !socket_path.exists() {
        eprintln!(
            "quire: server not running ({}), skipping event",
            socket_path.display()
        );
        return Ok(());
    }

    let mut stream = std::os::unix::net::UnixStream::connect(&socket_path)
        .into_diagnostic()
        .context("failed to connect to event socket")?;
    io::Write::write_all(&mut stream, line.as_bytes())
        .into_diagnostic()
        .context("failed to write event to socket")?;

    tracing::info!(repo = %event.repo, "push event sent to server");
    Ok(())
}
