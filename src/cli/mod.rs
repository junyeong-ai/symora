//! CLI module for Symora
//!
//! Provides command-line interface using clap derive macros.

pub mod commands;
pub mod location;
pub mod output;
pub mod response;

pub use location::ParsedLocation;
pub use output::OutputContext;

use clap::{Parser, Subcommand};

use commands::{
    actions::ActionsArgs, batch::BatchArgs, calls::CallsArgs, config::ConfigArgs,
    daemon::DaemonArgs, diagnostics::DiagnosticsArgs, doctor::DoctorArgs, edit::EditArgs,
    find::FindArgs, hover::HoverArgs, impact::ImpactArgs, init::InitArgs, rename::RenameArgs,
    search::SearchArgs, signature::SignatureArgs, status::StatusArgs,
};

const LONG_ABOUT: &str = r#"
Symora - Symbol-centric code intelligence CLI for AI coding agents

Symora provides LSP-powered code intelligence for efficient codebase navigation,
semantic analysis, and code search. Built for AI coding agents.

QUICK START:
  1. Initialize a project:    symora init
  2. Find symbols:            symora find symbol src/main.rs
  3. Search code:             symora search text "pattern"
  4. Get references:          symora find refs src/main.rs:10:5

SEARCH EXAMPLES:
  symora search text "LspService"                    # Fast regex search
  symora search text "fn.*async" -t rust -i          # Case insensitive, Rust only
  symora search ast "(function_item)" -l rust        # AST pattern search

LSP EXAMPLES:
  symora hover src/main.rs:10:5
  symora find def src/api.rs:25:10
  symora calls incoming src/api.rs:25:10

For more information: https://github.com/junyeong-ai/symora
"#;

/// Symora - Symbol-centric code intelligence CLI for AI coding agents
#[derive(Parser, Debug)]
#[command(name = "symora")]
#[command(author, version, about, long_about = LONG_ABOUT)]
#[command(propagate_version = true)]
#[command(after_help = "Use 'symora <COMMAND> --help' for more information about a command.")]
pub struct Cli {
    #[command(subcommand)]
    pub command: Commands,

    /// Output format (json, text)
    #[arg(long, global = true, default_value = "json")]
    pub format: String,

    /// Verbose output (show debug info)
    #[arg(short, long, global = true)]
    pub verbose: bool,
}

/// Available commands
#[derive(Subcommand, Debug)]
pub enum Commands {
    /// Initialize a new Symora project
    Init(InitArgs),

    /// Show project status
    Status(StatusArgs),

    /// Configuration management
    Config(ConfigArgs),

    /// Check dependencies (LSP servers, ripgrep)
    Doctor(DoctorArgs),

    /// Find symbols, references, definitions (LSP)
    Find(FindArgs),

    /// Search code (text: ripgrep, ast: tree-sitter)
    Search(SearchArgs),

    /// Get hover information for a position
    Hover(HoverArgs),

    /// Get function/method signature help
    Signature(SignatureArgs),

    /// Get LSP diagnostics for a file
    Diagnostics(DiagnosticsArgs),

    /// Rename a symbol across the codebase
    Rename(RenameArgs),

    /// Call hierarchy (incoming/outgoing)
    Calls(CallsArgs),

    /// Code actions (quickfix, refactor, source)
    Actions(ActionsArgs),

    /// Impact analysis for symbol changes
    Impact(ImpactArgs),

    /// Text editing operations
    Edit(EditArgs),

    /// Execute multiple commands in batch
    Batch(BatchArgs),

    /// Daemon server management (start, stop, status)
    Daemon(DaemonArgs),
}
