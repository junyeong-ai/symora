//! Position-driven navigation over a `documentSymbol` tree.

use crate::models::symbol::{Symbol, SymbolKind};

/// Whether a symbol's declared range covers a position. The one reading of
/// containment every navigator here shares, so two of them can never disagree
/// about which symbols a position is inside.
///
/// - `column = None`: line-only matching.
/// - `column = Some(col)`: line+column range matching.
fn contains_position(symbol: &Symbol, line: u32, column: Option<u32>) -> bool {
    let loc = &symbol.location;
    let (start, start_column) = loc.effective_start();
    let end = loc.end_line.unwrap_or(start);

    match column {
        None => line >= start && line <= end,
        Some(col) => {
            if start == end {
                start == line && col >= start_column && loc.end_column.is_none_or(|ec| col <= ec)
            } else {
                (line > start && line < end)
                    || (line == start && col >= start_column)
                    || (line == end && loc.end_column.is_none_or(|ec| col <= ec))
            }
        }
    }
}

/// Whether a position lies within `[start, name end]` of a symbol's
/// declaration. The name end is the server's stated span; when it stated
/// none, the span runs to the end of the name's line, so a declaration line
/// still addresses its symbol and body lines never do. A file-level symbol
/// stands for the file, not for a position in it, and matches nothing.
fn spans_position(symbol: &Symbol, start: (u32, u32), line: u32, column: u32) -> bool {
    if symbol.kind == SymbolKind::File {
        return false;
    }
    let loc = &symbol.location;
    let end = match (loc.name_end_line, loc.name_end_column) {
        (Some(l), Some(c)) => (l, c),
        _ => {
            let line_end = match loc.end_line {
                Some(l) if l == loc.line => loc.end_column.unwrap_or(u32::MAX),
                _ => u32::MAX,
            };
            (loc.line, line_end)
        }
    };
    (line, column) >= start && (line, column) <= end
}

/// Whether a position is on a symbol's name — the one place a position
/// means that symbol and nothing else, whatever language it is written in.
fn names_position(symbol: &Symbol, line: u32, column: u32) -> bool {
    let loc = &symbol.location;
    spans_position(symbol, (loc.line, loc.column), line, column)
}

/// Whether a position is on a symbol's declaration header: the name's line
/// from where the declaration begins there through the end of the name. This
/// is what a position addresses when it is on no token of its own — the
/// keyword or visibility before a name, the whitespace between them. Tokens
/// there that are something else (a return type, a receiver, an attribute on
/// the same line) are read through the language server first, so this is
/// the reading of last resort for a declaration line, never of the body it
/// introduces or of the attribute lines above it.
fn declares_position(symbol: &Symbol, line: u32, column: u32) -> bool {
    let loc = &symbol.location;
    spans_position(
        symbol,
        loc.effective_start().max((loc.line, 1)),
        line,
        column,
    )
}

/// Find the innermost symbol at a position (recursive search).
pub fn find_symbol_at_position(
    symbols: &[Symbol],
    line: u32,
    column: Option<u32>,
) -> Option<&Symbol> {
    fn search(symbols: &[Symbol], line: u32, column: Option<u32>) -> Option<&Symbol> {
        for symbol in symbols {
            if !contains_position(symbol, line, column) {
                continue;
            }
            if let Some(child) = search(&symbol.children, line, column) {
                return Some(child);
            }
            return Some(symbol);
        }
        None
    }
    search(symbols, line, column)
}

/// The innermost symbol whose name a position is on — see
/// [`names_position`].
pub fn find_named_at_position(symbols: &[Symbol], line: u32, column: u32) -> Option<&Symbol> {
    innermost(symbols, line, column, names_position)
}

/// The innermost symbol whose declaration header a position is on — see
/// [`declares_position`]. A position inside a body, on a usage, or on
/// nothing addresses no symbol here, whatever encloses it.
fn find_declaration_at_position(symbols: &[Symbol], line: u32, column: u32) -> Option<&Symbol> {
    innermost(symbols, line, column, declares_position)
}

fn innermost(
    symbols: &[Symbol],
    line: u32,
    column: u32,
    spans: fn(&Symbol, u32, u32) -> bool,
) -> Option<&Symbol> {
    for symbol in symbols {
        if !contains_position(symbol, line, Some(column)) {
            continue;
        }
        if let Some(child) = innermost(&symbol.children, line, column, spans) {
            return Some(child);
        }
        if spans(symbol, line, column) {
            return Some(symbol);
        }
    }
    None
}

/// The callable that owns a position: the innermost function, method, or
/// constructor whose range contains it.
///
/// [`find_symbol_at_position`] answers "what is declared here", which for a
/// position inside a body is the binding it landed in — a `const` on the
/// statement, a parameter, a field. That is the right answer for addressing a
/// symbol and the wrong one for naming the code a position *runs in*: a test
/// runner, a caller, and a reader all name the callable, never the local it
/// happens to sit next to. Kind decides, so the answer holds for any language
/// the server describes; a name is required because the answer is an identity
/// and an anonymous frame cannot serve as one — the search continues outward
/// past it rather than reporting a blank.
pub fn enclosing_callable(symbols: &[Symbol], line: u32, column: Option<u32>) -> Option<&Symbol> {
    fn search<'a>(
        symbols: &'a [Symbol],
        line: u32,
        column: Option<u32>,
        found: Option<&'a Symbol>,
    ) -> Option<&'a Symbol> {
        for symbol in symbols {
            if !contains_position(symbol, line, column) {
                continue;
            }
            let found = if symbol.kind.is_callable() && !symbol.name.trim().is_empty() {
                Some(symbol)
            } else {
                found
            };
            return search(&symbol.children, line, column, found);
        }
        found
    }
    search(symbols, line, column, None)
}

/// How a target position resolved against the symbol tree. Both
/// addressing modes (`file:line:col` and column-less `file:line`) produce
/// one of these; what differs per surface is only the *handling* of
/// `Ambiguous` — the edit path refuses (a guessed write is destructive),
/// the navigation path picks the first declaration and disclosing hints
/// name the alternatives (erroring on the ambiguity helps nobody, a silent
/// guess violates invariant 4).
pub enum SymbolResolution<'a> {
    Match(&'a Symbol),
    /// Several symbols are declared on the target line and the input
    /// doesn't single one out. Candidates are ordered by declaration
    /// column, so `[0]` is the line's first declaration.
    Ambiguous(Vec<&'a Symbol>),
    NotFound,
}

/// What a resolved anchor is. The states license different claims and are
/// kept distinct for that reason: `Resolved` and `Binding` are anchored at a
/// declaration — the reference set is exact — and differ only in whether the
/// symbol tree lists it; `NotASymbol` was checked and denotes nothing;
/// `Unavailable` could not be checked (a read failed), so neither "is" nor
/// "is not a symbol" may be claimed. Collapsing the last two would let a mere
/// read failure be reported as a false "not a symbol".
///
/// This is the single source of the anchor-resolution disclosure: every surface
/// (callers/callees/implementations/refs/impact/context) renders it through
/// [`as_status`](AnchorResolution::as_status) so the JSON marker is one
/// consistent `*_status` string, never a per-surface bool/shape.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AnchorResolution {
    /// Anchored at a listed symbol's declaration (possibly after
    /// disambiguating a multi-declaration line, or after resolving a usage
    /// through its definition).
    Resolved,
    /// Anchored at a declaration the symbol tree does not list — a local, a
    /// parameter, a generated item — reached through the position's
    /// definition. The answer is that binding's; the target cannot be named
    /// from the tree.
    Binding,
    /// Symbols were read but none is addressed by this position, and the
    /// language server resolves nothing there.
    NotASymbol,
    /// A read failed, so the position could not be resolved; whether it is a
    /// symbol is unknown.
    Unavailable,
}

impl AnchorResolution {
    /// Whether the input resolved to a listed symbol.
    pub fn is_resolved(self) -> bool {
        matches!(self, AnchorResolution::Resolved)
    }

    /// Whether the anchor position is a declaration — a reference reported
    /// there is the declaration itself, not a usage.
    pub fn is_declaration(self) -> bool {
        matches!(self, AnchorResolution::Resolved | AnchorResolution::Binding)
    }

    /// The public disclosure marker for an anchor that is not a listed symbol,
    /// or `None` when resolved (the field is then omitted). One shared
    /// vocabulary across every surface: `"binding"`, `"not_a_symbol"`,
    /// `"unavailable"` — never collapsed to a bool.
    pub fn as_status(self) -> Option<&'static str> {
        match self {
            AnchorResolution::Resolved => None,
            AnchorResolution::Binding => Some("binding"),
            AnchorResolution::NotASymbol => Some("not_a_symbol"),
            AnchorResolution::Unavailable => Some("unavailable"),
        }
    }
}

/// The navigation disclosure for a multi-declaration line resolved to its first
/// declaration: names the alternatives and how to target another. The single
/// home for this wording, so every surface says it the same way. `declared` is
/// ordered by declaration column, so `[0]` is the chosen first declaration.
pub fn ambiguity_hint(line: u32, declared: &[&Symbol]) -> String {
    let names: Vec<&str> = declared.iter().map(|s| s.name.as_str()).collect();
    let first = declared
        .first()
        .map(|s| s.name.as_str())
        .unwrap_or_default();
    format!(
        "Line {line} declares multiple symbols ({}); resolved to '{first}' — pass an explicit \
         column (file:line:column) to target another",
        names.join(", "),
    )
}

/// Resolution for a column-addressed target (`file:line:col`) against the
/// symbol tree alone: the symbol whose declaration header the exact position
/// is on. A column is a precise address, so a position inside a body
/// resolves to no declaration rather than to the symbol that happens to
/// enclose it. A surface that can ask the language server what a token
/// denotes does so before falling back to this (see `cli::analysis`); one
/// that addresses declarations in the tree — an edit — uses this directly.
pub fn column_addressed_symbol(symbols: &[Symbol], line: u32, column: u32) -> SymbolResolution<'_> {
    match find_declaration_at_position(symbols, line, column) {
        Some(symbol) => SymbolResolution::Match(symbol),
        None => SymbolResolution::NotFound,
    }
}

/// Resolution for a line-addressed target (`file:line`, no column): a
/// symbol DECLARED on the line wins over any enclosing block, so naming a
/// method's declaration line targets the method — never the impl/class
/// that happens to span it. Range matching only decides when the line
/// declares nothing (a body line), where the enclosing symbol is the one
/// honest reading. This is the single line-addressing rule for every
/// surface — edits and navigation resolve the same input to the same
/// symbol.
pub fn line_addressed_symbol(symbols: &[Symbol], line: u32) -> SymbolResolution<'_> {
    declared_or_enclosing(symbols, line)
}

fn declared_or_enclosing(symbols: &[Symbol], line: u32) -> SymbolResolution<'_> {
    let mut declared = symbols_declared_on_line(symbols, line);
    declared.sort_by_key(|s| s.location.column);
    match declared.as_slice() {
        [] => match find_symbol_at_position(symbols, line, None) {
            Some(symbol) => SymbolResolution::Match(symbol),
            None => SymbolResolution::NotFound,
        },
        [only] => SymbolResolution::Match(only),
        _ => SymbolResolution::Ambiguous(declared),
    }
}

/// Symbols whose declaration (name position) sits on `line`, innermost
/// scope included.
pub fn symbols_declared_on_line(symbols: &[Symbol], line: u32) -> Vec<&Symbol> {
    let mut found = Vec::new();
    for symbol in symbols {
        if symbol.location.line == line {
            found.push(symbol);
        }
        found.extend(symbols_declared_on_line(&symbol.children, line));
    }
    found
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::models::symbol::Location;

    /// A declaration whose visibility and keyword occupy the columns before
    /// the name (`pub fn name`), as a language server describes it: the range
    /// starts at column 1 of the name's line and the name span is stated.
    fn func(name: &str, line: u32, name_col: u32, end_line: u32) -> Symbol {
        Symbol::new(
            name.to_string(),
            SymbolKind::Function,
            Location::full(
                std::path::PathBuf::from("x.rs"),
                line,
                name_col,
                line,
                1,
                end_line,
                1,
            )
            .with_name_end(line, name_col + name.len() as u32),
        )
    }

    #[test]
    fn column_addressing_resolves_a_declaration_start_column_to_the_name() {
        // The function name is at column 8 (after `fn `), but the declaration
        // starts at column 1 (`pub`) — the column a structural match reports.
        // Column-addressing from column 1 must still land on the name anchor so
        // callers/refs/impact agree with an exact name position.
        let symbols = vec![func("load_config", 41, 8, 50)];
        let SymbolResolution::Match(sym) = column_addressed_symbol(&symbols, 41, 1) else {
            panic!("expected a Match");
        };
        assert_eq!((sym.location.line, sym.location.column), (41, 8));
        assert_eq!(sym.name, "load_config");
    }

    /// A column is a precise address: anywhere on the declaration — its
    /// keyword, its name, the position just past its name (where a caret at
    /// the end of an identifier sits, and where servers still read the
    /// identifier) — is that symbol; a position further along the line, or
    /// on a body line, is not, whatever encloses it. That is what keeps
    /// refs/callers/impact reading a position the way def/hover/rename read
    /// it.
    #[test]
    fn column_addressing_stops_at_the_end_of_the_name() {
        let symbols = vec![func("load_config", 41, 8, 50)];

        for column in [1, 8, 12, 19] {
            let SymbolResolution::Match(sym) = column_addressed_symbol(&symbols, 41, column) else {
                panic!("column {column} is on the declaration");
            };
            assert_eq!(sym.name, "load_config");
        }
        assert!(matches!(
            column_addressed_symbol(&symbols, 41, 25),
            SymbolResolution::NotFound
        ));
        assert!(matches!(
            column_addressed_symbol(&symbols, 45, 12),
            SymbolResolution::NotFound
        ));
    }

    /// The attributes, decorators, and doc comments a declaration's range
    /// begins with are tokens of their own, not the declaration: a column on
    /// them addresses no symbol (the caller reads the token through the
    /// language server), while a column-less line there still belongs to the
    /// declaration whose range it opens.
    #[test]
    fn attribute_lines_are_not_the_declaration_for_a_column_but_are_for_a_line() {
        let symbols = vec![Symbol::new(
            "load_config".to_string(),
            SymbolKind::Function,
            Location::full(std::path::PathBuf::from("x.rs"), 41, 8, 39, 1, 50, 1)
                .with_name_end(41, 19),
        )];

        assert!(matches!(
            column_addressed_symbol(&symbols, 39, 3),
            SymbolResolution::NotFound
        ));
        assert!(matches!(
            column_addressed_symbol(&symbols, 41, 1),
            SymbolResolution::Match(_)
        ));
        match line_addressed_symbol(&symbols, 39) {
            SymbolResolution::Match(sym) => assert_eq!(sym.name, "load_config"),
            _ => panic!("the attribute line opens the declaration's range"),
        }
    }

    /// A position inside a body resolves to no declaration even when the
    /// server lists a local binding there — the binding's own header is what
    /// addresses it, not the statement it introduces.
    #[test]
    fn column_addressing_inside_a_body_addresses_no_symbol() {
        let symbols = vec![nest(
            func("test_places_an_order", 18, 16, 22),
            vec![binding("order", 20, 11)],
        )];

        assert!(matches!(
            column_addressed_symbol(&symbols, 20, 30),
            SymbolResolution::NotFound
        ));
        let SymbolResolution::Match(sym) = column_addressed_symbol(&symbols, 20, 12) else {
            panic!("the binding's own name addresses it");
        };
        assert_eq!(sym.name, "order");
    }

    /// The name span is the one place a position means the symbol without
    /// asking anyone; the header additionally claims the keywords before it.
    /// A file-level symbol stands for the file and claims no position — it is
    /// what an empty symbol tree is padded with, and must not turn every
    /// column of line 1 into a match.
    #[test]
    fn a_name_span_is_narrower_than_a_header_and_a_file_symbol_claims_nothing() {
        let symbols = vec![
            Symbol::new(
                "x.rs".to_string(),
                SymbolKind::File,
                Location::point(std::path::PathBuf::from("x.rs"), 1, 1),
            ),
            func("load_config", 41, 8, 50),
        ];

        assert!(find_named_at_position(&symbols, 41, 1).is_none());
        assert_eq!(
            find_named_at_position(&symbols, 41, 10).map(|s| s.name.as_str()),
            Some("load_config")
        );
        assert_eq!(
            find_declaration_at_position(&symbols, 41, 1).map(|s| s.name.as_str()),
            Some("load_config")
        );
        assert!(find_named_at_position(&symbols, 1, 20).is_none());
        assert!(find_declaration_at_position(&symbols, 1, 20).is_none());
    }

    /// Without a stated name span the header runs to the end of the name's
    /// line: a declaration line still addresses its symbol, body lines never
    /// do.
    #[test]
    fn column_addressing_without_a_name_span_takes_the_declaration_line() {
        let symbols = vec![Symbol::new(
            "load_config".to_string(),
            SymbolKind::Function,
            Location::full(std::path::PathBuf::from("x.rs"), 41, 8, 41, 1, 50, 1),
        )];

        assert!(matches!(
            column_addressed_symbol(&symbols, 41, 40),
            SymbolResolution::Match(_)
        ));
        assert!(matches!(
            column_addressed_symbol(&symbols, 45, 12),
            SymbolResolution::NotFound
        ));
    }

    #[test]
    fn line_addressing_resolves_a_body_line_to_the_enclosing_symbol() {
        // A body line (no column) resolves to the enclosing symbol's own name
        // anchor, not the body line.
        let symbols = vec![func("load_config", 41, 8, 50)];
        let SymbolResolution::Match(sym) = line_addressed_symbol(&symbols, 45) else {
            panic!("expected a Match");
        };
        assert_eq!((sym.location.line, sym.location.column), (41, 8));
    }

    #[test]
    fn column_addressing_prefers_the_innermost_symbol_at_an_exact_column() {
        let mut outer = func("outer", 10, 8, 30);
        outer.children = vec![func("inner", 15, 12, 20)];
        let symbols = vec![outer];
        let SymbolResolution::Match(sym) = column_addressed_symbol(&symbols, 15, 12) else {
            panic!("expected a Match");
        };
        assert_eq!((sym.location.line, sym.location.column), (15, 12));
        assert_eq!(sym.name, "inner");
    }

    /// The line-addressing rule shared by every surface: a method's
    /// declaration line resolves to the method, never to the impl/class
    /// block that spans it (which is what a column-1 range match would
    /// pick).
    #[test]
    fn line_addressing_prefers_the_declared_symbol_over_the_enclosing_block() {
        let mut imp = func("MyImpl", 10, 1, 40);
        imp.children = vec![func("method", 15, 8, 20)];
        let symbols = vec![imp];

        match line_addressed_symbol(&symbols, 15) {
            SymbolResolution::Match(sym) => assert_eq!(sym.name, "method"),
            _ => panic!("the declared method must win"),
        }

        // A body line declares nothing — the enclosing symbol is the one
        // honest reading.
        match line_addressed_symbol(&symbols, 18) {
            SymbolResolution::Match(sym) => assert_eq!(sym.name, "method"),
            _ => panic!("body lines fall back to the enclosing symbol"),
        }
        match line_addressed_symbol(&symbols, 35) {
            SymbolResolution::Match(sym) => assert_eq!(sym.name, "MyImpl"),
            _ => panic!("lines outside the method enclose to the impl"),
        }
    }

    /// Multiple declarations on one line are ambiguous; candidates come
    /// back ordered by declaration column so "first declared" is
    /// deterministic for the surfaces that pick-and-disclose.
    #[test]
    fn line_addressing_reports_multi_declaration_lines_in_column_order() {
        let symbols = vec![func("second", 5, 20, 5), func("first", 5, 4, 5)];
        match line_addressed_symbol(&symbols, 5) {
            SymbolResolution::Ambiguous(candidates) => {
                let names: Vec<&str> = candidates.iter().map(|s| s.name.as_str()).collect();
                assert_eq!(names, ["first", "second"]);
            }
            _ => panic!("two declarations on one line are ambiguous"),
        }
    }
    /// A local `const name = …` as a server describes it: the declaration
    /// range spans the whole statement, the name span is stated.
    fn binding(name: &str, line: u32, col: u32) -> Symbol {
        Symbol::new(
            name.to_string(),
            SymbolKind::Constant,
            Location::full(
                std::path::PathBuf::from("x.rs"),
                line,
                col,
                line,
                col.saturating_sub(6),
                line,
                80,
            )
            .with_name_end(line, col + name.len() as u32),
        )
    }

    fn nest(mut parent: Symbol, children: Vec<Symbol>) -> Symbol {
        parent.children = children;
        parent
    }

    /// A position inside a body lands on whatever is declared there — a local
    /// binding, not the code it runs in. The two navigators answer different
    /// questions about the same position, and a caller that wants an identity
    /// wants the callable.
    #[test]
    fn a_position_inside_a_binding_resolves_to_the_callable_that_runs_it() {
        let symbols = vec![nest(
            func("test_places_an_order", 18, 16, 22),
            vec![binding("order", 20, 9)],
        )];

        let inner = find_symbol_at_position(&symbols, 20, Some(30)).expect("a symbol is declared");
        assert_eq!(inner.name, "order");

        let owner = enclosing_callable(&symbols, 20, Some(30)).expect("a callable owns the line");
        assert_eq!(owner.name, "test_places_an_order");
    }

    /// The nearest callable wins over an outer one, so a helper defined inside
    /// a test is named as itself rather than as its host.
    #[test]
    fn the_innermost_callable_owns_the_position() {
        let symbols = vec![nest(
            func("outer", 5, 4, 40),
            vec![nest(
                func("inner", 10, 8, 20),
                vec![binding("value", 12, 12)],
            )],
        )];

        let owner = enclosing_callable(&symbols, 12, Some(20)).expect("a callable owns the line");
        assert_eq!(owner.name, "inner");
    }

    /// An anonymous frame is a position, not an identity. The search passes it
    /// and reports the nearest callable that can be named — never a blank.
    #[test]
    fn an_anonymous_callable_is_passed_for_the_nearest_named_one() {
        let symbols = vec![nest(
            func("test_retries", 30, 16, 40),
            vec![nest(func("", 32, 20, 38), vec![binding("attempt", 34, 12)])],
        )];

        let owner = enclosing_callable(&symbols, 34, Some(20)).expect("a callable owns the line");
        assert_eq!(owner.name, "test_retries");
    }

    /// A position no callable contains has no owner to report; the caller is
    /// left to say so rather than handed the enclosing type.
    #[test]
    fn a_position_outside_every_callable_has_no_owner() {
        let symbols = vec![nest(
            Symbol::new(
                "Cart".to_string(),
                SymbolKind::Class,
                Location::full(std::path::PathBuf::from("x.rs"), 1, 7, 1, 7, 9, 1),
            ),
            vec![binding("total", 3, 5)],
        )];

        assert!(enclosing_callable(&symbols, 3, Some(9)).is_none());
    }
}
