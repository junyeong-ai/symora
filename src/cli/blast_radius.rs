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
use crate::services::lsp::LspService;

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

type CallerKey = (PathBuf, u32, u32);

pub async fn compute(
    lsp: &dyn LspService,
    file: &Path,
    line: u32,
    column: u32,
    is_exported: Option<bool>,
    test_matcher: &TestMatcher,
    cfg: &BlastRadiusConfig,
) -> Result<BlastRadius, LspError> {
    let max_depth = cfg.max_depth.max(1);
    let mut visited: HashSet<CallerKey> = HashSet::new();
    visited.insert((file.to_path_buf(), line, column));

    let mut frontier: Vec<CallerKey> = vec![(file.to_path_buf(), line, column)];
    let mut buckets: Vec<DepthBucket> = Vec::with_capacity(max_depth as usize);
    let mut max_depth_reached = false;

    for depth in 1..=max_depth {
        let calls_per_node = join_all(frontier.iter().map(|(f, l, c)| async move {
            lsp.incoming_calls(f, *l, *c).await.unwrap_or_default()
        }))
        .await;

        let mut next_frontier: Vec<CallerKey> = Vec::new();
        let mut depth_total = 0usize;
        let mut depth_test = 0usize;

        for calls in calls_per_node {
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

        if next_frontier.is_empty() {
            break;
        }
        if depth == max_depth {
            max_depth_reached = true;
        }
        frontier = next_frontier;
    }

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
        callers_by_depth: buckets,
        test_coverage_ratio: test_ratio,
        risk: compute_risk(transitive_callers, is_exported, test_ratio),
        confidence: compute_confidence(direct_callers, depth_reached),
    })
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

fn compute_confidence(direct_callers: usize, depth_reached: u32) -> f32 {
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
    score.clamp(0.0, 1.0)
}

#[cfg(test)]
mod tests {
    use super::*;

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
        let c = compute_confidence(0, 1);
        assert!(c < 0.7, "expected low confidence when no callers, got {c}");
    }

    #[test]
    fn confidence_climbs_with_depth_and_callers() {
        assert!(compute_confidence(5, 1) > compute_confidence(0, 1));
        assert!(compute_confidence(5, 3) > compute_confidence(5, 1));
        assert_eq!(compute_confidence(100, 5), 1.0);
    }
}
