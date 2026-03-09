use std::path::PathBuf;

use serde::{Deserialize, Serialize};

use crate::models::symbol::{Language, SymbolKind};

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
}

impl Default for StoreConfig {
    fn default() -> Self {
        Self {
            ttl_secs: 3600,
            index_content: true,
        }
    }
}
