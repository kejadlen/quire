//! Run-level data shared between the orchestrator and the runtime.
//!
//! The full `Run` lifecycle (state machine, db rows, log files) lives
//! in `quire-server::ci::run`. This module carries only the immutable
//! metadata and session credentials the runtime needs at execute time.

use jiff::Timestamp;
use rand::distr::Alphanumeric;
use rand::Rng as _;

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

/// Credentials and endpoint for a single orchestrator-dispatched run.
/// Holds everything quire-ci needs to call back to the server:
/// where it is, and the bearer token it issued.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ApiSession {
    /// Base URL of quire-server (e.g. `http://127.0.0.1:3000`).
    pub server_url: String,
    /// Bearer token minted at run creation time. Matches
    /// `runs.run_token` server-side.
    pub run_token: String,
}

impl ApiSession {
    /// Mint a fresh session for a new run. Generates a CSPRNG bearer
    /// token and derives the loopback server URL from `port`.
    pub fn new(port: u16) -> Self {
        Self {
            server_url: format!("http://127.0.0.1:{port}"),
            run_token: rand::rng()
                .sample_iter(&Alphanumeric)
                .take(32)
                .map(char::from)
                .collect(),
        }
    }
}
