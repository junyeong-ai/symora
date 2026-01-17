use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};
use tokio::sync::RwLock;

use super::registry::FileRegistry;
use super::types::{CallInfo, Edge, EdgeKind, SymbolId, SymbolNode, TypeHierarchyInfo};
use crate::models::lsp::CallHierarchyItem;
use crate::models::symbol::{Location, SymbolKind};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GraphConfig {
    pub max_nodes: usize,
    pub max_edges_per_symbol: usize,
    pub ttl_secs: u64,
}

impl Default for GraphConfig {
    fn default() -> Self {
        Self {
            max_nodes: 100_000,
            max_edges_per_symbol: 1_000,
            ttl_secs: 600,
        }
    }
}

#[derive(Debug, Default)]
struct GraphStats {
    cache_hits: AtomicU64,
    cache_misses: AtomicU64,
    invalidations: AtomicU64,
}

pub struct CodeGraph {
    files: RwLock<FileRegistry>,
    nodes: RwLock<HashMap<SymbolId, SymbolNode>>,
    edges: RwLock<HashMap<(SymbolId, EdgeKind), Vec<Edge>>>,
    stats: GraphStats,
    config: GraphConfig,
}

impl CodeGraph {
    pub fn new(config: GraphConfig) -> Self {
        Self {
            files: RwLock::new(FileRegistry::new()),
            nodes: RwLock::new(HashMap::new()),
            edges: RwLock::new(HashMap::new()),
            stats: GraphStats::default(),
            config,
        }
    }

    pub async fn get_references(&self, location: &Location) -> Option<Vec<Location>> {
        if self.check_and_invalidate_if_stale(&location.file).await {
            return None;
        }

        let symbol_id = self.location_to_id(location).await?;
        let edges = self.edges.read().await;
        let refs = edges.get(&(symbol_id, EdgeKind::Reference))?;

        self.stats.cache_hits.fetch_add(1, Ordering::Relaxed);
        Some(self.edges_to_locations(refs).await)
    }

    pub async fn get_incoming_calls(&self, location: &Location) -> Option<Vec<CallInfo>> {
        if self.check_and_invalidate_if_stale(&location.file).await {
            return None;
        }

        let symbol_id = self.location_to_id(location).await?;
        let edges = self.edges.read().await;
        let calls = edges.get(&(symbol_id, EdgeKind::CalledBy))?;

        self.stats.cache_hits.fetch_add(1, Ordering::Relaxed);
        Some(self.edges_to_call_info(calls).await)
    }

    pub async fn get_outgoing_calls(&self, location: &Location) -> Option<Vec<CallInfo>> {
        if self.check_and_invalidate_if_stale(&location.file).await {
            return None;
        }

        let symbol_id = self.location_to_id(location).await?;
        let edges = self.edges.read().await;
        let calls = edges.get(&(symbol_id, EdgeKind::Calls))?;

        self.stats.cache_hits.fetch_add(1, Ordering::Relaxed);
        Some(self.edges_to_call_info(calls).await)
    }

    pub async fn get_definition(&self, location: &Location) -> Option<Location> {
        if self.check_and_invalidate_if_stale(&location.file).await {
            return None;
        }

        let symbol_id = self.location_to_id(location).await?;
        let edges = self.edges.read().await;
        let defs = edges.get(&(symbol_id, EdgeKind::Definition))?;

        self.stats.cache_hits.fetch_add(1, Ordering::Relaxed);
        defs.first().and_then(|e| e.call_site.clone())
    }

    pub async fn get_implementations(&self, location: &Location) -> Option<Vec<Location>> {
        if self.check_and_invalidate_if_stale(&location.file).await {
            return None;
        }

        let symbol_id = self.location_to_id(location).await?;
        let edges = self.edges.read().await;
        let impls = edges.get(&(symbol_id, EdgeKind::Implementation))?;

        self.stats.cache_hits.fetch_add(1, Ordering::Relaxed);
        Some(self.edges_to_locations(impls).await)
    }

    pub async fn get_type_definition(&self, location: &Location) -> Option<Location> {
        if self.check_and_invalidate_if_stale(&location.file).await {
            return None;
        }

        let symbol_id = self.location_to_id(location).await?;
        let edges = self.edges.read().await;
        let defs = edges.get(&(symbol_id, EdgeKind::TypeDefinition))?;

        self.stats.cache_hits.fetch_add(1, Ordering::Relaxed);
        defs.first().and_then(|e| e.call_site.clone())
    }

    pub async fn get_supertypes(&self, location: &Location) -> Option<Vec<TypeHierarchyInfo>> {
        if self.check_and_invalidate_if_stale(&location.file).await {
            return None;
        }

        let symbol_id = self.location_to_id(location).await?;
        let edges = self.edges.read().await;
        let items = edges.get(&(symbol_id, EdgeKind::Supertype))?;

        self.stats.cache_hits.fetch_add(1, Ordering::Relaxed);
        Some(self.edges_to_type_hierarchy(items).await)
    }

    pub async fn get_subtypes(&self, location: &Location) -> Option<Vec<TypeHierarchyInfo>> {
        if self.check_and_invalidate_if_stale(&location.file).await {
            return None;
        }

        let symbol_id = self.location_to_id(location).await?;
        let edges = self.edges.read().await;
        let items = edges.get(&(symbol_id, EdgeKind::Subtype))?;

        self.stats.cache_hits.fetch_add(1, Ordering::Relaxed);
        Some(self.edges_to_type_hierarchy(items).await)
    }

    pub async fn cache_references(
        &self,
        source: &Location,
        source_name: &str,
        source_kind: SymbolKind,
        references: &[Location],
    ) {
        let source_id = self.ensure_symbol(source, source_name, source_kind).await;

        let edges: Vec<Edge> = references
            .iter()
            .map(|loc| Edge {
                target: self.create_temp_id(loc),
                target_name: None,
                target_kind: None,
                call_site: Some(loc.clone()),
            })
            .collect();

        self.store_edges(source_id, EdgeKind::Reference, edges)
            .await;
        self.stats.cache_misses.fetch_add(1, Ordering::Relaxed);
    }

    pub async fn cache_incoming_calls(
        &self,
        target: &Location,
        target_name: &str,
        target_kind: SymbolKind,
        callers: &[CallHierarchyItem],
    ) {
        self.cache_call_hierarchy(
            target,
            target_name,
            target_kind,
            callers,
            EdgeKind::CalledBy,
        )
        .await;
    }

    pub async fn cache_outgoing_calls(
        &self,
        source: &Location,
        source_name: &str,
        source_kind: SymbolKind,
        callees: &[CallHierarchyItem],
    ) {
        self.cache_call_hierarchy(source, source_name, source_kind, callees, EdgeKind::Calls)
            .await;
    }

    pub async fn cache_definition(&self, source: &Location, definition: &Location) {
        let source_id = self.ensure_location_id(source).await;
        let def_id = self.ensure_location_id(definition).await;

        let edge = Edge {
            target: def_id,
            target_name: None,
            target_kind: None,
            call_site: Some(definition.clone()),
        };

        self.store_edges(source_id, EdgeKind::Definition, vec![edge])
            .await;
        self.stats.cache_misses.fetch_add(1, Ordering::Relaxed);
    }

    pub async fn cache_implementations(&self, source: &Location, implementations: &[Location]) {
        let source_id = self.ensure_location_id(source).await;

        let edges: Vec<Edge> = implementations
            .iter()
            .map(|loc| Edge {
                target: self.create_temp_id(loc),
                target_name: None,
                target_kind: None,
                call_site: Some(loc.clone()),
            })
            .collect();

        self.store_edges(source_id, EdgeKind::Implementation, edges)
            .await;
        self.stats.cache_misses.fetch_add(1, Ordering::Relaxed);
    }

    pub async fn cache_type_definition(&self, source: &Location, type_def: &Location) {
        let source_id = self.ensure_location_id(source).await;
        let def_id = self.ensure_location_id(type_def).await;

        let edge = Edge {
            target: def_id,
            target_name: None,
            target_kind: None,
            call_site: Some(type_def.clone()),
        };

        self.store_edges(source_id, EdgeKind::TypeDefinition, vec![edge])
            .await;
        self.stats.cache_misses.fetch_add(1, Ordering::Relaxed);
    }

    pub async fn cache_supertypes(
        &self,
        source: &Location,
        items: &[crate::models::lsp::TypeHierarchyItem],
    ) {
        self.cache_type_hierarchy(source, items, EdgeKind::Supertype)
            .await;
    }

    pub async fn cache_subtypes(
        &self,
        source: &Location,
        items: &[crate::models::lsp::TypeHierarchyItem],
    ) {
        self.cache_type_hierarchy(source, items, EdgeKind::Subtype)
            .await;
    }

    async fn cache_type_hierarchy(
        &self,
        source: &Location,
        items: &[crate::models::lsp::TypeHierarchyItem],
        edge_kind: EdgeKind,
    ) {
        let source_id = self.ensure_location_id(source).await;

        let mut edges = Vec::with_capacity(items.len());
        for item in items {
            let item_id = self
                .ensure_symbol(&item.location, &item.name, item.kind)
                .await;
            edges.push(Edge {
                target: item_id,
                target_name: Some(Arc::from(item.name.as_str())),
                target_kind: Some(item.kind),
                call_site: Some(item.location.clone()),
            });
        }

        self.store_edges(source_id, edge_kind, edges).await;
        self.stats.cache_misses.fetch_add(1, Ordering::Relaxed);
    }

    async fn cache_call_hierarchy(
        &self,
        symbol: &Location,
        symbol_name: &str,
        symbol_kind: SymbolKind,
        items: &[CallHierarchyItem],
        edge_kind: EdgeKind,
    ) {
        let symbol_id = self.ensure_symbol(symbol, symbol_name, symbol_kind).await;

        let mut edges = Vec::with_capacity(items.len());
        for item in items {
            let item_id = self
                .ensure_symbol(&item.location, &item.name, item.kind)
                .await;
            edges.push(Edge {
                target: item_id,
                target_name: Some(Arc::from(item.name.as_str())),
                target_kind: Some(item.kind),
                call_site: item.call_site.clone(),
            });
        }

        self.store_edges(symbol_id, edge_kind, edges).await;
        self.stats.cache_misses.fetch_add(1, Ordering::Relaxed);
    }

    pub async fn invalidate_file(&self, path: &PathBuf) {
        let file_id = {
            let files = self.files.read().await;
            match files.get_id(path) {
                Some(id) => id,
                None => return,
            }
        };

        self.invalidate_file_by_id(file_id).await;
        self.stats.invalidations.fetch_add(1, Ordering::Relaxed);
    }

    async fn is_file_stale(&self, path: &PathBuf) -> bool {
        let stored_mtime = {
            let files = self.files.read().await;
            files
                .get_id(path)
                .and_then(|id| files.get_entry(id))
                .map(|entry| entry.file_mtime)
        };

        match stored_mtime {
            Some(mtime) if mtime > 0 => {
                let current_mtime = super::registry::get_file_mtime(path);
                current_mtime != mtime
            }
            _ => false,
        }
    }

    async fn check_and_invalidate_if_stale(&self, path: &PathBuf) -> bool {
        if self.is_file_stale(path).await {
            self.invalidate_file(path).await;
            true
        } else {
            false
        }
    }

    pub async fn clear(&self) {
        let mut files = self.files.write().await;
        let mut nodes = self.nodes.write().await;
        let mut edges = self.edges.write().await;

        files.clear();
        nodes.clear();
        edges.clear();
    }

    pub async fn cleanup_expired(&self) -> usize {
        let now = Instant::now();
        let ttl = Duration::from_secs(self.config.ttl_secs);

        let expired_files: Vec<u32> = {
            let files = self.files.read().await;
            files
                .entries()
                .filter(|(_, entry)| now.duration_since(entry.last_accessed) > ttl)
                .map(|(id, _)| *id)
                .collect()
        };

        for file_id in &expired_files {
            self.invalidate_file_by_id(*file_id).await;
        }

        expired_files.len()
    }

    pub fn stats(&self) -> CodeGraphStats {
        let hits = self.stats.cache_hits.load(Ordering::Relaxed);
        let misses = self.stats.cache_misses.load(Ordering::Relaxed);
        let total = hits + misses;

        CodeGraphStats {
            cache_hits: hits,
            cache_misses: misses,
            invalidations: self.stats.invalidations.load(Ordering::Relaxed),
            hit_rate: if total > 0 {
                hits as f64 / total as f64
            } else {
                0.0
            },
        }
    }

    pub async fn node_count(&self) -> usize {
        self.nodes.read().await.len()
    }

    pub async fn edge_count(&self) -> usize {
        self.edges.read().await.values().map(|v| v.len()).sum()
    }

    pub async fn file_count(&self) -> usize {
        self.files.read().await.file_count()
    }

    async fn location_to_id(&self, location: &Location) -> Option<SymbolId> {
        let files = self.files.read().await;
        let file_id = files.get_id(&location.file)?;
        Some(SymbolId::new(file_id, location.line, location.column))
    }

    fn create_temp_id(&self, location: &Location) -> SymbolId {
        SymbolId::new(0, location.line, location.column)
    }

    async fn ensure_location_id(&self, location: &Location) -> SymbolId {
        let file_id = {
            let existing = self.files.read().await.get_id(&location.file);
            match existing {
                Some(id) => id,
                None => self.files.write().await.get_or_create_id(&location.file),
            }
        };
        SymbolId::new(file_id, location.line, location.column)
    }

    async fn ensure_symbol(&self, location: &Location, name: &str, kind: SymbolKind) -> SymbolId {
        let file_id = {
            let existing = self.files.read().await.get_id(&location.file);
            match existing {
                Some(id) => id,
                None => self.files.write().await.get_or_create_id(&location.file),
            }
        };

        let symbol_id = SymbolId::new(file_id, location.line, location.column);

        if self.nodes.read().await.contains_key(&symbol_id) {
            return symbol_id;
        }

        self.evict_if_over_capacity().await;

        let needs_track = {
            let mut nodes = self.nodes.write().await;
            if let std::collections::hash_map::Entry::Vacant(e) = nodes.entry(symbol_id) {
                e.insert(SymbolNode {
                    name: Arc::from(name),
                    kind,
                });
                true
            } else {
                false
            }
        };

        if needs_track {
            let mut files = self.files.write().await;
            if let Some(entry) = files.get_entry_mut(file_id) {
                entry.symbols.push(symbol_id);
                entry.last_accessed = Instant::now();
                entry.file_mtime = super::registry::get_file_mtime(&entry.path.clone());
            }
        }

        symbol_id
    }

    async fn evict_if_over_capacity(&self) {
        let current_count = self.nodes.read().await.len();
        if current_count < self.config.max_nodes {
            return;
        }

        // Find the least recently accessed file
        let lru_file_id = {
            let files = self.files.read().await;
            files
                .entries()
                .filter(|(_, entry)| !entry.symbols.is_empty())
                .min_by_key(|(_, entry)| entry.last_accessed)
                .map(|(id, _)| *id)
        };

        if let Some(file_id) = lru_file_id {
            self.invalidate_file_by_id(file_id).await;
        }
    }

    async fn store_edges(&self, source: SymbolId, kind: EdgeKind, edges: Vec<Edge>) {
        let edges = if edges.len() > self.config.max_edges_per_symbol {
            edges
                .into_iter()
                .take(self.config.max_edges_per_symbol)
                .collect()
        } else {
            edges
        };

        let mut edge_map = self.edges.write().await;
        edge_map.insert((source, kind), edges);
    }

    async fn edges_to_locations(&self, edges: &[Edge]) -> Vec<Location> {
        let files = self.files.read().await;

        edges
            .iter()
            .filter_map(|edge| {
                edge.call_site.clone().or_else(|| {
                    let file_id = edge.target.file_id();
                    let path = files.get_path(file_id)?;
                    Some(Location::point(
                        path.clone(),
                        edge.target.line(),
                        edge.target.column(),
                    ))
                })
            })
            .collect()
    }

    async fn edges_to_call_info(&self, edges: &[Edge]) -> Vec<CallInfo> {
        let nodes = self.nodes.read().await;
        let files = self.files.read().await;

        edges
            .iter()
            .filter_map(|edge| {
                let file_id = edge.target.file_id();
                let path = files.get_path(file_id)?;

                let (name, kind) = if let Some(node) = nodes.get(&edge.target) {
                    (node.name.to_string(), node.kind)
                } else {
                    (
                        edge.target_name
                            .as_ref()
                            .map(|s| s.to_string())
                            .unwrap_or_else(|| "unknown".to_string()),
                        edge.target_kind.unwrap_or(SymbolKind::Function),
                    )
                };

                Some(CallInfo {
                    name,
                    kind,
                    location: Location::point(
                        path.clone(),
                        edge.target.line(),
                        edge.target.column(),
                    ),
                    call_site: edge.call_site.clone(),
                })
            })
            .collect()
    }

    async fn edges_to_type_hierarchy(&self, edges: &[Edge]) -> Vec<TypeHierarchyInfo> {
        let nodes = self.nodes.read().await;
        let files = self.files.read().await;

        edges
            .iter()
            .filter_map(|edge| {
                let file_id = edge.target.file_id();
                let path = files.get_path(file_id)?;

                let (name, kind) = if let Some(node) = nodes.get(&edge.target) {
                    (node.name.to_string(), node.kind)
                } else {
                    (
                        edge.target_name
                            .as_ref()
                            .map(|s| s.to_string())
                            .unwrap_or_else(|| "unknown".to_string()),
                        edge.target_kind.unwrap_or(SymbolKind::Class),
                    )
                };

                Some(TypeHierarchyInfo {
                    name,
                    kind,
                    location: Location::point(
                        path.clone(),
                        edge.target.line(),
                        edge.target.column(),
                    ),
                    detail: None,
                })
            })
            .collect()
    }

    async fn invalidate_file_by_id(&self, file_id: u32) {
        let symbols_to_remove: Vec<SymbolId> = {
            let files = self.files.read().await;
            files
                .get_entry(file_id)
                .map(|e| e.symbols.clone())
                .unwrap_or_default()
        };

        if symbols_to_remove.is_empty() {
            return;
        }

        {
            let mut nodes = self.nodes.write().await;
            for symbol_id in &symbols_to_remove {
                nodes.remove(symbol_id);
            }
        }

        {
            let mut edges = self.edges.write().await;
            for symbol_id in &symbols_to_remove {
                for kind in [
                    EdgeKind::Reference,
                    EdgeKind::Calls,
                    EdgeKind::CalledBy,
                    EdgeKind::Definition,
                    EdgeKind::TypeDefinition,
                    EdgeKind::Implementation,
                    EdgeKind::Supertype,
                    EdgeKind::Subtype,
                ] {
                    edges.remove(&(*symbol_id, kind));
                }
            }
        }

        {
            let mut files = self.files.write().await;
            if let Some(entry) = files.get_entry_mut(file_id) {
                entry.symbols.clear();
                entry.file_mtime = 0;
                entry.last_accessed = Instant::now();
            }
        }
    }
}

impl Default for CodeGraph {
    fn default() -> Self {
        Self::new(GraphConfig::default())
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CodeGraphStats {
    pub cache_hits: u64,
    pub cache_misses: u64,
    pub invalidations: u64,
    pub hit_rate: f64,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_location(file: &str, line: u32, col: u32) -> Location {
        Location::point(PathBuf::from(file), line, col)
    }

    #[tokio::test]
    async fn test_cache_and_retrieve_references() {
        let graph = CodeGraph::default();
        let source = test_location("/test/main.rs", 10, 5);
        let refs = vec![
            test_location("/test/lib.rs", 20, 10),
            test_location("/test/util.rs", 30, 15),
        ];

        graph
            .cache_references(&source, "my_function", SymbolKind::Function, &refs)
            .await;

        let cached = graph.get_references(&source).await;
        assert!(cached.is_some());
        assert_eq!(cached.unwrap().len(), 2);
    }

    #[tokio::test]
    async fn test_cache_miss_returns_none() {
        let graph = CodeGraph::default();
        let location = test_location("/test/main.rs", 10, 5);

        let cached = graph.get_references(&location).await;
        assert!(cached.is_none());
    }

    #[tokio::test]
    async fn test_invalidate_file() {
        let graph = CodeGraph::default();
        let source = test_location("/test/main.rs", 10, 5);
        let refs = vec![test_location("/test/lib.rs", 20, 10)];

        graph
            .cache_references(&source, "my_function", SymbolKind::Function, &refs)
            .await;

        graph.invalidate_file(&PathBuf::from("/test/main.rs")).await;

        let cached = graph.get_references(&source).await;
        assert!(cached.is_none());
    }

    #[tokio::test]
    async fn test_stats() {
        let graph = CodeGraph::default();
        let source = test_location("/test/main.rs", 10, 5);
        let refs = vec![test_location("/test/lib.rs", 20, 10)];

        graph
            .cache_references(&source, "my_function", SymbolKind::Function, &refs)
            .await;

        let _ = graph.get_references(&source).await;
        let _ = graph.get_references(&source).await;

        let stats = graph.stats();
        assert_eq!(stats.cache_misses, 1);
        assert_eq!(stats.cache_hits, 2);
    }

    #[tokio::test]
    async fn test_cache_and_retrieve_definition() {
        let graph = CodeGraph::default();
        let source = test_location("/test/main.rs", 10, 5);
        let definition = test_location("/test/lib.rs", 20, 10);

        graph.cache_definition(&source, &definition).await;

        let cached = graph.get_definition(&source).await;
        assert!(cached.is_some());
        assert_eq!(cached.unwrap().line, 20);
    }

    #[tokio::test]
    async fn test_cache_and_retrieve_implementations() {
        let graph = CodeGraph::default();
        let source = test_location("/test/trait.rs", 10, 5);
        let impls = vec![
            test_location("/test/impl1.rs", 20, 10),
            test_location("/test/impl2.rs", 30, 15),
        ];

        graph.cache_implementations(&source, &impls).await;

        let cached = graph.get_implementations(&source).await;
        assert!(cached.is_some());
        assert_eq!(cached.unwrap().len(), 2);
    }

    #[tokio::test]
    async fn test_max_nodes_eviction() {
        let config = GraphConfig {
            max_nodes: 2,
            max_edges_per_symbol: 100,
            ttl_secs: 600,
        };
        let graph = CodeGraph::new(config);

        // Add first symbol (file1)
        let source1 = test_location("/test/file1.rs", 10, 5);
        graph
            .cache_references(&source1, "fn1", SymbolKind::Function, &[])
            .await;
        assert_eq!(graph.node_count().await, 1);

        // Add second symbol (file2)
        let source2 = test_location("/test/file2.rs", 20, 5);
        graph
            .cache_references(&source2, "fn2", SymbolKind::Function, &[])
            .await;
        assert_eq!(graph.node_count().await, 2);

        // Add third symbol - should trigger eviction of LRU file (file1)
        let source3 = test_location("/test/file3.rs", 30, 5);
        graph
            .cache_references(&source3, "fn3", SymbolKind::Function, &[])
            .await;

        // Node count should not exceed max_nodes
        assert!(graph.node_count().await <= 2);

        // First file should be evicted (LRU)
        assert!(graph.get_references(&source1).await.is_none());
    }
}
