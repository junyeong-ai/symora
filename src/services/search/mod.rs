//! BM25 ranked search with SQLite FTS5

mod db;
mod index;
mod schema;
mod types;

pub use index::SearchIndex;
pub use types::{
    ContentSearchResult, IndexOptions, IndexStats, SearchConfig, SymbolSearchResult,
};
