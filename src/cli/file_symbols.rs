use std::path::Path;

use serde::Serialize;

use crate::app::App;
use crate::error::LspError;
use crate::models::lsp::FindSymbolsOptions;
use crate::models::symbol::{Language, Symbol};
use crate::services::store::SymbolExtractor;

/// What produced a file's declarations — the same word `search` uses for
/// the producer of a row, over the two producers a file-scoped answer has.
///
/// The two sources answer the same question at different fidelity, and which
/// one answered decides what the response can be asked for next: a document
/// tree nests and carries the server's own kinds, an AST read is flat and
/// carries containment in `name_path` alone.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum SymbolBackend {
    /// The language server's document-symbol answer.
    Document,
    /// The compiled-in tree-sitter grammar, read from the file's bytes.
    Ast,
}

/// A file's declarations, and what read them.
pub struct FileSymbols {
    pub symbols: Vec<Symbol>,
    pub backend: SymbolBackend,
}

/// The declarations a file makes.
///
/// The language server answers when it can; its answer nests and carries the
/// kinds the server itself assigns. When it cannot — not installed, not
/// serving, or the project it needs is not built — the grammar compiled into
/// this binary reads the same file. That is a weaker answer, not a synthesized
/// one: the same `Symbol` fields, filled from the bytes on disk, with the
/// backend stated so a caller never mistakes one for the other.
///
/// A language with neither surfaces the server's own error, because then
/// nothing read the file and there is nothing to disclose but why.
pub async fn declared_in(
    app: &App,
    file: &Path,
    options: FindSymbolsOptions,
) -> Result<FileSymbols, LspError> {
    let error = match app.lsp.find_symbols(file, options.clone()).await {
        Ok(mut symbols) => {
            Symbol::compute_paths_for_all(&mut symbols);
            return Ok(FileSymbols {
                symbols,
                backend: SymbolBackend::Document,
            });
        }
        Err(e) => e,
    };

    let language = Language::from_path(file);
    if !SymbolExtractor::is_supported(language) {
        return Err(error);
    }

    let max_bytes = u64::from(app.config().search.max_file_size_mb) * 1024 * 1024;
    let content = match tokio::fs::metadata(file).await {
        Ok(meta) if meta.len() > max_bytes => return Err(error),
        Ok(_) => match tokio::fs::read_to_string(file).await {
            Ok(content) => content,
            Err(_) => return Err(error),
        },
        Err(_) => return Err(error),
    };

    let mut symbols = SymbolExtractor::shared().extract(file, &content, language);
    if options.include_body {
        Symbol::attach_bodies(&mut symbols, &content);
    }
    Ok(FileSymbols {
        symbols,
        backend: SymbolBackend::Ast,
    })
}
