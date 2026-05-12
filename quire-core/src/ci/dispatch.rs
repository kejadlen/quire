//! Wire format for handing off a run from the orchestrator to
//! `quire-ci`.
//!
//! The orchestrator writes a [`Dispatch`] as JSON to a file inside
//! the run directory and passes the path via `--dispatch`. `quire-ci`
//! deserializes it on startup to recover push metadata and the
//! secrets the run-fns may resolve. Standalone `quire-ci run`
//! invocations skip the file entirely and fall back to placeholder
//! values.
//!
//! The file contains live secret values; callers must restrict
//! permissions (mode 0600 on Unix) before writing.

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
pub struct Dispatch {
    pub meta: RunMeta,
    pub git_dir: PathBuf,
    pub secrets: HashMap<String, String>,
}
