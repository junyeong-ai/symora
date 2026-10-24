use std::collections::HashMap;
use std::path::PathBuf;
use std::process::Stdio;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Weak};
use std::time::{Duration, Instant};

use serde_json::Value;
use tokio::process::{Child, ChildStdin, Command};
use tokio::sync::{Mutex, Notify, RwLock, oneshot};
use tokio::time::timeout;

use super::protocol::{
    ClientInfo, DidChangeWatchedFilesParams, InitializeParams, InitializeResult, LspDiagnostic,
    Message, Notification, PositionEncoding, RegistrationParams, Request, RequestId, Response,
    ResponseError, UnregistrationParams, error_codes,
};
use super::transport::{Transport, write_notification, write_request, write_response};
use super::watch::{FileChange, WATCHED_FILES_METHOD, WatchRegistry};
use crate::error::LspError;
use crate::models::lsp::path_to_uri;
use crate::models::symbol::Language;

mod capabilities;
mod indexing;

use capabilities::client_capabilities;
use indexing::IndexingStatus;

pub use indexing::{IndexingEvent, IndexingQuery, IndexingState};

type PendingRequest = oneshot::Sender<Response>;

const MAX_OPEN_DOCUMENTS: usize = 100;
const MAX_DIAGNOSTICS_CACHE: usize = 200;

/// Monotonic workspace-content generation. Bumped whenever any client
/// learns that content changed — a `didChange` from our own edits or the
/// drift sweep, a `didClose` of a vanished file — when the workspace watcher
/// reports a change on disk, and when a client session starts (a fresh server is a fresh world). Caches of
/// workspace-wide answers validate against it, so no cached answer ever
/// outlives the content it was computed from. Starts at 1: cache layers
/// reserve 0 as their "no validation" sentinel.
static CONTENT_GENERATION: AtomicU64 = AtomicU64::new(1);

/// Current workspace-content generation (see `CONTENT_GENERATION`).
pub fn content_generation() -> u64 {
    CONTENT_GENERATION.load(Ordering::Acquire)
}

fn bump_content_generation() {
    CONTENT_GENERATION.fetch_add(1, Ordering::AcqRel);
}

/// Record a workspace content change that happened outside any client's
/// overlay — symora's own write to a file no server has open, or a change the
/// workspace watcher saw on disk. Caches of
/// workspace-wide answers validate against the generation, so the bump
/// must not depend on a live client having the file open.
pub fn note_workspace_content_changed() {
    bump_content_generation();
}

#[derive(Debug, Clone, Copy)]
enum LogLevel {
    Error,
    Warn,
    Info,
    Debug,
    Ignore,
}

/// One `textDocument/publishDiagnostics` notification, kept with the
/// metadata needed to judge freshness: the document version the server
/// analyzed (when it reports one, LSP 3.15+) and a monotonic arrival
/// sequence for servers that don't.
#[derive(Debug, Clone)]
pub struct PublishedDiagnostics {
    pub doc_version: Option<u32>,
    pub seq: u64,
    pub items: Vec<LspDiagnostic>,
}

/// Cheap fingerprint of an overlay's backing file, captured when content
/// is sent to the server. A later mismatch (or a failed probe) is the
/// drift signal that triggers a content re-read on access; equality skips
/// it. mtime+len can miss a write that lands between the caller's read
/// and the probe — the next direct sync of that file (every targeted
/// command performs one) heals that window, and a probe failure always
/// reads as drift, never as fresh.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct DiskState {
    mtime: std::time::SystemTime,
    len: u64,
}

impl DiskState {
    async fn probe(path: &std::path::Path) -> Option<Self> {
        let meta = tokio::fs::metadata(path).await.ok()?;
        Some(Self {
            mtime: meta.modified().ok()?,
            len: meta.len(),
        })
    }
}

#[derive(Debug)]
struct DocumentState {
    version: u32,
    content_hash: u64,
    /// Fingerprint of the backing file at the time `content_hash` was
    /// sent. `None` means the probe failed and the next access re-reads.
    disk: Option<DiskState>,
}

impl DocumentState {
    fn new(content: &str, disk: Option<DiskState>) -> Self {
        Self {
            version: 1,
            content_hash: crate::infra::hash_content(content),
            disk,
        }
    }

    fn needs_update(&self, new_content: &str) -> bool {
        crate::infra::hash_content(new_content) != self.content_hash
    }

    fn update(&mut self, new_content: &str) {
        self.version += 1;
        self.content_hash = crate::infra::hash_content(new_content);
    }
}

/// What a document sync did to the server's view: opened a fresh overlay,
/// changed an existing one, or matched it (no notification sent).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SyncOutcome {
    Opened,
    Changed,
    Unchanged,
}

use std::collections::VecDeque;

struct DocumentCache {
    docs: HashMap<String, DocumentState>,
    lru_order: VecDeque<String>,
}

impl DocumentCache {
    fn new() -> Self {
        Self {
            docs: HashMap::new(),
            lru_order: VecDeque::new(),
        }
    }

    fn get_mut(&mut self, uri: &str) -> Option<&mut DocumentState> {
        if self.docs.contains_key(uri) {
            self.touch(uri);
        }
        self.docs.get_mut(uri)
    }

    fn touch(&mut self, uri: &str) {
        self.lru_order.retain(|u| u != uri);
        self.lru_order.push_front(uri.to_string());
    }

    fn insert(&mut self, uri: String, state: DocumentState) -> Option<String> {
        let evicted = if self.docs.len() >= MAX_OPEN_DOCUMENTS && !self.docs.contains_key(&uri) {
            self.evict_lru()
        } else {
            None
        };

        if !self.docs.contains_key(&uri) {
            self.lru_order.push_front(uri.clone());
        } else {
            self.touch(&uri);
        }
        self.docs.insert(uri, state);
        evicted
    }

    fn evict_lru(&mut self) -> Option<String> {
        if let Some(uri) = self.lru_order.pop_back() {
            self.docs.remove(&uri);
            return Some(uri);
        }
        None
    }

    fn remove(&mut self, uri: &str) -> bool {
        self.lru_order.retain(|u| u != uri);
        self.docs.remove(uri).is_some()
    }

    /// Snapshot for the drift sweep: every open document with the disk
    /// fingerprint its overlay was sent under.
    fn overlay_snapshot(&self) -> Vec<(String, Option<DiskState>)> {
        self.docs
            .iter()
            .map(|(uri, state)| (uri.clone(), state.disk))
            .collect()
    }
}

struct ClientUseState {
    in_flight: usize,
    last_used: Instant,
}

#[derive(Clone, Copy)]
pub(super) struct ClientUse {
    pub(super) in_flight: bool,
    pub(super) last_used: Instant,
}

struct RequestUse<'a>(&'a LspClient);

impl Drop for RequestUse<'_> {
    fn drop(&mut self) {
        let mut usage = self.0.usage.lock().expect("client usage lock poisoned");
        usage.last_used = Instant::now();
        usage.in_flight -= 1;
    }
}

const CANCELLATION_TIMEOUT: Duration = Duration::from_secs(2);

struct PendingRequestGuard {
    runtime: tokio::runtime::Handle,
    id: u64,
    pending: Arc<std::sync::Mutex<HashMap<RequestId, PendingRequest>>>,
    stdin: Arc<Mutex<Option<ChildStdin>>>,
    timeout: Duration,
}

impl Drop for PendingRequestGuard {
    fn drop(&mut self) {
        let abandoned = self
            .pending
            .lock()
            .expect("pending requests lock poisoned")
            .remove(&RequestId::Number(self.id))
            .is_some();
        if abandoned {
            send_cancellation(
                &self.runtime,
                Arc::clone(&self.stdin),
                self.id,
                self.timeout,
            );
        }
    }
}

fn send_cancellation(
    runtime: &tokio::runtime::Handle,
    stdin: Arc<Mutex<Option<ChildStdin>>>,
    id: u64,
    timeout: Duration,
) -> tokio::task::JoinHandle<()> {
    runtime.spawn(async move {
        let _ = crate::infra::deadline::run(timeout, async {
            let notification =
                Notification::new("$/cancelRequest", Some(serde_json::json!({ "id": id })));
            let writer = stdin.lock_owned().await;
            if writer.is_some() {
                write_notification(writer, &notification, timeout).await?;
            }
            Ok(())
        })
        .await;
    })
}

struct DocumentSyncGuard<'a> {
    client: &'a LspClient,
    complete: bool,
}

impl Drop for DocumentSyncGuard<'_> {
    fn drop(&mut self) {
        if !self.complete {
            self.client.cancel_pending_requests_terminated();
        }
    }
}

pub struct LspClient {
    language: Language,
    /// Spawned `kill_on_drop`, so the server process lives exactly as long as
    /// this client. Nothing the client starts may hold it strongly — a task
    /// or handler that did would keep an abandoned server running forever.
    process: Mutex<Option<Child>>,
    stdin: Arc<Mutex<Option<ChildStdin>>>,
    usage: std::sync::Mutex<ClientUseState>,
    next_id: AtomicU64,
    pending: Arc<std::sync::Mutex<HashMap<RequestId, PendingRequest>>>,
    diagnostics: RwLock<HashMap<String, PublishedDiagnostics>>,
    publish_seq: AtomicU64,
    document_cache: RwLock<DocumentCache>,
    root: PathBuf,
    config: Arc<crate::config::LspRuntimeConfig>,
    capabilities: RwLock<Option<InitializeResult>>,
    /// The negotiated position encoding, parsed once from the initialize
    /// response. Every inbound/outbound position conversion reads this.
    position_encoding: RwLock<PositionEncoding>,
    shutdown: RwLock<bool>,
    indexing_status: std::sync::Mutex<IndexingStatus>,
    indexing_notify: Notify,
    /// Serializes the drift sweep's `didClose` against in-flight requests:
    /// each request holds a read guard for its full dispatch window, and
    /// closing a vanished overlay takes the write guard — so an overlay is
    /// never closed out from under a request that may be answering from it.
    /// Requests are NOT serialized through one connection (the pending map
    /// allows true concurrency), which is why the gate exists.
    dispatch_gate: RwLock<()>,
    terminated: AtomicBool,
    /// Set as the server is sent `initialized`, before the write, so it
    /// stands by the time the server can register a file watcher.
    initialized: AtomicBool,
    /// Whether the server is offered `workspace.didChangeWatchedFiles`; the
    /// manager decides, by [`FileWatch`](super::watch::FileWatch).
    file_watching: bool,
    watched_files: std::sync::Mutex<WatchRegistry>,
    /// The settings snapshot used for this session's configuration replies.
    settings: RwLock<Option<serde_json::Value>>,
}

/// Walk a dotted configuration section path ("python.analysis") through
/// the settings payload.
fn lookup_section(settings: &Value, section: &str) -> Option<Value> {
    let mut node = settings;
    for part in section.split('.') {
        node = node.get(part)?;
    }
    Some(node.clone())
}

impl LspClient {
    pub fn new(
        language: Language,
        root: PathBuf,
        config: Arc<crate::config::LspRuntimeConfig>,
        file_watching: bool,
    ) -> Arc<Self> {
        bump_content_generation();
        Arc::new(Self {
            language,
            process: Mutex::new(None),
            stdin: Arc::new(Mutex::new(None)),
            usage: std::sync::Mutex::new(ClientUseState {
                in_flight: 0,
                last_used: Instant::now(),
            }),
            next_id: AtomicU64::new(1),
            pending: Arc::new(std::sync::Mutex::new(HashMap::new())),
            diagnostics: RwLock::new(HashMap::new()),
            publish_seq: AtomicU64::new(0),
            document_cache: RwLock::new(DocumentCache::new()),
            root,
            config,
            capabilities: RwLock::new(None),
            position_encoding: RwLock::new(PositionEncoding::default()),
            shutdown: RwLock::new(false),
            indexing_status: std::sync::Mutex::new(IndexingStatus {
                state: IndexingState::NotStarted,
                revision: 0,
            }),
            indexing_notify: Notify::new(),
            dispatch_gate: RwLock::new(()),
            terminated: AtomicBool::new(false),
            initialized: AtomicBool::new(false),
            file_watching,
            watched_files: std::sync::Mutex::new(WatchRegistry::default()),
            settings: RwLock::new(None),
        })
    }

    /// Start the language server. `admit` runs once the server has answered
    /// `initialize` and before it is told `initialized`, with its input held:
    /// whoever `admit` hands the client to writes nothing ahead of
    /// `initialized`, and a server that registers file watchers once told
    /// registers them with the client already handed on.
    pub async fn start(
        self: &Arc<Self>,
        launch: &super::servers::ServerLaunch,
        admit: impl FnOnce(),
    ) -> Result<(), LspError> {
        // Check if already running
        if self.is_running().await {
            return Ok(());
        }

        tracing::info!(
            "Starting {} language server: {} {:?}",
            self.language,
            launch.command.display(),
            launch.args
        );

        // Spawn server process
        let mut child = Command::new(&launch.command)
            .args(&launch.args)
            .envs(&launch.env)
            .current_dir(&self.root)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true)
            .spawn()
            .map_err(|e| LspError::ServerStart(format!("{}: {}", launch.command.display(), e)))?;

        let stdin = child
            .stdin
            .take()
            .ok_or_else(|| LspError::ServerStart("Failed to get stdin".to_string()))?;
        let stdout = child
            .stdout
            .take()
            .ok_or_else(|| LspError::ServerStart("Failed to get stdout".to_string()))?;
        let stderr = child
            .stderr
            .take()
            .ok_or_else(|| LspError::ServerStart("Failed to get stderr".to_string()))?;

        // Store process and stdin
        *self.process.lock().await = Some(child);
        *self.stdin.lock().await = Some(stdin);

        tokio::spawn(Self::read_responses(
            Arc::downgrade(self),
            Transport::new(stdout),
        ));

        // Drain the server's stderr continuously: an undrained pipe
        // eventually fills and deadlocks the server, and a crashing
        // server's last words are the only diagnostic there is.
        let language = self.language;
        tokio::spawn(async move {
            use tokio::io::AsyncBufReadExt;
            let mut lines = tokio::io::BufReader::new(stderr).lines();
            while let Ok(Some(line)) = lines.next_line().await {
                tracing::debug!("LSP {language} stderr: {line}");
            }
        });

        // Initialize the server
        self.initialize(admit).await?;

        tracing::info!("{} language server started successfully", self.language);
        Ok(())
    }

    /// Whether the server has been, or is being, sent `initialized`.
    pub fn initialized(&self) -> bool {
        self.initialized.load(Ordering::Acquire)
    }

    /// Check if server is running
    pub async fn is_running(&self) -> bool {
        if self.terminated.load(Ordering::Acquire) || self.stdin.lock().await.is_none() {
            return false;
        }
        let mut process = self.process.lock().await;
        if let Some(ref mut child) = *process {
            match child.try_wait() {
                Ok(None) => true,     // Still running
                Ok(Some(_)) => false, // Exited
                Err(_) => false,      // Error checking = treat as dead
            }
        } else {
            false
        }
    }

    /// Perform a health check on the LSP server
    ///
    /// Returns true if the server is running and responsive.
    /// Uses a short timeout to quickly detect unresponsive servers.
    pub async fn health_check(&self) -> bool {
        if !self.is_running().await {
            return false;
        }

        // Check if we're already shut down
        if *self.shutdown.read().await {
            return false;
        }

        // Try a lightweight request to verify responsiveness
        // Using capabilities check which should be fast
        self.capabilities.read().await.is_some()
    }

    /// Initialize the language server
    async fn initialize(&self, admit: impl FnOnce()) -> Result<(), LspError> {
        let configuration = super::configuration::ServerConfiguration::resolve(
            self.language,
            &self.root,
            &self.config,
        );
        *self.settings.write().await = Some(configuration.settings.clone());

        let params = InitializeParams {
            process_id: Some(std::process::id()),
            root_uri: Some(path_to_uri(&self.root)),
            workspace_folders: Some(vec![crate::infra::lsp::protocol::WorkspaceFolder {
                uri: path_to_uri(&self.root),
                name: self
                    .root
                    .file_name()
                    .map(|n| n.to_string_lossy().into_owned())
                    .unwrap_or_else(|| "workspace".to_string()),
            }]),
            capabilities: client_capabilities(self.language, self.file_watching),
            client_info: Some(ClientInfo {
                name: "symora".to_string(),
                version: Some(env!("CARGO_PKG_VERSION").to_string()),
            }),
            initialization_options: configuration.initialization_options,
        };

        tracing::debug!(
            "Initializing {} LSP with options: {:?}",
            self.language,
            params.initialization_options.is_some()
        );

        // A rejected handshake is a statement about the session, not about
        // the request that carried it: the server is installed and answering,
        // but it will never serve this workspace (a missing toolchain, an
        // unreadable project layout). Reporting it as the server failing to
        // start routes it to the same recovery advice as a server that never
        // came up, instead of the generic internal-error path an agent has no
        // move against.
        let result: InitializeResult = self
            .request("initialize", Some(serde_json::to_value(params)?))
            .await
            .map_err(|e| self.handshake_failure(e))?;

        // Close the encoding negotiation: record the server's choice once.
        *self.position_encoding.write().await =
            PositionEncoding::from_capabilities(&result.capabilities);

        // Store capabilities
        *self.capabilities.write().await = Some(result);

        let mut notifications = vec![Notification::new(
            "initialized",
            Some(serde_json::json!({})),
        )];
        if self
            .config
            .servers
            .get(self.language.lsp_id())
            .and_then(|server| server.settings.as_ref())
            .is_some()
        {
            notifications.push(Notification::new(
                "workspace/didChangeConfiguration",
                Some(serde_json::json!({"settings": configuration.settings})),
            ));
        }
        let stdin_guard = Arc::clone(&self.stdin).lock_owned().await;
        if stdin_guard.is_none() {
            return Err(self.handshake_failure(LspError::NotConnected));
        }
        admit();
        self.initialized.store(true, Ordering::Release);
        super::transport::write_notifications(
            stdin_guard,
            &notifications,
            self.config.request_timeout(),
        )
        .await
        .map_err(|e| self.handshake_failure(e.into()))?;

        Ok(())
    }

    /// Recast a failure of the initialize handshake as the server failing to
    /// start, preserving the server's own explanation — it is the only part
    /// that says what to fix.
    fn handshake_failure(&self, error: LspError) -> LspError {
        match error {
            LspError::ServerError { message, .. } | LspError::Protocol(message) => {
                LspError::ServerStart(format!("{} language server: {message}", self.language))
            }
            other => other,
        }
    }

    pub(super) fn mark_used(&self) {
        self.usage
            .lock()
            .expect("client usage lock poisoned")
            .last_used = Instant::now();
    }

    pub(super) fn usage(&self) -> ClientUse {
        let usage = self.usage.lock().expect("client usage lock poisoned");
        ClientUse {
            in_flight: usage.in_flight != 0,
            last_used: usage.last_used,
        }
    }

    /// Send a request and wait for response
    pub async fn request<T: serde::de::DeserializeOwned>(
        &self,
        method: &str,
        params: Option<Value>,
    ) -> Result<T, LspError> {
        crate::infra::deadline::run(
            self.config.request_timeout(),
            self.request_inner(method, params),
        )
        .await
    }

    async fn request_inner<T: serde::de::DeserializeOwned>(
        &self,
        method: &str,
        params: Option<Value>,
    ) -> Result<T, LspError> {
        self.usage
            .lock()
            .expect("client usage lock poisoned")
            .in_flight += 1;
        let _usage = RequestUse(self);

        // Check if server is terminated before sending
        if self.terminated.load(Ordering::Acquire) {
            return Err(LspError::ServerTerminated {
                language: self.language,
                established: self.capabilities.read().await.is_some(),
            });
        }

        // The document-access invariant: nothing is asked of the server
        // while an overlay it holds has drifted from disk. The sweep runs
        // BEFORE this request takes its dispatch-gate read guard, so a
        // sweep that must close a vanished overlay (write guard) never
        // deadlocks with its own request.
        self.refresh_drifted_overlays().await;

        // Held for the full dispatch window: a concurrent sweep's
        // `didClose` (write guard) waits until no request is in flight.
        let _dispatch = self.dispatch_gate.read().await;

        // Generate unique request ID
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);

        // Create response channel
        let (tx, rx) = oneshot::channel();

        // Register pending request
        {
            let mut pending = self.pending.lock().expect("pending requests lock poisoned");
            if self.terminated.load(Ordering::Acquire) {
                return Err(LspError::ServerTerminated {
                    language: self.language,
                    established: self.initialized.load(Ordering::Acquire),
                });
            }
            pending.insert(RequestId::Number(id), tx);
        }

        let _pending = PendingRequestGuard {
            runtime: tokio::runtime::Handle::current(),
            id,
            pending: Arc::clone(&self.pending),
            stdin: Arc::clone(&self.stdin),
            timeout: self.config.request_timeout().min(CANCELLATION_TIMEOUT),
        };

        // Build and send request
        let request = Request::new(id, method, params);

        tracing::trace!("{} LSP request {}: {}", self.language, id, method);

        {
            let stdin_guard = Arc::clone(&self.stdin).lock_owned().await;
            if stdin_guard.is_none() {
                return Err(LspError::NotConnected);
            }
            write_request(stdin_guard, &request, self.config.request_timeout()).await?;
        }

        match rx.await {
            Ok(response) => match response.into_result() {
                Ok(result) => {
                    serde_json::from_value(result).map_err(|e| LspError::Protocol(e.to_string()))
                }
                Err(err) if err.code == super::protocol::error_codes::SERVER_TERMINATED => {
                    Err(LspError::ServerTerminated {
                        language: self.language,
                        established: self.capabilities.read().await.is_some(),
                    })
                }
                Err(err) => Err(err.into()),
            },
            Err(_) => Err(LspError::RequestCancelled),
        }
    }

    pub async fn cancel_request(&self, id: u64) {
        {
            let mut pending = self.pending.lock().expect("pending requests lock poisoned");
            pending.remove(&RequestId::Number(id));
        }
        let sent = send_cancellation(
            &tokio::runtime::Handle::current(),
            Arc::clone(&self.stdin),
            id,
            self.config.request_timeout().min(CANCELLATION_TIMEOUT),
        );
        let _ = sent.await;
    }

    /// Send a notification (no response expected)
    pub async fn notify(&self, method: &str, params: Option<Value>) -> Result<(), LspError> {
        crate::infra::deadline::run(self.config.request_timeout(), async {
            let notification = Notification::new(method, params);

            let stdin_guard = Arc::clone(&self.stdin).lock_owned().await;
            if stdin_guard.is_none() {
                return Err(LspError::NotConnected);
            }
            write_notification(stdin_guard, &notification, self.config.request_timeout()).await?;

            Ok(())
        })
        .await
    }

    /// Background task that reads and dispatches responses. It holds the
    /// client weakly and only for one message at a time, so the reader never
    /// keeps the server process alive past its last owner.
    async fn read_responses(client: Weak<Self>, mut transport: Transport) {
        loop {
            let message = transport.read_message().await;
            let Some(client) = client.upgrade() else {
                break;
            };

            match message {
                Ok(message) => {
                    client.handle_message(message).await;
                }
                Err(e) => {
                    if !*client.shutdown.read().await {
                        tracing::error!("{} LSP read error: {}", client.language, e);
                        client.cancel_pending_requests_terminated();
                    }
                    break;
                }
            }
        }
    }

    /// Cancel all pending requests due to server termination
    fn cancel_pending_requests_terminated(&self) {
        self.terminated.store(true, Ordering::Release);
        let mut pending = self.pending.lock().expect("pending requests lock poisoned");
        let count = pending.len();
        if count > 0 {
            tracing::debug!(
                "Cancelling {} pending requests: {} server terminated",
                count,
                self.language
            );
            for (id, sender) in pending.drain() {
                let error_response = Response {
                    jsonrpc: "2.0".to_string(),
                    id: Some(id),
                    result: None,
                    error: Some(super::protocol::ResponseError {
                        code: super::protocol::error_codes::SERVER_TERMINATED,
                        message: format!(
                            "{:?} language server terminated unexpectedly",
                            self.language
                        ),
                        data: None,
                    }),
                };
                let _ = sender.send(error_response);
            }
        }
    }

    /// Cancel all pending requests (generic cancellation)
    async fn cancel_pending_requests(&self, reason: &str) {
        let mut pending = self.pending.lock().expect("pending requests lock poisoned");
        let count = pending.len();
        if count > 0 {
            tracing::debug!("Cancelling {} pending requests: {}", count, reason);
            for (id, sender) in pending.drain() {
                let error_response = Response {
                    jsonrpc: "2.0".to_string(),
                    id: Some(id),
                    result: None,
                    error: Some(super::protocol::ResponseError {
                        code: super::protocol::error_codes::REQUEST_CANCELLED,
                        message: reason.to_string(),
                        data: None,
                    }),
                };
                let _ = sender.send(error_response);
            }
        }
    }

    /// Handle an incoming message
    async fn handle_message(&self, message: Message) {
        match message {
            Message::Response(response) => {
                if let Some(id) = response.id.clone() {
                    let mut pending = self.pending.lock().expect("pending requests lock poisoned");
                    // Try direct match first, then string->number coercion for compatibility
                    let sender = pending.remove(&id).or_else(|| {
                        if let RequestId::String(s) = &id {
                            s.parse::<u64>()
                                .ok()
                                .and_then(|n| pending.remove(&RequestId::Number(n)))
                        } else {
                            None
                        }
                    });
                    match sender {
                        Some(tx) => {
                            let _ = tx.send(response);
                        }
                        None => {
                            tracing::debug!(
                                "Received response for unknown request ID {:?} (may have timed out)",
                                id
                            );
                        }
                    }
                }
            }
            Message::Request(request) => {
                self.handle_server_request(request).await;
            }
            Message::Notification(notification) => {
                let method = notification.method.as_str();
                let params = notification
                    .params
                    .clone()
                    .unwrap_or(serde_json::Value::Null);

                // Built-in notification handling
                match method {
                    "textDocument/publishDiagnostics" => {
                        let uri = params.get("uri").and_then(|u| u.as_str());
                        let doc_version = params
                            .get("version")
                            .and_then(|v| v.as_u64())
                            .map(|v| v as u32);
                        let diags = params.get("diagnostics").cloned();
                        if let (Some(uri), Some(diags)) = (uri, diags)
                            && let Ok(items) = serde_json::from_value::<Vec<LspDiagnostic>>(diags)
                        {
                            let seq = self.publish_seq.fetch_add(1, Ordering::AcqRel) + 1;
                            let mut cache = self.diagnostics.write().await;
                            let count = items.len();

                            // Evict oldest entry if at capacity (simple FIFO)
                            if cache.len() >= MAX_DIAGNOSTICS_CACHE
                                && !cache.contains_key(uri)
                                && let Some(oldest_key) = cache.keys().next().cloned()
                            {
                                cache.remove(&oldest_key);
                                tracing::trace!("Evicted diagnostics for {}", oldest_key);
                            }

                            cache.insert(
                                uri.to_string(),
                                PublishedDiagnostics {
                                    doc_version,
                                    seq,
                                    items,
                                },
                            );
                            tracing::debug!("Cached {} diagnostics for {}", count, uri);
                        }
                    }
                    "experimental/serverStatus" => self.observe_server_status(&params),
                    "window/logMessage" | "window/showMessage" => {
                        if let Some(msg) = params.get("message").and_then(|m| m.as_str()) {
                            let msg_type = params.get("type").and_then(|t| t.as_u64());
                            match Self::classify_log_level(self.language, msg, msg_type) {
                                LogLevel::Error => {
                                    tracing::error!("LSP {}: {}", self.language, msg)
                                }
                                LogLevel::Warn => tracing::warn!("LSP {}: {}", self.language, msg),
                                LogLevel::Info => tracing::info!("LSP {}: {}", self.language, msg),
                                LogLevel::Debug => {
                                    tracing::debug!("LSP {}: {}", self.language, msg)
                                }
                                LogLevel::Ignore => {}
                            }
                        }
                    }
                    _ => {
                        tracing::trace!("Unhandled notification: {}", method);
                    }
                }
            }
        }
    }

    /// Request shutdown, then wait for an acknowledged exit or terminate the process.
    pub async fn shutdown(&self) -> Result<(), LspError> {
        *self.shutdown.write().await = true;

        // Stage 1: Send LSP shutdown request (2s timeout)
        let shutdown_result = timeout(Duration::from_secs(2), async {
            self.request::<()>("shutdown", None).await?;
            self.notify("exit", None).await
        })
        .await;
        let acknowledged = matches!(shutdown_result, Ok(Ok(())));

        // Close stdin to signal EOF
        if let Ok(mut stdin) = self.stdin.try_lock() {
            stdin.take();
        } else if let Some(child) = self.process.lock().await.as_mut() {
            let _ = child.start_kill();
        }

        // Stage 2 & 3: Wait for process exit, then force kill
        if let Some(mut child) = self.process.lock().await.take() {
            if !acknowledged {
                let _ = child.start_kill();
            }
            let remaining = crate::infra::deadline::remaining(Duration::from_secs(5));
            match timeout(remaining, child.wait()).await {
                Ok(Ok(status)) => {
                    tracing::debug!("{} language server exited: {:?}", self.language, status);
                }
                Ok(Err(e)) => {
                    tracing::warn!("{} language server wait error: {}", self.language, e);
                }
                Err(_) => {
                    tracing::warn!(
                        "{} language server termination timed out, forcing kill",
                        self.language
                    );
                    if let Err(e) = child.kill().await {
                        tracing::debug!("{} failed to kill process: {}", self.language, e);
                    }
                }
            }
        }

        self.cancel_pending_requests("Server shutdown").await;
        tracing::info!("{} language server stopped", self.language);
        Ok(())
    }

    /// Reject edits whose server versions or open-document contents have changed.
    pub async fn validate_workspace_edit(
        &self,
        edit: &Value,
    ) -> Result<HashMap<PathBuf, String>, LspError> {
        let cache = self.document_cache.read().await;
        let documents = edit
            .get("documentChanges")
            .and_then(Value::as_array)
            .into_iter()
            .flatten();
        for document in documents {
            let Some(text_document) = document.get("textDocument") else {
                continue;
            };
            let Some(uri) = text_document.get("uri").and_then(Value::as_str) else {
                continue;
            };
            if let Some(version) = text_document
                .get("version")
                .filter(|version| !version.is_null())
            {
                let expected = version.as_u64();
                let current = cache
                    .docs
                    .get(uri)
                    .map(|document| u64::from(document.version));
                if expected.is_none() || expected != current {
                    return Err(LspError::ServerError {
                        code: LspError::CONTENT_MODIFIED_ERROR_CODE,
                        message: format!("Document version changed for {uri}"),
                    });
                }
            }
        }
        let uris: Vec<&str> = if let Some(changes) = edit.get("documentChanges") {
            changes
                .as_array()
                .into_iter()
                .flatten()
                .filter_map(|change| {
                    let edits = change.get("edits")?.as_array()?;
                    if edits.is_empty() {
                        return None;
                    }
                    change.get("textDocument")?.get("uri")?.as_str()
                })
                .collect()
        } else {
            edit.get("changes")
                .and_then(Value::as_object)
                .into_iter()
                .flat_map(|changes| {
                    changes.iter().filter_map(|(uri, edits)| {
                        edits
                            .as_array()
                            .is_some_and(|edits| !edits.is_empty())
                            .then_some(uri.as_str())
                    })
                })
                .collect()
        };
        let mut snapshots = HashMap::new();
        for uri in uris {
            let path = crate::models::lsp::uri_to_path(uri);
            let content = std::fs::read_to_string(&path)?;
            if let Some(document) = cache.docs.get(uri)
                && crate::infra::hash_content(&content) != document.content_hash
            {
                return Err(LspError::ServerError {
                    code: LspError::CONTENT_MODIFIED_ERROR_CODE,
                    message: format!("Document content changed for {uri}"),
                });
            }
            snapshots.insert(path, content);
        }
        Ok(snapshots)
    }

    /// Sync the document and return the version the server now knows it
    /// at — the key for judging `publishDiagnostics` freshness.
    ///
    /// When the sync produced a `didChange` — the content on disk moved
    /// under an open overlay — a `textDocument/didSave` follows: every
    /// caller passes content read straight from disk, so the overlay now
    /// matches the saved file, and save is the signal that triggers
    /// save-driven analysis (rust-analyzer runs flycheck on save, so a
    /// warm daemon's diagnostics would otherwise stay unconfirmed after
    /// every edit). A no-op sync sends nothing.
    pub async fn sync_document(&self, uri: &str, content: &str) -> Result<u32, LspError> {
        // Fingerprint the backing file alongside the content being sent,
        // so the drift sweep can later skip unchanged documents on a stat
        // instead of a read. Probed after the caller's read — a write
        // landing between the read and this probe is healed by the next
        // direct sync of the file (every targeted command performs one).
        crate::infra::deadline::run(self.config.request_timeout(), async {
            let disk = DiskState::probe(&crate::models::lsp::uri_to_path(uri)).await;
            let (version, outcome) = self.sync_document_inner(uri, content, disk).await?;
            if outcome == SyncOutcome::Changed {
                self.notify_did_save(uri).await;
            }
            Ok(version)
        })
        .await
    }

    /// Sync `uri`'s overlay to the bytes symora itself just wrote, and
    /// nudge save-driven analysis. Probes the disk fingerprint BEFORE
    /// reading the content, so a racing write can only make the stored
    /// fingerprint older than the content — the next sweep then re-detects
    /// drift (always-detect, never silently-fresh). Sends `didSave` for a
    /// fresh open too: the server has never analyzed the post-edit bytes,
    /// and our own write is exactly a save.
    pub async fn sync_edited_document(&self, file: &std::path::Path) -> Result<(), LspError> {
        crate::infra::deadline::run(self.config.request_timeout(), async {
            let disk = DiskState::probe(file).await;
            let content = tokio::fs::read_to_string(file)
                .await
                .map_err(|e| LspError::Protocol(format!("read {}: {e}", file.display())))?;
            let uri = path_to_uri(file);
            let (_, outcome) = self.sync_document_inner(&uri, &content, disk).await?;
            if outcome != SyncOutcome::Unchanged {
                self.notify_did_save(&uri).await;
                if outcome == SyncOutcome::Opened {
                    self.note_document_changed();
                }
            }
            Ok(())
        })
        .await
    }

    async fn notify_did_save(&self, uri: &str) {
        let _ = self
            .notify(
                "textDocument/didSave",
                Some(serde_json::json!({ "textDocument": { "uri": uri } })),
            )
            .await;
    }

    async fn sync_document_inner(
        &self,
        uri: &str,
        content: &str,
        disk: Option<DiskState>,
    ) -> Result<(u32, SyncOutcome), LspError> {
        let mut cache = self.document_cache.write().await;
        let mut synchronization = DocumentSyncGuard {
            client: self,
            complete: false,
        };
        let (version, outcome, evicted) = {
            let language_id = self
                .language
                .document_id(&crate::models::lsp::uri_to_path(uri));

            if let Some(state) = cache.get_mut(uri) {
                state.disk = disk;
                if state.needs_update(content) {
                    state.update(content);
                    self.note_document_changed();
                    let version = state.version;
                    self.notify(
                        "textDocument/didChange",
                        Some(serde_json::json!({
                            "textDocument": { "uri": uri, "version": version },
                            "contentChanges": [{ "text": content }]
                        })),
                    )
                    .await?;
                    (version, SyncOutcome::Changed, None)
                } else {
                    (state.version, SyncOutcome::Unchanged, None)
                }
            } else {
                let state = DocumentState::new(content, disk);
                let version = state.version;
                self.notify(
                    "textDocument/didOpen",
                    Some(serde_json::json!({
                        "textDocument": {
                            "uri": uri,
                            "languageId": language_id,
                            "version": state.version,
                            "text": content
                        }
                    })),
                )
                .await?;
                (
                    version,
                    SyncOutcome::Opened,
                    cache.insert(uri.to_string(), state),
                )
            }
        };

        drop(cache);
        if let Some(evicted_uri) = evicted {
            let _ = self
                .notify(
                    "textDocument/didClose",
                    Some(serde_json::json!({ "textDocument": { "uri": evicted_uri } })),
                )
                .await;
        }
        synchronization.complete = true;
        Ok((version, outcome))
    }

    /// The document-access invariant, enforced before every request:
    /// each overlay the server holds must match the bytes on disk.
    /// External edits (other tools, git) change files the server has
    /// open without a `didChange`; left alone, every cross-file answer —
    /// references, call hierarchies, rename sites — keeps reflecting
    /// deleted code until that exact file happens to be re-targeted.
    ///
    /// Check-on-access only — no polling, no watcher: a stat per open
    /// document gates a content re-read, and only a real content change
    /// produces a `didChange`. A document whose backing file is gone or
    /// no longer reads as text is closed: an overlay with no bytes
    /// behind it must not keep answering.
    ///
    /// Public so layers that cache workspace-wide answers can run the
    /// sweep before reading `content_generation` — the cache decision
    /// must see post-drift state.
    pub async fn refresh_drifted_overlays(&self) {
        if *self.shutdown.read().await {
            return;
        }
        let open_docs = self.document_cache.read().await.overlay_snapshot();
        for (uri, recorded) in open_docs {
            let path = crate::models::lsp::uri_to_path(&uri);
            let current = DiskState::probe(&path).await;
            if current.is_some() && current == recorded {
                continue;
            }
            match tokio::fs::read_to_string(&path).await {
                Ok(content) => {
                    // `current` was probed BEFORE this read: a write
                    // landing in between can only make the stored
                    // fingerprint older than the content, so the next
                    // sweep re-detects the drift. Probing after the read
                    // would record the write's fingerprint against the
                    // pre-write content — drift hidden until re-target.
                    let outcome = self.sync_document_inner(&uri, &content, current).await;
                    match outcome {
                        Ok((_, SyncOutcome::Changed)) => self.notify_did_save(&uri).await,
                        Ok(_) => {}
                        Err(e) => {
                            tracing::debug!("Failed to re-sync drifted overlay {uri}: {e}")
                        }
                    }
                }
                // Only a confirmed deletion closes the overlay. Any other
                // read error (permissions blip, an atomic-save window where
                // the path is briefly unreadable) is transient: skip this
                // sweep round and let the next request re-check, instead
                // of telling the server real bytes are gone.
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                    self.close_document(&uri).await
                }
                Err(e) => {
                    tracing::debug!("Skipping drift re-sync for {uri} (transient read error: {e})")
                }
            }
        }
    }

    /// Close an overlay whose backing file disappeared. The close is the
    /// honest signal — the server falls back to its own (absent) disk
    /// view instead of answering from bytes that no longer exist.
    ///
    /// Takes the dispatch gate's write guard first: requests on this
    /// client run concurrently (pending map, not a serialized connection),
    /// so an unguarded close could yank the overlay out from under an
    /// in-flight request that is being answered from it.
    async fn close_document(&self, uri: &str) {
        let _gate = self.dispatch_gate.write().await;
        let removed = self.document_cache.write().await.remove(uri);
        if removed {
            let mut synchronization = DocumentSyncGuard {
                client: self,
                complete: false,
            };
            let _ = self
                .notify(
                    "textDocument/didClose",
                    Some(serde_json::json!({ "textDocument": { "uri": uri } })),
                )
                .await;
            self.note_document_changed();
            synchronization.complete = true;
        }
    }

    fn classify_log_level(language: Language, message: &str, msg_type: Option<u64>) -> LogLevel {
        let msg_lower = message.to_lowercase();

        // Filter known noise patterns by language
        if Self::is_noise_message(language, &msg_lower) {
            return LogLevel::Ignore;
        }

        // LSP MessageType: 1=Error, 2=Warning, 3=Info, 4=Log
        match msg_type {
            Some(1) => LogLevel::Error,
            Some(2) => LogLevel::Warn,
            Some(3) => LogLevel::Info,
            _ => {
                // Content-based classification for messages without type
                if msg_lower.contains("error") || msg_lower.contains("exception") {
                    LogLevel::Error
                } else if msg_lower.contains("warning") || msg_lower.contains("warn") {
                    LogLevel::Warn
                } else {
                    LogLevel::Debug
                }
            }
        }
    }

    fn is_noise_message(language: Language, msg: &str) -> bool {
        match language {
            Language::Rust => {
                msg.contains("failed to find any projects")
                    || msg.contains("failed to discover workspace")
            }
            Language::TypeScript | Language::JavaScript => {
                msg.contains("loading typescript") || msg.contains("semantic check completed")
            }
            Language::Python => {
                msg.contains("background analysis") || msg.contains("indexing complete")
            }
            Language::Java => msg.contains("build artifact") || msg.contains("compilation unit"),
            Language::Kotlin => {
                msg.contains("resolving dependencies") || msg.contains("build scripts")
            }
            _ => false,
        }
    }

    async fn handle_server_request(&self, request: Request) {
        let response_result = match request.method.as_str() {
            "workspace/configuration" => self.handle_workspace_configuration(&request.params).await,
            "client/registerCapability" => self.register_capabilities(&request.params),
            "client/unregisterCapability" => self.unregister_capabilities(&request.params),
            "window/workDoneProgress/create" => Ok(serde_json::Value::Null),
            // Null = "no action item chosen" — spec-valid and the only
            // honest answer a headless client can give.
            "window/showMessageRequest" => Ok(serde_json::Value::Null),
            // Refresh requests only mean "please re-pull when convenient";
            // null is the truthful acknowledgement. Rejecting them with
            // METHOD_NOT_FOUND crashes vscode-languageserver-based servers
            // (pyright exits 1 on the error response) — verified by
            // byte-replay against pyright 1.1.410.
            "workspace/semanticTokens/refresh"
            | "workspace/codeLens/refresh"
            | "workspace/inlayHint/refresh"
            | "workspace/inlineValue/refresh"
            | "workspace/diagnostic/refresh"
            | "workspace/foldingRange/refresh" => Ok(serde_json::Value::Null),
            // Anything else stays an honest METHOD_NOT_FOUND — blanket-OK
            // would fake side effects (e.g. workspace/applyEdit claiming
            // an edit was applied).
            _ => {
                tracing::debug!("Unhandled server request: {}", request.method);
                Err(ResponseError {
                    code: error_codes::METHOD_NOT_FOUND,
                    message: format!("Method not found: {}", request.method),
                    data: None,
                })
            }
        };

        let response = match response_result {
            Ok(result) => Response {
                jsonrpc: "2.0".to_string(),
                id: Some(request.id),
                result: Some(result),
                error: None,
            },
            Err(error) => Response {
                jsonrpc: "2.0".to_string(),
                id: Some(request.id),
                result: None,
                error: Some(error),
            },
        };

        // A dropped response wedges the server's pending-request map;
        // wait for the writer instead of silently giving up on contention.
        let stdin_guard = Arc::clone(&self.stdin).lock_owned().await;
        if stdin_guard.is_some()
            && let Err(e) =
                write_response(stdin_guard, &response, self.config.request_timeout()).await
        {
            tracing::warn!("Failed to answer {} from server: {e}", request.method);
        }
    }

    /// Sections use the same settings snapshot throughout a server session.
    async fn handle_workspace_configuration(
        &self,
        params: &Option<Value>,
    ) -> Result<Value, ResponseError> {
        let settings = self.settings.read().await;
        let items: Vec<Value> = params
            .as_ref()
            .and_then(|p| p.get("items"))
            .and_then(|i| i.as_array())
            .map(|arr| {
                arr.iter()
                    .map(|item| {
                        match item.get("section").and_then(|s| s.as_str()) {
                            // An item without a section asks for the whole
                            // settings object (LSP spec).
                            None => settings.clone().unwrap_or_default(),
                            Some(section) => settings
                                .as_ref()
                                .and_then(|s| lookup_section(s, section))
                                .unwrap_or_else(|| Value::Object(serde_json::Map::new())),
                        }
                    })
                    .collect()
            })
            .unwrap_or_default();

        Ok(Value::Array(items))
    }

    fn invalid_registration(message: impl ToString) -> ResponseError {
        ResponseError {
            code: error_codes::INVALID_PARAMS,
            message: message.to_string(),
            data: None,
        }
    }

    fn register_capabilities(&self, params: &Option<Value>) -> Result<Value, ResponseError> {
        let params: RegistrationParams =
            serde_json::from_value(params.clone().unwrap_or(Value::Null))
                .map_err(Self::invalid_registration)?;
        if let Some(registration) = params
            .registrations
            .iter()
            .find(|registration| !self.file_watching || registration.method != WATCHED_FILES_METHOD)
        {
            return Err(Self::invalid_registration(format!(
                "Dynamic registration is not supported for {}",
                registration.method
            )));
        }
        self.watched_files
            .lock()
            .expect("watch registry poisoned")
            .register(&params.registrations)
            .map_err(Self::invalid_registration)?;
        Ok(Value::Null)
    }

    fn unregister_capabilities(&self, params: &Option<Value>) -> Result<Value, ResponseError> {
        let params: UnregistrationParams =
            serde_json::from_value(params.clone().unwrap_or(Value::Null))
                .map_err(Self::invalid_registration)?;
        if let Some(unregistration) = params.unregisterations.iter().find(|unregistration| {
            !self.file_watching || unregistration.method != WATCHED_FILES_METHOD
        }) {
            return Err(Self::invalid_registration(format!(
                "Dynamic registration is not supported for {}",
                unregistration.method
            )));
        }
        let mut watched = self.watched_files.lock().expect("watch registry poisoned");
        for unregistration in params.unregisterations {
            watched.unregister(&unregistration.id);
        }
        Ok(Value::Null)
    }

    /// Tell the server about the changes on disk its watchers asked for.
    pub async fn did_change_watched_files(&self, changes: &[FileChange]) {
        let events = self
            .watched_files
            .lock()
            .expect("watch registry poisoned")
            .select(changes);
        if events.is_empty() {
            return;
        }
        let params = serde_json::to_value(DidChangeWatchedFilesParams { changes: events })
            .expect("watched-file events serialize");
        if let Err(e) = self.notify(WATCHED_FILES_METHOD, Some(params)).await {
            tracing::debug!("{} missed watched-file changes: {e}", self.language);
        }
    }

    pub fn config(&self) -> &crate::config::LspRuntimeConfig {
        &self.config
    }

    pub fn language(&self) -> Language {
        self.language
    }

    /// The position encoding negotiated at initialize (utf-16 until then).
    pub async fn position_encoding(&self) -> PositionEncoding {
        *self.position_encoding.read().await
    }

    /// Whether the server's advertised capabilities affirmatively offer
    /// `feature`. False before initialize and when the provider is absent or
    /// `false` — the single binary capability signal the gating uses.
    pub async fn feature_advertised(&self, feature: crate::infra::lsp::LspFeature) -> bool {
        self.capabilities
            .read()
            .await
            .as_ref()
            .is_some_and(|init| init.capabilities.advertises(feature))
    }

    /// Latest `publishDiagnostics` for `uri`, or `None` when the server
    /// has never published for it — callers judge freshness against the
    /// version returned by `sync_document` (and `publish_seq_snapshot`
    /// for servers that omit versions).
    pub async fn published_diagnostics(&self, uri: &str) -> Option<PublishedDiagnostics> {
        self.diagnostics.read().await.get(uri).cloned()
    }

    /// Current publish arrival counter. Snapshot it before a sync to
    /// recognize publishes that arrived afterwards.
    pub fn publish_seq_snapshot(&self) -> u64 {
        self.publish_seq.load(Ordering::Acquire)
    }
}

impl Drop for LspClient {
    fn drop(&mut self) {
        if let Ok(mut process_guard) = self.process.try_lock() {
            if let Some(ref mut child) = *process_guard {
                let _ = child.start_kill();
                tracing::debug!("LspClient for {} dropped, process killed", self.language);
            }
        } else {
            tracing::warn!(
                "LspClient for {} dropped but could not acquire lock - potential zombie process",
                self.language
            );
        }
    }
}

#[cfg(test)]
mod test_support {
    use super::*;

    pub(super) fn test_client() -> Arc<LspClient> {
        LspClient::new(
            Language::Rust,
            PathBuf::from("/tmp"),
            Arc::new(crate::config::LspRuntimeConfig::default()),
            false,
        )
    }

    pub(super) async fn notify_client(
        client: &Arc<LspClient>,
        method: &str,
        params: serde_json::Value,
    ) {
        client
            .handle_message(Message::Notification(Notification::new(
                method,
                Some(params),
            )))
            .await;
    }
}

#[cfg(test)]
mod tests {
    use super::test_support::{notify_client, test_client};
    use super::*;

    #[cfg(unix)]
    #[tokio::test]
    async fn configuration_reaches_initialization_notifications_and_section_pulls() {
        let root = tempfile::tempdir().unwrap();
        let settings = serde_json::json!({"python": {"analysis": {
            "extraPaths": ["packages/core/src"], "autoSearchPaths": false
        }}});
        let mut config = crate::config::LspRuntimeConfig::default();
        config.servers.insert(
            "python".into(),
            crate::models::config::ServerOverride {
                initialization_options: Some(
                    serde_json::json!({"client": "configured"})
                        .as_object()
                        .unwrap()
                        .clone(),
                ),
                settings: Some(settings.as_object().unwrap().clone()),
                ..Default::default()
            },
        );
        let client = LspClient::new(
            Language::Python,
            root.path().to_path_buf(),
            Arc::new(config),
            false,
        );
        let launch = super::super::servers::ServerLaunch {
            command: PathBuf::from("python3"),
            args: vec![
                "-u".into(),
                "-c".into(),
                include_str!("../../../../tests/fixtures/lsp/configuration_server.py").into(),
            ],
            env: Default::default(),
        };
        client.start(&launch, || {}).await.unwrap();
        let observed: Value = client.request("test/configuration", None).await.unwrap();
        assert_eq!(observed["initialization_options"]["client"], "configured");
        assert!(observed["initialization_options"].get("python").is_some());
        assert_eq!(observed["notification"], observed["pull"][0]);
        assert_eq!(
            observed["pull"][0]["python"]["analysis"]["extraPaths"],
            settings["python"]["analysis"]["extraPaths"]
        );
        assert_eq!(observed["pull"][1]["autoSearchPaths"], false);
        assert_eq!(observed["pull"][1]["useLibraryCodeForTypes"], true);
        assert_eq!(observed["pull"][2], serde_json::json!({}));
        client.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn server_messages_and_failed_notifications_do_not_mark_use() {
        let client = test_client();
        let stamp = client.usage().last_used;
        notify_client(
            &client,
            "window/logMessage",
            serde_json::json!({"type": 3, "message": "ready"}),
        )
        .await;
        client
            .handle_message(Message::Request(Request::new(
                42,
                "workspace/configuration",
                Some(serde_json::json!({"items": []})),
            )))
            .await;
        assert!(
            client
                .notify("workspace/didChangeWatchedFiles", None)
                .await
                .is_err()
        );
        assert!(!client.usage().in_flight);
        assert_eq!(client.usage().last_used, stamp);
    }

    #[tokio::test]
    async fn a_request_error_releases_and_refreshes_use() {
        let client = test_client();
        let stamp = client.usage().last_used;
        assert!(matches!(
            client.request::<Value>("textDocument/hover", None).await,
            Err(LspError::NotConnected)
        ));
        assert!(!client.usage().in_flight);
        assert!(client.usage().last_used > stamp);
        assert!(client.pending.lock().unwrap().is_empty());
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn abandoning_a_sent_request_removes_pending_state_and_cancels_the_server_work() {
        let client = test_client();
        let mut echo = Command::new("cat")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .kill_on_drop(true)
            .spawn()
            .unwrap();
        *client.stdin.lock().await = echo.stdin.take();
        let mut server = Transport::new(echo.stdout.take().unwrap());
        let request = {
            let client = Arc::clone(&client);
            tokio::spawn(async move { client.request::<Value>("textDocument/hover", None).await })
        };
        let Message::Request(sent) = server.read_message().await.unwrap() else {
            panic!("expected a request");
        };
        assert_eq!(client.pending.lock().unwrap().len(), 1);
        request.abort();
        assert!(request.await.unwrap_err().is_cancelled());
        assert!(client.pending.lock().unwrap().is_empty());
        assert!(!client.usage().in_flight);
        let Message::Notification(cancelled) =
            tokio::time::timeout(Duration::from_secs(5), server.read_message())
                .await
                .unwrap()
                .unwrap()
        else {
            panic!("expected cancellation");
        };
        assert_eq!(cancelled.method, "$/cancelRequest");
        assert_eq!(
            cancelled.params.unwrap()["id"],
            serde_json::to_value(sent.id).unwrap()
        );
        drop(client);
        echo.wait().await.unwrap();
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn an_expired_shared_budget_cancels_a_sent_request() {
        let client = test_client();
        let mut echo = Command::new("cat")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .kill_on_drop(true)
            .spawn()
            .unwrap();
        *client.stdin.lock().await = echo.stdin.take();
        let mut server = Transport::new(echo.stdout.take().unwrap());
        let operation = {
            let client = Arc::clone(&client);
            tokio::spawn(crate::infra::deadline::scope(
                Duration::from_secs(10),
                async move { client.request::<Value>("textDocument/hover", None).await },
            ))
        };
        let Message::Request(sent) = server.read_message().await.unwrap() else {
            panic!("request expected");
        };
        tokio::time::pause();
        tokio::time::advance(Duration::from_secs(11)).await;
        assert!(matches!(
            operation.await.unwrap(),
            Err(LspError::Timeout(_))
        ));
        tokio::time::resume();
        assert!(client.pending.lock().unwrap().is_empty());
        assert!(!client.usage().in_flight);
        let Message::Notification(cancel) =
            tokio::time::timeout(Duration::from_secs(5), server.read_message())
                .await
                .unwrap()
                .unwrap()
        else {
            panic!("cancellation expected");
        };
        assert_eq!(cancel.method, "$/cancelRequest");
        assert_eq!(
            cancel.params.unwrap()["id"],
            serde_json::to_value(sent.id).unwrap()
        );
        drop(client);
        echo.wait().await.unwrap();
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn cancellation_survives_its_caller_while_waiting_for_the_writer() {
        let client = test_client();
        let mut echo = Command::new("cat")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .kill_on_drop(true)
            .spawn()
            .unwrap();
        *client.stdin.lock().await = echo.stdin.take();
        let mut server = Transport::new(echo.stdout.take().unwrap());
        let (sender, _receiver) = oneshot::channel();
        client
            .pending
            .lock()
            .unwrap()
            .insert(RequestId::Number(42), sender);
        let held = client.stdin.lock().await;
        let cancellation = {
            let client = Arc::clone(&client);
            tokio::spawn(async move { client.cancel_request(42).await })
        };
        tokio::time::timeout(Duration::from_secs(5), async {
            while !client.pending.lock().unwrap().is_empty() {
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        cancellation.abort();
        assert!(cancellation.await.unwrap_err().is_cancelled());
        drop(held);
        let Message::Notification(cancelled) =
            tokio::time::timeout(Duration::from_secs(5), server.read_message())
                .await
                .unwrap()
                .unwrap()
        else {
            panic!("expected cancellation");
        };
        assert_eq!(cancelled.method, "$/cancelRequest");
        assert_eq!(cancelled.params.unwrap()["id"], 42);
        drop(client);
        echo.wait().await.unwrap();
    }

    #[tokio::test(start_paused = true)]
    async fn notifications_and_document_sync_bound_lock_waits() {
        let client = test_client();
        let writer = client.stdin.lock().await;
        let notification = crate::infra::deadline::scope(Duration::from_secs(1), async {
            client.notify("textDocument/didSave", None).await
        })
        .await;
        assert!(matches!(notification, Err(LspError::Timeout(_))));
        drop(writer);

        let cache = client.document_cache.write().await;
        let synchronization = crate::infra::deadline::scope(Duration::from_secs(1), async {
            client
                .sync_document("file:///missing.rs", "fn work() {}")
                .await
        })
        .await;
        assert!(matches!(synchronization, Err(LspError::Timeout(_))));
        drop(cache);
        assert!(!client.terminated.load(Ordering::Acquire));
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn an_interrupted_document_sync_retires_the_uncertain_session() {
        use tokio::io::AsyncReadExt;

        let client = test_client();
        let mut echo = Command::new("cat")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .kill_on_drop(true)
            .spawn()
            .unwrap();
        *client.stdin.lock().await = echo.stdin.take();
        let mut reader = echo.stdout.take().unwrap();
        let synchronization = {
            let client = Arc::clone(&client);
            tokio::spawn(async move {
                client
                    .sync_document("file:///tmp/uncertain.rs", &"x".repeat(1024 * 1024))
                    .await
            })
        };
        reader.read_u8().await.unwrap();
        synchronization.abort();
        assert!(synchronization.await.unwrap_err().is_cancelled());
        assert!(client.terminated.load(Ordering::Acquire));
        assert!(!client.is_running().await);
        echo.kill().await.unwrap();
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn a_live_process_with_closed_input_is_not_a_reusable_session() {
        let client = test_client();
        let mut child = Command::new("sleep")
            .arg("20")
            .stdin(Stdio::piped())
            .kill_on_drop(true)
            .spawn()
            .unwrap();
        *client.stdin.lock().await = child.stdin.take();
        *client.process.lock().await = Some(child);
        assert!(client.is_running().await);
        client.stdin.lock().await.take();
        assert!(!client.is_running().await);
        let mut child = client.process.lock().await.take().unwrap();
        assert!(child.try_wait().unwrap().is_none());
        child.kill().await.unwrap();
    }

    #[tokio::test]
    async fn a_request_waiting_for_dispatch_cannot_enter_a_retired_session() {
        let client = test_client();
        let gate = client.dispatch_gate.write().await;
        let request = {
            let client = Arc::clone(&client);
            tokio::spawn(async move { client.request::<Value>("textDocument/hover", None).await })
        };
        tokio::time::timeout(Duration::from_secs(5), async {
            while !client.usage().in_flight {
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        client.cancel_pending_requests_terminated();
        drop(gate);
        assert!(matches!(
            request.await.unwrap(),
            Err(LspError::ServerTerminated { .. })
        ));
        assert!(client.pending.lock().unwrap().is_empty());
    }

    #[test]
    fn registration_batches_reject_unsupported_methods_before_mutating_watchers() {
        use super::super::protocol::FileChangeType;

        let client = LspClient::new(
            Language::Python,
            PathBuf::from("/w"),
            Arc::new(crate::config::LspRuntimeConfig::default()),
            true,
        );
        let watching = serde_json::json!({
            "id": "python", "method": WATCHED_FILES_METHOD,
            "registerOptions": { "watchers": [{ "globPattern": "**/*.py" }] }
        });
        let unsupported = serde_json::json!({
            "id": "calls", "method": "textDocument/prepareCallHierarchy"
        });
        let selected = || {
            client
                .watched_files
                .lock()
                .unwrap()
                .select(&[FileChange {
                    path: PathBuf::from("/w/main.py"),
                    change: FileChangeType::Changed,
                }])
                .len()
        };
        let error = client
            .register_capabilities(&Some(serde_json::json!({
                "registrations": [watching, unsupported]
            })))
            .unwrap_err();
        assert_eq!(error.code, error_codes::INVALID_PARAMS);
        assert_eq!(selected(), 0);

        assert!(
            client
                .register_capabilities(&Some(serde_json::json!({
                    "registrations": [watching]
                })))
                .is_ok()
        );
        assert_eq!(selected(), 1);
        let error = client
            .unregister_capabilities(&Some(serde_json::json!({
                "unregisterations": [watching, unsupported]
            })))
            .unwrap_err();
        assert_eq!(error.code, error_codes::INVALID_PARAMS);
        assert_eq!(selected(), 1);
        assert!(client.unregister_capabilities(&None).is_err());
        assert!(client.register_capabilities(&None).is_err());
        assert_eq!(selected(), 1);
        assert!(
            client
                .unregister_capabilities(&Some(serde_json::json!({
                    "unregisterations": [watching]
                })))
                .is_ok()
        );
        assert_eq!(selected(), 0);

        assert!(
            test_client()
                .register_capabilities(&Some(serde_json::json!({
                    "registrations": [watching]
                })))
                .is_err()
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn static_call_hierarchy_is_negotiated_when_dynamic_registration_is_unavailable() {
        use super::super::protocol::LspCallHierarchyItem;

        let root = tempfile::tempdir().unwrap();
        let mut config = crate::models::config::SymoraConfig::default();
        config.lsp.timeout_secs = 3;
        let client = LspClient::new(
            Language::Python,
            root.path().to_path_buf(),
            Arc::new(crate::config::LspRuntimeConfig::from(&config)),
            false,
        );
        let launch = super::super::servers::ServerLaunch {
            command: PathBuf::from("python3"),
            args: vec![
                "-u".to_string(),
                "-c".to_string(),
                include_str!("../../../../tests/fixtures/lsp/capability_server.py").to_string(),
            ],
            env: Default::default(),
        };
        client.start(&launch, || {}).await.unwrap();
        assert!(
            client
                .feature_advertised(crate::infra::lsp::LspFeature::IncomingCalls)
                .await
        );
        let items: Vec<LspCallHierarchyItem> = client
            .request(
                "textDocument/prepareCallHierarchy",
                Some(serde_json::json!({
                    "textDocument": { "uri": path_to_uri(&root.path().join("main.py")) },
                    "position": { "line": 0, "character": 4 }
                })),
            )
            .await
            .unwrap();
        assert_eq!(items.len(), 1);
        let calls: Vec<Value> = client
            .request(
                "callHierarchy/incomingCalls",
                Some(serde_json::json!({ "item": items[0] })),
            )
            .await
            .unwrap();
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0]["from"]["name"], "caller");
        client.shutdown().await.unwrap();
    }

    /// Every content change advances the workspace generation, so caches
    /// validated against it can never serve a pre-edit answer.
    #[test]
    fn content_changes_advance_the_workspace_generation() {
        let client = test_client();
        let before = content_generation();
        client.note_document_changed();
        assert!(content_generation() > before);
    }
}
