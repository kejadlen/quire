use std::collections::HashMap;
use std::path::PathBuf;
use std::rc::Rc;

use clap::Parser;
use miette::IntoDiagnostic;
use mlua::IntoLua;
use quire_core::ci::pipeline::{self, Pipeline, RunFn};
use quire_core::ci::run::RunMeta;
use quire_core::ci::runtime::{Runtime, RuntimeError, RuntimeHandle, ShOutput};

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
    Run,
}

fn main() -> miette::Result<()> {
    miette::set_panic_hook();
    let cli = Cli::parse();
    match cli.command {
        Commands::Validate => validate(cli.workspace),
        Commands::Run => run_pipeline(cli.workspace),
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

fn run_pipeline(workspace: PathBuf) -> miette::Result<()> {
    let pipeline = compile_at(&workspace)?;

    let job_ids: Vec<String> = pipeline
        .topo_order()
        .into_iter()
        .map(|s| s.to_string())
        .collect();
    if job_ids.is_empty() {
        println!("No jobs registered.");
        return Ok(());
    }

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
    ));

    // Install the runtime handle on the Lua VM once for the whole run;
    // each job's run-fn receives `rt_value` as its sole argument.
    let lua = runtime.lua();
    let rt_value = RuntimeHandle(runtime.clone())
        .into_lua(lua)
        .expect("install runtime on Lua VM");

    let mut failed_job: Option<(String, RuntimeError)> = None;
    for job_id in &job_ids {
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

        if let Err(e) = result {
            failed_job = Some((job_id.clone(), e));
            break;
        }
    }

    lua.remove_app_data::<Rc<Runtime>>();

    let outputs = runtime.take_outputs();
    print_outputs(&job_ids, &outputs);

    if let Some((job, err)) = failed_job {
        eprintln!("\nJob '{job}' failed.");
        return Err(err.into());
    }

    let nonzero: Vec<&str> = job_ids
        .iter()
        .filter(|id| {
            outputs
                .get(id.as_str())
                .is_some_and(|os| os.iter().any(|o| o.exit != 0))
        })
        .map(String::as_str)
        .collect();
    if nonzero.is_empty() {
        println!("\nAll jobs passed.");
    } else {
        println!(
            "\n{} job(s) had non-zero `(sh ...)` exits: {}",
            nonzero.len(),
            nonzero.join(", ")
        );
    }
    Ok(())
}

/// Read and compile the ci.fnl at `<workspace>/.quire/ci.fnl`.
fn compile_at(workspace: &std::path::Path) -> miette::Result<Pipeline> {
    let path = workspace.join(".quire").join("ci.fnl");
    let source = fs_err::read_to_string(&path).into_diagnostic()?;
    Ok(pipeline::compile(&source, &path.display().to_string())?)
}

/// Print captured `(sh …)` outputs, grouped by job, in execution
/// order. Skips jobs with no recorded output.
fn print_outputs(job_ids: &[String], outputs: &HashMap<String, Vec<ShOutput>>) {
    for job_id in job_ids {
        let Some(job_outputs) = outputs.get(job_id) else {
            continue;
        };
        if job_outputs.is_empty() {
            continue;
        }
        println!("==> {job_id}");
        for o in job_outputs {
            if !o.stdout.is_empty() {
                print!("{}", o.stdout);
            }
            if !o.stderr.is_empty() {
                eprint!("{}", o.stderr);
            }
            if o.exit != 0 {
                eprintln!("(exit {})", o.exit);
            }
        }
    }
}
