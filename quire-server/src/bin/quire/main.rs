mod commands;
mod server;

use clap::{CommandFactory, Parser, Subcommand};
use clap_complete::Shell;
use miette::IntoDiagnostic;
use miette::Result;
use quire::Quire;
use quire_core::telemetry::{self, FmtMode, MietteLayer};
use sentry::ClientInitGuard;

const VERSION: &str = env!("QUIRE_VERSION");

#[derive(Parser)]
#[command(name = "quire", version = VERSION, about = "A personal source forge")]
struct Cli {
    /// Output JSON instead of human-readable text.
    #[arg(long, global = true)]
    json: bool,

    /// Root directory for quire data (default: /var/quire).
    #[arg(long, global = true, env = "QUIRE_BASE_DIR")]
    base_dir: Option<String>,

    /// Generate shell completions and exit.
    #[arg(long, value_enum)]
    completions: Option<Shell>,

    #[command(subcommand)]
    command: Option<Commands>,
}

#[derive(Subcommand)]
enum Commands {
    /// Start the HTTP server.
    Serve {
        /// Seed the database with dev data before starting.
        #[cfg(feature = "dev")]
        #[arg(long)]
        seed: bool,
    },

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

    /// CI pipeline operations.
    Ci {
        #[command(subcommand)]
        command: CiCommands,
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

#[derive(Subcommand)]
enum CiCommands {
    /// Validate a repo's ci.fnl without running any jobs.
    Validate {
        /// Commit SHA to validate. Defaults to HEAD.
        #[arg(short, long)]
        sha: Option<String>,
    },

    /// Execute a repo's ci.fnl locally for testing.
    Run {
        /// Commit SHA to run. Defaults to the working-copy revision.
        #[arg(short, long)]
        sha: Option<String>,
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
            tracing::warn!(
                error = &e as &(dyn std::error::Error + 'static),
                "failed to load global config, skipping Sentry init"
            );
            return None;
        }
    };

    let sentry_config = config.sentry.as_ref()?;
    let dsn = match sentry_config.dsn.reveal() {
        Ok(dsn) => dsn,
        Err(e) => {
            tracing::warn!(
                error = &e as &(dyn std::error::Error + 'static),
                "failed to resolve Sentry DSN, skipping Sentry init"
            );
            return None;
        }
    };

    Some(sentry::init((
        dsn,
        telemetry::sentry_client_options(VERSION),
    )))
}

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();

    let quire = match cli.base_dir {
        Some(ref dir) => Quire::new(dir.into()),
        None => Quire::default(),
    };
    let _sentry = init_sentry(&quire);
    let miette_layer = MietteLayer::new()
        .with_type::<quire::Error>()
        .with_type::<quire::ci::Error>()
        .with_type::<quire_core::fennel::FennelError>();
    telemetry::init_tracing(miette_layer, FmtMode::AutoJson)?;

    if let Some(shell) = cli.completions {
        clap_complete::generate(shell, &mut Cli::command(), "quire", &mut std::io::stdout());
        return Ok(());
    }

    let Some(command) = cli.command else {
        Cli::command().print_help().into_diagnostic()?;
        return Ok(());
    };

    match command {
        Commands::Serve {
            #[cfg(feature = "dev")]
            seed,
        } => {
            #[cfg(feature = "dev")]
            let quire = if seed { commands::dev::seed()? } else { quire };
            #[cfg(not(feature = "dev"))]
            let ci_routes = quire::quire::web::router(quire.clone()).layer(
                axum::middleware::from_fn(quire::quire::web::auth::require_auth),
            );
            #[cfg(feature = "dev")]
            let ci_routes = if seed {
                quire::quire::web::router(quire.clone())
            } else {
                quire::quire::web::router(quire.clone()).layer(axum::middleware::from_fn(
                    quire::quire::web::auth::require_auth,
                ))
            };
            commands::serve::run(&quire, ci_routes).await?
        }
        Commands::Exec { command } => commands::exec::run(&quire, command).await?,
        Commands::Hook { hook_name } => commands::hook::run(&quire, hook_name).await?,
        Commands::Repo { command } => match command {
            RepoCommands::New { name } => commands::repo::new(&quire, &name).await?,
            RepoCommands::List => commands::repo::list(&quire).await?,
            RepoCommands::Rm { name } => commands::repo::rm(&quire, &name).await?,
        },
        Commands::Ci { command } => match command {
            CiCommands::Validate { sha } => commands::ci::validate(sha.as_deref()).await?,
            CiCommands::Run { sha } => commands::ci::run(&quire, sha.as_deref()).await?,
        },
    }

    Ok(())
}
