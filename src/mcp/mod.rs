//! Model Context Protocol (MCP) adapter — exposes Symora commands as MCP
//! tools over a JSON-RPC 2.0 stdio channel.
//!
//! The CLI remains the source of truth: every tool call runs the matching
//! command in-process against an `App` whose output sink is a buffer, and
//! the buffered JSON flows back as MCP `text` content. This keeps the
//! daemon, LSP pool, and indexing behaviour identical between `symora <cmd>`
//! and `tools/call` invocations.

mod http;
mod protocol;
mod server;
mod tools;

pub use http::serve_http;
pub use protocol::{MCP_PROTOCOL_VERSION, SUPPORTED_PROTOCOL_VERSIONS};
pub use server::serve_stdio;
