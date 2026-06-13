//! Position-driven navigation over a `documentSymbol` tree.

use crate::models::symbol::Symbol;

/// Find the innermost symbol at a position (recursive search).
///
/// - `column = None`: line-only matching (deepest symbol containing the line).
/// - `column = Some(col)`: line+column range matching (deepest symbol whose
///   range contains the position).
pub fn find_symbol_at_position(
    symbols: &[Symbol],
    line: u32,
    column: Option<u32>,
) -> Option<&Symbol> {
    fn search(symbols: &[Symbol], line: u32, column: Option<u32>) -> Option<&Symbol> {
        for symbol in symbols {
            let loc = &symbol.location;
            let start = loc.line;
            let end = loc.end_line.unwrap_or(start);

            let in_range = match column {
                None => line >= start && line <= end,
                Some(col) => {
                    if start == end {
                        loc.line == line
                            && col >= loc.column
                            && loc.end_column.is_none_or(|ec| col <= ec)
                    } else {
                        (line > start && line < end)
                            || (line == start && col >= loc.column)
                            || (line == end && loc.end_column.is_none_or(|ec| col <= ec))
                    }
                }
            };

            if in_range {
                if let Some(child) = search(&symbol.children, line, column) {
                    return Some(child);
                }
                return Some(symbol);
            }
        }
        None
    }
    search(symbols, line, column)
}

/// Resolve `(line, column)` to the most specific symbol available, falling
/// back from column-precise to line-only matching, and surfacing the
/// symbol's authoritative anchor position alongside it.
pub fn resolve_symbol_anchor(
    symbols: &[Symbol],
    line: u32,
    column: u32,
) -> Option<(u32, u32, &Symbol)> {
    find_symbol_at_position(symbols, line, Some(column))
        .or_else(|| find_symbol_at_position(symbols, line, None))
        .map(|symbol| (symbol.location.line, symbol.location.column, symbol))
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
    fn anchor_snaps_declaration_start_column_to_the_name() {
        // The function name is at column 8 (after `fn `), but the declaration
        // starts at column 1 (`pub`) — the column `search symbols` reports.
        // Resolving from column 1 must still land on the name anchor so
        // callers/refs/impact agree with an exact name position.
        let symbols = vec![func("load_config", 41, 8, 50)];
        let (line, column, sym) = resolve_symbol_anchor(&symbols, 41, 1).unwrap();
        assert_eq!((line, column), (41, 8));
        assert_eq!(sym.name, "load_config");
    }

    #[test]
    fn anchor_resolves_line_only_input_to_the_symbol() {
        // A line in the body (no meaningful column) resolves to the symbol's
        // own name anchor, not the body line.
        let symbols = vec![func("load_config", 41, 8, 50)];
        let (line, column, _) = resolve_symbol_anchor(&symbols, 45, 1).unwrap();
        assert_eq!((line, column), (41, 8));
    }

    #[test]
    fn anchor_prefers_the_innermost_symbol_at_an_exact_column() {
        let mut outer = func("outer", 10, 8, 30);
        outer.children = vec![func("inner", 15, 12, 20)];
        let symbols = vec![outer];
        let (line, column, sym) = resolve_symbol_anchor(&symbols, 15, 12).unwrap();
        assert_eq!((line, column), (15, 12));
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
}
