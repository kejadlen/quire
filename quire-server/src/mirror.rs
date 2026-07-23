//! Server-side mirror: push updated refs to a remote on every push.
//!
//! Triggered from the push event handler, independent of CI.

use std::collections::HashMap;

use quire_core::event::{PushEvent, PushRef};
use quire_core::secret::SecretString;
use thiserror::Error;

use crate::quire::{Quire, Repo};

/// Why a single mirror push failed. Ref and remote are added as log fields
/// at the call site, not carried here.
#[derive(Debug, Error)]
pub enum PushError {
    #[error("git rejected the push: {0}")]
    Rejected(String),

    #[error(transparent)]
    Secret(#[from] quire_core::secret::Error),

    #[error("running git push: {0}")]
    Spawn(#[from] std::io::Error),
}

/// One remote a ref failed to push to.
#[derive(Debug)]
pub struct PushFailure {
    pub url: String,
    pub cause: PushError,
}

/// Outcome of a manual mirror push for one ref: which remotes took it and
/// which rejected it. Unlike [`trigger`], failures are returned to the
/// caller rather than logged, so an operator running the command sees them.
#[derive(Debug)]
pub struct PushOutcome {
    /// Mirror URLs that accepted the ref.
    pub pushed: Vec<String>,
    /// Mirror URLs that rejected the ref, each with its cause.
    pub failed: Vec<PushFailure>,
}

/// Mirror updated refs to every remote configured for the repo.
///
/// For each updated ref, reads `.quire/config.fnl` at the new SHA to obtain
/// the `:mirrors` table. Each target names a global `:secrets` entry holding
/// its push token. Repos with no mirrors are skipped.
///
/// Failures are logged here rather than returned: each failed target is
/// emitted as its own `tracing` error event, so it reaches Sentry as an
/// individual exception with its `#[source]` chain intact instead of being
/// flattened into one aggregate.
pub fn trigger(quire: &Quire, event: &PushEvent) {
    let repo = match quire.repo(&event.repo) {
        Ok(repo) => repo,
        Err(error) => {
            tracing::error!(
                repo = %event.repo,
                error = &error as &(dyn std::error::Error + 'static),
                "mirror: resolving repo failed",
            );
            return;
        }
    };
    for push_ref in event.updated_refs() {
        let mirror = match Mirror::new(quire, &repo, push_ref) {
            Ok(mirror) => mirror,
            Err(error) => {
                tracing::error!(
                    repo = %event.repo,
                    ref_name = %push_ref.ref_name,
                    error = &error as &(dyn std::error::Error + 'static),
                    "mirror: reading config failed",
                );
                continue;
            }
        };
        if let Err(failures) = mirror.push_all() {
            for failure in failures {
                tracing::error!(
                    repo = %event.repo,
                    ref_name = %push_ref.ref_name,
                    mirror_url = %failure.url,
                    error = &failure.cause as &(dyn std::error::Error + 'static),
                    "mirror: push failed",
                );
            }
        }
    }
}

/// Mirror a single named ref to every remote configured for the repo.
///
/// Resolves `ref_name` (a full ref, e.g. `refs/heads/main`) to its current
/// commit, reads the `:mirrors` table from `.quire/config.fnl` at that SHA,
/// and pushes the ref non-force to each target — the same machinery as the
/// push-event path in [`trigger`], driven by hand instead of a push.
///
/// The ref must exist; a missing ref is an error rather than a no-op, since
/// an operator naming a ref expects it to be there. A repo with no mirrors
/// configured yields an empty [`PushOutcome`].
pub fn push_ref(
    quire: &Quire,
    repo_name: &str,
    ref_name: &str,
) -> Result<PushOutcome, crate::Error> {
    let repo = quire.repo(repo_name)?;

    let sha = resolve_ref(&repo, ref_name)?.ok_or_else(|| crate::Error::RefNotFound {
        repo: repo_name.to_owned(),
        ref_name: ref_name.to_owned(),
    })?;

    let push_ref = PushRef {
        ref_name: ref_name.to_owned(),
        old_sha: String::new(),
        new_sha: sha,
    };

    let mirror = Mirror::new(quire, &repo, &push_ref)?;

    match mirror.push_all() {
        Ok(()) => Ok(PushOutcome {
            pushed: mirror.mirrors.keys().cloned().collect(),
            failed: Vec::new(),
        }),
        Err(failed) => {
            let pushed = mirror
                .mirrors
                .keys()
                .filter(|url| !failed.iter().any(|f| &f.url == *url))
                .cloned()
                .collect();
            Ok(PushOutcome { pushed, failed })
        }
    }
}

/// Resolve a ref to its current commit SHA, or `None` if it doesn't exist.
///
/// `Err` is reserved for a failure to run git at all; a ref that simply
/// isn't present is `Ok(None)`.
fn resolve_ref(repo: &Repo, ref_name: &str) -> Result<Option<String>, std::io::Error> {
    let out = repo
        .git(&["rev-parse", "--verify", "--quiet", ref_name])
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null())
        .output()?;

    if !out.status.success() {
        return Ok(None);
    }

    let sha = String::from_utf8_lossy(&out.stdout).trim().to_owned();
    Ok((!sha.is_empty()).then_some(sha))
}

/// One updated ref's mirroring plan: the remotes to push it to, plus the repo
/// and secrets needed to authenticate.
struct Mirror<'a> {
    repo: &'a Repo,
    secrets: &'a HashMap<String, SecretString>,
    ref_name: &'a str,
    mirrors: HashMap<String, String>,
}

impl<'a> Mirror<'a> {
    /// Read the ref's config and build its mirroring plan, failing if the
    /// config can't be read.
    fn new(quire: &'a Quire, repo: &'a Repo, push_ref: &'a PushRef) -> Result<Self, crate::Error> {
        let secrets = &quire.config.secrets;
        let repo_config = repo.repo_config(&push_ref.new_sha)?;
        Ok(Self {
            repo,
            secrets,
            ref_name: &push_ref.ref_name,
            mirrors: repo_config.mirrors,
        })
    }

    /// Push the ref to every configured remote, collecting one failure per
    /// remote that rejected it. `Ok` only if every push succeeded.
    fn push_all(&self) -> Result<(), Vec<PushFailure>> {
        let mut failures = Vec::new();
        for (url, secret) in &self.mirrors {
            if let Err(cause) = self.push_remote(url, secret) {
                failures.push(PushFailure {
                    url: url.clone(),
                    cause,
                });
            }
        }
        if failures.is_empty() {
            Ok(())
        } else {
            Err(failures)
        }
    }

    /// Push the ref to one remote, reporting why the push failed.
    fn push_remote(&self, url: &str, secret: &str) -> Result<(), PushError> {
        let token = self
            .secrets
            .get(secret)
            .ok_or_else(|| quire_core::secret::Error::UnknownSecret(secret.to_owned()))?
            .reveal()?;

        // Plain (non-force) push: a non-fast-forward update — e.g. after the
        // source branch was rewritten — is rejected rather than overwriting the
        // mirror's ref. The mirror then stays put until reconciled by hand.
        let refspec = format!("{r}:{r}", r = self.ref_name);

        // Pass the auth token via git config env vars so it never appears in argv.
        let out = self
            .repo
            .git(&["push", "--porcelain", url, &refspec])
            .env("GIT_CONFIG_COUNT", "1")
            .env("GIT_CONFIG_KEY_0", "http.extraHeader")
            .env("GIT_CONFIG_VALUE_0", Self::auth_header(token))
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .output()?;

        if !out.status.success() {
            let stderr = String::from_utf8_lossy(&out.stderr).into_owned();
            return Err(PushError::Rejected(stderr));
        }

        tracing::info!(ref_name = %self.ref_name, mirror_url = %url, "mirror: push succeeded");
        Ok(())
    }

    /// Build the HTTP Basic `Authorization` header line for a push token.
    ///
    /// Uses the `token:x-oauth-basic` form, which GitHub and Gitea both accept
    /// for git-over-HTTPS push with a personal access token.
    fn auth_header(token: &str) -> String {
        format!(
            "Authorization: Basic {}",
            base64::Engine::encode(
                &base64::engine::general_purpose::STANDARD,
                format!("{token}:x-oauth-basic"),
            )
        )
    }
}

#[cfg(test)]
mod tests {
    use camino::Utf8Path;

    use super::*;

    #[test]
    fn auth_header_encodes_token_as_oauth_basic() {
        // base64("tok:x-oauth-basic") == "dG9rOngtb2F1dGgtYmFzaWM=".
        assert_eq!(
            Mirror::auth_header("tok"),
            "Authorization: Basic dG9rOngtb2F1dGgtYmFzaWM="
        );
    }

    /// Run a git subcommand in `cwd` with hermetic env, panicking on failure.
    fn git_in(cwd: &Utf8Path, args: &[&str]) {
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
        assert!(output.status.success(), "git {args:?} failed");
    }

    /// Build a Quire whose `foo.git` bare repo has one commit on `main`,
    /// optionally carrying a `.quire/config.fnl`. Returns the tempdir (kept
    /// alive for the test's duration), the Quire, and the repo name.
    fn quire_with_repo(config: Option<&str>) -> (camino_tempfile::Utf8TempDir, Quire, String) {
        let dir = camino_tempfile::tempdir().expect("tempdir");
        let quire = Quire::load(dir.path().to_path_buf()).expect("load");
        let name = "foo.git";
        let bare = quire.repos_dir().join(name);
        fs_err::create_dir_all(bare.parent().expect("parent")).expect("mkdir repos");
        git_in(dir.path(), &["init", "--bare", "-b", "main", bare.as_str()]);

        let work = camino_tempfile::tempdir().expect("workdir");
        git_in(work.path(), &["init", "-q", "-b", "main"]);
        if let Some(cfg) = config {
            let quire_dir = work.path().join(".quire");
            fs_err::create_dir_all(&quire_dir).expect("mkdir .quire");
            fs_err::write(quire_dir.join("config.fnl"), cfg).expect("write config");
        } else {
            fs_err::write(work.path().join("README"), "hi").expect("write readme");
        }
        git_in(work.path(), &["add", "."]);
        git_in(work.path(), &["commit", "-q", "-m", "init"]);
        git_in(work.path(), &["push", "-q", bare.as_str(), "main"]);

        (dir, quire, name.to_string())
    }

    #[test]
    fn push_ref_errors_when_ref_missing() {
        let (_dir, quire, repo) = quire_with_repo(None);
        let err = push_ref(&quire, &repo, "refs/heads/nope").expect_err("should error");
        assert!(
            matches!(err, crate::Error::RefNotFound { .. }),
            "expected RefNotFound, got {err:?}"
        );
    }

    #[test]
    fn push_ref_is_empty_outcome_without_mirrors() {
        let (_dir, quire, repo) = quire_with_repo(None);
        let outcome = push_ref(&quire, &repo, "refs/heads/main").expect("should succeed");
        assert!(outcome.pushed.is_empty());
        assert!(outcome.failed.is_empty());
    }
}
