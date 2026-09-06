//! JSON output contract for every CLI command.
//!
//! Each submodule groups outputs by the layer that produces them:
//!   - `symbol`: file/workspace symbol listings.
//!   - `lsp`: LSP-derived responses (definition, hover, hierarchies, etc.).
//!   - `analysis`: derived analytics (impact, refs, tests, coverage).
//!   - `editing`: code-action and rename results.
//!   - `disclosure`: what a response says about its own shortfalls.
//!
//! The output types are re-exported flat, so existing call sites use
//! `crate::cli::response::SymbolOutput` regardless of which submodule defines
//! them. `disclosure` keeps its own namespace: it is a vocabulary a command
//! reasons in, not a shape it emits.

mod analysis;
pub mod disclosure;
mod editing;
mod lsp;
mod symbol;

pub use analysis::{
    AffectedFileOutput, ImpactOutput, RefOutput, RefsOutput, TargetOutput, TestCoverageOutput,
    TestOutput,
};
pub use editing::{
    ActionOutput, ApplyActionOutput, CallerFileDiagnostics, CallerVerification, EditDiagnostics,
    EditOutput, FileChangeOutput, LineRange,
};
pub use lsp::{
    CallHierarchyOutput, DefinitionOutput, DiagnosticOutput, DisclosesIndexing, HoverOutput,
    ParameterOutput, SignatureHelpOutput, SignatureItemOutput, TypeInfoOutput,
};
pub use symbol::{ServerStatusOutput, SymbolOutput};

use std::path::Path;

use serde::{Deserialize, Serialize};

use super::errors::OutputError;

/// List-shaped response wrapper — the one list contract every command
/// (and the daemon wire) emits:
///
/// - `count` — total matches found
/// - `showing` — number actually emitted in `items`
/// - `items` — the result array
/// - `truncated` — present (and `true`) only when `showing < count`
/// - `stale` — present (and `true`) only when index-served rows came from
///   files that changed on disk after indexing
/// - `hints` / `next_commands` — omitted when empty
/// - `bodies_included` — present only on sections where body attachment
///   ran (`context --with-bodies`) and that still contain items
/// - `coverage_gaps` — languages a search could not cover; populated by every
///   symbol-search route, omitted (empty) on every other list response
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Section<T> {
    pub count: usize,
    pub showing: usize,
    pub items: Vec<T>,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub truncated: bool,
    /// Present when a file backing one of `items` changed on disk after it
    /// was indexed — those rows may no longer match the file, and re-running
    /// `symora search index build` refreshes them. This is index-vs-disk
    /// content drift, distinct from the edit-time stale-range guard. It errs
    /// toward being set: the size fitter drops rows without knowing which
    /// ones it spoke for, so a section it trimmed may carry the flag over
    /// survivors that are all current.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub stale: bool,
    /// Present (true) only when `count` is a lower bound rather than
    /// everything the answer's own SOURCES held — the answer was assembled
    /// from something the command could not see all of.
    ///
    /// Whether a source is itself current is a different axis and is not this
    /// flag: an index-backed count is exact for the build behind it, and that
    /// build is a snapshot, so a file written since it ran has no row to find
    /// and none to mark `stale`. That axis is read from `backend`, `stale`,
    /// and `search index status`, and cured by `search index build`. Setting
    /// this flag for it would mark nearly every index hit, and a flag that is
    /// almost always set is one an agent learns to skip. The cause is always
    /// named in `hints`, because the remedies differ: paths the walk could not
    /// read, an index built over such paths, or two overlapping sources at
    /// least one of which was not materialised whole. The same word `refs`
    /// uses for a reference set the server admits is short.
    ///
    /// It divides work with `coverage_gaps` rather than duplicating it: a gap
    /// names a LANGUAGE the answer does not speak for, and an agent reading
    /// one already knows the count is short for it. This says the count is
    /// short for a reason no language gap can express — every requested
    /// language was covered, and the answer is still not whole. Do not set it
    /// from a non-empty `coverage_gaps`; that would make the more precise
    /// field redundant and this one routine enough to ignore.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub incomplete: bool,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub hints: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub next_commands: Vec<String>,
    /// Present only on sections where body attachment ran
    /// (`context --with-bodies`) and that still contain items. Invariant:
    /// always equals the number of `items` carrying a complete `body` —
    /// at emission, and re-established by the transport size fitter if it
    /// drops items. `showing - bodies_included` items had their body
    /// omitted for one of three causes: the token budget was exhausted,
    /// the symbol was unresolvable at the item's position, or the symbol
    /// genuinely has no body (prototypes, interface methods). Only the
    /// first is cured by raising `body_tokens` — an omission that
    /// persists after a large raise is not budget-caused. Omission is
    /// disclosed, never silent.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub bodies_included: Option<usize>,
    /// Present only when the answer was computed under degraded
    /// workspace indexing — the list is then a lower bound, not a
    /// complete enumeration.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub indexing: Option<crate::models::lsp::IndexingDegradation>,
    /// Languages a requested search could not cover, so an empty `items`
    /// reads as "not searched here" rather than "no such symbol", and a
    /// short one reads as partial rather than whole. Populated by every
    /// symbol-search route — index-backed, wildcard, and workspace-only
    /// alike — at any result count; omitted everywhere else and when empty.
    /// A new `Section<T>` field is justified only when it applies to all
    /// list responses — this one is scoped by being inert elsewhere.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub coverage_gaps: Vec<CoverageGap>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<OutputError>,
}

/// A language a search did not cover, with a stable machine-branchable reason.
/// The shared shape for both `search`'s `Section.coverage_gaps` and `usage`'s
/// `coverage_gaps`, and the emitted form of `symbol_discovery::Uncovered`,
/// which owns the reason set: `not_indexed`, `server_not_installed`,
/// `timed_out`, `unsupported`, `unavailable`, `not_searched`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CoverageGap {
    pub language: String,
    pub reason: String,
}

impl<T> Section<T> {
    /// Complete result set — nothing was withheld.
    pub fn new(items: Vec<T>) -> Self {
        Self::with_total_count(items, None)
    }

    /// `items` after an emission cap; `count` is the total the command
    /// found before capping. `truncated` derives from `showing < count`.
    pub fn with_total(items: Vec<T>, count: usize) -> Self {
        Self::with_total_count(items, Some(count))
    }

    pub fn error(error: impl Into<OutputError>) -> Self {
        Self {
            count: 0,
            showing: 0,
            items: vec![],
            truncated: false,
            stale: false,
            incomplete: false,
            hints: vec![],
            next_commands: vec![],
            bodies_included: None,
            indexing: None,
            coverage_gaps: vec![],
            error: Some(error.into()),
        }
    }

    pub fn with_hints(mut self, hints: Vec<String>) -> Self {
        self.hints = hints;
        self
    }

    pub fn with_next_commands(mut self, next_commands: Vec<String>) -> Self {
        self.next_commands = next_commands;
        self
    }

    pub fn with_indexing(
        mut self,
        indexing: Option<crate::models::lsp::IndexingDegradation>,
    ) -> Self {
        self.indexing = indexing;
        self
    }

    pub fn with_bodies_included(mut self, bodies_included: Option<usize>) -> Self {
        self.bodies_included = bodies_included;
        self
    }

    pub fn with_stale(mut self, stale: bool) -> Self {
        self.stale = stale;
        self
    }

    /// Mark `count` as a lower bound (see the field doc). The caller states
    /// the cause in `hints`; the flag alone is what an agent branches on.
    pub fn with_incomplete(mut self, incomplete: bool) -> Self {
        self.incomplete = incomplete;
        self
    }

    /// Attach the languages a search could not cover (see the field doc).
    /// Search-only; other list responses leave it empty.
    pub fn with_coverage_gaps(mut self, coverage_gaps: Vec<CoverageGap>) -> Self {
        self.coverage_gaps = coverage_gaps;
        self
    }

    fn with_total_count(items: Vec<T>, count: Option<usize>) -> Self {
        let showing = items.len();
        let count = count.map_or(showing, |c| c.max(showing));
        Self {
            count,
            showing,
            items,
            truncated: showing < count,
            stale: false,
            incomplete: false,
            hints: vec![],
            next_commands: vec![],
            bodies_included: None,
            indexing: None,
            coverage_gaps: vec![],
            error: None,
        }
    }
}

/// Fit a serialized response under `max_chars` by dropping whole trailing
/// items from its `Section`-shaped lists, largest list first. `showing`
/// shrinks with the items, `truncated` is set, and one disclosure hint
/// naming `output.max_response_chars` lands on the largest list before
/// measuring, so its own length is budgeted. `count` and per-item shape
/// are never touched, and the value is never sliced: when even zero items
/// stays over the ceiling, the reduced-but-whole JSON is emitted as-is.
///
/// `measure` must serialize exactly the string the caller will emit —
/// the ceiling guards emitted characters, not an estimate. Returns true
/// when any items were dropped.
///
/// A section carrying `bodies_included` has the field recounted against
/// the items that survive (count of items with a `body` key), and removed
/// when the section empties — its contract is structural equality with
/// the emitted items, so a fitted response must re-establish it. The
/// fitter never inserts the key where attachment didn't run.
///
/// Tail-dropping is safe because every ranked producer sorts best-first;
/// detection is structural (count + showing + items with
/// `items.len() == showing`, non-empty) and recursive, because commands
/// flatten their Section to the top level (`refs`, `usage`) or nest it
/// under a key (`map file`, `context`, `pack`) — by the time the response
/// reaches the output layer, only the shape identifies it.
/// Whether the response states a content budget of its own (`budget_tokens`),
/// which the command already fitted its answer to.
///
/// The transport ceiling is the bound for a caller who named none. Applying it
/// on top of a named one contradicts the caller twice over: it returns less
/// than was asked for, and it leaves the response's own accounting of what it
/// spent describing items that were then dropped.
pub fn declares_budget(value: &serde_json::Value) -> bool {
    value
        .get("budget_tokens")
        .is_some_and(serde_json::Value::is_number)
}

pub fn fit_to_char_budget(
    value: &mut serde_json::Value,
    max_chars: usize,
    measure: &dyn Fn(&serde_json::Value) -> usize,
) -> bool {
    if measure(value) <= max_chars {
        return false;
    }

    let mut candidates = Vec::new();
    collect_section_paths(value, &mut String::new(), &mut candidates);
    if candidates.is_empty() {
        return false;
    }
    // Largest items array first; the stable sort keeps document order on
    // ties, and collection order is document order.
    candidates.sort_by_key(|candidate| std::cmp::Reverse(candidate.items_len));

    if let Some(node) = value
        .pointer_mut(&candidates[0].path)
        .and_then(serde_json::Value::as_object_mut)
    {
        let hint = format!(
            "Response exceeded output.max_response_chars ({max_chars}); items were dropped to fit — narrow the query or raise the ceiling in .symora/config.toml"
        );
        if let Some(hints) = node
            .entry("hints")
            .or_insert_with(|| serde_json::Value::Array(Vec::new()))
            .as_array_mut()
        {
            hints.push(serde_json::Value::String(hint));
        }
    }

    let mut dropped = false;
    for candidate in &candidates {
        let original = match value
            .pointer_mut(&candidate.path)
            .and_then(serde_json::Value::as_object_mut)
            .and_then(|node| node.get("items"))
            .and_then(serde_json::Value::as_array)
        {
            Some(items) => items.clone(),
            None => continue,
        };

        // Binary-search the largest kept count in [0, len) that fits —
        // serialized length is monotonic in kept items, and keeping all
        // of them is excluded because the response is already over.
        let mut lo = 0usize;
        let mut hi = original.len() - 1;
        let mut best = None;
        while lo <= hi {
            let mid = lo + (hi - lo) / 2;
            apply_keep(value, &candidate.path, &original, mid);
            if measure(value) <= max_chars {
                best = Some(mid);
                lo = mid + 1;
            } else if mid == 0 {
                break;
            } else {
                hi = mid - 1;
            }
        }
        let keep = best.unwrap_or(0);
        apply_keep(value, &candidate.path, &original, keep);
        // `bodies_included` counts the items it is emitted beside, and
        // `apply_keep` recounts it against the survivors; `stale` speaks for
        // whichever of them came from a file that has since changed. An empty
        // section has nothing left for either to speak for.
        //
        // A partial keep leaves `stale` alone. This layer cannot tell which
        // rows it named, and the two ways of being wrong are not equal: a flag
        // kept over rows that are all current costs a rebuild nobody needed,
        // while a flag dropped over a row that is stale hands an agent aged
        // content as though it were fresh.
        if keep == 0
            && let Some(node) = value
                .pointer_mut(&candidate.path)
                .and_then(serde_json::Value::as_object_mut)
        {
            node.remove("bodies_included");
            node.remove("stale");
        }
        dropped = true;
        // The removals above shrink the response too, so a section emptied at
        // the ceiling can come in under it once they are gone. Measuring again
        // is what keeps the next section's items from being dropped for a
        // budget this one already met.
        if best.is_some() || measure(value) <= max_chars {
            return true;
        }
        // Even zero items leaves the response over the ceiling — keep the
        // emptied list and continue with the next-largest one.
    }
    dropped
}

struct SectionCandidate {
    /// JSON Pointer to the Section-shaped node.
    path: String,
    /// Serialized length of its `items` array — the shrink-order key.
    items_len: usize,
}

fn collect_section_paths(
    value: &serde_json::Value,
    path: &mut String,
    out: &mut Vec<SectionCandidate>,
) {
    match value {
        serde_json::Value::Object(map) => {
            if is_section_shaped(map) {
                let items_len = serde_json::to_string(&map["items"])
                    .map(|s| s.len())
                    .unwrap_or(0);
                out.push(SectionCandidate {
                    path: path.clone(),
                    items_len,
                });
            }
            for (key, child) in map {
                let base = path.len();
                path.push('/');
                for ch in key.chars() {
                    match ch {
                        '~' => path.push_str("~0"),
                        '/' => path.push_str("~1"),
                        _ => path.push(ch),
                    }
                }
                collect_section_paths(child, path, out);
                path.truncate(base);
            }
        }
        serde_json::Value::Array(items) => {
            for (index, child) in items.iter().enumerate() {
                let base = path.len();
                path.push('/');
                path.push_str(&index.to_string());
                collect_section_paths(child, path, out);
                path.truncate(base);
            }
        }
        _ => {}
    }
}

/// The structural Section test: the typed key triple plus the
/// `items.len() == showing` consistency check. Non-empty `items` also
/// excludes `Section::error`'s 0/0/[] shape — there is nothing to drop.
fn is_section_shaped(map: &serde_json::Map<String, serde_json::Value>) -> bool {
    if map
        .get("count")
        .and_then(serde_json::Value::as_u64)
        .is_none()
    {
        return false;
    }
    let Some(showing) = map.get("showing").and_then(serde_json::Value::as_u64) else {
        return false;
    };
    let Some(items) = map.get("items").and_then(serde_json::Value::as_array) else {
        return false;
    };
    !items.is_empty() && items.len() as u64 == showing
}

/// Keep the first `keep` items of `original` at the section under `path`,
/// with `showing` and `truncated` kept consistent. `truncated: true` is
/// always correct here: `keep` < the original `showing` <= `count`.
/// `bodies_included`, when present, is recounted against the kept items
/// so its structural equality survives every probe.
fn apply_keep(
    value: &mut serde_json::Value,
    path: &str,
    original: &[serde_json::Value],
    keep: usize,
) {
    if let Some(node) = value
        .pointer_mut(path)
        .and_then(serde_json::Value::as_object_mut)
    {
        node.insert(
            "items".to_string(),
            serde_json::Value::Array(original[..keep].to_vec()),
        );
        node.insert("showing".to_string(), serde_json::Value::from(keep));
        node.insert("truncated".to_string(), serde_json::Value::Bool(true));
        if node.contains_key("bodies_included") {
            let with_body = original[..keep]
                .iter()
                .filter(|item| item.get("body").is_some())
                .count();
            node.insert(
                "bodies_included".to_string(),
                serde_json::Value::from(with_body),
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn complete_section_emits_count_showing_and_no_truncation() {
        let value = serde_json::to_value(Section::new(vec![1, 2, 3])).unwrap();
        assert_eq!(value["count"], 3);
        assert_eq!(value["showing"], 3);
        assert_eq!(value["items"], serde_json::json!([1, 2, 3]));
        assert!(value.get("truncated").is_none());
        assert!(value.get("hints").is_none());
        assert!(value.get("next_commands").is_none());
        assert!(value.get("error").is_none());
    }

    #[test]
    fn capped_section_derives_truncated_from_showing_lt_count() {
        let value = serde_json::to_value(Section::with_total(vec![1, 2], 10)).unwrap();
        assert_eq!(value["count"], 10);
        assert_eq!(value["showing"], 2);
        assert_eq!(value["truncated"], true);
    }

    #[test]
    fn count_never_underreports_emitted_items() {
        let section = Section::with_total(vec![1, 2, 3], 1);
        assert_eq!(section.count, 3);
        assert!(!section.truncated);
    }

    #[test]
    fn hints_and_next_commands_serialize_only_when_present() {
        let value = serde_json::to_value(
            Section::new(vec![1])
                .with_hints(vec!["narrow it".to_string()])
                .with_next_commands(vec!["symora map file src/a.rs".to_string()]),
        )
        .unwrap();
        assert_eq!(value["hints"][0], "narrow it");
        assert_eq!(value["next_commands"][0], "symora map file src/a.rs");
    }

    #[test]
    fn stale_serializes_only_when_true() {
        let stale = serde_json::to_value(Section::new(vec![1]).with_stale(true)).unwrap();
        assert_eq!(stale["stale"], true);

        let fresh = serde_json::to_value(Section::new(vec![1]).with_stale(false)).unwrap();
        assert!(fresh.get("stale").is_none());
    }

    #[test]
    fn indexing_marker_serializes_only_when_degraded() {
        let degraded = serde_json::to_value(
            Section::new(vec![1])
                .with_indexing(Some(crate::models::lsp::IndexingDegradation::TimedOut)),
        )
        .unwrap();
        assert_eq!(degraded["indexing"], "timed_out");

        let healthy = serde_json::to_value(Section::new(vec![1]).with_indexing(None)).unwrap();
        assert!(healthy.get("indexing").is_none());
    }

    #[test]
    fn bodies_included_serializes_only_when_present() {
        let some =
            serde_json::to_value(Section::new(vec![1, 2]).with_bodies_included(Some(2))).unwrap();
        assert_eq!(some["bodies_included"], 2);

        // Zero is meaningful — attachment ran, none admitted — not filler.
        let zero =
            serde_json::to_value(Section::new(vec![1]).with_bodies_included(Some(0))).unwrap();
        assert_eq!(zero["bodies_included"], 0);

        let none = serde_json::to_value(Section::new(vec![1]).with_bodies_included(None)).unwrap();
        assert!(none.get("bodies_included").is_none());
    }

    #[test]
    fn section_round_trips_through_the_wire() {
        let wire = serde_json::to_value(Section::with_total(vec![1, 2], 7)).unwrap();
        let parsed: Section<i32> = serde_json::from_value(wire).unwrap();
        assert_eq!(parsed.count, 7);
        assert_eq!(parsed.showing, 2);
        assert!(parsed.truncated);
        assert!(parsed.hints.is_empty());
    }

    /// The full set of keys is the public list contract
    /// (`.claude/rules/json-output-stability.md`). Pinning it here means a
    /// new envelope field can't slip in silently — adding one is a
    /// deliberate, breaking change that updates this assertion too.
    #[test]
    fn full_envelope_has_exactly_the_contract_keys() {
        let mut section = Section::with_total(vec![1, 2], 9)
            .with_hints(vec!["h".to_string()])
            .with_next_commands(vec!["c".to_string()])
            .with_indexing(Some(crate::models::lsp::IndexingDegradation::TimedOut))
            .with_stale(true)
            .with_bodies_included(Some(1))
            .with_coverage_gaps(vec![CoverageGap {
                language: "rust".to_string(),
                reason: "not_indexed".to_string(),
            }]);
        section.error = Some(crate::cli::OutputError::not_found("e"));

        let value = serde_json::to_value(section).unwrap();
        let mut keys: Vec<&str> = value
            .as_object()
            .unwrap()
            .keys()
            .map(String::as_str)
            .collect();
        keys.sort_unstable();
        assert_eq!(
            keys,
            [
                "bodies_included",
                "count",
                "coverage_gaps",
                "error",
                "hints",
                "indexing",
                "items",
                "next_commands",
                "showing",
                "stale",
                "truncated",
            ]
        );
    }

    /// An undecorated, complete result carries only the three always-on
    /// keys. If any optional field loses its `skip_serializing_if`, it
    /// surfaces here — agents must never have to parse zero/empty filler.
    #[test]
    fn minimal_envelope_omits_every_optional_key() {
        let value = serde_json::to_value(Section::new(vec![1, 2, 3])).unwrap();
        let mut keys: Vec<&str> = value
            .as_object()
            .unwrap()
            .keys()
            .map(String::as_str)
            .collect();
        keys.sort_unstable();
        assert_eq!(keys, ["count", "items", "showing"]);
    }

    #[test]
    fn error_section_is_empty_and_structured() {
        let value = serde_json::to_value(Section::<i32>::error(
            crate::cli::OutputError::not_found("nope"),
        ))
        .unwrap();
        assert_eq!(value["count"], 0);
        assert_eq!(value["showing"], 0);
        assert_eq!(value["error"]["code"], "not_found");
    }

    fn compact_chars(value: &serde_json::Value) -> usize {
        serde_json::to_string(value)
            .map(|s| s.chars().count())
            .unwrap_or(usize::MAX)
    }

    #[test]
    fn fit_drops_items_sets_truncated_and_discloses_hint() {
        let items: Vec<String> = (0..20)
            .map(|i| format!("item-{i:02}-{}", "x".repeat(40)))
            .collect();
        let mut value = serde_json::to_value(Section::new(items)).unwrap();

        assert!(fit_to_char_budget(&mut value, 500, &compact_chars));

        assert!(compact_chars(&value) <= 500);
        assert_eq!(value["count"], 20);
        let showing = value["showing"].as_u64().unwrap();
        assert!(showing < 20, "items must shrink, showing = {showing}");
        assert_eq!(value["items"].as_array().unwrap().len() as u64, showing);
        // Items drop from the end — the best-ranked leading items survive.
        assert_eq!(value["items"][0], format!("item-00-{}", "x".repeat(40)));
        assert_eq!(value["truncated"], true);
        assert!(
            value["hints"]
                .as_array()
                .unwrap()
                .iter()
                .any(|h| h.as_str().unwrap().contains("output.max_response_chars"))
        );
    }

    /// A command that fitted its answer to a budget the caller named has
    /// already answered the question the ceiling asks. Re-fitting it returns
    /// less than was asked for and leaves the response's own accounting
    /// describing items it no longer carries.
    #[test]
    fn a_declared_budget_is_not_re_fitted() {
        let budgeted = serde_json::json!({
            "budget_tokens": 16000,
            "estimated_tokens": 15690,
            "files": { "count": 3, "showing": 3, "items": ["a", "b", "c"] }
        });
        assert!(declares_budget(&budgeted));

        let unbudgeted = serde_json::json!({
            "count": 3, "showing": 3, "items": ["a", "b", "c"]
        });
        assert!(!declares_budget(&unbudgeted));

        let mut value = unbudgeted.clone();
        assert!(fit_to_char_budget(&mut value, 20, &|v| {
            serde_json::to_string(v).unwrap().chars().count()
        }));
    }

    /// Fields that speak for the items go with them. `stale` describes the
    /// files behind emitted rows, so a section the fitter emptied has nothing
    /// left for it to be about — and a reader would take it for a claim about
    /// rows that are no longer there.
    /// `stale` names files this layer cannot see, so once the fitter drops any
    /// row it might have spoken for, the claim cannot be checked against what
    /// is left — a section whose stale row was in the dropped tail would
    /// otherwise send an agent to rebuild for rows that never moved.
    #[test]
    fn a_partial_keep_keeps_the_claim_it_cannot_narrow() {
        let items: Vec<String> = (0..20)
            .map(|i| format!("item-{i:02}-{}", "x".repeat(40)))
            .collect();
        let mut value = serde_json::to_value(Section::new(items).with_stale(true)).unwrap();

        assert!(fit_to_char_budget(&mut value, 600, &compact_chars));

        let showing = value["showing"].as_u64().unwrap();
        assert!(showing > 0 && showing < 20, "a partial keep: {showing}");
        assert_eq!(
            value["stale"], true,
            "this layer cannot tell whether the stale row is one of the survivors, \
             and only one of the two answers can hand an agent aged content as fresh: {value}"
        );
    }

    #[test]
    fn fit_drops_the_fields_that_spoke_for_the_items_it_removed() {
        let items: Vec<String> = (0..20)
            .map(|i| format!("item-{i:02}-{}", "x".repeat(40)))
            .collect();
        let mut value = serde_json::to_value(Section::new(items).with_stale(true)).unwrap();
        assert_eq!(value["stale"], true);

        assert!(fit_to_char_budget(&mut value, 120, &compact_chars));

        assert_eq!(value["showing"], 0, "the budget leaves no room for items");
        assert!(
            value.get("stale").is_none(),
            "an empty section carries no claim about the files behind its rows: {value}"
        );
    }

    /// Emptying a section removes the fields that spoke for its items, and
    /// that shrinks the response as well. A fit reached that way is still a
    /// fit: continuing would drop the NEXT section's items to meet a ceiling
    /// this one already met. Stated as the property rather than against one
    /// ceiling, because the window where it shows is only as wide as the
    /// removed fields and the disclosure hint's own length moves with the
    /// ceiling.
    #[test]
    fn no_section_is_trimmed_for_a_ceiling_an_earlier_one_already_met() {
        let wide: Vec<String> = (0..8)
            .map(|i| format!("w-{i}-{}", "x".repeat(60)))
            .collect();
        let narrow: Vec<String> = (0..4).map(|i| format!("n-{i}")).collect();
        let value = serde_json::json!({
            "wide": serde_json::to_value(Section::new(wide).with_stale(true)).unwrap(),
            "narrow": serde_json::to_value(Section::new(narrow)).unwrap(),
        });

        for budget in 100..600 {
            let mut fitted = value.clone();
            if !fit_to_char_budget(&mut fitted, budget, &compact_chars) {
                continue;
            }
            if compact_chars(&fitted) > budget {
                // Even emptying everything left it over; nothing to judge.
                continue;
            }
            let mut untrimmed = fitted.clone();
            untrimmed["narrow"] = value["narrow"].clone();
            if compact_chars(&untrimmed) <= budget {
                assert_eq!(
                    fitted["narrow"]["showing"], 4,
                    "ceiling {budget}: the second section was trimmed although \
                     leaving it whole would have fit: {fitted}"
                );
            }
        }
    }

    #[test]
    fn fit_is_noop_under_budget() {
        let mut value = serde_json::to_value(Section::new(vec![1, 2, 3])).unwrap();
        let before = value.clone();

        assert!(!fit_to_char_budget(&mut value, 10_000, &compact_chars));
        assert_eq!(value, before);
    }

    #[test]
    fn fit_ignores_non_section_values() {
        // TestCoverageOutput-like: count + files, no showing/items.
        let mut coverage = serde_json::json!({
            "count": 3,
            "files": ["tests/a.rs", "tests/b.rs", "tests/c.rs"],
        });
        let before = coverage.clone();
        assert!(!fit_to_char_budget(&mut coverage, 10, &compact_chars));
        assert_eq!(coverage, before);

        // count + items without showing is not a Section either.
        let mut no_showing = serde_json::json!({ "count": 3, "items": [1, 2, 3] });
        let before = no_showing.clone();
        assert!(!fit_to_char_budget(&mut no_showing, 10, &compact_chars));
        assert_eq!(no_showing, before);
    }

    #[test]
    fn fit_handles_flattened_target_plus_section_shape() {
        // RefsOutput / UsageOutput flatten their Section beside `target`.
        let items: Vec<serde_json::Value> = (1..=8)
            .map(|line| {
                serde_json::json!({
                    "file": "src/very/long/path/to/handler.rs",
                    "line": line,
                    "column": 12,
                })
            })
            .collect();
        let mut value = serde_json::json!({
            "target": { "name": "process", "kind": "function", "file": "src/main.rs", "line": 12 },
            "count": 8,
            "showing": 8,
            "items": items,
        });

        assert!(fit_to_char_budget(&mut value, 550, &compact_chars));

        assert!(compact_chars(&value) <= 550);
        assert_eq!(value["count"], 8);
        assert!(value["showing"].as_u64().unwrap() < 8);
        assert_eq!(value["truncated"], true);
        // Sibling payload outside the Section is untouched.
        assert_eq!(value["target"]["name"], "process");
    }

    #[test]
    fn fit_handles_nested_section_shape() {
        // MapFileOutput-like: the Section nests under a key.
        let items: Vec<serde_json::Value> = (1..=10)
            .map(|line| {
                serde_json::json!({
                    "name": format!("function_number_{line:02}"),
                    "kind": "function",
                    "line": line,
                })
            })
            .collect();
        let mut value = serde_json::json!({
            "language": "rust",
            "symbols": { "count": 10, "showing": 10, "items": items },
        });

        assert!(fit_to_char_budget(&mut value, 450, &compact_chars));

        assert!(compact_chars(&value) <= 450);
        assert_eq!(value["language"], "rust");
        assert_eq!(value["symbols"]["count"], 10);
        assert!(value["symbols"]["showing"].as_u64().unwrap() < 10);
        assert_eq!(value["symbols"]["truncated"], true);
        assert!(
            value["symbols"]["hints"]
                .as_array()
                .unwrap()
                .iter()
                .any(|h| h.as_str().unwrap().contains("output.max_response_chars"))
        );
    }

    fn body_bearing_callees(bodies_included: Option<usize>) -> serde_json::Value {
        let mut section = serde_json::json!({
            "count": 4,
            "showing": 4,
            "items": [
                { "name": "alpha", "body": format!("fn alpha() {{ {} }}", "x".repeat(400)) },
                { "name": "beta" },
                { "name": "gamma", "body": format!("fn gamma() {{ {} }}", "y".repeat(400)) },
                { "name": "delta" },
            ],
        });
        if let Some(n) = bodies_included {
            section["bodies_included"] = serde_json::Value::from(n);
        }
        serde_json::json!({ "callees": section })
    }

    /// `bodies_included`'s contract is structural equality with the
    /// emitted items — a fitted response must recount it (never exceed
    /// `showing`, never describe dropped items), remove it when the
    /// section empties, and never invent it where attachment didn't run.
    #[test]
    fn fit_recounts_bodies_included() {
        // (a) Dropping the body-carrying tail recounts to the survivors.
        let mut value = body_bearing_callees(Some(2));
        assert!(fit_to_char_budget(&mut value, 800, &compact_chars));
        let section = &value["callees"];
        assert_eq!(section["showing"], 2, "ceiling must drop the last 2 items");
        assert_eq!(section["bodies_included"], 1);
        let showing = section["showing"].as_u64().unwrap();
        assert!(section["bodies_included"].as_u64().unwrap() <= showing);

        // (b) An emptied section has nothing to disclose — key removed.
        let mut value = body_bearing_callees(Some(2));
        assert!(fit_to_char_budget(&mut value, 50, &compact_chars));
        assert!(value["callees"]["items"].as_array().unwrap().is_empty());
        assert!(value["callees"].get("bodies_included").is_none());

        // (c) Coincidental `body` keys never grow the field where body
        // attachment didn't run.
        let mut value = body_bearing_callees(None);
        assert!(fit_to_char_budget(&mut value, 800, &compact_chars));
        assert!(value["callees"]["showing"].as_u64().unwrap() < 4);
        assert!(value["callees"].get("bodies_included").is_none());
    }

    #[test]
    fn fit_never_slices_when_envelope_alone_exceeds() {
        let mut value =
            serde_json::to_value(Section::new(vec!["a".to_string(), "b".to_string()])).unwrap();

        // A ceiling smaller than the empty envelope: everything droppable
        // is dropped, and the whole — still valid — JSON is emitted over
        // budget rather than sliced.
        assert!(fit_to_char_budget(&mut value, 5, &compact_chars));

        assert_eq!(value["count"], 2);
        assert_eq!(value["showing"], 0);
        assert!(value["items"].as_array().unwrap().is_empty());
        assert_eq!(value["truncated"], true);
        assert!(compact_chars(&value) > 5);
        serde_json::to_string(&value).expect("reduced value must stay serializable");
    }
}

/// File location with optional source snippet (relative path by default).
#[derive(Debug, Clone, Serialize)]
pub struct LocationOutput {
    pub file: String,
    pub line: u32,
    pub column: u32,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub snippet: Option<String>,
    /// Carried from the source [`crate::models::symbol::Location`]: present
    /// (and `true`) only when `column` is a degraded wire-offset guess (the
    /// target line was unreadable). Omitted in the common case, so an agent
    /// trusts the column unless this discloses otherwise.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub degraded_column: Option<bool>,
}

impl LocationOutput {
    /// Create from absolute path, converting to relative when within `root`.
    /// For a location built from bare coordinates (no source `Location`), the
    /// column is always a normally-decoded value — never degraded.
    pub fn from_path(path: &Path, line: u32, column: u32, root: &Path) -> Self {
        let file = path
            .strip_prefix(root)
            .map(|p| p.display().to_string())
            .unwrap_or_else(|_| path.display().to_string());

        Self {
            file,
            line,
            column,
            snippet: None,
            degraded_column: None,
        }
    }

    /// Create from a model `Location`, carrying its `degraded_column` flag so
    /// the disclosure survives to the emitted JSON. The single boundary every
    /// converter-derived location should cross — a degraded column is only ever
    /// produced there.
    pub fn from_location(location: &crate::models::symbol::Location, root: &Path) -> Self {
        Self {
            degraded_column: location.degraded_column,
            ..Self::from_path(&location.file, location.line, location.column, root)
        }
    }
}
