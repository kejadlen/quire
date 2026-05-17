//! Wire format for the `GET /api/runs/{id}/bootstrap` API response.
//!
//! quire-ci deserializes this when `--transport api` is active.

use std::path::PathBuf;

use serde::{Deserialize, Serialize};

use crate::ci::run::RunMeta;

/// What quire-ci needs to mirror the orchestrator's Sentry context.
///
/// The DSN is passed over a secured channel (0600 file or bearer-token
/// authenticated API). `trace_id` is the hex form of
/// [`sentry::protocol::TraceId`]; kept as a string here so
/// `quire-core` doesn't grow a `sentry` dep.
#[derive(Debug, Serialize, Deserialize)]
pub struct SentryHandoff {
    pub dsn: String,
    pub trace_id: String,
}

/// Payload returned by `GET /api/runs/{id}/bootstrap`.
///
/// Carries push metadata, the bare repo path, and the Sentry handoff.
/// Secrets are not bundled here — quire-ci fetches them individually
/// via `GET /api/runs/{id}/secrets/{name}`.
#[derive(Debug, Serialize, Deserialize)]
pub struct Bootstrap {
    pub meta: RunMeta,
    pub git_dir: PathBuf,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sentry: Option<SentryHandoff>,
}
