//! Compile-time and runtime CI primitives shared with `quire-ci`.
//!
//! The orchestrator lives in `quire-server::ci`; this module owns the
//! pieces that need to run identically inside a per-run container
//! (where `quire-ci` invokes them) and on the server (where the
//! orchestrator drives them).

pub mod dispatch;
pub mod event;
pub mod logs;
pub mod pipeline;
pub mod registration;
pub mod run;
pub mod runtime;
pub mod transport;
