use std::path::PathBuf;

use serde::{Deserialize, Serialize};

use crate::models::symbol::{Language, SymbolKind};

/// One page of search rows plus the exact total match count, so list
/// output can report `count`/`truncated` precisely instead of guessing
/// from limit saturation.
#[derive(Debug, Clone)]
pub struct SearchPage<T> {
    pub total: usize,
    pub rows: Vec<T>,
    /// True when a file backing one of `rows` changed on disk after it was
    /// indexed (or is gone) — the page is served from a stale index entry.
    /// Cleared by the next `index()` pass over the file.
    pub stale: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SymbolSearchResult {
    pub name: String,
    pub name_path: Option<String>,
    pub kind: SymbolKind,
    pub file: PathBuf,
    pub line: u32,
    pub column: u32,
    pub container: Option<String>,
    pub score: f64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ContentSearchResult {
    pub file: PathBuf,
    pub line: u32,
    pub content: String,
    pub score: f64,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct IndexStats {
    pub file_count: usize,
    pub symbol_count: usize,
    pub content_line_count: usize,
    pub index_size_bytes: u64,
    pub last_indexed: u64,
    pub is_indexing: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub progress: Option<f32>,
}

#[derive(Debug, Clone, Default)]
pub struct IndexOptions {
    pub force: bool,
    pub languages: Option<Vec<Language>>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StoreConfig {
    pub ttl_secs: u64,
    pub index_content: bool,
    /// How many files to extract concurrently during a `index` run. Higher
    /// gives better wall-clock time on cores but raises peak file-descriptor
    /// and SQLite write contention. 16 is a reasonable default.
    pub index_concurrency: usize,
}

impl Default for StoreConfig {
    fn default() -> Self {
        Self {
            ttl_secs: 3600,
            index_content: true,
            index_concurrency: crate::constants::defaults::STORE_INDEX_CONCURRENCY,
        }
    }
}
