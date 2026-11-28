use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::time::Instant;

use crate::daemon::params::{ApplyActionParams, FileParams, RenameParams, WorkspaceSymbolParams};
use crate::daemon::protocol::RpcError;
use crate::daemon::wire::{self, RenameResponse, SymbolsResponse};
use crate::models::lsp::FindSymbolsOptions;
use crate::models::symbol::Language;
use crate::services::lsp::LspService;
use crate::services::store::StoreService;

use super::config::DaemonRuntimeConfig;
use super::context::{ProjectContext, ProjectsMap};
use super::dispatch::{parse_params, to_json};

pub(super) async fn handle_status(
    projects: &ProjectsMap,
    config: &DaemonRuntimeConfig,
    start_time: Instant,
) -> Result<serde_json::Value, RpcError> {
    let contexts: Vec<(PathBuf, Arc<ProjectContext>, u64)> = {
        let guard = projects.read().await;
        guard
            .iter()
            .map(|(p, c)| {
                (
                    p.clone(),
                    Arc::clone(c),
                    c.request_count.load(Ordering::Relaxed),
                )
            })
            .collect()
    };

    let mut total_symbols = 0u64;
    let mut total_files = 0u64;
    // A project whose counts could not be read adds none to the totals,
    // which are then a lower bound.
    let mut incomplete = false;

    let mut active = Vec::with_capacity(contexts.len());
    for (path, ctx, request_count) in &contexts {
        let store = match ctx.store.index_status().await {
            Ok(stats) => {
                total_symbols += stats.symbol_count as u64;
                total_files += stats.file_count as u64;
                serde_json::json!({
                    "symbols": stats.symbol_count,
                    "files": stats.file_count,
                    "content_lines": stats.content_line_count,
                })
            }
            // In the form a store request's failure crosses the wire, so the
            // client reports it as it reports that request's.
            Err(e) => {
                tracing::warn!("Failed to get store stats for {}: {}", path.display(), e);
                incomplete = true;
                serde_json::json!({ "error": RpcError::from(e) })
            }
        };

        active.push(serde_json::json!({
            "project": path.display().to_string(),
            "requests": request_count,
            "store": store,
        }));
    }

    let mut store_totals = serde_json::Map::new();
    if incomplete {
        store_totals.insert("incomplete".to_string(), true.into());
    }
    store_totals.insert("symbols".to_string(), total_symbols.into());
    store_totals.insert("files".to_string(), total_files.into());

    Ok(serde_json::json!({
        "running": true,
        "pid": std::process::id(),
        "uptime_secs": start_time.elapsed().as_secs(),
        "socket_path": config.socket_path.display().to_string(),
        "active_projects": active.len(),
        "projects": active,
        "store_totals": store_totals,
    }))
}

pub(super) async fn handle_find_symbols(
    params: &serde_json::Value,
    ctx: &ProjectContext,
) -> Result<serde_json::Value, RpcError> {
    let p: FileParams = parse_params(params)?;

    let options = FindSymbolsOptions {
        include_body: p.body,
        depth: p.depth,
    };

    let symbols = ctx
        .lsp
        .find_symbols(Path::new(&p.file), options)
        .await
        .map_err(RpcError::from)?;

    to_json(SymbolsResponse {
        count: symbols.len(),
        symbols: symbols.iter().map(wire::Symbol::from).collect(),
        // Document symbols are a single-file query; the indexing marker
        // belongs to workspace-dependent answers only.
        indexing: None,
    })
    .map_err(RpcError::from)
}

pub(super) async fn handle_workspace_symbols(
    params: &serde_json::Value,
    ctx: &ProjectContext,
) -> Result<serde_json::Value, RpcError> {
    let p: WorkspaceSymbolParams = parse_params(params)?;

    let language = p
        .language
        .as_deref()
        .map(Language::parse_or_default)
        .unwrap_or(Language::Unknown);

    let result = ctx
        .lsp
        .workspace_symbols(&p.query, language)
        .await
        .map_err(RpcError::from)?;

    to_json(SymbolsResponse {
        count: result.data.len(),
        symbols: result.data.iter().map(wire::Symbol::from).collect(),
        indexing: result.indexing,
    })
    .map_err(RpcError::from)
}

// Rename and code-action handlers only COMPUTE workspace edits — the
// write happens in the requesting process after this response returns,
// and that writer then refreshes each touched file through the store
// service (`refresh_file`), which lands back here post-write. Touching
// the store or the symbol cache before the bytes change would re-index
// the pre-write content and accomplish nothing.

pub(super) async fn handle_rename(
    params: &serde_json::Value,
    ctx: &ProjectContext,
) -> Result<serde_json::Value, RpcError> {
    let p: RenameParams = parse_params(params)?;

    let result = ctx
        .lsp
        .rename(Path::new(&p.file), p.line, p.column, &p.new_name)
        .await
        .map_err(RpcError::from)?;

    to_json(RenameResponse {
        changes: result
            .data
            .map(|result| result.changes.iter().map(wire::FileChange::from).collect()),
        indexing: result.indexing,
    })
    .map_err(RpcError::from)
}

pub(super) async fn handle_apply_action(
    params: &serde_json::Value,
    ctx: &ProjectContext,
) -> Result<serde_json::Value, RpcError> {
    let p: ApplyActionParams = parse_params(params)?;

    let action: crate::models::lsp::CodeAction = serde_json::from_value(p.action)
        .map_err(|e| RpcError::invalid_params(&format!("Invalid action: {}", e)))?;

    let result = ctx
        .lsp
        .apply_code_action(Path::new(&p.file), &action)
        .await
        .map_err(RpcError::from)?;

    to_json(wire::ApplyActionResponse {
        changes: result.changes.iter().map(wire::FileChange::from).collect(),
    })
    .map_err(RpcError::from)
}

/// Bring the daemon's language layer in line with files the requesting
/// process just wrote: symbol-cache invalidation, workspace-generation
/// bump, and a live server's overlay sync + save — the daemon-side half
/// of the edit flow's `note_files_edited`.
pub(super) async fn handle_note_files_edited(
    params: &serde_json::Value,
    ctx: &ProjectContext,
) -> Result<serde_json::Value, RpcError> {
    let p: crate::daemon::params::EditedFilesParams = parse_params(params)?;

    let paths: Vec<PathBuf> = p.files.iter().map(PathBuf::from).collect();
    ctx.lsp.note_files_edited(&paths).await;
    Ok(serde_json::json!({"noted": true}))
}
