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

use super::init_options::get_initialization_options;
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

#[derive(Debug)]
struct DocumentState {
    version: u32,
    content_hash: u64,
    ref_count: u32,
}

impl DocumentState {
    fn new(content: &str) -> Self {
        Self {
            version: 1,
            content_hash: crate::infra::hash_content(content),
            ref_count: 1,
        }
    }

    fn needs_update(&self, new_content: &str) -> bool {
        crate::infra::hash_content(new_content) != self.content_hash
    }

    fn update(&mut self, new_content: &str) {
        self.version += 1;
        self.content_hash = crate::infra::hash_content(new_content);
    }

    fn acquire(&mut self) {
        self.ref_count += 1;
    }

    fn release(&mut self) -> bool {
        self.ref_count = self.ref_count.saturating_sub(1);
        self.ref_count == 0
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
        let len = self.lru_order.len();
        for _ in 0..len {
            if let Some(uri) = self.lru_order.pop_back() {
                if self
                    .docs
                    .get(&uri)
                    .map(|s| s.ref_count == 0)
                    .unwrap_or(false)
                {
                    self.docs.remove(&uri);
                    return Some(uri);
                }
                self.lru_order.push_front(uri);
            }
        }
        None
    }

    fn remove(&mut self, uri: &str) -> Option<DocumentState> {
        self.lru_order.retain(|u| u != uri);
        self.docs.remove(uri)
    }
}

pub struct DocumentSyncGuard {
    uri: String,
    client: Arc<LspClient>,
}

impl Drop for DocumentSyncGuard {
    fn drop(&mut self) {
        let uri = self.uri.clone();
        let client = Arc::clone(&self.client);
        tokio::spawn(async move {
            client.release_document(&uri).await;
        });
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HealthStatus {
    Healthy,
    Initializing,
    ShuttingDown,
    NotRunning,
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
    diagnostics: RwLock<HashMap<String, Vec<LspDiagnostic>>>,
    document_cache: RwLock<DocumentCache>,
    notification_handlers: RwLock<HashMap<String, NotificationHandler>>,
    root: PathBuf,
    capabilities: RwLock<Option<InitializeResult>>,
    shutdown: RwLock<bool>,
    indexing_state: AtomicU8,
    indexing_notify: Notify,
    terminated: AtomicBool,
    cross_file_waited: AtomicBool,
}

impl LspClient {
    pub fn new(language: Language, root: PathBuf) -> Arc<Self> {
        Arc::new(Self {
            language,
            process: Mutex::new(None),
            stdin: Mutex::new(None),
            next_id: AtomicU64::new(1),
            pending: RwLock::new(HashMap::new()),
            diagnostics: RwLock::new(HashMap::new()),
            document_cache: RwLock::new(DocumentCache::new()),
            notification_handlers: RwLock::new(HashMap::new()),
            root,
            capabilities: RwLock::new(None),
            shutdown: RwLock::new(false),
            indexing_state: AtomicU8::new(IndexingState::NotStarted.to_u8()),
            indexing_notify: Notify::new(),
            terminated: AtomicBool::new(false),
            cross_file_waited: AtomicBool::new(false),
        })
    }

    /// Start the language server
    pub async fn start(self: &Arc<Self>, command: &str, args: &[&str]) -> Result<(), LspError> {
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

        // Store process and stdin
        *self.process.lock().await = Some(child);
        *self.stdin.lock().await = Some(stdin);

        // Start response reader task
        let client = Arc::clone(self);
        tokio::spawn(async move {
            client.read_responses(Transport::new(stdout)).await;
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

    /// Get health status with details
    pub async fn health_status(&self) -> HealthStatus {
        if !self.is_running().await {
            return HealthStatus::NotRunning;
        }

        if *self.shutdown.read().await {
            return HealthStatus::ShuttingDown;
        }

        if self.capabilities.read().await.is_some() {
            HealthStatus::Healthy
        } else {
            HealthStatus::Initializing
        }
    }

    /// Initialize the language server
    async fn initialize(&self) -> Result<(), LspError> {
        let init_options = get_initialization_options(self.language, &self.root);

        let params = InitializeParams {
            process_id: Some(std::process::id()),
            root_uri: Some(path_to_uri(&self.root)),
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

        let result = timeout(crate::config::timeout_for(self.language, method), rx).await;

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

    pub async fn request_with_retry<T: serde::de::DeserializeOwned>(
        &self,
        method: &str,
        params: Option<Value>,
    ) -> Result<T, LspError> {
        use crate::infra::retry::{RetryConfig, with_retry};
        let config = RetryConfig::for_language(self.language);
        let params_clone = params.clone();
        with_retry(&config, || self.request(method, params_clone.clone())).await
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
                        let diags = params.get("diagnostics").cloned();
                        if let (Some(uri), Some(diags)) = (uri, diags)
                            && let Ok(diagnostics) =
                                serde_json::from_value::<Vec<LspDiagnostic>>(diags)
                        {
                            let mut cache = self.diagnostics.write().await;
                            let count = diagnostics.len();

                            // Evict oldest entry if at capacity (simple FIFO)
                            if cache.len() >= MAX_DIAGNOSTICS_CACHE
                                && !cache.contains_key(uri)
                                && let Some(oldest_key) = cache.keys().next().cloned()
                            {
                                cache.remove(&oldest_key);
                                tracing::trace!("Evicted diagnostics for {}", oldest_key);
                            }

                            cache.insert(uri.to_string(), diagnostics);
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
                    let _ = child.kill().await;
                }
            }
        }

        self.cancel_pending_requests("Server shutdown").await;
        tracing::info!("{} language server stopped", self.language);
        Ok(())
    }

    pub async fn sync_document(&self, uri: &str, content: &str) -> Result<(), LspError> {
        let evicted = {
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
                None
            } else {
                let state = DocumentState::new(content);
                self.invalidate_index();
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
                cache.insert(uri.to_string(), state)
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
        Ok(())
    }

    pub async fn acquire_document(
        self: &Arc<Self>,
        uri: &str,
        content: &str,
    ) -> Result<DocumentSyncGuard, LspError> {
        let evicted = {
            let mut cache = self.document_cache.write().await;
            let language_id = self.language.to_string().to_lowercase();

            if let Some(state) = cache.get_mut(uri) {
                state.acquire();
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
                None
            } else {
                let state = DocumentState::new(content);
                self.invalidate_index();
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
                cache.insert(uri.to_string(), state)
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

        Ok(DocumentSyncGuard {
            uri: uri.to_string(),
            client: Arc::clone(self),
        })
    }

    async fn release_document(&self, uri: &str) {
        let should_close = {
            let mut cache = self.document_cache.write().await;
            if let Some(state) = cache.docs.get_mut(uri) {
                if state.release() {
                    cache.remove(uri);
                    true
                } else {
                    false
                }
            } else {
                false
            }
        };

        if should_close {
            let _ = self
                .notify(
                    "textDocument/didClose",
                    Some(serde_json::json!({ "textDocument": { "uri": uri } })),
                )
                .await;
        }
    }

    pub fn indexing_state(&self) -> IndexingState {
        IndexingState::from_u8(self.indexing_state.load(Ordering::Acquire))
    }

    pub async fn ensure_cross_file_ready(&self) {
        if self
            .cross_file_waited
            .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
            .is_ok()
        {
            let wait = crate::config::cross_file_wait(self.language);
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
        }
    }

    pub async fn wait_for_indexing(&self) -> IndexingState {
        let current = self.indexing_state();
        if current == IndexingState::Ready {
            return IndexingState::Ready;
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
        crate::config::indexing_wait(self.language)
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
        self.on_notification("$/progress", move |params| {
            if let Some(value) = params.get("value")
                && value.get("kind").and_then(|k| k.as_str()) == Some("end")
                && let Some(title) = value.get("title").and_then(|t| t.as_str())
            {
                let t = title.to_lowercase();
                if t.contains("index") || t.contains("load") || t.contains("analyz") {
                    client_progress.set_indexing_state(IndexingState::Ready);
                }
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

    pub async fn close_document(&self, uri: &str) -> Result<(), LspError> {
        self.document_cache.write().await.remove(uri);
        self.notify(
            "textDocument/didClose",
            Some(serde_json::json!({ "textDocument": { "uri": uri } })),
        )
        .await
    }

    async fn handle_server_request(&self, request: Request) {
        let response_result = match request.method.as_str() {
            "workspace/configuration" => self.handle_workspace_configuration(&request.params),
            "client/registerCapability" => Ok(serde_json::Value::Null),
            "client/unregisterCapability" => Ok(serde_json::Value::Null),
            "window/workDoneProgress/create" => Ok(serde_json::Value::Null),
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

        if let Ok(mut stdin_guard) = self.stdin.try_lock()
            && let Some(stdin) = stdin_guard.as_mut()
        {
            let _ = write_response(stdin, &response).await;
        }
    }

    fn handle_workspace_configuration(
        &self,
        params: &Option<Value>,
    ) -> Result<Value, ResponseError> {
        let items = params
            .as_ref()
            .and_then(|p| p.get("items"))
            .and_then(|i| i.as_array())
            .map(|arr| arr.len())
            .unwrap_or(0);

        Ok(Value::Array(vec![
            Value::Object(serde_json::Map::new());
            items
        ]))
    }

    pub fn position_params(uri: &str, line: u32, column: u32) -> TextDocumentPositionParams {
        TextDocumentPositionParams {
            text_document: TextDocumentIdentifier::new(uri),
            position: Position::new(line, column),
        }
    }

    pub fn language(&self) -> Language {
        self.language
    }

    pub async fn capabilities(&self) -> Option<InitializeResult> {
        self.capabilities.read().await.clone()
    }

    pub async fn get_diagnostics(&self, uri: &str) -> Vec<LspDiagnostic> {
        self.diagnostics
            .read()
            .await
            .get(uri)
            .cloned()
            .unwrap_or_default()
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
