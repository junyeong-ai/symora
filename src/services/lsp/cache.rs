use std::collections::HashMap;
use std::hash::Hash;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use tokio::sync::RwLock;

use crate::models::lsp::Indexed;
use crate::models::symbol::{Language, Symbol};

struct CacheEntry<V> {
    value: V,
    extra_hash: u64,
    created_at: Instant,
}

struct AsyncCache<K: Eq + Hash + Clone, V: Clone> {
    entries: RwLock<HashMap<K, CacheEntry<V>>>,
    max_entries: usize,
    ttl: Duration,
    hits: AtomicU64,
    misses: AtomicU64,
}

impl<K: Eq + Hash + Clone, V: Clone> AsyncCache<K, V> {
    fn new(ttl: Duration, max_entries: usize) -> Self {
        Self {
            entries: RwLock::new(HashMap::new()),
            max_entries,
            ttl,
            hits: AtomicU64::new(0),
            misses: AtomicU64::new(0),
        }
    }

    /// Get a cached value or compute it. If `extra_hash` is non-zero, the entry
    /// is only valid when both the TTL and the hash match.
    async fn get_or_compute<F, Fut>(
        &self,
        key: &K,
        extra_hash: u64,
        compute: F,
    ) -> Result<V, crate::error::LspError>
    where
        F: FnOnce() -> Fut,
        Fut: std::future::Future<Output = Result<V, crate::error::LspError>>,
    {
        // Phase 1: Try read lock first (fast path)
        {
            let entries = self.entries.read().await;
            if let Some(entry) = entries.get(key)
                && entry.is_valid(extra_hash, self.ttl)
            {
                self.hits.fetch_add(1, Ordering::Relaxed);
                return Ok(entry.value.clone());
            }
        }

        // Phase 2: Compute outside any lock
        let value = compute().await?;

        // Phase 3: Insert with write lock
        let mut entries = self.entries.write().await;
        // Double-check: another task may have computed while we waited
        if let Some(entry) = entries.get(key)
            && entry.is_valid(extra_hash, self.ttl)
        {
            self.hits.fetch_add(1, Ordering::Relaxed);
            return Ok(entry.value.clone());
        }

        self.misses.fetch_add(1, Ordering::Relaxed);
        if entries.len() >= self.max_entries {
            Self::evict_oldest(&mut entries);
        }

        entries.insert(
            key.clone(),
            CacheEntry {
                value: value.clone(),
                extra_hash,
                created_at: Instant::now(),
            },
        );

        Ok(value)
    }

    fn evict_oldest(entries: &mut HashMap<K, CacheEntry<V>>) {
        if let Some(oldest_key) = entries
            .iter()
            .min_by_key(|(_, e)| e.created_at)
            .map(|(k, _)| k.clone())
        {
            entries.remove(&oldest_key);
        }
    }

    /// Fast-path lookup: the cached value when present and valid for
    /// `extra_hash`, else `None`. Counts a hit only on success.
    async fn get_valid(&self, key: &K, extra_hash: u64) -> Option<V> {
        let entries = self.entries.read().await;
        let entry = entries.get(key)?;
        if entry.is_valid(extra_hash, self.ttl) {
            self.hits.fetch_add(1, Ordering::Relaxed);
            return Some(entry.value.clone());
        }
        None
    }

    /// Store a freshly computed value (counted as a miss), evicting the
    /// oldest entry when the cap is reached.
    async fn insert(&self, key: K, value: V, extra_hash: u64) {
        self.misses.fetch_add(1, Ordering::Relaxed);
        let mut entries = self.entries.write().await;
        if entries.len() >= self.max_entries && !entries.contains_key(&key) {
            Self::evict_oldest(&mut entries);
        }
        entries.insert(
            key,
            CacheEntry {
                value,
                extra_hash,
                created_at: Instant::now(),
            },
        );
    }

    async fn remove(&self, key: &K) {
        self.entries.write().await.remove(key);
    }

    async fn retain(&self, f: impl Fn(&K, &CacheEntry<V>) -> bool) {
        self.entries.write().await.retain(|k, v| f(k, v));
    }

    async fn clear(&self) {
        self.entries.write().await.clear();
        self.hits.store(0, Ordering::Relaxed);
        self.misses.store(0, Ordering::Relaxed);
    }

    async fn cleanup_expired(&self) -> usize {
        let mut entries = self.entries.write().await;
        let before = entries.len();
        let ttl = self.ttl;
        entries.retain(|_, entry| entry.created_at.elapsed() < ttl);
        before - entries.len()
    }

    #[cfg(test)]
    async fn stats(&self) -> CacheStats {
        let entries = self.entries.read().await;
        CacheStats {
            entry_count: entries.len(),
            hits: self.hits.load(Ordering::Relaxed),
            misses: self.misses.load(Ordering::Relaxed),
        }
    }
}

impl<V> CacheEntry<V> {
    fn is_valid(&self, extra_hash: u64, ttl: Duration) -> bool {
        self.created_at.elapsed() < ttl && (extra_hash == 0 || self.extra_hash == extra_hash)
    }
}

#[cfg(test)]
#[derive(Debug, Clone)]
pub struct CacheStats {
    pub entry_count: usize,
    pub hits: u64,
    pub misses: u64,
}

// --- SymbolCache: file path keyed, content-hash validated ---

pub struct SymbolCache {
    inner: AsyncCache<PathBuf, Arc<Vec<Symbol>>>,
}

impl Default for SymbolCache {
    fn default() -> Self {
        Self::new(Duration::from_secs(300), 1000)
    }
}

impl SymbolCache {
    pub fn new(ttl: Duration, max_entries: usize) -> Self {
        Self {
            inner: AsyncCache::new(ttl, max_entries),
        }
    }

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
        self.inner
            .get_or_compute(&path.to_path_buf(), hash, || async {
                Ok(Arc::new(compute().await?))
            })
            .await
    }

    pub async fn invalidate(&self, path: &Path) {
        self.inner.remove(&path.to_path_buf()).await;
    }

    pub async fn clear(&self) {
        self.inner.clear().await;
    }

    pub async fn cleanup_expired(&self) -> usize {
        self.inner.cleanup_expired().await
    }

    #[cfg(test)]
    pub async fn stats(&self) -> CacheStats {
        self.inner.stats().await
    }
}

// --- WorkspaceSymbolCache: language+query keyed, generation validated ---

pub struct WorkspaceSymbolCache {
    inner: AsyncCache<WorkspaceCacheKey, Arc<Vec<Symbol>>>,
}

#[derive(Debug, Clone, Hash, PartialEq, Eq)]
struct WorkspaceCacheKey {
    language: Language,
    query: String,
}

impl Default for WorkspaceSymbolCache {
    fn default() -> Self {
        Self::new(Duration::from_secs(120), 50)
    }
}

impl WorkspaceSymbolCache {
    pub fn new(ttl: Duration, max_entries: usize) -> Self {
        Self {
            inner: AsyncCache::new(ttl, max_entries),
        }
    }

    /// Get or compute a workspace-symbol answer, valid only for the
    /// workspace-content `generation` it was computed under (see
    /// `infra::lsp::content_generation`). What bumps the generation: our
    /// own writes (`note_files_edited` after every edit), open-overlay
    /// drift picked up by the pre-request sweep, and client start (a
    /// fresh server is a fresh world). An EXTERNAL edit to a file no
    /// server has open is the one change the generation cannot see — that
    /// staleness is bounded by the TTL, which is why the TTL is short
    /// rather than memory-only.
    ///
    /// Answers computed under degraded indexing are returned with their
    /// marker but never cached: the server is still warming, and serving
    /// the lower bound for a whole TTL after quiescence would trap the
    /// "retry once warm" path the marker tells an agent to take.
    pub async fn get_or_compute<F, Fut>(
        &self,
        language: Language,
        query: &str,
        generation: u64,
        compute: F,
    ) -> Result<Indexed<Arc<Vec<Symbol>>>, crate::error::LspError>
    where
        F: FnOnce() -> Fut,
        Fut: std::future::Future<Output = Result<Indexed<Vec<Symbol>>, crate::error::LspError>>,
    {
        let key = WorkspaceCacheKey {
            language,
            query: query.to_string(),
        };
        // Only undegraded answers are ever stored, so a hit is complete.
        if let Some(cached) = self.inner.get_valid(&key, generation).await {
            return Ok(Indexed::complete(cached));
        }
        let computed = compute().await?;
        let value = Arc::new(computed.data);
        if computed.indexing.is_none() {
            self.inner.insert(key, Arc::clone(&value), generation).await;
        }
        Ok(Indexed::new(value, computed.indexing))
    }

    pub async fn invalidate_language(&self, language: Language) {
        self.inner.retain(|k, _| k.language != language).await;
    }

    pub async fn clear(&self) {
        self.inner.clear().await;
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
                    Location::point(PathBuf::from("/test/file.rs"), 1, 1),
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
                    Location::point(PathBuf::from("/test/file.rs"), 1, 1),
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
                    Location::point(PathBuf::from("/test/file.rs"), 1, 1),
                )])
            })
            .await
            .unwrap();

        assert_eq!(symbols[0].name, "bar");
    }

    /// A workspace-symbol answer is bound to the content generation it
    /// was computed under: the same generation hits, a newer one (any
    /// edit happened since) recomputes — a cached answer must never
    /// describe a workspace that no longer exists.
    #[tokio::test]
    async fn workspace_cache_invalidates_on_generation_change() {
        let cache = WorkspaceSymbolCache::default();
        let symbol = |name: &str| {
            Indexed::complete(vec![Symbol::new(
                name.to_string(),
                SymbolKind::Function,
                Location::point(PathBuf::from("/test/file.rs"), 1, 1),
            )])
        };

        let first = cache
            .get_or_compute(Language::Rust, "alpha", 1, || async { Ok(symbol("old")) })
            .await
            .unwrap();
        assert_eq!(first.data[0].name, "old");

        // Same generation: served from cache.
        let hit = cache
            .get_or_compute(Language::Rust, "alpha", 1, || async { Ok(symbol("new")) })
            .await
            .unwrap();
        assert_eq!(hit.data[0].name, "old");

        // The workspace changed: the entry is invalid, recompute.
        let fresh = cache
            .get_or_compute(Language::Rust, "alpha", 2, || async { Ok(symbol("new")) })
            .await
            .unwrap();
        assert_eq!(fresh.data[0].name, "new");
    }

    /// An answer computed under degraded indexing is returned with its
    /// marker but never cached: once the server reaches quiescence, the
    /// next query must recompute against the complete index instead of
    /// serving the stale lower bound for a whole TTL.
    #[tokio::test]
    async fn workspace_cache_never_stores_degraded_answers() {
        use crate::models::lsp::IndexingDegradation;

        let cache = WorkspaceSymbolCache::default();
        let symbol = |name: &str, marker: Option<IndexingDegradation>| {
            Indexed::new(
                vec![Symbol::new(
                    name.to_string(),
                    SymbolKind::Function,
                    Location::point(PathBuf::from("/test/file.rs"), 1, 1),
                )],
                marker,
            )
        };

        let degraded = cache
            .get_or_compute(Language::Rust, "alpha", 1, || async {
                Ok(symbol("partial", Some(IndexingDegradation::TimedOut)))
            })
            .await
            .unwrap();
        assert_eq!(degraded.indexing, Some(IndexingDegradation::TimedOut));
        assert_eq!(degraded.data[0].name, "partial");

        // Same generation, but the degraded answer was not cached — the
        // recompute (now complete) wins and IS cached.
        let complete = cache
            .get_or_compute(Language::Rust, "alpha", 1, || async {
                Ok(symbol("full", None))
            })
            .await
            .unwrap();
        assert_eq!(complete.indexing, None);
        assert_eq!(complete.data[0].name, "full");

        let hit = cache
            .get_or_compute(Language::Rust, "alpha", 1, || async {
                Err::<Indexed<Vec<Symbol>>, _>(crate::error::LspError::Protocol(
                    "must be served from cache".to_string(),
                ))
            })
            .await
            .expect("the complete answer was cached");
        assert_eq!(hit.indexing, None);
        assert_eq!(hit.data[0].name, "full");
    }

    #[tokio::test]
    async fn test_eviction() {
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
                        Location::point(path_clone, 1, 1),
                    )])
                })
                .await
                .unwrap();
        }

        let stats = cache.stats().await;
        assert_eq!(stats.entry_count, 2);
    }
}
