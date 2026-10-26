use std::path::Path;

use serde::Serialize;

use crate::app::App;
use crate::cli::utils::{SymbolResolution, column_addressed_symbol, placed_at};
use crate::error::LspError;
use crate::models::lsp::FindSymbolsOptions;
use crate::models::symbol::{Language, Symbol};
use crate::services::symbols::SymbolExtractor;

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

    read_by_grammar(app, file, &options).await.ok_or(error)
}

/// A file's declarations ([`declared_in`]), read by the grammar where the
/// language server's tree holds nothing `addresses` finds and the grammar's
/// reading of the same file does.
///
/// A server's tree can lack a declaration the grammar reads, and with it the
/// index and `--deterministic`: the Dart analyzer outlines no primary
/// constructor of an extension type or an enum, nor the fields their
/// parameters declare, and lists an extension type as a namespace, which
/// qualifies no member's path (`E/twice` is `twice` there). A request naming
/// one by its path would otherwise be answered as absent. The answer carries
/// the backend that produced it.
pub async fn declared_addressing(
    app: &App,
    file: &Path,
    options: FindSymbolsOptions,
    addresses: impl Fn(&[Symbol]) -> bool,
) -> Result<FileSymbols, LspError> {
    let answer = declared_in(app, file, options.clone()).await?;
    if answer.backend == SymbolBackend::Ast || addresses(&answer.symbols) {
        return Ok(answer);
    }
    Ok(match read_by_grammar(app, file, &options).await {
        Some(read) if addresses(&read.symbols) => read,
        _ => answer,
    })
}

/// A file's declarations ([`declared_in`]), read by the grammar where it
/// places a declaration's name at a position that the language server's tree
/// places none at, and reads as no symbol or as another declaration.
///
/// A position where a name is placed addresses that declaration. The
/// analyzer outlines no primary constructor of a Dart extension type or
/// enum, and the grammar places one where the type's name ends, which the
/// server's tree reads as the type itself: an edit there would address the
/// whole type. A symbol of the server's names the grammar's declaration
/// where the two names begin or end alike, as
/// [`SymbolExtractor::complete_from_declarations`] joins them, so a C++
/// definition the grammar places at its qualifier (`Widget::set`) stays the
/// server's.
pub async fn declared_at(
    app: &App,
    file: &Path,
    options: FindSymbolsOptions,
    line: u32,
    column: u32,
) -> Result<FileSymbols, LspError> {
    let answer = declared_in(app, file, options.clone()).await?;
    if answer.backend == SymbolBackend::Ast || placed_at(&answer.symbols, line, column).is_some() {
        return Ok(answer);
    }
    let Some(read) = read_by_grammar(app, file, &options).await else {
        return Ok(answer);
    };
    let Some(placed) = placed_at(&read.symbols, line, column) else {
        return Ok(answer);
    };
    Ok(
        match column_addressed_symbol(&answer.symbols, line, column) {
            SymbolResolution::Match(served) if names_alike(served, placed) => answer,
            _ => read,
        },
    )
}

/// Whether two readings' symbols name one declaration: their names begin,
/// or end, at one position.
fn names_alike(one: &Symbol, other: &Symbol) -> bool {
    let start = |symbol: &Symbol| (symbol.location.line, symbol.location.column);
    let end = |symbol: &Symbol| {
        symbol
            .location
            .name_end_line
            .zip(symbol.location.name_end_column)
    };
    start(one) == start(other) || end(one).is_some() && end(one) == end(other)
}

/// The declarations the grammar compiled into this binary reads in a file:
/// `None` where it has no grammar for the file's language, or the file is
/// over the size limit or unreadable.
pub async fn read_by_grammar(
    app: &App,
    file: &Path,
    options: &FindSymbolsOptions,
) -> Option<FileSymbols> {
    let language = Language::from_path(file);
    if !SymbolExtractor::is_supported(language) {
        return None;
    }
    let max_bytes = app.config().search.max_file_size_bytes();
    if tokio::fs::metadata(file).await.ok()?.len() > max_bytes {
        return None;
    }
    let content = tokio::fs::read_to_string(file).await.ok()?;
    let mut symbols = SymbolExtractor::shared().extract_with_signatures(file, &content, language);
    if options.include_body {
        Symbol::attach_bodies(&mut symbols, &content);
    }
    Some(FileSymbols {
        symbols,
        backend: SymbolBackend::Ast,
    })
}
