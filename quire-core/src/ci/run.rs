//! Run-level data shared between the orchestrator and the runtime.
//!
//! The full `Run` lifecycle (state machine, db rows, log files) lives
//! in `quire-server::ci::run`. This module carries only the immutable
//! metadata that the runtime needs at execute time so it can run in
//! either a server or an in-container `quire-ci` context.

use jiff::Timestamp;

/// Immutable metadata for a CI run. Passed to `Runs::create` at
/// enqueue time; the fields are written to the `runs` row once.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct RunMeta {
    /// The commit SHA that triggered this run.
    pub sha: String,
    /// The full ref name (e.g. `refs/heads/main`).
    pub r#ref: String,
    /// When the push occurred.
    pub pushed_at: Timestamp,
}
