use std::net::{IpAddr, SocketAddr};

use anyhow::Result;
use clap::{Args, Subcommand, ValueEnum};

use crate::app::App;
use crate::mcp::{self, McpProfile};

#[derive(Args, Debug)]
pub struct McpArgs {
    #[command(subcommand)]
    pub command: McpCommand,
}

#[derive(Subcommand, Debug)]
pub enum McpCommand {
    /// Start an MCP server speaking JSON-RPC 2.0 over the chosen transport.
    Serve {
        /// Transport for the MCP channel. Stdio is the default — most
        /// agents (Claude Code, Cursor, Codex) expect it. HTTP is for
        /// remote agents and shared hosting.
        #[arg(long, value_enum, default_value_t = McpTransport::Stdio)]
        transport: McpTransport,

        /// Bind address for `--transport http`. Default is loopback only.
        #[arg(long, default_value = "127.0.0.1")]
        host: IpAddr,

        /// Port for `--transport http`.
        #[arg(long, default_value_t = crate::constants::defaults::MCP_HTTP_DEFAULT_PORT)]
        port: u16,

        /// Tool-surface profile. `read-only` hides mutating tools from
        /// tools/list and refuses them at tools/call.
        #[arg(long, value_enum, default_value_t = McpProfile::Full)]
        profile: McpProfile,
    },
}

#[derive(Debug, Clone, Copy, ValueEnum)]
#[clap(rename_all = "lowercase")]
pub enum McpTransport {
    /// Line-delimited JSON-RPC over stdin/stdout.
    Stdio,
    /// JSON-RPC 2.0 over HTTP POST `/`.
    Http,
}

pub async fn execute(args: McpArgs, app: &App) -> Result<()> {
    match args.command {
        McpCommand::Serve {
            transport,
            host,
            port,
            profile,
        } => match transport {
            McpTransport::Stdio => mcp::serve_stdio(clone_app(app), profile).await,
            McpTransport::Http => {
                let addr = SocketAddr::new(host, port);
                mcp::serve_http(clone_app(app), addr, profile).await
            }
        },
    }
}

fn clone_app(app: &App) -> App {
    // App carries Arcs for every service, so a sink-swap clone keeps the
    // exact same daemon/LSP/store handles. We use the unchanged sink so
    // the per-tool `with_output_sink` swap stays the only buffering point.
    use std::sync::Arc;

    use crate::cli::OutputOptions;
    use crate::cli::output::StdoutSink;

    app.with_output_sink(Arc::new(StdoutSink), OutputOptions::default())
}
