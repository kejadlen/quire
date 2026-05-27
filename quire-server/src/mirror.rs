//! Server-side mirror: push updated refs to a remote on every push.
//!
//! Triggered from the push event handler, independent of CI.

use quire_core::event::PushEvent;

use crate::quire::Quire;

/// Mirror updated refs to a configured remote.
///
/// Reads `github.mirror-token` from global config for auth. For each updated
/// ref, reads `.quire/config.fnl` at the new SHA to obtain the `github.mirror`
/// URL. Skips repos with no mirror URL configured.
pub fn trigger(quire: &Quire, event: &PushEvent) {
    let repo = match quire.repo(&event.repo) {
        Ok(r) if r.exists() => r,
        Ok(_) => {
            tracing::warn!(repo = %event.repo, "mirror: repo not found on disk");
            return;
        }
        Err(e) => {
            tracing::error!(repo = %event.repo, error = %e, "mirror: invalid repo name");
            return;
        }
    };

    let config = match quire.global_config() {
        Ok(c) => c,
        Err(e) => {
            tracing::error!(
                repo = %event.repo,
                error = &e as &(dyn std::error::Error + 'static),
                "mirror: failed to load global config",
            );
            return;
        }
    };

    let mirror_token = match config.github.mirror_token {
        None => None,
        Some(ref secret) => match secret.reveal() {
            Ok(t) => Some(t.to_string()),
            Err(e) => {
                tracing::error!(
                    error = &e as &(dyn std::error::Error + 'static),
                    "mirror: failed to reveal mirror token",
                );
                return;
            }
        },
    };

    for push_ref in event.updated_refs() {
        let repo_config = match repo.repo_config(&push_ref.new_sha) {
            Ok(c) => c,
            Err(e) => {
                tracing::warn!(
                    ref_name = %push_ref.ref_name,
                    sha = %push_ref.new_sha,
                    error = &e as &(dyn std::error::Error + 'static),
                    "mirror: failed to read repo config, skipping ref",
                );
                continue;
            }
        };

        let Some(mirror_url) = repo_config.github.mirror else {
            continue;
        };

        push_to_mirror(
            &repo,
            &push_ref.ref_name,
            &mirror_url,
            mirror_token.as_deref(),
        );
    }
}

fn push_to_mirror(
    repo: &crate::quire::Repo,
    ref_name: &str,
    mirror_url: &str,
    token: Option<&str>,
) {
    // Force-push the ref to the mirror. The `+` prefix allows rewrites.
    let refspec = format!("+{ref_name}:{ref_name}");
    let mut cmd = repo.git(&["push", "--porcelain", mirror_url, &refspec]);

    // Pass the auth token via git config env vars so it never appears in argv.
    if let Some(token) = token {
        cmd.env("GIT_CONFIG_COUNT", "1")
            .env("GIT_CONFIG_KEY_0", "http.extraHeader")
            .env(
                "GIT_CONFIG_VALUE_0",
                format!("Authorization: Bearer {token}"),
            );
    }

    match cmd
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .output()
    {
        Ok(out) if out.status.success() => {
            tracing::info!(ref_name, mirror_url, "mirror: push succeeded");
        }
        Ok(out) => {
            tracing::error!(
                ref_name,
                mirror_url,
                stderr = %String::from_utf8_lossy(&out.stderr),
                "mirror: push failed",
            );
        }
        Err(e) => {
            tracing::error!(
                ref_name,
                mirror_url,
                error = %e,
                "mirror: failed to run git push",
            );
        }
    }
}
