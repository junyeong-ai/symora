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
