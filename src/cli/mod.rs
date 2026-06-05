pub mod analysis;
pub mod blast_radius;
pub mod commands;
pub mod errors;
pub mod input_error;
pub mod location;
pub mod output;
pub mod response;
pub mod symbol_discovery;
pub mod utils;
pub mod workspace;

pub use analysis::{LocationAnalysis, detect_exported};
pub use blast_radius::{BlastRadius, BlastRadiusConfig, RiskLevel};
pub use errors::{ErrorCode, OutputError};
pub use input_error::CliInputError;
pub use location::{LocationArg, ParsedLocation};
pub use output::{OutputContext, OutputFormat, OutputOptions};
pub use workspace::WorkspaceConfig;

use clap::{Parser, Subcommand};

#[cfg(unix)]
use commands::daemon::DaemonArgs;
use commands::{
    actions::ActionsArgs, bench::BenchArgs, callees::CalleesArgs, callers::CallersArgs,
    code_lens::CodeLensArgs, config::ConfigArgs, context::ContextArgs, def::DefArgs,
    diagnostics::DiagnosticsArgs, diff_impact::DiffImpactArgs, doctor::DoctorArgs, edit::EditArgs,
    folding::FoldingArgs, format::FormatArgs, hover::HoverArgs, impact::ImpactArgs,
    implementations::ImplementationsArgs, init::InitArgs, inlay_hints::InlayHintsArgs,
    map::MapArgs, mcp::McpArgs, pack::PackArgs, refs::RefsArgs, rename::RenameArgs,
    search::SearchArgs, selection::SelectionArgs, selfcmd::SelfcmdArgs, setup::SetupArgs,
    signature::SignatureArgs, status::StatusArgs, subtypes::SubtypesArgs,
    supertypes::SupertypesArgs, symbols::SymbolsArgs, typedef::TypedefArgs, usage::UsageArgs,
    write::WriteArgs,
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

EXPLORATION FLOW:
  1. `symora map summary` for project entrypoints and major areas
  2. `symora search symbols <query>` when you know only a rough name/path
  3. `symora map file <path>` to inspect one file before reading full code
  4. `symora symbols <file>` or `symora symbols --symbol <path>` for exact semantic follow-up
  5. `symora refs <loc>` once you have the exact symbol location

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

    /// Output format: pretty (default), compact (single-line JSON), or jsonl (newline-delimited).
    #[arg(long, value_enum, global = true, default_value_t = OutputFormat::Pretty)]
    pub format: OutputFormat,

    /// Run the command across every root in the named workspace
    /// (~/.config/symora/workspaces/&lt;name&gt;.toml). Each root gets its own
    /// child invocation; results are bundled into a single JSON envelope.
    #[arg(long, global = true)]
    pub workspace: Option<String>,

    /// Print an estimated token count for the response to stderr (does not alter stdout).
    #[arg(long, global = true)]
    pub token_estimate: bool,

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
    /// Inspect exact file symbols or exact workspace symbol trees
    Symbols(SymbolsArgs),
    /// Go to definition
    Def(DefArgs),
    /// Find all references
    Refs(RefsArgs),
    /// Go to type definition
    Typedef(TypedefArgs),
    /// Find implementations
    Implementations(ImplementationsArgs),
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
    /// Search rough symbol/content matches when you do not know the exact file yet
    Search(SearchArgs),
    /// Explore project structure, file overviews, and related files
    Map(MapArgs),
    /// Build a token-budgeted context pack ranked by an import-graph PageRank
    Pack(PackArgs),
    /// Measure end-to-end latency of LSP-less hot paths on this repository
    Bench(BenchArgs),

    // Edit
    /// Symbol-aware writes (replace-body, insert-before, insert-after)
    Write(WriteArgs),
    /// Code editing operations (LSP textEdits, format)
    Edit(EditArgs),
    /// Rename symbol
    Rename(RenameArgs),
    /// Code actions (refactoring, quickfix)
    Actions(ActionsArgs),

    // LSP Features
    /// Get inlay hints for a file
    InlayHints(InlayHintsArgs),
    /// Get folding ranges for a file
    Folding(FoldingArgs),
    /// Get selection ranges at position
    Selection(SelectionArgs),
    /// Get code lenses for a file
    CodeLens(CodeLensArgs),
    /// Format a file using LSP
    Format(FormatArgs),

    // Daemon (Unix only)
    #[cfg(unix)]
    /// Daemon server management
    Daemon(DaemonArgs),

    /// Run as a Model Context Protocol server (exposes Symora tools to AI agents)
    Mcp(McpArgs),

    // Lifecycle (CLI-only — not surfaced over MCP)
    /// Configure the Claude Code skill and language-server dependencies
    Setup(SetupArgs),
    /// Manage this binary itself (update, uninstall)
    #[command(name = "self")]
    Selfcmd(SelfcmdArgs),
}
