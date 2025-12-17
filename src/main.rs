//! Symora - Symbol-centric Code Intelligence CLI
//!
//! "Open the Gate to Code Structure"
//!
//! A powerful CLI tool for AI coding agents that provides LSP-based
//! semantic code analysis with symbol-level precision.

use clap::Parser;
use tracing_subscriber::{layer::SubscriberExt, util::SubscriberInitExt};

use symora::app::App;
use symora::cli::commands::daemon::{DaemonArgs, DaemonCommand};
use symora::cli::{Cli, Commands};

fn main() {
    // Initialize tracing with quiet defaults for AI agent consumption
    // Use RUST_LOG=symora=debug for verbose output
    tracing_subscriber::registry()
        .with(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "symora=warn".into()),
        )
        .with(
            tracing_subscriber::fmt::layer()
                .with_target(false)
                .compact(),
        )
        .init();

    // Run async main and handle errors with JSON output
    let runtime = match tokio::runtime::Runtime::new() {
        Ok(rt) => rt,
        Err(e) => {
            eprintln!(
                r#"{{"success":false,"error":"Failed to create runtime: {}"}}"#,
                e
            );
            std::process::exit(1);
        }
    };
    let result = runtime.block_on(async_main());

    if let Err(e) = result {
        // All errors are output as JSON for consistent AI agent consumption
        let response = serde_json::json!({
            "success": false,
            "error": e.to_string()
        });
        println!(
            "{}",
            serde_json::to_string_pretty(&response)
                .unwrap_or_else(|_| { format!(r#"{{"success":false,"error":"{}"}}"#, e) })
        );
        std::process::exit(2);
    }
}

async fn async_main() -> anyhow::Result<()> {
    // Parse CLI arguments
    let cli = Cli::parse();

    // Determine if we need daemon mode
    // Daemon server itself doesn't use daemon client
    let use_daemon = !matches!(
        &cli.command,
        Commands::Daemon(DaemonArgs {
            command: DaemonCommand::Start
        }) | Commands::Doctor(_)
    );

    // Initialize application
    let app = App::with_daemon(use_daemon)
        .await
        .map_err(|e| anyhow::anyhow!("Failed to initialize: {}", e))?;

    // Execute command
    execute_command(cli.command, &app).await
}

async fn execute_command(command: Commands, app: &App) -> anyhow::Result<()> {
    use symora::cli::commands;

    match command {
        // Project management
        Commands::Init(args) => commands::init::execute(args, app).await,
        Commands::Status(args) => commands::status::execute(args, app).await,
        Commands::Config(args) => commands::config::execute(args, app).await,
        Commands::Doctor(args) => commands::doctor::execute(args, app),

        // Symbol operations (LSP-based)
        Commands::Find(args) => commands::find::execute(args, app).await,
        Commands::Hover(args) => commands::hover::execute(args, app).await,
        Commands::Signature(args) => commands::signature::execute(args, app).await,
        Commands::Diagnostics(args) => commands::diagnostics::execute(args, app).await,
        Commands::Rename(args) => commands::rename::execute(args, app).await,

        // Call hierarchy
        Commands::Calls(args) => commands::calls::execute(args, app).await,

        // Code transformation
        Commands::Actions(args) => commands::actions::execute(args, app).await,
        Commands::Impact(args) => commands::impact::execute(args, app).await,
        Commands::Edit(args) => commands::edit::execute(args, app).await,

        // Search (AST pattern)
        Commands::Search(args) => commands::search::execute(args, app).await,

        // Batch mode
        Commands::Batch(args) => commands::batch::execute(args, app).await,

        // Daemon management
        Commands::Daemon(args) => commands::daemon::execute(args, app).await,
    }
}
