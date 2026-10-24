//! What an edit addresses: the project file, and the declaration a symbol
//! path or a position names in it.

use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};

use crate::app::App;
use crate::cli::ParsedLocation;
use crate::cli::declared_in;
use crate::cli::response::LineRange;
use crate::cli::utils::{declared_with, shell_arg};
use crate::models::lsp::FindSymbolsOptions;
use crate::models::symbol::Symbol;

use super::stale_revision;

fn symbol_not_found(pattern: &str, file: &str) -> anyhow::Error {
    anyhow::Error::new(
        crate::cli::OutputError::not_found(format!("Symbol not found: {pattern} in {file}"))
            .with_hint(format!(
                "List symbol paths with `symora symbols -- {}`",
                shell_arg(file)
            )),
    )
}

/// Candidates rendered inline in the ambiguity hint before `+N more`
/// takes over — the message always carries the true total, and the full
/// set stays one `symora symbols <file>` call away.
const AMBIGUOUS_CANDIDATE_DISPLAY_LIMIT: usize = 5;

/// A `--symbol` pattern matching several symbols — a bare name or wildcard
/// hitting siblings, a class and its constructors, or same-named parents
/// sharing a child path — is an under-specified target the agent fixes by
/// re-addressing. Each candidate is listed at its name, a `file:line:col`
/// target that addresses it alone.
fn ambiguous_symbol_path(pattern: &str, candidates: &[&Symbol], file: &str) -> anyhow::Error {
    let mut listing = candidates
        .iter()
        .take(AMBIGUOUS_CANDIDATE_DISPLAY_LIMIT)
        .map(|s| {
            format!(
                "{} ({}) at {file}:{}:{}",
                s.path(),
                s.kind,
                s.location.line,
                s.location.column
            )
        })
        .collect::<Vec<_>>()
        .join(", ");
    if candidates.len() > AMBIGUOUS_CANDIDATE_DISPLAY_LIMIT {
        listing.push_str(&format!(
            ", +{} more",
            candidates.len() - AMBIGUOUS_CANDIDATE_DISPLAY_LIMIT
        ));
    }
    anyhow::Error::new(
        crate::cli::OutputError::invalid(format!(
            "Symbol path '{pattern}' matches {} symbols in {file}",
            candidates.len()
        ))
        .with_hint(format!(
            "Candidates: {listing}. Target one by its location instead."
        )),
    )
}

fn conflicting_addressing(target: &str) -> anyhow::Error {
    anyhow::Error::new(
        crate::cli::OutputError::invalid(format!(
            "Target '{target}' is a location and --symbol was also given; pass exactly one"
        ))
        .with_hint("Use file:line[:col] alone, or a plain file path with --symbol"),
    )
}

/// Resolve file path from target string, handling relative paths.
/// Canonicalizes the result and validates it is within the project root.
pub(crate) fn resolve_file_path(app: &App, target: impl AsRef<Path>) -> Result<PathBuf> {
    let path = target.as_ref();
    let resolved = if path.is_absolute() {
        path.to_path_buf()
    } else {
        app.root().join(path)
    };
    // Validate existence before canonicalizing: a missing path makes
    // `canonicalize` fail with an opaque io error that reads as an internal
    // fault, when it is really a `not_found` the caller can act on.
    if !resolved.is_file() {
        anyhow::bail!(crate::cli::CliInputError::FileNotFound(resolved));
    }
    let canonical = resolved
        .canonicalize()
        .with_context(|| format!("Cannot resolve path: {}", resolved.display()))?;
    let root = app
        .root()
        .canonicalize()
        .unwrap_or_else(|_| app.root().to_path_buf());
    if !canonical.starts_with(&root) {
        anyhow::bail!(crate::cli::CliInputError::PathOutsideProject(canonical));
    }
    Ok(canonical)
}

/// Resolve a `--symbol` path pattern in a file to the one symbol it names,
/// through the same `matches_path` predicate every `--symbol` surface
/// resolves through (bare last-component, `/`-anchored suffix, `*` wildcard,
/// or a leading-`/` exact path). The async wrapper fetches the file's
/// symbols; `unique_symbol_by_path` owns the dispatch, so the destructive
/// resolution stays unit-tested without an LSP round-trip.
async fn find_symbol_by_path(app: &App, file: PathBuf, pattern: &str) -> Result<Addressed> {
    let expected_hash = crate::infra::hash_content(&fs::read_to_string(&file)?);
    let symbols = declared_in(app, &file, FindSymbolsOptions::default().with_depth(10))
        .await?
        .symbols;
    let symbol = unique_symbol_by_path(&symbols, pattern, &app.output.relative_path(&file))?;
    Ok(Addressed::among(file, symbol, &symbols, expected_hash))
}

/// Pick the one symbol a flexible `--symbol` pattern names: exactly one
/// match resolves; zero is a structured not-found; several is a structured
/// ambiguity listing the candidates — silently editing an arbitrary match
/// would be plausible-but-wrong. Pure (paths pre-computed by the caller),
/// so the destructive dispatch is directly testable.
fn unique_symbol_by_path(symbols: &[Symbol], pattern: &str, file_display: &str) -> Result<Symbol> {
    match Symbol::filter_by_path(symbols, pattern).as_slice() {
        [] => Err(symbol_not_found(pattern, file_display)),
        [only] => Ok((*only).clone()),
        many => Err(ambiguous_symbol_path(pattern, many, file_display)),
    }
}

/// Find the symbol an edit target addresses, through the shared
/// resolution functions every surface uses (`cli::utils::symbol_nav`).
/// With a column, resolution is position-precise — the position must be on
/// a symbol's declaration, never merely inside one; without a column, the
/// line's own declaration is preferred over the enclosing block
/// (`line_addressed_symbol`). Ambiguity and misses become structured
/// errors the agent can act on — guessing would edit a sibling or an
/// enclosing block the caller didn't address.
async fn find_symbol_at_location(
    app: &App,
    file: PathBuf,
    line: u32,
    column: Option<u32>,
) -> Result<Addressed> {
    use crate::cli::utils::{SymbolResolution, column_addressed_symbol, line_addressed_symbol};
    let expected_hash = crate::infra::hash_content(&fs::read_to_string(&file)?);

    let symbols = declared_in(app, &file, FindSymbolsOptions::default())
        .await?
        .symbols;

    let resolution = match column {
        Some(col) => column_addressed_symbol(&symbols, line, col),
        None => line_addressed_symbol(&symbols, line),
    };

    let file_display = app.output.relative_path(&file);
    match resolution {
        SymbolResolution::Match(symbol) => Ok(Addressed::among(
            file,
            symbol.clone(),
            &symbols,
            expected_hash,
        )),
        SymbolResolution::NotFound => Err(anyhow::Error::new(match column {
            Some(col) => crate::cli::OutputError::not_found(format!(
                "Column {col} of line {line} in {file_display} is not on a symbol's declaration"
            ))
            .with_hint(format!(
                "Pass the name column of the symbol to edit — `symora symbols -- {}` \
                 lists them; a column-less {file_display}:{line} addresses the symbol declared \
                 on that line, or the one enclosing it",
                shell_arg(&file_display)
            )),
            None => crate::cli::OutputError::not_found(format!(
                "No symbol found at line {line} in {file_display}"
            ))
            .with_hint(format!(
                "List symbol paths with `symora symbols -- {}`",
                shell_arg(&file_display)
            )),
        })),
        SymbolResolution::Ambiguous(declared) => {
            let names: Vec<&str> = declared.iter().map(|s| s.name.as_str()).collect();
            Err(anyhow::Error::new(
                crate::cli::OutputError::invalid(format!(
                    "Line {line} declares multiple symbols ({})",
                    names.join(", "),
                ))
                .with_hint("Pass the exact column of the symbol to edit (file:line:col)"),
            ))
        }
    }
}

/// The symbol an edit addresses, and the names its declaration declares
/// with it ([`declared_with`]), which a whole-line edit of it would remove.
pub(super) struct Addressed {
    pub(super) expected_hash: u64,
    pub(super) file: PathBuf,
    pub(super) symbol: Symbol,
    pub(super) declared_with: Vec<String>,
}

impl Addressed {
    fn among(file: PathBuf, symbol: Symbol, symbols: &[Symbol], expected_hash: u64) -> Self {
        let declared_with = declared_with(symbols, &symbol)
            .into_iter()
            .map(|other| other.name.clone())
            .collect();
        Self {
            expected_hash,
            file,
            symbol,
            declared_with,
        }
    }
}

/// Resolve full symbol from target. Auto-detects location format
/// (`file:line[:col]`) vs file path (requires --symbol); passing both
/// addressing modes at once is refused rather than silently picking one.
pub(super) async fn resolve_symbol(
    app: &App,
    target: &str,
    symbol_path: Option<String>,
) -> Result<Addressed> {
    // Location mode: find symbol at position. An omitted column is its
    // own addressing mode (line-declared symbol first), not column 1.
    if ParsedLocation::is_location_format(target) {
        if symbol_path.is_some() {
            anyhow::bail!(conflicting_addressing(target));
        }
        let loc = ParsedLocation::parse(target)?.to_absolute_with_root(Some(app.root()))?;
        let column = loc.column_explicit.then_some(loc.column);
        return find_symbol_at_location(app, loc.file, loc.line, column).await;
    }

    // Symbol mode: --symbol is required
    let pattern = symbol_path.ok_or_else(|| {
        anyhow::Error::new(
            crate::cli::OutputError::invalid("--symbol is required when target is a file path")
                .with_hint(
                    "Pass file:line[:col] for position addressing, or a file path plus --symbol",
                ),
        )
    })?;

    let file = resolve_file_path(app, target)?;
    find_symbol_by_path(app, file, &pattern).await
}

/// The symbol's full declaration span (1-indexed, inclusive), with the
/// stale-range guard: an LSP range past EOF means the server analyzed an
/// older revision, and splicing by it would corrupt the file.
pub(super) fn symbol_line_span(symbol: &Symbol, total_lines: usize) -> Result<LineRange> {
    let (start, _) = symbol.location.effective_start();
    let start = start.max(1);
    let end = symbol
        .location
        .end_line
        .unwrap_or(symbol.location.line)
        .max(start);
    // `> total_lines`, not `total_lines.max(1)`: an emptied file has 0 lines, so
    // a stale line-1 anchor (end == 1 > 0) must fail closed here rather than
    // reach the splice and surface as a misleading out-of-range error. A symbol
    // can never legitimately resolve into a 0-line file.
    if (end as usize) > total_lines {
        anyhow::bail!(stale_revision(format!(
            "Symbol end line {end} exceeds file length {total_lines}; LSP range is stale, retry"
        )));
    }
    Ok(LineRange { start, end })
}

#[cfg(test)]
mod tests {
    use super::super::test_support::sample_symbol;
    use super::*;
    use crate::cli::utils::{
        SymbolResolution, column_addressed_symbol, line_addressed_symbol, symbols_declared_on_line,
    };
    use crate::models::symbol::{Location, SymbolKind};

    /// A method inside an impl/class, addressed line-only: the symbol
    /// DECLARED on the line wins over the enclosing block whose range
    /// also contains it.
    #[test]
    fn line_addressing_prefers_the_symbol_declared_on_the_line() {
        let symbols = impl_with_method();
        match line_addressed_symbol(&symbols, 26) {
            SymbolResolution::Match(symbol) => assert_eq!(symbol.name, "new"),
            _ => panic!("expected the method declared on line 26"),
        }
    }

    /// A body line (no declaration) still resolves to the innermost
    /// enclosing symbol.
    #[test]
    fn line_addressing_falls_back_to_the_enclosing_symbol_inside_a_body() {
        let symbols = impl_with_method();
        match line_addressed_symbol(&symbols, 27) {
            SymbolResolution::Match(symbol) => assert_eq!(symbol.name, "new"),
            _ => panic!("expected the enclosing method for a body line"),
        }
        match line_addressed_symbol(&symbols, 31) {
            SymbolResolution::Match(symbol) => assert_eq!(symbol.name, "Rect"),
            _ => panic!("expected the enclosing impl for a line between methods"),
        }
    }

    #[test]
    fn line_addressing_reports_multiple_declarations_as_ambiguous() {
        let symbols = vec![
            Symbol::new(
                "A".to_string(),
                SymbolKind::Constant,
                Location::full(PathBuf::from("/tmp/foo.rs"), 5, 7, 5, 1, 5, 18),
            ),
            Symbol::new(
                "B".to_string(),
                SymbolKind::Constant,
                Location::full(PathBuf::from("/tmp/foo.rs"), 5, 26, 5, 20, 5, 37),
            ),
        ];
        match line_addressed_symbol(&symbols, 5) {
            SymbolResolution::Ambiguous(declared) => assert_eq!(declared.len(), 2),
            _ => panic!("two declarations on one line must stay ambiguous"),
        }
    }

    /// `file:line:col` is position-precise: the column must be on a symbol's
    /// declaration. The indent before a method and a line inside its body
    /// address nothing — resolving either to the enclosing impl would let a
    /// destructive edit land on a block the caller never named.
    #[test]
    fn column_addressing_keeps_position_precise() {
        let symbols = impl_with_method();
        match column_addressed_symbol(&symbols, 26, 12) {
            SymbolResolution::Match(symbol) => assert_eq!(symbol.name, "new"),
            _ => panic!("the method name column must address the method"),
        }
        match column_addressed_symbol(&symbols, 24, 1) {
            SymbolResolution::Match(symbol) => assert_eq!(symbol.name, "Rect"),
            _ => panic!("the impl's own declaration start addresses the impl"),
        }
        assert!(matches!(
            column_addressed_symbol(&symbols, 26, 1),
            SymbolResolution::NotFound
        ));
        assert!(matches!(
            column_addressed_symbol(&symbols, 27, 9),
            SymbolResolution::NotFound
        ));
    }

    /// `impl Rect` spanning lines 24..=32 with `pub fn new` declared on
    /// line 26 (name at column 12), body through line 29.
    fn impl_with_method() -> Vec<Symbol> {
        let method = Symbol::new(
            "new".to_string(),
            SymbolKind::Function,
            Location::full(PathBuf::from("/tmp/foo.rs"), 26, 12, 26, 5, 29, 6),
        );
        vec![
            Symbol::new(
                "Rect".to_string(),
                SymbolKind::Struct,
                Location::full(PathBuf::from("/tmp/foo.rs"), 24, 6, 24, 1, 32, 2),
            )
            .with_children(vec![method]),
        ]
    }

    fn path_candidate(line: u32) -> Symbol {
        Symbol::new(
            "bar".to_string(),
            SymbolKind::Method,
            Location::point(PathBuf::from("/tmp/a.rs"), line, 5),
        )
    }

    /// A pattern matching several symbols is an under-specified target the
    /// agent fixes by re-addressing — same code as the multi-symbol-line
    /// case, with the candidates' lines as the honest disambiguator.
    #[test]
    fn ambiguous_symbol_path_is_invalid_argument_with_candidates() {
        use crate::cli::{ErrorCode, OutputError};
        let first = path_candidate(10);
        let second = path_candidate(40);
        let out: OutputError = ambiguous_symbol_path("Foo/bar", &[&first, &second], "a.rs").into();
        assert!(matches!(out.code, ErrorCode::InvalidArgument));
        assert!(out.message.contains("matches 2 symbols"));
        let hint = out.hint.unwrap();
        assert!(hint.contains("a.rs:10:5") && hint.contains("a.rs:40:5"));
    }

    #[test]
    fn ambiguous_symbol_path_hint_caps_candidates() {
        use crate::cli::OutputError;
        let symbols: Vec<Symbol> = (1..=7).map(|i| path_candidate(i * 10)).collect();
        let candidates: Vec<&Symbol> = symbols.iter().collect();
        let out: OutputError = ambiguous_symbol_path("Foo/bar", &candidates, "a.rs").into();
        assert!(out.message.contains("matches 7 symbols"));
        let hint = out.hint.unwrap();
        assert!(hint.contains("+2 more"));
        assert!(hint.contains("a.rs:50:5") && !hint.contains("a.rs:60:5"));
    }

    #[test]
    fn symbol_not_found_is_structured() {
        use crate::cli::{ErrorCode, OutputError};
        let out: OutputError = symbol_not_found("Foo/bar", "src/a.rs").into();
        assert!(matches!(out.code, ErrorCode::NotFound));
        assert!(out.message.contains("Foo/bar"));
        assert!(out.hint.unwrap().contains("symora symbols"));
    }

    /// Two structs in a file, each with an `area` method. Build the symbol
    /// set both the unique and the ambiguous `--symbol` dispatch resolve
    /// against (paths pre-computed, as the live caller does).
    fn two_structs_with_area() -> Vec<Symbol> {
        let make = |name: &str, line: u32| {
            Symbol::new(
                name.to_string(),
                SymbolKind::Struct,
                Location::full(PathBuf::from("/tmp/foo.rs"), line, 8, line, 1, line + 3, 2),
            )
            .with_children(vec![Symbol::new(
                "area".to_string(),
                SymbolKind::Function,
                Location::full(
                    PathBuf::from("/tmp/foo.rs"),
                    line + 1,
                    8,
                    line + 1,
                    5,
                    line + 2,
                    6,
                ),
            )])
        };
        let mut symbols = vec![make("Rect", 1), make("Circle", 10)];
        Symbol::compute_paths_for_all(&mut symbols);
        symbols
    }

    /// The destructive `--symbol` dispatch resolves a pattern that names
    /// exactly one symbol — including the bare last-component and `*/method`
    /// wildcard forms the flexible matcher accepts.
    #[test]
    fn unique_symbol_by_path_resolves_a_single_match() {
        let mut symbols = impl_with_method();
        Symbol::compute_paths_for_all(&mut symbols);
        assert_eq!(
            unique_symbol_by_path(&symbols, "new", "foo.rs")
                .unwrap()
                .name,
            "new"
        );
        assert_eq!(
            unique_symbol_by_path(&symbols, "*/new", "foo.rs")
                .unwrap()
                .name,
            "new"
        );
        assert_eq!(
            unique_symbol_by_path(&symbols, "Rect/new", "foo.rs")
                .unwrap()
                .name,
            "new"
        );
    }

    /// A bare name hitting the same method on two distinct parents is an
    /// ambiguity the destructive edit refuses — never a silent arbitrary
    /// pick.
    #[test]
    fn unique_symbol_by_path_refuses_an_ambiguous_pattern() {
        use crate::cli::{ErrorCode, OutputError};
        let symbols = two_structs_with_area();
        let out: OutputError = unique_symbol_by_path(&symbols, "area", "foo.rs")
            .unwrap_err()
            .into();
        assert!(matches!(out.code, ErrorCode::InvalidArgument));
        assert!(out.message.contains("matches 2 symbols"));
    }

    /// A pattern that matches nothing is a structured not-found.
    #[test]
    fn unique_symbol_by_path_reports_a_miss_as_not_found() {
        use crate::cli::{ErrorCode, OutputError};
        let symbols = two_structs_with_area();
        let out: OutputError = unique_symbol_by_path(&symbols, "nonexistent", "foo.rs")
            .unwrap_err()
            .into();
        assert!(matches!(out.code, ErrorCode::NotFound));
    }

    #[test]
    fn conflicting_addressing_is_invalid_argument() {
        use crate::cli::{ErrorCode, OutputError};
        let out: OutputError = conflicting_addressing("src/a.rs:10:1").into();
        assert!(matches!(out.code, ErrorCode::InvalidArgument));
        assert!(out.hint.unwrap().contains("--symbol"));
    }

    /// A symbol on a multi-symbol line resolves only with a matching
    /// column; nothing is guessed on a precise miss.
    #[test]
    fn declared_on_line_finds_same_line_siblings() {
        let a = Symbol::new(
            "alpha".to_string(),
            SymbolKind::Function,
            Location::full(PathBuf::from("/tmp/a.rs"), 1, 4, 1, 1, 1, 20),
        );
        let b = Symbol::new(
            "beta".to_string(),
            SymbolKind::Function,
            Location::full(PathBuf::from("/tmp/a.rs"), 1, 28, 1, 25, 1, 44),
        );
        let symbols = vec![a, b];
        let found = symbols_declared_on_line(&symbols, 1);
        assert_eq!(found.len(), 2);
        assert!(symbols_declared_on_line(&symbols, 2).is_empty());
    }

    #[test]
    fn symbol_line_span_rejects_stale_lsp_data() {
        let sym = sample_symbol(1, 100);
        let err = symbol_line_span(&sym, 5).unwrap_err();
        assert!(err.to_string().contains("exceeds file length"));
    }

    #[test]
    fn symbol_line_span_rejects_a_stale_anchor_into_an_emptied_file() {
        // The file was emptied since the symbol resolved (0 lines): a line-1
        // anchor must fail closed as stale, not slip past `total_lines.max(1)`
        // into the splice where it would surface as a misleading range error.
        let sym = sample_symbol(1, 1);
        let err = symbol_line_span(&sym, 0).unwrap_err();
        assert!(err.to_string().contains("exceeds file length"));
    }

    #[test]
    fn symbol_line_span_includes_full_declaration() {
        let sym = sample_symbol(2, 4);
        let span = symbol_line_span(&sym, 10).unwrap();
        assert_eq!((span.start, span.end), (2, 4));
    }
}
