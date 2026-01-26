//! CLI module for Symora

pub mod commands;
pub mod location;
pub mod output;
pub mod response;
pub mod utils;

pub use location::ParsedLocation;
pub use output::{OutputContext, OutputOptions};

use clap::{Parser, Subcommand};

#[cfg(unix)]
use commands::daemon::DaemonArgs;
use commands::{
    actions::ActionsArgs, callees::CalleesArgs, callers::CallersArgs, config::ConfigArgs,
    context::ContextArgs, def::DefArgs, diagnostics::DiagnosticsArgs, diff_impact::DiffImpactArgs,
    doctor::DoctorArgs, edit::EditArgs, hover::HoverArgs, impact::ImpactArgs, impl_cmd::ImplArgs,
    init::InitArgs, refs::RefsArgs, rename::RenameArgs, search::SearchArgs,
    signature::SignatureArgs, status::StatusArgs, subtypes::SubtypesArgs,
    supertypes::SupertypesArgs, symbols::SymbolsArgs, typedef::TypedefArgs, usage::UsageArgs,
};

const LONG_ABOUT: &str = r#"
Symora - Symbol-centric code intelligence CLI for AI coding agents

Provides LSP-powered code intelligence for codebase navigation,
semantic analysis, and code search. Built for AI coding agents.

QUICK START:
  symora init                    Initialize project
  symora symbols src/main.rs     List symbols in file
  symora refs src/main.rs:10:5   Find references
  symora callers src/main.rs:10  Find who calls this

EXAMPLES:
  symora symbols src/lib.rs --body          Include source code
  symora refs src/api.rs:25:10 --snippet    Include code snippets
  symora context src/main.rs:50 --all       Get full context
  symora impact src/api.rs:30:5             Analyze change impact

For more: https://github.com/anthropics/symora
"#;

#[derive(Parser, Debug)]
#[command(name = "symora")]
#[command(author, version, about, long_about = LONG_ABOUT)]
#[command(propagate_version = true)]
pub struct Cli {
    #[command(subcommand)]
    pub command: Commands,

    /// Compact output for AI tools (minimal tokens)
    #[arg(short, long, global = true)]
    pub compact: bool,

    /// Quiet mode (errors only)
    #[arg(short, long, global = true)]
    pub quiet: bool,

    /// Enable debug logging
    #[arg(short, long, global = true)]
    pub verbose: bool,
}

#[derive(Subcommand, Debug)]
pub enum Commands {
    // Project
    /// Initialize a new project
    Init(InitArgs),
    /// Show project status
    Status(StatusArgs),
    /// Configuration management
    Config(ConfigArgs),
    /// Diagnose environment and language servers
    Doctor(DoctorArgs),

    // Navigation
    /// Find symbols in file or workspace
    Symbols(SymbolsArgs),
    /// Go to definition
    Def(DefArgs),
    /// Find all references
    Refs(RefsArgs),
    /// Go to type definition
    Typedef(TypedefArgs),
    /// Find implementations
    Impl(ImplArgs),
    /// Find callers (incoming calls)
    Callers(CallersArgs),
    /// Find callees (outgoing calls)
    Callees(CalleesArgs),
    /// Find parent types (supertypes)
    Supertypes(SupertypesArgs),
    /// Find child types (subtypes)
    Subtypes(SubtypesArgs),
    /// Get hover information
    Hover(HoverArgs),
    /// Get function signature at position
    Signature(SignatureArgs),

    // Context
    /// Gather all context for a symbol
    Context(ContextArgs),

    // Analysis
    /// Impact analysis for changes
    Impact(ImpactArgs),
    /// Impact analysis for git diff
    DiffImpact(DiffImpactArgs),
    /// Usage patterns and metrics
    Usage(UsageArgs),
    /// LSP diagnostics
    Diagnostics(DiagnosticsArgs),

    // Search
    /// Search code (symbols, content, ast)
    Search(SearchArgs),

    // Edit
    /// Code editing operations
    Edit(EditArgs),
    /// Rename symbol
    Rename(RenameArgs),
    /// Code actions (refactoring, quickfix)
    Actions(ActionsArgs),

    // Daemon (Unix only)
    #[cfg(unix)]
    /// Daemon server management
    Daemon(DaemonArgs),
}
