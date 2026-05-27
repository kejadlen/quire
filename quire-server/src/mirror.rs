//! Server-side mirror: push updated refs to a remote on every push.
//!
//! Triggered from the push event handler, independent of CI.

use miette::{Diagnostic, IntoDiagnostic as _};
use quire_core::event::{PushEvent, PushRef};
use thiserror::Error;

use crate::quire::Quire;

#[derive(Debug, Error, Diagnostic)]
#[error("mirror: {count} ref(s) failed")]
struct MirrorErrors {
    count: usize,
    #[related]
    errors: Vec<miette::Report>,
}

/// Mirror updated refs to a configured remote.
///
/// Reads `github.mirror-token` from global config for auth. For each updated
/// ref, reads `.quire/config.fnl` at the new SHA to obtain the `github.mirror`
/// URL. Skips repos with no mirror URL configured.
pub fn trigger(quire: &Quire, event: &PushEvent) -> miette::Result<()> {
    let repo = match quire.repo(&event.repo) {
        Ok(r) if r.exists() => r,
        Ok(_) => {
            return Err(miette::miette!("repo not found on disk: {}", event.repo));
        }
        Err(e) => return Err(e),
    };

    let config = quire.global_config().into_diagnostic()?;
    let mirror_token = config
        .github
        .mirror_token
        .map(|s| s.reveal().map(str::to_owned))
        .transpose()
        .into_diagnostic()?;

    let mut errors: Vec<miette::Report> = vec![];

    for push_ref in event.updated_refs() {
        if let Err(e) = mirror_ref(&repo, push_ref, mirror_token.as_deref()) {
            errors.push(miette::miette!("{}: {e}", push_ref.ref_name));
        }
    }

    if errors.is_empty() {
        Ok(())
    } else {
        let count = errors.len();
        Err(MirrorErrors { count, errors }.into())
    }
}

fn mirror_ref(
    repo: &crate::quire::Repo,
    push_ref: &PushRef,
    token: Option<&str>,
) -> crate::Result<()> {
    let repo_config = repo.repo_config(&push_ref.new_sha)?;
    let Some(mirror_url) = repo_config.github.mirror else {
        return Ok(());
    };
    let token = token
        .ok_or_else(|| crate::Error::Io(std::io::Error::other("mirror-token not configured")))?;
    push_to_mirror(repo, &push_ref.ref_name, &mirror_url, token)
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
