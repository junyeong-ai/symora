//! Direction-parameterized call-graph traversal — the single BFS core
//! shared by blast-radius impact analysis (incoming/upward) and downward
//! reachability/path queries (outgoing). Walks the LSP call hierarchy up to
//! `max_depth`, parallelising each frontier's round-trips, with a per-node
//! fan-out cap. It keeps a predecessor map so a path between two nodes can be
//! reconstructed, and surfaces the same honest lower-bound markers
//! (`max_depth_reached`, `truncated`, `indexing`) the impact surface already
//! relies on — it never synthesises an edge the language server did not
//! return.
//!
//! Determinism: each frontier and each node's neighbours are sorted by
//! `(file, line, column)` before traversal, so the discovered levels, the
//! first-writer-wins predecessor edges, and the reconstructed path are
//! identical for a fixed source state — daemon and direct agree.

use std::collections::{HashMap, HashSet};
use std::path::PathBuf;

use futures::future::join_all;

use crate::constants::defaults::{BLAST_RADIUS_MAX_CALLERS_PER_NODE, IMPACT_DEFAULT_DEPTH};
use crate::models::lsp::{CallHierarchyItem, IndexingDegradation};
use crate::services::lsp::LspService;

/// Traversal direction over the LSP call hierarchy.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Direction {
    /// `incoming_calls` — who calls the anchor (upward, toward callers).
    Incoming,
    /// `outgoing_calls` — what the anchor calls (downward, toward callees).
    Outgoing,
}

/// Bounds on a walk: how deep, and how many neighbours per node before the
/// fan-out cap trips `truncated`. `max_neighbors_per_node` is named for the
/// direction-neutral core; its default originates in the blast-radius tuning.
#[derive(Debug, Clone, Copy)]
pub struct WalkConfig {
    pub max_depth: u32,
    pub max_neighbors_per_node: usize,
}

impl Default for WalkConfig {
    fn default() -> Self {
        Self {
            max_depth: IMPACT_DEFAULT_DEPTH,
            max_neighbors_per_node: BLAST_RADIUS_MAX_CALLERS_PER_NODE,
        }
    }
}

/// A node's stable key in the walk: its declaration position.
pub type NodeKey = (PathBuf, u32, u32);

/// The result of a direction-parameterized call-graph walk. Carries the
/// newly-discovered items per depth (so a consumer can classify and count
/// them), a predecessor map for path reconstruction, and the honest
/// lower-bound markers.
#[derive(Debug, Clone)]
pub struct CallGraphWalk {
    /// Newly-visited items at each depth; `levels[0]` is depth 1. One entry
    /// per depth actually traversed (possibly empty), so `levels.len()` is the
    /// depth reached.
    pub levels: Vec<Vec<CallHierarchyItem>>,
    /// First-writer-wins edge from each discovered node back to the node it
    /// was reached through. The anchor has no entry.
    pub predecessor: HashMap<NodeKey, NodeKey>,
    /// Every node reached, including the anchor.
    pub visited: HashSet<NodeKey>,
    /// The final depth still had unexplored neighbours — the walk stopped at
    /// the cap, not from exhaustion, so the graph is a lower bound.
    pub max_depth_reached: bool,
    /// At least one node's neighbour list was cut at `max_neighbors_per_node`,
    /// so the discovered set is a lower bound.
    pub truncated: bool,
    /// Present when any hop ran under degraded workspace indexing — every
    /// count is then a lower bound.
    pub indexing: Option<IndexingDegradation>,
}

impl CallGraphWalk {
    /// Depth actually reached (number of levels traversed).
    pub fn depth_reached(&self) -> u32 {
        self.levels.len() as u32
    }

    /// Reconstruct the shortest discovered path from `anchor` to `target` as
    /// the ordered node keys from the first hop through `target` (the anchor
    /// itself is excluded). Returns `None` when `target` was not reached.
    ///
    /// The path is shortest because BFS assigns each node its first
    /// (shallowest) predecessor, and deterministic because the frontier and
    /// neighbour orders are sorted during the walk. A visited node always has
    /// a predecessor chain back to the anchor; if that chain cannot be
    /// followed (it never should), `None` is returned rather than a partial
    /// path — a fabricated chain would be plausible-but-wrong.
    pub fn path_to(&self, anchor: &NodeKey, target: &NodeKey) -> Option<Vec<NodeKey>> {
        if target == anchor || !self.visited.contains(target) {
            return None;
        }
        let mut chain = vec![target.clone()];
        let mut current = target.clone();
        while let Some(prev) = self.predecessor.get(&current) {
            if prev == anchor {
                chain.reverse();
                return Some(chain);
            }
            chain.push(prev.clone());
            current = prev.clone();
        }
        None
    }
}

fn key_of(item: &CallHierarchyItem) -> NodeKey {
    (
        item.location.file.clone(),
        item.location.line,
        item.location.column,
    )
}

/// Walk the call hierarchy from `anchor` in `direction`, breadth-first, up to
/// `cfg.max_depth`. Returns the discovered graph with honest lower-bound
/// markers — never an over-approximation.
pub async fn walk(
    lsp: &dyn LspService,
    anchor: NodeKey,
    direction: Direction,
    cfg: &WalkConfig,
) -> CallGraphWalk {
    let max_depth = cfg.max_depth.max(1);
    let mut visited: HashSet<NodeKey> = HashSet::new();
    visited.insert(anchor.clone());

    let mut predecessor: HashMap<NodeKey, NodeKey> = HashMap::new();
    let mut frontier: Vec<NodeKey> = vec![anchor];
    let mut levels: Vec<Vec<CallHierarchyItem>> = Vec::with_capacity(max_depth as usize);
    let mut max_depth_reached = false;
    let mut truncated = false;
    // Aggregates the computation-time snapshot of every hop: if ANY hop ran
    // under degraded indexing the whole walk is a lower bound, and quiescence
    // landing mid-walk must not strip that.
    let mut indexing: Option<IndexingDegradation> = None;

    for depth in 1..=max_depth {
        // Deterministic frontier order so the predecessor first-writer-wins
        // assignment and the reconstructed path are reproducible.
        frontier.sort();

        let neighbors_per_node = join_all(frontier.iter().map(|(f, l, c)| {
            let (f, l, c) = (f.clone(), *l, *c);
            async move {
                match direction {
                    Direction::Incoming => lsp.incoming_calls(&f, l, c).await,
                    Direction::Outgoing => lsp.outgoing_calls(&f, l, c).await,
                }
                .ok()
            }
        }))
        .await;

        let mut level_items: Vec<CallHierarchyItem> = Vec::new();
        let mut next_frontier: Vec<NodeKey> = Vec::new();

        for (parent, neighbors) in frontier.iter().zip(neighbors_per_node) {
            let mut items = match neighbors {
                Some(indexed) => {
                    if indexing.is_none() {
                        indexing = indexed.indexing;
                    }
                    indexed.data
                }
                None => Vec::new(),
            };
            if items.len() > cfg.max_neighbors_per_node {
                truncated = true;
            }
            // Sort before the cap so which neighbours survive truncation — and
            // their predecessor edges — are stable across runs.
            items.sort_by_key(key_of);
            for item in items.into_iter().take(cfg.max_neighbors_per_node) {
                let key = key_of(&item);
                if !visited.insert(key.clone()) {
                    continue;
                }
                predecessor.insert(key.clone(), parent.clone());
                if depth < max_depth {
                    next_frontier.push(key);
                }
                level_items.push(item);
            }
        }

        let level_count = level_items.len();
        levels.push(level_items);

        // A node discovered at the final depth still has unexplored
        // neighbours — the walk stopped at the cap, not from exhaustion. (The
        // frontier guard above never queues final-depth nodes, so this is
        // decided from the discovered count, not the frontier.)
        if depth == max_depth {
            max_depth_reached = level_count > 0;
            break;
        }
        if next_frontier.is_empty() {
            break;
        }
        frontier = next_frontier;
    }

    CallGraphWalk {
        levels,
        predecessor,
        visited,
        max_depth_reached,
        truncated,
        indexing,
    }
}

/// Shared LSP test double for call-graph walks: answers `incoming_calls` /
/// `outgoing_calls` / `find_implementations` from fixed maps and panics on
/// any other method, which `walk` and `blast_radius::compute` must never
/// reach. Lives outside `mod tests` so both this module's and
/// `blast_radius`'s test suites use one definition.
#[cfg(test)]
pub(crate) mod test_support {
    use super::*;
    use std::collections::HashMap;
    use std::path::Path;

    use async_trait::async_trait;

    use crate::error::LspError;
    use crate::models::lsp::{
        ApplyActionResult, CodeAction, CodeLens, FindSymbolsOptions, FoldingRange, HoverInfo,
        Indexed, InlayHint, PrepareRenameResult, Range, RenameResult, SelectionRange, ServerStatus,
        SignatureHelp, TextEdit, TypeHierarchyItem,
    };
    use crate::models::symbol::{Language, Location, Symbol, SymbolKind};

    /// Maps a `(line, column)` position to its neighbours in each direction,
    /// and answers the dynamic-dispatch probe with a fixed implementation set
    /// or a synthesised JSON-RPC error (`Err(code)`).
    pub(crate) struct CallGraphStub {
        pub incoming: HashMap<(u32, u32), Vec<CallHierarchyItem>>,
        pub outgoing: HashMap<(u32, u32), Vec<CallHierarchyItem>>,
        pub implementations: Result<Vec<Location>, i32>,
    }

    impl CallGraphStub {
        /// An outgoing-only stub (the reachability/path shape). The
        /// incoming-only shape is built as a struct literal by the
        /// blast-radius suite, which also sets `implementations`.
        pub(crate) fn outgoing(map: HashMap<(u32, u32), Vec<CallHierarchyItem>>) -> Self {
            Self {
                incoming: HashMap::new(),
                outgoing: map,
                implementations: Ok(vec![]),
            }
        }
    }

    /// A caller/callee item at `src/lib.rs:line:1`.
    pub(crate) fn node(line: u32) -> CallHierarchyItem {
        CallHierarchyItem {
            name: format!("node_{line}"),
            kind: SymbolKind::Function,
            location: Location::point(PathBuf::from("src/lib.rs"), line, 1),
            call_site: None,
        }
    }

    #[async_trait]
    impl LspService for CallGraphStub {
        async fn incoming_calls(
            &self,
            _file: &Path,
            line: u32,
            column: u32,
        ) -> Result<Indexed<Vec<CallHierarchyItem>>, LspError> {
            Ok(Indexed::complete(
                self.incoming
                    .get(&(line, column))
                    .cloned()
                    .unwrap_or_default(),
            ))
        }

        async fn outgoing_calls(
            &self,
            _file: &Path,
            line: u32,
            column: u32,
        ) -> Result<Indexed<Vec<CallHierarchyItem>>, LspError> {
            Ok(Indexed::complete(
                self.outgoing
                    .get(&(line, column))
                    .cloned()
                    .unwrap_or_default(),
            ))
        }

        async fn find_implementations(
            &self,
            _file: &Path,
            _line: u32,
            _column: u32,
        ) -> Result<Indexed<Vec<Location>>, LspError> {
            self.implementations
                .clone()
                .map(Indexed::complete)
                .map_err(|code| LspError::server_error_friendly(code, "stub error".to_string()))
        }

        async fn find_symbols(
            &self,
            _file: &Path,
            _options: FindSymbolsOptions,
        ) -> Result<Vec<Symbol>, LspError> {
            unreachable!()
        }
        async fn workspace_symbols(
            &self,
            _query: &str,
            _language: Language,
        ) -> Result<Indexed<Vec<Symbol>>, LspError> {
            unreachable!()
        }
        async fn find_references(
            &self,
            _file: &Path,
            _line: u32,
            _column: u32,
        ) -> Result<Indexed<Vec<Location>>, LspError> {
            unreachable!()
        }
        async fn goto_definition(
            &self,
            _file: &Path,
            _line: u32,
            _column: u32,
        ) -> Result<Option<Location>, LspError> {
            unreachable!()
        }
        async fn goto_type_definition(
            &self,
            _file: &Path,
            _line: u32,
            _column: u32,
        ) -> Result<Option<Location>, LspError> {
            unreachable!()
        }
        async fn hover(
            &self,
            _file: &Path,
            _line: u32,
            _column: u32,
        ) -> Result<Option<HoverInfo>, LspError> {
            unreachable!()
        }
        async fn signature_help(
            &self,
            _file: &Path,
            _line: u32,
            _column: u32,
        ) -> Result<Option<SignatureHelp>, LspError> {
            unreachable!()
        }
        async fn diagnostics(
            &self,
            _file: &Path,
        ) -> Result<crate::models::diagnostic::DiagnosticsReport, LspError> {
            unreachable!()
        }
        async fn prepare_rename(
            &self,
            _file: &Path,
            _line: u32,
            _column: u32,
        ) -> Result<Option<PrepareRenameResult>, LspError> {
            unreachable!()
        }
        async fn rename(
            &self,
            _file: &Path,
            _line: u32,
            _column: u32,
            _new_name: &str,
        ) -> Result<RenameResult, LspError> {
            unreachable!()
        }
        async fn supertypes(
            &self,
            _file: &Path,
            _line: u32,
            _column: u32,
        ) -> Result<Indexed<Vec<TypeHierarchyItem>>, LspError> {
            unreachable!()
        }
        async fn subtypes(
            &self,
            _file: &Path,
            _line: u32,
            _column: u32,
        ) -> Result<Indexed<Vec<TypeHierarchyItem>>, LspError> {
            unreachable!()
        }
        async fn inlay_hints(
            &self,
            _file: &Path,
            _range: Range,
        ) -> Result<Vec<InlayHint>, LspError> {
            unreachable!()
        }
        async fn folding_ranges(&self, _file: &Path) -> Result<Vec<FoldingRange>, LspError> {
            unreachable!()
        }
        async fn selection_ranges(
            &self,
            _file: &Path,
            _positions: Vec<(u32, u32)>,
        ) -> Result<Vec<SelectionRange>, LspError> {
            unreachable!()
        }
        async fn code_lenses(&self, _file: &Path) -> Result<Vec<CodeLens>, LspError> {
            unreachable!()
        }
        async fn code_actions(
            &self,
            _file: &Path,
            _line: u32,
            _column: u32,
        ) -> Result<Vec<CodeAction>, LspError> {
            unreachable!()
        }
        async fn apply_code_action(
            &self,
            _file: &Path,
            _action: &CodeAction,
        ) -> Result<ApplyActionResult, LspError> {
            unreachable!()
        }
        async fn format(&self, _file: &Path) -> Result<Vec<TextEdit>, LspError> {
            unreachable!()
        }
        async fn is_available(&self, _language: Language) -> bool {
            unreachable!()
        }
        async fn server_status(&self, _language: Language) -> ServerStatus {
            unreachable!()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::test_support::{CallGraphStub, node};
    use super::*;
    use std::collections::HashMap;

    fn anchor() -> NodeKey {
        (PathBuf::from("src/lib.rs"), 10, 5)
    }

    fn walk_outgoing(
        outgoing: HashMap<(u32, u32), Vec<CallHierarchyItem>>,
        cfg: WalkConfig,
    ) -> CallGraphWalk {
        let stub = CallGraphStub::outgoing(outgoing);
        tokio_test::block_on(walk(&stub, anchor(), Direction::Outgoing, &cfg))
    }

    #[test]
    fn direction_selects_the_hop_method() {
        // The same key resolves through outgoing_calls under Outgoing; the
        // incoming map (which would answer differently) is never consulted.
        let mut outgoing = HashMap::new();
        outgoing.insert((10, 5), vec![node(20)]);
        let stub = CallGraphStub {
            incoming: {
                let mut m = HashMap::new();
                m.insert((10, 5), vec![node(99)]);
                m
            },
            outgoing,
            implementations: Ok(vec![]),
        };
        let out = tokio_test::block_on(walk(
            &stub,
            anchor(),
            Direction::Outgoing,
            &WalkConfig::default(),
        ));
        assert_eq!(out.levels[0].len(), 1);
        assert_eq!(out.levels[0][0].location.line, 20);

        let inc = tokio_test::block_on(walk(
            &stub,
            anchor(),
            Direction::Incoming,
            &WalkConfig::default(),
        ));
        assert_eq!(inc.levels[0][0].location.line, 99);
    }

    #[test]
    fn fan_out_over_cap_sets_truncated() {
        let mut outgoing = HashMap::new();
        outgoing.insert((10, 5), vec![node(20), node(30), node(40)]);
        let out = walk_outgoing(
            outgoing,
            WalkConfig {
                max_depth: 1,
                max_neighbors_per_node: 2,
            },
        );
        assert_eq!(out.levels[0].len(), 2);
        assert!(out.truncated);
        // Deterministic survivors: lowest keys kept (lines 20, 30).
        let kept: Vec<u32> = out.levels[0].iter().map(|i| i.location.line).collect();
        assert_eq!(kept, vec![20, 30]);
    }

    #[test]
    fn bounded_not_reached_vs_clean_exhaustion() {
        // A chain anchor->20->30; depth 1 stops bounded (20 has more to give).
        let mut outgoing = HashMap::new();
        outgoing.insert((10, 5), vec![node(20)]);
        outgoing.insert((20, 1), vec![node(30)]);
        let bounded = walk_outgoing(
            outgoing.clone(),
            WalkConfig {
                max_depth: 1,
                max_neighbors_per_node: 8,
            },
        );
        assert!(bounded.max_depth_reached, "depth cap hit with a node found");

        // Depth 3 over the same 2-hop chain exhausts before the cap.
        let exhausted = walk_outgoing(
            outgoing,
            WalkConfig {
                max_depth: 3,
                max_neighbors_per_node: 8,
            },
        );
        assert!(
            !exhausted.max_depth_reached,
            "frontier emptied before the cap"
        );
        assert_eq!(exhausted.depth_reached(), 3); // levels: d1=[20], d2=[30], d3=[]
    }

    #[test]
    fn path_reconstruction_is_shortest_and_deterministic() {
        // Two routes to node 40: anchor->20->40 (len 2) and anchor->25->35->40
        // (len 3). BFS must report the length-2 chain, identically each run.
        let mut outgoing = HashMap::new();
        outgoing.insert((10, 5), vec![node(20), node(25)]);
        outgoing.insert((20, 1), vec![node(40)]);
        outgoing.insert((25, 1), vec![node(35)]);
        outgoing.insert((35, 1), vec![node(40)]);
        let target: NodeKey = (PathBuf::from("src/lib.rs"), 40, 1);

        let mut seen = std::collections::HashSet::new();
        for _ in 0..50 {
            let out = walk_outgoing(
                outgoing.clone(),
                WalkConfig {
                    max_depth: 5,
                    max_neighbors_per_node: 8,
                },
            );
            let chain = out.path_to(&anchor(), &target).expect("target reachable");
            let lines: Vec<u32> = chain.iter().map(|(_, l, _)| *l).collect();
            seen.insert(lines);
        }
        assert_eq!(seen.len(), 1, "path is deterministic across runs");
        assert_eq!(seen.into_iter().next().unwrap(), vec![20, 40]);
    }

    #[test]
    fn path_to_unreached_target_is_none() {
        let mut outgoing = HashMap::new();
        outgoing.insert((10, 5), vec![node(20)]);
        let out = walk_outgoing(
            outgoing,
            WalkConfig {
                max_depth: 3,
                max_neighbors_per_node: 8,
            },
        );
        let missing: NodeKey = (PathBuf::from("src/lib.rs"), 999, 1);
        assert!(out.path_to(&anchor(), &missing).is_none());
        // The anchor itself is never a path.
        assert!(out.path_to(&anchor(), &anchor()).is_none());
    }
}
