//! Response types for the quire-server HTTP API.

/// Response body from `GET /api/runs/:run_id/secrets/:name`.
#[derive(Debug, serde::Deserialize)]
pub struct SecretResponse {
    pub value: String,
}
