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
    LspDiagnostic, Message, Notification, Position, RegularExpressionsCapability, Request,
    RequestId, Response, ResponseError, StaleRequestSupport, TextDocumentClientCapabilities,
    TextDocumentIdentifier, TextDocumentPositionParams, WindowClientCapabilities,
    WorkspaceClientCapabilities, error_codes,
};
use super::transport::{Transport, write_notification, write_request, write_response};
use crate::error::LspError;
use crate::models::lsp::path_to_uri;
use crate::models::symbol::Language;

type PendingRequest = oneshot::Sender<Response>;
type NotificationHandler = Box<dyn Fn(serde_json::Value) + Send + Sync>;

const MAX_OPEN_DOCUMENTS: usize = 100;
const MAX_DIAGNOSTICS_CACHE: usize = 200;

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

#[derive(Debug)]
struct DocumentState {
    version: u32,
    content_hash: u64,
}

impl DocumentState {
    fn new(content: &str) -> Self {
        Self {
            version: 1,
            content_hash: crate::infra::hash_content(content),
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
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum IndexingState {
    NotStarted = 0,
    InProgress = 1,
    Ready = 2,
    TimedOut = 3,
    Stale = 4,
}

impl IndexingState {
    fn from_u8(value: u8) -> Self {
        match value {
            1 => Self::InProgress,
            2 => Self::Ready,
            3 => Self::TimedOut,
            4 => Self::Stale,
            _ => Self::NotStarted,
        }
    }

    fn to_u8(self) -> u8 {
        self as u8
    }

    pub fn is_usable(self) -> bool {
        matches!(self, Self::Ready | Self::TimedOut)
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
    shutdown: RwLock<bool>,
    indexing_state: AtomicU8,
    indexing_notify: Notify,
    terminated: AtomicBool,
    cross_file_waited: AtomicBool,
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
            shutdown: RwLock::new(false),
            indexing_state: AtomicU8::new(IndexingState::NotStarted.to_u8()),
            indexing_notify: Notify::new(),
            terminated: AtomicBool::new(false),
            cross_file_waited: AtomicBool::new(false),
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

        let result: InitializeResult = self
            .request("initialize", Some(serde_json::to_value(params)?))
            .await?;

        // Store capabilities
        *self.capabilities.write().await = Some(result);

        // Send initialized notification
        self.notify("initialized", Some(serde_json::json!({})))
            .await?;

        Ok(())
    }

    /// Build client capabilities optimized for the target language server (LSP 3.17 complete)
    fn client_capabilities(language: Language) -> ClientCapabilities {
        let general = GeneralClientCapabilities {
            position_encodings: Some(vec!["utf-16".to_string(), "utf-8".to_string()]),
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
            });
        }

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
    pub async fn sync_document(&self, uri: &str, content: &str) -> Result<u32, LspError> {
        self.sync_document_inner(uri, content).await
    }

    async fn sync_document_inner(&self, uri: &str, content: &str) -> Result<u32, LspError> {
        let (version, evicted) = {
            let mut cache = self.document_cache.write().await;
            let language_id = self.language.to_string().to_lowercase();

            if let Some(state) = cache.get_mut(uri) {
                if state.needs_update(content) {
                    state.update(content);
                    self.invalidate_index();
                    self.notify(
                        "textDocument/didChange",
                        Some(serde_json::json!({
                            "textDocument": { "uri": uri, "version": state.version },
                            "contentChanges": [{ "text": content }]
                        })),
                    )
                    .await?;
                }
                (state.version, None)
            } else {
                // Opening a file the server already indexed from disk adds
                // no new information — only a content *change* invalidates
                // the workspace index. Invalidating here put every warm
                // session through a pointless re-wait per first-open.
                let state = DocumentState::new(content);
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
                (version, cache.insert(uri.to_string(), state))
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
        Ok(version)
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

    pub fn set_indexing_state(&self, state: IndexingState) {
        self.indexing_state.store(state.to_u8(), Ordering::Release);
        if state == IndexingState::Ready {
            self.indexing_notify.notify_waiters();
        }
    }

    pub fn invalidate_index(&self) {
        let current = self.indexing_state();
        if matches!(current, IndexingState::Ready | IndexingState::TimedOut) {
            self.indexing_state
                .store(IndexingState::Stale.to_u8(), Ordering::Release);
            // The next cross-file query after an edit deserves a fresh
            // settle window, not a latched skip.
            self.cross_file_waited.store(false, Ordering::Release);
        }
    }

    pub async fn await_indexing_signal(&self) -> IndexingState {
        let current = self.indexing_state();
        // `TimedOut` is terminal-usable: re-waiting the full budget on
        // every request would make a slow server cost the timeout per
        // query forever. Recovery runs through `invalidate_index` (file
        // edits -> `Stale`) or a session restart, and degraded answers
        // stay marked via `indexing_degradation`.
        if current.is_usable() {
            return current;
        }

        self.set_indexing_state(IndexingState::InProgress);

        let max_wait = self.indexing_timeout();
        if max_wait.is_zero() {
            self.set_indexing_state(IndexingState::Ready);
            return IndexingState::Ready;
        }

        tracing::debug!(
            "Waiting {}ms for {} workspace indexing",
            max_wait.as_millis(),
            self.language
        );

        tokio::select! {
            _ = self.indexing_notify.notified() => {
                tracing::debug!("{} indexing completed via notification", self.language);
                self.set_indexing_state(IndexingState::Ready);
                IndexingState::Ready
            }
            _ = tokio::time::sleep(max_wait) => {
                tracing::debug!("{} indexing timeout", self.language);
                self.set_indexing_state(IndexingState::TimedOut);
                IndexingState::TimedOut
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
        let client_status = Arc::clone(self);
        self.on_notification("experimental/serverStatus", move |params| {
            if let Some(quiescent) = params.get("quiescent").and_then(|v| v.as_bool())
                && quiescent
            {
                client_status.set_indexing_state(IndexingState::Ready);
            }
        })
        .await;

        let client_lang = Arc::clone(self);
        self.on_notification("language/status", move |params| {
            if params.get("type").and_then(|v| v.as_str()) == Some("ProjectStatus")
                && params.get("message").and_then(|v| v.as_str()) == Some("OK")
            {
                client_lang.set_indexing_state(IndexingState::Ready);
            }
        })
        .await;

        let client_progress = Arc::clone(self);
        // `WorkDoneProgressBegin` carries the `title`; `End` carries only
        // the token. Remember which tokens began an indexing-shaped task
        // so the matching `end` can flip readiness — reading `title` off
        // `end` never fires on spec-compliant servers.
        let indexing_tokens: Arc<std::sync::Mutex<std::collections::HashSet<String>>> =
            Arc::new(std::sync::Mutex::new(std::collections::HashSet::new()));
        self.on_notification("$/progress", move |params| {
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
                        }
                    }
                }
                Some("end")
                    if indexing_tokens
                        .lock()
                        .expect("indexing token set poisoned")
                        .remove(&token) =>
                {
                    // Readiness makes any other pending begin-tokens moot;
                    // clearing bounds the set against servers that never
                    // end a token.
                    indexing_tokens
                        .lock()
                        .expect("indexing token set poisoned")
                        .clear();
                    client_progress.set_indexing_state(IndexingState::Ready);
                }
                _ => {}
            }
        })
        .await;

        let client_log = Arc::clone(self);
        let language = self.language;
        self.on_notification("window/logMessage", move |params| {
            if let Some(msg) = params.get("message").and_then(|m| m.as_str())
                && Self::is_readiness_signal(language, msg)
            {
                client_log.set_indexing_state(IndexingState::Ready);
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

    pub async fn capabilities(&self) -> Option<InitializeResult> {
        self.capabilities.read().await.clone()
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
