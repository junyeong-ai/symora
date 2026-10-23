pub mod analysis;
pub mod blast_radius;
pub mod call_graph;
pub mod commands;
pub mod errors;
pub mod file_symbols;
pub mod input_error;
pub mod location;
pub mod output;
pub mod response;
pub mod symbol_discovery;
pub mod utils;

pub use analysis::LocationAnalysis;
pub use blast_radius::BlastRadius;
pub use call_graph::{CallGraphWalk, Direction, WalkConfig};
pub use errors::{ErrorCode, OutputError};
pub use file_symbols::{FileSymbols, SymbolBackend, declared_in};
pub use input_error::{CliInputError, resolve_project_file};
pub use location::{LocationArg, ParsedLocation};
pub use output::{OutputContext, OutputFormat, OutputOptions};

use clap::{Parser, Subcommand};

#[cfg(unix)]
use commands::daemon::DaemonArgs;
use commands::{
    actions::ActionsArgs, callees::CalleesArgs, callers::CallersArgs, config::ConfigArgs,
    context::ContextArgs, def::DefArgs, diagnostics::DiagnosticsArgs, diff_impact::DiffImpactArgs,
    doctor::DoctorArgs, edit::EditArgs, hover::HoverArgs, impact::ImpactArgs,
    implementations::ImplementationsArgs, map::MapArgs, mcp::McpArgs, pack::PackArgs,
    refs::RefsArgs, rename::RenameArgs, search::SearchArgs, selfcmd::SelfcmdArgs, setup::SetupArgs,
    subtypes::SubtypesArgs, supertypes::SupertypesArgs, symbols::SymbolsArgs, typedef::TypedefArgs,
    usage::UsageArgs,
};

const LONG_ABOUT: &str = r#"
Symora - Symbol-centric code intelligence CLI for AI coding agents

Provides LSP-powered code intelligence for codebase navigation,
semantic analysis, and code search. Built for AI coding agents.

QUICK START:
  symora symbols src/main.rs     List symbols in file
  symora refs src/main.rs:10:5   Find references
  symora callers src/main.rs:10  Find who calls this

CHOOSE A QUERY:
  Known file or symbol: `symbols <file>` or `symbols <file> --symbol <path>`
  Unknown name or text: `search symbols <query>` or `search content <query>`
  Project orientation: optional `map summary` or `pack --tokens 4000`
  Exact semantic follow-up: `refs <loc>`, `callers <loc>`, or `context <loc>`

EXAMPLES:
  symora symbols src/lib.rs --body          Include source code
  symora refs src/api.rs:25:10 --snippet    Include code snippets
  symora context src/main.rs:50 --all       Get full context
  symora impact src/api.rs:30:5             Analyze change impact

For more: https://github.com/junyeong-ai/symora
"#;

#[derive(Parser, Debug)]
#[command(name = "symora")]
#[command(author, version, about, long_about = LONG_ABOUT)]
#[command(propagate_version = true)]
pub struct Cli {
    #[command(subcommand)]
    pub command: Commands,

    /// Output format: pretty (default) or compact (single-line JSON).
    #[arg(long, value_enum, global = true, default_value_t = OutputFormat::Pretty)]
    pub format: OutputFormat,

    /// Refuse to run unless this binary's version satisfies the SemVer
    /// requirement (e.g. `0.21`, `>=0.21,<0.22`). A pinned caller sets it so a
    /// version whose output it was not written against fails loudly instead of
    /// being parsed as if it were.
    #[arg(long, global = true, value_name = "REQ")]
    pub check_version: Option<String>,

    /// Answer only from the index and the compiled-in grammars, never a
    /// language server, so which servers a machine has installed cannot
    /// change the answer. A command with no such source fails instead of
    /// degrading to a weaker answer wearing the same shape.
    #[arg(long, global = true)]
    pub deterministic: bool,

    /// Quiet mode (errors only)
    #[arg(short, long, global = true)]
    pub quiet: bool,

    /// Enable debug logging
    #[arg(short, long, global = true)]
    pub verbose: bool,
}

/// Hold the running binary to a caller's pinned requirement.
///
/// A consumer that parses this tool's JSON was written against a version of
/// it. Checking that here rather than in each caller's shell is what keeps the
/// check on the producing side of the contract, where the version is known
/// exactly and the failure is one typed error rather than a parsed
/// `--version` string.
pub fn check_version(requirement: &str) -> Result<(), OutputError> {
    let parsed = semver::VersionReq::parse(requirement).map_err(|e| {
        OutputError::invalid(format!(
            "--check-version `{requirement}` is not a SemVer requirement: {e}"
        ))
        .with_hint("Write a requirement such as `0.21`, `=0.21.0`, or `>=0.21,<0.22`.")
    })?;
    let running = semver::Version::parse(env!("CARGO_PKG_VERSION")).map_err(|e| {
        OutputError::new(
            ErrorCode::Internal,
            format!(
                "this binary's version `{}` is not SemVer: {e}",
                env!("CARGO_PKG_VERSION")
            ),
        )
    })?;
    if parsed.matches(&running) {
        return Ok(());
    }
    Err(OutputError::precondition_failed(format!(
        "symora {running} does not satisfy `{requirement}`"
    ))
    .with_hint("Install the version this caller was written against, or widen the requirement."))
}

#[derive(Subcommand, Debug)]
pub enum Commands {
    // Environment
    /// Configuration management
    Config(ConfigArgs),
    /// Diagnose environment and language servers
    Doctor(DoctorArgs),

    // Navigation
    /// Inspect a file's symbols, or resolve a symbol tree by path
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

    // Edit
    /// Source mutations: replace-body, insert-before/after, delete, replace, pattern
    Edit(EditArgs),
    /// Rename symbol
    Rename(RenameArgs),
    /// Code actions (refactoring, quickfix)
    Actions(ActionsArgs),

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

/// Parse every `symora …` command a piece of advice names, as an agent would
/// run it: `<location>` and `"<symbol>"` are the placeholders it fills, and a
/// named command ends at ` or ` or at the end of the advice.
#[cfg(test)]
pub(crate) fn assert_named_commands_parse(advice: &str) {
    use clap::Parser;

    for named in advice.split("symora ").skip(1) {
        let command = named
            .split(" or ")
            .next()
            .unwrap_or(named)
            .replace("<location>", "src/lib.rs:1:1")
            .replace("\"<symbol>\"", "name");
        let args = std::iter::once("symora")
            .chain(command.split_whitespace().map(|arg| arg.trim_matches('"')));
        if let Err(err) = Cli::try_parse_from(args) {
            panic!("`{advice}` names `symora {command}`: {err}");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The pin is checked where the version is known exactly, so a caller
    /// never has to parse `--version` prose to enforce one.
    #[test]
    fn check_version_holds_the_binary_to_the_requirement() {
        let running = env!("CARGO_PKG_VERSION");
        assert!(check_version(&format!("={running}")).is_ok());
        assert!(check_version(">=0.0.1").is_ok());

        let refused = check_version("<0.0.1").expect_err("a version below the floor is refused");
        assert_eq!(refused.code, ErrorCode::PreconditionFailed);
        assert!(refused.message.contains(running));

        let malformed = check_version("not-a-req").expect_err("a malformed requirement is refused");
        assert_eq!(malformed.code, ErrorCode::InvalidArgument);
    }
    use clap::CommandFactory;

    /// clap's internal consistency checks (duplicate flag names, conflicting
    /// ids, …) only run as debug assertions when a command is built — a
    /// collision panics every debug invocation while `cargo test` stays
    /// green. Building the full tree here turns that whole failure class
    /// into a test failure instead.
    #[test]
    fn clap_tree_passes_debug_assertions() {
        super::Cli::command().debug_assert();
    }
}
