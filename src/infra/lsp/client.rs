use std::collections::HashMap;
use std::path::PathBuf;
use std::process::Stdio;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU8, AtomicU64, Ordering};
use std::time::Duration;

use serde_json::Value;
use tokio::process::{Child, ChildStdin, Command};
use tokio::sync::{Mutex, Notify, RwLock, oneshot};
use tokio::time::timeout;

use super::init_options::init_options;
use super::protocol::{
    ClientCapabilities, ClientInfo, GeneralClientCapabilities, InitializeParams, InitializeResult,
    LspDiagnostic, Message, Notification, Position, PositionEncoding, RegularExpressionsCapability,
    Request, RequestId, Response, ResponseError, StaleRequestSupport,
    TextDocumentClientCapabilities, TextDocumentIdentifier, TextDocumentPositionParams,
    WindowClientCapabilities, WorkspaceClientCapabilities, error_codes,
};
use super::transport::{Transport, write_notification, write_request, write_response};
use crate::error::LspError;
use crate::models::lsp::path_to_uri;
use crate::models::symbol::Language;

type PendingRequest = oneshot::Sender<Response>;
type NotificationHandler = Box<dyn Fn(serde_json::Value) + Send + Sync>;

const MAX_OPEN_DOCUMENTS: usize = 100;
const MAX_DIAGNOSTICS_CACHE: usize = 200;

/// Monotonic workspace-content generation. Bumped whenever any client
/// learns that content changed — a `didChange` from our own edits or the
/// drift sweep, a `didClose` of a vanished file — and when a client
/// session starts (a fresh server is a fresh world). Caches of
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
/// overlay — symora's own write to a file no server has open. Caches of
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

/// Workspace-indexing readiness, derived from server signals.
///
/// `Ready` is reached either through an explicit quiescence signal (a status
/// notification, a drained set of indexing-progress tokens, a known readiness
/// log line) OR by a wait elapsing without the server ever signalling activity
/// — a server with no indexing phase (or none it exposes) is ready, not
/// degraded. `TimedOut` is reached ONLY when a wait elapses AFTER the server
/// signalled it was indexing (`InProgress`): a genuine lower bound, disclosed
/// via the `indexing` marker. That distinction — real activity then timeout vs.
/// silence then timeout — is what keeps an unsignalled server from falsely
/// reporting every complete result as degraded, with no per-language allow-list.
/// The state moves only through the transition table in
/// [`IndexingState::on_event`], applied atomically (compare-and-swap) so a
/// racing signal can never be stomped by a blind store.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum IndexingState {
    NotStarted = 0,
    InProgress = 1,
    Ready = 2,
    TimedOut = 3,
}

/// The events that may move [`IndexingState`]. Every state change goes
/// through `IndexingState::on_event` — there is no other write path — so
/// the legal transitions live in exactly one testable place.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IndexingEvent {
    /// A bounded wait is claiming the session (start of
    /// `await_indexing_signal`). A pure no-op on the state — waiting is not
    /// evidence the server is indexing.
    WaitStarted,
    /// The bounded wait's budget expired. Concludes `InProgress` as a degraded
    /// lower bound (`TimedOut`); from `NotStarted` (no activity ever seen) it
    /// resolves to `Ready` — there was no indexing phase to wait for.
    WaitTimedOut,
    /// The server signalled a fully analyzed workspace (quiescent=true,
    /// a drained progress-token set, a known readiness log line).
    ServerQuiescent,
    /// The server signalled it is (re)working: `experimental/serverStatus`
    /// busy, or an indexing-progress `begin`. The only signal that opens
    /// `InProgress` — the evidence a timeout needs to mean a real lower bound.
    ServerBusy,
}

impl IndexingState {
    fn from_u8(value: u8) -> Self {
        match value {
            1 => Self::InProgress,
            2 => Self::Ready,
            3 => Self::TimedOut,
            _ => Self::NotStarted,
        }
    }

    fn to_u8(self) -> u8 {
        self as u8
    }

    pub fn is_usable(self) -> bool {
        matches!(self, Self::Ready | Self::TimedOut)
    }

    /// The legal transition table. Anything not listed is a no-op, which
    /// is what makes the racy interleavings safe:
    ///
    /// - Only a quiescence signal ever produces `Ready`.
    /// - A busy signal re-opens `InProgress` from `Ready` (the server is
    ///   genuinely re-working) but NOT from `TimedOut`: a timed-out index
    ///   that is still incomplete must keep its disclosed marker until
    ///   quiescence proves completion.
    /// - `WaitStarted` is a pure no-op: claiming a wait is NOT evidence the
    ///   server is indexing. `InProgress` is reached only by a REAL activity
    ///   signal (`ServerBusy`, fired by `experimental/serverStatus` busy or an
    ///   indexing-progress `begin`). This is the difference between "the server
    ///   told us it is working" and "a query happened to wait".
    /// - A timeout that concludes an in-flight `InProgress` is a genuine lower
    ///   bound -> `TimedOut` (disclosed via the `indexing` marker). A timeout
    ///   from `NotStarted` — the wait elapsed without the server EVER signalling
    ///   activity — means there is no indexing phase to wait for (or none this
    ///   server exposes), so it resolves to `Ready`, NOT `TimedOut`. This is
    ///   what stops a server with no readiness signal from falsely marking every
    ///   complete result as degraded, with no per-language allow-list.
    pub fn on_event(self, event: IndexingEvent) -> IndexingState {
        use IndexingEvent::*;
        use IndexingState::*;
        match (self, event) {
            (_, ServerQuiescent) => Ready,
            (TimedOut, ServerBusy) => TimedOut,
            (_, ServerBusy) => InProgress,
            (state, WaitStarted) => state,
            (InProgress, WaitTimedOut) => TimedOut,
            (NotStarted, WaitTimedOut) => Ready,
            (state, WaitTimedOut) => state,
        }
    }
}

pub struct LspClient {
    language: Language,
    process: Mutex<Option<Child>>,
    stdin: Mutex<Option<ChildStdin>>,
    next_id: AtomicU64,
    pending: RwLock<HashMap<RequestId, PendingRequest>>,
    diagnostics: RwLock<HashMap<String, PublishedDiagnostics>>,
    publish_seq: AtomicU64,
    document_cache: RwLock<DocumentCache>,
    notification_handlers: RwLock<HashMap<String, NotificationHandler>>,
    root: PathBuf,
    config: Arc<crate::config::LspRuntimeConfig>,
    capabilities: RwLock<Option<InitializeResult>>,
    /// The negotiated position encoding, parsed once from the initialize
    /// response. Every inbound/outbound position conversion reads this.
    position_encoding: RwLock<PositionEncoding>,
    shutdown: RwLock<bool>,
    indexing_state: AtomicU8,
    indexing_notify: Notify,
    /// Serializes the drift sweep's `didClose` against in-flight requests:
    /// each request holds a read guard for its full dispatch window, and
    /// closing a vanished overlay takes the write guard — so an overlay is
    /// never closed out from under a request that may be answering from it.
    /// Requests are NOT serialized through one connection (the pending map
    /// allows true concurrency), which is why the gate exists.
    dispatch_gate: RwLock<()>,
    terminated: AtomicBool,
    cross_file_waited: AtomicBool,
    /// Set once the server demonstrates an explicit status protocol
    /// (`experimental/serverStatus`). From then on the fuzzy progress and
    /// log-line readiness heuristics stand down — a heuristic must never
    /// overrule a server that can state readiness precisely.
    status_channel_seen: AtomicBool,
    /// The initializationOptions payload, kept as the single source of
    /// truth for settings: servers that pull configuration at runtime
    /// (`workspace/configuration` — pyright reads `python.pythonPath`
    /// this way, not from initializationOptions) are answered from the
    /// same payload instead of empty objects that wipe their settings.
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
    ) -> Arc<Self> {
        bump_content_generation();
        Arc::new(Self {
            language,
            process: Mutex::new(None),
            stdin: Mutex::new(None),
            next_id: AtomicU64::new(1),
            pending: RwLock::new(HashMap::new()),
            diagnostics: RwLock::new(HashMap::new()),
            publish_seq: AtomicU64::new(0),
            document_cache: RwLock::new(DocumentCache::new()),
            notification_handlers: RwLock::new(HashMap::new()),
            root,
            config,
            capabilities: RwLock::new(None),
            position_encoding: RwLock::new(PositionEncoding::default()),
            shutdown: RwLock::new(false),
            indexing_state: AtomicU8::new(IndexingState::NotStarted.to_u8()),
            indexing_notify: Notify::new(),
            dispatch_gate: RwLock::new(()),
            terminated: AtomicBool::new(false),
            cross_file_waited: AtomicBool::new(false),
            status_channel_seen: AtomicBool::new(false),
            settings: RwLock::new(None),
        })
    }

    /// Start the language server
    pub async fn start(self: &Arc<Self>, command: &str, args: &[String]) -> Result<(), LspError> {
        // Check if already running
        if self.is_running().await {
            return Ok(());
        }

        tracing::info!(
            "Starting {} language server: {} {:?}",
            self.language,
            command,
            args
        );

        // Spawn server process
        let mut child = Command::new(command)
            .args(args)
            .current_dir(&self.root)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true)
            .spawn()
            .map_err(|e| LspError::ServerStart(format!("{}: {}", command, e)))?;

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

        // Start response reader task
        let client = Arc::clone(self);
        tokio::spawn(async move {
            client.read_responses(Transport::new(stdout)).await;
        });

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

        // Register notification handlers before initialization
        self.register_default_handlers().await;

        // Initialize the server
        self.initialize().await?;

        tracing::info!("{} language server started successfully", self.language);
        Ok(())
    }

    /// Check if server is running
    pub async fn is_running(&self) -> bool {
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
    async fn initialize(&self) -> Result<(), LspError> {
        let init_options = init_options(self.language, &self.root);
        *self.settings.write().await = init_options.clone();

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
            capabilities: Self::client_capabilities(self.language),
            client_info: Some(ClientInfo {
                name: "symora".to_string(),
                version: Some(env!("CARGO_PKG_VERSION").to_string()),
            }),
            initialization_options: init_options,
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

        // Send initialized notification
        self.notify("initialized", Some(serde_json::json!({})))
            .await
            .map_err(|e| self.handshake_failure(e))?;

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

    /// Build client capabilities optimized for the target language server (LSP 3.17 complete)
    fn client_capabilities(language: Language) -> ClientCapabilities {
        let general = GeneralClientCapabilities {
            position_encodings: Some(vec!["utf-8".to_string(), "utf-16".to_string()]),
            stale_request_support: Some(StaleRequestSupport {
                cancel: true,
                retry_on_content_modified: Some(vec![
                    "textDocument/semanticTokens/full".to_string(),
                    "textDocument/semanticTokens/range".to_string(),
                    "textDocument/semanticTokens/full/delta".to_string(),
                ]),
            }),
            regular_expressions: Some(RegularExpressionsCapability {
                engine: "ECMAScript".to_string(),
                version: Some("ES2020".to_string()),
            }),
        };

        let window = WindowClientCapabilities {
            work_done_progress: Some(true),
            show_message: Some(serde_json::json!({
                "messageActionItem": {
                    "additionalPropertiesSupport": true
                }
            })),
            show_document: Some(serde_json::json!({
                "support": true
            })),
        };

        let text_document = TextDocumentClientCapabilities {
            synchronization: Some(serde_json::json!({
                "dynamicRegistration": true,
                "willSave": true,
                "willSaveWaitUntil": true,
                "didSave": true
            })),
            completion: Some(Self::completion_capabilities(language)),
            hover: Some(serde_json::json!({
                "dynamicRegistration": true,
                "contentFormat": ["markdown", "plaintext"]
            })),
            signature_help: Some(serde_json::json!({
                "dynamicRegistration": true,
                "signatureInformation": {
                    "documentationFormat": ["markdown", "plaintext"],
                    "parameterInformation": { "labelOffsetSupport": true },
                    "activeParameterSupport": true
                },
                "contextSupport": true
            })),
            declaration: Some(serde_json::json!({
                "dynamicRegistration": true,
                "linkSupport": true
            })),
            definition: Some(serde_json::json!({
                "dynamicRegistration": true,
                "linkSupport": true
            })),
            type_definition: Some(serde_json::json!({
                "dynamicRegistration": true,
                "linkSupport": true
            })),
            implementation: Some(serde_json::json!({
                "dynamicRegistration": true,
                "linkSupport": true
            })),
            references: Some(serde_json::json!({
                "dynamicRegistration": true
            })),
            document_highlight: Some(serde_json::json!({
                "dynamicRegistration": true
            })),
            document_symbol: Some(serde_json::json!({
                "dynamicRegistration": true,
                "symbolKind": {
                    "valueSet": (1..=26).collect::<Vec<_>>()
                },
                "hierarchicalDocumentSymbolSupport": true,
                "tagSupport": { "valueSet": [1] },
                "labelSupport": true
            })),
            code_action: Some(serde_json::json!({
                "dynamicRegistration": true,
                "isPreferredSupport": true,
                "disabledSupport": true,
                "dataSupport": true,
                "resolveSupport": {
                    "properties": ["edit"]
                },
                "codeActionLiteralSupport": {
                    "codeActionKind": {
                        "valueSet": [
                            "", "quickfix", "refactor", "refactor.extract", "refactor.inline",
                            "refactor.rewrite", "source", "source.organizeImports", "source.fixAll"
                        ]
                    }
                },
                "honorsChangeAnnotations": true
            })),
            code_lens: Some(serde_json::json!({
                "dynamicRegistration": true
            })),
            document_link: Some(serde_json::json!({
                "dynamicRegistration": true,
                "tooltipSupport": true
            })),
            color_provider: Some(serde_json::json!({
                "dynamicRegistration": true
            })),
            formatting: Some(serde_json::json!({
                "dynamicRegistration": true
            })),
            range_formatting: Some(serde_json::json!({
                "dynamicRegistration": true
            })),
            on_type_formatting: Some(serde_json::json!({
                "dynamicRegistration": true
            })),
            rename: Some(serde_json::json!({
                "dynamicRegistration": true,
                "prepareSupport": true,
                "prepareSupportDefaultBehavior": 1,
                "honorsChangeAnnotations": true
            })),
            publish_diagnostics: Some(serde_json::json!({
                "relatedInformation": true,
                "tagSupport": { "valueSet": [1, 2] },
                "versionSupport": true,
                "codeDescriptionSupport": true,
                "dataSupport": true
            })),
            folding_range: Some(serde_json::json!({
                "dynamicRegistration": true,
                "rangeLimit": 5000,
                "lineFoldingOnly": false,
                "foldingRangeKind": {
                    "valueSet": ["comment", "imports", "region"]
                },
                "foldingRange": { "collapsedText": true }
            })),
            selection_range: Some(serde_json::json!({
                "dynamicRegistration": true
            })),
            linked_editing_range: Some(serde_json::json!({
                "dynamicRegistration": true
            })),
            call_hierarchy: Some(serde_json::json!({
                "dynamicRegistration": true
            })),
            semantic_tokens: Some(serde_json::json!({
                "dynamicRegistration": true,
                "requests": {
                    "range": true,
                    "full": { "delta": true }
                },
                "tokenTypes": [
                    "namespace", "type", "class", "enum", "interface", "struct", "typeParameter",
                    "parameter", "variable", "property", "enumMember", "event", "function",
                    "method", "macro", "keyword", "modifier", "comment", "string", "number",
                    "regexp", "operator", "decorator"
                ],
                "tokenModifiers": [
                    "declaration", "definition", "readonly", "static", "deprecated", "abstract",
                    "async", "modification", "documentation", "defaultLibrary"
                ],
                "formats": ["relative"],
                "overlappingTokenSupport": false,
                "multilineTokenSupport": true,
                "serverCancelSupport": true,
                "augmentsSyntaxTokens": true
            })),
            moniker: Some(serde_json::json!({
                "dynamicRegistration": true
            })),
            type_hierarchy: Some(serde_json::json!({
                "dynamicRegistration": true
            })),
            inline_value: Some(serde_json::json!({
                "dynamicRegistration": true
            })),
            inlay_hint: Some(serde_json::json!({
                "dynamicRegistration": true,
                "resolveSupport": {
                    "properties": ["tooltip", "textEdits", "label.tooltip", "label.location", "label.command"]
                }
            })),
            diagnostic: Some(serde_json::json!({
                "dynamicRegistration": true,
                "relatedDocumentSupport": true
            })),
        };

        let workspace = WorkspaceClientCapabilities {
            apply_edit: Some(true),
            workspace_edit: Some(serde_json::json!({
                "documentChanges": true,
                "resourceOperations": ["create", "rename", "delete"],
                "failureHandling": "textOnlyTransactional",
                "normalizesLineEndings": true,
                "changeAnnotationSupport": { "groupsOnLabel": true }
            })),
            did_change_configuration: Some(serde_json::json!({
                "dynamicRegistration": true
            })),
            did_change_watched_files: Some(serde_json::json!({
                "dynamicRegistration": true,
                "relativePatternSupport": true
            })),
            symbol: Some(serde_json::json!({
                "dynamicRegistration": true,
                "symbolKind": {
                    "valueSet": (1..=26).collect::<Vec<_>>()
                },
                "tagSupport": { "valueSet": [1] },
                "resolveSupport": { "properties": ["location.range"] }
            })),
            execute_command: Some(serde_json::json!({
                "dynamicRegistration": true
            })),
            workspace_folders: Some(true),
            configuration: Some(true),
            semantic_tokens: Some(serde_json::json!({
                "refreshSupport": true
            })),
            code_lens: Some(serde_json::json!({
                "refreshSupport": true
            })),
            file_operations: Some(serde_json::json!({
                "dynamicRegistration": true,
                "didCreate": true,
                "willCreate": true,
                "didRename": true,
                "willRename": true,
                "didDelete": true,
                "willDelete": true
            })),
            inline_value: Some(serde_json::json!({
                "refreshSupport": true
            })),
            inlay_hint: Some(serde_json::json!({
                "refreshSupport": true
            })),
            diagnostics: Some(serde_json::json!({
                "refreshSupport": true
            })),
        };

        ClientCapabilities {
            general: Some(general),
            window: Some(window),
            text_document: Some(text_document),
            workspace: Some(workspace),
            // `serverStatusNotification` asks rust-analyzer to send
            // `experimental/serverStatus` — the authoritative quiescent
            // signal the readiness state machine prefers over progress
            // titles. Other servers ignore unknown experimental keys.
            experimental: Some(serde_json::json!({
                "serverStatusNotification": true
            })),
        }
    }

    /// Build completion capabilities based on language
    fn completion_capabilities(language: Language) -> serde_json::Value {
        let snippet_support = !matches!(language, Language::Kotlin);

        serde_json::json!({
            "dynamicRegistration": true,
            "contextSupport": true,
            "completionItem": {
                "snippetSupport": snippet_support,
                "commitCharactersSupport": true,
                "documentationFormat": ["markdown", "plaintext"],
                "deprecatedSupport": true,
                "preselectSupport": true,
                "tagSupport": { "valueSet": [1] },
                "insertReplaceSupport": false,
                "resolveSupport": {
                    "properties": ["documentation", "detail", "additionalTextEdits"]
                },
                "insertTextModeSupport": { "valueSet": [1, 2] },
                "labelDetailsSupport": true
            },
            "insertTextMode": 2,
            "completionItemKind": {
                "valueSet": (1..=25).collect::<Vec<_>>()
            },
            "completionList": {
                "itemDefaults": ["commitCharacters", "editRange", "insertTextFormat", "insertTextMode"]
            }
        })
    }

    /// Send a request and wait for response
    pub async fn request<T: serde::de::DeserializeOwned>(
        &self,
        method: &str,
        params: Option<Value>,
    ) -> Result<T, LspError> {
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
            let mut pending = self.pending.write().await;
            pending.insert(RequestId::Number(id), tx);
        }

        // Build and send request
        let request = Request::new(id, method, params);

        tracing::trace!("{} LSP request {}: {}", self.language, id, method);

        {
            let mut stdin_guard = self.stdin.lock().await;
            let stdin = stdin_guard.as_mut().ok_or(LspError::NotConnected)?;
            write_request(stdin, &request).await?;
        }

        let result = timeout(self.config.timeout_for(self.language, method), rx).await;

        match result {
            Ok(Ok(response)) => match response.into_result() {
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
            Ok(Err(_)) => Err(LspError::RequestCancelled),
            Err(_) => {
                self.cancel_request(id).await;
                Err(LspError::Timeout(format!(
                    "{:?} '{}' timed out. The language server may be busy or unresponsive",
                    self.language, method
                )))
            }
        }
    }

    pub async fn cancel_request(&self, id: u64) {
        {
            let mut pending = self.pending.write().await;
            pending.remove(&RequestId::Number(id));
        }
        let _ = self
            .notify("$/cancelRequest", Some(serde_json::json!({ "id": id })))
            .await;
    }

    /// Send a notification (no response expected)
    pub async fn notify(&self, method: &str, params: Option<Value>) -> Result<(), LspError> {
        let notification = Notification::new(method, params);

        let mut stdin_guard = self.stdin.lock().await;
        let stdin = stdin_guard.as_mut().ok_or(LspError::NotConnected)?;
        write_notification(stdin, &notification).await?;

        Ok(())
    }

    /// Background task that reads and dispatches responses
    async fn read_responses(self: Arc<Self>, mut transport: Transport) {
        loop {
            if *self.shutdown.read().await {
                break;
            }

            match transport.read_message().await {
                Ok(message) => {
                    self.handle_message(message).await;
                }
                Err(e) => {
                    if !*self.shutdown.read().await {
                        tracing::error!("{} LSP read error: {}", self.language, e);
                        self.cancel_pending_requests_terminated().await;
                    }
                    break;
                }
            }
        }
    }

    /// Cancel all pending requests due to server termination
    async fn cancel_pending_requests_terminated(&self) {
        self.terminated.store(true, Ordering::Release);
        let mut pending = self.pending.write().await;
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
        let mut pending = self.pending.write().await;
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
                    let mut pending = self.pending.write().await;
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

                // Check registered handlers first
                {
                    let handlers = self.notification_handlers.read().await;
                    if let Some(handler) = handlers.get(method) {
                        handler(params.clone());
                    }
                }

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

    /// Shutdown the language server with 3-stage graceful termination
    pub async fn shutdown(&self) -> Result<(), LspError> {
        *self.shutdown.write().await = true;

        // Stage 1: Send LSP shutdown request (2s timeout)
        let shutdown_result = timeout(Duration::from_secs(2), async {
            if let Ok(()) = self.request::<()>("shutdown", None).await {
                let _ = self.notify("exit", None).await;
            }
        })
        .await;

        if shutdown_result.is_err() {
            tracing::debug!("{} LSP shutdown request timed out", self.language);
        }

        // Close stdin to signal EOF
        self.stdin.lock().await.take();

        // Stage 2 & 3: Wait for process exit, then force kill
        if let Some(mut child) = self.process.lock().await.take() {
            match timeout(Duration::from_secs(5), child.wait()).await {
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
        let disk = DiskState::probe(&crate::models::lsp::uri_to_path(uri)).await;
        let (version, outcome) = self.sync_document_inner(uri, content, disk).await?;
        if outcome == SyncOutcome::Changed {
            self.notify_did_save(uri).await;
        }
        Ok(version)
    }

    /// Sync `uri`'s overlay to the bytes symora itself just wrote, and
    /// nudge save-driven analysis. Probes the disk fingerprint BEFORE
    /// reading the content, so a racing write can only make the stored
    /// fingerprint older than the content — the next sweep then re-detects
    /// drift (always-detect, never silently-fresh). Sends `didSave` for a
    /// fresh open too: the server has never analyzed the post-edit bytes,
    /// and our own write is exactly a save.
    pub async fn sync_edited_document(&self, file: &std::path::Path) -> Result<(), LspError> {
        let disk = DiskState::probe(file).await;
        let content = tokio::fs::read_to_string(file)
            .await
            .map_err(|e| LspError::Protocol(format!("read {}: {e}", file.display())))?;
        let uri = path_to_uri(file);
        let (_, outcome) = self.sync_document_inner(&uri, &content, disk).await?;
        if outcome != SyncOutcome::Unchanged {
            self.notify_did_save(&uri).await;
            if outcome == SyncOutcome::Opened {
                // The didChange path already noted the content change; a
                // fresh open of just-edited bytes is the same event for
                // settle windows and workspace-answer caches.
                self.note_document_changed();
            }
        }
        Ok(())
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
        let (version, outcome, evicted) = {
            let mut cache = self.document_cache.write().await;
            let language_id = self.language.to_string().to_lowercase();

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
                // Opening a file the server already indexed from disk adds
                // no new information — only a content *change* re-arms the
                // cross-file settle window. Re-arming here put every warm
                // session through a pointless wait per first-open.
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

        if let Some(evicted_uri) = evicted {
            let _ = self
                .notify(
                    "textDocument/didClose",
                    Some(serde_json::json!({ "textDocument": { "uri": evicted_uri } })),
                )
                .await;
        }
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
            let _ = self
                .notify(
                    "textDocument/didClose",
                    Some(serde_json::json!({ "textDocument": { "uri": uri } })),
                )
                .await;
            self.note_document_changed();
        }
    }

    pub fn indexing_state(&self) -> IndexingState {
        IndexingState::from_u8(self.indexing_state.load(Ordering::Acquire))
    }

    pub async fn sleep_for_cross_file_settle(&self) {
        if self
            .cross_file_waited
            .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
            .is_ok()
        {
            let wait = self.config.cross_file_wait(self.language);
            if !wait.is_zero() {
                tracing::debug!(
                    "Waiting {}ms for {} cross-file indexing",
                    wait.as_millis(),
                    self.language
                );
                tokio::time::sleep(wait).await;
            }
        }
    }

    /// Apply one indexing event atomically and return the state it
    /// produced. The transition table (`IndexingState::on_event`) is the
    /// only authority; the compare-and-swap loop guarantees a concurrent
    /// signal is incorporated, never overwritten — the failure mode the
    /// old blind `store` had (a quiescence landing mid-update was stomped
    /// and its wake-up lost, latching a permanent false `TimedOut`).
    pub fn apply_indexing_event(&self, event: IndexingEvent) -> IndexingState {
        loop {
            let current = self.indexing_state();
            let next = current.on_event(event);
            if next == current {
                return current;
            }
            if self
                .indexing_state
                .compare_exchange(
                    current.to_u8(),
                    next.to_u8(),
                    Ordering::AcqRel,
                    Ordering::Acquire,
                )
                .is_ok()
            {
                if next == IndexingState::Ready {
                    self.indexing_notify.notify_waiters();
                }
                return next;
            }
        }
    }

    /// Test-only raw setter for staging a scenario mid-table.
    #[cfg(test)]
    fn force_indexing_state(&self, state: IndexingState) {
        self.indexing_state.store(state.to_u8(), Ordering::Release);
    }

    /// A document's content changed under this session (our own edit or
    /// an external one picked up by the drift sweep). Readiness itself is
    /// signal-driven and stays put — an edit doesn't unprove a server's
    /// demonstrated quiescence, and a server that genuinely re-indexes
    /// reports busy through its status channel — but the next cross-file
    /// query deserves a fresh settle window, not a latched skip, and any
    /// cached workspace-wide answer is now of a world that no longer
    /// exists.
    pub fn note_document_changed(&self) {
        self.cross_file_waited.store(false, Ordering::Release);
        bump_content_generation();
    }

    pub async fn await_indexing_signal(&self) -> IndexingState {
        // `TimedOut` is terminal-usable: re-waiting the full budget on
        // every request would make a slow server cost the timeout per
        // query forever. Recovery is signal-driven — a later quiescence
        // signal flips the state to `Ready` — and degraded answers stay
        // marked via the `indexing` marker until one arrives. Claiming the
        // wait is a no-op transition (it is not evidence the server is
        // indexing), so a `Ready`/`TimedOut` that landed since the caller
        // checked is returned, never overwritten.
        let state = self.apply_indexing_event(IndexingEvent::WaitStarted);
        if state.is_usable() {
            return state;
        }

        let max_wait = self.indexing_timeout();
        if max_wait.is_zero() {
            // No budget means we do not wait. A server that already signalled it
            // is indexing (`InProgress`) is disclosed as `TimedOut`; one that has
            // signalled nothing resolves to `Ready` — no evidence of an indexing
            // phase to disclose as a lower bound.
            return self.apply_indexing_event(IndexingEvent::WaitTimedOut);
        }

        tracing::debug!(
            "Waiting {}ms for {} workspace indexing",
            max_wait.as_millis(),
            self.language
        );

        let deadline = tokio::time::Instant::now() + max_wait;
        loop {
            let notified = self.indexing_notify.notified();
            tokio::pin!(notified);
            // Register the waiter BEFORE re-reading the state, so a
            // quiescence signal that lands in the gap either flips the
            // state we are about to read or wakes the registered waiter —
            // it can no longer fall between the two.
            notified.as_mut().enable();
            let current = self.indexing_state();
            if current.is_usable() {
                return current;
            }
            tokio::select! {
                _ = &mut notified => {
                    let state = self.indexing_state();
                    if state.is_usable() {
                        tracing::debug!("{} indexing completed via notification", self.language);
                        return state;
                    }
                    // A busy signal re-opened InProgress between the wake
                    // and the read: keep waiting on the remaining budget.
                }
                _ = tokio::time::sleep_until(deadline) => {
                    tracing::debug!("{} indexing timeout", self.language);
                    // The transition refuses to conclude anything but an
                    // in-flight wait, so a Ready that arrived between the
                    // deadline firing and this store is returned intact.
                    return self.apply_indexing_event(IndexingEvent::WaitTimedOut);
                }
            }
        }
    }

    fn indexing_timeout(&self) -> Duration {
        self.config.indexing_wait(self.language)
    }

    /// Register a notification handler for a specific method
    pub async fn on_notification<F>(&self, method: &str, handler: F)
    where
        F: Fn(serde_json::Value) + Send + Sync + 'static,
    {
        self.notification_handlers
            .write()
            .await
            .insert(method.to_string(), Box::new(handler));
    }

    pub async fn register_default_handlers(self: &Arc<Self>) {
        // The authoritative quiescence channel (rust-analyzer's
        // `experimental/serverStatus`, requested via the matching client
        // capability): quiescent=true means the workspace is fully
        // analyzed, false that the server is (re)working — including
        // after edits, so readiness re-evaluates instead of latching.
        // Once a server demonstrates this channel, the fuzzy progress and
        // log heuristics below stand down for good.
        let client_status = Arc::clone(self);
        self.on_notification("experimental/serverStatus", move |params| {
            if let Some(quiescent) = params.get("quiescent").and_then(|v| v.as_bool()) {
                client_status
                    .status_channel_seen
                    .store(true, Ordering::Release);
                client_status.apply_indexing_event(if quiescent {
                    IndexingEvent::ServerQuiescent
                } else {
                    IndexingEvent::ServerBusy
                });
            }
        })
        .await;

        let client_lang = Arc::clone(self);
        self.on_notification("language/status", move |params| {
            if params.get("type").and_then(|v| v.as_str()) == Some("ProjectStatus")
                && params.get("message").and_then(|v| v.as_str()) == Some("OK")
            {
                client_lang.apply_indexing_event(IndexingEvent::ServerQuiescent);
            }
        })
        .await;

        let client_progress = Arc::clone(self);
        // Heuristic for servers without a status protocol.
        // `WorkDoneProgressBegin` carries the `title`; `End` carries only
        // the token. Remember which tokens began an indexing-shaped task
        // and flip readiness only when the LAST of them ends — flipping
        // on the first end would present a half-built index as
        // authoritative while a sibling phase is still running. A server
        // that leaks a begin-token never reaches a heuristic Ready;
        // that fails toward the disclosed `timed_out` marker, never
        // toward a false "complete".
        let indexing_tokens: Arc<std::sync::Mutex<std::collections::HashSet<String>>> =
            Arc::new(std::sync::Mutex::new(std::collections::HashSet::new()));
        self.on_notification("$/progress", move |params| {
            if client_progress.status_channel_seen.load(Ordering::Acquire) {
                return;
            }
            let Some(token) = params.get("token").map(|t| t.to_string()) else {
                return;
            };
            let Some(value) = params.get("value") else {
                return;
            };
            match value.get("kind").and_then(|k| k.as_str()) {
                Some("begin") => {
                    if let Some(title) = value.get("title").and_then(|t| t.as_str()) {
                        let t = title.to_lowercase();
                        if t.contains("index") || t.contains("load") || t.contains("analyz") {
                            indexing_tokens
                                .lock()
                                .expect("indexing token set poisoned")
                                .insert(token);
                            // Real activity: the server told us it is indexing,
                            // so a later timeout is a genuine lower bound rather
                            // than the silence of a server with no indexing phase.
                            client_progress.apply_indexing_event(IndexingEvent::ServerBusy);
                        }
                    }
                }
                Some("end") => {
                    let mut tokens = indexing_tokens.lock().expect("indexing token set poisoned");
                    if tokens.remove(&token) && tokens.is_empty() {
                        drop(tokens);
                        client_progress.apply_indexing_event(IndexingEvent::ServerQuiescent);
                    }
                }
                _ => {}
            }
        })
        .await;

        let client_log = Arc::clone(self);
        let language = self.language;
        self.on_notification("window/logMessage", move |params| {
            if client_log.status_channel_seen.load(Ordering::Acquire) {
                return;
            }
            if let Some(msg) = params.get("message").and_then(|m| m.as_str())
                && Self::is_readiness_signal(language, msg)
            {
                client_log.apply_indexing_event(IndexingEvent::ServerQuiescent);
            }
        })
        .await;
    }

    fn is_readiness_signal(language: Language, message: &str) -> bool {
        match language {
            Language::Python => message.contains("Found") && message.contains("source file"),
            Language::TypeScript | Language::JavaScript => {
                message.contains("Loading completed") || message.contains("project load finished")
            }
            Language::Java => message.contains("initialized") || message.contains("Initialized"),
            _ => false,
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
            "client/registerCapability" => Ok(serde_json::Value::Null),
            "client/unregisterCapability" => Ok(serde_json::Value::Null),
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
        let mut stdin_guard = self.stdin.lock().await;
        if let Some(stdin) = stdin_guard.as_mut()
            && let Err(e) = write_response(stdin, &response).await
        {
            tracing::warn!("Failed to answer {} from server: {e}", request.method);
        }
    }

    /// Answer `workspace/configuration` from the initializationOptions
    /// payload. Servers like pyright read their effective settings from
    /// this pull, not from initializationOptions — answering with empty
    /// objects silently wipes every injected setting (pythonPath first
    /// among them). Unknown sections still answer `{}`.
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

    pub fn position_params(uri: &str, line: u32, column: u32) -> TextDocumentPositionParams {
        TextDocumentPositionParams {
            text_document: TextDocumentIdentifier::new(uri),
            position: Position::new(line, column),
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
mod tests {
    use super::*;

    fn test_client() -> Arc<LspClient> {
        LspClient::new(
            Language::Rust,
            PathBuf::from("/tmp"),
            Arc::new(crate::config::LspRuntimeConfig::default()),
        )
    }

    async fn notify_client(client: &Arc<LspClient>, method: &str, params: serde_json::Value) {
        client
            .handle_message(Message::Notification(Notification::new(
                method,
                Some(params),
            )))
            .await;
    }

    fn progress(token: &str, kind: &str, title: Option<&str>) -> serde_json::Value {
        let mut value = serde_json::json!({ "kind": kind });
        if let Some(title) = title {
            value["title"] = serde_json::Value::String(title.to_string());
        }
        serde_json::json!({ "token": token, "value": value })
    }

    /// `experimental/serverStatus` drives readiness in BOTH directions:
    /// quiescent=true is Ready, false re-opens InProgress — the state is
    /// re-evaluated per signal, never latched on a one-shot verdict.
    #[tokio::test]
    async fn server_status_drives_readiness_in_both_directions() {
        let client = test_client();
        client.register_default_handlers().await;

        notify_client(
            &client,
            "experimental/serverStatus",
            serde_json::json!({"quiescent": true}),
        )
        .await;
        assert_eq!(client.indexing_state(), IndexingState::Ready);

        notify_client(
            &client,
            "experimental/serverStatus",
            serde_json::json!({"quiescent": false}),
        )
        .await;
        assert_eq!(client.indexing_state(), IndexingState::InProgress);

        notify_client(
            &client,
            "experimental/serverStatus",
            serde_json::json!({"quiescent": true}),
        )
        .await;
        assert_eq!(client.indexing_state(), IndexingState::Ready);
    }

    /// A quiescence signal also clears an earlier timed-out verdict —
    /// the degradation marker must stop the moment the server is ready.
    #[tokio::test]
    async fn server_status_clears_a_timed_out_verdict() {
        let client = test_client();
        client.register_default_handlers().await;
        client.force_indexing_state(IndexingState::TimedOut);

        notify_client(
            &client,
            "experimental/serverStatus",
            serde_json::json!({"quiescent": true}),
        )
        .await;
        assert_eq!(client.indexing_state(), IndexingState::Ready);
    }

    /// A busy signal must NOT clear a timed-out verdict: the index is
    /// still incomplete, so the disclosed degraded state stands until a
    /// quiescence signal proves completion. (Clearing it would let the
    /// next answer emit unmarked off the same incomplete index.)
    #[tokio::test]
    async fn server_busy_does_not_clear_a_timed_out_verdict() {
        let client = test_client();
        client.register_default_handlers().await;
        client.force_indexing_state(IndexingState::TimedOut);

        notify_client(
            &client,
            "experimental/serverStatus",
            serde_json::json!({"quiescent": false}),
        )
        .await;
        assert_eq!(client.indexing_state(), IndexingState::TimedOut);

        notify_client(
            &client,
            "experimental/serverStatus",
            serde_json::json!({"quiescent": true}),
        )
        .await;
        assert_eq!(client.indexing_state(), IndexingState::Ready);
    }

    /// The full transition table: every pair not explicitly legal is a
    /// no-op. This is the single place the legal moves are encoded, so
    /// the race fixes (no blind stores) reduce to this table being right.
    #[test]
    fn indexing_transition_table_is_exact() {
        use IndexingEvent::*;
        use IndexingState::*;

        let states = [NotStarted, InProgress, Ready, TimedOut];

        // A quiescence signal is the only path to Ready, from anywhere.
        for state in states {
            assert_eq!(state.on_event(ServerQuiescent), Ready);
        }

        // Busy — the only opener of InProgress — re-opens from anywhere except
        // the disclosed TimedOut. This is real server activity, the evidence a
        // later timeout needs to mean a genuine lower bound.
        assert_eq!(NotStarted.on_event(ServerBusy), InProgress);
        assert_eq!(InProgress.on_event(ServerBusy), InProgress);
        assert_eq!(Ready.on_event(ServerBusy), InProgress);
        assert_eq!(TimedOut.on_event(ServerBusy), TimedOut);

        // Claiming the wait is a pure no-op: waiting is NOT evidence the server
        // is indexing, so it never moves NotStarted into InProgress (nor stomps
        // a Ready/TimedOut that landed since the caller checked).
        assert_eq!(NotStarted.on_event(WaitStarted), NotStarted);
        assert_eq!(InProgress.on_event(WaitStarted), InProgress);
        assert_eq!(Ready.on_event(WaitStarted), Ready);
        assert_eq!(TimedOut.on_event(WaitStarted), TimedOut);

        // A timeout concludes an in-flight InProgress as a degraded lower bound;
        // from NotStarted (the server never signalled activity) it resolves to
        // Ready — no indexing phase to wait for — never a false TimedOut. It
        // never overwrites a Ready/TimedOut that already landed.
        assert_eq!(InProgress.on_event(WaitTimedOut), TimedOut);
        assert_eq!(NotStarted.on_event(WaitTimedOut), Ready);
        assert_eq!(Ready.on_event(WaitTimedOut), Ready);
        assert_eq!(TimedOut.on_event(WaitTimedOut), TimedOut);
    }

    /// An already-quiescent server must never be re-latched into a false
    /// TimedOut by a waiter racing the signal: the wait claim is a
    /// transition, so the landed Ready is returned, not overwritten.
    #[tokio::test]
    async fn wait_returns_a_ready_that_landed_before_the_claim() {
        let client = test_client();
        client.apply_indexing_event(IndexingEvent::ServerQuiescent);
        assert_eq!(client.await_indexing_signal().await, IndexingState::Ready);
        // And again — quiescence is durable across repeated waits.
        assert_eq!(client.await_indexing_signal().await, IndexingState::Ready);
    }

    /// The progress heuristic flips Ready only when the LAST tracked
    /// indexing token ends — one phase finishing while a sibling runs is
    /// not readiness.
    #[tokio::test]
    async fn progress_heuristic_waits_for_all_indexing_tokens_to_drain() {
        let client = test_client();
        client.register_default_handlers().await;

        notify_client(
            &client,
            "$/progress",
            progress("t1", "begin", Some("Indexing")),
        )
        .await;
        notify_client(
            &client,
            "$/progress",
            progress("t2", "begin", Some("Loading workspace")),
        )
        .await;
        notify_client(&client, "$/progress", progress("t1", "end", None)).await;
        assert_eq!(
            client.indexing_state(),
            IndexingState::InProgress,
            "the server is indexing (a sibling phase is still in flight), not yet ready"
        );

        notify_client(&client, "$/progress", progress("t2", "end", None)).await;
        assert_eq!(client.indexing_state(), IndexingState::Ready);
    }

    /// An indexing-progress `begin` is real activity: it opens `InProgress`, so
    /// a later timeout reads as a genuine lower bound rather than the silence of
    /// a server with no indexing phase. (Before, only a synthetic wait-claim
    /// opened InProgress, which is why a silent server timed out forever.)
    #[tokio::test]
    async fn indexing_progress_begin_opens_in_progress() {
        let client = test_client();
        client.register_default_handlers().await;
        assert_eq!(client.indexing_state(), IndexingState::NotStarted);

        notify_client(
            &client,
            "$/progress",
            progress("t1", "begin", Some("Indexing")),
        )
        .await;
        assert_eq!(client.indexing_state(), IndexingState::InProgress);
    }

    /// Once the server demonstrates an explicit status channel, progress
    /// titles stop being readiness signals — a fuzzy heuristic must not
    /// overrule a server that reported itself busy.
    #[tokio::test]
    async fn progress_heuristic_stands_down_once_the_status_channel_speaks() {
        let client = test_client();
        client.register_default_handlers().await;

        notify_client(
            &client,
            "experimental/serverStatus",
            serde_json::json!({"quiescent": false}),
        )
        .await;
        notify_client(
            &client,
            "$/progress",
            progress("t1", "begin", Some("Indexing")),
        )
        .await;
        notify_client(&client, "$/progress", progress("t1", "end", None)).await;
        assert_eq!(
            client.indexing_state(),
            IndexingState::InProgress,
            "the status channel owns readiness once it has spoken"
        );
    }

    /// An edit does not unprove a server's demonstrated quiescence: the
    /// state stays Ready (servers that genuinely re-index report busy via
    /// their status channel), so the post-edit checked-delete workflow
    /// never starves on a signal that will never re-fire.
    #[tokio::test]
    async fn document_change_does_not_unprove_readiness() {
        let client = test_client();
        client.force_indexing_state(IndexingState::Ready);
        client.note_document_changed();
        assert_eq!(client.indexing_state(), IndexingState::Ready);

        client.force_indexing_state(IndexingState::TimedOut);
        client.note_document_changed();
        assert_eq!(
            client.indexing_state(),
            IndexingState::TimedOut,
            "an edit doesn't complete an incomplete index either"
        );
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

    #[test]
    fn test_request_id_generation() {
        let counter = AtomicU64::new(1);
        assert_eq!(counter.fetch_add(1, Ordering::Relaxed), 1);
        assert_eq!(counter.fetch_add(1, Ordering::Relaxed), 2);
        assert_eq!(counter.fetch_add(1, Ordering::Relaxed), 3);
    }

    #[test]
    fn test_position_params() {
        let params = LspClient::position_params("file:///test.rs", 10, 5);
        assert_eq!(params.text_document.uri, "file:///test.rs");
        assert_eq!(params.position.line, 10);
        assert_eq!(params.position.character, 5);
    }
}
