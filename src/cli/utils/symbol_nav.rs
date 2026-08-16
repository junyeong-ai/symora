//! Position-driven navigation over a `documentSymbol` tree.

use crate::models::symbol::Symbol;

/// Whether a symbol's declared range covers a position. The one reading of
/// containment every navigator here shares, so two of them can never disagree
/// about which symbols a position is inside.
///
/// - `column = None`: line-only matching.
/// - `column = Some(col)`: line+column range matching.
fn contains_position(symbol: &Symbol, line: u32, column: Option<u32>) -> bool {
    let loc = &symbol.location;
    let start = loc.line;
    let end = loc.end_line.unwrap_or(start);

    match column {
        None => line >= start && line <= end,
        Some(col) => {
            if start == end {
                loc.line == line && col >= loc.column && loc.end_column.is_none_or(|ec| col <= ec)
            } else {
                (line > start && line < end)
                    || (line == start && col >= loc.column)
                    || (line == end && loc.end_column.is_none_or(|ec| col <= ec))
            }
        }
    }
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

/// Whether a snapped/analyzed anchor resolved to a symbol, and if not, why. The
/// two failure cases are kept distinct because they license different claims:
/// `NotASymbol` was checked and is genuinely not a symbol; `Unavailable` could
/// not be checked (the symbol read failed), so neither "is" nor "is not a
/// symbol" may be claimed. Collapsing them would let a mere read failure be
/// reported as a false "not a symbol".
///
/// This is the single source of the anchor-resolution disclosure: every surface
/// (callers/callees/implementations/refs/impact/context) renders it through
/// [`as_status`](AnchorResolution::as_status) so the JSON marker is one
/// consistent `*_status` string, never a per-surface bool/shape.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AnchorResolution {
    /// Snapped to a symbol's name anchor (possibly after disambiguating a
    /// multi-declaration line).
    Resolved,
    /// Symbols were read but none is addressed by this position.
    NotASymbol,
    /// The symbol read failed, so the position could not be snapped; whether it
    /// is a symbol is unknown.
    Unavailable,
}

impl AnchorResolution {
    /// Whether the input snapped cleanly to a symbol.
    pub fn is_resolved(self) -> bool {
        matches!(self, AnchorResolution::Resolved)
    }

    /// The public disclosure marker for an unresolved anchor, or `None` when
    /// resolved (the field is then omitted). One shared vocabulary across every
    /// surface: `"not_a_symbol"` vs `"unavailable"` — never collapsed to a bool.
    pub fn as_status(self) -> Option<&'static str> {
        match self {
            AnchorResolution::Resolved => None,
            AnchorResolution::NotASymbol => Some("not_a_symbol"),
            AnchorResolution::Unavailable => Some("unavailable"),
        }
    }
}

/// The navigation disclosure for a multi-declaration line resolved to its first
/// declaration: names the alternatives and how to target another. The single
/// home for this wording, shared by `snap_to_symbol_anchor` and
/// `resolve_navigation_target` so the two cannot drift. `declared` is ordered by
/// declaration column, so `[0]` is the chosen first declaration.
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

/// Resolution for a column-addressed target (`file:line:col`): the
/// innermost symbol whose range contains the exact position. When the
/// column matches nothing, fall back to the line's declarations while
/// that stays unambiguous.
pub fn column_addressed_symbol(symbols: &[Symbol], line: u32, column: u32) -> SymbolResolution<'_> {
    if let Some(symbol) = find_symbol_at_position(symbols, line, Some(column)) {
        return SymbolResolution::Match(symbol);
    }
    declared_or_enclosing(symbols, line)
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
    use crate::models::symbol::{Location, SymbolKind};

    fn func(name: &str, line: u32, name_col: u32, end_line: u32) -> Symbol {
        Symbol::new(
            name.to_string(),
            SymbolKind::Function,
            Location::full(
                std::path::PathBuf::from("x.rs"),
                line,
                name_col,
                line,
                name_col,
                end_line,
                1,
            ),
        )
    }

    #[test]
    fn column_addressing_snaps_declaration_start_column_to_the_name() {
        // The function name is at column 8 (after `fn `), but the declaration
        // starts at column 1 (`pub`) — the column `search symbols` reports.
        // Column-addressing from column 1 must still land on the name anchor so
        // callers/refs/impact agree with an exact name position.
        let symbols = vec![func("load_config", 41, 8, 50)];
        let SymbolResolution::Match(sym) = column_addressed_symbol(&symbols, 41, 1) else {
            panic!("expected a Match");
        };
        assert_eq!((sym.location.line, sym.location.column), (41, 8));
        assert_eq!(sym.name, "load_config");
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
    fn binding(name: &str, line: u32, col: u32) -> Symbol {
        Symbol::new(
            name.to_string(),
            SymbolKind::Constant,
            Location::full(
                std::path::PathBuf::from("x.rs"),
                line,
                col,
                line,
                col,
                line,
                80,
            ),
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
