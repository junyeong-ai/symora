//! Cross-cutting CLI helpers, grouped by concern:
//!   - `refs`: aggregate LSP `find_references` output into per-test /
//!     per-module statistics.
//!   - `symbol_nav`: position-driven walks over a symbol tree.
//!   - `io`: read line snippets from disk for `--snippet` output.
//!   - `signature`: heuristic `fn xxx(...)` extraction from a body.

mod io;
mod refs;
mod signature;
mod symbol_nav;
pub mod ui;

pub use io::{read_line_at, read_lines_around};
pub use refs::{RefsClassification, extract_module};
pub use signature::extract_signature;
pub use symbol_nav::{
    AnchorResolution, SymbolResolution, ambiguity_hint, column_addressed_symbol,
    enclosing_callable, find_named_at_position, find_symbol_at_position, line_addressed_symbol,
    symbols_declared_on_line,
};
