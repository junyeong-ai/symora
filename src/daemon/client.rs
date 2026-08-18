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
use crate::daemon::proves_no_listener;
use crate::daemon::server::DaemonRuntimeConfig;
use crate::error::LspError;
use crate::infra::file_lock::FileLock;
use crate::models::symbol::Language;

fn calculate_timeout(config: &LspRuntimeConfig, file: Option<&Path>, method: &str) -> Duration {
    // Daemon-only operations with fixed timeouts
    match method {
        methods::PING
        | methods::STATUS
        | methods::SHUTDOWN
        | methods::REFRESH_FILES
        | methods::NOTE_FILES_EDITED => {
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

/// What [`DaemonClient::ensure_running`] found, and did about it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DaemonStart {
    /// A daemon of this binary was already serving.
    AlreadyRunning,
    /// None was serving; one was started.
    Started,
    /// A daemon from a different binary was serving and was replaced.
    Replaced,
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

    /// Ensure a daemon of *this* binary is running, starting or replacing
    /// one as necessary — the single owner of daemon lifecycle, so no
    /// other path may spawn one. A daemon left over from a different
    /// binary is replaced: the wire format is guaranteed only within one
    /// build.
    pub async fn ensure_running(&self) -> Result<DaemonStart, LspError> {
        if matches!(self.ping().await, Ok(true)) {
            return Ok(DaemonStart::AlreadyRunning);
        }
        self.start_daemon_with_lock().await
    }

    async fn start_daemon_with_lock(&self) -> Result<DaemonStart, LspError> {
        let lock = FileLock::exclusive(&self.config.lock_path)
            .map_err(|e| LspError::ServerStart(format!("Failed to open lock file: {e}")))?;
        match lock {
            Some(_lock) => self.start_daemon_locked().await,
            None => {
                tracing::debug!("Another process is starting daemon, waiting...");
                self.wait_for_daemon(Duration::from_secs(10))
                    .await
                    .map(|()| DaemonStart::Started)
            }
        }
    }

    /// The whole sequence — re-check, replace, spawn, wait — runs under the
    /// startup lock. Releasing it at spawn time would let the next process
    /// find no daemon yet, start a second one, and have that one take the
    /// socket out from under the first.
    async fn start_daemon_locked(&self) -> Result<DaemonStart, LspError> {
        let replaced = match self.ping().await {
            Ok(true) => return Ok(DaemonStart::AlreadyRunning),
            Ok(false) => {
                tracing::info!("Daemon binary differs from CLI; replacing daemon");
                self.shutdown().await?;
                true
            }
            // Nothing is listening — the only failure that licenses a spawn.
            // Anything else left the question open, and starting a second
            // daemon would displace a live one's socket.
            Err(LspError::NotConnected) => false,
            Err(e) => return Err(e),
        };

        self.spawn_daemon()?;
        self.wait_for_daemon(Duration::from_secs(10)).await?;
        Ok(if replaced {
            DaemonStart::Replaced
        } else {
            DaemonStart::Started
        })
    }

    fn spawn_daemon(&self) -> Result<(), LspError> {
        let exe = std::env::current_exe()
            .map_err(|e| LspError::ServerStart(format!("Failed to get executable path: {}", e)))?;

        let child = Command::new(&exe)
            .current_dir(&self.project_root)
            .arg("daemon")
            .arg("serve")
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
            if matches!(self.ping().await, Ok(true)) {
                tracing::debug!("Daemon is ready after {:?}", start.elapsed());
                return Ok(());
            }
            tokio::time::sleep(poll_interval).await;
        }

        Err(LspError::Timeout(
            "Daemon failed to start within timeout".to_string(),
        ))
    }

    /// Ping the daemon. `Ok(true)` means it is alive *and* running the
    /// same binary as this client — version and build token both — so a
    /// daemon left over from an earlier build of the same version is
    /// restarted before any wire exchange; `Ok(false)` means alive but
    /// from a different binary.
    async fn ping(&self) -> Result<bool, LspError> {
        let response = self
            .send_request(methods::PING, None, Duration::from_secs(30))
            .await?;
        if response.error.is_some() {
            return Err(LspError::Protocol("Ping failed".to_string()));
        }
        let field = |key: &str| {
            response
                .result
                .as_ref()
                .and_then(|r| r.get(key))
                .and_then(|v| v.as_str())
                .map(str::to_owned)
        };
        // Fail closed: a daemon that reports no build identity is an older
        // binary and never passes. A mismatched wire is a silent wrong
        // answer; a replaced daemon is at worst a loud restart.
        let same_binary = field("version").is_some_and(|v| v == env!("CARGO_PKG_VERSION"))
            && field("build").is_some_and(|b| b == crate::daemon::protocol::BUILD_ID);
        Ok(same_binary)
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
            .map_err(|e| {
                if proves_no_listener(&e) {
                    LspError::NotConnected
                } else {
                    LspError::Io(e)
                }
            })?;

        let (reader, mut writer) = stream.into_split();
        let mut reader = BufReader::new(reader);

        let id = self.next_request_id.fetch_add(1, Ordering::Relaxed);
        let request = Request::new(id, method, params);
        let request_json = serde_json::to_string(&request)?;

        writer.write_all(request_json.as_bytes()).await?;
        writer.write_all(b"\n").await?;
        writer.flush().await?;

        let mut line = String::new();
        let read = timeout(timeout_duration, reader.read_line(&mut line))
            .await
            .map_err(|_| {
                LspError::Timeout(format!(
                    "Operation '{}' timed out after {}s. Try 'symora daemon restart'",
                    method,
                    timeout_duration.as_secs()
                ))
            })??;

        // End of stream: the daemon closed without answering — it exited or
        // was replaced while this request was in flight. Parsing the empty
        // read would blame the payload for the connection, and send the
        // caller after a malformed response that was never sent. It is not
        // `NotConnected` either: this connection was accepted, so all it
        // proves is that this peer stopped answering, and a replacement may
        // already be serving the socket.
        if read == 0 {
            return Err(LspError::ConnectionLost(method.to_string()));
        }

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

    /// An error the daemon typed as an [`LspError`] carries its variant in
    /// `data` and is reconstructed from it. Anything else — a store error, a
    /// JSON-RPC framing failure — did not come from a language server, so it
    /// is carried through verbatim: `server_error_friendly` rewrites messages
    /// by matching language-server prose, and applied here it would restate
    /// an unrelated failure in the vocabulary of positions and documents.
    pub(crate) fn extract_result(response: Response) -> Result<serde_json::Value, LspError> {
        if let Some(error) = response.error {
            if let Some(data) = error.data
                && let Ok(wire) = serde_json::from_value::<super::wire_error::WireLspError>(data)
            {
                return Err(wire.into());
            }
            return Err(LspError::ServerError {
                code: error.code,
                message: error.message,
            });
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
        end_line: u32,
    ) -> Result<serde_json::Value, LspError> {
        self.ensure_running().await?;
        let params = serde_json::json!({
            "file": file.display().to_string(),
            "start_line": start_line,
            "end_line": end_line
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

    /// Tell the daemon symora just wrote these files, so its LSP layer
    /// (symbol caches, workspace generation, live overlays) catches up.
    /// Best-effort with the same gating as `refresh_files`: a daemon that
    /// is not running has no caches or overlays to catch up, so this never
    /// starts one.
    pub async fn note_files_edited(&self, files: &[PathBuf]) -> Result<(), LspError> {
        let Some(params) = self.edited_files_params(files).await else {
            return Ok(());
        };
        self.request_with_project(
            methods::NOTE_FILES_EDITED,
            params,
            files.first().map(PathBuf::as_path),
        )
        .await
        .and_then(Self::extract_result)
        .map(|_| ())
    }

    // Search Operations

    pub async fn search_symbols(
        &self,
        query: &str,
        limit: Option<usize>,
        kind: Option<&str>,
        language: Option<&str>,
    ) -> Result<serde_json::Value, LspError> {
        self.ensure_running().await?;
        let params = serde_json::json!({
            "query": query,
            "limit": limit,
            "kind": kind,
            "language": language,
        });
        self.request_with_project(methods::SEARCH_SYMBOLS, params, None)
            .await
            .and_then(Self::extract_result)
    }

    pub async fn search_content(
        &self,
        query: &str,
        limit: Option<usize>,
        languages: &[String],
    ) -> Result<serde_json::Value, LspError> {
        self.ensure_running().await?;
        let params = serde_json::json!({
            "query": query,
            "limit": limit,
            "languages": languages,
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

    pub async fn indexed_languages(&self) -> Result<serde_json::Value, LspError> {
        self.ensure_running().await?;
        self.request_with_project(methods::INDEXED_LANGUAGES, serde_json::json!({}), None)
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

    /// Shared gating for the post-edit batch calls: only a live,
    /// same-version daemon gets them — the wire format is guaranteed
    /// within one version, and a best-effort note is not worth replacing
    /// a stale daemon over. Uses a short 2-second ping timeout to avoid
    /// blocking edit workflows. `None` means "skip silently".
    async fn edited_files_params(&self, files: &[PathBuf]) -> Option<serde_json::Value> {
        if files.is_empty() {
            return None;
        }
        let same_version_daemon = matches!(
            timeout(Duration::from_secs(2), self.ping()).await,
            Ok(Ok(true))
        );
        if !same_version_daemon {
            return None; // Not running, slow, or a different binary.
        }
        Some(serde_json::json!({
            "files": files
                .iter()
                .map(|f| f.display().to_string())
                .collect::<Vec<_>>(),
        }))
    }

    /// Re-index just-edited files in the daemon's store. Does not start a
    /// daemon; a refresh failure from a running daemon is returned so the
    /// edit layer can log the disclosed warn (the edit already succeeded).
    pub async fn refresh_files(&self, files: &[PathBuf]) -> Result<(), LspError> {
        let Some(params) = self.edited_files_params(files).await else {
            return Ok(());
        };
        self.request_with_project(
            methods::REFRESH_FILES,
            params,
            files.first().map(PathBuf::as_path),
        )
        .await
        .and_then(Self::extract_result)
        .map(|_| ())
    }

    // Daemon Control Operations

    pub async fn status(&self) -> Result<serde_json::Value, LspError> {
        self.send_request(methods::STATUS, None, Duration::from_secs(30))
            .await
            .and_then(Self::extract_result)
    }

    /// Stop the running daemon. `Ok(false)` means the request reached no
    /// daemon — there was nothing to stop, which callers report rather
    /// than dress up as a stop that happened.
    pub async fn shutdown(&self) -> Result<bool, LspError> {
        let reached = match self
            .send_request(methods::SHUTDOWN, None, Duration::from_secs(30))
            .await
        {
            Ok(_) => true,
            Err(LspError::NotConnected) => false,
            Err(e) => return Err(e),
        };
        self.wait_for_shutdown().await?;
        Ok(reached)
    }

    /// Wait until nothing answers on the socket. Only a refused or absent
    /// socket confirms the daemon is gone; a connection that fails for any
    /// other reason leaves the question open and is reported as itself,
    /// because the caller's next move is to start a replacement.
    async fn wait_for_shutdown(&self) -> Result<(), LspError> {
        let start = std::time::Instant::now();
        let max_wait = Duration::from_secs(5);

        while start.elapsed() < max_wait {
            match UnixStream::connect(&self.config.socket_path).await {
                Ok(_) => {}
                Err(e) if proves_no_listener(&e) => {
                    tracing::debug!("Daemon shutdown confirmed");
                    return Ok(());
                }
                Err(e) => return Err(LspError::Io(e)),
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }

        // Starting a replacement now would hand it a socket the old daemon
        // still owns, and it would exit rather than serve.
        Err(LspError::Timeout(
            "Daemon did not stop within timeout".to_string(),
        ))
    }
}
