//! Wire format for handing off a run from the orchestrator to
//! `quire-ci`.
//!
//! The orchestrator writes a [`Bootstrap`] as JSON to a file inside
//! the run directory and passes the path via `--bootstrap`. `quire-ci`
//! deserializes it on startup to recover push metadata and the
//! secrets the run-fns may resolve. Standalone `quire-ci run`
//! invocations skip the file entirely and fall back to placeholder
//! values.
//!
//! The file contains live secret values; callers must restrict
//! permissions (mode 0600 on Unix) before writing. The file is a
//! one-shot handoff: `quire-ci` unlinks it as soon as it has read
//! the bytes into memory, and the orchestrator best-effort unlinks
//! after the subprocess exits as a safety net. Plaintext secrets
//! should never persist in the run directory.

use std::collections::HashMap;
use std::path::PathBuf;

use serde::{Deserialize, Serialize};

use crate::ci::run::RunMeta;

/// Inputs the orchestrator supplies to a quire-ci subprocess.
///
/// Secret values cross as plaintext — `SecretString` deliberately
/// doesn't implement `Serialize` to avoid accidental leaks. The
/// orchestrator reveals values into this map before writing the
/// file (mode 0600); quire-ci wraps them back into `SecretString`s
/// on read.
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
    pub secrets: HashMap<String, String>,
    /// Sentry handoff, present only when the orchestrator's global
    /// config sets a DSN. Carries the matching trace id so both
    /// sides' events land on the same trace.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sentry: Option<SentryHandoff>,
}

/// What quire-ci needs to mirror the orchestrator's Sentry context.
///
/// The DSN is plaintext like the secrets — the 0600 mode on the
/// bootstrap file is the line of defense. `trace_id` is the hex form
/// of [`sentry::protocol::TraceId`]; kept as a string here so
/// `quire-core` doesn't grow a `sentry` dep.
#[derive(Debug, Serialize, Deserialize)]
pub struct SentryHandoff {
    pub dsn: String,
    pub trace_id: String,
}
