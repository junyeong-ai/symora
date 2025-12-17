//! Symbol caching with LRU eviction and statistics

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use tokio::sync::RwLock;

use crate::models::symbol::{Language, Symbol};

struct CacheEntry {
    content_hash: u64,
    symbols: Arc<Vec<Symbol>>,
    created_at: Instant,
    last_accessed: Instant,
}

impl CacheEntry {
    fn is_valid(&self, content_hash: u64, ttl: Duration) -> bool {
        self.content_hash == content_hash && self.created_at.elapsed() < ttl
    }
}

/// Thread-safe symbol cache with LRU eviction
pub struct SymbolCache {
    entries: RwLock<HashMap<PathBuf, CacheEntry>>,
    max_entries: usize,
    ttl: Duration,
    hits: AtomicU64,
    misses: AtomicU64,
}

impl Default for SymbolCache {
    fn default() -> Self {
        Self::new(Duration::from_secs(300), 1000)
    }
}

impl SymbolCache {
    pub fn new(ttl: Duration, max_entries: usize) -> Self {
        Self {
            entries: RwLock::new(HashMap::new()),
            max_entries,
            ttl,
            hits: AtomicU64::new(0),
            misses: AtomicU64::new(0),
        }
    }

    /// Get cached symbols or compute them
    pub async fn get_or_compute<F, Fut>(
        &self,
        path: &Path,
        content: &str,
        compute: F,
    ) -> Result<Arc<Vec<Symbol>>, crate::error::LspError>
    where
        F: FnOnce() -> Fut,
        Fut: std::future::Future<Output = Result<Vec<Symbol>, crate::error::LspError>>,
    {
        let hash = crate::infra::hash_content(content);

        // Fast path: check cache with read lock
        {
            let mut entries = self.entries.write().await;
            if let Some(entry) = entries.get_mut(path)
                && entry.is_valid(hash, self.ttl)
            {
                entry.last_accessed = Instant::now();
                self.hits.fetch_add(1, Ordering::Relaxed);
                tracing::trace!("Symbol cache hit: {}", path.display());
                return Ok(Arc::clone(&entry.symbols));
            }
        }

        // Cache miss - compute symbols
        self.misses.fetch_add(1, Ordering::Relaxed);
        tracing::trace!("Symbol cache miss: {}", path.display());
        let symbols = Arc::new(compute().await?);

        // Store in cache with eviction
        {
            let mut entries = self.entries.write().await;

            // Evict if at capacity
            if entries.len() >= self.max_entries {
                self.evict_lru(&mut entries);
            }

            entries.insert(
                path.to_path_buf(),
                CacheEntry {
                    content_hash: hash,
                    symbols: Arc::clone(&symbols),
                    created_at: Instant::now(),
                    last_accessed: Instant::now(),
                },
            );
        }

        Ok(symbols)
    }

    /// Evict least recently used entry
    fn evict_lru(&self, entries: &mut HashMap<PathBuf, CacheEntry>) {
        if let Some((oldest_path, _)) = entries
            .iter()
            .min_by_key(|(_, e)| e.last_accessed)
            .map(|(p, e)| (p.clone(), e.last_accessed))
        {
            entries.remove(&oldest_path);
            tracing::trace!("Evicted cache entry: {}", oldest_path.display());
        }
    }

    /// Invalidate cache for a specific file
    pub async fn invalidate(&self, path: &Path) {
        let mut entries = self.entries.write().await;
        entries.remove(path);
    }

    /// Clear all cached entries
    pub async fn clear(&self) {
        let mut entries = self.entries.write().await;
        entries.clear();
        self.hits.store(0, Ordering::Relaxed);
        self.misses.store(0, Ordering::Relaxed);
    }

    /// Remove expired entries
    pub async fn cleanup_expired(&self) -> usize {
        let mut entries = self.entries.write().await;
        let before = entries.len();
        let ttl = self.ttl;
        entries.retain(|_, entry| entry.created_at.elapsed() < ttl);
        before - entries.len()
    }

    /// Get cache statistics
    pub async fn stats(&self) -> CacheStats {
        let entries = self.entries.read().await;
        let total_symbols: usize = entries.values().map(|e| e.symbols.len()).sum();
        let hits = self.hits.load(Ordering::Relaxed);
        let misses = self.misses.load(Ordering::Relaxed);
        CacheStats {
            entry_count: entries.len(),
            total_symbols,
            hits,
            misses,
            hit_rate: if hits + misses > 0 {
                hits as f64 / (hits + misses) as f64
            } else {
                0.0
            },
        }
    }
}

#[derive(Debug, Clone)]
pub struct CacheStats {
    pub entry_count: usize,
    pub total_symbols: usize,
    pub hits: u64,
    pub misses: u64,
    pub hit_rate: f64,
}

/// Workspace-level symbol cache with version tracking
pub struct WorkspaceSymbolCache {
    entries: RwLock<HashMap<WorkspaceCacheKey, WorkspaceCacheEntry>>,
    server_versions: RwLock<HashMap<Language, String>>,
    ttl: Duration,
    max_entries: usize,
}

#[derive(Debug, Clone, Hash, PartialEq, Eq)]
struct WorkspaceCacheKey {
    language: Language,
    query: String,
}

struct WorkspaceCacheEntry {
    symbols: Arc<Vec<Symbol>>,
    created_at: Instant,
    server_version: String,
}

impl Default for WorkspaceSymbolCache {
    fn default() -> Self {
        Self::new(Duration::from_secs(120), 50)
    }
}

impl WorkspaceSymbolCache {
    pub fn new(ttl: Duration, max_entries: usize) -> Self {
        Self {
            entries: RwLock::new(HashMap::new()),
            server_versions: RwLock::new(HashMap::new()),
            ttl,
            max_entries,
        }
    }

    pub async fn get_or_compute<F, Fut>(
        &self,
        language: Language,
        query: &str,
        compute: F,
    ) -> Result<Arc<Vec<Symbol>>, crate::error::LspError>
    where
        F: FnOnce() -> Fut,
        Fut: std::future::Future<Output = Result<Vec<Symbol>, crate::error::LspError>>,
    {
        let key = WorkspaceCacheKey {
            language,
            query: query.to_string(),
        };

        let current_version = self.get_server_version(language).await;

        {
            let entries = self.entries.read().await;
            if let Some(entry) = entries.get(&key)
                && entry.created_at.elapsed() < self.ttl
                && entry.server_version == current_version
            {
                tracing::trace!("Workspace symbol cache hit: {}:{}", language, query);
                return Ok(Arc::clone(&entry.symbols));
            }
        }

        tracing::trace!("Workspace symbol cache miss: {}:{}", language, query);
        let symbols = Arc::new(compute().await?);

        {
            let mut entries = self.entries.write().await;
            if entries.len() >= self.max_entries {
                self.evict_oldest(&mut entries);
            }
            entries.insert(
                key,
                WorkspaceCacheEntry {
                    symbols: Arc::clone(&symbols),
                    created_at: Instant::now(),
                    server_version: current_version,
                },
            );
        }

        Ok(symbols)
    }

    pub async fn update_server_version(&self, language: Language, version: String) {
        let mut versions = self.server_versions.write().await;
        let old_version = versions.insert(language, version.clone());
        if old_version.as_ref() != Some(&version) {
            drop(versions);
            self.invalidate_language(language).await;
        }
    }

    async fn get_server_version(&self, language: Language) -> String {
        self.server_versions
            .read()
            .await
            .get(&language)
            .cloned()
            .unwrap_or_default()
    }

    pub async fn invalidate_language(&self, language: Language) {
        let mut entries = self.entries.write().await;
        entries.retain(|k, _| k.language != language);
    }

    fn evict_oldest(&self, entries: &mut HashMap<WorkspaceCacheKey, WorkspaceCacheEntry>) {
        if let Some((oldest_key, _)) = entries
            .iter()
            .min_by_key(|(_, e)| e.created_at)
            .map(|(k, e)| (k.clone(), e.created_at))
        {
            entries.remove(&oldest_key);
        }
    }

    pub async fn clear(&self) {
        self.entries.write().await.clear();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::models::symbol::{Location, SymbolKind};

    #[tokio::test]
    async fn test_cache_hit() {
        let cache = SymbolCache::default();
        let path = Path::new("/test/file.rs");
        let content = "fn main() {}";

        // First call - cache miss
        let symbols1 = cache
            .get_or_compute(path, content, || async {
                Ok(vec![Symbol::new(
                    "main".to_string(),
                    SymbolKind::Function,
                    Location::new(PathBuf::from("/test/file.rs"), 1, 1, 1, 12),
                )])
            })
            .await
            .unwrap();

        assert_eq!(symbols1.len(), 1);

        // Second call - cache hit (same content)
        let symbols2 = cache
            .get_or_compute(path, content, || async {
                Ok(vec![]) // This should not be called
            })
            .await
            .unwrap();

        assert_eq!(symbols2.len(), 1);
        assert_eq!(symbols1[0].name, symbols2[0].name);

        let stats = cache.stats().await;
        assert_eq!(stats.hits, 1);
        assert_eq!(stats.misses, 1);
    }

    #[tokio::test]
    async fn test_cache_invalidation_on_content_change() {
        let cache = SymbolCache::default();
        let path = Path::new("/test/file.rs");

        // First content
        let _ = cache
            .get_or_compute(path, "fn foo() {}", || async {
                Ok(vec![Symbol::new(
                    "foo".to_string(),
                    SymbolKind::Function,
                    Location::new(PathBuf::from("/test/file.rs"), 1, 1, 1, 10),
                )])
            })
            .await
            .unwrap();

        // Different content - should recompute
        let symbols = cache
            .get_or_compute(path, "fn bar() {}", || async {
                Ok(vec![Symbol::new(
                    "bar".to_string(),
                    SymbolKind::Function,
                    Location::new(PathBuf::from("/test/file.rs"), 1, 1, 1, 10),
                )])
            })
            .await
            .unwrap();

        assert_eq!(symbols[0].name, "bar");
    }

    #[tokio::test]
    async fn test_lru_eviction() {
        let cache = SymbolCache::new(Duration::from_secs(300), 2);

        // Add 3 entries to a cache with max 2
        for i in 0..3 {
            let path = PathBuf::from(format!("/test/file{}.rs", i));
            let content = format!("fn test{}() {{}}", i);
            let path_clone = path.clone();
            let _ = cache
                .get_or_compute(&path, &content, || async move {
                    Ok(vec![Symbol::new(
                        format!("test{}", i),
                        SymbolKind::Function,
                        Location::new(path_clone, 1, 1, 1, 10),
                    )])
                })
                .await
                .unwrap();
        }

        let stats = cache.stats().await;
        assert_eq!(stats.entry_count, 2);
    }
}
