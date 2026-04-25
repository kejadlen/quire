mod commands;

use clap::{CommandFactory, Parser, Subcommand};
use clap_complete::Shell;
use miette::IntoDiagnostic;
use miette::Result;
use quire::Config;
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

    /// Invoked by git hooks configured via hook.<name>.command.
    Hook {
        /// The hook name (e.g. post-receive).
        hook_name: crate::commands::hook::HookName,
    },

    /// Manage repositories.
    Repo {
        #[command(subcommand)]
        command: RepoCommands,
    },
}

#[derive(Subcommand)]
enum RepoCommands {
    /// Create a new bare repository.
    New {
        /// Repository name (e.g. foo.git or work/foo.git).
        name: String,
    },

    /// List all repositories.
    List,

    /// Delete a repository.
    Rm {
        /// Repository name (e.g. foo.git or work/foo.git).
        name: String,
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

    let config = Config::default();

    match command {
        Commands::Serve => commands::serve::run(&config).await?,
        Commands::Exec { command } => commands::exec::run(&config, command).await?,
        Commands::Hook { hook_name } => commands::hook::run(hook_name).await?,
        Commands::Repo { command } => match command {
            RepoCommands::New { name } => commands::repo::new::run(&config, &name).await?,
            RepoCommands::List => commands::repo::list::run(&config).await?,
            RepoCommands::Rm { name } => commands::repo::rm::run(&config, &name).await?,
        },
    }

    Ok(())
}
