//! Shared transport types for CI ↔ server communication.
//!
//! The on-the-wire pairing both sides agree on. The orchestrator
//! constructs a `Transport` per run (minting the auth token);
//! quire-ci reconstructs it from the `QUIRE__*` environment variables.

/// Credentials and endpoint coordinates for a single CI run's API
/// channel. Holds everything quire-ci needs to call back to the
/// server about *this* run: which run, where the server is, and
/// the bearer token it issued.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ApiSession {
    /// Base URL of quire-server (e.g. `http://127.0.0.1:3000`).
    pub server_url: String,
    /// Bearer token minted at run creation time. Matches
    /// `runs.run_token` server-side. Also serves as the run
    /// identifier — the server looks up the run by this token.
    pub run_token: String,
}

/// Runtime transport for a single CI run.
/// Use `None` for local runs where no server is involved.
#[derive(Clone, Debug)]
pub struct Transport {
    pub session: ApiSession,
}
