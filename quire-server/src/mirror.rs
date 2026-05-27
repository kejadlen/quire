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
pub fn trigger(quire: &Quire, event: &PushEvent) -> crate::Result<()> {
    let repo = match quire.repo(&event.repo) {
        Ok(r) if r.exists() => r,
        Ok(_) => {
            tracing::warn!(repo = %event.repo, "mirror: repo not found on disk");
            return Ok(());
        }
        Err(e) => {
            return Err(crate::Error::Io(std::io::Error::other(e.to_string())));
        }
    };

    let config = quire.global_config()?;
    let mirror_token = config
        .github
        .mirror_token
        .map(|s| s.reveal().map(str::to_owned))
        .transpose()?;

    for push_ref in event.updated_refs() {
        let mirror_url = match repo.mirror_url(&push_ref.new_sha) {
            Ok(Some(url)) => url,
            Ok(None) => continue,
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

        let Some(token) = mirror_token.as_deref() else {
            tracing::warn!(
                ref_name = %push_ref.ref_name,
                "mirror: mirror-token not configured, skipping ref",
            );
            continue;
        };

        if let Err(e) = push_to_mirror(&repo, &push_ref.ref_name, &mirror_url, token) {
            tracing::error!(
                ref_name = %push_ref.ref_name,
                mirror_url,
                error = &e as &(dyn std::error::Error + 'static),
                "mirror: push failed",
            );
        }
    }

    Ok(())
}

fn push_to_mirror(
    repo: &crate::quire::Repo,
    ref_name: &str,
    mirror_url: &str,
    token: &str,
) -> crate::Result<()> {
    // Force-push the ref to the mirror. The `+` prefix allows rewrites.
    let refspec = format!("+{ref_name}:{ref_name}");

    // Pass the auth token via git config env vars so it never appears in argv.
    let out = repo
        .git(&["push", "--porcelain", mirror_url, &refspec])
        .env("GIT_CONFIG_COUNT", "1")
        .env("GIT_CONFIG_KEY_0", "http.extraHeader")
        .env(
            "GIT_CONFIG_VALUE_0",
            format!("Authorization: Bearer {token}"),
        )
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .output()?;

    if !out.status.success() {
        let stderr = String::from_utf8_lossy(&out.stderr);
        return Err(crate::Error::Io(std::io::Error::other(format!(
            "git push to {mirror_url} failed: {stderr}"
        ))));
    }

    tracing::info!(ref_name, mirror_url, "mirror: push succeeded");
    Ok(())
}
