//! Wire-format events emitted by `quire-ci` during a pipeline run.
//!
//! Events are serialized as one JSON object per line (JSONL), tagged
//! by `type`. The stream interleaves `job_started`, `sh_started`,
//! `sh_finished`, and `job_completed` / `job_failed` events in
//! execution order.

use serde::{Deserialize, Serialize};

/// A single event in the run's structured output stream.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Event {
    JobStarted { job_id: String },
    JobCompleted { job_id: String },
    JobFailed { job_id: String },
    ShStarted { job_id: String, cmd: String },
    ShFinished { job_id: String, exit_code: i32 },
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn job_started_event_serializes_in_expected_shape() {
        let event = Event::JobStarted {
            job_id: "build".into(),
        };
        let json = serde_json::to_string(&event).unwrap();
        assert_eq!(json, r#"{"type":"job_started","job_id":"build"}"#);
    }

    #[test]
    fn job_completed_event_serializes_in_expected_shape() {
        let event = Event::JobCompleted {
            job_id: "build".into(),
        };
        let json = serde_json::to_string(&event).unwrap();
        assert_eq!(json, r#"{"type":"job_completed","job_id":"build"}"#);
    }

    #[test]
    fn job_failed_event_serializes_in_expected_shape() {
        let event = Event::JobFailed {
            job_id: "build".into(),
        };
        let json = serde_json::to_string(&event).unwrap();
        assert_eq!(json, r#"{"type":"job_failed","job_id":"build"}"#);
    }

    #[test]
    fn sh_started_event_serializes_in_expected_shape() {
        let event = Event::ShStarted {
            job_id: "build".into(),
            cmd: "echo hi".into(),
        };
        let json = serde_json::to_string(&event).unwrap();
        assert_eq!(
            json,
            r#"{"type":"sh_started","job_id":"build","cmd":"echo hi"}"#
        );
    }

    #[test]
    fn sh_finished_event_serializes_in_expected_shape() {
        let event = Event::ShFinished {
            job_id: "build".into(),
            exit_code: 0,
        };
        let json = serde_json::to_string(&event).unwrap();
        assert_eq!(
            json,
            r#"{"type":"sh_finished","job_id":"build","exit_code":0}"#
        );
    }
}
