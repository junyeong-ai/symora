//! Model Context Protocol (MCP) adapter — exposes Symora commands as MCP
//! tools over a JSON-RPC 2.0 stdio channel.
//!
//! The CLI remains the source of truth: every tool call runs the matching
//! command in-process against an `App` whose output sink is a buffer, and
//! the buffered JSON flows back as MCP `text` content. This keeps the
//! daemon, LSP pool, and indexing behaviour identical between `symora <cmd>`
//! and `tools/call` invocations.

mod http;
mod instructions;
mod protocol;
mod server;
mod tools;

pub use http::serve_http;
pub use protocol::{MCP_PROTOCOL_VERSION, SUPPORTED_PROTOCOL_VERSIONS};
pub use server::serve_stdio;

/// Tool-surface profile for `symora mcp serve`.
///
/// `ReadOnly` removes every mutating tool from `tools/list` *and* refuses
/// it at `tools/call` — the boundary holds even for clients that ignore
/// the advertised list. The filter predicate is the typed
/// `annotations.read_only_hint`, never description text.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, clap::ValueEnum)]
#[clap(rename_all = "kebab-case")]
pub enum McpProfile {
    /// Full catalog: read and mutate tools.
    #[default]
    Full,
    /// Mutating tools are hidden from `tools/list` and refused at
    /// `tools/call`.
    ReadOnly,
}

impl McpProfile {
    pub fn allows(self, tool: &protocol::ToolDefinition) -> bool {
        match self {
            Self::Full => true,
            Self::ReadOnly => tool.annotations.read_only_hint,
        }
    }
}
