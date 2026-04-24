use std::os::unix::process::CommandExt;
use std::path::Path;
use std::process::Command;

use color_eyre::eyre::{self, Context};
use color_eyre::Result;

const GIT_COMMANDS: &[&str] = &["git-receive-pack", "git-upload-pack", "git-upload-archive"];

const REPOS_DIR: &str = "/var/quire/repos";

pub async fn run(command: Vec<String>) -> Result<()> {
    let input = if command.len() == 1 {
        // Single argument: the full SSH_ORIGINAL_COMMAND string.
        // e.g. git-receive-pack '/foo.git'
        command[0].clone()
    } else {
        // Already split into words (e.g. from CLI: quire exec git-receive-pack /foo.git).
        command.join(" ")
    };

    let words = shell_words::split(&input).context("failed to parse command")?;

    if words.is_empty() {
        eyre::bail!("no command provided");
    }

    let git_cmd = &words[0];

    if !GIT_COMMANDS.contains(&git_cmd.as_str()) {
        eyre::bail!("unsupported command: {git_cmd}");
    }

    if words.len() != 2 {
        eyre::bail!("expected usage: {git_cmd} '<repo>', got {} arguments", words.len() - 1);
    }

    let repo = validate_repo_path(&words[1])?;

    let repo_dir = Path::new(REPOS_DIR).join(&repo);
    if !repo_dir.is_dir() {
        eyre::bail!("repository not found: {repo}");
    }

    tracing::info!(%git_cmd, %repo, "dispatching git command");

    let repo_dir = Path::new(REPOS_DIR).join(&repo);
    let err = Command::new(git_cmd).arg(".").current_dir(&repo_dir).exec();

    Err(eyre::eyre!("exec failed: {err}"))
}

/// Validate a repo path argument from the SSH protocol.
///
/// Git sends paths like '/foo.git'. We strip the leading slash,
/// reject path traversal (..), require a .git suffix, and reject
/// empty or double-slash paths.
fn validate_repo_path(raw: &str) -> Result<String> {
    let path = raw.trim_start_matches('/');

    if path.is_empty() {
        eyre::bail!("empty repository path");
    }

    if path.contains("..") {
        eyre::bail!("invalid repository path: {raw}");
    }

    if !path.ends_with(".git") {
        eyre::bail!("invalid repository path (must end in .git): {raw}");
    }

    if path.contains("//") {
        eyre::bail!("invalid repository path: {raw}");
    }

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
