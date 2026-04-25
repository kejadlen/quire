use std::path::{Path, PathBuf};

use miette::{Result, ensure};

/// A validated repository name relative to the repos directory.
#[derive(Debug, Clone)]
pub struct Repo {
    name: String,
}

impl Repo {
    /// Parse a repository name (e.g. `foo.git`, `work/foo.git`).
    ///
    /// Rejects path traversal, missing `.git` suffix, empty segments,
    /// reserved path components, and more than one level of grouping.
    pub fn from_name(name: &str) -> Result<Self> {
        validate_segments(name)?;
        Ok(Repo {
            name: name.to_string(),
        })
    }

    pub fn name(&self) -> &str {
        &self.name
    }

    pub fn path(&self, repos_dir: &Path) -> PathBuf {
        repos_dir.join(&self.name)
    }

    pub fn exists(&self, repos_dir: &Path) -> bool {
        self.path(repos_dir).is_dir()
    }
}

/// Validate segments of a repository name.
///
/// Allows at most one level of grouping (e.g. `foo.git` or `work/foo.git`).
/// Rejects path traversal, missing `.git` suffix, empty segments, and
/// reserved path components.
fn validate_segments(name: &str) -> Result<String> {
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

    Ok(name.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn from_name_valid() {
        assert_eq!(Repo::from_name("foo.git").unwrap().name(), "foo.git");
        assert_eq!(
            Repo::from_name("work/foo.git").unwrap().name(),
            "work/foo.git"
        );
    }

    #[test]
    fn rejects_empty() {
        assert!(Repo::from_name("").is_err());
    }

    #[test]
    fn rejects_traversal() {
        assert!(Repo::from_name("../foo.git").is_err());
        assert!(Repo::from_name("foo/../../bar.git").is_err());
        assert!(Repo::from_name("./foo.git").is_err());
    }

    #[test]
    fn rejects_no_git_suffix() {
        assert!(Repo::from_name("foo").is_err());
    }

    #[test]
    fn rejects_deep_nesting() {
        assert!(Repo::from_name("a/b/c.git").is_err());
    }

    #[test]
    fn rejects_double_slash() {
        assert!(Repo::from_name("foo//bar.git").is_err());
    }

    #[test]
    fn rejects_dot_git_segment() {
        assert!(Repo::from_name("foo/.git").is_err());
    }
}
