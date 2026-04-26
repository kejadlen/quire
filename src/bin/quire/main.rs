mod commands;

use clap::{CommandFactory, Parser, Subcommand};
use clap_complete::Shell;
use miette::IntoDiagnostic;
use miette::Result;
use quire::Quire;
use sentry::ClientInitGuard;
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

/// Initialize Sentry if the global config provides a DSN.
///
/// Returns the guard if initialized, or None if Sentry is not configured.
/// Logs a warning on failure but does not abort.
fn init_sentry(quire: &Quire) -> Option<ClientInitGuard> {
    let config = match quire.global_config() {
        Ok(config) => config,
        Err(e) => {
            tracing::warn!(%e, "failed to load global config, skipping Sentry init");
            return None;
        }
    };

    let sentry_config = config.sentry.as_ref()?;
    let dsn = match sentry_config.dsn.reveal() {
        Ok(dsn) => dsn,
        Err(e) => {
            tracing::warn!(%e, "failed to resolve Sentry DSN, skipping Sentry init");
            return None;
        }
    };

    let guard = sentry::init((
        dsn,
        sentry::ClientOptions {
            release: Some(VERSION.into()),
            ..Default::default()
        },
    ));

    Some(guard)
}

#[tokio::main]
async fn main() -> Result<()> {
    let quire = Quire::default();
    let _sentry = init_sentry(&quire);

    tracing_subscriber::registry()
        .with(sentry_tracing::layer())
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
        Commands::Serve => commands::serve::run(&quire).await?,
        Commands::Exec { command } => commands::exec::run(&quire, command).await?,
        Commands::Hook { hook_name } => commands::hook::run(&quire, hook_name).await?,
        Commands::Repo { command } => match command {
            RepoCommands::New { name } => commands::repo::new(&quire, &name).await?,
            RepoCommands::List => commands::repo::list(&quire).await?,
            RepoCommands::Rm { name } => commands::repo::rm(&quire, &name).await?,
        },
    }

    Ok(())
}
