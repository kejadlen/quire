use std::path::{Path, PathBuf};

use miette::{IntoDiagnostic, Result, ensure, miette};

use crate::fennel::Fennel;
use crate::secret::SecretString;

/// Parsed global configuration (`/var/quire/config.fnl`).
///
/// Top-level stays open for future keys (notifications defaults, SMTP, etc.).
#[derive(serde::Deserialize, Debug)]
pub struct GlobalConfig {
    pub github: GithubConfig,
    #[serde(default)]
    pub sentry: Option<SentryConfig>,
}

#[derive(serde::Deserialize, Debug)]
pub struct GithubConfig {
    pub token: SecretString,
}

#[derive(serde::Deserialize, Debug)]
pub struct SentryConfig {
    pub dsn: SecretString,
}

/// Per-repo configuration parsed from `.quire/config.fnl`.
///
/// Loaded from `HEAD:.quire/config.fnl` in the bare repo via `git show`.
#[derive(serde::Deserialize, Debug, Default, PartialEq)]
pub struct RepoConfig {
    pub mirror: Option<MirrorConfig>,
}

#[derive(serde::Deserialize, Debug, PartialEq)]
pub struct MirrorConfig {
    #[serde(deserialize_with = "deserialize_mirror_url")]
    pub url: String,
}

/// Reject URLs with embedded user[:password]@ credentials so a misconfigured
/// repo can't leak a token via tracing, Sentry, or git's own error output.
/// Tokens come from global config and ride in `http.extraHeader`.
fn deserialize_mirror_url<'de, D>(deserializer: D) -> std::result::Result<String, D::Error>
where
    D: serde::Deserializer<'de>,
{
    use serde::Deserialize;
    let url = String::deserialize(deserializer)?;
    if let Some((_, after_scheme)) = url.split_once("://")
        && let Some(at) = after_scheme.find('@')
        && !after_scheme[..at].contains('/')
    {
        return Err(serde::de::Error::custom(
            "mirror URL must not embed credentials; tokens come from global config",
        ));
    }
    Ok(url)
}

/// A resolved repository path.
///
/// Created by `Quire::repo` after validating the name.
pub struct Repo {
    path: PathBuf,
}

impl Repo {
    /// Validate a repository name and create a `Repo` at the given path.
    ///
    /// Allows at most one level of grouping (e.g. `foo.git` or `work/foo.git`).
    /// Rejects path traversal, missing `.git` suffix, empty segments, and
    /// reserved path components.
    pub fn new(base: &Path, name: &str) -> Result<Self> {
        Self::validate_name(name)?;
        Ok(Self {
            path: base.join(name),
        })
    }

    /// Create a `Repo` from an already-resolved filesystem path.
    ///
    /// Verifies the path falls under `base` and passes name validation.
    /// Used by hooks that receive `GIT_DIR` from git.
    pub fn from_path(base: &Path, path: &Path) -> Result<Self> {
        let relative = path
            .strip_prefix(base)
            .map_err(|_| miette!("path is not under repos directory: {}", path.display()))?;
        let name = relative.to_string_lossy();
        Self::validate_name(&name)?;
        Ok(Self {
            path: path.to_path_buf(),
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

    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn exists(&self) -> bool {
        self.path.is_dir()
    }

    /// Start a git command rooted in this bare repo.
    ///
    /// Returns a `Command` with `current_dir` set. The caller decides
    /// `.status()`, `.output()`, or anything else.
    pub fn git(&self, args: &[&str]) -> std::process::Command {
        let mut cmd = std::process::Command::new("git");
        cmd.args(args).current_dir(&self.path);
        cmd
    }

    /// Push `main` to the configured mirror, injecting the GitHub token via
    /// `http.extraHeader` so it never appears in the URL or git's error output.
    ///
    /// The token is passed through `GIT_CONFIG_*` env vars on the child
    /// process. This keeps it out of the command line (visible via `ps`),
    /// but it remains visible in `/proc/<pid>/environ` to anything running
    /// as the same uid for the lifetime of the push. Acceptable today
    /// (single-user container, no CI runner yet); revisit when CI lands.
    pub fn push_to_mirror(
        &self,
        mirror: &MirrorConfig,
        token: &str,
        refs: &[&str],
    ) -> crate::Result<()> {
        let mut args = vec!["push", "--porcelain", &mirror.url];
        args.extend(refs);

        let status = self
            .git(&args)
            .env("GIT_CONFIG_COUNT", "1")
            .env("GIT_CONFIG_KEY_0", "http.extraHeader")
            .env("GIT_CONFIG_VALUE_0", github_auth_header(token))
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
            .map_err(crate::Error::Io)?;

        if !status.success() {
            return Err(crate::Error::Git(format!("push to {} failed", mirror.url)));
        }
        Ok(())
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
        let has_head = self
            .git(&["rev-parse", "--verify", "HEAD"])
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
            .map_err(crate::Error::Io)?
            .success();

        if !has_head {
            return Ok(RepoConfig::default());
        }

        let output = self
            .git(&["show", "HEAD:.quire/config.fnl"])
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

        let fennel = Fennel::new()?;
        Ok(fennel.load_string(&source, "HEAD:.quire/config.fnl")?)
    }
}

/// Build the `Authorization` header value used to authenticate `git push`
/// to GitHub over HTTPS.
///
/// GitHub's git smart HTTP endpoint (`/info/refs`, `git-receive-pack`)
/// rejects `Authorization: Bearer <PAT>` with 401, even though the REST
/// API accepts the same token via Bearer. Git then falls back to
/// prompting for a username, which fails inside a post-receive hook
/// because there's no TTY. HTTP Basic with any non-empty username and
/// the token as the password is the documented form for git push.
fn github_auth_header(token: &str) -> String {
    use base64::{Engine, engine::general_purpose::STANDARD};
    let encoded = STANDARD.encode(format!("x-access-token:{token}"));
    format!("Authorization: Basic {encoded}")
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

    pub fn socket_path(&self) -> PathBuf {
        self.base_dir.join("server.sock")
    }

    /// Load and parse the global Fennel config file.
    ///
    /// Re-reads on every call. Cheap at current call volume; revisit if
    /// `quire serve` ends up loading per-request.
    pub fn global_config(&self) -> crate::Result<GlobalConfig> {
        let config_path = self.config_path();
        if !config_path.exists() {
            return Err(crate::Error::ConfigNotFound(
                config_path.display().to_string(),
            ));
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

        git(&["init", "--bare", "-b", "main"]);

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

    #[test]
    fn global_config_loads_with_sentry() {
        let dir = tempfile::tempdir().expect("tempdir");
        let config_path = dir.path().join("config.fnl");
        fs_err::write(
            &config_path,
            r#"{:github {:token "ghp_test"} :sentry {:dsn "https://key@sentry.io/123"}}"#,
        )
        .expect("write");

        let q = Quire {
            base_dir: dir.path().to_path_buf(),
        };
        let config = q.global_config().expect("global_config should load");
        assert_eq!(config.github.token.reveal().unwrap(), "ghp_test");
        let sentry = config.sentry.expect("sentry should be present");
        assert_eq!(sentry.dsn.reveal().unwrap(), "https://key@sentry.io/123");
    }

    #[test]
    fn global_config_sentry_is_optional() {
        let dir = tempfile::tempdir().expect("tempdir");
        let config_path = dir.path().join("config.fnl");
        fs_err::write(&config_path, r#"{:github {:token "ghp_test"}}"#).expect("write");

        let q = Quire {
            base_dir: dir.path().to_path_buf(),
        };
        let config = q.global_config().expect("global_config should load");
        assert!(config.sentry.is_none());
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
        if !output.status.success() {
            panic!(
                "git {:?} failed:\n{}",
                args,
                String::from_utf8_lossy(&output.stderr)
            );
        }
    }

    /// Helper: create a bare repo at `bare` with `main` and one commit.
    fn make_bare_with_main(work: &Path, bare: &Path) {
        fs_err::create_dir_all(work).expect("mkdir work");
        git_in(work, &["init", "-b", "main"]);
        git_in(work, &["commit", "--allow-empty", "-m", "initial"]);
        git_in(
            work.parent().unwrap_or(work),
            &[
                "clone",
                "--bare",
                work.to_str().unwrap(),
                bare.to_str().unwrap(),
            ],
        );
    }

    fn rev_parse(repo: &Path, rev: &str) -> String {
        let output = std::process::Command::new("git")
            .args(["-C", repo.to_str().unwrap(), "rev-parse", rev])
            .output()
            .expect("rev-parse");
        assert!(output.status.success(), "rev-parse failed");
        String::from_utf8(output.stdout)
            .expect("utf-8")
            .trim()
            .to_string()
    }

    #[test]
    fn github_auth_header_is_basic_with_x_access_token_username() {
        use base64::{Engine, engine::general_purpose::STANDARD};

        let header = super::github_auth_header("ghp_test");
        let encoded = header
            .strip_prefix("Authorization: Basic ")
            .unwrap_or_else(|| panic!("missing Basic prefix: {header}"));
        // libcurl rejects header values containing newlines, so the encoded
        // form must not wrap.
        assert!(
            !encoded.contains('\n'),
            "encoded header value must not wrap: {encoded:?}"
        );
        let decoded = STANDARD.decode(encoded).expect("valid base64");
        assert_eq!(decoded, b"x-access-token:ghp_test");
    }

    #[test]
    fn push_to_mirror_pushes_main_to_file_mirror() {
        let dir = tempfile::tempdir().expect("tempdir");
        let work = dir.path().join("work");
        let source = dir.path().join("source.git");
        let target = dir.path().join("target.git");

        make_bare_with_main(&work, &source);
        fs_err::create_dir_all(&target).expect("mkdir target");
        git_in(&target, &["init", "--bare", "-b", "main"]);

        let repo = Repo {
            path: source.clone(),
        };
        let mirror = MirrorConfig {
            url: format!("file://{}", target.display()),
        };
        repo.push_to_mirror(&mirror, "ignored-for-file-url", &["main"])
            .expect("push should succeed");

        assert_eq!(rev_parse(&source, "main"), rev_parse(&target, "main"));
    }

    #[test]
    fn push_to_mirror_errors_when_target_unreachable() {
        let dir = tempfile::tempdir().expect("tempdir");
        let work = dir.path().join("work");
        let source = dir.path().join("source.git");

        make_bare_with_main(&work, &source);

        let repo = Repo { path: source };
        let mirror = MirrorConfig {
            url: "file:///nonexistent/quire-test/target.git".to_string(),
        };
        let err = repo.push_to_mirror(&mirror, "x", &["main"]).unwrap_err();
        assert!(
            matches!(err, crate::Error::Git(_)),
            "expected Git error, got {err:?}"
        );
    }

    #[test]
    fn mirror_url_rejects_embedded_credentials() {
        let dir = bare_repo_with_config(
            r#"{:mirror {:url "https://x:token@github.com/owner/repo.git"}}"#,
        );
        let bare = dir.path().join("repos").join("test.git");
        let repo = Repo { path: bare };

        let err = repo.config().unwrap_err();
        let msg = err.to_string();
        assert!(
            msg.contains("credentials"),
            "expected credential error, got: {msg}"
        );
    }
}
