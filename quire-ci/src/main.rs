use std::path::PathBuf;

use clap::Parser;
use miette::IntoDiagnostic;

/// Validate a quire CI pipeline.
#[derive(Parser)]
#[command(version, propagate_version = true)]
struct Cli {
    #[command(subcommand)]
    command: Commands,
}

#[derive(clap::Subcommand)]
enum Commands {
    /// Compile and validate a ci.fnl pipeline.
    Validate {
        /// Workspace root containing .quire/ci.fnl. Defaults to cwd.
        #[arg(short, long, default_value = ".")]
        workspace: PathBuf,
    },
}

fn main() -> miette::Result<()> {
    miette::set_panic_hook();
    run()
}

fn run() -> miette::Result<()> {
    let cli = Cli::parse();
    let Commands::Validate { workspace } = cli.command;

    let path = workspace.join(".quire").join("ci.fnl");
    let source = fs_err::read_to_string(&path).into_diagnostic()?;

    let pipeline = quire_core::ci::pipeline::compile(&source, &path.display().to_string())?;

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
