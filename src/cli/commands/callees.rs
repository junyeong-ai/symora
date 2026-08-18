//! Outgoing (downward) call hierarchy. Three query modes over one shared
//! traversal core (`cli::call_graph`):
//!
//! - direct (default): the anchor's immediate callees — single hop.
//! - `--depth N`: the downward *reachable set* to depth N, with honest
//!   lower-bound markers. This is the capability plain call hierarchy lacks:
//!   "what does this transitively call?".
//! - `--to <loc>`: the shortest call *chain* from the anchor to a target —
//!   "how does this function reach that one?" — answering with the ordered
//!   frames, or a typed reachability verdict that never overclaims.
//!
//! Upward (incoming) transitive reachability is deliberately NOT offered
//! here: `impact` already walks the incoming graph depth-bounded with the
//! same markers, and a second door to it would be a parallel surface.

use std::collections::HashMap;

use anyhow::Result;
use clap::Args;
use serde::Serialize;

use crate::app::App;
use crate::cli::call_graph::{self, Direction, NodeKey, WalkConfig, key_of};
use crate::cli::commands::common::{anchor_of, execute_list};
use crate::cli::response::{CallHierarchyOutput, LocationOutput, Section};
use crate::cli::{LocationArg, ParsedLocation};
use crate::constants::defaults::IMPACT_MAX_DEPTH;
use crate::models::lsp::{CallHierarchyItem, IndexingDegradation};

#[derive(Args, Debug)]
pub struct CalleesArgs {
    #[command(flatten)]
    pub loc: LocationArg,

    /// Maximum results
    #[arg(long)]
    pub limit: Option<usize>,

    /// Transitive callee depth (capped at the impact max). Above 1, returns
    /// the downward reachable set instead of just the direct callees. Omitted
    /// = direct callees only.
    #[arg(long)]
    pub depth: Option<u32>,

    /// Shortest call chain FROM the anchor TO this target (`file:line[:col]`):
    /// "how does this function reach that one?". Searches to `--depth` (or the
    /// impact max when unset).
    #[arg(long)]
    pub to: Option<String>,
}

/// Downward reachable set: every callee reachable within `depth`, deduped to
/// its shallowest discovery, with the traversal's honest lower-bound markers.
#[derive(Debug, Serialize)]
struct CalleesReachOutput {
    #[serde(flatten)]
    section: Section<CallHierarchyOutput>,
    /// Depth actually reached.
    depth: u32,
    /// The walk stopped at the depth cap with callees still unexplored — a
    /// deeper `--depth` may surface more. The set is a lower bound.
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    max_depth_reached: bool,
    /// At least one node's callee list hit the per-node fan-out cap, so the
    /// set is a lower bound.
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    callees_truncated: bool,
    /// At least one hop's callee query failed and was treated as empty, so the
    /// reachable set is a lower bound.
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    incomplete: bool,
    /// The anchor (from-position) did not resolve to a listed symbol:
    /// "binding" (a declaration the tree does not list; the set is exactly
    /// its), "not_a_symbol" (read OK, denotes nothing), or "unavailable" (a
    /// read failed). Omitted when resolved. For the last two the reachable set
    /// is not authoritatively "no callees". One shared `*_status` vocabulary
    /// across every surface (`AnchorResolution::as_status`).
    #[serde(skip_serializing_if = "Option::is_none")]
    anchor_status: Option<&'static str>,
}

/// Whether the anchor reaches the target through resolved outgoing calls.
#[derive(Debug, Clone, Copy, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
enum Reachability {
    /// A call chain was found; `chain` carries it.
    Found,
    /// No chain within the searched bound — `max_depth_reached` /
    /// `callees_truncated` / `indexing` say why; a deeper or wider search may
    /// still find one.
    NotReachedWithinBound,
    /// The reachable callee set was fully explored and the target is not in
    /// it: no path through statically-resolved calls. Dynamic dispatch
    /// (trait/virtual calls) is not folded in, so this remains a lower bound,
    /// never an absolute "unreachable".
    NoStaticPath,
}

/// Shortest-call-chain answer for a `--to` query.
#[derive(Debug, Serialize)]
struct CalleesPathOutput {
    /// The (resolved) target the chain was sought to.
    target: LocationOutput,
    reachability: Reachability,
    /// Present (and always true) only when `reachability` is `no_static_path`:
    /// the static call graph was fully explored without a path, but dynamic
    /// dispatch (trait/virtual/callback calls) is not folded in, so the target
    /// may still be reachable at runtime. Makes the lower-bound nature of the
    /// negative machine-branchable instead of leaving an agent to read
    /// `no_static_path` as an absolute "unreachable".
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    dynamic_dispatch_possible: bool,
    /// The ordered frames from the first hop through the target — present
    /// only when `reachability` is `found`.
    #[serde(skip_serializing_if = "Option::is_none")]
    chain: Option<Vec<CallHierarchyOutput>>,
    /// Depth actually reached.
    depth: u32,
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    max_depth_reached: bool,
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    callees_truncated: bool,
    /// A hop's callee query failed and was treated as empty, so a would-be
    /// `no_static_path` is a possible missed path: the verdict degrades to
    /// `not_reached_within_bound` and this flag discloses why.
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    incomplete: bool,
    /// The `--to` target did not resolve to a verified symbol: "not_a_symbol" or
    /// "unavailable" (omitted when resolved). The verdict about it is then never
    /// an authoritative negative — it is forced to the non-absolute
    /// `not_reached_within_bound`. Same `*_status` vocabulary as `anchor_status`.
    #[serde(skip_serializing_if = "Option::is_none")]
    target_status: Option<&'static str>,
    /// The anchor (from-position) did not resolve to a verified symbol:
    /// "not_a_symbol" or "unavailable" (omitted when resolved), so the verdict
    /// is never an authoritative negative.
    #[serde(skip_serializing_if = "Option::is_none")]
    anchor_status: Option<&'static str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    indexing: Option<IndexingDegradation>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    hints: Vec<String>,
}

pub async fn execute(args: CalleesArgs, app: &App) -> Result<()> {
    let limit = args.limit.unwrap_or(app.config().lsp.calls_limit);

    if let Some(to) = args.to {
        let depth = args
            .depth
            .unwrap_or(IMPACT_MAX_DEPTH)
            .clamp(1, IMPACT_MAX_DEPTH);
        return execute_path(app, args.loc, &to, depth).await;
    }

    match args.depth {
        Some(depth) if depth > 1 => {
            execute_reach(app, args.loc, depth.clamp(1, IMPACT_MAX_DEPTH), limit).await
        }
        // Direct callees — the original single-hop behaviour, unchanged.
        _ => {
            execute_list(
                app,
                args.loc,
                limit,
                "callees",
                |file, line, col| async move { app.lsp.outgoing_calls(&file, line, col).await },
                |c, root| CallHierarchyOutput::from_item(&c, root),
            )
            .await
        }
    }
}

/// Probe the anchor's outgoing calls once so a capability gap (the server
/// lacks call hierarchy) or a transient error surfaces honestly, exactly as
/// single-hop callees does, instead of the walk silently swallowing it into a
/// misleading empty/not-reached answer.
async fn probe_outgoing(app: &App, file: &std::path::Path, line: u32, column: u32) -> Result<()> {
    if let Err(e) = app.lsp.outgoing_calls(file, line, column).await {
        app.output.print_error(e);
        return Err(anyhow::anyhow!("probe failed"));
    }
    Ok(())
}

async fn execute_reach(app: &App, loc: LocationArg, depth: u32, limit: usize) -> Result<()> {
    let ctx = &app.output;
    let loc = loc.parse()?.to_absolute()?;
    let anchor = anchor_of(app.lsp.as_ref(), &loc).await;

    if probe_outgoing(app, &anchor.file, anchor.line, anchor.column)
        .await
        .is_err()
    {
        return Ok(());
    }

    let walk = call_graph::walk(
        app.lsp.as_ref(),
        (anchor.file.clone(), anchor.line, anchor.column),
        Direction::Outgoing,
        &WalkConfig {
            max_depth: depth,
            ..Default::default()
        },
    )
    .await;

    let reachable: Vec<CallHierarchyItem> = walk.levels.iter().flatten().cloned().collect();
    let total = reachable.len();
    let items: Vec<CallHierarchyOutput> = reachable
        .iter()
        .take(limit)
        .map(|c| CallHierarchyOutput::from_item(c, ctx.root()))
        .collect();

    let hints = anchor.anchor_hints(ctx.root(), "callees");

    ctx.print_success(CalleesReachOutput {
        section: Section::with_total(items, total)
            .with_hints(hints)
            .with_indexing(walk.indexing),
        depth: walk.depth_reached(),
        max_depth_reached: walk.max_depth_reached,
        callees_truncated: walk.truncated,
        incomplete: walk.incomplete,
        anchor_status: anchor.resolution.as_status(),
    });

    Ok(())
}

async fn execute_path(app: &App, loc: LocationArg, to: &str, depth: u32) -> Result<()> {
    let ctx = &app.output;
    let loc = loc.parse()?.to_absolute()?;
    let target_loc = ParsedLocation::parse(to)?.to_absolute()?;

    let anchor = anchor_of(app.lsp.as_ref(), &loc).await;
    let target = anchor_of(app.lsp.as_ref(), &target_loc).await;

    if probe_outgoing(app, &anchor.file, anchor.line, anchor.column)
        .await
        .is_err()
    {
        return Ok(());
    }

    let anchor_key: NodeKey = (anchor.file.clone(), anchor.line, anchor.column);
    let target_key: NodeKey = (target.file.clone(), target.line, target.column);

    let walk = call_graph::walk(
        app.lsp.as_ref(),
        anchor_key.clone(),
        Direction::Outgoing,
        &WalkConfig {
            max_depth: depth,
            ..Default::default()
        },
    )
    .await;

    // Two questions, two predicates. Looking a path UP needs an exact key, so
    // a binding qualifies: the tree does not list it, but the position names
    // it precisely. An endpoint that is not a symbol has a phantom key that
    // could coincidentally read as `found`, and one whose symbols could not be
    // read was never resolved to its declaration, so a path from it would not
    // be the path the user meant.
    //
    // Calling the absence of a path ABSOLUTE needs more: an endpoint that is
    // actually a node in the graph. A binding never is, so an empty answer
    // about one says nothing about reachability, which is why the `bounded`
    // terms below turn on `!is_resolved()` rather than on this.
    let chain_keys = if target.is_declaration() && anchor.is_declaration() {
        walk.path_to(&anchor_key, &target_key)
    } else {
        None
    };

    let (reachability, chain) = match chain_keys {
        Some(keys) => {
            // Resolve each frame's key back to its discovered item for names.
            let item_by_key: HashMap<NodeKey, &CallHierarchyItem> = walk
                .levels
                .iter()
                .flatten()
                .map(|it| (key_of(it), it))
                .collect();
            // Every path key is a discovered node, so it must resolve in
            // item_by_key. Use map + expect (not filter_map) so a future drift
            // in the levels/predecessor invariant fails loudly instead of
            // emitting a silently-gapped `found` chain.
            let frames: Vec<CallHierarchyOutput> = keys
                .iter()
                .map(|k| {
                    let it = item_by_key
                        .get(k)
                        .expect("path key must resolve to a discovered call-graph item");
                    CallHierarchyOutput::from_item(it, ctx.root())
                })
                .collect();
            (Reachability::Found, Some(frames))
        }
        None => {
            // Bounded (a deeper/wider search might find a path) versus a clean
            // exhaustion of the statically-resolved reachable set.
            let bounded = walk.max_depth_reached
                || walk.truncated
                || walk.indexing.is_some()
                || walk.incomplete
                || !target.is_resolved()
                || !anchor.is_resolved();
            let verdict = if bounded {
                Reachability::NotReachedWithinBound
            } else {
                Reachability::NoStaticPath
            };
            (verdict, None)
        }
    };

    let mut hints = anchor.verdict_hints(
        "from-position",
        ctx.root(),
        "anchor at a declaration (e.g. a search_symbols result)",
    );
    hints.extend(target.verdict_hints(
        "--to target",
        ctx.root(),
        "point --to at a declaration (e.g. a search_symbols result)",
    ));

    let dynamic_dispatch_possible = reachability == Reachability::NoStaticPath;
    if dynamic_dispatch_possible {
        hints.push(
            "no_static_path is a lower bound: dynamic dispatch (trait/virtual/callback calls) is \
             not analyzed, so the target may still be reachable at runtime"
                .to_string(),
        );
    }

    ctx.print_success(CalleesPathOutput {
        target: LocationOutput::from_path(&target.file, target.line, target.column, ctx.root()),
        reachability,
        dynamic_dispatch_possible,
        chain,
        depth: walk.depth_reached(),
        max_depth_reached: walk.max_depth_reached,
        callees_truncated: walk.truncated,
        incomplete: walk.incomplete,
        target_status: target.resolution.as_status(),
        anchor_status: anchor.resolution.as_status(),
        indexing: walk.indexing,
        hints,
    });

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn frame(name: &str, line: u32) -> CallHierarchyOutput {
        CallHierarchyOutput {
            name: name.to_string(),
            location: LocationOutput {
                file: "src/lib.rs".to_string(),
                line,
                column: 1,
                snippet: None,
                degraded_column: None,
            },
            call_site: None,
            body: None,
        }
    }

    fn target() -> LocationOutput {
        LocationOutput {
            file: "src/lib.rs".to_string(),
            line: 99,
            column: 1,
            snippet: None,
            degraded_column: None,
        }
    }

    // The three reachability verdicts must serialize to the documented,
    // mutually-distinguishable shapes — this is the public JSON contract.

    #[test]
    fn found_carries_the_chain_and_omits_bound_markers() {
        let out = CalleesPathOutput {
            target: target(),
            reachability: Reachability::Found,
            dynamic_dispatch_possible: false,
            chain: Some(vec![frame("mid", 20), frame("sink", 99)]),
            depth: 3,
            max_depth_reached: false,
            callees_truncated: false,
            incomplete: false,
            target_status: None,
            anchor_status: None,
            indexing: None,
            hints: vec![],
        };
        let v = serde_json::to_value(out).unwrap();
        assert_eq!(v["reachability"], "found");
        assert_eq!(v["chain"].as_array().unwrap().len(), 2);
        // Bound markers are omitted when false — no defensive parsing for the
        // common found case.
        assert!(v.get("max_depth_reached").is_none());
        assert!(v.get("callees_truncated").is_none());
        assert!(v.get("indexing").is_none());
        assert!(v.get("hints").is_none());
        // Dynamic-dispatch disclosure belongs only to no_static_path.
        assert!(v.get("dynamic_dispatch_possible").is_none());
    }

    #[test]
    fn not_reached_within_bound_omits_chain_and_shows_why() {
        let out = CalleesPathOutput {
            target: target(),
            reachability: Reachability::NotReachedWithinBound,
            dynamic_dispatch_possible: false,
            chain: None,
            depth: 2,
            max_depth_reached: true,
            callees_truncated: false,
            incomplete: false,
            target_status: None,
            anchor_status: None,
            indexing: None,
            hints: vec![],
        };
        let v = serde_json::to_value(out).unwrap();
        assert_eq!(v["reachability"], "not_reached_within_bound");
        assert!(v.get("chain").is_none());
        assert_eq!(v["max_depth_reached"], true);
    }

    #[test]
    fn no_static_path_is_a_clean_negative_without_markers() {
        let out = CalleesPathOutput {
            target: target(),
            reachability: Reachability::NoStaticPath,
            dynamic_dispatch_possible: true,
            chain: None,
            depth: 3,
            max_depth_reached: false,
            callees_truncated: false,
            incomplete: false,
            target_status: None,
            anchor_status: None,
            indexing: None,
            hints: vec![],
        };
        let v = serde_json::to_value(out).unwrap();
        assert_eq!(v["reachability"], "no_static_path");
        assert!(v.get("chain").is_none());
        assert!(v.get("max_depth_reached").is_none());
        assert!(v.get("callees_truncated").is_none());
        // A clean negative carries no incomplete marker — that flag is what
        // separates a true `no_static_path` from a hop-error lower bound.
        assert!(v.get("incomplete").is_none());
        // The negative discloses itself as a lower bound: dynamic dispatch is
        // not folded in, so it is never read as an absolute "unreachable".
        assert_eq!(v["dynamic_dispatch_possible"], true);
    }

    #[test]
    fn target_unresolved_is_disclosed_and_never_an_absolute_negative() {
        let out = CalleesPathOutput {
            target: target(),
            reachability: Reachability::NotReachedWithinBound,
            dynamic_dispatch_possible: false,
            chain: None,
            depth: 3,
            max_depth_reached: false,
            callees_truncated: false,
            incomplete: false,
            target_status: Some("not_a_symbol"),
            anchor_status: None,
            indexing: None,
            hints: vec!["did not resolve to a symbol".to_string()],
        };
        let v = serde_json::to_value(out).unwrap();
        assert_eq!(v["target_status"], "not_a_symbol");
        // A target that was never a symbol must never read as the absolute
        // no_static_path — the verdict stays non-absolute.
        assert_eq!(v["reachability"], "not_reached_within_bound");
    }

    #[test]
    fn reach_output_flattens_section_and_omits_false_markers() {
        let out = CalleesReachOutput {
            section: Section::with_total(vec![frame("a", 20), frame("b", 30)], 2),
            depth: 2,
            max_depth_reached: false,
            callees_truncated: false,
            incomplete: false,
            anchor_status: None,
        };
        let v = serde_json::to_value(out).unwrap();
        // Section fields are flattened beside the reach-specific ones.
        assert_eq!(v["count"], 2);
        assert_eq!(v["showing"], 2);
        assert_eq!(v["items"].as_array().unwrap().len(), 2);
        assert_eq!(v["depth"], 2);
        assert!(v.get("max_depth_reached").is_none());
        assert!(v.get("callees_truncated").is_none());
    }

    #[test]
    fn reach_output_surfaces_lower_bound_markers_when_set() {
        let out = CalleesReachOutput {
            section: Section::with_total(vec![frame("a", 20)], 5),
            depth: 3,
            max_depth_reached: true,
            callees_truncated: true,
            incomplete: false,
            anchor_status: None,
        };
        let v = serde_json::to_value(out).unwrap();
        assert_eq!(v["max_depth_reached"], true);
        assert_eq!(v["callees_truncated"], true);
        // Section's own truncation (showing < count) is a distinct concept and
        // still reported.
        assert_eq!(v["truncated"], true);
    }
}
