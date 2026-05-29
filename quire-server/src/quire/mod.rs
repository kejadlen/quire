use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, OnceLock};

pub mod web;

use crate::ci::{Ci, Executor, Runs};
use crate::{RepoNameError, Result};
pub use quire_core::telemetry::SentryConfig;

use quire_core::fennel::{Fennel, FennelError};
use quire_core::secret::SecretString;

/// Parsed global configuration (`/var/quire/config.fnl`).
///
/// Top-level stays open for future keys (notifications defaults, SMTP, etc.).
#[derive(serde::Deserialize, Debug, Clone)]
#[serde(rename_all = "kebab-case")]
pub struct GlobalConfig {
    #[serde(default)]
    pub sentry: Option<SentryConfig>,
    /// Named secrets exposed to `ci.fnl` jobs as `(secret :name)`.
    /// Each value is a `SecretString` (plain literal or `{:file "..."}`).
    #[serde(default)]
    pub secrets: HashMap<String, SecretString>,
    /// TCP port the HTTP server binds to on all interfaces (`0.0.0.0`).
    /// The API transport derives `http://127.0.0.1:{port}` from this for
    /// quire-ci's bootstrap URL.
    #[serde(default = "default_port")]
    pub port: u16,
    /// CI configuration.
    #[serde(default)]
    pub ci: CiConfig,
    /// GitHub integration settings.
    #[serde(default)]
    pub github: GlobalGithubConfig,
}

impl Default for GlobalConfig {
    fn default() -> Self {
        Self {
            sentry: None,
            secrets: HashMap::new(),
            port: default_port(),
            ci: CiConfig::default(),
            github: GlobalGithubConfig::default(),
        }
    }
}

/// Global GitHub integration configuration.
#[derive(serde::Deserialize, Debug, Default, Clone)]
#[serde(rename_all = "kebab-case")]
pub struct GlobalGithubConfig {
    /// Bearer token used to authenticate push access to the mirror remote.
    #[serde(default)]
    pub mirror_token: Option<SecretString>,
}

fn default_port() -> u16 {
    3000
}

#[derive(serde::Deserialize, Debug, Default, Clone)]
pub struct CiConfig {
    /// How the orchestrator dispatches CI runs. Defaults to shelling
    /// out to the `quire-ci` binary via `Executor::Process`.
    #[serde(default)]
    pub executor: Executor,
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
        let Ok(relative) = path.strip_prefix(repos_base) else {
            return Err(RepoNameError::PathOutsideBase(path.display().to_string()).into());
        };
        let name = relative.to_string_lossy();
        Self::validate_name(&name)?;
        Ok(Self {
            quire_root: repos_base.parent().unwrap_or(repos_base).to_path_buf(),
            name: name.to_string(),
        })
    }

    fn validate_name(name: &str) -> std::result::Result<(), RepoNameError> {
        if name.is_empty() {
            return Err(RepoNameError::Empty);
        }
        if name.contains("..") {
            return Err(RepoNameError::Invalid(name.to_string()));
        }
        if !name.ends_with(".git") {
            return Err(RepoNameError::MissingGitSuffix(name.to_string()));
        }
        if name.contains("//") {
            return Err(RepoNameError::Invalid(name.to_string()));
        }

        let segments = name.split('/').collect::<Vec<_>>();
        if segments.len() > 2 {
            return Err(RepoNameError::TooManySegments(name.to_string()));
        }

        for seg in &segments {
            if seg.is_empty() || *seg == "." || *seg == ".." || *seg == ".git" {
                return Err(RepoNameError::Invalid(name.to_string()));
            }
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

    /// Read and parse `.quire/config.fnl` at the given commit SHA.
    ///
    /// Returns defaults if the file does not exist at that commit.
    pub fn repo_config(&self, sha: &str) -> Result<RepoConfig> {
        let path = format!("{sha}:.quire/config.fnl");

        // cat-file -e: exit 0 if the object exists, non-zero if absent.
        if !self.git(&["cat-file", "-e", &path]).status()?.success() {
            return Ok(RepoConfig::default());
        }

        let out = self
            .git(&["show", &path])
            .stdout(std::process::Stdio::piped())
            .output()?;
        let source = String::from_utf8(out.stdout)?;
        Ok(Fennel::load_config_str(&source, ".quire/config.fnl")?)
    }

    /// The base directory for CI runs (`runs/<repo>/`).
    pub fn runs_base(&self) -> PathBuf {
        self.quire_root.join("runs").join(&self.name)
    }

    /// Access CI runs for this repo.
    pub fn runs(&self, db_path: &Path) -> Runs {
        Runs::new(
            db_path.to_path_buf(),
            self.name().to_string(),
            self.runs_base(),
        )
    }
}

/// Per-repo CI configuration parsed from `.quire/config.fnl`.
#[derive(serde::Deserialize, Debug, Default, Clone)]
#[serde(default, rename_all = "kebab-case")]
pub struct RepoConfig {
    pub github: RepoGithubConfig,
}

/// Per-repo GitHub configuration.
#[derive(serde::Deserialize, Debug, Default, Clone)]
#[serde(default, rename_all = "kebab-case")]
pub struct RepoGithubConfig {
    /// Remote URL to mirror every pushed ref to.
    /// E.g. `"https://github.com/user/repo.git"`.
    pub mirror: Option<String>,
}

/// Application runtime context.
///
/// Carries configuration and provides resolved paths to repositories.
/// Commands receive a `&Quire` instead of threading config around.
#[derive(Clone)]
pub struct Quire {
    base_dir: PathBuf,
    pub config: GlobalConfig,
    db_pool: Arc<OnceLock<Mutex<rusqlite::Connection>>>,
}

impl Quire {
    /// Load config from `base_dir/config.fnl` and create a `Quire` rooted there.
    ///
    /// Returns built-in defaults if the file is absent; propagates parse errors.
    pub fn load(base_dir: PathBuf) -> Result<Self> {
        let config_path = base_dir.join("config.fnl");
        let config = match Fennel::load_config::<GlobalConfig>(&config_path) {
            Ok(config) => config,
            Err(FennelError::Io(e)) if e.kind() == std::io::ErrorKind::NotFound => {
                tracing::warn!(path = %config_path.display(), "config file not found, using defaults");
                GlobalConfig::default()
            }
            Err(e) => return Err(e.into()),
        };
        Ok(Self {
            base_dir,
            config,
            db_pool: Arc::new(OnceLock::new()),
        })
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

    pub fn db_path(&self) -> PathBuf {
        self.base_dir.join("quire.db")
    }

    pub fn socket_path(&self) -> PathBuf {
        self.base_dir.join("server.sock")
    }

    /// Return the shared DB connection for the web view.
    ///
    /// Lazily initialises the connection on first call. Once open, the
    /// same connection is reused for all subsequent requests.
    pub fn db_pool(&self) -> &Mutex<rusqlite::Connection> {
        self.db_pool.get_or_init(|| {
            let conn = crate::db::open(&self.db_path()).expect("failed to open database");
            Mutex::new(conn)
        })
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
impl Default for Quire {
    fn default() -> Self {
        Self {
            base_dir: PathBuf::from("/var/quire"),
            config: GlobalConfig::default(),
            db_pool: Arc::new(OnceLock::new()),
        }
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
        let q = Quire::load(dir.path().to_path_buf()).expect("load");
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
        let q = Quire::load(dir.path().to_path_buf()).expect("load");
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
        let q = Quire::load(dir.path().to_path_buf()).expect("load");
        let path = dir.path().join("repos").join("foo.git");
        let repo = q.repo_from_path(&path).expect("should resolve");
        assert_eq!(repo.path(), path);
    }

    #[test]
    fn repo_from_path_outside_repos() {
        let dir = tempfile::tempdir().expect("tempdir");
        let q = Quire::load(dir.path().to_path_buf()).expect("load");
        let path = PathBuf::from("/tmp/evil.git");
        assert!(q.repo_from_path(&path).is_err());
    }

    #[test]
    fn repo_from_path_rejects_bad_name() {
        let dir = tempfile::tempdir().expect("tempdir");
        let q = Quire::load(dir.path().to_path_buf()).expect("load");
        let path = dir.path().join("repos").join("foo"); // missing .git
        assert!(q.repo_from_path(&path).is_err());
    }

    #[test]
    fn global_config_ci_defaults() {
        let dir = tempfile::tempdir().expect("tempdir");
        fs_err::write(dir.path().join("config.fnl"), "{}").expect("write");

        let q = Quire::load(dir.path().to_path_buf()).expect("should load");
        assert_eq!(q.config.ci.executor, Executor::Process);
        assert_eq!(q.config.port, 3000);
    }

    #[test]
    fn global_config_parses_custom_port() {
        let dir = tempfile::tempdir().expect("tempdir");
        fs_err::write(dir.path().join("config.fnl"), r#"{:port 4000}"#).expect("write");

        let q = Quire::load(dir.path().to_path_buf()).expect("should load");
        assert_eq!(q.config.port, 4000);
    }

    #[test]
    fn global_config_loads_from_fennel_file() {
        let dir = tempfile::tempdir().expect("tempdir");
        fs_err::write(dir.path().join("config.fnl"), "{}").expect("write");

        let q = Quire::load(dir.path().to_path_buf()).expect("should load");
        assert!(q.config.secrets.is_empty());
    }

    #[test]
    fn global_config_missing_file_uses_defaults() {
        let dir = tempfile::tempdir().expect("tempdir");

        let q = Quire::load(dir.path().to_path_buf()).expect("missing file should use defaults");
        assert_eq!(q.config.port, 3000);
        assert!(q.config.sentry.is_none());
        assert!(q.config.secrets.is_empty());
    }

    #[test]
    fn global_config_loads_with_sentry() {
        let dir = tempfile::tempdir().expect("tempdir");
        fs_err::write(
            dir.path().join("config.fnl"),
            r#"{:sentry {:dsn "https://key@sentry.io/123"}}"#,
        )
        .expect("write");

        let q = Quire::load(dir.path().to_path_buf()).expect("should load");
        let sentry = q.config.sentry.expect("sentry should be present");
        assert_eq!(sentry.dsn.reveal().unwrap(), "https://key@sentry.io/123");
    }

    #[test]
    fn global_config_sentry_is_optional() {
        let dir = tempfile::tempdir().expect("tempdir");
        fs_err::write(dir.path().join("config.fnl"), "{}").expect("write");

        let q = Quire::load(dir.path().to_path_buf()).expect("should load");
        assert!(q.config.sentry.is_none());
    }

    #[test]
    fn global_config_secrets_default_empty() {
        let dir = tempfile::tempdir().expect("tempdir");
        fs_err::write(dir.path().join("config.fnl"), "{}").expect("write");

        let q = Quire::load(dir.path().to_path_buf()).expect("should load");
        assert!(q.config.secrets.is_empty());
    }

    #[test]
    fn global_config_loads_secrets_map() {
        let dir = tempfile::tempdir().expect("tempdir");
        let secret_file = dir.path().join("gh_token");
        fs_err::write(&secret_file, "ghp_from_file\n").expect("write secret");
        fs_err::write(
            dir.path().join("config.fnl"),
            format!(
                r#"{{:secrets {{:github_token {{:file "{}"}}
                   :slack_webhook "https://hooks.slack.com/abc"}}}}"#,
                secret_file.display()
            ),
        )
        .expect("write");

        let q = Quire::load(dir.path().to_path_buf()).expect("should load");
        assert_eq!(q.config.secrets.len(), 2);
        assert_eq!(
            q.config.secrets["github_token"].reveal().unwrap(),
            "ghp_from_file"
        );
        assert_eq!(
            q.config.secrets["slack_webhook"].reveal().unwrap(),
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
