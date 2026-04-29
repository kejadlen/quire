use crate::ci::{RunMeta, RunState, RunStateFile};

/// A single ref update from a push.
#[derive(Clone, Debug, serde::Serialize, serde::Deserialize, PartialEq)]
pub struct PushRef {
    pub r#ref: String,
    pub old_sha: String,
    pub new_sha: String,
}

/// A push event sent from hook to serve over the event socket.
#[derive(Debug, serde::Serialize, serde::Deserialize, PartialEq)]
pub struct PushEvent {
    pub r#type: String,
    pub repo: String,
    pub pushed_at: String,
    pub refs: Vec<PushRef>,
}

/// Dispatch a push event: CI gating and mirror push.
pub async fn dispatch_push(quire: &crate::Quire, event: &PushEvent) {
    let repo = match quire.repo(&event.repo) {
        Ok(r) if r.exists() => r,
        Ok(_) => {
            tracing::error!(repo = %event.repo, "repo not found on disk");
            return;
        }
        Err(e) => {
            tracing::error!(repo = %event.repo, %e, "invalid repo name in event");
            return;
        }
    };

    dispatch_ci(&repo, event);
    dispatch_mirror(quire, repo, event).await;
}

/// Check each updated ref for .quire/ci.fnl, create runs, and eval + validate.
fn dispatch_ci(repo: &crate::quire::Repo, event: &PushEvent) {
    for push_ref in event.updated_refs() {
        if !repo.has_ci_fnl(&push_ref.new_sha) {
            continue;
        }

        let meta = RunMeta {
            sha: push_ref.new_sha.clone(),
            r#ref: push_ref.r#ref.clone(),
            pushed_at: event.pushed_at.clone(),
        };

        let runs = repo.runs();
        let mut run = match runs.create(&meta) {
            Ok(r) => r,
            Err(e) => {
                tracing::error!(repo = %event.repo, %e, "failed to create CI run");
                continue;
            }
        };

        tracing::info!(
            run_id = %run.id(),
            sha = %push_ref.new_sha,
            r#ref = %push_ref.r#ref,
            "created CI run"
        );

        run.transition(RunState::Active).unwrap_or_else(|e| {
            tracing::error!(run_id = %run.id(), %e, "failed to transition run to active");
        });

        let result = eval_and_validate(repo, &push_ref.new_sha);
        match result {
            Ok(()) => {
                if let Err(e) = run.transition(RunState::Complete) {
                    tracing::error!(run_id = %run.id(), %e, "failed to transition run to complete");
                }
            }
            Err(e) => {
                tracing::error!(run_id = %run.id(), %e, "CI evaluation failed");
                if let Err(te) = run.transition(RunState::Failed) {
                    tracing::error!(run_id = %run.id(), %te, "failed to transition run to failed");
                } else if let Err(we) = run.write_state(&RunStateFile {
                    status: RunState::Failed,
                    started_at: None,
                    finished_at: Some(jiff::Zoned::now().to_string()),
                }) {
                    tracing::error!(run_id = %run.id(), %we, "failed to write state for failed run");
                }
            }
        }
    }
}

/// Evaluate ci.fnl at a given SHA and validate the job graph.
fn eval_and_validate(repo: &crate::quire::Repo, sha: &str) -> crate::Result<()> {
    let source = repo.ci_fnl_source(sha)?;
    let fennel = crate::fennel::Fennel::new()?;
    let eval_result = crate::ci::eval_ci(&fennel, &source, &format!("{sha}:.quire/ci.fnl"))?;
    crate::ci::validate(&eval_result.jobs)?;
    Ok(())
}

/// Push updated refs to the configured mirror.
async fn dispatch_mirror(quire: &crate::Quire, repo: crate::quire::Repo, event: &PushEvent) {
    let config = match repo.config() {
        Ok(c) => c,
        Err(e) => {
            tracing::error!(repo = %event.repo, %e, "failed to load repo config");
            return;
        }
    };

    let Some(mirror) = config.mirror else {
        tracing::debug!(repo = %event.repo, "no mirror configured, skipping");
        return;
    };

    let global_config = match quire.global_config() {
        Ok(c) => c,
        Err(e) => {
            tracing::error!(%e, "failed to load global config for mirror push");
            return;
        }
    };

    let token = match global_config.github.token.reveal() {
        Ok(t) => t.to_string(),
        Err(e) => {
            tracing::error!(%e, "failed to resolve GitHub token");
            return;
        }
    };

    // Only push refs that were actually updated (non-zero new sha).
    let refs: Vec<String> = event
        .updated_refs()
        .iter()
        .map(|r| r.r#ref.clone())
        .collect();

    if refs.is_empty() {
        return;
    }

    let mirror_url = mirror.url.clone();
    tracing::info!(url = %mirror.url, refs = ?refs, "pushing to mirror");

    let result = tokio::task::spawn_blocking(move || {
        let ref_slices: Vec<&str> = refs.iter().map(|s| s.as_str()).collect();
        repo.push_to_mirror(&mirror, &token, &ref_slices)
    })
    .await;

    match result {
        Ok(Ok(())) => tracing::info!(url = %mirror_url, "mirror push complete"),
        Ok(Err(e)) => tracing::error!(url = %mirror_url, %e, "mirror push failed"),
        Err(e) => tracing::error!(url = %mirror_url, %e, "mirror push task panicked"),
    }
}

impl PushEvent {
    /// Build a push event from the repo name and updated refs.
    ///
    /// `repo` is the repo name relative to the repos dir (e.g. "foo.git").
    pub fn new(repo: String, refs: Vec<PushRef>) -> Self {
        let pushed_at = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs().to_string())
            .unwrap_or_else(|_| "0".to_string());

        Self {
            r#type: "push".to_string(),
            repo,
            pushed_at,
            refs,
        }
    }

    /// Refs that are not deletions (non-zero new sha).
    pub fn updated_refs(&self) -> Vec<&PushRef> {
        self.refs
            .iter()
            .filter(|r| r.new_sha != "0000000000000000000000000000000000000000")
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn push_event_new_populates_fields() {
        let refs = vec![PushRef {
            old_sha: "a".to_string(),
            new_sha: "b".to_string(),
            r#ref: "refs/heads/main".to_string(),
        }];
        let event = PushEvent::new("foo.git".to_string(), refs.clone());

        assert_eq!(event.r#type, "push");
        assert_eq!(event.repo, "foo.git");
        assert_eq!(event.refs, refs);
        assert_ne!(event.pushed_at, "0");
    }

    #[test]
    fn push_event_round_trips_json() {
        let refs = vec![
            PushRef {
                old_sha: "aaa".to_string(),
                new_sha: "bbb".to_string(),
                r#ref: "refs/heads/main".to_string(),
            },
            PushRef {
                old_sha: "ccc".to_string(),
                new_sha: "ddd".to_string(),
                r#ref: "refs/heads/feature".to_string(),
            },
        ];
        let event = PushEvent::new("work/foo.git".to_string(), refs);

        let json = serde_json::to_string(&event).expect("serialize");
        let parsed: PushEvent = serde_json::from_str(&json).expect("deserialize");

        assert_eq!(parsed.r#type, "push");
        assert_eq!(parsed.repo, "work/foo.git");
        assert_eq!(parsed.refs.len(), 2);
        assert_eq!(parsed.refs[0].r#ref, "refs/heads/main");
        assert_eq!(parsed.refs[1].r#ref, "refs/heads/feature");
    }
}
