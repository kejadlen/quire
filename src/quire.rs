use std::collections::HashMap;
use std::path::{Path, PathBuf};

use miette::{Context, IntoDiagnostic, Result, ensure};

use crate::ci::{Ci, Runs};
use crate::fennel::Fennel;
use crate::secret::SecretString;
use crate::{Error, Result as AppResult};

/// Parsed global configuration (`/var/quire/config.fnl`).
///
/// Top-level stays open for future keys (notifications defaults, SMTP, etc.).
#[derive(serde::Deserialize, Debug)]
pub struct GlobalConfig {
    #[serde(default)]
    pub sentry: Option<SentryConfig>,
    /// Named secrets exposed to `ci.fnl` jobs as `(secret :name)`.
    /// Each value is a `SecretString` (plain literal or `{:file "..."}`).
    #[serde(default)]
    pub secrets: HashMap<String, SecretString>,
}

#[derive(serde::Deserialize, Debug)]
pub struct SentryConfig {
    pub dsn: SecretString,
}

/// A resolved repository path.
///
/// Created by `Quire::repo` after validating the name.
pub struct Repo {
    /// The quire root directory (e.g. `/var/quire`).
    quire_root: PathBuf,
    name: String,
}

impl Repo {
    /// Validate a repository name and create a `Repo` at the given path.
    ///
    /// Allows at most one level of grouping (e.g. `foo.git` or `work/foo.git`).
    /// Rejects path traversal, missing `.git` suffix, empty segments, and
    /// reserved path components.
    pub fn new(repos_base: &Path, name: &str) -> Result<Self> {
        Self::validate_name(name)?;
        Ok(Self {
            quire_root: repos_base.parent().unwrap_or(repos_base).to_path_buf(),
            name: name.to_string(),
        })
    }

    /// Create a `Repo` from an already-resolved filesystem path.
    ///
    /// Verifies the path falls under `base` and passes name validation.
    /// Used by hooks that receive `GIT_DIR` from git.
    pub fn from_path(repos_base: &Path, path: &Path) -> Result<Self> {
        let relative = path
            .strip_prefix(repos_base)
            .into_diagnostic()
            .context(format!(
                "path is not under repos directory: {}",
                path.display()
            ))?;
        let name = relative.to_string_lossy();
        Self::validate_name(&name)?;
        Ok(Self {
            quire_root: repos_base.parent().unwrap_or(repos_base).to_path_buf(),
            name: name.to_string(),
        })
    }

    fn validate_name(name: &str) -> Result<()> {
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

    pub fn path(&self) -> PathBuf {
        self.quire_root.join("repos").join(&self.name)
    }

    /// The repo name relative to the repos directory (e.g. `foo.git`).
    pub fn name(&self) -> &str {
        &self.name
    }

    pub fn exists(&self) -> bool {
        self.path().is_dir()
    }

    /// Start a git command rooted in this bare repo.
    ///
    /// Returns a `Command` with `current_dir` set. The caller decides
    /// `.status()`, `.output()`, or anything else.
    pub fn git(&self, args: &[&str]) -> std::process::Command {
        let mut cmd = std::process::Command::new("git");
        cmd.args(args).current_dir(self.path());
        cmd
    }

    /// Access CI operations for this repo.
    pub fn ci(&self) -> Ci {
        Ci::new(self.path())
    }

    /// The base directory for CI runs (`runs/<repo>/`).
    pub fn runs_base(&self) -> PathBuf {
        self.quire_root.join("runs").join(&self.name)
    }

    /// Access CI runs for this repo.
    pub fn runs(&self) -> Runs {
        Runs::new(self.runs_base())
    }
}

/// Application runtime context.
///
/// Carries configuration and provides resolved paths to repositories.
/// Commands receive a `&Quire` instead of threading config around.
#[derive(Clone)]
pub struct Quire {
    base_dir: PathBuf,
}

impl Default for Quire {
    fn default() -> Self {
        Self::new(PathBuf::from("/var/quire"))
    }
}

impl Quire {
    /// Create a `Quire` rooted at the given base directory.
    pub fn new(base_dir: PathBuf) -> Self {
        Self { base_dir }
    }

    pub fn base_dir(&self) -> &Path {
        &self.base_dir
    }

    pub fn repos_dir(&self) -> PathBuf {
        self.base_dir.join("repos")
    }

    pub fn config_path(&self) -> PathBuf {
        self.base_dir.join("config.fnl")
    }

    pub fn socket_path(&self) -> PathBuf {
        self.base_dir.join("server.sock")
    }

    /// Load and parse the global Fennel config file.
    ///
    /// Re-reads on every call. Cheap at current call volume; revisit if
    /// `quire serve` ends up loading per-request.
    pub fn global_config(&self) -> AppResult<GlobalConfig> {
        let config_path = self.config_path();
        if !config_path.exists() {
            return Err(Error::ConfigNotFound(config_path.display().to_string()));
        }
        let fennel = Fennel::new()?;
        Ok(fennel.load_file(&config_path)?)
    }

    /// Validate a repository name and return its resolved path.
    ///
    /// Delegates to `Repo::new` for name validation.
    pub fn repo(&self, name: &str) -> Result<Repo> {
        Repo::new(&self.repos_dir(), name)
    }

    /// Resolve a filesystem path to a `Repo`.
    ///
    /// Delegates to `Repo::from_path` for path and name validation.
    pub fn repo_from_path(&self, path: &Path) -> Result<Repo> {
        Repo::from_path(&self.repos_dir(), path)
    }

    /// List all repositories under the repos directory.
    ///
    /// Walks at most two levels deep, collecting directories ending in `.git`.
    /// This enforces the "at most one level of grouping" rule structurally.
    pub fn repos(&self) -> Result<impl Iterator<Item = Repo>> {
        let repos_dir = self.repos_dir();

        let mut repos: Vec<Repo> = walkdir::WalkDir::new(&repos_dir)
            .max_depth(2)
            .into_iter()
            .filter_map(|entry| entry.ok())
            .filter(|entry| entry.file_type().is_dir())
            .filter_map(|entry| {
                let name = entry.path().strip_prefix(&repos_dir).ok()?;
                let name = name.to_string_lossy();
                if name.ends_with(".git") {
                    Some(name.to_string())
                } else {
                    None
                }
            })
            .map(|name| Repo::new(&repos_dir, &name))
            .collect::<Result<Vec<_>>>()?;

        repos.sort_by(|a, b| a.name().cmp(b.name()));
        Ok(repos.into_iter())
    }
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
        assert_eq!(q.socket_path(), PathBuf::from("/var/quire/server.sock"));
    }

    #[test]
    fn repo_valid() {
        let q = quire();
        assert!(q.repo("foo.git").is_ok());
        assert!(q.repo("work/foo.git").is_ok());
    }

    #[test]
    fn repo_name_returns_name() {
        let q = quire();
        let repo = q.repo("foo.git").unwrap();
        assert_eq!(repo.name(), "foo.git");
    }

    #[test]
    fn repos_lists_bare_repos() {
        let dir = tempfile::tempdir().expect("tempdir");
        let q = Quire::new(dir.path().to_path_buf());
        let repos_dir = q.repos_dir();

        // Create two bare repos.
        for name in ["alpha.git", "work/bravo.git"] {
            let bare = repos_dir.join(name);
            fs_err::create_dir_all(&bare).expect("mkdir");
            git_in(&bare, &["init", "--bare", "-b", "main"]);
        }

        let repos: Vec<_> = q.repos().expect("repos").collect();
        assert_eq!(repos.len(), 2);
        assert_eq!(repos[0].name(), "alpha.git");
        assert_eq!(repos[1].name(), "work/bravo.git");
    }

    #[test]
    fn repos_empty_when_no_dirs() {
        let dir = tempfile::tempdir().expect("tempdir");
        let q = Quire::new(dir.path().to_path_buf());
        let repos: Vec<_> = q.repos().expect("repos").collect();
        assert!(repos.is_empty());
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
    fn repo_from_path_valid() {
        let dir = tempfile::tempdir().expect("tempdir");
        let q = Quire {
            base_dir: dir.path().to_path_buf(),
        };
        let path = dir.path().join("repos").join("foo.git");
        let repo = q.repo_from_path(&path).expect("should resolve");
        assert_eq!(repo.path(), path);
    }

    #[test]
    fn repo_from_path_outside_repos() {
        let dir = tempfile::tempdir().expect("tempdir");
        let q = Quire {
            base_dir: dir.path().to_path_buf(),
        };
        let path = PathBuf::from("/tmp/evil.git");
        assert!(q.repo_from_path(&path).is_err());
    }

    #[test]
    fn repo_from_path_rejects_bad_name() {
        let dir = tempfile::tempdir().expect("tempdir");
        let q = Quire {
            base_dir: dir.path().to_path_buf(),
        };
        let path = dir.path().join("repos").join("foo"); // missing .git
        assert!(q.repo_from_path(&path).is_err());
    }

    #[test]
    fn global_config_loads_from_fennel_file() {
        let dir = tempfile::tempdir().expect("tempdir");
        let config_path = dir.path().join("config.fnl");
        fs_err::write(&config_path, "{}").expect("write");

        let q = Quire {
            base_dir: dir.path().to_path_buf(),
        };
        let config = q.global_config().expect("global_config should load");
        assert!(config.secrets.is_empty());
    }

    #[test]
    fn global_config_missing_file_errors() {
        let dir = tempfile::tempdir().expect("tempdir");

        let q = Quire {
            base_dir: dir.path().to_path_buf(),
        };
        let err = q.global_config().unwrap_err();
        assert!(
            matches!(err, Error::ConfigNotFound(_)),
            "expected ConfigNotFound, got {err:?}"
        );
    }

    #[test]
    fn global_config_loads_with_sentry() {
        let dir = tempfile::tempdir().expect("tempdir");
        let config_path = dir.path().join("config.fnl");
        fs_err::write(
            &config_path,
            r#"{:sentry {:dsn "https://key@sentry.io/123"}}"#,
        )
        .expect("write");

        let q = Quire {
            base_dir: dir.path().to_path_buf(),
        };
        let config = q.global_config().expect("global_config should load");
        let sentry = config.sentry.expect("sentry should be present");
        assert_eq!(sentry.dsn.reveal().unwrap(), "https://key@sentry.io/123");
    }

    #[test]
    fn global_config_sentry_is_optional() {
        let dir = tempfile::tempdir().expect("tempdir");
        let config_path = dir.path().join("config.fnl");
        fs_err::write(&config_path, "{}").expect("write");

        let q = Quire {
            base_dir: dir.path().to_path_buf(),
        };
        let config = q.global_config().expect("global_config should load");
        assert!(config.sentry.is_none());
    }

    #[test]
    fn global_config_secrets_default_empty() {
        let dir = tempfile::tempdir().expect("tempdir");
        let config_path = dir.path().join("config.fnl");
        fs_err::write(&config_path, "{}").expect("write");

        let q = Quire {
            base_dir: dir.path().to_path_buf(),
        };
        let config = q.global_config().expect("global_config should load");
        assert!(config.secrets.is_empty());
    }

    #[test]
    fn global_config_loads_secrets_map() {
        let dir = tempfile::tempdir().expect("tempdir");
        let config_path = dir.path().join("config.fnl");
        let secret_file = dir.path().join("gh_token");
        fs_err::write(&secret_file, "ghp_from_file\n").expect("write secret");
        fs_err::write(
            &config_path,
            format!(
                r#"{{:secrets {{:github_token {{:file "{}"}}
                   :slack_webhook "https://hooks.slack.com/abc"}}}}"#,
                secret_file.display()
            ),
        )
        .expect("write");

        let q = Quire {
            base_dir: dir.path().to_path_buf(),
        };
        let config = q.global_config().expect("global_config should load");
        assert_eq!(config.secrets.len(), 2);
        assert_eq!(
            config.secrets["github_token"].reveal().unwrap(),
            "ghp_from_file"
        );
        assert_eq!(
            config.secrets["slack_webhook"].reveal().unwrap(),
            "https://hooks.slack.com/abc"
        );
    }

    /// Helper: run a git subcommand in `cwd` with hermetic env, panicking on failure.
    fn git_in(cwd: &Path, args: &[&str]) {
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
        assert!(output.status.success());
    }
}
