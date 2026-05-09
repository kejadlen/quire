mod event;
mod sink;

use std::cell::RefCell;
use std::collections::HashMap;
use std::io;
use std::path::PathBuf;
use std::rc::Rc;

use clap::Parser;
use miette::IntoDiagnostic;
use mlua::IntoLua;
use quire_core::ci::pipeline::{self, Pipeline, RunFn};
use quire_core::ci::run::RunMeta;
use quire_core::ci::runtime::{Runtime, RuntimeError, RuntimeEvent, RuntimeHandle};

use crate::event::Event;
use crate::sink::{EventSink, JsonlSink, NullSink};

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
    /// Synthesizes a placeholder `quire/push` and runs with no
    /// secrets — `(secret :name)` calls error, and `(jobs upstream)`
    /// reads return Nil for everything except `quire/push` (the
    /// runtime doesn't yet propagate run-fn outputs into downstream
    /// jobs' input views).
    Run {
        /// Where to send structured run events.
        #[arg(long, value_enum, default_value_t = EventsKind::Null)]
        events: EventsKind,

        /// Directory for per-sh CRI log files. Defaults to a fresh
        /// tempdir whose path is printed on stdout at the end of the
        /// run.
        #[arg(long)]
        out_dir: Option<PathBuf>,
    },
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

#[derive(Clone, Copy, Debug, PartialEq, Eq, clap::ValueEnum)]
#[value(rename_all = "lowercase")]
enum EventsKind {
    /// Drop events.
    Null,
    /// JSONL on stdout, one event per line.
    Stdout,
}

fn main() -> miette::Result<()> {
    miette::set_panic_hook();
    let cli = Cli::parse();
    match cli.command {
        Commands::Validate => validate(cli.workspace),
        Commands::Run { events, out_dir } => {
            let sink: Box<dyn EventSink> = match events {
                EventsKind::Null => Box::new(NullSink),
                EventsKind::Stdout => Box::new(JsonlSink::new(io::stdout())),
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
            run_pipeline(cli.workspace, sink, log_dir)
        }
    }
}

fn validate(workspace: PathBuf) -> miette::Result<()> {
    let pipeline = compile_at(&workspace)?;

    let jobs = pipeline.jobs();
    if jobs.is_empty() {
        println!("No jobs registered.");
        return Ok(());
    }

    if let Some(image) = pipeline.image() {
        println!("Image: {image}");
    }

    let topo = pipeline.topo_order();
    println!("Jobs (topological order):");
    for id in &topo {
        let job = pipeline.job(id).expect("topo_order returns valid ids");
        let inputs = job.inputs.join(", ");
        println!("  {id} <- [{inputs}]");
    }

    println!("\nAll validations passed.");
    Ok(())
}

fn run_pipeline(
    workspace: PathBuf,
    sink: Box<dyn EventSink>,
    log_dir: PathBuf,
) -> miette::Result<()> {
    let pipeline = compile_at(&workspace)?;

    let job_ids: Vec<String> = pipeline
        .topo_order()
        .into_iter()
        .map(|s| s.to_string())
        .collect();
    if job_ids.is_empty() {
        return Ok(());
    }

    let sink: Rc<RefCell<Box<dyn EventSink>>> = Rc::new(RefCell::new(sink));

    let meta = RunMeta {
        sha: "0".repeat(40),
        r#ref: "HEAD".to_string(),
        pushed_at: jiff::Timestamp::now(),
    };

    let git_dir = workspace.join(".git");
    let runtime = Rc::new(Runtime::new(
        pipeline,
        HashMap::new(),
        &meta,
        &git_dir,
        workspace,
        log_dir,
    ));

    // Active job pointer, shared between the main loop and the runtime
    // callback (which translates RuntimeEvent → wire Event).
    let current_job: Rc<RefCell<Option<String>>> = Rc::new(RefCell::new(None));

    {
        let cb_sink = sink.clone();
        let cb_current_job = current_job.clone();
        runtime.set_event_callback(Box::new(move |event| {
            let job_id = cb_current_job
                .borrow()
                .clone()
                .expect("runtime fires sh events only inside enter_job/leave_job");
            let wire_event = match event {
                RuntimeEvent::ShStarted { cmd } => Event::ShStarted {
                    job_id,
                    cmd: cmd.to_string(),
                },
                RuntimeEvent::ShFinished { exit } => Event::ShFinished {
                    job_id,
                    exit_code: exit,
                },
            };
            cb_sink.borrow_mut().emit(wire_event).expect("emit event");
        }));
    }

    // Install the runtime handle on the Lua VM once for the whole run;
    // each job's run-fn receives `rt_value` as its sole argument.
    let lua = runtime.lua();
    let rt_value = RuntimeHandle(runtime.clone())
        .into_lua(lua)
        .expect("install runtime on Lua VM");

    let mut failed_job: Option<(String, RuntimeError)> = None;
    for job_id in &job_ids {
        *current_job.borrow_mut() = Some(job_id.clone());

        sink.borrow_mut()
            .emit(Event::JobStarted {
                job_id: job_id.clone(),
            })
            .expect("emit job_started");

        let run_fn = runtime
            .job(job_id)
            .expect("topo_order returns valid ids")
            .run_fn
            .clone();

        runtime.enter_job(job_id);
        let result: Result<(), RuntimeError> = match run_fn {
            RunFn::Lua(f) => f
                .call::<mlua::Value>(rt_value.clone())
                .map(|_| ())
                .map_err(RuntimeError::from),
            RunFn::Rust(f) => f(&runtime),
        };
        runtime.leave_job();

        let finish_event = if result.is_ok() {
            Event::JobCompleted {
                job_id: job_id.clone(),
            }
        } else {
            Event::JobFailed {
                job_id: job_id.clone(),
            }
        };
        sink.borrow_mut()
            .emit(finish_event)
            .expect("emit job finish");

        *current_job.borrow_mut() = None;

        if let Err(e) = result {
            failed_job = Some((job_id.clone(), e));
            break;
        }
    }

    lua.remove_app_data::<Rc<Runtime>>();

    if let Some((_, err)) = failed_job {
        return Err(err.into());
    }

    Ok(())
}

/// Read and compile the ci.fnl at `<workspace>/.quire/ci.fnl`.
fn compile_at(workspace: &std::path::Path) -> miette::Result<Pipeline> {
    let path = workspace.join(".quire").join("ci.fnl");
    let source = fs_err::read_to_string(&path).into_diagnostic()?;
    Ok(pipeline::compile(&source, &path.display().to_string())?)
}
