use std::net::{IpAddr, SocketAddr};

use anyhow::Result;
use clap::{Args, Subcommand, ValueEnum};
use serde::Serialize;

use crate::app::App;
use crate::mcp::{self, McpProfile, ToolDefinition};

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

    /// Print the tool catalog as JSON — names, descriptions, input and
    /// output schemas, and mutation annotations. The exact payload
    /// `tools/list` serves; each output schema also describes the JSON
    /// the matching CLI command emits.
    Tools {
        /// Profile whose visible catalog to print.
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

/// The catalog view `mcp tools` prints. `tools` reproduces the MCP wire
/// shape byte-for-byte (camelCase keys included) — it is the `tools/list`
/// payload, not a translation of it.
#[derive(Serialize)]
struct McpToolsOutput {
    version: &'static str,
    profile: String,
    tools: Vec<ToolDefinition>,
}

fn catalog_output(profile: McpProfile) -> McpToolsOutput {
    let profile_name = profile
        .to_possible_value()
        .expect("every McpProfile variant maps to a CLI value")
        .get_name()
        .to_string();
    McpToolsOutput {
        version: env!("CARGO_PKG_VERSION"),
        profile: profile_name,
        tools: mcp::visible_catalog(profile),
    }
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
        McpCommand::Tools { profile } => {
            app.output.print_success(catalog_output(profile));
            Ok(())
        }
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

#[cfg(test)]
mod tests {
    use super::*;

    fn as_json(profile: McpProfile) -> serde_json::Value {
        serde_json::to_value(catalog_output(profile)).expect("catalog serializes")
    }

    #[test]
    fn full_listing_carries_exactly_version_profile_and_tools() {
        let output = as_json(McpProfile::Full);
        let keys: Vec<&str> = output
            .as_object()
            .expect("top-level object")
            .keys()
            .map(String::as_str)
            .collect();
        assert_eq!(keys, ["version", "profile", "tools"]);
        assert_eq!(output["version"], env!("CARGO_PKG_VERSION"));
        assert_eq!(output["profile"], "full");
    }

    #[test]
    fn listed_tools_reproduce_the_tools_list_wire_shape() {
        let output = as_json(McpProfile::Full);
        let tools = output["tools"].as_array().expect("tools array");
        assert!(!tools.is_empty());
        for tool in tools {
            assert!(tool["name"].is_string(), "tool without name: {tool}");
            assert!(
                tool["description"].is_string(),
                "{} lacks a description",
                tool["name"]
            );
            assert!(
                tool["inputSchema"].is_object(),
                "{} lacks an inputSchema",
                tool["name"]
            );
            assert!(
                tool["annotations"]["readOnlyHint"].is_boolean(),
                "{} lacks a typed mutation annotation",
                tool["name"]
            );
        }
    }

    #[test]
    fn read_only_listing_is_a_strict_read_only_subset() {
        let full = as_json(McpProfile::Full);
        let read_only = as_json(McpProfile::ReadOnly);
        assert_eq!(read_only["profile"], "read-only");

        let full_tools = full["tools"].as_array().expect("tools array");
        let visible = read_only["tools"].as_array().expect("tools array");
        assert!(visible.len() < full_tools.len());
        for tool in visible {
            assert_eq!(
                tool["annotations"]["readOnlyHint"], true,
                "{} leaked into the read-only listing",
                tool["name"]
            );
        }
    }
}
