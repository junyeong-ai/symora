//! Daemon Client Implementation
//!
//! Connects to the daemon server over Unix socket.
//! Implements auto-start with proper race condition handling via file locking.

use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::UnixStream;
use tokio::process::Command;
use tokio::time::timeout;

use crate::daemon::protocol::{Request, Response, methods};
use crate::daemon::server::DaemonConfig;
use crate::error::LspError;

/// Daemon client for CLI commands
pub struct DaemonClient {
    config: DaemonConfig,
    project_root: PathBuf,
    next_request_id: AtomicU64,
}

// ============================================================================
// Macros for RPC Method Generation
// ============================================================================

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
                self.request_with_project($method, params).await.and_then(Self::extract_result)
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
                self.request_with_project($method, params).await.and_then(Self::extract_result)
            }
        )*
    };
}

impl DaemonClient {
    /// Create a new daemon client
    pub fn new(project_root: &Path) -> Self {
        Self {
            config: DaemonConfig::default(),
            project_root: project_root.to_path_buf(),
            next_request_id: AtomicU64::new(1),
        }
    }

    // ========================================================================
    // Connection Management
    // ========================================================================

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
        let response = self.send_request(methods::PING, None).await?;
        if response.error.is_some() {
            return Err(LspError::Protocol("Ping failed".to_string()));
        }
        Ok(())
    }

    // ========================================================================
    // Request Infrastructure
    // ========================================================================

    async fn send_request(
        &self,
        method: &str,
        params: Option<serde_json::Value>,
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
        timeout(Duration::from_secs(30), reader.read_line(&mut line))
            .await
            .map_err(|_| {
                LspError::Timeout(format!(
                    "Operation '{}' timed out after 30s. Try 'symora daemon restart'",
                    method
                ))
            })??;

        Ok(serde_json::from_str(&line)?)
    }

    async fn request_with_project(
        &self,
        method: &str,
        mut params: serde_json::Value,
    ) -> Result<Response, LspError> {
        if let Some(obj) = params.as_object_mut() {
            obj.insert(
                "project".to_string(),
                serde_json::Value::String(self.project_root.display().to_string()),
            );
        }
        self.send_request(method, Some(params)).await
    }

    fn extract_result(response: Response) -> Result<serde_json::Value, LspError> {
        if let Some(error) = response.error {
            return Err(LspError::server_error_friendly(error.code, error.message));
        }
        response
            .result
            .ok_or_else(|| LspError::Protocol("Empty response".to_string()))
    }

    // ========================================================================
    // Position-based LSP Operations (file, line, column)
    // ========================================================================

    rpc_position! {
        find_references => methods::FIND_REFS,
        goto_definition => methods::FIND_DEF,
        goto_type_definition => methods::FIND_TYPEDEF,
        find_implementations => methods::FIND_IMPL,
        hover => methods::HOVER,
        signature_help => methods::SIGNATURE_HELP,
        incoming_calls => methods::CALLS_INCOMING,
        outgoing_calls => methods::CALLS_OUTGOING,
        supertypes => methods::SUPERTYPES,
        subtypes => methods::SUBTYPES,
        prepare_rename => methods::PREPARE_RENAME,
        code_actions => methods::CODE_ACTIONS,
    }

    // ========================================================================
    // File-based LSP Operations
    // ========================================================================

    rpc_file! {
        diagnostics => methods::DIAGNOSTICS,
        folding_ranges => methods::FOLDING_RANGES,
        code_lens => methods::CODE_LENS,
    }

    // ========================================================================
    // Custom Parameter Operations
    // ========================================================================

    pub async fn find_symbols_with_options(
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
        self.request_with_project(methods::FIND_SYMBOL, params)
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
        self.request_with_project(methods::RENAME, params)
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
        self.request_with_project(methods::INLAY_HINTS, params)
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
        self.request_with_project(methods::SELECTION_RANGES, params)
            .await
            .and_then(Self::extract_result)
    }

    pub async fn workspace_symbols(
        &self,
        query: &str,
        language: &str,
    ) -> Result<serde_json::Value, LspError> {
        self.ensure_running().await?;
        let params = serde_json::json!({
            "query": query,
            "language": language
        });
        self.request_with_project(methods::WORKSPACE_SYMBOL, params)
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
        self.request_with_project(methods::APPLY_CODE_ACTION, params)
            .await
            .and_then(Self::extract_result)
    }

    // ========================================================================
    // Daemon Control Operations
    // ========================================================================

    pub async fn status(&self) -> Result<serde_json::Value, LspError> {
        self.send_request(methods::STATUS, None)
            .await
            .and_then(Self::extract_result)
    }

    pub async fn shutdown(&self) -> Result<(), LspError> {
        let _ = self.send_request(methods::SHUTDOWN, None).await;
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
