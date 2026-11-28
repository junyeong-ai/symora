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

/// The part of a caller's budget kept back from the daemon for its answer
/// to come back in. A daemon given all of it runs out together with the
/// caller, whose own wait then ends first, and the daemon's account of the
/// timeout — what it saw of the server meanwhile — never arrives.
const ANSWER_MARGIN: Duration = Duration::from_millis(250);

/// How long the client waits for a method's answer; `None` waits for as long
/// as the daemon takes, which a daemon that exits still ends, as a lost
/// connection.
pub(super) fn calculate_timeout(config: &LspRuntimeConfig, method: &str) -> Option<Duration> {
    // Daemon-only operations with fixed timeouts
    match method {
        methods::PING
        | methods::STATUS
        | methods::SHUTDOWN
        | methods::RESTART_PROJECT
        | methods::REFRESH_FILES
        | methods::NOTE_FILES_EDITED => {
            return Some(Duration::from_secs(30));
        }
        // A build spends its caller's budget where one is in force — a
        // symbol search's — and has none otherwise: `search index build`
        // runs to completion, as it does in process.
        methods::INDEX_BUILD => {
            return crate::infra::deadline::current().map(|_| config.request_timeout());
        }
        methods::INDEX_CLEAR | methods::INDEX_STATUS => return Some(Duration::from_secs(120)),
        methods::SEARCH_SYMBOLS | methods::SEARCH_CONTENT => return Some(Duration::from_secs(60)),
        methods::TREE_CURRENCY | methods::INDEXED_LANGUAGES => {
            return Some(config.request_timeout());
        }
        _ => {}
    }

    Some(
        crate::daemon::lsp_request_timeout(config, method)
            .expect("LSP request method must have a mapping"),
    )
}

/// Whether a method spends the caller's shared budget: its wait and the
/// daemon's start consume the caller's remaining allowance, which the request
/// carries to the daemon. Every LSP request does; an index build does where
/// its caller has a budget to spend.
fn spends_caller_budget(method: &str) -> bool {
    methods::to_lsp_method(method).is_some() || method == methods::INDEX_BUILD
}

tokio::task_local! {
    /// The daemon instance that answered the running command's first request
    /// that binds ([`methods::binds_to_instance`]).
    static ANSWERED_BY: std::sync::OnceLock<String>;
}

/// Run `command` as one command: each request it makes that binds
/// ([`methods::binds_to_instance`]) is answered by the daemon instance that
/// answered its first, or fails as [`LspError::DaemonReplaced`] — refused by
/// any other daemon before it does any of the work, and a request no daemon
/// accepts is not taken to a new one. A command that spans a daemon's
/// restart would otherwise go on against its successor, whose sessions hold
/// none of what the earlier answers rested on, and answer with a mix of the
/// two. The CLI runs each invocation as one, the MCP server each tool call.
pub async fn one_command<F: std::future::Future>(command: F) -> F::Output {
    ANSWERED_BY.scope(std::sync::OnceLock::new(), command).await
}

/// The instance the running command is bound to, once one answered it.
fn bound_instance() -> Option<String> {
    ANSWERED_BY
        .try_with(|bound| bound.get().cloned())
        .ok()
        .flatten()
}

/// Bind the running command to `answered`, the instance that answered its
/// request for `method`, refusing the answer of any instance but the one it
/// is bound to. Outside a command nothing binds; an answer naming no
/// instance came from a daemon of another build, which is never the one a
/// bound command began on.
fn bind(method: &str, answered: Option<&str>) -> Result<(), LspError> {
    let same = ANSWERED_BY.try_with(|bound| match answered {
        Some(answered) => bound.get_or_init(|| answered.to_string()) == answered,
        None => bound.get().is_none(),
    });
    match same {
        Ok(false) => Err(LspError::DaemonReplaced(method.to_string())),
        Ok(true) | Err(_) => Ok(()),
    }
}

/// A peer closing with the request unread appears as EOF on macOS and as a
/// reset or broken pipe on Linux. Both are the lost answer described by
/// `ConnectionLost`, and neither permits replaying the request.
fn request_io_error(method: &str, error: std::io::Error) -> LspError {
    match error.kind() {
        std::io::ErrorKind::ConnectionReset | std::io::ErrorKind::BrokenPipe => {
            LspError::ConnectionLost(method.to_string())
        }
        _ => LspError::Io(error),
    }
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

/// A daemon socket path the platform cannot address. A Unix socket is named
/// by a path that fits, with its terminating NUL, in `sockaddr_un`'s
/// `sun_path` (104 bytes on macOS, 108 on Linux), so under a home deep enough
/// no daemon can listen for this installation, nor any client reach one.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SocketPathTooLong {
    path: PathBuf,
    capacity: usize,
}

impl SocketPathTooLong {
    fn check(path: &Path) -> Option<Self> {
        // SAFETY: `sockaddr_un` holds only integers and a byte array, for
        // which all-zero bytes are a valid value.
        let capacity = unsafe { std::mem::zeroed::<libc::sockaddr_un>() }
            .sun_path
            .len();
        (path.as_os_str().len() >= capacity).then(|| Self {
            path: path.to_path_buf(),
            capacity,
        })
    }

    /// Bytes the path must lose to be addressable.
    pub fn excess(&self) -> usize {
        self.path.as_os_str().len() + 1 - self.capacity
    }
}

impl std::fmt::Display for SocketPathTooLong {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "its socket path `{}` is {} bytes, and a Unix socket's path must be shorter \
             than {} bytes on this platform",
            self.path.display(),
            self.path.as_os_str().len(),
            self.capacity
        )
    }
}

/// Why no daemon can serve this installation. Every command then runs in
/// process, answering as a daemon would without reusing language servers
/// between invocations.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Unaddressable {
    SocketPathTooLong(SocketPathTooLong),
    /// A path the daemon and its callers each derive from the environment
    /// is relative. Each resolves it in its own working directory, and a
    /// daemon runs in its installation's, so the two would name different
    /// files: a socket no caller reaches, or a configuration other than the
    /// one a direct run reads.
    RelativePath {
        role: &'static str,
        path: PathBuf,
    },
}

impl Unaddressable {
    /// What lets a daemon run.
    pub fn remedy(&self) -> String {
        match self {
            Self::SocketPathTooLong(too_long) => format!(
                "A daemon runs under a HOME at least {} bytes shorter.",
                too_long.excess()
            ),
            Self::RelativePath { .. } => {
                "A daemon runs once HOME, and XDG_CONFIG_HOME where it is set, are absolute \
                 paths."
                    .to_string()
            }
        }
    }
}

impl std::fmt::Display for Unaddressable {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::SocketPathTooLong(too_long) => too_long.fmt(f),
            Self::RelativePath { role, path } => write!(
                f,
                "its {role} `{}` is a relative path, which a daemon would resolve in a \
                 directory other than its callers'",
                path.display()
            ),
        }
    }
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
                let params = serde_json::json!({
                    "file": file.display().to_string(),
                    "line": line,
                    "column": column
                });
                self.request_with_project($method, params)
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
                let params = serde_json::json!({
                    "file": file.display().to_string()
                });
                self.request_with_project($method, params)
                    .await
                    .and_then(Self::extract_result)
            }
        )*
    };
}

impl DaemonClient {
    /// Create a new daemon client
    pub fn new(project_root: &Path) -> Result<Self, LspError> {
        Ok(Self {
            config: DaemonRuntimeConfig::load()?,
            lsp_config: DaemonRuntimeConfig::load_lsp_config(project_root),
            project_root: project_root.to_path_buf(),
            next_request_id: AtomicU64::new(1),
        })
    }

    // Connection Management

    /// Ensure a daemon of *this* binary is running, starting or replacing
    /// one as necessary — the single owner of daemon lifecycle, so no
    /// other path may spawn one. A daemon left over from a different
    /// build at this installation's executable path is replaced: the wire
    /// format is guaranteed only within one build.
    pub async fn ensure_running(&self) -> Result<DaemonStart, LspError> {
        if matches!(self.ping().await, Ok(true)) {
            return Ok(DaemonStart::AlreadyRunning);
        }
        self.start_daemon_with_lock().await
    }

    /// Whether this process can reach a daemon: address its socket, whose path
    /// the platform bounds (104 bytes on macOS, 108 on Linux) and the daemon
    /// must resolve as its callers do, and open the startup lock every path
    /// that starts or replaces a daemon takes. A home too deep for the
    /// socket's path, an environment naming its paths relatively, or a
    /// sandbox that forbids the daemon directory leaves the caller running in
    /// process — the same answers, since daemon and direct execution agree,
    /// without warm reuse across invocations.
    pub fn can_start_daemon(&self) -> bool {
        if let Some(reason) = self.unaddressable() {
            tracing::info!("daemon unavailable ({reason}); running in process");
            return false;
        }
        let lock = &self.config.lock_path;
        let opened = lock
            .parent()
            .map_or(Ok(()), std::fs::create_dir_all)
            .and_then(|()| {
                std::fs::OpenOptions::new()
                    .write(true)
                    .create(true)
                    .truncate(false)
                    .open(lock)
            });
        match opened {
            Ok(_) => true,
            Err(e) => {
                tracing::info!(
                    "daemon unavailable ({}: {e}); running in process",
                    lock.display()
                );
                false
            }
        }
    }

    /// Why no daemon can be running for this installation and none can be
    /// started, when that is so.
    pub fn unaddressable(&self) -> Option<Unaddressable> {
        let socket = &self.config.socket_path;
        let config = crate::services::dist::paths::config_dir();
        if socket.is_relative() {
            return Some(Unaddressable::RelativePath {
                role: "socket path",
                path: socket.clone(),
            });
        }
        if config.is_relative() {
            return Some(Unaddressable::RelativePath {
                role: "configuration directory",
                path: config,
            });
        }
        SocketPathTooLong::check(socket).map(Unaddressable::SocketPathTooLong)
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

        let child = self
            .daemon_command(&exe)
            .spawn()
            .map_err(|e| LspError::ServerStart(format!("Failed to spawn daemon: {}", e)))?;

        drop(child);
        tracing::info!("Daemon process spawned");
        Ok(())
    }

    fn daemon_command(&self, exe: &Path) -> Command {
        let mut command = Command::new(exe);
        command
            // The daemon serves every project of this installation, so it
            // runs in the installation's own directory rather than in one
            // project's: a working directory marks a directory as in use,
            // and one inherited from the first caller would keep that
            // project's tree from being removed for as long as the daemon
            // serves the others.
            .current_dir(self.config.runtime_dir())
            .arg("daemon")
            .arg("serve")
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            // A process group of its own keeps what a terminal signals the
            // job that started it, Ctrl-C or a hangup, off a daemon that
            // outlives it.
            .process_group(0);
        // A daemon raises its own open-file limit from the one it is handed,
        // and hands that one to the servers it starts.
        crate::infra::fd_limit::restore_for_child(command.as_std_mut());
        command
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

        Err(LspError::timeout(
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
            .send_request(methods::PING, None, Some(Duration::from_secs(30)))
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
        timeout_duration: Option<Duration>,
    ) -> Result<Response, LspError> {
        match timeout_duration.filter(|_| spends_caller_budget(method)) {
            Some(limit) => {
                crate::infra::deadline::run(
                    limit,
                    self.send_request_inner(method, params, timeout_duration),
                )
                .await
            }
            None => {
                self.send_request_inner(method, params, timeout_duration)
                    .await
            }
        }
    }

    async fn send_request_inner(
        &self,
        method: &str,
        params: Option<serde_json::Value>,
        timeout_duration: Option<Duration>,
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
        let mut request = Request::new(id, method, params);
        let binds = methods::binds_to_instance(method);
        if binds {
            request.instance = bound_instance();
        }
        if let Some(limit) = timeout_duration.filter(|_| spends_caller_budget(method)) {
            request.timeout_ms = Some(
                crate::infra::deadline::remaining(limit)
                    .saturating_sub(ANSWER_MARGIN)
                    .as_millis()
                    .try_into()
                    .unwrap_or(u64::MAX),
            );
        }
        let request_json = serde_json::to_string(&request)?;

        writer
            .write_all(request_json.as_bytes())
            .await
            .map_err(|e| request_io_error(method, e))?;
        writer
            .write_all(b"\n")
            .await
            .map_err(|e| request_io_error(method, e))?;
        writer
            .flush()
            .await
            .map_err(|e| request_io_error(method, e))?;

        let mut line = String::new();
        let read = match timeout_duration {
            Some(limit) => timeout(limit, reader.read_line(&mut line))
                .await
                .map_err(|_| {
                    LspError::timeout(format!(
                        "Operation '{}' timed out after {}s",
                        method,
                        limit.as_secs()
                    ))
                })?,
            None => reader.read_line(&mut line).await,
        }
        .map_err(|e| request_io_error(method, e))?;

        // End of stream: the daemon closed without answering — it exited or
        // was replaced while this request was in flight. Parsing the empty
        // read would blame the payload for the connection, and send the
        // caller after a malformed response that was never sent. It is not
        // `NotConnected` either: the request may have reached this peer, so
        // all it proves is that this peer stopped answering, and a replacement
        // may already be serving the socket.
        if read == 0 {
            return Err(LspError::ConnectionLost(method.to_string()));
        }

        let response: Response = serde_json::from_str(&line)?;
        if binds {
            bind(method, response.instance.as_deref())?;
        }
        Ok(response)
    }

    fn inject_project(&self, params: &mut serde_json::Value) {
        if let Some(obj) = params.as_object_mut() {
            obj.insert(
                "project".to_string(),
                serde_json::Value::String(self.project_root.display().to_string()),
            );
        }
    }

    async fn send_request_with_project(
        &self,
        method: &str,
        mut params: serde_json::Value,
    ) -> Result<Response, LspError> {
        self.inject_project(&mut params);
        let timeout = calculate_timeout(&self.lsp_config, method);
        self.send_request(method, Some(params), timeout).await
    }

    async fn request_with_project(
        &self,
        method: &str,
        params: serde_json::Value,
    ) -> Result<Response, LspError> {
        let budget =
            calculate_timeout(&self.lsp_config, method).filter(|_| spends_caller_budget(method));
        match budget {
            Some(limit) => {
                crate::infra::deadline::run(limit, self.request_project_inner(method, params)).await
            }
            None => self.request_project_inner(method, params).await,
        }
    }

    async fn request_project_inner(
        &self,
        method: &str,
        params: serde_json::Value,
    ) -> Result<Response, LspError> {
        // A command bound to a daemon goes on only there: that daemon is of
        // this build and was serving, and one started or found in its place
        // would refuse the request, so none is.
        if methods::binds_to_instance(method) && bound_instance().is_some() {
            return match self.send_request_with_project(method, params).await {
                Err(LspError::NotConnected) => Err(LspError::DaemonReplaced(method.to_string())),
                result => result,
            };
        }
        self.ensure_running().await?;
        match self.send_request_with_project(method, params.clone()).await {
            Err(LspError::NotConnected) => {
                // Connect proved the request was never sent; an accepted
                // request may have mutated files and must not be replayed.
                self.ensure_running().await?;
                self.send_request_with_project(method, params).await
            }
            result => result,
        }
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
        incoming_calls => methods::INCOMING_CALLS,
        outgoing_calls => methods::OUTGOING_CALLS,
        prepare_rename => methods::PREPARE_RENAME,
        code_actions => methods::CODE_ACTIONS,
    }

    // File-based LSP Operations

    rpc_file! {
        diagnostics => methods::DIAGNOSTICS,
    }

    // Custom Parameter Operations

    pub async fn find_symbols(
        &self,
        file: &Path,
        include_body: bool,
        depth: u32,
    ) -> Result<serde_json::Value, LspError> {
        let params = serde_json::json!({
            "file": file.display().to_string(),
            "body": include_body,
            "depth": depth
        });
        self.request_with_project(methods::FIND_SYMBOLS, params)
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

    pub async fn workspace_symbols(
        &self,
        query: &str,
        language: &str,
    ) -> Result<serde_json::Value, LspError> {
        let params = serde_json::json!({
            "query": query,
            "language": language
        });
        self.request_with_project(methods::WORKSPACE_SYMBOLS, params)
            .await
            .and_then(Self::extract_result)
    }

    pub async fn apply_code_action(
        &self,
        file: &Path,
        action: &serde_json::Value,
    ) -> Result<serde_json::Value, LspError> {
        let params = serde_json::json!({
            "file": file.display().to_string(),
            "action": action
        });
        self.request_with_project(methods::APPLY_CODE_ACTION, params)
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
        self.send_request_with_project(methods::NOTE_FILES_EDITED, params)
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
        within: &[std::path::PathBuf],
    ) -> Result<serde_json::Value, LspError> {
        let params = serde_json::json!({
            "query": query,
            "limit": limit,
            "kind": kind,
            "language": language,
            "within": within
                .iter()
                .map(|path| path.to_string_lossy())
                .collect::<Vec<_>>(),
        });
        self.request_with_project(methods::SEARCH_SYMBOLS, params)
            .await
            .and_then(Self::extract_result)
    }

    pub async fn search_content(
        &self,
        query: &str,
        limit: Option<usize>,
        languages: &[String],
    ) -> Result<serde_json::Value, LspError> {
        let params = serde_json::json!({
            "query": query,
            "limit": limit,
            "languages": languages,
        });
        self.request_with_project(methods::SEARCH_CONTENT, params)
            .await
            .and_then(Self::extract_result)
    }

    pub async fn index_build(
        &self,
        force: bool,
        languages: Option<Vec<String>>,
    ) -> Result<serde_json::Value, LspError> {
        let params = serde_json::json!({
            "force": force,
            "languages": languages,
        });
        self.request_with_project(methods::INDEX_BUILD, params)
            .await
            .and_then(Self::extract_result)
    }

    pub async fn index_status(&self) -> Result<serde_json::Value, LspError> {
        self.request_with_project(methods::INDEX_STATUS, serde_json::json!({}))
            .await
            .and_then(Self::extract_result)
    }

    pub async fn tree_currency(&self) -> Result<serde_json::Value, LspError> {
        self.request_with_project(methods::TREE_CURRENCY, serde_json::json!({}))
            .await
            .and_then(Self::extract_result)
    }

    pub async fn indexed_languages(&self) -> Result<serde_json::Value, LspError> {
        self.request_with_project(methods::INDEXED_LANGUAGES, serde_json::json!({}))
            .await
            .and_then(Self::extract_result)
    }

    pub async fn index_clear(&self) -> Result<serde_json::Value, LspError> {
        self.request_with_project(methods::INDEX_CLEAR, serde_json::json!({}))
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
        self.send_request_with_project(methods::REFRESH_FILES, params)
            .await
            .and_then(Self::extract_result)
            .map(|_| ())
    }

    // Daemon Control Operations

    /// Unload only this project's services without starting or replacing a daemon.
    pub async fn restart_project(&self) -> Result<super::wire::ProjectRestartStatus, LspError> {
        match self.ping().await {
            Ok(true) => {}
            Ok(false) => {
                return Err(LspError::Protocol(
                    "Running daemon belongs to a different build; project restart requires the same binary"
                        .to_string(),
                ));
            }
            Err(LspError::NotConnected) => return Ok(super::wire::ProjectRestartStatus::NotLoaded),
            Err(e) => return Err(e),
        }
        let result = self
            .send_request_with_project(methods::RESTART_PROJECT, serde_json::json!({}))
            .await
            .and_then(Self::extract_result)?;
        Ok(serde_json::from_value(result)?)
    }

    pub async fn status(&self) -> Result<serde_json::Value, LspError> {
        self.send_request(methods::STATUS, None, Some(Duration::from_secs(30)))
            .await
            .and_then(Self::extract_result)
    }

    /// Stop the running daemon. `Ok(false)` means the request reached no
    /// daemon — there was nothing to stop, which callers report rather
    /// than dress up as a stop that happened.
    pub async fn shutdown(&self) -> Result<bool, LspError> {
        let reached = match self
            .send_request(methods::SHUTDOWN, None, Some(Duration::from_secs(30)))
            .await
        {
            Ok(_) => true,
            Err(LspError::NotConnected) => false,
            Err(e) => return Err(e),
        };
        self.wait_for_shutdown().await?;
        Ok(reached)
    }

    /// Wait until nothing answers on the socket. Only a connect failure
    /// accepted by [`proves_no_listener`] confirms the daemon is gone.
    /// Other failures leave the question open and are reported as themselves,
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
        Err(LspError::timeout(
            "Daemon did not stop within timeout".to_string(),
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{Error, ErrorKind};

    /// The platform refuses to address a socket path exactly where the check
    /// calls it too long, and accepts it once `excess` bytes are gone.
    #[test]
    fn a_socket_path_is_too_long_where_the_platform_refuses_it() {
        use std::os::unix::net::SocketAddr;

        for len in 90..=120 {
            let path = format!("/{}", "s".repeat(len - 1));
            let too_long = SocketPathTooLong::check(Path::new(&path));
            assert_eq!(
                too_long.is_some(),
                SocketAddr::from_pathname(&path).is_err(),
                "{len} bytes"
            );
            if let Some(too_long) = too_long {
                let fits = len - too_long.excess();
                assert!(SocketAddr::from_pathname(&path[..fits]).is_ok(), "{len}");
                assert!(SocketAddr::from_pathname(&path[..=fits]).is_err(), "{len}");
            }
        }
        assert!(SocketPathTooLong::check(Path::new("/s")).is_none());
    }

    fn client(runtime: &Path, project: &Path) -> DaemonClient {
        DaemonClient {
            config: DaemonRuntimeConfig {
                socket_path: runtime.join("daemon.sock"),
                pid_path: runtime.join("daemon.pid"),
                lock_path: runtime.join("daemon.lock"),
                bind_lock_path: runtime.join("daemon.bind.lock"),
                idle_timeout: Duration::from_secs(60),
                max_concurrent: 1,
            },
            lsp_config: Default::default(),
            project_root: project.to_path_buf(),
            next_request_id: AtomicU64::new(1),
        }
    }

    /// The daemon a project's command starts serves every project of the
    /// installation, so it runs in the installation's directory, never in
    /// the project's.
    #[test]
    fn the_daemon_runs_in_its_installation_directory() {
        let client = client(Path::new("/home/u/.symora"), Path::new("/work/tree"));
        let command = client.daemon_command(Path::new("/bin/symora"));
        assert_eq!(
            command.as_std().get_current_dir(),
            Some(Path::new("/home/u/.symora"))
        );
    }

    /// A relative socket path resolves in the daemon's directory to another
    /// socket than the one its caller connects to, so no daemon serves it.
    #[test]
    fn a_relative_socket_path_is_unaddressable() {
        let client = client(Path::new("h/.symora"), Path::new("/work/tree"));
        assert!(matches!(
            client.unaddressable(),
            Some(Unaddressable::RelativePath {
                role: "socket path",
                ..
            })
        ));
        assert!(!client.can_start_daemon());
    }

    /// A stand-in daemon on `socket` answering every request as the instance
    /// `serving` names at the time, its ping as this build's; `None` stops
    /// it listening. Records each project request's bound instance.
    fn stand_in(
        socket: PathBuf,
        serving: std::sync::Arc<std::sync::Mutex<Option<&'static str>>>,
        bound: std::sync::Arc<std::sync::Mutex<Vec<Option<String>>>>,
    ) -> tokio::task::JoinHandle<()> {
        let listener = tokio::net::UnixListener::bind(&socket).unwrap();
        tokio::spawn(async move {
            loop {
                let (stream, _) = listener.accept().await.unwrap();
                let (reader, mut writer) = stream.into_split();
                let mut line = String::new();
                BufReader::new(reader).read_line(&mut line).await.unwrap();
                let request: Request = serde_json::from_str(&line).unwrap();
                let result = match request.method.as_str() {
                    methods::PING => serde_json::json!({
                        "pong": true,
                        "version": env!("CARGO_PKG_VERSION"),
                        "build": crate::daemon::protocol::BUILD_ID,
                    }),
                    _ => {
                        bound.lock().unwrap().push(request.instance.clone());
                        serde_json::json!([])
                    }
                };
                let instance = *serving.lock().unwrap();
                let mut response = Response::success(request.id, result);
                response.instance = instance.map(str::to_string);
                let json = serde_json::to_string(&response).unwrap();
                writer
                    .write_all(format!("{json}\n").as_bytes())
                    .await
                    .unwrap();
                if instance.is_none() {
                    drop(listener);
                    std::fs::remove_file(&socket).unwrap();
                    return;
                }
            }
        })
    }

    /// One command's requests are answered by the daemon that answered its
    /// first: the next carries that instance, and an answer from another is
    /// refused, as is going on where none listens any more — without
    /// starting one, which could only be another. Separate commands bind
    /// separately.
    #[tokio::test]
    async fn a_command_is_answered_by_the_daemon_that_answered_it_first() {
        use std::sync::{Arc, Mutex};

        let runtime = tempfile::tempdir().unwrap();
        let client = client(runtime.path(), Path::new("/work/tree"));
        let serving = Arc::new(Mutex::new(Some("first")));
        let bound = Arc::new(Mutex::new(Vec::new()));
        let daemon = stand_in(
            client.config.socket_path.clone(),
            Arc::clone(&serving),
            Arc::clone(&bound),
        );
        let ask = || client.search_content("needle", None, &[]);

        let across = one_command(async {
            let first = ask().await;
            *serving.lock().unwrap() = Some("second");
            (first, ask().await)
        })
        .await;
        assert!(across.0.is_ok(), "{:?}", across.0);
        assert!(
            matches!(&across.1, Err(LspError::DaemonReplaced(method)) if method == methods::SEARCH_CONTENT),
            "{:?}",
            across.1
        );
        assert_eq!(
            *bound.lock().unwrap(),
            [None, Some("first".to_string())],
            "the second request named the instance the first was answered by"
        );

        for _ in 0..2 {
            assert!(one_command(ask()).await.is_ok(), "a new command binds anew");
        }

        let gone = one_command(async {
            let first = ask().await;
            *serving.lock().unwrap() = None;
            let last = ask().await;
            (first, last, ask().await)
        })
        .await;
        assert!(gone.0.is_ok() && gone.1.is_err(), "{gone:?}");
        assert!(
            matches!(&gone.2, Err(LspError::DaemonReplaced(_))),
            "{:?}",
            gone.2
        );
        daemon.await.unwrap();
    }

    #[test]
    fn request_io_errors_distinguish_peer_closure_from_other_failures() {
        for kind in [ErrorKind::ConnectionReset, ErrorKind::BrokenPipe] {
            assert!(matches!(
                request_io_error(methods::SEARCH_CONTENT, Error::from(kind)),
                LspError::ConnectionLost(method) if method == methods::SEARCH_CONTENT
            ));
        }
        for kind in [
            ErrorKind::ConnectionRefused,
            ErrorKind::NotFound,
            ErrorKind::PermissionDenied,
            ErrorKind::WouldBlock,
            ErrorKind::TimedOut,
            ErrorKind::InvalidInput,
            ErrorKind::UnexpectedEof,
            ErrorKind::Other,
        ] {
            let error = Error::new(kind, "request I/O failure");
            let LspError::Io(error) = request_io_error(methods::SEARCH_CONTENT, error) else {
                panic!("{kind:?} must remain an I/O error");
            };
            assert_eq!(error.kind(), kind);
            assert_eq!(error.to_string(), "request I/O failure");
        }
    }
}
