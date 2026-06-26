//! Server-side mirror: push updated refs to a remote on every push.
//!
//! Triggered from the push event handler, independent of CI.

use std::collections::HashMap;

use quire_core::event::PushEvent;
use quire_core::secret::SecretString;
use thiserror::Error;

use crate::quire::{Quire, Repo};

/// A failure mirroring one ref, carrying where it happened.
#[derive(Debug, Error)]
pub enum TargetError {
    /// Couldn't read `.quire/config.fnl` at the pushed ref.
    #[error("reading .quire/config.fnl at {ref_name}: {source}")]
    Config {
        ref_name: String,
        #[source]
        source: crate::Error,
    },

    /// A push of one ref to one remote failed.
    #[error("mirroring {} to {}: {cause}", .push.ref_name, .push.url)]
    Push {
        push: Push,
        #[source]
        cause: PushError,
    },
}

/// Why a single mirror push failed, before ref/url context is attached.
#[derive(Debug, Error)]
pub enum PushError {
    #[error("git rejected the push: {0}")]
    Rejected(String),

    #[error(transparent)]
    Secret(#[from] quire_core::secret::Error),

    #[error("running git push: {0}")]
    Spawn(#[from] std::io::Error),
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
    let mirror = Mirror {
        repo: &repo,
        secrets: &quire.config.secrets,
    };

    for push in mirror.plan(event) {
        if let Err(cause) = mirror.run(&push) {
            mirror.log_failure(&TargetError::Push { push, cause });
        }
    }
}

/// One mirror push to perform: a ref pushed to a remote, authenticated with
/// the named secret.
#[derive(Debug)]
pub struct Push {
    ref_name: String,
    url: String,
    secret: String,
}

/// Mirroring bound to one repo and the global secrets it authenticates with.
struct Mirror<'a> {
    repo: &'a Repo,
    secrets: &'a HashMap<String, SecretString>,
}

impl Mirror<'_> {
    /// Expand each updated ref into one `Push` per configured mirror. A ref
    /// whose config cannot be read is logged and contributes no pushes.
    fn plan(&self, event: &PushEvent) -> Vec<Push> {
        let mut pushes = Vec::new();
        for push_ref in event.updated_refs() {
            match self.repo.repo_config(&push_ref.new_sha) {
                Ok(config) => pushes.extend(config.mirrors.into_iter().map(|(url, secret)| Push {
                    ref_name: push_ref.ref_name.clone(),
                    url,
                    secret,
                })),
                Err(source) => self.log_failure(&TargetError::Config {
                    ref_name: push_ref.ref_name.clone(),
                    source,
                }),
            }
        }
        pushes
    }

    /// Emit one mirror target failure as a `tracing` error so sentry-tracing
    /// captures it as an individual exception, source chain and all.
    fn log_failure(&self, error: &TargetError) {
        tracing::error!(
            repo = %self.repo.name(),
            error = error as &(dyn std::error::Error + 'static),
            "mirror: target failed",
        );
    }

    /// Force-push the ref to the remote, reporting why the push failed.
    fn run(&self, push: &Push) -> Result<(), PushError> {
        let token = self.resolve_token(&push.secret)?;

        // Force-push the ref to the mirror. The `+` prefix allows rewrites.
        let refspec = format!("+{r}:{r}", r = push.ref_name);

        // Pass the auth token via git config env vars so it never appears in argv.
        let out = self
            .repo
            .git(&["push", "--porcelain", &push.url, &refspec])
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

        tracing::info!(ref_name = %push.ref_name, mirror_url = %push.url, "mirror: push succeeded");
        Ok(())
    }

    /// Resolve a named token from the global secrets map.
    fn resolve_token(&self, name: &str) -> Result<&str, quire_core::secret::Error> {
        self.secrets
            .get(name)
            .ok_or_else(|| quire_core::secret::Error::UnknownSecret(name.to_string()))?
            .reveal()
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

    /// `resolve_token` ignores the repo, so any valid `Repo` will do.
    fn dummy_repo() -> Repo {
        Repo::new(std::path::Path::new("/srv/repos"), "r.git").unwrap()
    }

    #[test]
    fn auth_header_encodes_token_as_oauth_basic() {
        // base64("tok:x-oauth-basic") == "dG9rOngtb2F1dGgtYmFzaWM=".
        assert_eq!(
            Mirror::auth_header("tok"),
            "Authorization: Basic dG9rOngtb2F1dGgtYmFzaWM="
        );
    }

    #[test]
    fn resolve_token_returns_revealed_secret() {
        let repo = dummy_repo();
        let mut secrets = HashMap::new();
        secrets.insert("gitea-mirror".to_string(), SecretString::from("s3cret"));
        let mirror = Mirror {
            repo: &repo,
            secrets: &secrets,
        };
        assert_eq!(mirror.resolve_token("gitea-mirror").unwrap(), "s3cret");
    }

    #[test]
    fn resolve_token_errors_on_missing_secret() {
        let repo = dummy_repo();
        let secrets = HashMap::new();
        let mirror = Mirror {
            repo: &repo,
            secrets: &secrets,
        };
        let err = mirror.resolve_token("absent").unwrap_err();
        assert!(matches!(err, quire_core::secret::Error::UnknownSecret(name) if name == "absent"));
    }
}
