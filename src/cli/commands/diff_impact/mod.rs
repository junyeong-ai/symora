use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::path::{Path, PathBuf};

use anyhow::Result;
use clap::Args;
use serde::Serialize;

use crate::app::App;
use crate::cli::analysis::LocationAnalysis;
use crate::cli::response::disclosure::LowerBound;
use crate::cli::response::{CallHierarchyOutput, LocationOutput};
use crate::cli::utils::{members_at_deletion, members_at_line, shell_arg};
use crate::cli::{SymbolBackend, declared_in};
use crate::models::lsp::FindSymbolsOptions;
use crate::models::symbol::{Language, Symbol, SymbolKind};
use crate::services::store::{SymbolExtractor, TypeHeaders};

mod git;

use git::{
    Base, DiffHunk, ParsedDiff, changed_files, git_show, is_ancestor_of_head, parse_git_diff,
    resolve_base, unmerged_files, unstaged_files,
};

#[derive(Args, Debug)]
pub struct DiffImpactArgs {
    /// Revision the working tree is compared with: one revision, not a range.
    /// For a branch's own changes, pass `$(git merge-base main HEAD)`
    #[arg(default_value = "HEAD")]
    pub revision: String,

    /// Compare the staged index, instead of the working tree, with the revision
    #[arg(long)]
    pub staged: bool,

    /// Include caller analysis for changed symbols
    #[arg(long)]
    pub callers: bool,

    /// Maximum changed symbols to analyze (0 = unlimited)
    #[arg(long, default_value = "50")]
    pub max_symbols: usize,
}

#[derive(Debug, Serialize)]
pub struct DiffImpactOutput {
    pub revision: String,
    pub changed_files_count: usize,
    pub changed_symbols_count: usize,
    pub total_references: usize,
    pub coverage: DiffCoverage,
    /// Files whose changes could not be measured in full: nothing could read
    /// the symbols of their changed lines (removed lines are read by the
    /// grammar compiled into symora alone), they are symbolic links, whose
    /// lines are a path rather than source, git reports them as binary and
    /// names no lines, or — with `--staged` — unstaged edits sit over the
    /// staged ones, so the lines the diff names are not the lines on disk.
    /// What was not measured is absent from `changes`, so the result is a
    /// lower bound for these files. `hints` names the binary and staged causes; a file listed
    /// without one is a link or one whose symbols could not be read. Omitted
    /// when empty.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub unmeasured_files: Vec<String>,
    /// The analysis stopped before running out of changed symbols, so every
    /// count here is a lower bound. Carried in the shared shape's words so a
    /// reader treats it as it treats any other short answer. Omitted when the
    /// whole diff was analysed.
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub incomplete: bool,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub hints: Vec<String>,
    pub changes: Vec<ChangedSymbolImpact>,
}

/// Test coverage summary for diff analysis (aggregate over all changed symbols)
#[derive(Debug, Serialize)]
pub struct DiffCoverage {
    pub with_tests: usize,
    pub without_tests: usize,
    /// Tested fraction of the *measurable* (Added/Modified) symbols. Omitted
    /// when nothing was measurable (an empty or pure-deletion diff), so a ratio
    /// is never asserted over zero measurements as a vacuous 1.0.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub ratio: Option<f32>,
}

/// Changed symbol impact data (pure fact). Added/Modified rows carry the
/// symbol's current-tree identity and reference counts. Deleted rows carry the
/// pre-image identity and OMIT references — a deleted symbol has no current
/// references to count, and a literal `0` would read as a verified "no
/// references". The omitted fields make that absence structural, never a
/// synthesized zero.
#[derive(Debug, Serialize)]
pub struct ChangedSymbolImpact {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub kind: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub location: Option<LocationOutput>,
    pub change_type: ChangeType,
    /// Total reference count (Added/Modified only).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub refs: Option<usize>,
    /// Test code references (Added/Modified only).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub test_refs: Option<usize>,
    /// Production code references (Added/Modified only).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub prod_refs: Option<usize>,
    /// Present only when an Added/Modified symbol's reference counts are not
    /// authoritative: "unavailable" (the reference query failed, so `refs` are
    /// absent because UNKNOWN, not zero) or "indexing_degraded" (the query ran
    /// under a warming index, so the counts are a lower bound). The same
    /// `*_status` disclosure idiom as `callers_status`, so a reference-count
    /// limitation is at least as disclosed as a partial (callers-only) one.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub refs_status: Option<&'static str>,
    /// Present only when callers were requested but the list is not authoritative:
    /// "unavailable" (the incoming-call query failed) or "indexing_degraded" (it
    /// ran under degraded workspace indexing, so the list is a lower bound). An
    /// empty `callers` then means "unknown", not a verified "no callers".
    #[serde(skip_serializing_if = "Option::is_none")]
    pub callers_status: Option<&'static str>,
    /// Present only on Deleted rows: whether the deleted symbol was identified
    /// from the pre-image. The disclosure that the row is a deletion fact, not
    /// a live-symbol measurement.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub deletion: Option<DeletionResolution>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub callers: Vec<CallHierarchyOutput>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ChangeType {
    Added,
    Modified,
    Deleted,
}

/// How a Deleted row's symbol was resolved. Mirrors the `DispatchStatus` /
/// `IndexingDegradation` disclosure idiom: a typed enum naming the state,
/// omitted when not applicable. The two non-`Resolved` states are kept distinct
/// because they license different claims: `NoSymbolInRange` was checked against
/// the pre-image, so lines removed inside a surviving callable are reported as
/// that callable's `Modified` instead; `PreimageUnavailable` could not be
/// checked at all, so what was deleted is unknown and the row stays a disclosed
/// deletion.
#[derive(Debug, Clone, Copy, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum DeletionResolution {
    /// The deleted symbol was identified from the pre-image (old git tree); its
    /// references are not recomputed (it no longer exists).
    Resolved,
    /// The built-in grammar read the pre-image and found no declaration in the
    /// removed lines. The grammars read functions, methods, types and the like,
    /// not every declaration form (a TypeScript `const` holding a value, a C
    /// prototype), so this is not proof that none was removed.
    NoSymbolInRange,
    /// The pre-image itself could not be read (the `git show` of the old tree
    /// failed), so what was deleted is unknown — never guessed from diff text or
    /// a live neighbour, and never reclassified to a live `Modified`.
    PreimageUnavailable,
}

pub async fn execute(args: DiffImpactArgs, app: &App) -> Result<()> {
    let ctx = &app.output;
    let root = ctx.root();

    let base = resolve_base(root, &args.revision)?;
    let ParsedDiff {
        mut hunks,
        binary,
        links,
    } = parse_git_diff(root, base.tree_ish(), args.staged)?;
    let changed_files = changed_files(root, base.tree_ish(), args.staged)?;
    let mut hints = Vec::new();
    // Files are told apart by path until they are rendered: two names that
    // are not UTF-8 can render alike.
    let mut unmeasured: Vec<PathBuf> = Vec::new();

    if let Base::Commit(commit) = &base
        && !is_ancestor_of_head(root, commit)
    {
        hints.push(format!(
            "`{rev}` is not an ancestor of HEAD, so this diff also takes back what `{rev}` \
             gained after this branch left it; `symora diff-impact -- \"$(git merge-base -- {rev_arg} HEAD)\"` \
             measures this branch alone.",
            rev = args.revision,
            rev_arg = shell_arg(&args.revision),
        ));
    }

    let binary: BTreeSet<PathBuf> = binary.into_iter().collect();
    if !binary.is_empty() {
        hints.push(format!(
            "git reports {} as binary, so which of their lines changed is not known; a `-diff` \
             or `binary` attribute does this to a text file.",
            displayed(&binary, root)
        ));
        unmeasured.extend(binary);
    }
    unmeasured.extend(links);

    if args.staged {
        let unstaged = unstaged_files(root)?;
        let unmerged = unmerged_files(root, base.tree_ish())?;
        // A file with no staged lines, a pure rename or a mode change, has
        // nothing under the unstaged edits; a conflicted file has no staged
        // lines of its own to measure.
        let overlaid: BTreeSet<PathBuf> = hunks
            .iter()
            .map(|h| &h.file)
            .chain(&unmerged)
            .filter(|file| unstaged.contains(*file) && !unmeasured.contains(file))
            .cloned()
            .collect();
        hunks.retain(|h| !unstaged.contains(&h.file));
        if !overlaid.is_empty() {
            hints.push(format!(
                "Unstaged edits sit over the staged ones in {}, so the staged lines are not the \
                 lines on disk; stage or stash those edits to measure them.",
                displayed(&overlaid, root)
            ));
            unmeasured.extend(overlaid);
        }
    }

    let HunkAnalysis {
        changes,
        unmeasured: unreadable,
        capped,
        ..
    } = analyze_hunks(app, &hunks, base.tree_ish(), args.callers, args.max_symbols).await;
    unmeasured.extend(unreadable);
    let mut listed = HashSet::new();
    unmeasured.retain(|file| listed.insert(file.clone()));
    if capped {
        // The cause of `incomplete` leads the hints.
        hints.insert(0, LowerBound::AnalysisCapped(args.max_symbols).hint());
    }

    // Coverage is measured only over rows that have reference counts
    // (Added/Modified). Deleted rows carry no refs — counting them as
    // "without tests" would pollute the ratio with symbols that have no live
    // references to test.
    let total_refs: usize = changes.iter().filter_map(|c| c.refs).sum();
    let measurable = changes.iter().filter(|c| c.refs.is_some()).count();
    let with_tests = changes
        .iter()
        .filter(|c| c.test_refs.is_some_and(|t| t > 0))
        .count();
    let without_tests = measurable.saturating_sub(with_tests);
    let coverage_ratio = if measurable == 0 {
        None
    } else {
        Some(with_tests as f32 / measurable as f32)
    };

    ctx.print_success(DiffImpactOutput {
        revision: args.revision,
        changed_files_count: changed_files.len(),
        changed_symbols_count: changes.len(),
        total_references: total_refs,
        coverage: DiffCoverage {
            with_tests,
            without_tests,
            ratio: coverage_ratio,
        },
        changes,
        unmeasured_files: unmeasured
            .iter()
            .map(|file| relative_display(file, root))
            .collect(),
        incomplete: capped,
        hints,
    });
    Ok(())
}

fn relative_display(file: &Path, root: &Path) -> String {
    file.strip_prefix(root)
        .unwrap_or(file)
        .display()
        .to_string()
}

fn displayed(files: &BTreeSet<PathBuf>, root: &Path) -> String {
    files
        .iter()
        .map(|file| relative_display(file, root))
        .collect::<Vec<_>>()
        .join(", ")
}

/// Symbol-level change type for a live hunk. An Added HUNK only makes the SYMBOL
/// Added when the symbol's declaration line falls inside the added range;
/// otherwise the symbol pre-existed and merely received inserted body lines, so
/// it was Modified. (With `git diff --unified=0`, an in-body insertion is an
/// Added hunk, `old_count==0`, whose lines sit inside a pre-existing symbol.)
/// Modified/Deleted hunks pass through. This mirrors the deletion path's
/// body-line reclassification, keeping addition and deletion symmetric.
fn symbol_change_type(
    hunk_type: &ChangeType,
    decl_line: u32,
    hunk_start: u32,
    hunk_count: u32,
) -> ChangeType {
    let decl_in_added_range = decl_line >= hunk_start && decl_line < hunk_start + hunk_count.max(1);
    if matches!(hunk_type, ChangeType::Added) && !decl_in_added_range {
        ChangeType::Modified
    } else {
        hunk_type.clone()
    }
}

/// The changed symbols a diff's hunks were assigned to, bounded by
/// `--max-symbols`, and the files whose lines could not be assigned.
struct HunkAnalysis {
    changes: Vec<ChangedSymbolImpact>,
    unmeasured: Vec<PathBuf>,
    max_symbols: usize,
    /// Work was left undone because the cap was reached.
    capped: bool,
}

impl HunkAnalysis {
    /// Whether another changed symbol may be reported. When none may, the
    /// analysis is recorded as having stopped short.
    fn has_room(&mut self) -> bool {
        let full = self.max_symbols > 0 && self.changes.len() >= self.max_symbols;
        self.capped |= full;
        !full
    }
}

async fn analyze_hunks(
    app: &App,
    hunks: &[DiffHunk],
    preimage_ref: &str,
    include_callers: bool,
    max_symbols: usize,
) -> HunkAnalysis {
    let root = app.output.root();
    let mut file_hunks: BTreeMap<&PathBuf, Vec<&DiffHunk>> = BTreeMap::new();
    for hunk in hunks {
        file_hunks.entry(&hunk.file).or_default().push(hunk);
    }

    let mut analysis = HunkAnalysis {
        changes: Vec::new(),
        unmeasured: Vec::new(),
        max_symbols,
        capped: false,
    };
    for (file, hunks) in file_hunks {
        if !analysis.has_room() {
            break;
        }

        // Current-tree symbols, loaded once when the file survives — needed both
        // for live (Added/Modified) hunks and to recognise a deletion that only
        // removed body lines of a surviving symbol (a Modified, not a deletion).
        // Where the grammar answers, it is asked for members alone: it also
        // reads declarations inside bodies it lists no symbol for (a Kotlin
        // `init` block, a callback), whose lines are the enclosing member's.
        let file_exists = file.exists();
        let on_disk = if file_exists {
            read_on_disk(file)
        } else {
            None
        };
        let served = if file_exists {
            Some(declared_in(app, file, FindSymbolsOptions::default()).await)
        } else {
            None
        };
        let current_symbols: Option<&[Symbol]> = match &served {
            Some(Ok(read)) if read.backend == SymbolBackend::Ast => {
                on_disk.as_ref().map(|now| now.members.as_slice())
            }
            Some(Ok(read)) => Some(read.symbols.as_slice()),
            _ => None,
        };
        let no_headers = TypeHeaders::default();
        let headers_now = on_disk.as_ref().map_or(&no_headers, |now| &now.headers);

        // One dedup set per file, keyed by declaration identity (line, column),
        // NOT name: two distinct same-named declarations (impl A::new vs
        // impl B::new) both count, while one symbol touched by several hunks
        // counts once. Spans deletion-derived and live rows alike.
        let mut seen: std::collections::HashSet<(u32, u32)> = std::collections::HashSet::new();

        // Removed lines are read from the base's version of the file, never the
        // current tree, and by the built-in grammar alone.
        let removing: Vec<&DiffHunk> = hunks.iter().filter(|h| h.old_count > 0).copied().collect();
        let preimage = removing
            .first()
            .map(|hunk| read_preimage(root, preimage_ref, &hunk.old_file));
        match &preimage {
            None => {}
            Some(Preimage::Unparsed) => analysis.unmeasured.push(file.clone()),
            Some(Preimage::Unavailable) => {
                for _ in &removing {
                    if !analysis.has_room() {
                        break;
                    }
                    analysis
                        .changes
                        .push(unresolved_deletion(DeletionResolution::PreimageUnavailable));
                }
            }
            Some(Preimage::Parsed(before)) => {
                let before = before.as_slice();

                // A pure deletion removed every declaration in its range from
                // where it stood.
                let mut deleted: Vec<Identity> = Vec::new();
                let mut modified_by_removal: Vec<&Symbol> = Vec::new();
                for hunk in removing
                    .iter()
                    .filter(|h| matches!(h.change_type, ChangeType::Deleted))
                {
                    if !analysis.has_room() {
                        break;
                    }
                    let declared: Vec<&Symbol> = declared_in_removed(before, hunk).collect();
                    for symbol in &declared {
                        if !analysis.has_room() {
                            break;
                        }
                        deleted.push(identity(symbol));
                        analysis
                            .changes
                            .push(deleted_row(symbol, &hunk.old_file, root));
                    }
                    let enclosing: Vec<&Symbol> = current_symbols
                        .map(|current| {
                            removed_lines_owners(before, current, headers_now, hunk, &declared)
                        })
                        .unwrap_or_default();
                    if enclosing.is_empty() && declared.is_empty() {
                        analysis
                            .changes
                            .push(unresolved_deletion(DeletionResolution::NoSymbolInRange));
                    }
                    modified_by_removal.extend(enclosing);
                }

                // Lines replaced in place may hold a declaration the change
                // kept (an edited signature) or one it removed (a rename). It
                // was removed when the file now declares fewer of its identity
                // than before, counting both versions with the same grammar,
                // and a pure deletion has not already accounted for it. When
                // more replaced declarations share such an identity than were
                // removed (overloads; same-named Go methods, whose receiver the
                // grammar does not record), a declaration edited in place is
                // declared again in the lines that replaced it, so those that
                // are not are the removed ones; past them, the first in file
                // order are reported.
                let replacing: Vec<&DiffHunk> = removing
                    .iter()
                    .filter(|h| matches!(h.change_type, ChangeType::Modified))
                    .copied()
                    .collect();
                if !replacing.is_empty() && analysis.has_room() {
                    match &on_disk {
                        None => analysis.unmeasured.push(file.clone()),
                        Some(now) => {
                            let after = &now.members;
                            let mut removed = counts(before.iter().map(identity));
                            for id in after.iter().map(identity).chain(deleted) {
                                if let Some(n) = removed.get_mut(&id) {
                                    *n = n.saturating_sub(1);
                                }
                            }
                            let replaced: Vec<(&DiffHunk, &Symbol)> = replacing
                                .iter()
                                .flat_map(|&hunk| {
                                    declared_in_removed(before, hunk)
                                        .map(move |symbol| (hunk, symbol))
                                })
                                .collect();
                            let declared_again = |hunk: &DiffHunk, symbol: &Symbol| {
                                let end = hunk.start_line.saturating_add(hunk.line_count);
                                after.iter().any(|kept| {
                                    (hunk.start_line..end).contains(&kept.location.line)
                                        && identity(kept) == identity(symbol)
                                })
                            };
                            let mut chosen = vec![false; replaced.len()];
                            for again in [false, true] {
                                for (i, (hunk, symbol)) in replaced.iter().enumerate() {
                                    if declared_again(hunk, symbol) != again {
                                        continue;
                                    }
                                    if let Some(n) = removed.get_mut(&identity(symbol))
                                        && *n > 0
                                    {
                                        *n -= 1;
                                        chosen[i] = true;
                                    }
                                }
                            }
                            for ((hunk, symbol), chosen) in replaced.into_iter().zip(chosen) {
                                if !chosen {
                                    continue;
                                }
                                if !analysis.has_room() {
                                    break;
                                }
                                analysis
                                    .changes
                                    .push(deleted_row(symbol, &hunk.old_file, root));
                            }
                        }
                    }
                }
                if let Some(current) = current_symbols {
                    for hunk in &replacing {
                        let declared: Vec<&Symbol> = declared_in_removed(before, hunk).collect();
                        modified_by_removal.extend(removed_lines_owners(
                            before,
                            current,
                            headers_now,
                            hunk,
                            &declared,
                        ));
                    }
                }
                for sym in modified_by_removal {
                    if !seen.insert((sym.location.line, sym.location.column)) {
                        continue;
                    }
                    if !analysis.has_room() {
                        break;
                    }
                    let impact = analyze_symbol_impact(
                        app,
                        file,
                        sym.clone(),
                        ChangeType::Modified,
                        include_callers,
                    )
                    .await;
                    analysis.changes.push(impact);
                }
            }
        }

        // Added/Modified hunks need the current tree.
        let live_hunks: Vec<&DiffHunk> = hunks
            .iter()
            .filter(|h| !matches!(h.change_type, ChangeType::Deleted))
            .copied()
            .collect();
        if live_hunks.is_empty() {
            continue;
        }
        let Some(symbols) = current_symbols else {
            // Added/Modified hunks exist but no current symbols. If the file is
            // present, find_symbols errored — disclose it as an unmeasured file
            // (a lower bound) instead of silently dropping its changes.
            if file_exists {
                analysis.unmeasured.push(file.clone());
            }
            continue;
        };

        for hunk in live_hunks {
            if !analysis.has_room() {
                break;
            }

            let removed_a_declaration = match &preimage {
                Some(Preimage::Parsed(before)) => {
                    declared_in_removed(before, hunk).next().is_some()
                }
                _ => false,
            };
            for sym in added_lines_owners(symbols, headers_now, hunk, removed_a_declaration) {
                if !seen.insert((sym.location.line, sym.location.column)) {
                    continue;
                }

                if !analysis.has_room() {
                    break;
                }

                let change_type = symbol_change_type(
                    &hunk.change_type,
                    sym.location.line,
                    hunk.start_line,
                    hunk.line_count,
                );

                let impact =
                    analyze_symbol_impact(app, file, sym.clone(), change_type, include_callers)
                        .await;

                analysis.changes.push(impact);
            }
        }
    }

    analysis
}

async fn analyze_symbol_impact(
    app: &App,
    file: &Path,
    sym: Symbol,
    change_type: ChangeType,
    include_callers: bool,
) -> ChangedSymbolImpact {
    let lsp = app.lsp.as_ref();
    let root = app.output.root();
    let name = sym.name.clone();
    let kind = sym.kind.to_string();
    let line = sym.location.line;
    let column = sym.location.column;

    let analysis = match LocationAnalysis::for_symbol(lsp, file, sym, root).await {
        Ok(a) => a,
        Err(_) => {
            // Measurement failed (LSP error/unavailable) — emit absent refs,
            // never a fabricated `0` that reads as a verified "no references".
            // Coverage already excludes rows whose refs are `None`.
            return ChangedSymbolImpact {
                name: Some(name),
                kind: Some(kind),
                location: Some(LocationOutput::from_path(file, line, column, root)),
                change_type,
                refs: None,
                test_refs: None,
                prod_refs: None,
                refs_status: Some("unavailable"),
                callers: vec![],
                callers_status: None,
                deletion: None,
            };
        }
    };

    let classified = analysis.classify(app.test_scope());

    // A failed incoming-call query, or one run under degraded indexing, must not
    // pass as a verified caller set: disclose it so an empty list is read as
    // "unknown"/"lower bound", never an authoritative "no callers".
    let (callers, callers_status) = if include_callers {
        match lsp.incoming_calls(file, line, column).await {
            Ok(calls) => {
                let status = calls.indexing.is_some().then_some("indexing_degraded");
                let items = calls
                    .data
                    .iter()
                    .take(app.config().lsp.calls_limit)
                    .map(|c| CallHierarchyOutput::from_item(c, root))
                    .collect();
                (items, status)
            }
            Err(_) => (vec![], Some("unavailable")),
        }
    } else {
        (vec![], None)
    };

    ChangedSymbolImpact {
        name: Some(name),
        kind: Some(kind),
        location: Some(LocationOutput::from_path(file, line, column, root)),
        change_type,
        refs: Some(classified.total),
        test_refs: Some(classified.test),
        prod_refs: Some(classified.prod),
        // A reference count from a query run under degraded indexing is a lower
        // bound, not a verified total — disclose it exactly as callers_status
        // does for the incoming-call query above.
        refs_status: analysis.indexing().is_some().then_some("indexing_degraded"),
        callers,
        callers_status,
        deletion: None,
    }
}

/// What the built-in grammar reads in the base's version of a file.
enum Preimage {
    /// What its removed lines declared is unknown: no grammar compiled into
    /// this binary reads the file's language, or the base's version is not
    /// text, as the current one is not when it cannot be read either.
    Unparsed,
    /// The base's version could not be read (`git show` failed).
    Unavailable,
    Parsed(Vec<Symbol>),
}

fn read_preimage(root: &Path, preimage_ref: &str, file: &Path) -> Preimage {
    let language = Language::from_path(file);
    if !SymbolExtractor::is_supported(language) {
        return Preimage::Unparsed;
    }
    let relpath = file.strip_prefix(root).unwrap_or(file);
    let Some(bytes) = git_show(root, preimage_ref, relpath) else {
        return Preimage::Unavailable;
    };
    match String::from_utf8(bytes) {
        Ok(content) => {
            Preimage::Parsed(SymbolExtractor::shared().extract_members(file, &content, language))
        }
        Err(_) => Preimage::Unparsed,
    }
}

/// The current members a hunk's removed lines belong to, read from what held
/// each line in the base. A line inside a surviving member or type modified
/// it: that one now ends before the hunk's new lines, or begins right after
/// them when the removed lines were its head (an annotation its range opens
/// on); the current file alone cannot tell such lines from those around them
/// (a Python function has no closing line). A line between members modified
/// their container when the hunk left nothing in its place and removed no
/// declaration, whose removal it would go with; lines the hunk put in its
/// place answer for it, read where they now stand. A line of a declaration
/// the hunk removed is that declaration's. `headers` are the current file's
/// type headers; the base is read by the grammar, which names no type
/// parameter, so it needs none.
fn removed_lines_owners<'a>(
    before: &[Symbol],
    current: &'a [Symbol],
    headers: &TypeHeaders,
    hunk: &DiffHunk,
    declared: &[&Symbol],
) -> Vec<&'a Symbol> {
    let (mut ends_before, mut begins_after, mut between) = (false, false, false);
    for line in hunk.old_start..hunk.old_start + hunk.old_count {
        let owners = members_at_line(before, &TypeHeaders::default(), line);
        if owners
            .iter()
            .any(|owner| declared.iter().any(|gone| std::ptr::eq(*gone, *owner)))
        {
            continue;
        }
        match owners.last() {
            Some(owner) if owner.location.effective_start().0 >= hunk.old_start => {
                begins_after = true
            }
            Some(owner) if !owner.kind.holds_members() => ends_before = true,
            _ => between = true,
        }
    }
    between &= declared.is_empty() && hunk.line_count == 0;
    // The last current line before the hunk's new lines: a pure deletion's
    // new start names it, a replacement's new start is its first new line.
    let point = if hunk.line_count == 0 {
        hunk.start_line
    } else {
        hunk.start_line.saturating_sub(1)
    };
    let mut owners = Vec::new();
    if ends_before {
        owners.extend(members_at_line(current, headers, point));
    }
    if begins_after {
        owners.extend(members_at_line(
            current,
            headers,
            point + hunk.line_count + 1,
        ));
    }
    if between {
        owners.extend(members_at_deletion(current, headers, point));
    }
    owners
}

/// The members a hunk's new lines belong to, by the rule removed lines are
/// read by (`members_at_line`). A line of a type's header (`headers`, as the
/// built-in grammar reads the current file) modified the type; so did a line
/// between its members, unless the hunk declares something in its new lines
/// or removed a declaration, whose change it goes with.
fn added_lines_owners<'a>(
    current: &'a [Symbol],
    headers: &TypeHeaders,
    hunk: &DiffHunk,
    removed_a_declaration: bool,
) -> Vec<&'a Symbol> {
    let lines = hunk.start_line..hunk.start_line + hunk.line_count.max(1);
    let owners: Vec<(u32, Vec<&Symbol>)> = lines
        .clone()
        .map(|line| (line, members_at_line(current, headers, line)))
        .collect();
    let declares = removed_a_declaration
        || owners
            .iter()
            .flat_map(|(_, held)| held)
            .any(|symbol| lines.contains(&symbol.location.line));
    let between = |line: u32, held: &[&Symbol]| {
        held.last().is_some_and(|owner| {
            owner.kind.holds_members()
                && owner.location.effective_start().0 < hunk.start_line
                && owner.location.line != line
                && !headers.lines.contains(&line)
        })
    };
    owners
        .into_iter()
        .filter(|(line, held)| !(declares && between(*line, held)))
        .flat_map(|(_, held)| held)
        .collect()
}

/// The base's declarations on the lines a hunk removed.
fn declared_in_removed<'a>(
    before: &'a [Symbol],
    hunk: &DiffHunk,
) -> impl Iterator<Item = &'a Symbol> {
    let lines = hunk.old_start..hunk.old_start.saturating_add(hunk.old_count);
    before
        .iter()
        .filter(move |symbol| lines.contains(&symbol.location.line))
}

/// What tells a declaration apart from the others in its file, as far as
/// whether a change removed it goes. Overloads share one.
type Identity = (Option<String>, String, SymbolKind);

fn identity(symbol: &Symbol) -> Identity {
    (symbol.container.clone(), symbol.name.clone(), symbol.kind)
}

fn counts(identities: impl Iterator<Item = Identity>) -> HashMap<Identity, usize> {
    let mut counts = HashMap::new();
    for id in identities {
        *counts.entry(id).or_insert(0) += 1;
    }
    counts
}

/// The file on disk as the built-in grammar reads it: the members that stand
/// for themselves, and the lines each type's header is written on.
struct OnDisk {
    members: Vec<Symbol>,
    headers: TypeHeaders,
}

/// `None` when the built-in grammar cannot read the file: no grammar reads
/// its language, or it is not text.
fn read_on_disk(file: &Path) -> Option<OnDisk> {
    let language = Language::from_path(file);
    if !SymbolExtractor::is_supported(language) {
        return None;
    }
    let content = std::fs::read_to_string(file).ok()?;
    let extractor = SymbolExtractor::shared();
    Some(OnDisk {
        members: extractor.extract_members(file, &content, language),
        headers: extractor.type_headers(file, &content, language),
    })
}

/// A declaration the change removed, read from the pre-image. Its position is
/// in the base's version of `old_file`: the only honest position for a symbol
/// that no longer exists in the current tree.
fn deleted_row(symbol: &Symbol, old_file: &Path, root: &Path) -> ChangedSymbolImpact {
    ChangedSymbolImpact {
        name: Some(symbol.name.clone()),
        kind: Some(symbol.kind.to_string()),
        location: Some(LocationOutput::from_path(
            old_file,
            symbol.location.line,
            symbol.location.column,
            root,
        )),
        change_type: ChangeType::Deleted,
        refs: None,
        test_refs: None,
        prod_refs: None,
        refs_status: None,
        callers: vec![],
        callers_status: None,
        deletion: Some(DeletionResolution::Resolved),
    }
}

/// A removal whose declarations were not identified, disclosed rather than
/// dropped.
fn unresolved_deletion(resolution: DeletionResolution) -> ChangedSymbolImpact {
    ChangedSymbolImpact {
        name: None,
        kind: None,
        location: None,
        change_type: ChangeType::Deleted,
        refs: None,
        test_refs: None,
        prod_refs: None,
        refs_status: None,
        callers: vec![],
        callers_status: None,
        deletion: Some(resolution),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    /// A line a hunk adds between a type's members goes with the member the
    /// hunk adds there, whether or not the built-in grammar reads the type's
    /// members: it reads no TypeScript enum member, which a server lists.
    #[test]
    fn an_added_member_takes_the_lines_added_with_it_whatever_the_grammar_reads() {
        use crate::models::symbol::Location;

        let content = "export enum Mode {\n  Fast,\n\n  Idle,\n  Slow,\n}\n";
        let path = PathBuf::from("m.ts");
        let headers = SymbolExtractor::new().type_headers(&path, content, Language::TypeScript);
        let member = |name: &str, line| {
            let end = 3 + name.len() as u32;
            Symbol::new(
                name.to_string(),
                SymbolKind::EnumMember,
                Location::full(path.clone(), line, 3, line, 3, line, end),
            )
        };
        let symbols = vec![
            Symbol::new(
                "Mode".to_string(),
                SymbolKind::Enum,
                Location::full(path.clone(), 1, 13, 1, 1, 6, 2),
            )
            .with_children(vec![
                member("Fast", 2),
                member("Idle", 4),
                member("Slow", 5),
            ]),
        ];
        let hunk = DiffHunk {
            file: path.clone(),
            old_file: path.clone(),
            start_line: 3,
            line_count: 2,
            old_start: 2,
            old_count: 0,
            change_type: ChangeType::Added,
        };
        let owners: Vec<&str> = added_lines_owners(&symbols, &headers, &hunk, false)
            .iter()
            .map(|symbol| symbol.name.as_str())
            .collect();
        assert_eq!(owners, ["Idle"]);
    }

    /// An Added HUNK only makes the SYMBOL Added when its declaration is inside
    /// the added range; an in-body insertion into a pre-existing symbol is
    /// Modified — symmetric with the deletion path's body-line reclassification.
    #[test]
    fn symbol_change_type_reclassifies_in_body_insertion() {
        // New function: declaration (line 5) inside the added range [5, 9).
        assert!(matches!(
            symbol_change_type(&ChangeType::Added, 5, 5, 4),
            ChangeType::Added
        ));
        // In-body insertion: enclosing symbol declared at line 1, added range
        // [3, 4) — declaration outside, so Modified, not Added.
        assert!(matches!(
            symbol_change_type(&ChangeType::Added, 1, 3, 1),
            ChangeType::Modified
        ));
        // Modified/Deleted hunks pass through unchanged.
        assert!(matches!(
            symbol_change_type(&ChangeType::Modified, 1, 3, 1),
            ChangeType::Modified
        ));
    }

    // The Deleted-row JSON contract: never a live neighbour's identity, never
    // a synthesized zero reference count.

    fn loc() -> LocationOutput {
        LocationOutput {
            file: "src/lib.rs".to_string(),
            line: 10,
            column: 1,
            snippet: None,
            degraded_column: None,
        }
    }

    #[test]
    fn deleted_resolved_row_omits_refs_and_discloses() {
        let row = ChangedSymbolImpact {
            name: Some("gone".to_string()),
            kind: Some("function".to_string()),
            location: Some(loc()),
            change_type: ChangeType::Deleted,
            refs: None,
            test_refs: None,
            prod_refs: None,
            refs_status: None,
            callers: vec![],
            callers_status: None,
            deletion: Some(DeletionResolution::Resolved),
        };
        let v = serde_json::to_value(row).unwrap();
        assert_eq!(v["change_type"], "deleted");
        assert_eq!(v["deletion"], "resolved");
        assert_eq!(v["name"], "gone");
        // No reference counts on a deleted symbol — keys absent, never 0.
        assert!(v.get("refs").is_none());
        assert!(v.get("test_refs").is_none());
        assert!(v.get("prod_refs").is_none());
    }

    #[test]
    fn deleted_preimage_unavailable_row_omits_identity() {
        let row = ChangedSymbolImpact {
            name: None,
            kind: None,
            location: None,
            change_type: ChangeType::Deleted,
            refs: None,
            test_refs: None,
            prod_refs: None,
            refs_status: None,
            callers: vec![],
            callers_status: None,
            deletion: Some(DeletionResolution::PreimageUnavailable),
        };
        let v = serde_json::to_value(row).unwrap();
        assert_eq!(v["deletion"], "preimage_unavailable");
        // Never a guessed name or a live neighbour's location.
        assert!(v.get("name").is_none());
        assert!(v.get("location").is_none());
    }

    #[test]
    fn modified_row_shape_is_unchanged() {
        // Added/Modified rows serialize refs as bare numbers and carry no
        // deletion key — byte-identical to the pre-change shape.
        let row = ChangedSymbolImpact {
            name: Some("touched".to_string()),
            kind: Some("function".to_string()),
            location: Some(loc()),
            change_type: ChangeType::Modified,
            refs: Some(3),
            test_refs: Some(1),
            prod_refs: Some(2),
            refs_status: None,
            callers: vec![],
            callers_status: None,
            deletion: None,
        };
        let v = serde_json::to_value(row).unwrap();
        assert_eq!(v["refs"], 3);
        assert_eq!(v["test_refs"], 1);
        assert!(v.get("deletion").is_none());
    }
}
