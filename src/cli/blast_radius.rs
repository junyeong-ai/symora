//! Bounded traversal of the language server's static caller graph.

use std::path::Path;

use serde::Serialize;

use crate::cli::call_graph::{self, Direction, WalkConfig};
use crate::error::LspError;
use crate::models::symbol::SymbolKind;
use crate::services::TestScope;
use crate::services::lsp::LspService;

#[derive(Debug, Clone, Serialize)]
pub struct BlastRadius {
    pub direct_callers: usize,
    pub transitive_callers: usize,
    pub depth: u32,
    /// The walk stopped at `depth` with callers there it did not look past,
    /// so `transitive_callers` is a lower bound. `direct_callers` does not
    /// depend on depth.
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub max_depth_reached: bool,
    /// True when at least one node's caller list was cut at
    /// `max_neighbors_per_node` — the counts below are then a lower bound,
    /// not a complete enumeration.
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub callers_truncated: bool,
    /// Present only when the caller graph was walked under degraded
    /// workspace indexing — every count is then a lower bound.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub indexing: Option<crate::models::lsp::IndexingDegradation>,
    /// True when at least one hop's call-hierarchy request failed and was
    /// treated as an empty caller set — the counts are then a lower bound.
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub incomplete: bool,
    /// Present only when the anchor is dynamically dispatched (a
    /// trait/interface method, or the interface itself). The call-hierarchy
    /// counts then exclude callers reached through implementations, so they
    /// are a lower bound. Absence means the graph is complete for this
    /// anchor.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub dynamic_dispatch: Option<DynamicDispatch>,
    pub callers_by_depth: Vec<DepthBucket>,
    /// What share of the callers above are test code. Absent when there are
    /// none: a share of nothing is not zero, and `impact`'s `coverage` counts
    /// the test REFERENCES to this symbol, which a caller graph does not see —
    /// a type is referenced but never called.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub test_caller_ratio: Option<f64>,
}

#[derive(Debug, Clone, Serialize)]
pub struct DepthBucket {
    pub depth: u32,
    pub count: usize,
    pub test: usize,
    pub prod: usize,
}

/// Why a dynamically-dispatched anchor's caller graph may be incomplete.
/// The transitive count is a lower bound in every variant.
#[derive(Debug, Clone, Copy, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum DispatchStatus {
    /// Implementations/overrides exist; their transitive callers are not
    /// folded into the counts, so the graph is a lower bound at the anchor.
    Incomplete,
    /// The anchor is an interface but the language server cannot resolve
    /// implementations, so dynamic callers can't be determined at all.
    Unavailable,
}

/// Dynamic-dispatch disclosure for a blast radius. Surfaced only when the
/// anchor is dynamically dispatched — extends the same honesty machinery as
/// `indexing`/`callers_truncated`: it states that the graph is a lower
/// bound and why, never synthesizing the missing callers.
#[derive(Debug, Clone, Copy, Serialize)]
pub struct DynamicDispatch {
    pub status: DispatchStatus,
    /// Implementations/overrides discovered (0 when `unavailable`).
    pub implementations: usize,
}

pub async fn compute(
    lsp: &dyn LspService,
    file: &Path,
    line: u32,
    column: u32,
    anchor_kind: Option<SymbolKind>,
    test_scope: &TestScope,
    cfg: &WalkConfig,
) -> Result<BlastRadius, LspError> {
    // Blast radius is the upward (incoming) call graph. The traversal,
    // fan-out cap, depth/exhaustion markers, and degradation aggregation all
    // live in the shared `call_graph::walk` core; this function adds only the
    // projection (test/prod classification and
    // dynamic-dispatch disclosure).
    let walk = call_graph::walk(
        lsp,
        (file.to_path_buf(), line, column),
        Direction::Incoming,
        cfg,
    )
    .await;

    // A caller is classified where it is DECLARED, so a test living inside
    // the production file it exercises is counted as coverage rather than as
    // one more production dependency.
    let mut classifier = test_scope.classifier();
    let buckets: Vec<DepthBucket> = walk
        .levels
        .iter()
        .enumerate()
        .map(|(i, items)| {
            let test = items
                .iter()
                .filter(|c| classifier.is_test_code(&c.location.file, c.location.line))
                .count();
            DepthBucket {
                depth: (i + 1) as u32,
                count: items.len(),
                test,
                prod: items.len() - test,
            }
        })
        .collect();

    let dynamic_dispatch = detect_dynamic_dispatch(lsp, file, line, column, anchor_kind).await;

    let direct_callers = buckets.first().map(|b| b.count).unwrap_or(0);
    let transitive_callers: usize = buckets.iter().map(|b| b.count).sum();
    let total_test: usize = buckets.iter().map(|b| b.test).sum();
    let test_ratio =
        (transitive_callers > 0).then(|| coarse(total_test as f64 / transitive_callers as f64));
    let depth_reached = buckets.last().map(|b| b.depth).unwrap_or(0);

    Ok(BlastRadius {
        direct_callers,
        transitive_callers,
        depth: depth_reached,
        max_depth_reached: walk.max_depth_reached,
        callers_truncated: walk.truncated,
        indexing: walk.indexing,
        incomplete: walk.incomplete,
        dynamic_dispatch,
        callers_by_depth: buckets,
        test_caller_ratio: test_ratio,
    })
}

/// Disclose whether the anchor is dynamically dispatched, so the caller
/// graph's incompleteness is visible rather than silently presented as
/// authoritative (invariant #4). LSP truth only — no name-matching: a
/// non-empty `find_implementations` is the sole positive signal.
///
/// Gated on `SymbolKind` so a non-overridable anchor (a free function, a
/// type, a field) costs no round-trip. For an interface anchor whose server
/// lacks the capability we say so (`unavailable`); for an ordinary method
/// on such a server we stay silent, because we cannot tell it is virtual
/// and a blanket marker on every method would be noise, not signal.
async fn detect_dynamic_dispatch(
    lsp: &dyn LspService,
    file: &Path,
    line: u32,
    column: u32,
    anchor_kind: Option<SymbolKind>,
) -> Option<DynamicDispatch> {
    let kind = anchor_kind?;
    let structural_class = kind == SymbolKind::Class
        && crate::models::symbol::Language::from_path(file)
            == crate::models::symbol::Language::Python;
    if !matches!(kind, SymbolKind::Method | SymbolKind::Interface) && !structural_class {
        return None;
    }
    match lsp.find_implementations(file, line, column).await {
        Ok(impls) => {
            // A server may list a method nothing overrides as its own
            // implementation (tsserver does). That entry is the anchor, not a
            // second target a call could dispatch to.
            let implementations = impls
                .data
                .iter()
                .filter(|location| !location.covers(file, line, column))
                .count();
            (implementations > 0 || impls.indexing.is_some()).then_some(DynamicDispatch {
                status: if implementations > 0 {
                    DispatchStatus::Incomplete
                } else {
                    DispatchStatus::Unavailable
                },
                implementations,
            })
        }
        // Unsupported and failed lookups cannot establish implementation completeness.
        Err(e)
            if (matches!(kind, SymbolKind::Interface | SymbolKind::Method) || structural_class)
                && e.is_unsupported() =>
        {
            Some(DynamicDispatch {
                status: DispatchStatus::Unavailable,
                implementations: 0,
            })
        }
        Err(_) => Some(DynamicDispatch {
            status: DispatchStatus::Unavailable,
            implementations: 0,
        }),
    }
}

fn coarse(value: f64) -> f64 {
    (value * 100.0).round() / 100.0
}

#[cfg(test)]
mod tests {
    use super::*;

    use std::collections::HashMap;
    use std::path::PathBuf;

    use crate::cli::call_graph::test_support::{CallGraphStub, node as caller};
    use crate::models::lsp::CallHierarchyItem;
    use crate::models::symbol::Location;

    fn compute_with(
        incoming: HashMap<(u32, u32), Vec<CallHierarchyItem>>,
        cfg: WalkConfig,
    ) -> BlastRadius {
        compute_for(incoming, Ok(vec![]), None, cfg)
    }

    fn compute_for(
        incoming: HashMap<(u32, u32), Vec<CallHierarchyItem>>,
        implementations: Result<Vec<Location>, i32>,
        anchor_kind: Option<SymbolKind>,
        cfg: WalkConfig,
    ) -> BlastRadius {
        let stub = CallGraphStub {
            incoming,
            outgoing: HashMap::new(),
            implementations,
            errors: std::collections::HashSet::new(),
        };
        let matcher = TestScope::default();
        tokio_test::block_on(compute(
            &stub,
            Path::new("src/lib.rs"),
            10,
            5,
            anchor_kind,
            &matcher,
            &cfg,
        ))
        .expect("blast radius computes")
    }

    #[test]
    fn callers_within_cap_are_not_truncated() {
        let mut incoming = HashMap::new();
        incoming.insert((10, 5), vec![caller(20), caller(30)]);
        let radius = compute_with(
            incoming,
            WalkConfig {
                max_depth: 1,
                max_neighbors_per_node: 2,
            },
        );
        assert_eq!(radius.direct_callers, 2);
        assert!(!radius.callers_truncated);
    }

    #[test]
    fn callers_over_cap_set_truncated() {
        let mut incoming = HashMap::new();
        incoming.insert((10, 5), vec![caller(20), caller(30), caller(40)]);
        let radius = compute_with(
            incoming,
            WalkConfig {
                max_depth: 1,
                max_neighbors_per_node: 2,
            },
        );
        assert_eq!(radius.direct_callers, 2);
        assert!(radius.callers_truncated);
    }

    /// The walk's own bounds are what "unfinished" means here. A graph cut by
    /// the depth limit or by a node's neighbour cap holds callers the counts
    /// never saw, and both say so on the response — so neither may also read
    /// as a complete answer.
    #[test]
    fn a_walk_stopped_by_its_own_bounds_is_not_certain() {
        let complete = compute_with(
            HashMap::from([((10, 5), vec![caller(20), caller(30)])]),
            WalkConfig {
                max_depth: 2,
                max_neighbors_per_node: 8,
            },
        );
        assert!(!complete.max_depth_reached && !complete.callers_truncated);

        let depth_bound = compute_with(
            HashMap::from([
                ((10, 5), vec![caller(20)]),
                ((20, 1), vec![caller(30)]),
                ((30, 1), vec![caller(40)]),
            ]),
            WalkConfig {
                max_depth: 2,
                max_neighbors_per_node: 8,
            },
        );
        assert!(
            depth_bound.max_depth_reached,
            "the fixture must trip the cap"
        );

        let neighbour_bound = compute_with(
            HashMap::from([((10, 5), vec![caller(20), caller(30), caller(40)])]),
            WalkConfig {
                max_depth: 3,
                max_neighbors_per_node: 2,
            },
        );
        assert!(
            neighbour_bound.callers_truncated && !neighbour_bound.max_depth_reached,
            "the fixture must bound the walk by the neighbour cap alone"
        );
    }

    /// A share of nothing is not zero. An anchor with no callers has no
    /// composition to report, and publishing `0.0` reads as "none of them are
    /// tests" beside an `impact` whose `coverage` counts the test references
    /// a caller graph never sees.
    #[test]
    fn a_ratio_over_no_callers_is_absent_rather_than_zero() {
        let empty = compute_with(
            HashMap::new(),
            WalkConfig {
                max_depth: 2,
                max_neighbors_per_node: 8,
            },
        );
        assert_eq!(empty.transitive_callers, 0);
        assert_eq!(empty.test_caller_ratio, None);

        let called = compute_with(
            HashMap::from([((10, 5), vec![caller(20), caller(30)])]),
            WalkConfig {
                max_depth: 2,
                max_neighbors_per_node: 8,
            },
        );
        assert!(
            called.test_caller_ratio.is_some(),
            "callers exist, so their composition does"
        );
    }

    fn impl_at(line: u32) -> Location {
        Location::point(PathBuf::from("src/lib.rs"), line, 1)
    }

    #[test]
    fn dynamic_dispatch_marked_when_implementations_exist() {
        let mut incoming = HashMap::new();
        incoming.insert((10, 5), vec![caller(20)]);
        let radius = compute_for(
            incoming,
            Ok(vec![impl_at(40), impl_at(50)]),
            Some(SymbolKind::Method),
            WalkConfig {
                max_depth: 1,
                max_neighbors_per_node: 8,
            },
        );
        let dispatch = radius
            .dynamic_dispatch
            .expect("marker present for a method with impls");
        assert_eq!(dispatch.status, DispatchStatus::Incomplete);
        assert_eq!(dispatch.implementations, 2);
        // The verified caller count is untouched — widening is not done.
        assert_eq!(radius.direct_callers, 1);
    }

    #[test]
    fn a_method_listed_as_its_own_implementation_is_not_dispatched() {
        let radius = compute_for(
            HashMap::new(),
            Ok(vec![Location::point(PathBuf::from("src/lib.rs"), 10, 5)]),
            Some(SymbolKind::Method),
            WalkConfig::default(),
        );
        assert!(radius.dynamic_dispatch.is_none());

        let whole_declaration = compute_for(
            HashMap::new(),
            Ok(vec![Location::full(
                PathBuf::from("src/lib.rs"),
                10,
                1,
                10,
                1,
                14,
                2,
            )]),
            Some(SymbolKind::Method),
            WalkConfig::default(),
        );
        assert!(whole_declaration.dynamic_dispatch.is_none());

        let overridden = compute_for(
            HashMap::new(),
            Ok(vec![
                Location::point(PathBuf::from("src/lib.rs"), 10, 5),
                Location::point(PathBuf::from("src/lib.rs"), 10, 40),
                impl_at(40),
            ]),
            Some(SymbolKind::Method),
            WalkConfig::default(),
        );
        assert_eq!(overridden.dynamic_dispatch.unwrap().implementations, 2);
    }

    #[test]
    fn no_marker_for_non_dispatch_kinds_or_without_impls() {
        // A struct anchor is never probed, even if impls were available.
        let radius = compute_for(
            HashMap::new(),
            Ok(vec![impl_at(40)]),
            Some(SymbolKind::Struct),
            WalkConfig::default(),
        );
        assert!(radius.dynamic_dispatch.is_none());

        // A method with no implementations: the graph is complete, no marker.
        let radius = compute_for(
            HashMap::new(),
            Ok(vec![]),
            Some(SymbolKind::Method),
            WalkConfig::default(),
        );
        assert!(radius.dynamic_dispatch.is_none());
    }

    /// A runtime JSON-RPC MethodNotFound is the same permanent capability
    /// statement as the static table: an interface anchor discloses
    /// `unavailable`. A failed implementation lookup cannot establish
    /// that a method has no dynamic implementations.
    #[test]
    fn runtime_method_not_found_marks_interface_unavailable() {
        let radius = compute_for(
            HashMap::new(),
            Err(-32601),
            Some(SymbolKind::Interface),
            WalkConfig::default(),
        );
        let dispatch = radius
            .dynamic_dispatch
            .expect("interface + runtime -32601 must disclose unavailability");
        assert_eq!(dispatch.status, DispatchStatus::Unavailable);
        assert_eq!(dispatch.implementations, 0);

        let radius = compute_for(
            HashMap::new(),
            Err(-32603),
            Some(SymbolKind::Interface),
            WalkConfig::default(),
        );
        assert_eq!(
            radius.dynamic_dispatch.unwrap().status,
            DispatchStatus::Unavailable
        );

        let radius = compute_for(
            HashMap::new(),
            Err(-32601),
            Some(SymbolKind::Method),
            WalkConfig::default(),
        );
        assert_eq!(
            radius.dynamic_dispatch.unwrap().status,
            DispatchStatus::Unavailable
        );
    }
}
