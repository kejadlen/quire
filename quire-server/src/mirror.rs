//! Server-side mirror: push a branch (and a version tag) to a remote on every push.
//!
//! Triggered from the push event handler, independent of CI.

use quire_core::event::PushEvent;

use crate::quire::Quire;

/// Mirror updated refs to a configured remote.
///
/// Reads `github.mirror-token` from global config for auth. For each updated
/// ref, reads `.quire/config.fnl` at the new SHA to obtain `github.mirror` (URL)
/// and `github.branch` (the ref that triggers mirroring). Skips refs that don't
/// match the configured branch, and repos with no mirror URL set.
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

        if push_ref.ref_name != repo_config.github.branch {
            continue;
        }

        push_to_mirror(
            &repo,
            &push_ref.new_sha,
            &push_ref.ref_name,
            &mirror_url,
            mirror_token.as_deref(),
        );
    }
}

fn push_to_mirror(
    repo: &crate::quire::Repo,
    sha: &str,
    ref_name: &str,
    mirror_url: &str,
    token: Option<&str>,
) {
    let tag = make_tag(sha);

    // Create the tag locally; ignore "already exists" errors.
    let tag_out = repo
        .git(&["tag", &tag, sha])
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::piped())
        .output();
    match tag_out {
        Err(e) => {
            tracing::error!(sha, tag, error = %e, "mirror: failed to run git tag");
            return;
        }
        Ok(out) if !out.status.success() => {
            let stderr = String::from_utf8_lossy(&out.stderr);
            if !stderr.contains("already exists") {
                tracing::error!(sha, tag, %stderr, "mirror: git tag failed");
                return;
            }
        }
        Ok(_) => {}
    }

    // Force-push the branch and push the tag.
    // The `+` prefix in the branch refspec allows fast-forward and force pushes.
    let refspec_branch = format!("+{ref_name}:{ref_name}");
    let refspec_tag = format!("refs/tags/{tag}:refs/tags/{tag}");
    let mut cmd = repo.git(&[
        "push",
        "--porcelain",
        mirror_url,
        &refspec_branch,
        &refspec_tag,
    ]);

    // Pass the auth token via git config env vars so it never appears in argv.
    if let Some(token) = token {
        cmd.env("GIT_CONFIG_COUNT", "1")
            .env("GIT_CONFIG_KEY_0", "http.extraHeader")
            .env("GIT_CONFIG_VALUE_0", format!("Authorization: Bearer {token}"));
    }

    match cmd
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .output()
    {
        Ok(out) if out.status.success() => {
            tracing::info!(ref_name, tag, mirror_url, "mirror: push succeeded");
        }
        Ok(out) => {
            tracing::error!(
                ref_name,
                tag,
                mirror_url,
                stderr = %String::from_utf8_lossy(&out.stderr),
                "mirror: push failed",
            );
        }
        Err(e) => {
            tracing::error!(ref_name, mirror_url, error = %e, "mirror: failed to run git push");
        }
    }
}

fn make_tag(sha: &str) -> String {
    let date = jiff::Timestamp::now().strftime("%Y-%m-%d").to_string();
    let sha8 = &sha[..sha.len().min(8)];
    format!("v{date}-{sha8}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn make_tag_format() {
        let sha = "abc12345def67890";
        let tag = make_tag(sha);
        assert!(tag.starts_with('v'), "tag should start with 'v': {tag}");
        // v<date>-<sha8>
        let parts: Vec<&str> = tag.splitn(2, '-').collect();
        assert_eq!(parts.len(), 2);
        assert_eq!(&tag[tag.len() - 8..], "abc12345");
    }

    #[test]
    fn make_tag_short_sha() {
        let sha = "abc";
        let tag = make_tag(sha);
        assert!(tag.ends_with("abc"), "short sha should be used as-is: {tag}");
    }
}
