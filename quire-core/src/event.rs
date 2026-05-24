/// A single ref update from a push.
#[derive(Clone, Debug, serde::Serialize, serde::Deserialize, PartialEq)]
pub struct PushRef {
    #[serde(rename = "ref")]
    pub ref_name: String,
    pub old_sha: String,
    pub new_sha: String,
}

/// A push event sent from hook to server over the event socket, and
/// from quire-server to quire-ci over the webhook.
#[derive(Debug, serde::Serialize, serde::Deserialize, PartialEq)]
pub struct PushEvent {
    // Kept for backward compat with the unix socket protocol.
    pub r#type: String,
    pub repo: String,
    pub pushed_at: jiff::Timestamp,
    pub refs: Vec<PushRef>,
}

impl PushEvent {
    pub fn new(repo: String, refs: Vec<PushRef>) -> Self {
        Self {
            r#type: "push".to_string(),
            repo,
            pushed_at: jiff::Timestamp::now(),
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
            ref_name: "refs/heads/main".to_string(),
        }];
        let event = PushEvent::new("foo.git".to_string(), refs.clone());

        assert_eq!(event.r#type, "push");
        assert_eq!(event.repo, "foo.git");
        assert_eq!(event.refs, refs);
        assert!(event.pushed_at > jiff::Timestamp::UNIX_EPOCH);
    }

    #[test]
    fn updated_refs_filters_deletions() {
        let refs = vec![
            PushRef {
                old_sha: "aaa".to_string(),
                new_sha: "bbb".to_string(),
                ref_name: "refs/heads/main".to_string(),
            },
            PushRef {
                old_sha: "ccc".to_string(),
                new_sha: "0000000000000000000000000000000000000000".to_string(),
                ref_name: "refs/heads/feature".to_string(),
            },
        ];
        let event = PushEvent::new("foo.git".to_string(), refs);

        let updated = event.updated_refs();
        assert_eq!(updated.len(), 1);
        assert_eq!(updated[0].ref_name, "refs/heads/main");
    }

    #[test]
    fn push_event_round_trips_json() {
        let refs = vec![
            PushRef {
                old_sha: "aaa".to_string(),
                new_sha: "bbb".to_string(),
                ref_name: "refs/heads/main".to_string(),
            },
            PushRef {
                old_sha: "ccc".to_string(),
                new_sha: "ddd".to_string(),
                ref_name: "refs/heads/feature".to_string(),
            },
        ];
        let event = PushEvent::new("work/foo.git".to_string(), refs);

        let json = serde_json::to_string(&event).expect("serialize");
        let parsed: PushEvent = serde_json::from_str(&json).expect("deserialize");

        assert_eq!(parsed.r#type, "push");
        assert_eq!(parsed.repo, "work/foo.git");
        assert_eq!(parsed.refs.len(), 2);
        assert_eq!(parsed.refs[0].ref_name, "refs/heads/main");
        assert_eq!(parsed.refs[1].ref_name, "refs/heads/feature");
    }
}
