use std::sync::Arc;

use anyhow::Result;
use rmcp::model::{
    CallToolRequestParams, CallToolResponse, CallToolResult, ContentBlock, Implementation,
    ListToolsResult, PaginatedRequestParams, ServerCapabilities, ServerConfig, Tool,
};
use rmcp::service::RequestContext;
use rmcp::{ErrorData, RoleServer, ServerHandler, ServiceExt};
use serde_json::Value;
use tokio_util::task::TaskTracker;

use super::{McpProfile, instructions, tools};
use crate::app::App;

/// The app each tool call runs in. A call is one command, so it runs under
/// the configuration as it reads when the call starts ([`App::reloaded`]),
/// and an app whose services a newer one does not share is shut down once no
/// call holds it.
pub(super) struct Apps {
    current: tokio::sync::Mutex<Arc<App>>,
    retired: std::sync::Mutex<Vec<Arc<App>>>,
    stopping: TaskTracker,
}

impl Apps {
    fn new(app: App) -> Self {
        Self {
            current: tokio::sync::Mutex::new(Arc::new(app)),
            retired: std::sync::Mutex::new(Vec::new()),
            stopping: TaskTracker::new(),
        }
    }

    async fn for_call(&self) -> Result<Arc<App>> {
        let mut current = self.current.lock().await;
        let next = Arc::new(current.reloaded().await?);
        let previous = std::mem::replace(&mut *current, Arc::clone(&next));
        drop(current);
        if !previous.shares_services_with(&next) {
            self.retired
                .lock()
                .expect("retired apps lock poisoned")
                .push(previous);
        }
        self.retire_unheld();
        Ok(next)
    }

    /// Shut down each retired app no call holds. None can take hold of one
    /// again, since calls are handed the current app alone.
    fn retire_unheld(&self) {
        let unheld: Vec<_> = self
            .retired
            .lock()
            .expect("retired apps lock poisoned")
            .extract_if(.., |app| Arc::strong_count(app) == 1)
            .collect();
        for app in unheld {
            self.stopping.spawn(async move { app.shutdown().await });
        }
    }

    /// Stop every language server an app of this server started, once the
    /// transport has stopped serving.
    pub(super) async fn shutdown(&self) {
        self.stopping.close();
        self.stopping.wait().await;
        let current = Arc::clone(&*self.current.lock().await);
        let retired =
            std::mem::take(&mut *self.retired.lock().expect("retired apps lock poisoned"));
        futures::future::join_all(
            std::iter::once(current)
                .chain(retired)
                .map(|app| async move { app.shutdown().await }),
        )
        .await;
    }
}

#[derive(Clone)]
pub(super) struct McpServer {
    apps: Arc<Apps>,
    profile: McpProfile,
}

impl McpServer {
    pub fn new(app: App, profile: McpProfile) -> Self {
        Self {
            apps: Arc::new(Apps::new(app)),
            profile,
        }
    }

    pub(super) fn apps(&self) -> Arc<Apps> {
        Arc::clone(&self.apps)
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
        let app = self
            .apps
            .for_call()
            .await
            .map_err(|e| ErrorData::internal_error(e.to_string(), None))?;
        let output = tokio::select! {
            output = tools::dispatch(&request.name, arguments, &app, self.profile) => output,
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
                let body = app.output.error_response(error);
                let mut result = CallToolResult::error(vec![ContentBlock::text(body.to_string())]);
                result.structured_content = Some(body);
                result
            }
        };
        drop(app);
        self.apps.retire_unheld();
        Ok(result.into())
    }
}

pub async fn serve_stdio(app: App, profile: McpProfile) -> Result<()> {
    let server = McpServer::new(app, profile);
    let apps = server.apps();
    let service = server.serve((stdin()?, tokio::io::stdout())).await?;
    let served = service.waiting().await;
    apps.shutdown().await;
    served?;
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::app::Wiring;
    use crate::cli::OutputOptions;

    /// A tool call runs under the configuration as it reads when the call
    /// starts, its errors included. An edit that leaves the language-server
    /// settings as they were keeps the services; one that changes them builds
    /// new ones, and the app they replace is shut down once no call holds it.
    #[tokio::test]
    async fn a_tool_call_runs_under_the_configuration_as_it_reads_at_its_start() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir(dir.path().join(".symora")).unwrap();
        let configure = |text: &str| {
            std::fs::write(dir.path().join(".symora/config.toml"), text).unwrap();
        };
        let app = App::new_at(
            dir.path().to_path_buf(),
            OutputOptions::default(),
            Wiring {
                use_daemon: false,
                deterministic: true,
                ..Default::default()
            },
        )
        .await
        .unwrap();
        let apps = Apps::new(app);

        configure("[output\n");
        assert!(
            !apps
                .for_call()
                .await
                .unwrap()
                .output
                .config_errors_snapshot()
                .is_empty()
        );
        configure("[output]\nmax_response_chars = 1234\n");
        let first = apps.for_call().await.unwrap();
        assert!(first.output.config_errors_snapshot().is_empty());
        assert_eq!(first.config().output.max_response_chars, 1234);
        assert!(apps.retired.lock().unwrap().is_empty());

        configure("[lsp]\ntimeout_secs = 7\n");
        let second = apps.for_call().await.unwrap();
        assert_eq!(second.config().lsp.timeout_secs, 7);
        assert!(!second.shares_services_with(&first));
        assert_eq!(apps.retired.lock().unwrap().len(), 1);
        drop(first);
        apps.retire_unheld();
        assert!(apps.retired.lock().unwrap().is_empty());
        drop(second);
        apps.shutdown().await;
    }
}
