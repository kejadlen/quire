mod sink;

use std::cell::RefCell;
use std::io;
use std::path::PathBuf;
use std::rc::Rc;

use facet::Facet;
use figue::{self as args, Driver, FigueBuiltins};
use miette::{IntoDiagnostic, Result, bail};
use quire_core::api::SecretResponse;
use quire_core::ci::bootstrap::Bootstrap;
use quire_core::ci::event::{Event, EventKind, JobOutcome, RunOutcome};
use quire_core::ci::pipeline::{self, Pipeline, RunFn};
use quire_core::ci::run::ApiSession;
use quire_core::ci::run::RunMeta;
use quire_core::ci::runtime::{Runtime, RuntimeError, RuntimeEvent, RuntimeHandle};
use quire_core::fennel::FennelError;
use quire_core::secret::{Error as SecretError, Result as SecretResult, SecretRegistry};
use quire_core::telemetry::{self, FmtMode, MietteLayer};
use tracing_opentelemetry::OpenTelemetrySpanExt as _;
use reqwest::header::{AUTHORIZATION, HeaderMap, HeaderValue};
use std::sync::Arc;

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

/// Run and validate quire CI pipelines.
#[derive(Facet)]
struct Cli {
    /// Workspace root containing .quire/ci.fnl. Defaults to cwd.
    #[facet(args::named, args::short = 'w', default = ".")]
    workspace: PathBuf,

    /// Transport credentials and telemetry settings for
    /// orchestrator-dispatched runs, sourced from `QUIRE__*` env vars:
    /// `QUIRE__SERVER_URL`, `QUIRE__RUN_TOKEN`, `QUIRE__SENTRY_DSN`.
    #[facet(args::config, args::env_prefix = "QUIRE")]
    quire: QuireConfig,

    #[facet(flatten)]
    builtins: FigueBuiltins,

    #[facet(args::subcommand)]
    command: Commands,
}

/// Transport credentials and telemetry settings sourced from `QUIRE__*`
/// environment variables. Fields can also be overridden via
/// `--quire.<field>` on the CLI.
#[derive(Facet)]
struct QuireConfig {
    /// Base URL of quire-server, e.g. `http://127.0.0.1:3000`
    /// (`QUIRE__SERVER_URL`).
    #[facet(default)]
    server_url: String,

    /// Bearer token minted at run creation time (`QUIRE__RUN_TOKEN`).
    #[facet(sensitive, default)]
    run_token: String,

    /// Sentry DSN for error reporting (`QUIRE__SENTRY_DSN`).
    #[facet(default)]
    sentry_dsn: Option<String>,
}

#[derive(Facet)]
#[repr(u8)]
enum Commands {
    /// Compile and validate a ci.fnl pipeline.
    Validate,

    /// Execute a pipeline dispatched by the orchestrator.
    ///
    /// Transport credentials and telemetry settings are supplied via
    /// `QUIRE__*` environment variables (see top-level `--quire.*`
    /// options).
    Run {
        /// Where to send the structured event stream. Accepts:
        ///   `null`   — drop events (default).
        ///   `stdout` — write JSONL to stdout.
        ///   `<path>` — write JSONL to this file. The orchestrator
        ///              reads the file post-run to populate `jobs`
        ///              and `sh_events` database rows.
        #[facet(args::named, default = "null")]
        events: String,

        /// Directory for per-sh CRI log files. Defaults to a fresh
        /// tempdir whose path is printed on stdout at the end of the
        /// run.
        #[facet(args::named, default)]
        out_dir: Option<PathBuf>,

        /// Run in local mode. Derives commit SHA and ref from `--git-dir`
        /// instead of fetching bootstrap data from the server. Pass
        /// `QUIRE__SERVER_URL` to use the server API instead.
        #[facet(args::named, default)]
        local: bool,

        /// Path to the bare git repo for this run. Required when
        /// `--local` is set; server-dispatched runs receive this via the
        /// bootstrap API instead.
        #[facet(args::named, default)]
        git_dir: Option<PathBuf>,
    },
}

/// RAII wrapper around a tempdir holding captured sh logs. On drop,
/// prints each log file's contents to stdout, then lets the underlying
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

impl std::str::FromStr for EventsTarget {
    type Err = std::convert::Infallible;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Ok(match s {
            "null" => EventsTarget::Null,
            "stdout" => EventsTarget::Stdout,
            path => EventsTarget::File(PathBuf::from(path)),
        })
    }
}

/// HTTP client for quire-server's CI API.
///
/// Wraps `reqwest::blocking::Client` with the `Authorization` header
/// pre-configured and the per-run base URL baked in. Cloning is cheap
/// (the underlying connection pool is reference-counted).
struct RunClient {
    client: reqwest::blocking::Client,
    base_url: String,
}

impl RunClient {
    fn new(session: ApiSession) -> Self {
        let mut headers = HeaderMap::new();
        let mut auth = HeaderValue::from_str(&format!("Bearer {}", session.run_token))
            .expect("auth token contains only ASCII");
        auth.set_sensitive(true);
        headers.insert(AUTHORIZATION, auth);
        Self {
            client: reqwest::blocking::Client::builder()
                .default_headers(headers)
                .build()
                .expect("failed to build HTTP client"),
            base_url: format!("{}/api/run", session.server_url),
        }
    }

    /// Send an authenticated GET to `{base_url}/{path}` and return the response.
    ///
    /// Spawns a dedicated OS thread so `reqwest::blocking` works regardless of
    /// whether the calling thread holds a Tokio runtime guard (which it does
    /// during pipeline execution due to Sentry's runtime requirement).
    fn get(&self, path: &str) -> reqwest::Result<reqwest::blocking::Response> {
        let url = format!("{}/{}", self.base_url, path);
        let client = self.client.clone();
        std::thread::spawn(move || client.get(&url).send())
            .join()
            .expect("HTTP thread panicked")
    }

    /// Fetch the bootstrap payload from the server API.
    ///
    /// One-shot: the server marks the bootstrap as fetched after the first
    /// successful call and returns 410 on any subsequent call.
    fn fetch_bootstrap(&self) -> Result<(PathBuf, RunMeta, TelemetryContext)> {
        let bootstrap: Bootstrap =
            (|| -> reqwest::Result<_> { self.get("bootstrap")?.error_for_status()?.json() })()
                .into_diagnostic()?;
        Ok((
            bootstrap.git_dir,
            bootstrap.meta,
            TelemetryContext {
                traceparent: bootstrap.traceparent,
                repo: Some(bootstrap.repo),
                run_id: Some(bootstrap.run_id),
            },
        ))
    }

    /// Fetch a single secret by name from the server.
    fn fetch_secret(&self, name: &str) -> SecretResult<String> {
        let resp = self
            .get(&format!("secrets/{name}"))
            .map_err(|e| SecretError::Resolve(Arc::new(e)))?;
        match resp.status() {
            s if s.is_success() => resp
                .json::<SecretResponse>()
                .map(|r| r.value)
                .map_err(|e| SecretError::Resolve(Arc::new(e))),
            reqwest::StatusCode::NOT_FOUND => Err(SecretError::UnknownSecret(name.to_string())),
            _ => Err(SecretError::Resolve(Arc::new(
                resp.error_for_status().unwrap_err(),
            ))),
        }
    }
}

fn main() -> Result<()> {
    miette::set_panic_hook();

    let config = figue::builder::<Cli>()
        .into_diagnostic()?
        .cli(|cli| cli.args(std::env::args().skip(1)))
        .env(|env| env)
        .help(|h| h.program_name("quire-ci").version(VERSION))
        .build();

    let cli: Cli = Driver::new(config).run().unwrap();
    let workspace = cli.workspace;

    match cli.command {
        Commands::Validate => validate(workspace),
        Commands::Run {
            events,
            out_dir,
            local,
            git_dir,
        } => {
            let sink: Box<dyn EventSink> = match events.parse::<EventsTarget>().unwrap() {
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

            let session = ApiSession {
                server_url: cli.quire.server_url,
                run_token: cli.quire.run_token,
            };
            let client = RunClient::new(session.clone());

            let (git_dir, meta, sentry_ctx) = if local {
                let Some(git_dir) = git_dir else {
                    bail!("--git-dir is required for local runs");
                };
                let sha = git_rev_parse(&git_dir, "HEAD")?;
                let git_ref = git_symbolic_ref(&git_dir).unwrap_or_else(|_| "@".to_string());
                let meta = RunMeta {
                    sha,
                    r#ref: git_ref,
                    pushed_at: jiff::Timestamp::now(),
                };
                (git_dir, meta, TelemetryContext::default())
            } else {
                client.fetch_bootstrap()?
            };

            // Drop order: `_sentry` flushes first (still inside the
            // runtime), then `_enter`, then `rt`.
            let _sentry = cli
                .quire
                .sentry_dsn
                .as_deref()
                .map(|dsn| init_sentry(dsn, &meta, &sentry_ctx));

            // No type registrations: quire-ci's user-level errors
            // (CompileError, JobError, FennelError) are no longer logged
            // at tracing::error, so the miette renderer would never fire
            // for them. The layer stays installed in case future ops
            // errors want to register types.
            let miette_layer = MietteLayer::new();
            // _tracing_guard must be declared AFTER _sentry so it drops
            // BEFORE _sentry — OTEL provider flushes spans to Sentry SDK
            // before the Sentry client flushes to the server.
            let _tracing_guard = telemetry::init_tracing(miette_layer, FmtMode::Plain)?;

            let run_span =
                tracing::info_span!("quire.ci.run", sha = %meta.sha, r#ref = %meta.r#ref);
            if let Some(tp) = sentry_ctx.traceparent.as_deref() {
                run_span.set_parent(telemetry::context_from_traceparent(tp));
            }
            let _run_span = run_span.entered();

            let registry = SecretRegistry::new(move |name| client.fetch_secret(name));

            run_pipeline(workspace, sink, log_dir, git_dir, meta, registry)
        }
    }
}

/// Sentry-only run context from the bootstrap handoff. Every field is
/// present together (orchestrator-dispatched with a DSN configured) or
/// absent together (local run, which has no orchestrator). Kept apart
/// from [`RunMeta`]: `meta` carries push facts the pipeline needs,
/// these tag observability only.
#[derive(Default)]
struct TelemetryContext {
    traceparent: Option<String>,
    repo: Option<String>,
    run_id: Option<String>,
}

/// Initialize Sentry. Tags the scope with `service=quire-ci` plus the
/// run's sha and ref so events from this binary are distinguishable
/// from quire-server's in the same project. `repo`, `run_id`, and the
/// trace context come from the bootstrap handoff and are attached only
/// when present (absent for local runs); the trace id links both
/// sides' events onto the same trace.
fn init_sentry(dsn: &str, meta: &RunMeta, ctx: &TelemetryContext) -> sentry::ClientInitGuard {
    let guard = sentry::init((dsn, telemetry::sentry_client_options(VERSION)));
    sentry::configure_scope(|scope| {
        scope.set_tag("service", "quire-ci");
        scope.set_tag("sha", &meta.sha);
        scope.set_tag("ref", &meta.r#ref);
        if let Some(repo) = &ctx.repo {
            scope.set_tag("repo", repo);
        }
        if let Some(run_id) = &ctx.run_id {
            scope.set_tag("run_id", run_id);
        }
    });
    guard
}

fn git_rev_parse(git_dir: &std::path::Path, rev: &str) -> Result<String> {
    let out = std::process::Command::new("git")
        .arg("--git-dir")
        .arg(git_dir)
        .arg("rev-parse")
        .arg(rev)
        .output()
        .into_diagnostic()?;
    if !out.status.success() {
        let stderr = String::from_utf8_lossy(&out.stderr);
        bail!("git rev-parse {rev} failed: {stderr}");
    }
    Ok(String::from_utf8(out.stdout)
        .into_diagnostic()?
        .trim()
        .to_string())
}

fn git_symbolic_ref(git_dir: &std::path::Path) -> Result<String> {
    let out = std::process::Command::new("git")
        .arg("--git-dir")
        .arg(git_dir)
        .arg("symbolic-ref")
        .arg("HEAD")
        .output()
        .into_diagnostic()?;
    if !out.status.success() {
        bail!("HEAD is detached");
    }
    Ok(String::from_utf8(out.stdout)
        .into_diagnostic()?
        .trim()
        .to_string())
}

fn validate(workspace: PathBuf) -> Result<()> {
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

fn run_pipeline(
    workspace: PathBuf,
    mut sink: Box<dyn EventSink>,
    log_dir: PathBuf,
    git_dir: PathBuf,
    meta: RunMeta,
    registry: SecretRegistry,
) -> Result<()> {
    let pipeline = match compile_at(&workspace) {
        Ok(p) => p,
        Err(e) => {
            tracing::warn!(
                error = &*e as &(dyn std::error::Error + 'static),
                "ci.fnl failed to compile"
            );
            sink.emit(Event {
                at_ms: jiff::Timestamp::now().as_millisecond(),
                kind: EventKind::RunFinished {
                    outcome: RunOutcome::PipelineFailure,
                },
            })
            .expect("emit run_finished");
            return Ok(());
        }
    };

    let job_ids: Vec<String> = pipeline.jobs().iter().map(|j| j.id.clone()).collect();
    if job_ids.is_empty() {
        sink.emit(Event {
            at_ms: jiff::Timestamp::now().as_millisecond(),
            kind: EventKind::RunFinished {
                outcome: RunOutcome::Success,
            },
        })
        .expect("emit run_finished");
        return Ok(());
    }

    // Keep the source around so a Lua failure inside a run-fn can be
    // wrapped via `FennelError::from_lua` — same source-code-annotated
    // diagnostic that compile-time errors get.
    let source = pipeline.source().to_string();
    let source_name = pipeline.source_name().to_string();

    let sink: Rc<RefCell<Box<dyn EventSink>>> = Rc::new(RefCell::new(sink));

    let runtime = Rc::new(Runtime::new(
        pipeline, registry, &meta, &git_dir, workspace, log_dir,
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
fn compile_at(workspace: &std::path::Path) -> Result<Pipeline> {
    let path = workspace.join(".quire").join("ci.fnl");
    let source = fs_err::read_to_string(&path).into_diagnostic()?;
    Ok(pipeline::compile(&source, &path.display().to_string())?)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_events_target_classifies_input() {
        assert!(matches!("null".parse(), Ok(EventsTarget::Null)));
        assert!(matches!("stdout".parse(), Ok(EventsTarget::Stdout)));
        let Ok(EventsTarget::File(path)) = "/tmp/run.jsonl".parse::<EventsTarget>() else {
            panic!("expected File target");
        };
        assert_eq!(path, PathBuf::from("/tmp/run.jsonl"));
    }
}
