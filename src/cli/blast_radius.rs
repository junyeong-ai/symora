//! Change-impact "blast radius" — the transitive caller graph for a
//! symbol at a precise location, with risk + confidence signals.
//!
//! Walks the LSP `incoming_calls` graph BFS-style up to `max_depth`,
//! parallelising the per-frontier round-trips. Stops early when a
//! frontier becomes empty; surfaces `max_depth_reached` so callers know
//! when to ask for a wider search.
//!
//! Risk + confidence use *objective* thresholds (transitive count,
//! exported-API flag, test-vs-prod ratio). They are deliberately not
//! ML-tuned — Symora's CLAUDE.md anti-goal is "heuristic tweaks without
//! repeated evidence", so this stays simple and predictable.

use std::path::Path;

use serde::Serialize;

use crate::cli::call_graph::{self, Direction, WalkConfig};
use crate::error::LspError;
use crate::models::symbol::SymbolKind;
use crate::services::TestScope;
use crate::services::lsp::LspService;

/// A dynamically-dispatched anchor's call-hierarchy graph is a lower bound:
/// implementations' transitive callers are not folded into the counts
/// (Phase 1 discloses the gap rather than over-approximating by widening).
/// Such a graph therefore never earns more than this confidence, no matter
/// how deep the walk reached.
const DYNAMIC_DISPATCH_CONFIDENCE_CAP: f64 = 0.7;

/// A walk that swallowed a hop error (an LSP failure mid-traversal) is a known
/// lower bound, so its caller graph can never be presented as high confidence.
const INCOMPLETE_WALK_CONFIDENCE_CAP: f64 = 0.5;

#[derive(Debug, Clone, Serialize)]
pub struct BlastRadius {
    pub direct_callers: usize,
    pub transitive_callers: usize,
    pub depth: u32,
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
    pub test_coverage_ratio: f64,
    pub risk: RiskLevel,
    pub confidence: f64,
}

#[derive(Debug, Clone, Serialize)]
pub struct DepthBucket {
    pub depth: u32,
    pub count: usize,
    pub test: usize,
    pub prod: usize,
}

#[derive(Debug, Clone, Copy, Serialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum RiskLevel {
    Low,
    Medium,
    High,
    Critical,
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
    is_exported: Option<bool>,
    anchor_kind: Option<SymbolKind>,
    test_scope: &TestScope,
    cfg: &WalkConfig,
) -> Result<BlastRadius, LspError> {
    // Blast radius is the upward (incoming) call graph. The traversal,
    // fan-out cap, depth/exhaustion markers, and degradation aggregation all
    // live in the shared `call_graph::walk` core; this function adds only the
    // risk-specific projection (test/prod classification, risk, confidence,
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
    // one more production dependency inflating the risk.
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
    let test_ratio = if transitive_callers == 0 {
        0.0
    } else {
        coarse(total_test as f64 / transitive_callers as f64)
    };
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
        test_coverage_ratio: test_ratio,
        // Risk is computed from the *verified* call-hierarchy count only.
        // Dynamic-dispatch incompleteness is disclosed via `dynamic_dispatch`
        // + a capped `confidence`, never by inflating the risk label off a
        // graph we know is a lower bound.
        risk: compute_risk(transitive_callers, is_exported, test_ratio),
        // A swallowed hop error makes the graph a lower bound, so — like
        // dynamic dispatch — it caps confidence rather than being presented
        // as an authoritative count.
        confidence: {
            let c = compute_confidence(direct_callers, depth_reached, dynamic_dispatch.as_ref());
            if walk.incomplete {
                c.min(INCOMPLETE_WALK_CONFIDENCE_CAP)
            } else {
                c
            }
        },
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
    if !matches!(kind, SymbolKind::Method | SymbolKind::Interface) {
        return None;
    }
    match lsp.find_implementations(file, line, column).await {
        Ok(impls) if !impls.data.is_empty() => Some(DynamicDispatch {
            status: DispatchStatus::Incomplete,
            implementations: impls.data.len(),
        }),
        Ok(_) => None,
        // `Unavailable` means a genuine capability gap — the server does not
        // implement `textDocument/implementation`, whether declared statically
        // or answered as MethodNotFound at runtime — on an interface anchor.
        // A transient error (timeout, server restart) is not a capability
        // statement: stay silent rather than mislabel it.
        Err(e) if kind == SymbolKind::Interface && e.is_unsupported() => Some(DynamicDispatch {
            status: DispatchStatus::Unavailable,
            implementations: 0,
        }),
        Err(_) => None,
    }
}

/// Round a published score to the precision it actually carries.
///
/// `risk` and `confidence` are assembled from fixed steps and compared
/// against fixed thresholds, so binary floating point would otherwise leak
/// values like `0.9000000357627869` into the output contract. Rounding at
/// the point of computation — before the thresholds see the value — keeps
/// the published number and the number the verdict was made on identical,
/// so an agent can reproduce the verdict from the response alone.
fn coarse(value: f64) -> f64 {
    (value * 100.0).round() / 100.0
}

fn compute_risk(transitive: usize, exported: Option<bool>, test_ratio: f64) -> RiskLevel {
    let exported = exported.unwrap_or(false);
    if transitive == 0 {
        return RiskLevel::Low;
    }
    // Exported APIs take precedence over test coverage: a public symbol
    // with high test ratio is still a breaking-change risk because
    // downstream consumers don't see those tests.
    if exported && transitive > 50 {
        return RiskLevel::Critical;
    }
    if exported || transitive > 50 {
        return RiskLevel::High;
    }
    // For internal symbols, high test coverage genuinely lowers risk:
    // well-exercised paths guard behavior, so a breaking change is more likely
    // to surface as a failing test.
    if test_ratio > 0.8 {
        return RiskLevel::Low;
    }
    if transitive > 5 {
        return RiskLevel::Medium;
    }
    RiskLevel::Low
}

fn compute_confidence(
    direct_callers: usize,
    depth_reached: u32,
    dynamic_dispatch: Option<&DynamicDispatch>,
) -> f64 {
    // Confidence is high when the LSP actually returned a call hierarchy
    // (direct_callers > 0) AND we explored the requested depth without
    // tripping the safety cap. Zero direct callers is the dominant
    // false-negative case (LSP feature unsupported, or a true leaf).
    let mut score: f64 = 0.5;
    if direct_callers > 0 {
        score += 0.3;
    }
    if depth_reached >= 2 {
        score += 0.1;
    }
    if depth_reached >= 3 {
        score += 0.1;
    }
    let score = coarse(score.clamp(0.0, 1.0));
    // A dynamically-dispatched anchor's graph is a known lower bound, so it
    // can never be presented as high confidence regardless of depth.
    if dynamic_dispatch.is_some() {
        score.min(DYNAMIC_DISPATCH_CONFIDENCE_CAP)
    } else {
        score
    }
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
            Some(false),
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

    #[test]
    fn risk_zero_callers_is_low() {
        assert_eq!(compute_risk(0, Some(true), 0.0), RiskLevel::Low);
        assert_eq!(compute_risk(0, Some(false), 0.0), RiskLevel::Low);
    }

    #[test]
    fn risk_high_test_coverage_lowers_internal_only() {
        // Internal symbol with strong tests is genuinely Low.
        assert_eq!(compute_risk(20, Some(false), 0.95), RiskLevel::Low);
        // Exported API is still a breaking-change risk regardless of tests:
        // downstream consumers can't see those tests.
        assert_eq!(compute_risk(20, Some(true), 0.95), RiskLevel::High);
        assert_eq!(compute_risk(60, Some(true), 0.95), RiskLevel::Critical);
    }

    #[test]
    fn risk_exported_and_many_callers_is_critical() {
        assert_eq!(compute_risk(60, Some(true), 0.0), RiskLevel::Critical);
    }

    #[test]
    fn risk_exported_or_many_is_high() {
        assert_eq!(compute_risk(10, Some(true), 0.0), RiskLevel::High);
        assert_eq!(compute_risk(60, Some(false), 0.0), RiskLevel::High);
    }

    #[test]
    fn risk_moderate_internal_is_medium() {
        assert_eq!(compute_risk(10, Some(false), 0.0), RiskLevel::Medium);
    }

    #[test]
    fn risk_few_internal_is_low() {
        assert_eq!(compute_risk(3, Some(false), 0.0), RiskLevel::Low);
    }

    #[test]
    fn confidence_zero_callers_stays_at_baseline() {
        let c = compute_confidence(0, 1, None);
        assert!(c < 0.7, "expected low confidence when no callers, got {c}");
    }

    #[test]
    fn confidence_climbs_with_depth_and_callers() {
        assert!(compute_confidence(5, 1, None) > compute_confidence(0, 1, None));
        assert!(compute_confidence(5, 3, None) > compute_confidence(5, 1, None));
        assert_eq!(compute_confidence(100, 5, None), 1.0);
    }

    #[test]
    fn dynamic_dispatch_caps_confidence() {
        let dispatch = DynamicDispatch {
            status: DispatchStatus::Incomplete,
            implementations: 3,
        };
        // Without the marker this anchor would score 1.0; the cap pulls it
        // down because the graph is a known lower bound.
        assert_eq!(compute_confidence(100, 5, None), 1.0);
        assert_eq!(
            compute_confidence(100, 5, Some(&dispatch)),
            DYNAMIC_DISPATCH_CONFIDENCE_CAP
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
        assert!(radius.confidence <= DYNAMIC_DISPATCH_CONFIDENCE_CAP);
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
    /// `unavailable`. Transient errors and non-interface anchors stay
    /// silent — never a mislabeled capability claim.
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
        assert!(radius.confidence <= DYNAMIC_DISPATCH_CONFIDENCE_CAP);

        // A transient server error is not a capability statement.
        let radius = compute_for(
            HashMap::new(),
            Err(-32603),
            Some(SymbolKind::Interface),
            WalkConfig::default(),
        );
        assert!(radius.dynamic_dispatch.is_none());

        // A method can't be known virtual — silent even on -32601.
        let radius = compute_for(
            HashMap::new(),
            Err(-32601),
            Some(SymbolKind::Method),
            WalkConfig::default(),
        );
        assert!(radius.dynamic_dispatch.is_none());
    }
}
