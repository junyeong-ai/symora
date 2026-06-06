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
}
