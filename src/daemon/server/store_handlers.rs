use crate::daemon::params::{
    EditedFilesParams, IndexBuildParams, ProjectParams, SearchContentParams, SearchSymbolsParams,
};
use crate::daemon::protocol::RpcError;
use crate::daemon::wire;
use crate::models::symbol::{Language, SymbolKind};
use crate::services::store::{IndexOptions, SearchPage, StoreService};

use super::context::{ProjectsMap, get_context};
use super::dispatch::parse_params;

/// Re-index just-edited files in the store. A failure travels back to the
/// requesting process as an error so its edit layer can log the warn the
/// disclosed-best-effort path expects — the edit itself already succeeded,
/// and the daemon never swallows the failure silently.
pub(super) async fn handle_refresh_files(
    params: &serde_json::Value,
    projects: &ProjectsMap,
) -> Result<serde_json::Value, RpcError> {
    let p: EditedFilesParams = parse_params(params)?;
    let ctx = get_context(projects, &p.project).await?;
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
) -> Result<serde_json::Value, RpcError> {
    let p: SearchSymbolsParams = parse_params(params)?;
    let ctx = get_context(projects, &p.project).await?;
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

    serde_json::to_value(search_response(&page, items)).map_err(RpcError::from)
}

pub(super) async fn handle_search_content(
    params: &serde_json::Value,
    projects: &ProjectsMap,
) -> Result<serde_json::Value, RpcError> {
    let p: SearchContentParams = parse_params(params)?;
    let ctx = get_context(projects, &p.project).await?;
    ctx.touch();

    let languages: Vec<Language> = p
        .languages
        .iter()
        .map(|l| Language::parse_or_default(l))
        .collect();

    let page = ctx
        .store
        .search_content(&p.query, p.limit.unwrap_or(100), &languages)
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

    serde_json::to_value(search_response(&page, items)).map_err(RpcError::from)
}

/// Every field of the page except its rows, which the caller has already
/// rendered. Destructured rather than read field by field, so a field added to
/// the page fails to compile here until the wire carries it — the daemon and a
/// direct run would otherwise disagree about a fact only one of them has.
fn search_response<T>(
    page: &SearchPage<T>,
    items: Vec<serde_json::Value>,
) -> wire::SearchResponse<serde_json::Value> {
    let SearchPage {
        total,
        rows: _,
        stale_files,
        covered,
        unread_paths,
    } = page;
    wire::SearchResponse {
        count: *total,
        items,
        stale_files: stale_files.clone(),
        covered: covered.iter().map(|l| l.lsp_id().to_string()).collect(),
        unread_paths: unread_paths.clone(),
    }
}

pub(super) async fn handle_index_build(
    params: &serde_json::Value,
    projects: &ProjectsMap,
) -> Result<serde_json::Value, RpcError> {
    let p: IndexBuildParams = parse_params(params)?;
    let ctx = get_context(projects, &p.project).await?;
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
) -> Result<serde_json::Value, RpcError> {
    let p: ProjectParams = parse_params(params)?;
    let ctx = get_context(projects, &p.project).await?;
    ctx.touch();

    let stats = ctx.store.index_status().await.map_err(RpcError::from)?;
    serde_json::to_value(stats).map_err(RpcError::from)
}

pub(super) async fn handle_index_is_current(
    params: &serde_json::Value,
    projects: &ProjectsMap,
) -> Result<serde_json::Value, RpcError> {
    let p: ProjectParams = parse_params(params)?;
    let ctx = get_context(projects, &p.project).await?;
    ctx.touch();

    let current = ctx.store.index_is_current().await.map_err(RpcError::from)?;
    serde_json::to_value(current).map_err(RpcError::from)
}

pub(super) async fn handle_indexed_languages(
    params: &serde_json::Value,
    projects: &ProjectsMap,
) -> Result<serde_json::Value, RpcError> {
    let p: ProjectParams = parse_params(params)?;
    let ctx = get_context(projects, &p.project).await?;
    ctx.touch();

    let languages = ctx
        .store
        .indexed_languages()
        .await
        .map_err(RpcError::from)?;
    serde_json::to_value(languages).map_err(RpcError::from)
}

pub(super) async fn handle_index_clear(
    params: &serde_json::Value,
    projects: &ProjectsMap,
) -> Result<serde_json::Value, RpcError> {
    let p: ProjectParams = parse_params(params)?;
    let ctx = get_context(projects, &p.project).await?;
    ctx.touch();

    ctx.store.index_clear().await.map_err(RpcError::from)?;

    Ok(serde_json::json!({
        "cleared": true
    }))
}
