use std::path::Path;
use std::sync::Arc;

use crate::error::LspError;
use crate::infra::lsp::LspFeature;
use crate::infra::lsp::protocol::{
    LspLocation, TextDocumentIdentifier, TextDocumentPositionParams,
};
use crate::models::lsp::{Indexed, path_to_uri};
use crate::models::symbol::Location;

use super::converters::*;
use super::helpers::*;
use super::position::PositionConverter;
use super::service::{DefaultLspService, degradation_of, ensure_indexed};

pub(super) async fn find_references(
    service: &DefaultLspService,
    file: &Path,
    line: u32,
    column: u32,
) -> Result<Indexed<Vec<Location>>, LspError> {
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
                client.sleep_for_cross_file_settle().await;
                // Snapshot the indexing state the query is issued under —
                // the marker derives from this, never from a re-read after
                // the result lands (quiescence racing the request must not
                // strip the marker from a lower-bound answer).
                let ran_under = degradation_of(client.indexing_state());

                let content = read_file_validated(&file, max_file_size).await?;
                let uri = path_to_uri(&file);
                client.sync_document(&uri, &content).await?;

                let params = serde_json::json!({
                    "textDocument": { "uri": uri },
                    "position": to_lsp_position(line, column, &content, client.position_encoding().await),
                    "context": { "includeDeclaration": true }
                });

                let result: serde_json::Value = client
                    .request("textDocument/references", Some(params))
                    .await?;

                if result.is_null() {
                    return Ok(Indexed::new(Vec::new(), ran_under));
                }

                let locations: Vec<LspLocation> = serde_json::from_value(result)
                    .map_err(|e| LspError::Protocol(e.to_string()))?;

                let mut conv = PositionConverter::new(client.position_encoding().await)
                    .with_content(&file, &content);
                let all_locations: Vec<Location> =
                    locations.iter().map(|l| convert_location(l, &mut conv)).collect();

                Ok(Indexed::new(
                    filter_locations_within_project(all_locations, &project_root),
                    ran_under,
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
    let project_root = service.manager.root().to_path_buf();
    goto_location(
        service,
        file,
        line,
        column,
        "textDocument/definition",
        None,
        |locs| select_best_definition(locs, &project_root).cloned(),
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
        |locs| locs.first().cloned(),
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
    select: impl Fn(&[LspLocation]) -> Option<LspLocation>,
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
            let select = &select;
            async move {
                ensure_indexed(&client, &file, manager.root()).await;
                client.sleep_for_cross_file_settle().await;

                let content = read_file_validated(&file, max_file_size).await?;
                let uri = path_to_uri(&file);
                client.sync_document(&uri, &content).await?;

                let params = TextDocumentPositionParams {
                    text_document: TextDocumentIdentifier::new(&uri),
                    position: to_lsp_position(
                        line,
                        column,
                        &content,
                        client.position_encoding().await,
                    ),
                };

                let result: serde_json::Value = client
                    .request(&method, Some(serde_json::to_value(params)?))
                    .await?;

                // The chosen target is decoded from the wire encoding here,
                // where the client (and so the encoding) is in scope.
                let chosen = parse_location_response(&result).and_then(|locs| select(&locs));
                match chosen {
                    Some(loc) => {
                        let mut conv = PositionConverter::new(client.position_encoding().await);
                        Ok(Some(convert_location(&loc, &mut conv)))
                    }
                    None => Ok(None),
                }
            }
        })
        .await
}

pub(super) async fn find_implementations(
    service: &DefaultLspService,
    file: &Path,
    line: u32,
    column: u32,
) -> Result<Indexed<Vec<Location>>, LspError> {
    let max_file_size = service.max_file_size_bytes();
    let file = file.to_path_buf();
    let manager = Arc::clone(&service.manager);

    service
        .execute_with_retry(&file, |client| {
            let file = file.clone();
            let manager = Arc::clone(&manager);
            async move {
                check_feature_support_live(&client, &file, LspFeature::FindImplementations).await?;
                ensure_indexed(&client, &file, manager.root()).await;
                client.sleep_for_cross_file_settle().await;
                let ran_under = degradation_of(client.indexing_state());

                let content = read_file_validated(&file, max_file_size).await?;
                let uri = path_to_uri(&file);
                client.sync_document(&uri, &content).await?;

                let params = TextDocumentPositionParams {
                    text_document: TextDocumentIdentifier::new(&uri),
                    position: to_lsp_position(
                        line,
                        column,
                        &content,
                        client.position_encoding().await,
                    ),
                };

                let result: serde_json::Value = client
                    .request(
                        "textDocument/implementation",
                        Some(serde_json::to_value(params)?),
                    )
                    .await?;

                let locs = parse_location_response(&result).unwrap_or_default();
                // Post-flight null-gap: an empty implementation result from a
                // server that doesn't advertise the provider is unsupported, not
                // "no implementations".
                if locs.is_empty()
                    && !client
                        .feature_advertised(LspFeature::FindImplementations)
                        .await
                {
                    return Err(unsupported_error(&file, LspFeature::FindImplementations));
                }

                let mut conv = PositionConverter::new(client.position_encoding().await)
                    .with_content(&file, &content);
                Ok(Indexed::new(
                    locs.iter()
                        .map(|l| convert_location(l, &mut conv))
                        .collect(),
                    ran_under,
                ))
            }
        })
        .await
}
