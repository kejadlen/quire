mod commands;

use clap::{CommandFactory, Parser, Subcommand};
use clap_complete::Shell;
use miette::IntoDiagnostic;
use miette::Result;
use tracing_subscriber::EnvFilter;
use tracing_subscriber::fmt;
use tracing_subscriber::prelude::*;

const VERSION: &str = env!("QUIRE_VERSION");

#[derive(Parser)]
#[command(name = "quire", version = VERSION, about = "A personal source forge")]
struct Cli {
    /// Output JSON instead of human-readable text.
    #[arg(long, global = true)]
    json: bool,

    /// Generate shell completions and exit.
    #[arg(long, value_enum)]
    completions: Option<Shell>,

    #[command(subcommand)]
    command: Option<Commands>,
}

#[derive(Subcommand)]
enum Commands {
    /// Start the HTTP server.
    Serve,

    /// Dispatch an SSH-originated command.
    Exec {
        /// The original SSH command string (e.g. git-receive-pack '/foo.git').
        /// Pass as a single argument: quire exec "git-receive-pack '/foo.git'"
        command: Vec<String>,
    },
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::registry()
        .with(fmt::layer())
        .with(EnvFilter::from_default_env())
        .init();

    let cli = Cli::parse();

    if let Some(shell) = cli.completions {
        clap_complete::generate(shell, &mut Cli::command(), "quire", &mut std::io::stdout());
        return Ok(());
    }

    let Some(command) = cli.command else {
        Cli::command().print_help().into_diagnostic()?;
        return Ok(());
    };

    match command {
        Commands::Serve => commands::serve::run().await?,
        Commands::Exec { command } => commands::exec::run(command).await?,
    }

    Ok(())
}
