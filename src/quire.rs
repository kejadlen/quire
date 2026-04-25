use std::path::{Path, PathBuf};

use miette::{IntoDiagnostic, Result, ensure};

use crate::config::Config;

/// Application runtime context.
///
/// Carries configuration and provides resolved paths to repositories.
/// Commands receive a `&Quire` instead of threading `&Config` around.
pub struct Quire {
    config: Config,
}

impl Quire {
    pub fn new(config: Config) -> Self {
        Self { config }
    }

    pub fn repos_dir(&self) -> &Path {
        &self.config.repos_dir
    }

    /// Validate a repository name and return its resolved path.
    ///
    /// Rejects path traversal, missing `.git` suffix, empty segments,
    /// reserved path components, and more than one level of grouping.
    pub fn repo(&self, name: &str) -> Result<PathBuf> {
        validate_repo_name(name)?;
        Ok(self.config.repos_dir.join(name))
    }

    /// List all repository names under the repos directory.
    pub fn repos(&self) -> Result<impl Iterator<Item = String> + '_> {
        let entries = fs_err::read_dir(&self.config.repos_dir).into_diagnostic()?;

        let mut repos: Vec<String> = Vec::new();
        for entry in entries {
            let entry = entry.into_diagnostic()?;
            let path = entry.path();

            if !path.is_dir() {
                continue;
            }

            let Ok(relative) = path.strip_prefix(&self.config.repos_dir) else {
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
        Ok(repos.into_iter())
    }
}

/// Validate a repository name.
///
/// Allows at most one level of grouping (e.g. `foo.git` or `work/foo.git`).
/// Rejects path traversal, missing `.git` suffix, empty segments, and
/// reserved path components.
fn validate_repo_name(name: &str) -> Result<()> {
    ensure!(!name.is_empty(), "repository name cannot be empty");
    ensure!(!name.contains(".."), "invalid repository name: {name}");
    ensure!(
        name.ends_with(".git"),
        "repository name must end in .git: {name}"
    );
    ensure!(!name.contains("//"), "invalid repository name: {name}");

    let segments = name.split('/').collect::<Vec<_>>();
    ensure!(
        segments.len() <= 2,
        "repository name allows at most one level of grouping: {name}"
    );

    for seg in &segments {
        ensure!(!seg.is_empty(), "invalid repository name: {name}");
        ensure!(
            *seg != "." && *seg != ".." && *seg != ".git",
            "invalid repository name: {name}"
        );
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn quire() -> Quire {
        Quire::new(Config::default())
    }

    #[test]
    fn repo_valid() {
        let q = quire();
        assert!(q.repo("foo.git").is_ok());
        assert!(q.repo("work/foo.git").is_ok());
    }

    #[test]
    fn repo_resolves_path() {
        let q = quire();
        assert_eq!(
            q.repo("foo.git").unwrap(),
            PathBuf::from("/var/quire/repos/foo.git")
        );
    }

    #[test]
    fn rejects_empty() {
        let q = quire();
        assert!(q.repo("").is_err());
    }

    #[test]
    fn rejects_traversal() {
        let q = quire();
        assert!(q.repo("../foo.git").is_err());
        assert!(q.repo("foo/../../bar.git").is_err());
        assert!(q.repo("./foo.git").is_err());
    }

    #[test]
    fn rejects_no_git_suffix() {
        let q = quire();
        assert!(q.repo("foo").is_err());
    }

    #[test]
    fn rejects_deep_nesting() {
        let q = quire();
        assert!(q.repo("a/b/c.git").is_err());
    }

    #[test]
    fn rejects_double_slash() {
        let q = quire();
        assert!(q.repo("foo//bar.git").is_err());
    }

    #[test]
    fn rejects_dot_git_segment() {
        let q = quire();
        assert!(q.repo("foo/.git").is_err());
    }
}
