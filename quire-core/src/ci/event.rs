//! Wire-format events emitted by `quire-ci` during a pipeline run.
//!
//! Each event is one JSON object on its own line (JSONL). The
//! envelope holds the producer-side timestamp; [`EventKind`] holds
//! the variant-specific payload. `#[serde(flatten)]` keeps the wire
//! format flat: `{"at_ms": 110, "type": "sh_started", "job_id": …}`.
//!
//! Consumers that need durations pair `*Started` with the matching
//! `*Finished` by `job_id` (plus per-job sh sequence) and read both
//! events' `at_ms` fields.

use serde::{Deserialize, Serialize};

/// Terminal state of a job in the event stream.
#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum JobOutcome {
    Complete,
    Failed,
}

/// Outcome of the complete pipeline run, carried by [`EventKind::RunFinished`].
#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum RunOutcome {
    Success,
    PipelineFailure,
}

/// A single event in the run's structured output stream.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct Event {
    /// Producer-side wall-clock millisecond timestamp.
    pub at_ms: i64,
    #[serde(flatten)]
    pub kind: EventKind,
}

/// The variant-specific payload of an [`Event`].
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum EventKind {
    /// A job's run-fn is about to fire.
    JobStarted { job_id: String },
    /// A job's run-fn returned. `outcome` is `complete` if the run-fn
    /// returned `Ok`, else `failed`.
    JobFinished { job_id: String, outcome: JobOutcome },
    /// An sh process is about to spawn.
    ShStarted { job_id: String, cmd: String },
    /// An sh process exited.
    ShFinished { job_id: String, exit_code: i32 },
    /// The pipeline run has finished cleanly. Always the last event in
    /// the stream — its presence signals that quire-ci ran to completion
    /// rather than crashing mid-run.
    RunFinished { outcome: RunOutcome },
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn job_started_serializes_in_expected_shape() {
        let event = Event {
            at_ms: 100,
            kind: EventKind::JobStarted {
                job_id: "build".into(),
            },
        };
        let json = serde_json::to_string(&event).unwrap();
        assert_eq!(
            json,
            r#"{"at_ms":100,"type":"job_started","job_id":"build"}"#
        );
    }

    #[test]
    fn job_finished_serializes_in_expected_shape() {
        let event = Event {
            at_ms: 250,
            kind: EventKind::JobFinished {
                job_id: "build".into(),
                outcome: JobOutcome::Complete,
            },
        };
        let json = serde_json::to_string(&event).unwrap();
        assert_eq!(
            json,
            r#"{"at_ms":250,"type":"job_finished","job_id":"build","outcome":"complete"}"#
        );
    }

    #[test]
    fn job_finished_failed_outcome_serializes_as_failed() {
        let event = Event {
            at_ms: 250,
            kind: EventKind::JobFinished {
                job_id: "build".into(),
                outcome: JobOutcome::Failed,
            },
        };
        let json = serde_json::to_string(&event).unwrap();
        assert!(json.contains(r#""outcome":"failed""#));
    }

    #[test]
    fn sh_started_serializes_in_expected_shape() {
        let event = Event {
            at_ms: 110,
            kind: EventKind::ShStarted {
                job_id: "build".into(),
                cmd: "echo hi".into(),
            },
        };
        let json = serde_json::to_string(&event).unwrap();
        assert_eq!(
            json,
            r#"{"at_ms":110,"type":"sh_started","job_id":"build","cmd":"echo hi"}"#
        );
    }

    #[test]
    fn sh_finished_serializes_in_expected_shape() {
        let event = Event {
            at_ms: 190,
            kind: EventKind::ShFinished {
                job_id: "build".into(),
                exit_code: 0,
            },
        };
        let json = serde_json::to_string(&event).unwrap();
        assert_eq!(
            json,
            r#"{"at_ms":190,"type":"sh_finished","job_id":"build","exit_code":0}"#
        );
    }

    #[test]
    fn event_round_trips_through_json() {
        let event = Event {
            at_ms: 110,
            kind: EventKind::ShStarted {
                job_id: "build".into(),
                cmd: "echo hi".into(),
            },
        };
        let json = serde_json::to_string(&event).unwrap();
        let decoded: Event = serde_json::from_str(&json).unwrap();
        assert_eq!(decoded, event);
    }
}
