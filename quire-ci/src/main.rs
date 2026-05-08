use std::path::PathBuf;
use std::process::ExitCode;

use miette::IntoDiagnostic;
use quire_core::ci::pipeline::CompileError;

fn main() -> ExitCode {
    miette::set_panic_hook();
    match run() {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("{e:?}");
            ExitCode::FAILURE
        }
    }
}

fn run() -> miette::Result<()> {
    let path = find_ci_fnl()?;

    let source = fs_err::read_to_string(&path).into_diagnostic()?;

    let pipeline = match quire_core::ci::pipeline::compile(&source, &path.display().to_string()) {
        Ok(p) => p,
        Err(CompileError::Fennel(err)) => {
            return Err(miette::Report::new_boxed(err));
        }
        Err(CompileError::Pipeline(err)) => {
            return Err(miette::Report::new_boxed(err));
        }
    };

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

/// Walk from cwd upward to find `.quire/ci.fnl`.
fn find_ci_fnl() -> miette::Result<PathBuf> {
    let cwd = std::env::current_dir().into_diagnostic()?;
    let mut dir = cwd.as_path();
    loop {
        let candidate = dir.join(".quire").join("ci.fnl");
        if candidate.is_file() {
            return Ok(candidate);
        }
        dir = dir
            .parent()
            .ok_or_else(|| miette::miette!("no .quire/ci.fnl found in any parent directory"))?;
    }
}
