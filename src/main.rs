use clap::Parser;
use tracing_subscriber::{layer::SubscriberExt, util::SubscriberInitExt};

use symora::app::App;
#[cfg(unix)]
use symora::cli::commands::daemon::{DaemonArgs, DaemonCommand};
use symora::cli::{Cli, Commands, OutputOptions};

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
        let err: symora::cli::OutputError = e.into();
        let envelope = serde_json::json!({ "error": err });
        eprintln!("{}", envelope);
        std::process::exit(2);
    }
}

async fn async_main() -> anyhow::Result<()> {
    let cli = Cli::parse();

    let format =
        if std::env::var(symora::constants::env::FORMAT_OVERRIDE).as_deref() == Ok("compact") {
            symora::cli::OutputFormat::Compact
        } else {
            cli.format
        };

    let output_options = OutputOptions {
        format,
        quiet: cli.quiet,
        token_estimate: cli.token_estimate,
    };

    if let Some(name) = cli.workspace.as_deref() {
        return run_workspace_dispatch(name, output_options).await;
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

    let app = App::new(output_options, use_daemon)
        .await
        .map_err(|e| anyhow::anyhow!("Failed to initialize: {}", e))?;

    tokio::select! {
        result = execute_command(cli.command, &app) => result,
        _ = shutdown_signal() => {
            tracing::debug!("Received shutdown signal");
            Ok(())
        }
    }
}

async fn run_workspace_dispatch(name: &str, output_options: OutputOptions) -> anyhow::Result<()> {
    use symora::cli::OutputContext;
    use symora::cli::workspace::{WorkspaceConfig, run_workspace, strip_workspace_flag};

    let ws = WorkspaceConfig::load(name)
        .map_err(|e| anyhow::anyhow!("Failed to load workspace '{name}': {e}"))?;
    let exe = std::env::current_exe()?;
    let argv: Vec<String> = std::env::args().collect();
    let forwarded = strip_workspace_flag(argv).into_iter().skip(1).collect();

    let envelope = run_workspace(ws, &exe, forwarded).await;
    let any_failed = envelope
        .get("items")
        .and_then(|r| r.as_array())
        .map(|arr| arr.iter().any(|entry| entry.get("error").is_some()))
        .unwrap_or(false);

    let cwd = std::env::current_dir()?;
    OutputContext::new(cwd, output_options).print_success(envelope);

    if any_failed {
        // CI / `set -e` users need a non-zero exit when at least one root
        // failed, even though the JSON envelope reports both successes and
        // failures together.
        std::process::exit(2);
    }
    Ok(())
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
    use symora::cli::commands;

    match command {
        // Project
        Commands::Init(args) => commands::init::execute(args, app).await,
        Commands::Status(args) => commands::status::execute(args, app).await,
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
        Commands::Signature(args) => commands::signature::execute(args, app).await,

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
        Commands::Bench(args) => commands::bench::execute(args, app).await,

        // Edit
        Commands::Edit(args) => commands::edit::execute(args, app).await,
        Commands::Rename(args) => commands::rename::execute(args, app).await,
        Commands::Actions(args) => commands::actions::execute(args, app).await,

        // LSP Features
        Commands::InlayHints(args) => commands::inlay_hints::execute(args, app).await,
        Commands::Folding(args) => commands::folding::execute(args, app).await,
        Commands::Selection(args) => commands::selection::execute(args, app).await,
        Commands::CodeLens(args) => commands::code_lens::execute(args, app).await,
        Commands::Format(args) => commands::format::execute(args, app).await,

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
