//! Daemon Server Implementation

use std::collections::HashMap;
#[cfg(unix)]
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use serde::de::DeserializeOwned;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::{UnixListener, UnixStream};
use tokio::sync::{RwLock, Semaphore, broadcast};

use crate::config;
use crate::daemon::dto::{LocationDto, SymbolDto};
use crate::daemon::handlers::*;
use crate::daemon::protocol::{Request, RequestId, Response, RpcError, methods};
use crate::models::config::SymoraConfig;
use crate::models::lsp::FindSymbolsOptions;
use crate::services::lsp::{DefaultLspService, LspService};

type ProjectsMap = Arc<RwLock<HashMap<PathBuf, Arc<ProjectContext>>>>;

// ============================================================================
// Configuration
// ============================================================================

#[derive(Debug, Clone)]
pub struct DaemonConfig {
    pub socket_path: PathBuf,
    pub pid_path: PathBuf,
    pub lock_path: PathBuf,
    pub idle_timeout: Duration,
    pub max_concurrent: usize,
}

impl Default for DaemonConfig {
    fn default() -> Self {
        let base = dirs::home_dir()
            .unwrap_or_else(|| PathBuf::from("/tmp"))
            .join(".symora");

        let settings = Self::load_settings();

        Self {
            socket_path: base.join("daemon.sock"),
            pid_path: base.join("daemon.pid"),
            lock_path: base.join("daemon.lock"),
            idle_timeout: Duration::from_secs(settings.idle_timeout_mins * 60),
            max_concurrent: settings.max_concurrent,
        }
    }
}

impl DaemonConfig {
    fn load_settings() -> crate::models::config::DaemonSettings {
        Self::load_config().map(|c| c.daemon).unwrap_or_default()
    }

    fn load_config() -> Option<SymoraConfig> {
        std::env::var("XDG_CONFIG_HOME")
            .map(PathBuf::from)
            .ok()
            .or_else(|| dirs::home_dir().map(|h| h.join(".config")))
            .map(|d| d.join("symora/config.toml"))
            .filter(|p| p.exists())
            .and_then(|p| std::fs::read_to_string(&p).ok())
            .and_then(|c| toml::from_str::<SymoraConfig>(&c).ok())
    }

    pub fn init_lsp_settings() {
        if let Some(symora_config) = Self::load_config() {
            config::init(&symora_config);
        }
    }
}

// ============================================================================
// Project Context
// ============================================================================

struct ProjectContext {
    lsp: Arc<dyn LspService + Send + Sync>,
    last_used: RwLock<Instant>,
    request_count: AtomicU64,
}

impl ProjectContext {
    fn new(path: &Path) -> Self {
        Self {
            lsp: Arc::new(DefaultLspService::new(path)),
            last_used: RwLock::new(Instant::now()),
            request_count: AtomicU64::new(0),
        }
    }

    async fn touch(&self) {
        self.request_count.fetch_add(1, Ordering::Relaxed);
        *self.last_used.write().await = Instant::now();
    }
}

// ============================================================================
// Daemon Server
// ============================================================================

pub struct DaemonServer {
    config: DaemonConfig,
    projects: ProjectsMap,
    semaphore: Arc<Semaphore>,
    start_time: Instant,
    shutdown_tx: broadcast::Sender<()>,
}

impl DaemonServer {
    const REQUEST_TIMEOUT: Duration = Duration::from_secs(120);

    pub fn new(config: DaemonConfig) -> Self {
        let (shutdown_tx, _) = broadcast::channel(1);
        Self {
            semaphore: Arc::new(Semaphore::new(config.max_concurrent)),
            config,
            projects: Arc::new(RwLock::new(HashMap::new())),
            start_time: Instant::now(),
            shutdown_tx,
        }
    }

    pub async fn run(&self) -> Result<(), std::io::Error> {
        self.setup_socket_dir().await?;

        let _ = tokio::fs::remove_file(&self.config.socket_path).await;
        let listener = UnixListener::bind(&self.config.socket_path)?;

        #[cfg(unix)]
        {
            let perms = std::fs::Permissions::from_mode(0o600);
            tokio::fs::set_permissions(&self.config.socket_path, perms).await?;
        }

        tracing::info!("Daemon listening on {:?}", self.config.socket_path);
        self.write_pid_file().await?;

        let mut shutdown_rx = self.shutdown_tx.subscribe();
        let mut cleanup_interval = tokio::time::interval(Duration::from_secs(60));
        cleanup_interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);

        loop {
            tokio::select! {
                result = listener.accept() => {
                    if let Ok((stream, _)) = result {
                        self.spawn_connection_handler(stream);
                    }
                }
                _ = cleanup_interval.tick() => {
                    self.cleanup_idle_servers().await;
                }
                _ = shutdown_rx.recv() => {
                    tracing::info!("Shutdown signal received");
                    break;
                }
            }
        }

        self.cleanup().await;
        Ok(())
    }

    async fn setup_socket_dir(&self) -> Result<(), std::io::Error> {
        if let Some(parent) = self.config.socket_path.parent() {
            tokio::fs::create_dir_all(parent).await?;
            #[cfg(unix)]
            {
                let perms = std::fs::Permissions::from_mode(0o700);
                tokio::fs::set_permissions(parent, perms).await?;
            }
        }
        Ok(())
    }

    fn spawn_connection_handler(&self, stream: UnixStream) {
        let projects = Arc::clone(&self.projects);
        let semaphore = Arc::clone(&self.semaphore);
        let config = self.config.clone();
        let start_time = self.start_time;
        let shutdown_tx = self.shutdown_tx.clone();

        tokio::spawn(async move {
            if let Err(e) =
                handle_connection(stream, projects, semaphore, config, start_time, shutdown_tx)
                    .await
            {
                tracing::warn!("Connection error: {}", e);
            }
        });
    }

    async fn write_pid_file(&self) -> Result<(), std::io::Error> {
        tokio::fs::write(&self.config.pid_path, std::process::id().to_string()).await?;
        #[cfg(unix)]
        {
            let perms = std::fs::Permissions::from_mode(0o600);
            tokio::fs::set_permissions(&self.config.pid_path, perms).await?;
        }
        Ok(())
    }

    async fn cleanup_idle_servers(&self) {
        let idle: Vec<_> = {
            let projects = self.projects.read().await;
            projects
                .iter()
                .filter(|(_, ctx)| {
                    ctx.last_used
                        .try_read()
                        .map(|t| t.elapsed() > self.config.idle_timeout)
                        .unwrap_or(false)
                })
                .map(|(p, c)| (p.clone(), Arc::clone(c)))
                .collect()
        };

        if idle.is_empty() {
            return;
        }

        {
            let mut projects = self.projects.write().await;
            for (path, _) in &idle {
                projects.remove(path);
            }
        }

        for (path, ctx) in idle {
            ctx.lsp.shutdown().await;
            tracing::info!("Removed idle project: {:?}", path);
        }
    }

    async fn cleanup(&self) {
        let projects = self.projects.read().await;
        for (_, ctx) in projects.iter() {
            ctx.lsp.shutdown().await;
        }
        let _ = tokio::fs::remove_file(&self.config.socket_path).await;
        let _ = tokio::fs::remove_file(&self.config.pid_path).await;
    }

    pub fn shutdown(&self) {
        let _ = self.shutdown_tx.send(());
    }
}

// ============================================================================
// Connection Handling
// ============================================================================

async fn handle_connection(
    stream: UnixStream,
    projects: ProjectsMap,
    semaphore: Arc<Semaphore>,
    config: DaemonConfig,
    start_time: Instant,
    shutdown_tx: broadcast::Sender<()>,
) -> Result<(), std::io::Error> {
    let (reader, mut writer) = stream.into_split();
    let mut reader = BufReader::new(reader);
    let mut line = String::new();

    while reader.read_line(&mut line).await? > 0 {
        let _permit = match semaphore.acquire().await {
            Ok(permit) => permit,
            Err(_) => break,
        };

        let result = tokio::time::timeout(
            DaemonServer::REQUEST_TIMEOUT,
            process_request(&line, &projects, &config, start_time),
        )
        .await;

        let (response, should_shutdown) = match result {
            Ok(r) => r,
            Err(_) => {
                let id = serde_json::from_str::<Request>(&line)
                    .ok()
                    .map(|r| r.id)
                    .unwrap_or(RequestId::Number(0));
                (
                    Response::error(id, RpcError::internal_error("Request timed out")),
                    false,
                )
            }
        };

        let json = serde_json::to_string(&response).unwrap_or_else(|_| {
            r#"{"jsonrpc":"2.0","id":null,"error":{"code":-32603,"message":"Serialization error"}}"#.to_string()
        });

        writer.write_all(json.as_bytes()).await?;
        writer.write_all(b"\n").await?;
        writer.flush().await?;

        if should_shutdown {
            let _ = shutdown_tx.send(());
        }

        line.clear();
    }

    Ok(())
}

async fn process_request(
    json: &str,
    projects: &ProjectsMap,
    config: &DaemonConfig,
    start_time: Instant,
) -> (Response, bool) {
    let request: Request = match serde_json::from_str(json) {
        Ok(r) => r,
        Err(_) => {
            return (
                Response::error(RequestId::Number(0), RpcError::parse_error()),
                false,
            );
        }
    };

    let id = request.id.clone();
    let is_shutdown = request.method == methods::SHUTDOWN;

    let result = dispatch(&request, projects, config, start_time).await;
    let response = match result {
        Ok(v) => Response::success(id, v),
        Err(e) => Response::error(id, e),
    };

    (response, is_shutdown)
}

// ============================================================================
// Request Dispatch
// ============================================================================

async fn dispatch(
    request: &Request,
    projects: &ProjectsMap,
    config: &DaemonConfig,
    start_time: Instant,
) -> Result<serde_json::Value, RpcError> {
    let params = request.params.clone().unwrap_or(serde_json::json!({}));

    match request.method.as_str() {
        // System
        methods::PING => Ok(serde_json::json!({"pong": true})),
        methods::STATUS => handle_status(projects, config, start_time).await,
        methods::SHUTDOWN => Ok(serde_json::json!({"shutting_down": true})),

        // Symbol operations
        methods::FIND_SYMBOL => handle_find_symbols(&params, projects).await,
        methods::WORKSPACE_SYMBOL => handle_workspace_symbol(&params, projects).await,

        // Position-based operations
        methods::FIND_REFS => handle_position(&params, projects, |ctx, f, l, c| async move {
            let refs = ctx.lsp.find_references(&f, l, c).await?;
            Ok(serde_json::json!({
                "count": refs.len(),
                "references": refs.iter().map(LocationDto::from).collect::<Vec<_>>()
            }))
        }).await,

        methods::FIND_DEF => handle_position(&params, projects, |ctx, f, l, c| async move {
            let def = ctx.lsp.goto_definition(&f, l, c).await?;
            Ok(match def {
                Some(loc) => serde_json::json!({ "definition": LocationDto::from(&loc) }),
                None => serde_json::json!({ "definition": null, "message": "No definition found" }),
            })
        }).await,

        methods::FIND_TYPEDEF => handle_position(&params, projects, |ctx, f, l, c| async move {
            let def = ctx.lsp.goto_type_definition(&f, l, c).await?;
            Ok(match def {
                Some(loc) => serde_json::json!({ "definition": LocationDto::from(&loc) }),
                None => serde_json::json!({ "definition": null, "message": "No type definition found" }),
            })
        }).await,

        methods::FIND_IMPL => handle_position(&params, projects, |ctx, f, l, c| async move {
            let impls = ctx.lsp.find_implementations(&f, l, c).await?;
            Ok(serde_json::json!({
                "count": impls.len(),
                "implementations": impls.iter().map(LocationDto::from).collect::<Vec<_>>()
            }))
        }).await,

        methods::HOVER => handle_position(&params, projects, |ctx, f, l, c| async move {
            let hover = ctx.lsp.hover(&f, l, c).await?;
            Ok(match hover {
                Some(h) => serde_json::json!({ "content": h.content }),
                None => serde_json::json!({ "content": null, "message": "No hover information" }),
            })
        }).await,

        methods::SIGNATURE_HELP => handle_position(&params, projects, |ctx, f, l, c| async move {
            let help = ctx.lsp.signature_help(&f, l, c).await?;
            Ok(match help {
                Some(h) => serde_json::json!({
                    "signatures": h.signatures.iter().map(|s| serde_json::json!({
                        "label": s.label,
                        "documentation": s.documentation,
                        "parameters": s.parameters.iter().map(|p| serde_json::json!({
                            "label": p.label,
                            "documentation": p.documentation,
                        })).collect::<Vec<_>>(),
                        "active_parameter": s.active_parameter,
                    })).collect::<Vec<_>>(),
                    "active_signature": h.active_signature,
                    "active_parameter": h.active_parameter,
                }),
                None => serde_json::json!({ "signatures": [], "message": "No signature help available" }),
            })
        }).await,

        methods::CALLS_INCOMING => handle_position(&params, projects, |ctx, f, l, c| async move {
            let calls = ctx.lsp.incoming_calls(&f, l, c).await?;
            Ok(serde_json::json!({
                "count": calls.len(),
                "calls": calls.iter().map(|c| serde_json::json!({
                    "name": c.name,
                    "kind": c.kind.to_string(),
                    "file": c.location.file.display().to_string(),
                    "line": c.location.line,
                    "column": c.location.column,
                    "call_site": c.call_site.as_ref().map(|cs| serde_json::json!({
                        "file": cs.file.display().to_string(),
                        "line": cs.line,
                        "column": cs.column,
                    })),
                })).collect::<Vec<_>>()
            }))
        }).await,

        methods::CALLS_OUTGOING => handle_position(&params, projects, |ctx, f, l, c| async move {
            let calls = ctx.lsp.outgoing_calls(&f, l, c).await?;
            Ok(serde_json::json!({
                "count": calls.len(),
                "calls": calls.iter().map(|c| serde_json::json!({
                    "name": c.name,
                    "kind": c.kind.to_string(),
                    "file": c.location.file.display().to_string(),
                    "line": c.location.line,
                    "column": c.location.column,
                    "call_site": c.call_site.as_ref().map(|cs| serde_json::json!({
                        "file": cs.file.display().to_string(),
                        "line": cs.line,
                        "column": cs.column,
                    })),
                })).collect::<Vec<_>>()
            }))
        }).await,

        methods::SUPERTYPES => handle_position(&params, projects, |ctx, f, l, c| async move {
            let items = ctx.lsp.supertypes(&f, l, c).await?;
            Ok(serde_json::json!({
                "count": items.len(),
                "items": items.iter().map(|item| TypeHierarchyItemJson {
                    name: item.name.clone(),
                    kind: item.kind.to_string(),
                    file: item.location.file.display().to_string(),
                    line: item.location.line,
                    column: item.location.column,
                    detail: item.detail.clone(),
                }).collect::<Vec<_>>()
            }))
        }).await,

        methods::SUBTYPES => handle_position(&params, projects, |ctx, f, l, c| async move {
            let items = ctx.lsp.subtypes(&f, l, c).await?;
            Ok(serde_json::json!({
                "count": items.len(),
                "items": items.iter().map(|item| TypeHierarchyItemJson {
                    name: item.name.clone(),
                    kind: item.kind.to_string(),
                    file: item.location.file.display().to_string(),
                    line: item.location.line,
                    column: item.location.column,
                    detail: item.detail.clone(),
                }).collect::<Vec<_>>()
            }))
        }).await,

        methods::PREPARE_RENAME => handle_position(&params, projects, |ctx, f, l, c| async move {
            let result = ctx.lsp.prepare_rename(&f, l, c).await?;
            Ok(match result {
                Some(r) => serde_json::json!({
                    "placeholder": r.placeholder,
                    "range": {
                        "start": { "line": r.range.start.line, "character": r.range.start.character },
                        "end": { "line": r.range.end.line, "character": r.range.end.character }
                    }
                }),
                None => serde_json::json!({}),
            })
        }).await,

        methods::CODE_ACTIONS => handle_position(&params, projects, |ctx, f, l, c| async move {
            let actions = ctx.lsp.code_actions(&f, l, c).await?;
            Ok(serde_json::json!({
                "count": actions.len(),
                "actions": actions.iter().map(|a| CodeActionJson {
                    title: a.title.clone(),
                    kind: a.kind.to_string(),
                    is_preferred: a.is_preferred,
                    diagnostics: a.diagnostics.clone(),
                }).collect::<Vec<_>>()
            }))
        }).await,

        // File-based operations
        methods::DIAGNOSTICS => handle_file(&params, projects, |ctx, f| async move {
            let diags = ctx.lsp.diagnostics(&f).await?;
            Ok(serde_json::json!({
                "count": diags.len(),
                "diagnostics": diags.iter().map(|d| serde_json::json!({
                    "message": d.message,
                    "severity": format!("{:?}", d.severity),
                    "line": d.range.start.line + 1,
                    "column": d.range.start.character + 1,
                })).collect::<Vec<_>>()
            }))
        }).await,

        methods::FOLDING_RANGES => handle_file(&params, projects, |ctx, f| async move {
            let ranges = ctx.lsp.folding_ranges(&f).await?;
            Ok(serde_json::json!({
                "count": ranges.len(),
                "ranges": ranges.iter().map(|r| FoldingRangeJson {
                    start_line: r.start_line,
                    end_line: r.end_line,
                    start_character: r.start_character,
                    end_character: r.end_character,
                    kind: Some(r.kind.to_string()),
                    collapsed_text: r.collapsed_text.clone(),
                }).collect::<Vec<_>>()
            }))
        }).await,

        methods::CODE_LENS => handle_file(&params, projects, |ctx, f| async move {
            let lenses = ctx.lsp.code_lens(&f).await?;
            Ok(serde_json::json!({
                "count": lenses.len(),
                "lenses": lenses.iter().map(|l| CodeLensJson {
                    start_line: l.range.start.line,
                    start_character: l.range.start.character,
                    end_line: l.range.end.line,
                    end_character: l.range.end.character,
                    command: l.command.as_ref().map(|c| CodeLensCommandJson {
                        title: c.title.clone(),
                        command: c.command.clone(),
                        arguments: c.arguments.clone(),
                    }),
                    data: l.data.clone(),
                }).collect::<Vec<_>>()
            }))
        }).await,

        // Special operations
        methods::RENAME => handle_rename(&params, projects).await,
        methods::INLAY_HINTS => handle_inlay_hints(&params, projects).await,
        methods::SELECTION_RANGES => handle_selection_ranges(&params, projects).await,
        methods::APPLY_CODE_ACTION => handle_apply_action(&params, projects).await,

        _ => Err(RpcError::method_not_found(&request.method)),
    }
}

// ============================================================================
// Handler Helpers
// ============================================================================

fn parse_params<T: DeserializeOwned>(params: &serde_json::Value) -> Result<T, RpcError> {
    serde_json::from_value(params.clone()).map_err(|e| RpcError::invalid_params(&e.to_string()))
}

async fn get_context(
    projects: &ProjectsMap,
    project: &str,
) -> Result<Arc<ProjectContext>, RpcError> {
    let path = PathBuf::from(project);

    {
        let guard = projects.read().await;
        if let Some(ctx) = guard.get(&path) {
            return Ok(Arc::clone(ctx));
        }
    }

    let ctx = Arc::new(ProjectContext::new(&path));
    let mut guard = projects.write().await;
    guard.insert(path, Arc::clone(&ctx));
    Ok(ctx)
}

/// Generic handler for position-based operations (file, line, column)
async fn handle_position<F, Fut>(
    params: &serde_json::Value,
    projects: &ProjectsMap,
    handler: F,
) -> Result<serde_json::Value, RpcError>
where
    F: FnOnce(Arc<ProjectContext>, PathBuf, u32, u32) -> Fut,
    Fut: std::future::Future<Output = Result<serde_json::Value, crate::error::LspError>>,
{
    let p: PositionParams = parse_params(params)?;
    let ctx = get_context(projects, &p.project).await?;
    ctx.touch().await;
    handler(ctx, PathBuf::from(p.file), p.line, p.column)
        .await
        .map_err(RpcError::from)
}

/// Generic handler for file-based operations
async fn handle_file<F, Fut>(
    params: &serde_json::Value,
    projects: &ProjectsMap,
    handler: F,
) -> Result<serde_json::Value, RpcError>
where
    F: FnOnce(Arc<ProjectContext>, PathBuf) -> Fut,
    Fut: std::future::Future<Output = Result<serde_json::Value, crate::error::LspError>>,
{
    let p: FileParams = parse_params(params)?;
    let ctx = get_context(projects, &p.project).await?;
    ctx.touch().await;
    handler(ctx, PathBuf::from(p.file))
        .await
        .map_err(RpcError::from)
}

// ============================================================================
// Special Handlers
// ============================================================================

async fn handle_status(
    projects: &ProjectsMap,
    config: &DaemonConfig,
    start_time: Instant,
) -> Result<serde_json::Value, RpcError> {
    let guard = projects.read().await;
    let active: Vec<_> = guard
        .iter()
        .map(|(path, ctx)| {
            serde_json::json!({
                "project": path.display().to_string(),
                "requests": ctx.request_count.load(Ordering::Relaxed),
            })
        })
        .collect();

    Ok(serde_json::json!({
        "running": true,
        "pid": std::process::id(),
        "uptime_secs": start_time.elapsed().as_secs(),
        "socket_path": config.socket_path.display().to_string(),
        "active_projects": active.len(),
        "projects": active,
    }))
}

async fn handle_find_symbols(
    params: &serde_json::Value,
    projects: &ProjectsMap,
) -> Result<serde_json::Value, RpcError> {
    let p: FileParams = parse_params(params)?;
    let ctx = get_context(projects, &p.project).await?;
    ctx.touch().await;

    let options = FindSymbolsOptions {
        include_body: p.body,
        depth: p.depth,
    };

    let symbols = ctx
        .lsp
        .find_symbols(Path::new(&p.file), options)
        .await
        .map_err(RpcError::from)?;

    Ok(serde_json::json!({
        "count": symbols.len(),
        "symbols": symbols.iter().map(SymbolDto::from_symbol).collect::<Vec<_>>()
    }))
}

async fn handle_workspace_symbol(
    params: &serde_json::Value,
    projects: &ProjectsMap,
) -> Result<serde_json::Value, RpcError> {
    let p: WorkspaceSymbolParams = parse_params(params)?;
    let ctx = get_context(projects, &p.project).await?;
    ctx.touch().await;

    let language = p
        .language
        .as_deref()
        .map(crate::models::symbol::Language::from_str_loose)
        .unwrap_or(crate::models::symbol::Language::Unknown);

    let symbols = ctx
        .lsp
        .workspace_symbols(&p.query, language)
        .await
        .map_err(RpcError::from)?;

    Ok(serde_json::json!({
        "count": symbols.len(),
        "symbols": symbols.iter().map(|s| serde_json::json!({
            "name": s.name,
            "kind": s.kind.to_string(),
            "file": s.location.file.display().to_string(),
            "line": s.location.line,
            "column": s.location.column,
            "container": s.container,
        })).collect::<Vec<_>>()
    }))
}

async fn handle_rename(
    params: &serde_json::Value,
    projects: &ProjectsMap,
) -> Result<serde_json::Value, RpcError> {
    let p: RenameParams = parse_params(params)?;
    let ctx = get_context(projects, &p.project).await?;
    ctx.touch().await;

    let result = ctx
        .lsp
        .rename(Path::new(&p.file), p.line, p.column, &p.new_name)
        .await
        .map_err(RpcError::from)?;

    Ok(serde_json::json!({
        "changes": result.changes.iter().map(|c| FileChangeJson {
            file: c.file.display().to_string(),
            edit_count: c.edit_count,
        }).collect::<Vec<_>>()
    }))
}

async fn handle_inlay_hints(
    params: &serde_json::Value,
    projects: &ProjectsMap,
) -> Result<serde_json::Value, RpcError> {
    let p: RangeParams = parse_params(params)?;
    let ctx = get_context(projects, &p.project).await?;
    ctx.touch().await;

    let range = crate::models::lsp::Range::new(
        crate::models::lsp::Position::new(p.start_line, p.start_column),
        crate::models::lsp::Position::new(p.end_line, p.end_column),
    );

    let hints = ctx
        .lsp
        .inlay_hints(Path::new(&p.file), range)
        .await
        .map_err(RpcError::from)?;

    Ok(serde_json::json!({
        "count": hints.len(),
        "hints": hints.iter().map(|h| InlayHintJson {
            line: h.position.line,
            character: h.position.character,
            label: h.label.clone(),
            kind: match h.kind {
                crate::models::lsp::InlayHintKind::Type => 1,
                crate::models::lsp::InlayHintKind::Parameter => 2,
            },
            padding_left: h.padding_left,
            padding_right: h.padding_right,
        }).collect::<Vec<_>>()
    }))
}

async fn handle_selection_ranges(
    params: &serde_json::Value,
    projects: &ProjectsMap,
) -> Result<serde_json::Value, RpcError> {
    let p: SelectionRangeParams = parse_params(params)?;
    let ctx = get_context(projects, &p.project).await?;
    ctx.touch().await;

    let positions: Vec<(u32, u32)> = p.positions.iter().map(|p| (p.line, p.column)).collect();

    let ranges = ctx
        .lsp
        .selection_ranges(Path::new(&p.file), positions)
        .await
        .map_err(RpcError::from)?;

    fn to_json(r: &crate::models::lsp::SelectionRange) -> SelectionRangeJson {
        SelectionRangeJson {
            start_line: r.range.start.line,
            start_character: r.range.start.character,
            end_line: r.range.end.line,
            end_character: r.range.end.character,
            parent: r.parent.as_ref().map(|p| Box::new(to_json(p))),
        }
    }

    Ok(serde_json::json!({
        "count": ranges.len(),
        "ranges": ranges.iter().map(to_json).collect::<Vec<_>>()
    }))
}

async fn handle_apply_action(
    params: &serde_json::Value,
    projects: &ProjectsMap,
) -> Result<serde_json::Value, RpcError> {
    let p: ApplyActionParams = parse_params(params)?;
    let ctx = get_context(projects, &p.project).await?;
    ctx.touch().await;

    let action: crate::models::lsp::CodeAction = serde_json::from_value(p.action)
        .map_err(|e| RpcError::invalid_params(&format!("Invalid action: {}", e)))?;

    let result = ctx
        .lsp
        .apply_code_action(Path::new(&p.file), &action)
        .await
        .map_err(RpcError::from)?;

    Ok(serde_json::json!({
        "changes": result.changes.iter().map(|c| serde_json::json!({
            "file": c.file.display().to_string(),
            "edits": c.edits.iter().map(|e| serde_json::json!({
                "range": {
                    "start": { "line": e.range.start.line, "character": e.range.start.character },
                    "end": { "line": e.range.end.line, "character": e.range.end.character }
                },
                "new_text": e.new_text,
            })).collect::<Vec<_>>()
        })).collect::<Vec<_>>()
    }))
}
