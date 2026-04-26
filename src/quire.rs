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

/// Per-repo configuration parsed from `.quire/config.fnl`.
///
/// Loaded from `HEAD:.quire/config.fnl` in the bare repo via `git show`.
#[derive(serde::Deserialize, Debug, Default, PartialEq)]
pub struct RepoConfig {
    #[serde(default)]
    pub mirror: Option<MirrorConfig>,
}

#[derive(serde::Deserialize, Debug, PartialEq)]
pub struct MirrorConfig {
    pub url: String,
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

    /// Load per-repo config from `HEAD:.quire/config.fnl`.
    ///
    /// Returns a default (empty) `RepoConfig` when:
    /// - HEAD doesn't exist (fresh repo, no pushes yet).
    /// - The config file is absent from HEAD.
    /// - The `:mirror` key is absent from the parsed config.
    ///
    /// Returns an error when the config file exists but contains
    /// malformed Fennel — source labels point at the right line.
    pub fn config(&self) -> crate::Result<RepoConfig> {
        // Check whether HEAD exists first — exit code distinguishes this
        // reliably without parsing stderr text.
        let has_head = std::process::Command::new("git")
            .args(["rev-parse", "--verify", "HEAD"])
            .current_dir(&self.path)
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
            .map_err(crate::Error::Io)?
            .success();

        if !has_head {
            return Ok(RepoConfig::default());
        }

        let output = std::process::Command::new("git")
            .args(["show", "HEAD:.quire/config.fnl"])
            .current_dir(&self.path)
            .output()
            .map_err(crate::Error::Io)?;

        if !output.status.success() {
            // HEAD exists but the file doesn't — not an error.
            return Ok(RepoConfig::default());
        }

        let source = String::from_utf8(output.stdout).map_err(|e| {
            crate::Error::Io(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                format!("config is not valid UTF-8: {e}"),
            ))
        })?;

        let fennel = Fennel::new().map_err(|e| crate::Error::Fennel(e.to_string()))?;
        fennel
            .load_string(&source, "HEAD:.quire/config.fnl")
            .map_err(|e| crate::Error::Fennel(e.to_string()))
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

    /// Helper: create a temp dir with a bare repo that has one commit
    /// containing `.quire/config.fnl` with the given content.
    fn bare_repo_with_config(config_content: &str) -> tempfile::TempDir {
        let dir = tempfile::tempdir().expect("tempdir");
        let work = dir.path().join("work");
        let bare = dir.path().join("repos").join("test.git");

        // Create a worktree repo, commit the config, then clone --bare.
        fs_err::create_dir_all(&work).expect("mkdir work");
        let git = |args: &[&str]| {
            let output = std::process::Command::new("git")
                .args(args)
                .current_dir(&work)
                .env("GIT_AUTHOR_NAME", "test")
                .env("GIT_AUTHOR_EMAIL", "test@test")
                .env("GIT_COMMITTER_NAME", "test")
                .env("GIT_COMMITTER_EMAIL", "test@test")
                .env("GIT_CONFIG_GLOBAL", "/dev/null")
                .env("GIT_CONFIG_SYSTEM", "/dev/null")
                .output()
                .expect("git command");
            if !output.status.success() {
                panic!(
                    "git {:?} failed:\n{}",
                    args,
                    String::from_utf8_lossy(&output.stderr)
                );
            }
            output
        };

        git(&["init"]);
        git(&["commit", "--allow-empty", "-m", "initial"]);

        let config_dir = work.join(".quire");
        fs_err::create_dir_all(&config_dir).expect("mkdir .quire");
        fs_err::write(config_dir.join("config.fnl"), config_content).expect("write config");
        git(&["add", "."]);
        git(&["commit", "-m", "add config"]);

        git(&[
            "clone",
            "--bare",
            work.to_str().unwrap(),
            bare.to_str().unwrap(),
        ]);

        dir
    }

    /// Helper: create a temp dir with an empty bare repo (no HEAD).
    fn empty_bare_repo() -> (tempfile::TempDir, Repo) {
        let dir = tempfile::tempdir().expect("tempdir");
        let bare = dir.path().join("repos").join("test.git");
        fs_err::create_dir_all(&bare).expect("mkdir repos/test.git");

        let git = |args: &[&str]| {
            let output = std::process::Command::new("git")
                .args(args)
                .current_dir(&bare)
                .env("GIT_AUTHOR_NAME", "test")
                .env("GIT_AUTHOR_EMAIL", "test@test")
                .env("GIT_COMMITTER_NAME", "test")
                .env("GIT_COMMITTER_EMAIL", "test@test")
                .env("GIT_CONFIG_GLOBAL", "/dev/null")
                .env("GIT_CONFIG_SYSTEM", "/dev/null")
                .output()
                .expect("git command");
            if !output.status.success() {
                panic!(
                    "git {:?} failed:\n{}",
                    args,
                    String::from_utf8_lossy(&output.stderr)
                );
            }
            output
        };

        git(&["init", "--bare"]);

        let repo = Repo { path: bare };

        (dir, repo)
    }

    /// Helper: create a bare repo with at least one commit but no `.quire/config.fnl`.
    fn bare_repo_without_config() -> (tempfile::TempDir, Repo) {
        let dir = tempfile::tempdir().expect("tempdir");
        let work = dir.path().join("work");
        let bare = dir.path().join("repos").join("test.git");

        let git = |args: &[&str], cwd: &Path| {
            let output = std::process::Command::new("git")
                .args(args)
                .current_dir(cwd)
                .env("GIT_AUTHOR_NAME", "test")
                .env("GIT_AUTHOR_EMAIL", "test@test")
                .env("GIT_COMMITTER_NAME", "test")
                .env("GIT_COMMITTER_EMAIL", "test@test")
                .env("GIT_CONFIG_GLOBAL", "/dev/null")
                .env("GIT_CONFIG_SYSTEM", "/dev/null")
                .output()
                .expect("git command");
            if !output.status.success() {
                panic!(
                    "git {:?} failed:\n{}",
                    args,
                    String::from_utf8_lossy(&output.stderr)
                );
            }
            output
        };

        fs_err::create_dir_all(&work).expect("mkdir work");
        git(&["init"], &work);
        // Commit with no .quire directory.
        git(&["commit", "--allow-empty", "-m", "initial"], &work);
        git(
            &[
                "clone",
                "--bare",
                work.to_str().unwrap(),
                bare.to_str().unwrap(),
            ],
            &work,
        );

        let repo = Repo { path: bare };
        (dir, repo)
    }

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
    fn repo_config_loads_mirror_url() {
        let dir = bare_repo_with_config(r#"{:mirror {:url "https://github.com/owner/repo.git"}}"#);
        let bare = dir.path().join("repos").join("test.git");
        let repo = Repo { path: bare };

        let config = repo.config().expect("config should load");
        assert_eq!(
            config.mirror,
            Some(MirrorConfig {
                url: "https://github.com/owner/repo.git".to_string(),
            })
        );
    }

    #[test]
    fn repo_config_returns_no_mirror_when_head_missing() {
        let (_dir, repo) = empty_bare_repo();
        let config = repo.config().expect("should return default config");
        assert_eq!(config.mirror, None);
    }

    #[test]
    fn repo_config_returns_no_mirror_when_file_absent() {
        let (_dir, repo) = bare_repo_without_config();
        let config = repo.config().expect("should return default config");
        assert_eq!(config.mirror, None);
    }

    #[test]
    fn repo_config_returns_no_mirror_when_key_absent() {
        let dir = bare_repo_with_config("{}");
        let bare = dir.path().join("repos").join("test.git");
        let repo = Repo { path: bare };

        let config = repo.config().expect("should return default config");
        assert_eq!(config.mirror, None);
    }

    #[test]
    fn repo_config_errors_on_malformed_fennel() {
        let dir = bare_repo_with_config("{:bad {:}");
        let bare = dir.path().join("repos").join("test.git");
        let repo = Repo { path: bare };

        let err = repo.config().unwrap_err();
        // The error message should reference the config path.
        let msg = err.to_string();
        assert!(
            msg.contains("HEAD:.quire/config.fnl"),
            "error should mention the config path: {msg}"
        );
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
