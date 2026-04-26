use std::path::{Path, PathBuf};

use miette::{IntoDiagnostic, Result, ensure};

use crate::fennel::Fennel;
use crate::secret::SecretString;

/// Parsed global configuration (`/var/quire/config.fnl`).
///
/// Top-level stays open for future keys (notifications defaults, SMTP, etc.).
#[derive(serde::Deserialize, Debug)]
pub struct GlobalConfig {
    pub github: GithubConfig,
}

#[derive(serde::Deserialize, Debug)]
pub struct GithubConfig {
    pub token: SecretString,
}

/// A resolved repository path.
///
/// Created by `Quire::repo` after validating the name.
pub struct Repo {
    path: PathBuf,
}

impl Repo {
    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn exists(&self) -> bool {
        self.path.is_dir()
    }
}

/// Application runtime context.
///
/// Carries configuration and provides resolved paths to repositories.
/// Commands receive a `&Quire` instead of threading config around.
pub struct Quire {
    base_dir: PathBuf,
}

impl Default for Quire {
    fn default() -> Self {
        Self {
            base_dir: PathBuf::from("/var/quire"),
        }
    }
}

impl Quire {
    pub fn base_dir(&self) -> &Path {
        &self.base_dir
    }

    pub fn repos_dir(&self) -> PathBuf {
        self.base_dir.join("repos")
    }

    pub fn config_path(&self) -> PathBuf {
        self.base_dir.join("config.fnl")
    }

    /// Load and parse the global Fennel config file.
    ///
    /// Caches the result — subsequent calls return the same instance.
    /// Returns a typed error if the file is missing or malformed.
    /// Load and parse the global Fennel config file.
    ///
    /// Returns a typed error if the file is missing or malformed.
    pub fn global_config(&self) -> crate::Result<GlobalConfig> {
        let config_path = self.config_path();
        if !config_path.exists() {
            return Err(crate::Error::ConfigNotFound(
                config_path.display().to_string(),
            ));
        }
        let fennel = Fennel::new().map_err(|e| crate::Error::Fennel(e.to_string()))?;
        fennel
            .load_file(&config_path)
            .map_err(|e| crate::Error::Fennel(e.to_string()))
    }

    /// Validate a repository name and return its resolved path.
    ///
    /// Rejects path traversal, missing `.git` suffix, empty segments,
    /// reserved path components, and more than one level of grouping.
    pub fn repo(&self, name: &str) -> Result<Repo> {
        validate_repo_name(name)?;
        Ok(Repo {
            path: self.repos_dir().join(name),
        })
    }

    /// List all repository names under the repos directory.
    pub fn repos(&self) -> Result<impl Iterator<Item = String> + '_> {
        let repos_dir = self.repos_dir();
        let entries = fs_err::read_dir(&repos_dir).into_diagnostic()?;

        let mut repos: Vec<String> = Vec::new();
        for entry in entries {
            let entry = entry.into_diagnostic()?;
            let path = entry.path();

            if !path.is_dir() {
                continue;
            }

            let Ok(relative) = path.strip_prefix(&repos_dir) else {
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
        Quire::default()
    }

    #[test]
    fn default_paths() {
        let q = Quire::default();
        assert_eq!(q.base_dir(), Path::new("/var/quire"));
        assert_eq!(q.repos_dir(), PathBuf::from("/var/quire/repos"));
        assert_eq!(q.config_path(), PathBuf::from("/var/quire/config.fnl"));
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
            q.repo("foo.git").unwrap().path(),
            Path::new("/var/quire/repos/foo.git")
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

    #[test]
    fn global_config_loads_from_fennel_file() {
        let dir = tempfile::tempdir().expect("tempdir");
        let config_path = dir.path().join("config.fnl");
        fs_err::write(&config_path, r#"{:github {:token "ghp_test123"}}"#).expect("write");

        let q = Quire {
            base_dir: dir.path().to_path_buf(),
        };
        let config = q.global_config().expect("global_config should load");
        assert_eq!(config.github.token.reveal().unwrap(), "ghp_test123");
    }

    #[test]
    fn global_config_missing_file_errors() {
        let dir = tempfile::tempdir().expect("tempdir");

        let q = Quire {
            base_dir: dir.path().to_path_buf(),
        };
        let err = q.global_config().unwrap_err();
        assert!(
            matches!(err, crate::Error::ConfigNotFound(_)),
            "expected ConfigNotFound, got {err:?}"
        );
    }
}
