//! Cross-cutting CLI helpers, grouped by concern:
//!   - `test_matcher`: classify a file as test vs production source.
//!   - `refs`: aggregate LSP `find_references` output into per-test /
//!     per-module statistics.
//!   - `symbol_nav`: position-driven walks over a symbol tree.
//!   - `io`: read line snippets from disk for `--snippet` output.
//!   - `signature`: heuristic `fn xxx(...)` extraction from a body.

mod io;
mod refs;
mod signature;
mod symbol_nav;
mod test_matcher;
pub mod ui;

pub use io::{read_line_at, read_lines_around};
pub use refs::{RefsClassification, classify_refs, extract_module};
pub use signature::extract_signature;
pub use symbol_nav::{
    AnchorResolution, SymbolResolution, ambiguity_hint, column_addressed_symbol,
    find_symbol_at_position, line_addressed_symbol, resolve_symbol_anchor,
    symbols_declared_on_line,
};
pub use test_matcher::TestMatcher;
