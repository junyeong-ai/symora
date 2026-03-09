use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::UnixStream;
use tokio::process::Command;
use tokio::time::timeout;

use crate::config::LspRuntimeConfig;
use crate::daemon::protocol::{Request, Response, methods};
use crate::daemon::server::DaemonRuntimeConfig;
use crate::error::LspError;
use crate::models::symbol::Language;

fn calculate_timeout(config: &LspRuntimeConfig, file: Option<&Path>, method: &str) -> Duration {
    // Daemon-only operations with fixed timeouts
    match method {
        methods::PING | methods::STATUS | methods::SHUTDOWN | methods::INVALIDATE_FILE => {
            return Duration::from_secs(30);
        }
        methods::INDEX_BUILD => return Duration::from_secs(600),
        methods::INDEX_CLEAR | methods::INDEX_STATUS => return Duration::from_secs(120),
        methods::SEARCH_SYMBOLS | methods::SEARCH_CONTENT => return Duration::from_secs(60),
        _ => {}
    }

    // Determine language from file path
    let language = file.map(Language::from_path).unwrap_or(Language::Unknown);

    // Map daemon method to LSP method for config lookup
    let lsp_method = methods::to_lsp_method(method).unwrap_or("textDocument/hover");

    config.timeout_for(language, lsp_method)
}

/// Calculate timeout for operations where language is known but file path is not.
fn calculate_timeout_for_language(
    config: &LspRuntimeConfig,
    language: Language,
    method: &str,
) -> Duration {
    let lsp_method = methods::to_lsp_method(method).unwrap_or("textDocument/hover");
    config.timeout_for(language, lsp_method)
}

/// Daemon client for CLI commands
pub struct DaemonClient {
    config: DaemonRuntimeConfig,
    lsp_config: std::sync::Arc<LspRuntimeConfig>,
    project_root: PathBuf,
    next_request_id: AtomicU64,
}

// Macros for RPC Method Generation

/// Generates position-based RPC methods (file, line, column)
macro_rules! rpc_position {
    ($($name:ident => $method:expr),* $(,)?) => {
        $(
            pub async fn $name(
                &self,
                file: &Path,
                line: u32,
                column: u32,
            ) -> Result<serde_json::Value, LspError> {
                self.ensure_running().await?;
                let params = serde_json::json!({
                    "file": file.display().to_string(),
                    "line": line,
                    "column": column
                });
                self.request_with_project($method, params, Some(file))
                    .await
                    .and_then(Self::extract_result)
            }
        )*
    };
}

/// Generates file-only RPC methods
macro_rules! rpc_file {
    ($($name:ident => $method:expr),* $(,)?) => {
        $(
            pub async fn $name(&self, file: &Path) -> Result<serde_json::Value, LspError> {
                self.ensure_running().await?;
                let params = serde_json::json!({
                    "file": file.display().to_string()
                });
                self.request_with_project($method, params, Some(file))
                    .await
                    .and_then(Self::extract_result)
            }
        )*
    };
}

impl DaemonClient {
    /// Create a new daemon client
    pub fn new(project_root: &Path) -> Self {
        Self {
            config: DaemonRuntimeConfig::load(project_root),
            lsp_config: DaemonRuntimeConfig::load_lsp_config(project_root),
            project_root: project_root.to_path_buf(),
            next_request_id: AtomicU64::new(1),
        }
    }

    // Connection Management

    /// Ensure daemon is running, starting it if necessary
    pub async fn ensure_running(&self) -> Result<(), LspError> {
        if self.ping().await.is_ok() {
            return Ok(());
        }
        self.start_daemon_with_lock().await
    }

    async fn start_daemon_with_lock(&self) -> Result<(), LspError> {
        use std::fs::OpenOptions;

        if let Some(parent) = self.config.lock_path.parent() {
            std::fs::create_dir_all(parent).map_err(|e| {
                LspError::ServerStart(format!("Failed to create daemon directory: {}", e))
            })?;
        }

        let lock_file = OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(false)
            .open(&self.config.lock_path)
            .map_err(|e| LspError::ServerStart(format!("Failed to open lock file: {}", e)))?;

        if Self::try_lock_exclusive(&lock_file) {
            if self.ping().await.is_ok() {
                Self::unlock(&lock_file);
                return Ok(());
            }

            let result = self.spawn_daemon();
            Self::unlock(&lock_file);
            result?;
        } else {
            tracing::debug!("Another process is starting daemon, waiting...");
        }

        self.wait_for_daemon(Duration::from_secs(10)).await
    }

    #[cfg(unix)]
    fn try_lock_exclusive(file: &std::fs::File) -> bool {
        use std::os::unix::io::AsRawFd;
        // SAFETY: flock with a valid fd is safe; LOCK_NB makes it non-blocking
        unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) == 0 }
    }

    #[cfg(not(unix))]
    fn try_lock_exclusive(_file: &std::fs::File) -> bool {
        true
    }

    #[cfg(unix)]
    fn unlock(file: &std::fs::File) {
        use std::os::unix::io::AsRawFd;
        unsafe {
            libc::flock(file.as_raw_fd(), libc::LOCK_UN);
        }
    }

    #[cfg(not(unix))]
    fn unlock(_file: &std::fs::File) {}

    fn spawn_daemon(&self) -> Result<(), LspError> {
        let exe = std::env::current_exe()
            .map_err(|e| LspError::ServerStart(format!("Failed to get executable path: {}", e)))?;

        let child = Command::new(&exe)
            .arg("daemon")
            .arg("start")
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .map_err(|e| LspError::ServerStart(format!("Failed to spawn daemon: {}", e)))?;

        drop(child);
        tracing::info!("Daemon process spawned");
        Ok(())
    }

    async fn wait_for_daemon(&self, max_wait: Duration) -> Result<(), LspError> {
        let start = std::time::Instant::now();
        let poll_interval = Duration::from_millis(100);

        while start.elapsed() < max_wait {
            if self.ping().await.is_ok() {
                tracing::debug!("Daemon is ready after {:?}", start.elapsed());
                return Ok(());
            }
            tokio::time::sleep(poll_interval).await;
        }

        Err(LspError::Timeout(
            "Daemon failed to start within timeout".to_string(),
        ))
    }

    async fn ping(&self) -> Result<(), LspError> {
        let response = self
            .send_request(methods::PING, None, Duration::from_secs(30))
            .await?;
        if response.error.is_some() {
            return Err(LspError::Protocol("Ping failed".to_string()));
        }
        Ok(())
    }

    // Request Infrastructure

    async fn send_request(
        &self,
        method: &str,
        params: Option<serde_json::Value>,
        timeout_duration: Duration,
    ) -> Result<Response, LspError> {
        let stream = UnixStream::connect(&self.config.socket_path)
            .await
            .map_err(|_| LspError::NotConnected)?;

        let (reader, mut writer) = stream.into_split();
        let mut reader = BufReader::new(reader);

        let id = self.next_request_id.fetch_add(1, Ordering::Relaxed);
        let request = Request::new(id, method, params);
        let request_json = serde_json::to_string(&request)?;

        writer.write_all(request_json.as_bytes()).await?;
        writer.write_all(b"\n").await?;
        writer.flush().await?;

        let mut line = String::new();
        timeout(timeout_duration, reader.read_line(&mut line))
            .await
            .map_err(|_| {
                LspError::Timeout(format!(
                    "Operation '{}' timed out after {}s. Try 'symora daemon restart'",
                    method,
                    timeout_duration.as_secs()
                ))
            })??;

        Ok(serde_json::from_str(&line)?)
    }

    fn inject_project(&self, params: &mut serde_json::Value) {
        if let Some(obj) = params.as_object_mut() {
            obj.insert(
                "project".to_string(),
                serde_json::Value::String(self.project_root.display().to_string()),
            );
        }
    }

    async fn request_with_project(
        &self,
        method: &str,
        mut params: serde_json::Value,
        file: Option<&Path>,
    ) -> Result<Response, LspError> {
        self.inject_project(&mut params);
        let timeout = calculate_timeout(&self.lsp_config, file, method);
        self.send_request(method, Some(params), timeout).await
    }

    async fn request_with_project_timeout(
        &self,
        method: &str,
        mut params: serde_json::Value,
        timeout: Duration,
    ) -> Result<Response, LspError> {
        self.inject_project(&mut params);
        self.send_request(method, Some(params), timeout).await
    }

    fn extract_result(response: Response) -> Result<serde_json::Value, LspError> {
        if let Some(error) = response.error {
            return Err(LspError::server_error_friendly(error.code, error.message));
        }
        response
            .result
            .ok_or_else(|| LspError::Protocol("Empty response".to_string()))
    }

    // Position-based LSP Operations (file, line, column)

    rpc_position! {
        find_references => methods::FIND_REFERENCES,
        goto_definition => methods::GOTO_DEFINITION,
        goto_type_definition => methods::GOTO_TYPE_DEFINITION,
        find_implementations => methods::FIND_IMPLEMENTATIONS,
        hover => methods::HOVER,
        signature_help => methods::SIGNATURE_HELP,
        incoming_calls => methods::INCOMING_CALLS,
        outgoing_calls => methods::OUTGOING_CALLS,
        supertypes => methods::SUPERTYPES,
        subtypes => methods::SUBTYPES,
        prepare_rename => methods::PREPARE_RENAME,
        code_actions => methods::CODE_ACTIONS,
    }

    // File-based LSP Operations

    rpc_file! {
        diagnostics => methods::DIAGNOSTICS,
        folding_ranges => methods::FOLDING_RANGES,
        code_lenses => methods::CODE_LENSES,
        format => methods::FORMAT,
    }

    // Custom Parameter Operations

    pub async fn find_symbols(
        &self,
        file: &Path,
        include_body: bool,
        depth: u32,
    ) -> Result<serde_json::Value, LspError> {
        self.ensure_running().await?;
        let params = serde_json::json!({
            "file": file.display().to_string(),
            "body": include_body,
            "depth": depth
        });
        self.request_with_project(methods::FIND_SYMBOLS, params, Some(file))
            .await
            .and_then(Self::extract_result)
    }

    pub async fn rename(
        &self,
        file: &Path,
        line: u32,
        column: u32,
        new_name: &str,
    ) -> Result<serde_json::Value, LspError> {
        self.ensure_running().await?;
        let params = serde_json::json!({
            "file": file.display().to_string(),
            "line": line,
            "column": column,
            "new_name": new_name
        });
        self.request_with_project(methods::RENAME, params, Some(file))
            .await
            .and_then(Self::extract_result)
    }

    pub async fn inlay_hints(
        &self,
        file: &Path,
        start_line: u32,
        start_column: u32,
        end_line: u32,
        end_column: u32,
    ) -> Result<serde_json::Value, LspError> {
        self.ensure_running().await?;
        let params = serde_json::json!({
            "file": file.display().to_string(),
            "start_line": start_line,
            "start_column": start_column,
            "end_line": end_line,
            "end_column": end_column
        });
        self.request_with_project(methods::INLAY_HINTS, params, Some(file))
            .await
            .and_then(Self::extract_result)
    }

    pub async fn selection_ranges(
        &self,
        file: &Path,
        positions: &[(u32, u32)],
    ) -> Result<serde_json::Value, LspError> {
        self.ensure_running().await?;
        let params = serde_json::json!({
            "file": file.display().to_string(),
            "positions": positions.iter()
                .map(|(l, c)| serde_json::json!({"line": l, "column": c}))
                .collect::<Vec<_>>()
        });
        self.request_with_project(methods::SELECTION_RANGES, params, Some(file))
            .await
            .and_then(Self::extract_result)
    }

    pub async fn workspace_symbols(
        &self,
        query: &str,
        language: &str,
    ) -> Result<serde_json::Value, LspError> {
        self.ensure_running().await?;
        let language_enum = Language::parse_or_default(language);
        let params = serde_json::json!({
            "query": query,
            "language": language
        });
        let timeout = calculate_timeout_for_language(
            &self.lsp_config,
            language_enum,
            methods::WORKSPACE_SYMBOLS,
        );
        self.request_with_project_timeout(methods::WORKSPACE_SYMBOLS, params, timeout)
            .await
            .and_then(Self::extract_result)
    }

    pub async fn apply_code_action(
        &self,
        file: &Path,
        action: &serde_json::Value,
    ) -> Result<serde_json::Value, LspError> {
        self.ensure_running().await?;
        let params = serde_json::json!({
            "file": file.display().to_string(),
            "action": action
        });
        self.request_with_project(methods::APPLY_CODE_ACTION, params, Some(file))
            .await
            .and_then(Self::extract_result)
    }

    pub async fn language_status(&self, language: &str) -> Result<serde_json::Value, LspError> {
        self.ensure_running().await?;
        let params = serde_json::json!({
            "language": language
        });
        self.request_with_project(methods::LANGUAGE_STATUS, params, None)
            .await
            .and_then(Self::extract_result)
    }

    // Search Operations

    pub async fn search_symbols(
        &self,
        query: &str,
        limit: Option<usize>,
        kind: Option<&str>,
    ) -> Result<serde_json::Value, LspError> {
        self.ensure_running().await?;
        let params = serde_json::json!({
            "query": query,
            "limit": limit,
            "kind": kind,
        });
        self.request_with_project(methods::SEARCH_SYMBOLS, params, None)
            .await
            .and_then(Self::extract_result)
    }

    pub async fn search_content(
        &self,
        query: &str,
        limit: Option<usize>,
        language: Option<&str>,
    ) -> Result<serde_json::Value, LspError> {
        self.ensure_running().await?;
        let params = serde_json::json!({
            "query": query,
            "limit": limit,
            "language": language,
        });
        self.request_with_project(methods::SEARCH_CONTENT, params, None)
            .await
            .and_then(Self::extract_result)
    }

    pub async fn index_build(
        &self,
        force: bool,
        languages: Option<Vec<String>>,
    ) -> Result<serde_json::Value, LspError> {
        self.ensure_running().await?;
        let params = serde_json::json!({
            "force": force,
            "languages": languages,
        });
        self.request_with_project(methods::INDEX_BUILD, params, None)
            .await
            .and_then(Self::extract_result)
    }

    pub async fn index_status(&self) -> Result<serde_json::Value, LspError> {
        self.ensure_running().await?;
        self.request_with_project(methods::INDEX_STATUS, serde_json::json!({}), None)
            .await
            .and_then(Self::extract_result)
    }

    pub async fn index_clear(&self) -> Result<serde_json::Value, LspError> {
        self.ensure_running().await?;
        self.request_with_project(methods::INDEX_CLEAR, serde_json::json!({}), None)
            .await
            .and_then(Self::extract_result)
    }

    // Store Operations

    /// Best-effort file invalidation in the store index.
    /// Does not start the daemon if not running.
    /// Uses a short 2-second ping timeout to avoid blocking edit workflows.
    pub async fn invalidate_file(&self, file: &Path) -> Result<(), LspError> {
        let is_running = timeout(Duration::from_secs(2), self.ping())
            .await
            .map(|r| r.is_ok())
            .unwrap_or(false);
        if !is_running {
            return Ok(()); // Daemon not running or slow, nothing to invalidate
        }
        let params = serde_json::json!({
            "file": file.display().to_string()
        });
        if let Err(e) = self
            .request_with_project(methods::INVALIDATE_FILE, params, Some(file))
            .await
        {
            tracing::warn!("Failed to invalidate file {}: {}", file.display(), e);
        }
        Ok(())
    }

    // Daemon Control Operations

    pub async fn status(&self) -> Result<serde_json::Value, LspError> {
        self.send_request(methods::STATUS, None, Duration::from_secs(30))
            .await
            .and_then(Self::extract_result)
    }

    pub async fn shutdown(&self) -> Result<(), LspError> {
        if let Err(e) = self
            .send_request(methods::SHUTDOWN, None, Duration::from_secs(30))
            .await
        {
            tracing::warn!("Shutdown request failed (may already be stopped): {}", e);
        }
        self.wait_for_shutdown().await
    }

    async fn wait_for_shutdown(&self) -> Result<(), LspError> {
        let start = std::time::Instant::now();
        let max_wait = Duration::from_secs(5);

        while start.elapsed() < max_wait {
            if !self.config.socket_path.exists() {
                tracing::debug!("Daemon shutdown confirmed");
                return Ok(());
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }

        tracing::warn!("Daemon may not have shutdown cleanly");
        Ok(())
    }
}
