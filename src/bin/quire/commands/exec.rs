use std::os::unix::process::CommandExt;
use std::process::Command;

use miette::{Context, IntoDiagnostic, Result, bail, ensure};

use quire::Config;
use quire::repo::Repo;

const GIT_COMMANDS: &[&str] = &["git-receive-pack", "git-upload-pack", "git-upload-archive"];

pub async fn run(config: &Config, command: Vec<String>) -> Result<()> {
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
        dispatch_git(config, cmd, &words[1..])
    } else if cmd == "quire" {
        dispatch_quire(config, &words[1..])
    } else {
        bail!("unsupported command: {cmd}")
    }
}

fn dispatch_git(config: &Config, git_cmd: &str, args: &[String]) -> Result<()> {
    ensure!(
        args.len() == 1,
        "expected usage: {git_cmd} '<repo>', got {} arguments",
        args.len()
    );

    let path = args[0].trim_start_matches('/');
    ensure!(!path.is_empty(), "empty repository path");

    let repo = Repo::from_name(path)?;
    ensure!(
        repo.exists(&config.repos_dir),
        "repository not found: {}",
        repo.name()
    );

    let repo_dir = repo.path(&config.repos_dir);

    tracing::info!(%git_cmd, name = %repo.name(), "dispatching git command");
    let err = Command::new(git_cmd).arg(".").current_dir(&repo_dir).exec();

    bail!("exec failed: {err}")
}

fn dispatch_quire(_config: &Config, args: &[String]) -> Result<()> {
    ensure!(!args.is_empty(), "no quire subcommand provided");

    ensure!(args[0] == "repo", "unsupported quire command: {}", args[0]);

    tracing::info!(subcmd = "repo", "dispatching quire command");
    let err = Command::new("quire").arg("repo").args(&args[1..]).exec();
    bail!("exec failed: {err}")
}
