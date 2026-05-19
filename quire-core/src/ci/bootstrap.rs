//! Wire format for handing off a run from the orchestrator to
//! `quire-ci`.
//!
//! For server-dispatched runs the orchestrator stores a [`Bootstrap`]
//! in the database; `quire-ci` fetches it via `GET /api/run/bootstrap`
//! using the per-run bearer token. For local dev runs without a server
//! the orchestrator writes the JSON to a file and passes the path via
//! `--bootstrap`.

use std::path::PathBuf;

use serde::{Deserialize, Serialize};

use crate::ci::run::RunMeta;

/// Inputs the orchestrator supplies to a quire-ci subprocess.
///
/// `git_dir` is the bare repo the run is scoped to. quire-ci surfaces
/// it via `(jobs :quire/push).git-dir`, which the mirror job's run-fn
/// passes to git as `GIT_DIR`. The materialized workspace is a flat
/// `git archive` extract with no `.git` inside, so quire-ci has no
/// way to recover this path on its own.
#[derive(Debug, Serialize, Deserialize)]
pub struct Bootstrap {
    pub meta: RunMeta,
    pub git_dir: PathBuf,
    /// Sentry trace id for the orchestrator's span, present only when
    /// the global config sets a DSN. Allows quire-ci to attach its
    /// events to the same trace. The DSN itself travels via the
    /// `QUIRE__SENTRY_DSN` environment variable.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sentry_trace_id: Option<String>,
}
