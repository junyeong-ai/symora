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

    match runtime.block_on(async_main()) {
        // Exiting skips destructors, so the runtime is dropped first, which
        // waits for the work it holds: its tasks are dropped, stopping the
        // language servers this process started, and a handler writing
        // source files, which it does without yielding, finishes. A read of
        // stdin there would hold the exit until the client wrote again, so
        // `mcp serve` reads stdin outside it.
        Ok(errored) => {
            drop(runtime);
            if errored {
                std::process::exit(2);
            }
        }
        Err(e) => refuse(e.into()),
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
        // Clap answers a missing subcommand with the whole help text, which
        // states no error to report and is the one case `usage_error` cannot
        // read as one.
        Err(e) if e.kind() == ErrorKind::DisplayHelpOnMissingArgumentOrSubcommand => refuse(
            symora::cli::OutputError::invalid("a subcommand is required")
                .with_hint("Run the command with --help to list its subcommands."),
        ),
        Err(e) => refuse(usage_error(&e)),
    }
}

/// A usage error in the shape every other failure takes. Clap states the
/// error in its first paragraph, with any arguments or values it lists, then
/// gives its tips, the usage line and where help is, a paragraph each: the
/// statement is the message and the rest is the hint.
fn usage_error(error: &clap::Error) -> symora::cli::OutputError {
    let rendered = error.to_string();
    let mut paragraphs = rendered
        .split("\n\n")
        .map(|paragraph| {
            paragraph
                .lines()
                .map(str::trim)
                .filter(|line| !line.is_empty())
                .collect::<Vec<_>>()
                .join(" ")
        })
        .filter(|paragraph| !paragraph.is_empty());
    let statement = paragraphs.next().unwrap_or_default();
    let message = statement.strip_prefix("error: ").unwrap_or(&statement);
    let hint: Vec<String> = paragraphs
        .map(|paragraph| match paragraph.ends_with('.') {
            true => paragraph,
            false => format!("{paragraph}."),
        })
        .collect();
    symora::cli::OutputError::invalid(message).with_hint(hint.join(" "))
}

/// Run the command; true when it reported a failure.
async fn async_main() -> anyhow::Result<bool> {
    let mut cli = parse_cli()?;

    let output_options = OutputOptions {
        format: cli
            .format
            .unwrap_or_else(symora::cli::OutputFormat::for_stdout),
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

    // A daemon stops itself when asked, through the teardown that asks its
    // servers to exit; any other command is dropped where it stands.
    #[cfg(unix)]
    let stops_itself = matches!(
        &cli.command,
        Commands::Daemon(DaemonArgs {
            command: DaemonCommand::Serve
        })
    );
    #[cfg(not(unix))]
    let stops_itself = false;
    let result = if stops_itself {
        execute_command(cli.command, &app).await
    } else {
        tokio::select! {
            result = execute_command(cli.command, &app) => result,
            () = symora::infra::signal::stop_requested() => {
                tracing::debug!("Received shutdown signal");
                Ok(())
            }
        }
    };
    // A failure a command returns is reported through the same output layer
    // as one it prints, so both carry what that layer adds to a response.
    if let Err(e) = result {
        app.report_error(e);
    }
    app.shutdown().await;

    // A handler that hit a recoverable failure printed the `{error}` JSON to
    // stdout and returned Ok; surface that as a non-zero exit so a CI / `set
    // -e` caller gets the same failure signal the MCP `isError` flag carries,
    // without scraping stdout. Checked above the daemon/direct split so both
    // modes agree; a shutdown-signal win leaves the flag false and exits 0.
    Ok(app.errored())
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
        // Environment
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
        Commands::Diagnostics(args) => commands::diagnostics::execute(args, app).await,

        // Search
        Commands::Search(args) => commands::search::execute(args, app).await,
        Commands::Map(args) => commands::map::execute(args, app).await,
        Commands::Pack(args) => commands::pack::execute(args, app).await,

        // Edit
        Commands::Edit(args) => commands::edit::execute(args, app).await,
        Commands::Rename(args) => commands::rename::execute(args, app).await,
        Commands::Actions(args) => commands::actions::execute(args, app).await,

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
