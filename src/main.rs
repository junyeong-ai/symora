use clap::Parser;
use tracing_subscriber::{layer::SubscriberExt, util::SubscriberInitExt};

use symora::app::App;
#[cfg(unix)]
use symora::cli::commands::daemon::{DaemonArgs, DaemonCommand};
use symora::cli::commands::mcp::{McpArgs, McpCommand};
use symora::cli::{Cli, Commands, OutputOptions};
use symora::infra::lsp::watch::FileWatch;

// jemalloc keeps fragmentation flat under the SQLite batch + LSP fan-out
// workload that dominates Symora. The cfg gate matches the optional
// dependency in Cargo.toml so Windows / msvc builds fall back to the
// system allocator without losing functionality.
#[cfg(all(not(target_env = "msvc"), not(target_os = "windows")))]
#[global_allocator]
static ALLOC: tikv_jemallocator::Jemalloc = tikv_jemallocator::Jemalloc;

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let verbose = args.iter().any(|a| a == "-v" || a == "--verbose");

    let env_filter = if verbose {
        tracing_subscriber::EnvFilter::try_from_default_env()
            .unwrap_or_else(|_| "symora=debug".into())
    } else {
        tracing_subscriber::EnvFilter::try_from_default_env()
            .unwrap_or_else(|_| "symora=warn".into())
    };

    // Logs go to stderr, always: stdout is the machine-readable JSON
    // surface, and a single warning line interleaved there corrupts the
    // stream every agent parses.
    tracing_subscriber::registry()
        .with(env_filter)
        .with(
            tracing_subscriber::fmt::layer()
                .with_target(verbose)
                .with_writer(std::io::stderr)
                .compact(),
        )
        .init();

    let runtime = match tokio::runtime::Runtime::new() {
        Ok(rt) => rt,
        Err(e) => {
            eprintln!(r#"{{"error":"Failed to create runtime: {}"}}"#, e);
            std::process::exit(1);
        }
    };

    if let Err(e) = runtime.block_on(async_main()) {
        refuse(e.into());
    }
}

/// Report a failure that stopped the run before a command could answer.
///
/// The envelope goes to stdout because that is where every other failure this
/// tool reports goes: a caller parsing one stream finds an unparseable empty
/// response otherwise, which is the one shape it cannot act on. Logs stay on
/// stderr.
fn refuse(err: symora::cli::OutputError) -> ! {
    println!("{}", serde_json::json!({ "error": err }));
    std::process::exit(2);
}

/// Parse the command line, reporting a usage error the way every other
/// failure is reported.
///
/// Clap's own rendering is prose on stderr with an empty stdout, which is the
/// one failure an agent parsing this tool cannot read. `--help` and
/// `--version` are not failures and keep clap's rendering.
fn parse_cli() -> anyhow::Result<Cli> {
    use clap::error::ErrorKind;

    match Cli::try_parse() {
        Ok(cli) => Ok(cli),
        Err(e) if matches!(e.kind(), ErrorKind::DisplayHelp | ErrorKind::DisplayVersion) => {
            let _ = e.print();
            std::process::exit(0);
        }
        Err(e) => {
            let message = e
                .to_string()
                .lines()
                .next()
                .unwrap_or("invalid arguments")
                .trim_start_matches("error: ")
                .to_string();
            refuse(
                symora::cli::OutputError::invalid(message)
                    .with_hint("Run the command with --help for its accepted arguments."),
            )
        }
    }
}

async fn async_main() -> anyhow::Result<()> {
    let mut cli = parse_cli()?;

    let output_options = OutputOptions {
        format: cli.format,
        quiet: cli.quiet,
    };

    if let Some(requirement) = cli.check_version.as_deref()
        && let Err(e) = symora::cli::check_version(requirement)
    {
        refuse(e);
    }

    if let Some(input) = read_edit_stdin(&mut cli.command)? {
        *edit_payload(&mut cli.command).unwrap() = input;
    }

    #[cfg(unix)]
    let use_daemon = std::env::var(symora::constants::env::NO_DAEMON)
        .ok()
        .as_deref()
        != Some("1")
        && !matches!(
            &cli.command,
            Commands::Daemon(DaemonArgs {
                command: DaemonCommand::Start
            })
        );

    #[cfg(not(unix))]
    let use_daemon = true;

    let app = App::new(
        output_options,
        symora::app::Wiring {
            use_daemon,
            deterministic: cli.deterministic,
            file_watch: match &cli.command {
                Commands::Mcp(McpArgs {
                    command: McpCommand::Serve { .. },
                }) => FileWatch::On,
                _ => FileWatch::Off,
            },
        },
    )
    .await
    .map_err(|e| anyhow::anyhow!("Failed to initialize: {}", e))?;

    tokio::select! {
        result = execute_command(cli.command, &app) => result,
        _ = shutdown_signal() => {
            tracing::debug!("Received shutdown signal");
            Ok(())
        }
    }?;

    // A handler that hit a recoverable failure printed the `{error}` JSON to
    // stdout and returned Ok; surface that as a non-zero exit so a CI / `set
    // -e` caller gets the same failure signal the MCP `isError` flag carries,
    // without scraping stdout. Checked above the daemon/direct split so both
    // modes agree; a shutdown-signal win leaves the flag false and exits 0.
    if app.errored() {
        std::process::exit(2);
    }
    Ok(())
}

fn edit_payload(command: &mut Commands) -> Option<&mut String> {
    use symora::cli::commands::edit::EditCommand;

    let Commands::Edit(args) = command else {
        return None;
    };
    match &mut args.command {
        EditCommand::ReplaceBody { body: value, .. }
        | EditCommand::InsertBefore { code: value, .. }
        | EditCommand::InsertAfter { code: value, .. }
        | EditCommand::Replace { text: value, .. }
        | EditCommand::Pattern { text: value, .. } => Some(value),
        EditCommand::Delete { .. } => None,
    }
}

fn read_edit_stdin(command: &mut Commands) -> anyhow::Result<Option<String>> {
    use std::io::Read;

    if edit_payload(command).is_some_and(|value| value == "-") {
        let mut input = String::new();
        std::io::stdin().read_to_string(&mut input)?;
        Ok(Some(input))
    } else {
        Ok(None)
    }
}

async fn shutdown_signal() {
    #[cfg(unix)]
    {
        use tokio::signal::unix::{SignalKind, signal};
        let mut sigterm = signal(SignalKind::terminate()).expect("SIGTERM handler");
        let mut sigint = signal(SignalKind::interrupt()).expect("SIGINT handler");
        tokio::select! {
            _ = sigterm.recv() => {},
            _ = sigint.recv() => {},
        }
    }
    #[cfg(not(unix))]
    {
        tokio::signal::ctrl_c().await.ok();
    }
}

async fn execute_command(command: Commands, app: &App) -> anyhow::Result<()> {
    if matches!(&command, Commands::Mcp(_)) {
        return execute_scoped(command, app).await;
    }
    #[cfg(unix)]
    if matches!(&command, Commands::Daemon(_)) {
        return execute_scoped(command, app).await;
    }
    let limit = std::time::Duration::from_secs(app.config().lsp.timeout_secs);
    symora::infra::deadline::scope(limit, execute_scoped(command, app)).await
}

async fn execute_scoped(command: Commands, app: &App) -> anyhow::Result<()> {
    use symora::cli::commands;

    match command {
        // Project
        Commands::Config(args) => commands::config::execute(args, app).await,
        Commands::Doctor(args) => commands::doctor::execute(args, app).await,

        // Navigation
        Commands::Symbols(args) => commands::symbols::execute(args, app).await,
        Commands::Def(args) => commands::def::execute(args, app).await,
        Commands::Refs(args) => commands::refs::execute(args, app).await,
        Commands::Typedef(args) => commands::typedef::execute(args, app).await,
        Commands::Implementations(args) => commands::implementations::execute(args, app).await,
        Commands::Callers(args) => commands::callers::execute(args, app).await,
        Commands::Callees(args) => commands::callees::execute(args, app).await,
        Commands::Supertypes(args) => commands::supertypes::execute(args, app).await,
        Commands::Subtypes(args) => commands::subtypes::execute(args, app).await,
        Commands::Hover(args) => commands::hover::execute(args, app).await,

        // Context
        Commands::Context(args) => commands::context::execute(args, app).await,

        // Analysis
        Commands::Impact(args) => commands::impact::execute(args, app).await,
        Commands::DiffImpact(args) => commands::diff_impact::execute(args, app).await,
        Commands::Usage(args) => commands::usage::execute(args, app).await,
        Commands::Diagnostics(args) => commands::diagnostics::execute(args, app).await,

        // Search
        Commands::Search(args) => commands::search::execute(args, app).await,
        Commands::Map(args) => commands::map::execute(args, app).await,
        Commands::Pack(args) => commands::pack::execute(args, app).await,

        // Edit
        Commands::Edit(args) => commands::edit::execute(args, app).await,
        Commands::Rename(args) => commands::rename::execute(args, app).await,
        Commands::Actions(args) => commands::actions::execute(args, app).await,

        // LSP Features

        // Daemon
        #[cfg(unix)]
        Commands::Daemon(args) => commands::daemon::execute(args, app).await,

        // MCP
        Commands::Mcp(args) => commands::mcp::execute(args, app).await,

        // Lifecycle
        Commands::Setup(args) => commands::setup::execute(args, app).await,
        Commands::Selfcmd(args) => commands::selfcmd::execute(args, app).await,
    }
}
