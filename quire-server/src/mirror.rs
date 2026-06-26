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

/// Why mirroring one ref failed: either its config couldn't be read (so no
/// remotes were attempted), or one or more of its remotes rejected the push.
enum MirrorError {
    Config(crate::Error),
    Pushes(Vec<PushFailure>),
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
    let secrets = &quire.config.secrets;
    for push_ref in event.updated_refs() {
        if let Err(error) = mirror_ref(&repo, secrets, push_ref) {
            match error {
                MirrorError::Config(source) => tracing::error!(
                    repo = %event.repo,
                    ref_name = %push_ref.ref_name,
                    error = &source as &(dyn std::error::Error + 'static),
                    "mirror: reading config failed",
                ),
                MirrorError::Pushes(failures) => {
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
    }
}

/// Mirror one updated ref to every remote configured at its new SHA. Attempts
/// every remote, returning the config-read failure (nothing attempted) or the
/// failures from each remote that rejected the push. `Ok` if all succeeded; a
/// ref with no mirrors is a no-op.
fn mirror_ref(
    repo: &Repo,
    secrets: &HashMap<String, SecretString>,
    push_ref: &PushRef,
) -> Result<(), MirrorError> {
    let config = repo
        .repo_config(&push_ref.new_sha)
        .map_err(MirrorError::Config)?;
    let mut failures = Vec::new();
    for (url, secret) in config.mirrors {
        if let Err(cause) = mirror_push(repo, secrets, &push_ref.ref_name, &url, &secret) {
            failures.push(PushFailure { url, cause });
        }
    }
    if failures.is_empty() {
        Ok(())
    } else {
        Err(MirrorError::Pushes(failures))
    }
}

/// Push one ref to one mirror remote, reporting why the push failed.
fn mirror_push(
    repo: &Repo,
    secrets: &HashMap<String, SecretString>,
    ref_name: &str,
    url: &str,
    secret: &str,
) -> Result<(), PushError> {
    let token = secrets
        .get(secret)
        .ok_or_else(|| quire_core::secret::Error::UnknownSecret(secret.to_owned()))?
        .reveal()?;

    // The `+` prefix lets the remote accept rewrites: if the source branch
    // was rewritten locally before the mirror ran, the mirror still applies.
    let refspec = format!("+{r}:{r}", r = ref_name);

    // Pass the auth token via git config env vars so it never appears in argv.
    let out = repo
        .git(&["push", "--porcelain", url, &refspec])
        .env("GIT_CONFIG_COUNT", "1")
        .env("GIT_CONFIG_KEY_0", "http.extraHeader")
        .env("GIT_CONFIG_VALUE_0", auth_header(token))
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .output()?;

    if !out.status.success() {
        let stderr = String::from_utf8_lossy(&out.stderr).into_owned();
        return Err(PushError::Rejected(stderr));
    }

    tracing::info!(ref_name = %ref_name, mirror_url = %url, "mirror: push succeeded");
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn auth_header_encodes_token_as_oauth_basic() {
        // base64("tok:x-oauth-basic") == "dG9rOngtb2F1dGgtYmFzaWM=".
        assert_eq!(
            auth_header("tok"),
            "Authorization: Basic dG9rOngtb2F1dGgtYmFzaWM="
        );
    }
}
