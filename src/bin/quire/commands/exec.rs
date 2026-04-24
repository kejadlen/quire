use std::os::unix::process::CommandExt;
use std::process::Command;

use miette::{Context, IntoDiagnostic, Result, bail, ensure};

use quire::Config;

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

    let git_cmd = &words[0];

    ensure!(
        GIT_COMMANDS.contains(&git_cmd.as_str()),
        "unsupported command: {git_cmd}"
    );

    ensure!(
        words.len() == 2,
        "expected usage: {git_cmd} '<repo>', got {} arguments",
        words.len() - 1
    );

    let repo = validate_repo_path(&words[1])?;

    let repo_dir = config.repos_dir.join(&repo);
    ensure!(repo_dir.is_dir(), "repository not found: {repo}");

    tracing::info!(%git_cmd, %repo, "dispatching git command");
    let err = Command::new(git_cmd).arg(".").current_dir(&repo_dir).exec();

    bail!("exec failed: {err}")
}

/// Validate a repo path argument from the SSH protocol.
///
/// Git sends paths like '/foo.git'. We strip the leading slash,
/// reject path traversal (..), require a .git suffix, and reject
/// empty or double-slash paths.
fn validate_repo_path(raw: &str) -> Result<String> {
    let path = raw.trim_start_matches('/');

    ensure!(!path.is_empty(), "empty repository path");
    ensure!(!path.contains(".."), "invalid repository path: {raw}");
    ensure!(
        path.ends_with(".git"),
        "invalid repository path (must end in .git): {raw}"
    );
    ensure!(!path.contains("//"), "invalid repository path: {raw}");

    Ok(path.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn valid_repo_paths() {
        assert_eq!(validate_repo_path("/foo.git").unwrap(), "foo.git");
        assert_eq!(validate_repo_path("foo.git").unwrap(), "foo.git");
        assert_eq!(validate_repo_path("/work/foo.git").unwrap(), "work/foo.git");
    }

    #[test]
    fn rejects_traversal() {
        assert!(validate_repo_path("/../etc/passwd").is_err());
        assert!(validate_repo_path("/foo/../../bar.git").is_err());
    }

    #[test]
    fn rejects_no_git_suffix() {
        assert!(validate_repo_path("/foo").is_err());
    }

    #[test]
    fn rejects_empty() {
        assert!(validate_repo_path("").is_err());
        assert!(validate_repo_path("/").is_err());
    }

    #[test]
    fn rejects_double_slash() {
        assert!(validate_repo_path("/foo//bar.git").is_err());
    }
}
