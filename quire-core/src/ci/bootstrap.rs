//! Wire format for handing off a run from the orchestrator to
//! `quire-ci`.
//!
//! The orchestrator writes a [`Bootstrap`] as JSON to a file inside
//! the run directory and passes the path via `--bootstrap`. `quire-ci`
//! deserializes it on startup to recover push metadata. Standalone
//! `quire-ci run` invocations skip the file entirely and fall back to
//! placeholder values.
//!
//! The file is a one-shot handoff: `quire-ci` unlinks it as soon as
//! it has read the bytes into memory, and the orchestrator
//! best-effort unlinks after the subprocess exits as a safety net.

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
