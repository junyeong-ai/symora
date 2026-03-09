use std::path::Path;
use std::sync::Arc;

use crate::error::LspError;
use crate::infra::lsp::LspFeature;
use crate::infra::lsp::protocol::{
    LspLocation, TextDocumentIdentifier, TextDocumentPositionParams,
};
use crate::models::lsp::path_to_uri;
use crate::models::symbol::{Language, Location};

use super::converters::*;
use super::helpers::*;
use super::service::{DefaultLspService, ensure_indexed};

pub(super) async fn find_references(
    service: &DefaultLspService,
    file: &Path,
    line: u32,
    column: u32,
) -> Result<Vec<Location>, LspError> {
    let max_file_size = service.max_file_size_bytes();
    let file = file.to_path_buf();
    let manager = Arc::clone(&service.manager);
    let project_root = manager.root().to_path_buf();

    service
        .execute_with_retry(&file, |client| {
            let file = file.clone();
            let manager = Arc::clone(&manager);
            let project_root = project_root.clone();
            async move {
                ensure_indexed(&client, &file, manager.root()).await;
                client.ensure_cross_file_ready().await;

                let content = read_file_validated(&file, max_file_size).await?;
                let uri = path_to_uri(&file);
                client.sync_document(&uri, &content).await?;

                let params = serde_json::json!({
                    "textDocument": { "uri": uri },
                    "position": to_lsp_position(line, column),
                    "context": { "includeDeclaration": true }
                });

                let result: serde_json::Value = client
                    .request("textDocument/references", Some(params))
                    .await?;

                if result.is_null() {
                    return Ok(Vec::new());
                }

                let locations: Vec<LspLocation> = serde_json::from_value(result)
                    .map_err(|e| LspError::Protocol(e.to_string()))?;

                let all_locations: Vec<Location> = locations.iter().map(convert_location).collect();

                Ok(filter_locations_within_project(
                    all_locations,
                    &project_root,
                ))
            }
        })
        .await
}

pub(super) async fn goto_definition(
    service: &DefaultLspService,
    file: &Path,
    line: u32,
    column: u32,
) -> Result<Option<Location>, LspError> {
    let language = Language::from_path(file);
    goto_location(
        service,
        file,
        line,
        column,
        "textDocument/definition",
        None,
        |locs| select_best_definition(locs, language).map(convert_location),
    )
    .await
}

pub(super) async fn goto_type_definition(
    service: &DefaultLspService,
    file: &Path,
    line: u32,
    column: u32,
) -> Result<Option<Location>, LspError> {
    goto_location(
        service,
        file,
        line,
        column,
        "textDocument/typeDefinition",
        Some(LspFeature::GotoTypeDefinition),
        |locs| locs.first().map(convert_location),
    )
    .await
}

async fn goto_location(
    service: &DefaultLspService,
    file: &Path,
    line: u32,
    column: u32,
    method: &str,
    feature: Option<LspFeature>,
    select: impl Fn(&[LspLocation]) -> Option<Location>,
) -> Result<Option<Location>, LspError> {
    if let Some(feat) = feature {
        check_feature_support(file, feat)?;
    }

    let max_file_size = service.max_file_size_bytes();
    let file = file.to_path_buf();
    let manager = Arc::clone(&service.manager);
    let method = method.to_string();

    service
        .execute_with_retry(&file, |client| {
            let file = file.clone();
            let manager = Arc::clone(&manager);
            let method = method.clone();
            async move {
                ensure_indexed(&client, &file, manager.root()).await;
                client.ensure_cross_file_ready().await;

                let content = read_file_validated(&file, max_file_size).await?;
                let uri = path_to_uri(&file);
                client.sync_document(&uri, &content).await?;

                let params = TextDocumentPositionParams {
                    text_document: TextDocumentIdentifier::new(&uri),
                    position: to_lsp_position(line, column),
                };

                let result: serde_json::Value = client
                    .request(&method, Some(serde_json::to_value(params)?))
                    .await?;

                Ok(parse_location_response(&result))
            }
        })
        .await
        .map(|locs| locs.and_then(|l| select(&l)))
}

pub(super) async fn find_implementations(
    service: &DefaultLspService,
    file: &Path,
    line: u32,
    column: u32,
) -> Result<Vec<Location>, LspError> {
    check_feature_support(file, LspFeature::FindImplementations)?;

    let max_file_size = service.max_file_size_bytes();
    let file = file.to_path_buf();
    let manager = Arc::clone(&service.manager);

    service
        .execute_with_retry(&file, |client| {
            let file = file.clone();
            let manager = Arc::clone(&manager);
            async move {
                ensure_indexed(&client, &file, manager.root()).await;
                client.ensure_cross_file_ready().await;

                let content = read_file_validated(&file, max_file_size).await?;
                let uri = path_to_uri(&file);
                client.sync_document(&uri, &content).await?;

                let params = TextDocumentPositionParams {
                    text_document: TextDocumentIdentifier::new(&uri),
                    position: to_lsp_position(line, column),
                };

                let result: serde_json::Value = client
                    .request(
                        "textDocument/implementation",
                        Some(serde_json::to_value(params)?),
                    )
                    .await?;

                Ok(parse_location_response(&result)
                    .map(|locs| locs.iter().map(convert_location).collect())
                    .unwrap_or_default())
            }
        })
        .await
}
