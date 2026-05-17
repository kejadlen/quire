//! Shared transport types for CI ↔ server communication.
//!
//! The on-the-wire pairing both sides agree on. The orchestrator
//! constructs a `Transport` per run (minting the auth token and
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

/// Transport mode for CI ↔ server communication.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, serde::Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum TransportMode {
    #[default]
    Filesystem,
    Api,
}

impl std::str::FromStr for TransportMode {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "filesystem" => Ok(Self::Filesystem),
            "api" => Ok(Self::Api),
            other => Err(format!("unknown transport mode: {other}")),
        }
    }
}

/// Runtime transport for a single CI run.
/// Use `None` for local runs where no server is involved.
#[derive(Clone, Debug)]
pub struct Transport {
    pub session: ApiSession,
    pub mode: TransportMode,
    /// When true, skip writing secrets to bootstrap; quire-ci fetches
    /// secrets via `GET /api/runs/{id}/secrets/{name}` instead.
    pub api_secrets: bool,
}
