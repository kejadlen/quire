mod sink;

use std::cell::RefCell;
use std::collections::HashMap;
use std::io;
use std::path::PathBuf;
use std::rc::Rc;

use clap::Parser;
use miette::IntoDiagnostic;
use quire_core::ci::event::{Event, EventKind, JobOutcome, RunOutcome};
use quire_core::ci::pipeline::{self, Pipeline, RunFn};
use quire_core::ci::run::RunMeta;
use quire_core::ci::runtime::{Runtime, RuntimeError, RuntimeEvent, RuntimeHandle};
use quire_core::ci::transport::ApiSession;
use quire_core::fennel::FennelError;
use quire_core::telemetry::{self, FmtMode, MietteLayer};

/// Errors from running a job's `run_fn`. Lua errors are re-wrapped
/// via [`FennelError::from_lua`] so they carry the same source-code
/// annotation that compile-time errors get — both miette (terminal)
/// and `%err` (tracing/Sentry) see the full diagnostic.
#[derive(Debug, thiserror::Error, miette::Diagnostic)]
enum JobError {
    #[error(transparent)]
    #[diagnostic(transparent)]
    Fennel(#[from] Box<FennelError>),

    #[error(transparent)]
    #[diagnostic(transparent)]
    Runtime(#[from] RuntimeError),
}

use crate::sink::{EventSink, JsonlSink, NullSink};

const VERSION: &str = env!("QUIRE_VERSION");

/// Run a quire CI pipeline locally.
#[derive(Parser)]
#[command(version, propagate_version = true)]
struct Cli {
    /// Workspace root containing .quire/ci.fnl. Defaults to cwd.
    #[arg(short, long, default_value = ".", global = true)]
    workspace: PathBuf,

    #[command(subcommand)]
    command: Commands,
}

#[derive(clap::Subcommand)]
enum Commands {
    /// Compile and validate a ci.fnl pipeline.
    Validate,

    /// Run the whole pipeline against the workspace, in topo order.
    ///
    /// `--dispatch <path>` points at a JSON file (see
    /// [`quire_core::ci::dispatch::Dispatch`]) that supplies push
    /// metadata and secrets when the orchestrator dispatches via
    /// `:executor :quire-ci`. Standalone invocations omit the flag
    /// and fall back to placeholder meta with no secrets — `(secret
    /// :name)` calls error, and `(jobs upstream)` reads return Nil
    /// for everything except `quire/push`.
    Run {
        /// Where to send the structured event stream. Accepts:
        ///   `null`   — drop events (default).
        ///   `stdout` — write JSONL to stdout.
        ///   `<path>` — write JSONL to this file. The orchestrator
        ///              reads the file post-run to populate `jobs`
        ///              and `sh_events` database rows.
        #[arg(long, default_value = "null", value_parser = parse_events_target)]
        events: EventsTarget,

        /// Directory for per-sh CRI log files. Defaults to a fresh
        /// tempdir whose path is printed on stdout at the end of the
        /// run.
        #[arg(long)]
        out_dir: Option<PathBuf>,

        /// Path to a JSON dispatch file produced by the orchestrator.
        /// Carries push metadata and the secrets the run-fns may
        /// resolve. Omit for standalone runs (placeholder meta, no
        /// secrets).
        #[arg(long)]
        dispatch: Option<PathBuf>,

        #[command(flatten)]
        transport: TransportFlags,
    },
}

/// CLI flags for the CI ↔ server transport. Grouped so the related
/// args travel together and `Transport == Api` can pull its required
/// peers in via `required_if_eq`.
#[derive(clap::Args, Debug)]
struct TransportFlags {
    /// Transport for CI ↔ server communication.
    #[arg(long, default_value = "filesystem", value_enum)]
    transport: Transport,

    /// Run ID assigned by the orchestrator.
    /// Required when `--transport api`.
    #[arg(long, required_if_eq("transport", "api"))]
    run_id: Option<String>,

    /// Base URL of quire-server (e.g. `http://127.0.0.1:3000`).
    /// Required when `--transport api`.
    #[arg(long, required_if_eq("transport", "api"))]
    server_url: Option<String>,
}

impl TransportFlags {
    /// Promote into the resolved [`TransportArgs`], folding in the
    /// `QUIRE_CI_TOKEN` env var. clap's `required_if_eq` guarantees
    /// `--run-id` and `--server-url` are present for `Transport::Api`;
    /// the token must arrive via env and is non-optional on the wire.
    fn resolve(self, auth_token: Option<String>) -> miette::Result<TransportArgs> {
        match self.transport {
            Transport::Filesystem => Ok(TransportArgs::Filesystem),
            Transport::Api => {
                let auth_token = auth_token.ok_or_else(|| {
                    miette::miette!("--transport api requires the QUIRE_CI_TOKEN env var")
                })?;
                Ok(TransportArgs::Api(ApiSession {
                    run_id: self
                        .run_id
                        .expect("clap requires --run-id when --transport api"),
                    server_url: self
                        .server_url
                        .expect("clap requires --server-url when --transport api"),
                    auth_token,
                }))
            }
        }
    }
}

/// RAII wrapper around the tempdir that holds a `quire-ci run`'s
/// captured sh logs when no `--out-dir` was passed. On drop, prints
/// each log file's contents to stdout, then lets the underlying
/// [`tempfile::TempDir`] clean up the directory. Drop fires whether
/// the run succeeded or failed.
struct DumpLogsOnDrop {
    dir: tempfile::TempDir,
}

impl DumpLogsOnDrop {
    fn path(&self) -> &std::path::Path {
        self.dir.path()
    }

    /// Walk `<path>/jobs/<job_id>/sh-<n>.log` in alphabetical order
    /// and print each file's contents to stdout, stripping the CRI
    /// line prefix so the output reads like the original sh
    /// stdout/stderr.
    fn dump(&self) -> std::io::Result<()> {
        let jobs_dir = self.path().join("jobs");
        let mut jobs: Vec<_> = fs_err::read_dir(&jobs_dir)?
            .filter_map(Result::ok)
            .collect();
        jobs.sort_by_key(|e| e.file_name());
        for job in jobs {
            let mut shes: Vec<_> = fs_err::read_dir(job.path())?
                .filter_map(Result::ok)
                .collect();
            shes.sort_by_key(|e| e.file_name());
            for sh in shes {
                println!(
                    "==> {}/{}",
                    job.file_name().to_string_lossy(),
                    sh.file_name().to_string_lossy(),
                );
                let contents = fs_err::read_to_string(sh.path())?;
                for line in contents.lines() {
                    // CRI: "<ts> <stream> <tag> <text>"
                    let stripped = line.splitn(4, ' ').nth(3).unwrap_or(line);
                    println!("{stripped}");
                }
            }
        }
        Ok(())
    }
}

impl Drop for DumpLogsOnDrop {
    fn drop(&mut self) {
        // Field drops run after this body, so `self.dir` cleans up
        // the directory after we've finished reading from it.
        let _ = self.dump();
    }
}

/// Where the event stream is written. Resolved into a concrete
/// [`EventSink`] at run time.
#[derive(Clone, Debug)]
enum EventsTarget {
    Null,
    Stdout,
    File(PathBuf),
}

fn parse_events_target(s: &str) -> Result<EventsTarget, String> {
    match s {
        "null" => Ok(EventsTarget::Null),
        "stdout" => Ok(EventsTarget::Stdout),
        path => Ok(EventsTarget::File(PathBuf::from(path))),
    }
}

/// Transport for CI ↔ server communication.
///
/// CLI-shape only; the resolved variant (carrying the shared
/// [`ApiSession`]) is `TransportArgs`.
#[derive(Clone, Debug, PartialEq, Eq, clap::ValueEnum)]
pub enum Transport {
    Filesystem,
    Api,
}

/// Resolved transport produced by [`TransportFlags::resolve`]. The
/// `Api` variant carries the shared [`ApiSession`] — same shape the
/// server constructed when it created the run.
#[derive(Debug)]
#[allow(dead_code)] // session read by the upcoming API client
enum TransportArgs {
    Filesystem,
    Api(ApiSession),
}

fn main() -> miette::Result<()> {
    miette::set_panic_hook();
    let cli = Cli::parse();
    match cli.command {
        Commands::Validate => validate(cli.workspace),
        Commands::Run {
            events,
            out_dir,
            dispatch,
            transport,
        } => {
            let sink: Box<dyn EventSink> = match events {
                EventsTarget::Null => Box::new(NullSink),
                EventsTarget::Stdout => Box::new(JsonlSink::new(io::stdout())),
                EventsTarget::File(path) => {
                    let file = fs_err::File::create(&path).into_diagnostic()?;
                    Box::new(JsonlSink::new(io::BufWriter::new(file.into_parts().0)))
                }
            };
            let (log_dir, _dump) = match out_dir {
                Some(path) => {
                    fs_err::create_dir_all(&path).into_diagnostic()?;
                    (path, None)
                }
                None => {
                    let dir = tempfile::tempdir().into_diagnostic()?;
                    let path = dir.path().to_path_buf();
                    (path, Some(DumpLogsOnDrop { dir }))
                }
            };
            let auth_token = std::env::var("QUIRE_CI_TOKEN").ok();
            let transport = transport.resolve(auth_token)?;
            let (git_dir, meta, secrets, sentry_handoff) = match dispatch {
                Some(path) => load_dispatch(&path)?,
                None => (
                    cli.workspace.join(".git"),
                    placeholder_meta(),
                    HashMap::new(),
                    None,
                ),
            };

            // Sentry's reqwest transport spawns Tokio tasks for HTTP
            // sends, so the client must be constructed and dropped from
            // within a runtime context. A single worker thread is
            // enough — the main thread does the synchronous pipeline
            // work and only crosses into Tokio when sentry flushes.
            let rt = tokio::runtime::Builder::new_multi_thread()
                .worker_threads(1)
                .enable_all()
                .build()
                .into_diagnostic()?;
            let _enter = rt.enter();

            // Drop order: `_sentry` flushes first (still inside the
            // runtime), then `_enter`, then `rt`.
            let _sentry = init_sentry(sentry_handoff.as_ref(), &meta);
            // No type registrations: quire-ci's user-level errors
            // (CompileError, JobError, FennelError) are no longer logged
            // at tracing::error, so the miette renderer would never fire
            // for them. The layer stays installed in case future ops
            // errors want to register types.
            let miette_layer = MietteLayer::new();
            telemetry::init_tracing(miette_layer, FmtMode::Plain)?;

            run_pipeline(
                cli.workspace,
                sink,
                log_dir,
                git_dir,
                meta,
                secrets,
                transport,
            )
        }
    }
}

/// Initialize Sentry when the orchestrator passed a handoff. Tags
/// the scope with `service=quire-ci` plus the run's sha and ref so
/// events from this binary are distinguishable from quire-server's
/// in the same project, and attaches the orchestrator's trace id so
/// the two sides' events group on the same trace. A malformed
/// trace_id (shouldn't happen — the orchestrator emits the canonical
/// hex form) is logged and skipped rather than aborting Sentry init.
fn init_sentry(
    handoff: Option<&quire_core::ci::dispatch::SentryHandoff>,
    meta: &RunMeta,
) -> Option<sentry::ClientInitGuard> {
    let handoff = handoff?;
    let guard = sentry::init((
        handoff.dsn.as_str(),
        telemetry::sentry_client_options(VERSION),
    ));
    sentry::configure_scope(|scope| {
        scope.set_tag("service", "quire-ci");
        scope.set_tag("sha", &meta.sha);
        scope.set_tag("ref", &meta.r#ref);
        match handoff.trace_id.parse::<sentry::protocol::TraceId>() {
            Ok(trace_id) => {
                scope.set_context(
                    "trace",
                    sentry::protocol::Context::Trace(Box::new(sentry::protocol::TraceContext {
                        trace_id,
                        span_id: sentry::protocol::SpanId::default(),
                        op: Some("quire.ci.run".into()),
                        ..Default::default()
                    })),
                );
            }
            Err(e) => {
                tracing::warn!(
                    trace_id = %handoff.trace_id,
                    error = %e,
                    "malformed trace_id in dispatch; quire-ci events won't link to orchestrator",
                );
            }
        }
    });
    Some(guard)
}

fn validate(workspace: PathBuf) -> miette::Result<()> {
    let pipeline = compile_at(&workspace)?;

    if pipeline.job_count() == 0 {
        println!("No jobs registered.");
        return Ok(());
    }

    if let Some(image) = pipeline.image() {
        println!("Image: {image}");
    }

    println!("Jobs (topological order):");
    for job in pipeline.jobs() {
        let inputs = job.inputs.join(", ");
        println!("  {} <- [{inputs}]", job.id);
    }

    println!("\nAll validations passed.");
    Ok(())
}

/// Standalone runs synthesize a placeholder `quire/push`. Real meta
/// arrives via `--dispatch` from the orchestrator.
fn placeholder_meta() -> RunMeta {
    RunMeta {
        sha: "0".repeat(40),
        r#ref: "HEAD".to_string(),
        pushed_at: jiff::Timestamp::now(),
    }
}

/// Read and parse the dispatch file the orchestrator wrote before
/// spawning. Wraps revealed secret values back into `SecretString`.
///
/// Unlinks the file as soon as the bytes are in memory — secrets only
/// need to live on disk for the moment between `write_dispatch` and
/// this read, and getting them off disk early limits the blast radius
/// of a later panic or crash leaving a 0600 file behind.
///
/// The Sentry handoff, when present, carries the DSN and the
/// orchestrator's trace id — the 0600 dispatch file is the line of
/// defense for both.
#[allow(clippy::type_complexity)]
fn load_dispatch(
    path: &std::path::Path,
) -> miette::Result<(
    PathBuf,
    RunMeta,
    HashMap<String, quire_core::secret::SecretString>,
    Option<quire_core::ci::dispatch::SentryHandoff>,
)> {
    use quire_core::ci::dispatch::Dispatch;
    use quire_core::secret::SecretString;

    let bytes = fs_err::read(path).into_diagnostic()?;
    if let Err(e) = fs_err::remove_file(path) {
        // Don't abort — the bytes are already loaded and the server
        // will best-effort unlink after we exit. But this is a
        // security-relevant cleanup, so it's worth surfacing.
        eprintln!(
            "warning: failed to remove dispatch file {}: {e}",
            path.display()
        );
    }
    let dispatch: Dispatch = serde_json::from_slice(&bytes).into_diagnostic()?;
    let secrets = dispatch
        .secrets
        .into_iter()
        .map(|(name, value)| (name, SecretString::from(value)))
        .collect();
    Ok((dispatch.git_dir, dispatch.meta, secrets, dispatch.sentry))
}

fn run_pipeline(
    workspace: PathBuf,
    sink: Box<dyn EventSink>,
    log_dir: PathBuf,
    git_dir: PathBuf,
    meta: RunMeta,
    secrets: HashMap<String, quire_core::secret::SecretString>,
    _transport: TransportArgs,
) -> miette::Result<()> {
    let pipeline = compile_at(&workspace)?;

    let job_ids: Vec<String> = pipeline.jobs().iter().map(|j| j.id.clone()).collect();
    if job_ids.is_empty() {
        return Ok(());
    }

    // Keep the source around so a Lua failure inside a run-fn can be
    // wrapped via `FennelError::from_lua` — same source-code-annotated
    // diagnostic that compile-time errors get.
    let source = pipeline.source().to_string();
    let source_name = pipeline.source_name().to_string();

    let sink: Rc<RefCell<Box<dyn EventSink>>> = Rc::new(RefCell::new(sink));

    let runtime = Rc::new(Runtime::new(
        pipeline, secrets, &meta, &git_dir, workspace, log_dir,
    ));

    // Active job pointer, shared between the main loop and the
    // runtime callback. The callback translates RuntimeEvent into
    // wire events; consumers pair ShStarted/ShFinished by job_id +
    // sequence to assemble a per-sh DB row.
    let current_job: Rc<RefCell<Option<String>>> = Rc::new(RefCell::new(None));

    {
        let cb_sink = sink.clone();
        let cb_current_job = current_job.clone();
        runtime.set_event_callback(Box::new(move |event| {
            let job_id = cb_current_job
                .borrow()
                .clone()
                .expect("runtime fires sh events only inside enter_job/leave_job");
            let kind = match event {
                RuntimeEvent::ShStarted { cmd } => EventKind::ShStarted {
                    job_id,
                    cmd: cmd.to_string(),
                },
                RuntimeEvent::ShFinished { exit } => EventKind::ShFinished {
                    job_id,
                    exit_code: exit,
                },
            };
            let wire = Event {
                at_ms: jiff::Timestamp::now().as_millisecond(),
                kind,
            };
            cb_sink.borrow_mut().emit(wire).expect("emit sh event");
        }));
    }

    // Install the runtime on the Lua VM for the duration of this
    // function. Dropping `_runtime_guard` at end of scope tears the
    // install down — including on the early `Err(err.into())` return
    // below.
    let _runtime_guard =
        RuntimeHandle::install(runtime.clone(), runtime.lua()).expect("install runtime on Lua VM");

    let mut failed_job: Option<(String, JobError)> = None;
    for job_id in &job_ids {
        *current_job.borrow_mut() = Some(job_id.clone());

        sink.borrow_mut()
            .emit(Event {
                at_ms: jiff::Timestamp::now().as_millisecond(),
                kind: EventKind::JobStarted {
                    job_id: job_id.clone(),
                },
            })
            .expect("emit job_started");

        let run_fn = runtime
            .job(job_id)
            .expect("pipeline.jobs() returns valid ids")
            .run_fn
            .clone();

        runtime.enter_job(job_id);
        let rt =
            RuntimeHandle::runtime_table(runtime.lua()).expect("runtime table should be installed");
        let result: Result<(), JobError> = match run_fn {
            RunFn::Lua(f) => f.call::<mlua::Value>(rt).map(|_| ()).map_err(|lua_err| {
                JobError::Fennel(Box::new(FennelError::from_lua(
                    &source,
                    &source_name,
                    lua_err,
                )))
            }),
            RunFn::Rust(f) => f(&runtime).map_err(JobError::from),
        };
        runtime.leave_job();

        let outcome = if result.is_ok() {
            JobOutcome::Complete
        } else {
            JobOutcome::Failed
        };
        sink.borrow_mut()
            .emit(Event {
                at_ms: jiff::Timestamp::now().as_millisecond(),
                kind: EventKind::JobFinished {
                    job_id: job_id.clone(),
                    outcome,
                },
            })
            .expect("emit job_finished");

        *current_job.borrow_mut() = None;

        if let Err(e) = result {
            failed_job = Some((job_id.clone(), e));
            break;
        }
    }

    let run_outcome = if let Some((job_id, err)) = &failed_job {
        // Log at warn so it appears in stderr (and the run's log viewed
        // in the UI) without tripping sentry-tracing's ERROR → Event
        // mapping. Job failures are user-pipeline issues, not ops.
        tracing::warn!(job = %job_id, error = err as &(dyn std::error::Error + 'static), "job run-fn failed");
        RunOutcome::PipelineFailure
    } else {
        RunOutcome::Success
    };

    sink.borrow_mut()
        .emit(Event {
            at_ms: jiff::Timestamp::now().as_millisecond(),
            kind: EventKind::RunFinished {
                outcome: run_outcome,
            },
        })
        .expect("emit run_finished");

    Ok(())
}

/// Read and compile the ci.fnl at `<workspace>/.quire/ci.fnl`.
fn compile_at(workspace: &std::path::Path) -> miette::Result<Pipeline> {
    let path = workspace.join(".quire").join("ci.fnl");
    let source = fs_err::read_to_string(&path).into_diagnostic()?;
    Ok(pipeline::compile(&source, &path.display().to_string())?)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_events_target_classifies_input() {
        assert!(matches!(
            parse_events_target("null"),
            Ok(EventsTarget::Null)
        ));
        assert!(matches!(
            parse_events_target("stdout"),
            Ok(EventsTarget::Stdout)
        ));
        let Ok(EventsTarget::File(path)) = parse_events_target("/tmp/run.jsonl") else {
            panic!("expected File target");
        };
        assert_eq!(path, PathBuf::from("/tmp/run.jsonl"));
    }

    #[test]
    fn load_dispatch_unlinks_after_read() {
        use quire_core::ci::dispatch::Dispatch;

        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("dispatch.json");
        let dispatch = Dispatch {
            meta: RunMeta {
                sha: "0".repeat(40),
                r#ref: "HEAD".to_string(),
                pushed_at: jiff::Timestamp::now(),
            },
            git_dir: PathBuf::from("/tmp/repo.git"),
            secrets: HashMap::from([("token".to_string(), "shh".to_string())]),
            sentry: None,
        };
        fs_err::write(&path, serde_json::to_vec(&dispatch).unwrap()).expect("write");

        let (git_dir, meta, secrets, sentry) = load_dispatch(&path).expect("load");
        assert!(!path.exists(), "dispatch file should be removed after read");
        assert_eq!(git_dir, PathBuf::from("/tmp/repo.git"));
        assert_eq!(meta.r#ref, "HEAD");
        assert_eq!(secrets.len(), 1);
        assert!(sentry.is_none());
    }
}
