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
    pub pushed_at: jiff::Timestamp,
    pub refs: Vec<PushRef>,
}

impl PushEvent {
    /// Build a push event from the repo name and updated refs.
    ///
    /// `repo` is the repo name relative to the repos dir (e.g. "foo.git").
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
            r#ref: "refs/heads/main".to_string(),
        }];
        let event = PushEvent::new("foo.git".to_string(), refs.clone());

        assert_eq!(event.r#type, "push");
        assert_eq!(event.repo, "foo.git");
        assert_eq!(event.refs, refs);
        assert!(event.pushed_at > jiff::Timestamp::UNIX_EPOCH);
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
