use std::collections::HashMap;
use std::hash::Hash;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use tokio::sync::RwLock;

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
    /// `infra::lsp::content_generation`). Any edit — ours or external —
    /// bumps the generation, so a cached answer can never describe a
    /// workspace that no longer exists; the TTL only bounds memory.
    pub async fn get_or_compute<F, Fut>(
        &self,
        language: Language,
        query: &str,
        generation: u64,
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
        self.inner
            .get_or_compute(&key, generation, || async {
                Ok(Arc::new(compute().await?))
            })
            .await
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
            vec![Symbol::new(
                name.to_string(),
                SymbolKind::Function,
                Location::point(PathBuf::from("/test/file.rs"), 1, 1),
            )]
        };

        let first = cache
            .get_or_compute(Language::Rust, "alpha", 1, || async { Ok(symbol("old")) })
            .await
            .unwrap();
        assert_eq!(first[0].name, "old");

        // Same generation: served from cache.
        let hit = cache
            .get_or_compute(Language::Rust, "alpha", 1, || async { Ok(symbol("new")) })
            .await
            .unwrap();
        assert_eq!(hit[0].name, "old");

        // The workspace changed: the entry is invalid, recompute.
        let fresh = cache
            .get_or_compute(Language::Rust, "alpha", 2, || async { Ok(symbol("new")) })
            .await
            .unwrap();
        assert_eq!(fresh[0].name, "new");
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
