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
struct PushFailure {
    url: String,
    cause: PushError,
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
        if let Err(failures) = mirror.push() {
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
    fn push(&self) -> Result<(), Vec<PushFailure>> {
        let mut failures = Vec::new();
        for (url, secret) in &self.mirrors {
            if let Err(cause) = self.force_push(url, secret) {
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

    /// Force-push the ref to one remote, reporting why the push failed.
    fn force_push(&self, url: &str, secret: &str) -> Result<(), PushError> {
        let token = self
            .secrets
            .get(secret)
            .ok_or_else(|| quire_core::secret::Error::UnknownSecret(secret.to_owned()))?
            .reveal()?;

        // The `+` prefix lets the remote accept rewrites: if the source branch
        // was rewritten locally before the mirror ran, the mirror still applies.
        let refspec = format!("+{r}:{r}", r = self.ref_name);

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
    use super::*;

    #[test]
    fn auth_header_encodes_token_as_oauth_basic() {
        // base64("tok:x-oauth-basic") == "dG9rOngtb2F1dGgtYmFzaWM=".
        assert_eq!(
            Mirror::auth_header("tok"),
            "Authorization: Basic dG9rOngtb2F1dGgtYmFzaWM="
        );
    }
}
