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
pub use file_symbols::{
    FileSymbols, SymbolBackend, declared_addressing, declared_at, declared_in, read_by_grammar,
};
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
};

const LONG_ABOUT: &str = r#"
Symora - Symbol-centric code intelligence CLI for AI coding agents

Answers navigation, impact, and edit questions from language servers, a
SQLite symbol/content index, and tree-sitter grammars, as JSON on stdout.

CHOOSE A QUERY:
  Known file or symbol:   symbols <file> [--symbol <path>]
  Unknown name or text:   search symbols <query> | search content <query>
  Who uses it:            refs <loc> | callers <loc> | context <loc>
  What a change reaches:  impact <loc> | diff-impact [REVISION]
  Orientation:            map summary | pack --tokens 4000
  Edit by declaration:    edit replace-body <file> --symbol <path> --dry-run

<loc> is file:line[:column], 1-indexed. Run `symora <command> --help` for its options.
"#;

#[derive(Parser, Debug)]
#[command(name = "symora")]
#[command(author, version, about, long_about = LONG_ABOUT)]
#[command(propagate_version = true)]
pub struct Cli {
    #[command(subcommand)]
    pub command: Commands,

    /// Output format [default: pretty on a terminal, compact otherwise]
    #[arg(long, value_enum, global = true, help_heading = GLOBAL)]
    pub format: Option<OutputFormat>,

    #[arg(
        long,
        global = true,
        value_name = "REQ",
        help_heading = GLOBAL,
        help = concat!(
            "Refuse to run unless this binary satisfies the SemVer requirement, e.g. `",
            env!("CARGO_PKG_VERSION_MAJOR"),
            ".",
            env!("CARGO_PKG_VERSION_MINOR"),
            "` or `=",
            env!("CARGO_PKG_VERSION"),
            "`"
        )
    )]
    pub check_version: Option<String>,

    /// Never consult a language server: answer from the index, compiled-in
    /// grammars and file scans; a command that needs one fails as `unsupported`
    #[arg(long, global = true, help_heading = GLOBAL)]
    pub deterministic: bool,

    /// Print errors only
    #[arg(short, long, global = true, help_heading = GLOBAL)]
    pub quiet: bool,

    /// Log debug detail to stderr
    #[arg(short, long, global = true, help_heading = GLOBAL)]
    pub verbose: bool,
}

const GLOBAL: &str = "Global Options";

/// Hold the running binary to a caller's pinned requirement.
///
/// A consumer that parses this tool's JSON was written against a version of
/// it. Checking that here rather than in each caller's shell is what keeps the
/// check on the producing side of the contract, where the version is known
/// exactly and the failure is one typed error rather than a parsed
/// `--version` string.
pub fn check_version(requirement: &str) -> Result<(), OutputError> {
    let running = semver::Version::parse(env!("CARGO_PKG_VERSION")).map_err(|e| {
        OutputError::new(
            ErrorCode::Internal,
            format!(
                "this binary's version `{}` is not SemVer: {e}",
                env!("CARGO_PKG_VERSION")
            ),
        )
    })?;
    let parsed = semver::VersionReq::parse(requirement).map_err(|e| {
        let (major, minor) = (running.major, running.minor);
        OutputError::invalid(format!(
            "--check-version `{requirement}` is not a SemVer requirement: {e}"
        ))
        .with_hint(format!(
            "Write a requirement such as `{major}.{minor}`, `={running}`, or \
             `>={major}.{minor},<{major}.{}`.",
            minor + 1
        ))
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
    /// LSP diagnostics
    Diagnostics(DiagnosticsArgs),

    // Search
    /// Search rough symbol/content matches when you do not know the exact file yet
    Search(SearchArgs),
    /// Project overview, or one file's declarations, tests, and neighbours
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
