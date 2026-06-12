use std::path::PathBuf;
use std::sync::Arc;
use std::time::Instant;

use serde::Serialize;
use serde::de::DeserializeOwned;

use crate::config::LspRuntimeConfig;
use crate::daemon::params::{FileParams, PositionParams};
use crate::daemon::protocol::{Request, RpcError, methods};
use crate::daemon::wire;
use crate::error::LspError;
use crate::services::lsp::LspService;

use super::config::DaemonRuntimeConfig;
use super::context::{ProjectContext, ProjectsMap, get_context};
use super::handlers;
use super::store_handlers;

pub(super) async fn dispatch(
    request: Request,
    projects: &ProjectsMap,
    config: &DaemonRuntimeConfig,
    lsp_config: &Arc<LspRuntimeConfig>,
    start_time: Instant,
) -> Result<serde_json::Value, RpcError> {
    let params = request.params.unwrap_or(serde_json::json!({}));

    match request.method.as_str() {
        // System
        // `version` lets the client detect a daemon left over from a
        // different binary and restart it before any wire exchange, so the
        // wire format never needs cross-version compatibility.
        methods::PING => Ok(serde_json::json!({
            "pong": true,
            "version": env!("CARGO_PKG_VERSION"),
        })),
        methods::STATUS => handlers::handle_status(projects, config, start_time).await,
        methods::SHUTDOWN => Ok(serde_json::json!({"shutting_down": true})),

        // Symbol operations
        methods::FIND_SYMBOLS => handlers::handle_find_symbols(&params, projects, lsp_config).await,
        methods::WORKSPACE_SYMBOLS => {
            handlers::handle_workspace_symbols(&params, projects, lsp_config).await
        }

        // Position-based operations
        methods::FIND_REFERENCES => {
            handle_position(&params, projects, lsp_config, |ctx, f, l, c| async move {
                to_json(wire::ReferencesResponse::from(
                    ctx.lsp.find_references(&f, l, c).await?,
                ))
            })
            .await
        }

        methods::GOTO_DEFINITION => {
            handle_position(&params, projects, lsp_config, |ctx, f, l, c| async move {
                to_json(wire::DefinitionResponse::from_location(
                    ctx.lsp.goto_definition(&f, l, c).await?,
                    wire::DefinitionKind::Definition,
                ))
            })
            .await
        }

        methods::GOTO_TYPE_DEFINITION => {
            handle_position(&params, projects, lsp_config, |ctx, f, l, c| async move {
                to_json(wire::DefinitionResponse::from_location(
                    ctx.lsp.goto_type_definition(&f, l, c).await?,
                    wire::DefinitionKind::TypeDefinition,
                ))
            })
            .await
        }

        methods::FIND_IMPLEMENTATIONS => {
            handle_position(&params, projects, lsp_config, |ctx, f, l, c| async move {
                to_json(wire::ImplementationsResponse::from(
                    ctx.lsp.find_implementations(&f, l, c).await?,
                ))
            })
            .await
        }

        methods::HOVER => {
            handle_position(&params, projects, lsp_config, |ctx, f, l, c| async move {
                to_json(wire::HoverResponse::from_hover(
                    ctx.lsp.hover(&f, l, c).await?,
                ))
            })
            .await
        }

        methods::SIGNATURE_HELP => {
            handle_position(&params, projects, lsp_config, |ctx, f, l, c| async move {
                to_json(wire::SignatureResponse::from_help(
                    ctx.lsp.signature_help(&f, l, c).await?,
                ))
            })
            .await
        }

        methods::INCOMING_CALLS => {
            handle_position(&params, projects, lsp_config, |ctx, f, l, c| async move {
                to_json(wire::CallsResponse::from(
                    ctx.lsp.incoming_calls(&f, l, c).await?,
                ))
            })
            .await
        }

        methods::OUTGOING_CALLS => {
            handle_position(&params, projects, lsp_config, |ctx, f, l, c| async move {
                to_json(wire::CallsResponse::from(
                    ctx.lsp.outgoing_calls(&f, l, c).await?,
                ))
            })
            .await
        }

        methods::SUPERTYPES => {
            handle_position(&params, projects, lsp_config, |ctx, f, l, c| async move {
                to_json(wire::TypeHierarchyResponse::from(
                    ctx.lsp.supertypes(&f, l, c).await?,
                ))
            })
            .await
        }

        methods::SUBTYPES => {
            handle_position(&params, projects, lsp_config, |ctx, f, l, c| async move {
                to_json(wire::TypeHierarchyResponse::from(
                    ctx.lsp.subtypes(&f, l, c).await?,
                ))
            })
            .await
        }

        methods::PREPARE_RENAME => {
            handle_position(&params, projects, lsp_config, |ctx, f, l, c| async move {
                let result = ctx.lsp.prepare_rename(&f, l, c).await?;
                to_json(wire::PrepareRenameResponse {
                    placeholder: result.as_ref().map(|r| r.placeholder.clone()),
                    range: result.map(|r| wire::Range::from(&r.range)),
                })
            })
            .await
        }

        methods::CODE_ACTIONS => {
            handle_position(&params, projects, lsp_config, |ctx, f, l, c| async move {
                to_json(wire::CodeActionsResponse::from_actions(
                    ctx.lsp.code_actions(&f, l, c).await?,
                ))
            })
            .await
        }

        // File-based operations
        methods::DIAGNOSTICS => {
            handle_file(&params, projects, lsp_config, |ctx, f| async move {
                to_json(wire::DiagnosticsResponse::from(
                    ctx.lsp.diagnostics(&f).await?,
                ))
            })
            .await
        }

        methods::FOLDING_RANGES => {
            handle_file(&params, projects, lsp_config, |ctx, f| async move {
                to_json(wire::FoldingRangesResponse::from(
                    ctx.lsp.folding_ranges(&f).await?,
                ))
            })
            .await
        }

        methods::CODE_LENSES => {
            handle_file(&params, projects, lsp_config, |ctx, f| async move {
                to_json(wire::CodeLensResponse::from(ctx.lsp.code_lenses(&f).await?))
            })
            .await
        }

        methods::FORMAT => {
            handle_file(&params, projects, lsp_config, |ctx, f| async move {
                to_json(wire::FormatResponse::from(ctx.lsp.format(&f).await?))
            })
            .await
        }

        // Special operations
        methods::RENAME => handlers::handle_rename(&params, projects, lsp_config).await,
        methods::INLAY_HINTS => handlers::handle_inlay_hints(&params, projects, lsp_config).await,
        methods::SELECTION_RANGES => {
            handlers::handle_selection_ranges(&params, projects, lsp_config).await
        }
        methods::APPLY_CODE_ACTION => {
            handlers::handle_apply_action(&params, projects, lsp_config).await
        }

        // Language status
        methods::LANGUAGE_STATUS => {
            handlers::handle_language_status(&params, projects, lsp_config).await
        }

        // Post-edit notes
        methods::NOTE_FILES_EDITED => {
            handlers::handle_note_files_edited(&params, projects, lsp_config).await
        }

        // Store operations
        methods::REFRESH_FILES => {
            store_handlers::handle_refresh_files(&params, projects, lsp_config).await
        }
        methods::SEARCH_SYMBOLS => {
            store_handlers::handle_search_symbols(&params, projects, lsp_config).await
        }
        methods::SEARCH_CONTENT => {
            store_handlers::handle_search_content(&params, projects, lsp_config).await
        }
        methods::INDEX_BUILD => {
            store_handlers::handle_index_build(&params, projects, lsp_config).await
        }
        methods::INDEX_STATUS => {
            store_handlers::handle_index_status(&params, projects, lsp_config).await
        }
        methods::INDEX_CLEAR => {
            store_handlers::handle_index_clear(&params, projects, lsp_config).await
        }

        _ => Err(RpcError::method_not_found(&request.method)),
    }
}

pub(super) fn parse_params<T: DeserializeOwned>(params: &serde_json::Value) -> Result<T, RpcError> {
    serde_json::from_value(params.clone()).map_err(|e| RpcError::invalid_params(&e.to_string()))
}

pub(super) fn to_json<T: Serialize>(value: T) -> Result<serde_json::Value, LspError> {
    serde_json::to_value(value).map_err(|e| LspError::Protocol(e.to_string()))
}

async fn handle_position<F, Fut>(
    params: &serde_json::Value,
    projects: &ProjectsMap,
    lsp_config: &Arc<LspRuntimeConfig>,
    handler: F,
) -> Result<serde_json::Value, RpcError>
where
    F: FnOnce(Arc<ProjectContext>, PathBuf, u32, u32) -> Fut,
    Fut: std::future::Future<Output = Result<serde_json::Value, LspError>>,
{
    let p: PositionParams = parse_params(params)?;
    let ctx = get_context(projects, &p.project, lsp_config).await?;
    ctx.touch();
    handler(ctx, PathBuf::from(p.file), p.line, p.column)
        .await
        .map_err(RpcError::from)
}

async fn handle_file<F, Fut>(
    params: &serde_json::Value,
    projects: &ProjectsMap,
    lsp_config: &Arc<LspRuntimeConfig>,
    handler: F,
) -> Result<serde_json::Value, RpcError>
where
    F: FnOnce(Arc<ProjectContext>, PathBuf) -> Fut,
    Fut: std::future::Future<Output = Result<serde_json::Value, LspError>>,
{
    let p: FileParams = parse_params(params)?;
    let ctx = get_context(projects, &p.project, lsp_config).await?;
    ctx.touch();
    handler(ctx, PathBuf::from(p.file))
        .await
        .map_err(RpcError::from)
}
