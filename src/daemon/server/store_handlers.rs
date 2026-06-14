use std::sync::Arc;

use crate::cli::response::Section;
use crate::config::LspRuntimeConfig;
use crate::daemon::params::{
    EditedFilesParams, IndexBuildParams, ProjectParams, SearchContentParams, SearchSymbolsParams,
};
use crate::daemon::protocol::RpcError;
use crate::models::symbol::{Language, SymbolKind};
use crate::services::store::{IndexOptions, StoreService};

use super::context::{ProjectsMap, get_context};
use super::dispatch::parse_params;

/// Re-index just-edited files in the store. A failure travels back to the
/// requesting process as an error so its edit layer can log the warn the
/// disclosed-best-effort path expects — the edit itself already succeeded,
/// and the daemon never swallows the failure silently.
pub(super) async fn handle_refresh_files(
    params: &serde_json::Value,
    projects: &ProjectsMap,
    lsp_config: &Arc<LspRuntimeConfig>,
) -> Result<serde_json::Value, RpcError> {
    let p: EditedFilesParams = parse_params(params)?;
    let ctx = get_context(projects, &p.project, lsp_config).await?;
    ctx.touch();
    let paths: Vec<std::path::PathBuf> = p.files.iter().map(std::path::PathBuf::from).collect();
    ctx.store
        .refresh_files(&paths)
        .await
        .map_err(RpcError::from)?;
    Ok(serde_json::json!({"refreshed": true}))
}

pub(super) async fn handle_search_symbols(
    params: &serde_json::Value,
    projects: &ProjectsMap,
    lsp_config: &Arc<LspRuntimeConfig>,
) -> Result<serde_json::Value, RpcError> {
    let p: SearchSymbolsParams = parse_params(params)?;
    let ctx = get_context(projects, &p.project, lsp_config).await?;
    ctx.touch();

    let kind_filter = p.kind.as_ref().map(|k| SymbolKind::parse_or_default(k));
    let lang_filter = p
        .language
        .as_ref()
        .map(|l| crate::models::symbol::Language::parse_or_default(l));

    let page = ctx
        .store
        .search_symbols(&p.query, p.limit.unwrap_or(100), kind_filter, lang_filter)
        .await
        .map_err(RpcError::from)?;

    let items = page
        .rows
        .iter()
        .map(|r| {
            serde_json::json!({
                "name": r.name,
                "name_path": r.name_path,
                "kind": r.kind.to_string(),
                "file": r.file.display().to_string(),
                "line": r.line,
                "column": r.column,
                "container": r.container,
                "score": r.score,
            })
        })
        .collect();

    serde_json::to_value(Section::with_total(items, page.total).with_stale(page.stale))
        .map_err(RpcError::from)
}

pub(super) async fn handle_search_content(
    params: &serde_json::Value,
    projects: &ProjectsMap,
    lsp_config: &Arc<LspRuntimeConfig>,
) -> Result<serde_json::Value, RpcError> {
    let p: SearchContentParams = parse_params(params)?;
    let ctx = get_context(projects, &p.project, lsp_config).await?;
    ctx.touch();

    let language = p.language.as_ref().map(|l| Language::parse_or_default(l));

    let page = ctx
        .store
        .search_content(&p.query, p.limit.unwrap_or(100), language)
        .await
        .map_err(RpcError::from)?;

    let items = page
        .rows
        .iter()
        .map(|r| {
            serde_json::json!({
                "file": r.file.display().to_string(),
                "line": r.line,
                "content": r.content,
                "score": r.score,
            })
        })
        .collect();

    serde_json::to_value(Section::with_total(items, page.total).with_stale(page.stale))
        .map_err(RpcError::from)
}

pub(super) async fn handle_index_build(
    params: &serde_json::Value,
    projects: &ProjectsMap,
    lsp_config: &Arc<LspRuntimeConfig>,
) -> Result<serde_json::Value, RpcError> {
    let p: IndexBuildParams = parse_params(params)?;
    let ctx = get_context(projects, &p.project, lsp_config).await?;
    ctx.touch();

    let languages: Option<Vec<Language>> = p.languages.as_ref().map(|langs| {
        langs
            .iter()
            .map(|l| Language::parse_or_default(l))
            .collect()
    });

    let options = IndexOptions {
        force: p.force,
        languages,
    };

    let stats = ctx.store.index(options).await.map_err(RpcError::from)?;
    serde_json::to_value(stats).map_err(RpcError::from)
}

pub(super) async fn handle_index_status(
    params: &serde_json::Value,
    projects: &ProjectsMap,
    lsp_config: &Arc<LspRuntimeConfig>,
) -> Result<serde_json::Value, RpcError> {
    let p: ProjectParams = parse_params(params)?;
    let ctx = get_context(projects, &p.project, lsp_config).await?;
    ctx.touch();

    let stats = ctx.store.index_status().await.map_err(RpcError::from)?;
    serde_json::to_value(stats).map_err(RpcError::from)
}

pub(super) async fn handle_index_clear(
    params: &serde_json::Value,
    projects: &ProjectsMap,
    lsp_config: &Arc<LspRuntimeConfig>,
) -> Result<serde_json::Value, RpcError> {
    let p: ProjectParams = parse_params(params)?;
    let ctx = get_context(projects, &p.project, lsp_config).await?;
    ctx.touch();

    ctx.store.index_clear().await.map_err(RpcError::from)?;

    Ok(serde_json::json!({
        "cleared": true
    }))
}
