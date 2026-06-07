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

use std::collections::HashSet;
use std::path::{Path, PathBuf};

use futures::future::join_all;
use serde::Serialize;

use crate::cli::utils::TestMatcher;
use crate::constants::defaults::{BLAST_RADIUS_MAX_CALLERS_PER_NODE, IMPACT_DEFAULT_DEPTH};
use crate::error::LspError;
use crate::models::symbol::SymbolKind;
use crate::services::lsp::LspService;

/// A dynamically-dispatched anchor's call-hierarchy graph is a lower bound:
/// implementations' transitive callers are not folded into the counts
/// (Phase 1 discloses the gap rather than over-approximating by widening).
/// Such a graph therefore never earns more than this confidence, no matter
/// how deep the walk reached.
const DYNAMIC_DISPATCH_CONFIDENCE_CAP: f32 = 0.7;

#[derive(Debug, Clone, Copy)]
pub struct BlastRadiusConfig {
    pub max_depth: u32,
    pub max_callers_per_node: usize,
}

impl Default for BlastRadiusConfig {
    fn default() -> Self {
        Self {
            max_depth: IMPACT_DEFAULT_DEPTH,
            max_callers_per_node: BLAST_RADIUS_MAX_CALLERS_PER_NODE,
        }
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct BlastRadius {
    pub direct_callers: usize,
    pub transitive_callers: usize,
    pub depth: u32,
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub max_depth_reached: bool,
    /// True when at least one node's caller list was cut at
    /// `max_callers_per_node` — the counts below are then a lower bound,
    /// not a complete enumeration.
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub callers_truncated: bool,
    /// Present only when the caller graph was walked under degraded
    /// workspace indexing — every count is then a lower bound.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub indexing: Option<crate::models::lsp::IndexingDegradation>,
    /// Present only when the anchor is dynamically dispatched (a
    /// trait/interface method, or the interface itself). The call-hierarchy
    /// counts then exclude callers reached through implementations, so they
    /// are a lower bound. Absence means the graph is complete for this
    /// anchor.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub dynamic_dispatch: Option<DynamicDispatch>,
    pub callers_by_depth: Vec<DepthBucket>,
    pub test_coverage_ratio: f32,
    pub risk: RiskLevel,
    pub confidence: f32,
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

type CallerKey = (PathBuf, u32, u32);

pub async fn compute(
    lsp: &dyn LspService,
    file: &Path,
    line: u32,
    column: u32,
    is_exported: Option<bool>,
    anchor_kind: Option<SymbolKind>,
    test_matcher: &TestMatcher,
    cfg: &BlastRadiusConfig,
) -> Result<BlastRadius, LspError> {
    let max_depth = cfg.max_depth.max(1);
    let mut visited: HashSet<CallerKey> = HashSet::new();
    visited.insert((file.to_path_buf(), line, column));

    let mut frontier: Vec<CallerKey> = vec![(file.to_path_buf(), line, column)];
    let mut buckets: Vec<DepthBucket> = Vec::with_capacity(max_depth as usize);
    let mut max_depth_reached = false;
    let mut callers_truncated = false;

    for depth in 1..=max_depth {
        let calls_per_node = join_all(frontier.iter().map(|(f, l, c)| async move {
            lsp.incoming_calls(f, *l, *c).await.unwrap_or_default()
        }))
        .await;

        let mut next_frontier: Vec<CallerKey> = Vec::new();
        let mut depth_total = 0usize;
        let mut depth_test = 0usize;

        for calls in calls_per_node {
            if calls.len() > cfg.max_callers_per_node {
                callers_truncated = true;
            }
            for call in calls.into_iter().take(cfg.max_callers_per_node) {
                let key = (
                    call.location.file.clone(),
                    call.location.line,
                    call.location.column,
                );
                if !visited.insert(key.clone()) {
                    continue;
                }
                depth_total += 1;
                if test_matcher.is_test_file(&call.location.file) {
                    depth_test += 1;
                }
                if depth < max_depth {
                    next_frontier.push(key);
                }
            }
        }

        buckets.push(DepthBucket {
            depth,
            count: depth_total,
            test: depth_test,
            prod: depth_total - depth_test,
        });

        // Nodes found at the final depth still have unexplored callers —
        // the walk stopped because of the cap, not exhaustion. (The
        // frontier guard above never queues nodes at `max_depth`, so this
        // must be decided from `depth_total`, not from the frontier.)
        if depth == max_depth {
            max_depth_reached = depth_total > 0;
            break;
        }
        if next_frontier.is_empty() {
            break;
        }
        frontier = next_frontier;
    }

    // Both are independent post-walk probes (no data dependency on each
    // other), so run them concurrently rather than paying two serial
    // round-trips.
    let (indexing, dynamic_dispatch) = tokio::join!(
        lsp.indexing_degradation(crate::models::symbol::Language::from_path(file)),
        detect_dynamic_dispatch(lsp, file, line, column, anchor_kind),
    );

    let direct_callers = buckets.first().map(|b| b.count).unwrap_or(0);
    let transitive_callers: usize = buckets.iter().map(|b| b.count).sum();
    let total_test: usize = buckets.iter().map(|b| b.test).sum();
    let test_ratio = if transitive_callers == 0 {
        0.0
    } else {
        total_test as f32 / transitive_callers as f32
    };
    let depth_reached = buckets.last().map(|b| b.depth).unwrap_or(0);

    Ok(BlastRadius {
        direct_callers,
        transitive_callers,
        depth: depth_reached,
        max_depth_reached,
        callers_truncated,
        indexing,
        dynamic_dispatch,
        callers_by_depth: buckets,
        test_coverage_ratio: test_ratio,
        // Risk is computed from the *verified* call-hierarchy count only.
        // Dynamic-dispatch incompleteness is disclosed via `dynamic_dispatch`
        // + a capped `confidence`, never by inflating the risk label off a
        // graph we know is a lower bound.
        risk: compute_risk(transitive_callers, is_exported, test_ratio),
        confidence: compute_confidence(direct_callers, depth_reached, dynamic_dispatch.as_ref()),
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
        Ok(impls) if !impls.is_empty() => Some(DynamicDispatch {
            status: DispatchStatus::Incomplete,
            implementations: impls.len(),
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

fn compute_risk(transitive: usize, exported: Option<bool>, test_ratio: f32) -> RiskLevel {
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
    // tests catch the regression before it ships.
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
) -> f32 {
    // Confidence is high when the LSP actually returned a call hierarchy
    // (direct_callers > 0) AND we explored the requested depth without
    // tripping the safety cap. Zero direct callers is the dominant
    // false-negative case (LSP feature unsupported, or a true leaf).
    let mut score: f32 = 0.5;
    if direct_callers > 0 {
        score += 0.3;
    }
    if depth_reached >= 2 {
        score += 0.1;
    }
    if depth_reached >= 3 {
        score += 0.1;
    }
    let score = score.clamp(0.0, 1.0);
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

    use async_trait::async_trait;
    use std::collections::HashMap;

    use crate::models::lsp::{
        ApplyActionResult, CallHierarchyItem, CodeAction, CodeLens, FindSymbolsOptions,
        FoldingRange, HoverInfo, IndexingDegradation, InlayHint, PrepareRenameResult, Range,
        RenameResult, SelectionRange, ServerStatus, SignatureHelp, TextEdit, TypeHierarchyItem,
    };
    use crate::models::symbol::{Language, Location, Symbol, SymbolKind};

    /// Call-graph stub: maps a (line, column) position to its incoming
    /// callers, and returns a fixed implementation set for the
    /// dynamic-dispatch probe. Every other `LspService` method is
    /// unreachable from `compute` and panics loudly if that ever changes.
    struct CallGraphStub {
        incoming: HashMap<(u32, u32), Vec<CallHierarchyItem>>,
        implementations: Vec<Location>,
    }

    fn caller(line: u32) -> CallHierarchyItem {
        CallHierarchyItem {
            name: format!("caller_{line}"),
            kind: SymbolKind::Function,
            location: Location::point(PathBuf::from("src/lib.rs"), line, 1),
            call_site: None,
        }
    }

    #[async_trait]
    impl LspService for CallGraphStub {
        async fn indexing_degradation(&self, _language: Language) -> Option<IndexingDegradation> {
            None
        }

        async fn incoming_calls(
            &self,
            _file: &Path,
            line: u32,
            column: u32,
        ) -> Result<Vec<CallHierarchyItem>, LspError> {
            Ok(self
                .incoming
                .get(&(line, column))
                .cloned()
                .unwrap_or_default())
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
        ) -> Result<Vec<Symbol>, LspError> {
            unreachable!()
        }
        async fn find_references(
            &self,
            _file: &Path,
            _line: u32,
            _column: u32,
        ) -> Result<Vec<Location>, LspError> {
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
        async fn find_implementations(
            &self,
            _file: &Path,
            _line: u32,
            _column: u32,
        ) -> Result<Vec<Location>, LspError> {
            Ok(self.implementations.clone())
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
        async fn outgoing_calls(
            &self,
            _file: &Path,
            _line: u32,
            _column: u32,
        ) -> Result<Vec<CallHierarchyItem>, LspError> {
            unreachable!()
        }
        async fn supertypes(
            &self,
            _file: &Path,
            _line: u32,
            _column: u32,
        ) -> Result<Vec<TypeHierarchyItem>, LspError> {
            unreachable!()
        }
        async fn subtypes(
            &self,
            _file: &Path,
            _line: u32,
            _column: u32,
        ) -> Result<Vec<TypeHierarchyItem>, LspError> {
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

    fn compute_with(
        incoming: HashMap<(u32, u32), Vec<CallHierarchyItem>>,
        cfg: BlastRadiusConfig,
    ) -> BlastRadius {
        compute_for(incoming, vec![], None, cfg)
    }

    fn compute_for(
        incoming: HashMap<(u32, u32), Vec<CallHierarchyItem>>,
        implementations: Vec<Location>,
        anchor_kind: Option<SymbolKind>,
        cfg: BlastRadiusConfig,
    ) -> BlastRadius {
        let stub = CallGraphStub {
            incoming,
            implementations,
        };
        let matcher = TestMatcher::default();
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
            BlastRadiusConfig {
                max_depth: 1,
                max_callers_per_node: 2,
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
            BlastRadiusConfig {
                max_depth: 1,
                max_callers_per_node: 2,
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
            vec![impl_at(40), impl_at(50)],
            Some(SymbolKind::Method),
            BlastRadiusConfig {
                max_depth: 1,
                max_callers_per_node: 8,
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
            vec![impl_at(40)],
            Some(SymbolKind::Struct),
            BlastRadiusConfig::default(),
        );
        assert!(radius.dynamic_dispatch.is_none());

        // A method with no implementations: the graph is complete, no marker.
        let radius = compute_for(
            HashMap::new(),
            vec![],
            Some(SymbolKind::Method),
            BlastRadiusConfig::default(),
        );
        assert!(radius.dynamic_dispatch.is_none());
    }
}
