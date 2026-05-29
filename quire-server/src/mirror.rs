//! Server-side mirror: push updated refs to a remote on every push.
//!
//! Triggered from the push event handler, independent of CI.

use miette::Diagnostic;
use quire_core::event::{PushEvent, PushRef};
use thiserror::Error;

use crate::quire::Quire;

#[derive(Debug, Error, Diagnostic)]
#[error("mirror: {} ref(s) failed", errors.len())]
struct MirrorErrors {
    #[related]
    errors: Vec<MirrorError>,
}

#[derive(Debug, Error, Diagnostic)]
enum MirrorError {
    #[error("repo not found on disk: {0}")]
    RepoNotFound(String),

    #[error("git push to {url} failed: {stderr}")]
    PushFailed { url: String, stderr: String },

    #[error(transparent)]
    App(#[from] crate::Error),

    #[error(transparent)]
    Io(#[from] std::io::Error),
}

/// Mirror updated refs to a configured remote.
///
/// Reads `github.mirror-token` from global config for auth. For each updated
/// ref, reads `.quire/config.fnl` at the new SHA to obtain the `github.mirror`
/// URL. Skips repos with no mirror URL configured. If no token is configured,
/// mirroring is skipped entirely.
pub fn trigger(quire: &Quire, event: &PushEvent) -> miette::Result<()> {
    let repo = quire.repo(&event.repo)?;
    if !repo.exists() {
        return Err(MirrorError::RepoNotFound(event.repo.clone()).into());
    }

    let config = quire.global_config();
    let Some(mirror_token) = config
        .github
        .mirror_token
        .as_ref()
        .map(|s| s.reveal().map(str::to_owned))
        .transpose()?
    else {
        return Ok(());
    };

    let errors: Vec<MirrorError> = event
        .updated_refs()
        .into_iter()
        .filter_map(|push_ref| mirror_ref(&repo, push_ref, &mirror_token).err())
        .collect();

    if errors.is_empty() {
        Ok(())
    } else {
        Err(MirrorErrors { errors }.into())
    }
}

fn mirror_ref(
    repo: &crate::quire::Repo,
    push_ref: &PushRef,
    token: &str,
) -> Result<(), MirrorError> {
    let repo_config = repo.repo_config(&push_ref.new_sha)?;
    let Some(mirror_url) = repo_config.github.mirror else {
        return Ok(());
    };

    // Force-push the ref to the mirror. The `+` prefix allows rewrites.
    let refspec = format!("+{r}:{r}", r = push_ref.ref_name);

    // Pass the auth token via git config env vars so it never appears in argv.
    let out = repo
        .git(&["push", "--porcelain", &mirror_url, &refspec])
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
        return Err(MirrorError::PushFailed {
            url: mirror_url,
            stderr: String::from_utf8_lossy(&out.stderr).into_owned(),
        });
    }

    tracing::info!(
        ref_name = %push_ref.ref_name,
        mirror_url = %mirror_url,
        "mirror: push succeeded"
    );
    Ok(())
}
