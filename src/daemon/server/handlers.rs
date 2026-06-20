use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::time::Instant;

use crate::config::LspRuntimeConfig;
use crate::daemon::params::{
    ApplyActionParams, FileParams, InlayHintsParams, LanguageStatusParams, RenameParams,
    SelectionRangeParams, WorkspaceSymbolParams,
};
use crate::daemon::protocol::RpcError;
use crate::daemon::wire::{
    self, InlayHintsResponse, RenameResponse, SelectionRangesResponse, SymbolsResponse,
};
use crate::models::lsp::{FindSymbolsOptions, ServerStatus};
use crate::models::symbol::Language;
use crate::services::lsp::LspService;
use crate::services::store::StoreService;

use super::config::DaemonRuntimeConfig;
use super::context::{ProjectContext, ProjectsMap, get_context};
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

    let mut active = Vec::with_capacity(contexts.len());
    for (path, ctx, request_count) in &contexts {
        let stats = match ctx.store.index_status().await {
            Ok(s) => s,
            Err(e) => {
                tracing::warn!("Failed to get store stats for {}: {}", path.display(), e);
                Default::default()
            }
        };

        total_symbols += stats.symbol_count as u64;
        total_files += stats.file_count as u64;

        active.push(serde_json::json!({
            "project": path.display().to_string(),
            "requests": request_count,
            "store": {
                "symbols": stats.symbol_count,
                "files": stats.file_count,
                "content_lines": stats.content_line_count,
            }
        }));
    }

    Ok(serde_json::json!({
        "running": true,
        "pid": std::process::id(),
        "uptime_secs": start_time.elapsed().as_secs(),
        "socket_path": config.socket_path.display().to_string(),
        "active_projects": active.len(),
        "projects": active,
        "store_totals": {
            "symbols": total_symbols,
            "files": total_files,
        }
    }))
}

pub(super) async fn handle_find_symbols(
    params: &serde_json::Value,
    projects: &ProjectsMap,
    lsp_config: &Arc<LspRuntimeConfig>,
) -> Result<serde_json::Value, RpcError> {
    let p: FileParams = parse_params(params)?;
    let ctx = get_context(projects, &p.project, lsp_config).await?;
    ctx.touch();

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
    projects: &ProjectsMap,
    lsp_config: &Arc<LspRuntimeConfig>,
) -> Result<serde_json::Value, RpcError> {
    let p: WorkspaceSymbolParams = parse_params(params)?;
    let ctx = get_context(projects, &p.project, lsp_config).await?;
    ctx.touch();

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
    projects: &ProjectsMap,
    lsp_config: &Arc<LspRuntimeConfig>,
) -> Result<serde_json::Value, RpcError> {
    let p: RenameParams = parse_params(params)?;
    let ctx = get_context(projects, &p.project, lsp_config).await?;
    ctx.touch();

    let result = ctx
        .lsp
        .rename(Path::new(&p.file), p.line, p.column, &p.new_name)
        .await
        .map_err(RpcError::from)?;

    to_json(RenameResponse {
        changes: result.changes.iter().map(wire::FileChange::from).collect(),
    })
    .map_err(RpcError::from)
}

pub(super) async fn handle_inlay_hints(
    params: &serde_json::Value,
    projects: &ProjectsMap,
    lsp_config: &Arc<LspRuntimeConfig>,
) -> Result<serde_json::Value, RpcError> {
    let p: InlayHintsParams = parse_params(params)?;
    let ctx = get_context(projects, &p.project, lsp_config).await?;
    ctx.touch();

    let hints = ctx
        .lsp
        .inlay_hints(Path::new(&p.file), p.start_line, p.end_line)
        .await
        .map_err(RpcError::from)?;

    to_json(InlayHintsResponse {
        count: hints.len(),
        hints: hints
            .iter()
            .map(|h| wire::InlayHint {
                line: h.position.line,
                character: h.position.character,
                label: h.label.clone(),
                kind: Some(h.kind.to_lsp()),
                padding_left: h.padding_left,
                padding_right: h.padding_right,
            })
            .collect(),
    })
    .map_err(RpcError::from)
}

pub(super) async fn handle_selection_ranges(
    params: &serde_json::Value,
    projects: &ProjectsMap,
    lsp_config: &Arc<LspRuntimeConfig>,
) -> Result<serde_json::Value, RpcError> {
    let p: SelectionRangeParams = parse_params(params)?;
    let ctx = get_context(projects, &p.project, lsp_config).await?;
    ctx.touch();

    let positions: Vec<(u32, u32)> = p
        .positions
        .iter()
        .map(|pos| (pos.line, pos.column))
        .collect();

    let ranges = ctx
        .lsp
        .selection_ranges(Path::new(&p.file), positions)
        .await
        .map_err(RpcError::from)?;

    fn to_wire(r: &crate::models::lsp::SelectionRange) -> wire::SelectionRange {
        wire::SelectionRange {
            start_line: r.range.start.line,
            start_character: r.range.start.character,
            end_line: r.range.end.line,
            end_character: r.range.end.character,
            parent: r.parent.as_ref().map(|p| Box::new(to_wire(p))),
        }
    }

    to_json(SelectionRangesResponse {
        count: ranges.len(),
        ranges: ranges.iter().map(to_wire).collect(),
    })
    .map_err(RpcError::from)
}

pub(super) async fn handle_apply_action(
    params: &serde_json::Value,
    projects: &ProjectsMap,
    lsp_config: &Arc<LspRuntimeConfig>,
) -> Result<serde_json::Value, RpcError> {
    let p: ApplyActionParams = parse_params(params)?;
    let ctx = get_context(projects, &p.project, lsp_config).await?;
    ctx.touch();

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

pub(super) async fn handle_language_status(
    params: &serde_json::Value,
    projects: &ProjectsMap,
    lsp_config: &Arc<LspRuntimeConfig>,
) -> Result<serde_json::Value, RpcError> {
    let p: LanguageStatusParams = parse_params(params)?;
    let ctx = get_context(projects, &p.project, lsp_config).await?;
    ctx.touch();

    let language = Language::parse_or_default(&p.language);
    let available = ctx.lsp.is_available(language).await;
    let status = ctx.lsp.server_status(language).await;

    let status_str = match &status {
        ServerStatus::Running => "running",
        ServerStatus::Stopped => "stopped",
        ServerStatus::NotInstalled { .. } => "not_installed",
        ServerStatus::NotSupported => "not_supported",
        ServerStatus::CriticalFailure { .. } => "critical_failure",
    };

    let install_hint = match &status {
        ServerStatus::NotInstalled { hint } => hint.clone(),
        _ => None,
    };

    let reason = match &status {
        ServerStatus::CriticalFailure { reason } => Some(reason.clone()),
        _ => None,
    };

    // Omit the optionals when absent rather than emitting `null` filler: an
    // install hint exists only for `not_installed`, a reason only for
    // `critical_failure`. The client decodes both with `get`, so absence is the
    // signal — matching the omit-when-absent contract the wire types follow.
    let mut value = serde_json::json!({
        "language": p.language,
        "available": available,
        "status": status_str,
    });
    let obj = value
        .as_object_mut()
        .expect("json! map is always an object");
    if let Some(hint) = install_hint {
        obj.insert("install_hint".to_string(), serde_json::Value::String(hint));
    }
    if let Some(reason) = reason {
        obj.insert("reason".to_string(), serde_json::Value::String(reason));
    }
    Ok(value)
}

/// Bring the daemon's language layer in line with files the requesting
/// process just wrote: symbol-cache invalidation, workspace-generation
/// bump, and a live server's overlay sync + save — the daemon-side half
/// of the edit flow's `note_files_edited`.
pub(super) async fn handle_note_files_edited(
    params: &serde_json::Value,
    projects: &ProjectsMap,
    lsp_config: &Arc<LspRuntimeConfig>,
) -> Result<serde_json::Value, RpcError> {
    let p: crate::daemon::params::EditedFilesParams = parse_params(params)?;
    let ctx = get_context(projects, &p.project, lsp_config).await?;
    ctx.touch();

    let paths: Vec<PathBuf> = p.files.iter().map(PathBuf::from).collect();
    ctx.lsp.note_files_edited(&paths).await;
    Ok(serde_json::json!({"noted": true}))
}
