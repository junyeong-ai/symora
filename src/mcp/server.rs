use std::sync::Arc;

use anyhow::Result;
use rmcp::model::{
    CallToolRequestParams, CallToolResponse, CallToolResult, ContentBlock, Implementation,
    ListToolsResult, PaginatedRequestParams, ServerCapabilities, ServerConfig, Tool,
};
use rmcp::service::RequestContext;
use rmcp::{ErrorData, RoleServer, ServerHandler, ServiceExt};
use serde_json::{Value, json};

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
                let body = json!({ "error": error });
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
        .serve(rmcp::transport::stdio())
        .await?;
    service.waiting().await?;
    Ok(())
}
