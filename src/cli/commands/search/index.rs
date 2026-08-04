use anyhow::Result;
use serde::Serialize;

use crate::app::App;
use crate::cli::OutputError;
use crate::models::symbol::Language;
use crate::services::store::IndexOptions;

use super::IndexCommand;

#[derive(Serialize)]
struct IndexStatusOutput {
    file_count: usize,
    symbol_count: usize,
    content_line_count: usize,
    index_size_bytes: u64,
    last_indexed: u64,
    is_indexing: bool,
    /// The languages this index answers authoritatively for — empty until a
    /// build completes. Row counts alone cannot tell a whole index from one
    /// a narrowed build or a per-file refresh left partial, and a symbol
    /// search reads as complete only for the languages listed here.
    languages: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    progress: Option<f32>,
}

#[derive(Serialize)]
struct IndexBuildOutput {
    status: String,
    file_count: usize,
    symbol_count: usize,
    content_line_count: usize,
    index_size_bytes: u64,
}

pub async fn execute_index_command(app: &App, command: IndexCommand) -> Result<()> {
    let ctx = &app.output;

    match command {
        IndexCommand::Build { force, languages } => {
            let languages = languages.map(|s| {
                s.split(',')
                    .map(str::trim)
                    .filter(|l| !l.is_empty())
                    .map(Language::parse_or_default)
                    .collect()
            });
            match app.store.index(IndexOptions { force, languages }).await {
                Ok(stats) => ctx.print_success(IndexBuildOutput {
                    status: "completed".to_string(),
                    file_count: stats.file_count,
                    symbol_count: stats.symbol_count,
                    content_line_count: stats.content_line_count,
                    index_size_bytes: stats.index_size_bytes,
                }),
                Err(e) => ctx.print_error(OutputError::internal(e.to_string())),
            }
        }
        IndexCommand::Status => match app.store.index_status().await {
            Ok(stats) => ctx.print_success(IndexStatusOutput {
                file_count: stats.file_count,
                symbol_count: stats.symbol_count,
                content_line_count: stats.content_line_count,
                index_size_bytes: stats.index_size_bytes,
                last_indexed: stats.last_indexed,
                is_indexing: stats.is_indexing,
                languages: stats
                    .languages
                    .iter()
                    .map(|l| l.lsp_id().to_string())
                    .collect(),
                progress: stats.progress,
            }),
            Err(e) => ctx.print_error(OutputError::internal(e.to_string())),
        },
        IndexCommand::Clear => match app.store.index_clear().await {
            Ok(()) => ctx.print_success(serde_json::json!({ "cleared": true })),
            Err(e) => ctx.print_error(OutputError::internal(e.to_string())),
        },
    }

    Ok(())
}
