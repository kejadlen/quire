use std::os::unix::process::CommandExt;
use std::process::Command;

use miette::{Context, IntoDiagnostic, Result, bail, ensure};

use quire::Quire;

const GIT_COMMANDS: &[&str] = &["git-receive-pack", "git-upload-pack", "git-upload-archive"];

pub async fn run(quire: &Quire, command: Vec<String>) -> Result<()> {
    let input = if command.len() == 1 {
        // Single argument: the full SSH_ORIGINAL_COMMAND string.
        // e.g. git-receive-pack '/foo.git'
        command[0].clone()
    } else {
        // Already split into words (e.g. from CLI: quire exec git-receive-pack /foo.git).
        command.join(" ")
    };

    let words = shell_words::split(&input)
        .into_diagnostic()
        .context("failed to parse command")?;

    ensure!(!words.is_empty(), "no command provided");

    let cmd = &words[0];

    if GIT_COMMANDS.contains(&cmd.as_str()) {
        dispatch_git(quire, cmd, &words[1..])
    } else if cmd == "quire" {
        dispatch_quire(&words[1..])
    } else {
        bail!("unsupported command: {cmd}");
    }
}

fn dispatch_git(quire: &Quire, git_cmd: &str, args: &[String]) -> Result<()> {
    ensure!(
        args.len() == 1,
        "expected usage: {git_cmd} '<repo>', got {} arguments",
        args.len()
    );

    let path = args[0].trim_start_matches('/');
    ensure!(!path.is_empty(), "empty repository path");

    let repo = quire.repo(path)?;

    tracing::info!(%git_cmd, %path, "dispatching git command");
    // Use `git <subcommand>` instead of `git-<subcommand>` so the git
    // binary handles dispatch to libexec/git-core/ internally.
    let Some(subcommand) = git_cmd.strip_prefix("git-") else {
        bail!("unexpected git command: {git_cmd}");
    };
    let err = Command::new("git")
        .arg(subcommand)
        .arg(".")
        .current_dir(repo.path())
        .exec();

    bail!("exec failed: {err}");
}

fn dispatch_quire(args: &[String]) -> Result<()> {
    ensure!(!args.is_empty(), "no quire subcommand provided");

    // Allowlist: `repo <any>` and `mirror push <repo> <ref>`. An explicit mirror
    // push grants no new capability over SSH — pushing already mirrors every
    // updated ref — it just re-triggers after a mirror-side failure.
    let words: Vec<&str> = args.iter().map(String::as_str).collect();
    ensure!(
        matches!(words.as_slice(), ["repo", ..] | ["mirror", "push", ..]),
        "unsupported quire command: {}",
        args.join(" ")
    );

    tracing::info!(subcmd = %args[0], "dispatching quire command");
    let err = Command::new("quire").args(args).exec();
    bail!("exec failed: {err}");
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mirror_dispatch_requires_push_subcommand() {
        assert!(dispatch_quire(&["mirror".into()]).is_err());
        assert!(dispatch_quire(&["mirror".into(), "status".into()]).is_err());
    }

    #[test]
    fn non_allowlisted_quire_commands_are_rejected() {
        // `serve` and `ci` are real subcommands that must stay undispatchable
        // over SSH; the accept paths exec and can't run in-process.
        assert!(dispatch_quire(&["serve".into()]).is_err());
        assert!(dispatch_quire(&["ci".into(), "run".into()]).is_err());
    }
}
