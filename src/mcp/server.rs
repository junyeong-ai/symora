use std::sync::Arc;

use anyhow::Result;
use rmcp::model::{
    CallToolRequestParams, CallToolResponse, CallToolResult, ContentBlock, Implementation,
    ListToolsResult, PaginatedRequestParams, ServerCapabilities, ServerConfig, Tool,
};
use rmcp::service::RequestContext;
use rmcp::{ErrorData, RoleServer, ServerHandler, ServiceExt};
use serde_json::Value;

use super::{McpProfile, instructions, tools};
use crate::app::App;

#[derive(Clone)]
pub(super) struct McpServer {
    app: Arc<App>,
    profile: McpProfile,
}

impl McpServer {
    pub fn new(app: Arc<App>, profile: McpProfile) -> Self {
        Self { app, profile }
    }

    fn tool_definition(&self, name: &str) -> Option<Tool> {
        tools::catalog()
            .iter()
            .find(|tool| tool.name == name && self.profile.allows(tool))
            .map(Tool::from)
    }
}

impl ServerHandler for McpServer {
    fn get_info(&self) -> ServerConfig {
        ServerConfig::new(ServerCapabilities::builder().enable_tools().build())
            .with_server_info(Implementation::new("symora", env!("CARGO_PKG_VERSION")))
            .with_instructions(instructions::for_profile(self.profile))
    }

    fn get_tool(&self, name: &str) -> Option<Tool> {
        self.tool_definition(name)
    }

    async fn list_tools(
        &self,
        _request: Option<PaginatedRequestParams>,
        _context: RequestContext<RoleServer>,
    ) -> Result<ListToolsResult, ErrorData> {
        let tools = tools::catalog()
            .iter()
            .filter(|tool| self.profile.allows(tool))
            .map(Tool::from)
            .collect();
        Ok(ListToolsResult {
            tools,
            ..Default::default()
        })
    }

    async fn call_tool(
        &self,
        request: CallToolRequestParams,
        context: RequestContext<RoleServer>,
    ) -> Result<CallToolResponse, ErrorData> {
        let arguments = request.arguments.map_or(Value::Null, Value::Object);
        let output = tokio::select! {
            output = tools::dispatch(&request.name, arguments, &self.app, self.profile) => output,
            _ = context.ct.cancelled() => return Err(ErrorData::internal_error("Tool call cancelled", None)),
        };
        let result = match output {
            Ok(output) => {
                let mut result = CallToolResult::success(output.content);
                result.is_error = Some(output.is_error);
                result.structured_content = output.structured;
                result
            }
            Err(error) => {
                let body = self.app.output.error_response(error);
                let mut result = CallToolResult::error(vec![ContentBlock::text(body.to_string())]);
                result.structured_content = Some(body);
                result
            }
        };
        Ok(result.into())
    }
}

pub async fn serve_stdio(app: App, profile: McpProfile) -> Result<()> {
    let service = McpServer::new(Arc::new(app), profile)
        .serve((stdin()?, tokio::io::stdout()))
        .await?;
    service.waiting().await?;
    Ok(())
}

/// Standard input, read on a thread of its own rather than on the runtime's
/// blocking pool. A read of stdin cannot be cancelled, and dropping the
/// runtime waits for its blocking work, so a read pending there would hold
/// the exit until the client wrote again or closed the stream; a thread
/// outside the runtime ends with the process.
fn stdin() -> std::io::Result<impl tokio::io::AsyncRead + Send + Unpin + 'static> {
    use std::io::Read;

    let (sender, mut chunks) = tokio::sync::mpsc::channel(16);
    std::thread::Builder::new()
        .name("mcp-stdin".into())
        .spawn(move || {
            let mut stdin = std::io::stdin().lock();
            let mut buffer = vec![0; 64 * 1024];
            loop {
                let chunk = match stdin.read(&mut buffer) {
                    Ok(0) => return,
                    Ok(read) => Ok(std::io::Cursor::new(buffer[..read].to_vec())),
                    Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
                    Err(e) => Err(e),
                };
                let failed = chunk.is_err();
                if sender.blocking_send(chunk).is_err() || failed {
                    return;
                }
            }
        })?;
    Ok(tokio_util::io::StreamReader::new(futures::stream::poll_fn(
        move |cx| chunks.poll_recv(cx),
    )))
}
