//! Wire format for the bootstrap API response.
//!
//! The orchestrator stores a [`Bootstrap`] in the database; `quire-ci`
//! fetches it via `GET /api/run/bootstrap` using the per-run bearer
//! token. Local runs pass `--local --git-dir <path>` and derive the
//! commit SHA and ref directly from the git dir.

use serde::{Deserialize, Serialize};

use crate::ci::run::RunMeta;

/// Inputs the orchestrator supplies to a quire-ci subprocess.
///
/// Carries push facts (`meta`), run identity (`repo`, `run_id`), and
/// the trace context (`traceparent`). The bare repo the run is scoped
/// to stays server-side — quire-ci runs against the materialized
/// workspace and has no need for it.
#[derive(Debug, Serialize, Deserialize)]
pub struct Bootstrap {
    pub meta: RunMeta,
    /// The repo this run is scoped to (matches the `runs.repo`
    /// column). quire-ci tags Sentry events with it.
    pub repo: String,
    /// The server-assigned run id (UUIDv7, the `runs.id` PK).
    /// quire-ci tags Sentry events with it.
    pub run_id: String,
    /// W3C traceparent header value for the orchestrator's span, present only when
    /// the global config sets a DSN. Allows quire-ci to attach its
    /// events to the same trace via OTEL context propagation. The DSN itself travels via the
    /// `QUIRE__SENTRY_DSN` environment variable.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub traceparent: Option<String>,
}
