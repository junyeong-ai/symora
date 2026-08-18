use std::path::PathBuf;

use serde::{Deserialize, Serialize};

use crate::models::symbol::{Language, SymbolKind};

/// A path a build could not read, and whether the walk knew it to be a file.
///
/// The distinction is load-bearing, and only the walk can make it: a file's
/// name settles its language, so one the build could not open keeps matches
/// from that language alone. A directory's name settles nothing — one called
/// `generated.py` can hide any language — and a later stat cannot recover the
/// difference, because the path is unreadable now too.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct UnreadPath {
    pub path: String,
    pub is_file: bool,
}

/// One page of search rows plus the exact total match count, so list
/// output can report `count`/`truncated` precisely instead of guessing
/// from limit saturation.
#[derive(Debug, Clone)]
pub struct SearchPage<T> {
    pub total: usize,
    pub rows: Vec<T>,
    /// The files backing `rows` that changed on disk after they were indexed
    /// (or are gone) — the page is served from stale index entries. Cleared by
    /// the next `index()` pass over each file.
    ///
    /// Named rather than a flag because a caller that filters the page has to
    /// narrow the question to the files its answer kept.
    pub stale_files: Vec<String>,
    /// The languages this answer speaks for, read in the same snapshot as
    /// the rows: what the last completed build covers, narrowed to what
    /// was asked for. Whatever a caller reads live must lie outside it, or
    /// the two would answer for the same file.
    pub covered: Vec<Language>,
    /// Paths the build behind this page could not read, read in the same
    /// snapshot. A non-empty list makes `total` a lower bound for the
    /// languages in `covered` those paths could hold: they are absent from
    /// the index although its scope names them.
    pub unread_paths: Vec<UnreadPath>,
}

impl<T> SearchPage<T> {
    /// Whether anything backing this page is stale.
    pub fn stale(&self) -> bool {
        !self.stale_files.is_empty()
    }
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
    /// The languages the last completed build extracts symbols for — the
    /// set this index answers authoritatively for. Empty until a build
    /// completes, which is how a partial index is told from a whole one.
    pub languages: Vec<Language>,
    /// Paths this build could not read — a file it found but could not
    /// open, or a directory it could not enter and whose contents it
    /// therefore never saw. Both are permission or I/O failures, so
    /// retrying could still cover them. A missing or non-text file is not
    /// counted: neither is a hole, because neither belongs in the index at
    /// all. These do, and they are absent from an index whose scope claims
    /// their language, so the build names them rather than leaving the hole
    /// silent — and naming them is what makes the repair actionable and lets
    /// a later per-file refresh clear one.
    ///
    /// No `serde(default)`, here or on `languages`: the daemon and the client
    /// are the same build — the client replaces a daemon whose version or
    /// build id differs — so a field missing from the wire is a foreign
    /// sender, and defaulting one that qualifies a completeness claim would
    /// answer it with silence.
    pub unread_paths: Vec<UnreadPath>,
}

#[derive(Debug, Clone, Default)]
pub struct IndexOptions {
    pub force: bool,
    pub languages: Option<Vec<Language>>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StoreConfig {
    pub index_content: bool,
    /// How many files to extract concurrently during a `index` run. Higher
    /// gives better wall-clock time on cores but raises peak file-descriptor
    /// and SQLite write contention. 16 is a reasonable default.
    pub index_concurrency: usize,
}

impl Default for StoreConfig {
    fn default() -> Self {
        Self {
            index_content: true,
            index_concurrency: crate::constants::defaults::STORE_INDEX_CONCURRENCY,
        }
    }
}
