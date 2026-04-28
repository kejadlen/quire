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

/// Build a push event from parsed refs.
///
/// `repo` is the repo name relative to the repos dir (e.g. "foo.git").
/// `pushed_at` is seconds since Unix epoch as a string.
pub fn build_push_event(repo: String, refs: Vec<PushRef>) -> PushEvent {
    let pushed_at = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs().to_string())
        .unwrap_or_else(|_| "0".to_string());

    PushEvent {
        r#type: "push".to_string(),
        repo,
        pushed_at,
        refs,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn build_push_event_populates_fields() {
        let refs = vec![PushRef {
            old_sha: "a".to_string(),
            new_sha: "b".to_string(),
            r#ref: "refs/heads/main".to_string(),
        }];
        let event = build_push_event("foo.git".to_string(), refs.clone());

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
        let event = build_push_event("work/foo.git".to_string(), refs);

        let json = serde_json::to_string(&event).expect("serialize");
        let parsed: PushEvent = serde_json::from_str(&json).expect("deserialize");

        assert_eq!(parsed.r#type, "push");
        assert_eq!(parsed.repo, "work/foo.git");
        assert_eq!(parsed.refs.len(), 2);
        assert_eq!(parsed.refs[0].r#ref, "refs/heads/main");
        assert_eq!(parsed.refs[1].r#ref, "refs/heads/feature");
    }
}
