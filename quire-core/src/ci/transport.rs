//! Shared transport types for CI ↔ server communication.
//!
//! The on-the-wire pairing both sides agree on. The orchestrator
//! constructs an `ApiSession` per run (minting the auth token and
//! using the run's UUID); quire-ci reconstructs it from CLI flags
//! plus `QUIRE_CI_TOKEN`.

/// Credentials and endpoint coordinates for a single CI run's API
/// channel. Holds everything quire-ci needs to call back to the
/// server about *this* run: which run, where the server is, and
/// the bearer token it issued.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ApiSession {
    /// Run UUID assigned by the orchestrator. Also the value stored
    /// in `runs.id` server-side.
    pub run_id: String,
    /// Base URL of quire-server (e.g. `http://127.0.0.1:3000`).
    pub server_url: String,
    /// Bearer token minted at run creation time. Matches
    /// `runs.auth_token` server-side.
    pub auth_token: String,
}
