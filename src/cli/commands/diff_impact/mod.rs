use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::path::{Path, PathBuf};

use anyhow::Result;
use clap::Args;
use futures::StreamExt;
use serde::Serialize;

use crate::app::App;
use crate::cli::analysis::{Constructions, DerivedCallers, callers_from_references};
use crate::cli::errors::OutputError;
use crate::cli::python_members::{MemberGap, PythonMembers};
use crate::cli::response::disclosure::{
    Cap, CoverageReason, DIFF_IN_PARTS, LowerBound, name_some, with_cap,
};
use crate::cli::response::{
    CallHierarchyOutput, INDEXING_DEGRADED, LocationOutput, NOT_CALLABLE, REFERENCES_DERIVED,
    ReferenceCount, Section, share,
};
use crate::cli::utils::{members_at_deletion, members_at_line, shell_arg};
use crate::cli::{FileSymbols, SymbolBackend, declarations_of, declared_in, names_one};
use crate::constants::defaults::REFERENCE_COUNT_CONCURRENCY;
use crate::models::lsp::{CallHierarchyItem, FindSymbolsOptions};
use crate::models::symbol::{Language, Location, Symbol, SymbolKind};
use crate::services::symbols::{SymbolExtractor, TypeHeaders};

mod git;

use git::{
    Base, DiffHunk, ParsedDiff, changed_files, check_paths, git_show, is_ancestor_of_head,
    matched_files, parse_git_diff, resolve_base, unmerged_files, unstaged_files, untracked_files,
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

    /// Analyse only the changed files these git pathspecs match, read from
    /// the project root: a file or directory, a glob such as `*.py`, or
    /// `:!dir` to leave one out. A rename is analysed when either of its
    /// paths matches. References and callers are still searched across the
    /// whole project
    #[arg(short, long, value_name = "PATHSPEC")]
    pub path: Option<Vec<PathBuf>>,

    /// List each added or modified function's, method's and constructor's
    /// callers: the callables whose code holds one of the references its
    /// count was taken from, with no second search
    /// (`callers_status: references_derived`)
    #[arg(long)]
    pub callers: bool,

    /// Ask every added or modified symbol's callers of the language server's
    /// call hierarchy instead, a second workspace search per symbol; implies
    /// --callers
    #[arg(long)]
    pub call_hierarchy: bool,

    /// Maximum added and modified symbols whose references are counted, and
    /// apart from them, deleted symbols listed (0 = unlimited): a deletion,
    /// which has none to count, never takes a counted symbol's place
    #[arg(long, default_value_t = crate::constants::defaults::DIFF_IMPACT_MAX_SYMBOLS)]
    pub max_symbols: usize,
}

#[derive(Debug, Serialize)]
pub struct DiffImpactOutput {
    pub revision: String,
    /// The files the diff changes, those `--path` matches where it is given.
    pub changed_files_count: usize,
    /// With `--path`, the files the diff changes that it does not match,
    /// which nothing here measures; 0 where it matches every one. Omitted
    /// without `--path`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub excluded_files_count: Option<usize>,
    /// The reference counts of the changed symbols whose counts were
    /// measured, summed. Omitted when none was: a failed lookup counted
    /// nothing, and a deleted symbol has no live references to count.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub total_references: Option<usize>,
    /// How many added and modified rows carry each `refs_status`: rows whose
    /// count is a lower bound ("indexing_degraded") or was not taken (the
    /// other reasons), which make `total_references` and `coverage` lower
    /// bounds. Every status is counted, so one row a server cannot answer
    /// does not stand for the rows it did. Omitted when every count is
    /// authoritative.
    #[serde(skip_serializing_if = "BTreeMap::is_empty")]
    pub refs_status_counts: BTreeMap<&'static str, usize>,
    /// How many rows carry each `callers_status`, as `refs_status_counts`
    /// counts reference statuses. Omitted when every caller list asked for
    /// is the server's authoritative answer, and without `--callers`.
    #[serde(skip_serializing_if = "BTreeMap::is_empty")]
    pub callers_status_counts: BTreeMap<&'static str, usize>,
    pub coverage: DiffCoverage,
    /// Files whose changes could not be measured in full: nothing could read
    /// the symbols of their changed lines (removed lines are read by the
    /// grammar compiled into symora alone), they hold documentation or
    /// configuration (Markdown, MDX, YAML, TOML, JSON), whose keys and
    /// headings are not read as symbols, they are symbolic links, whose
    /// lines are a path rather than source, git reports them as binary and
    /// names no lines, with `--staged` unstaged edits sit over the staged
    /// ones, so the lines the diff names are not the lines on disk, their
    /// changes lie outside every declaration (a comment, a docstring, an
    /// import, a module-level statement) and gave no row, or `--max-symbols`
    /// refused one of their rows or stopped before reading them. What was
    /// not measured is absent from `changes`, so the result is
    /// a lower bound for these files. A hint names each file's cause.
    /// Omitted when empty.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub unmeasured_files: Vec<String>,
    /// `--max-symbols` stopped the analysis: the added and modified symbols,
    /// or the deletions, past it are not listed, and once the counted
    /// symbols reach it the files after the one it stopped in are not read,
    /// so every count here is a lower bound. `unmeasured_files` names the
    /// files it left unfinished. Carried in the shared shape's words so a
    /// reader treats it as it treats any other short answer. Omitted when the
    /// whole diff was analysed.
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub incomplete: bool,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub hints: Vec<String>,
    pub changes: Section<ChangedSymbolImpact>,
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
    pub ratio: Option<f64>,
}

/// Changed symbol impact data (pure fact). Added/Modified rows carry the
/// symbol's current-tree identity and reference counts. Deleted rows carry the
/// pre-image identity and OMIT references — a deleted symbol has no current
/// references to count, and a literal `0` would read as a verified "no
/// references". The omitted fields make that absence structural, never a
/// synthesized zero. A deletion whose declarations were not identified has no
/// `name` or `kind`; its position is the first line it removed.
#[derive(Debug, Serialize)]
pub struct ChangedSymbolImpact {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub kind: Option<String>,
    #[serde(flatten)]
    pub location: LocationOutput,
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
    /// authoritative: "member_shadowed" or "member_unresolved" (its Python
    /// workspace member resolves elsewhere, or is not known to resolve here,
    /// so the counts are a lower bound no retry raises), "indexing_degraded"
    /// (the query ran under degraded workspace analysis, so the counts are a
    /// lower bound), or why none was
    /// taken, in the `coverage_gaps` reasons — "timed_out", "unsupported",
    /// "server_not_installed", "not_consulted", or "unavailable" (the query
    /// failed, or was not asked where the server reads no position as the
    /// declaration) — with the counts absent rather than zero.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub refs_status: Option<&'static str>,
    /// Present only when callers were requested but the list is not
    /// authoritative: "indexing_degraded" (a lower bound), the reason the
    /// incoming-call query failed, as `refs_status` names one, or
    /// "unavailable" where no reference count was taken to ask it from, when
    /// `callers` is absent: what calls the symbol is unknown, not "nothing".
    #[serde(skip_serializing_if = "Option::is_none")]
    pub callers_status: Option<&'static str>,
    /// Present only on Deleted rows: whether the deleted symbol was identified
    /// from the pre-image. The disclosure that the row is a deletion fact, not
    /// a live-symbol measurement.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub deletion: Option<DeletionResolution>,
    /// With `--callers`, the incoming calls the server answered, in the shared
    /// list shape: `count` is every caller it named and `items` stops at
    /// `lsp.calls_limit`. Absent without `--callers` and where no list was
    /// obtained (`callers_status` says why).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub callers: Option<Section<CallHierarchyOutput>>,
}

/// What the diff did to a declaration, read from whether its identity — its
/// name path and kind, as the grammar compiled into symora reads both
/// versions of its file, the base's under the path it had there — is
/// declared in the base and in the current revision ([`Sides`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ChangeType {
    /// Declared in the current revision and not in the base.
    Added,
    /// Declared in both, wherever the diff moved it: its lines changed.
    Modified,
    /// Declared in the base and not in the current revision.
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
    let paths: &[PathBuf] = args.path.as_deref().unwrap_or_default();
    if args.path.as_ref().is_some_and(Vec::is_empty) {
        return Err(OutputError::invalid("path must contain at least one pathspec").into());
    }
    // A suggested command measures the same paths again.
    let options: String = paths
        .iter()
        .map(|path| format!(" --path={}", shell_arg(&path.display().to_string())))
        .collect();

    let base = resolve_base(root, &args.revision, &options)?;
    check_paths(root, base.tree_ish(), paths)?;
    let ParsedDiff {
        mut hunks,
        mut binary,
        mut links,
    } = parse_git_diff(root, base.tree_ish(), args.staged)?;
    let changed = changed_files(root, base.tree_ish(), args.staged)?;
    let mut hints = Vec::new();
    // Files are told apart by path until they are rendered: two names that
    // are not UTF-8 can render alike.
    let mut unmeasured: Vec<PathBuf> = Vec::new();

    if let Base::Commit(commit) = &base
        && !is_ancestor_of_head(root, commit)
    {
        hints.push(format!(
            "`{rev}` is not an ancestor of HEAD, so this diff also takes back what `{rev}` \
             gained after this branch left it; `symora diff-impact{options} -- \"$(git merge-base -- {rev_arg} HEAD)\"` \
             measures this branch alone.",
            rev = args.revision,
            rev_arg = shell_arg(&args.revision),
        ));
    }

    // `--path` confines the diff to the files it matches, a rename by
    // either of its paths, once the whole diff's renames are known.
    let (changed_files_count, excluded_files_count) = if paths.is_empty() {
        (changed.len(), None)
    } else {
        let matched = matched_files(root, base.tree_ish(), args.staged, paths)?;
        let held: HashSet<PathBuf> = changed
            .iter()
            .filter(|c| matched.contains(&c.file) || matched.contains(&c.old_file))
            .map(|c| c.file.clone())
            .collect();
        hunks.retain(|hunk| held.contains(&hunk.file));
        binary.retain(|file| held.contains(file));
        links.retain(|file| held.contains(file));
        let excluded = changed.len() - held.len();
        if excluded > 0 {
            hints.push(format!(
                "--path leaves out {excluded} of the {} files the diff changes, which this \
                 answer does not measure; another --path measures them.",
                changed.len()
            ));
        }
        (held.len(), Some(excluded))
    };

    let binary: BTreeSet<PathBuf> = binary.into_iter().collect();
    if !binary.is_empty() {
        hints.push(format!(
            "git reports {} as binary, so which of their lines changed is not known; a `-diff` \
             or `binary` attribute does this to a text file.",
            displayed(&binary, root)
        ));
        unmeasured.extend(binary);
    }
    let links: BTreeSet<PathBuf> = links.into_iter().collect();
    if !links.is_empty() {
        hints.push(format!(
            "Symbolic links hold the path they point to rather than source: {}.",
            displayed(&links, root)
        ));
        unmeasured.extend(links);
    }

    let documents: BTreeSet<PathBuf> = hunks
        .iter()
        .filter(|hunk| documents_or_configures(&hunk.file))
        .map(|hunk| hunk.file.clone())
        .collect();
    if !documents.is_empty() {
        hunks.retain(|hunk| !documents.contains(&hunk.file));
        let named: Vec<String> = documents
            .iter()
            .map(|file| relative_display(file, root))
            .collect();
        hints.push(format!(
            "Documentation and configuration files are not read as symbols: {}. Read their diff \
             for what changed.",
            name_some(&named)
        ));
        unmeasured.extend(documents);
    }

    if !args.staged {
        let untracked: Vec<String> = untracked_files(root, paths)?
            .iter()
            .map(|file| relative_display(file, root))
            .collect();
        if !untracked.is_empty() {
            hints.push(format!(
                "Untracked files are not in this diff: {}. `git add --intent-to-add -- <file>` \
                 puts one in it as a new file.",
                name_some(&untracked)
            ));
        }
    }

    if args.staged {
        let unstaged = unstaged_files(root)?;
        let unmerged = unmerged_files(root, base.tree_ish(), paths)?;
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
        mut changes,
        unmeasured: unreadable,
        unfinished,
        hints: row_hints,
    } = analyze_hunks(
        app,
        &hunks,
        base.tree_ish(),
        CallerSource::asked(&args),
        args.max_symbols,
    )
    .await;
    hints.extend(unmeasured_hints(&unreadable, root));
    unmeasured.extend(unreadable.into_iter().map(|(file, _)| file));
    let capped = !unfinished.is_empty();
    if capped {
        // The cause of `incomplete` leads the hints.
        let files: Vec<String> = unfinished
            .iter()
            .map(|file| relative_display(file, root))
            .collect();
        hints.insert(
            0,
            LowerBound::AnalysisCapped {
                symbols: args.max_symbols,
                files,
            }
            .hint(),
        );
        unmeasured.extend(unfinished);
    }
    let mut listed = HashSet::new();
    unmeasured.retain(|file| listed.insert(file.clone()));
    if let Some(hint) = qualify_member_rows(app, &mut changes) {
        hints.insert(usize::from(capped), hint);
    }
    hints.extend(row_hints);
    let timed_out = Some(CoverageReason::TimedOut.as_str());
    if changes
        .iter()
        .any(|c| c.refs_status == timed_out || c.callers_status == timed_out)
    {
        hints.push(
            LowerBound::AnalysisTimedOut {
                secs: app.config().lsp.timeout_secs,
                narrowing: Some(DIFF_IN_PARTS),
            }
            .hint(),
        );
    }

    // Coverage is measured only over rows that have reference counts
    // (Added/Modified). Deleted rows carry no refs — counting them as
    // "without tests" would pollute the ratio with symbols that have no live
    // references to test.
    let measurable = changes.iter().filter(|c| c.refs.is_some()).count();
    let total_refs = (measurable > 0).then(|| changes.iter().filter_map(|c| c.refs).sum());
    let refs_status_counts = status_counts(changes.iter().map(|c| c.refs_status));
    let callers_status_counts = status_counts(changes.iter().map(|c| c.callers_status));
    let with_tests = changes
        .iter()
        .filter(|c| c.test_refs.is_some_and(|t| t > 0))
        .count();
    let without_tests = measurable.saturating_sub(with_tests);

    let output = DiffImpactOutput {
        revision: args.revision,
        changed_files_count,
        excluded_files_count,
        total_references: total_refs,
        refs_status_counts,
        callers_status_counts,
        coverage: DiffCoverage {
            with_tests,
            without_tests,
            ratio: share(with_tests, measurable),
        },
        changes: Section::new(changes),
        unmeasured_files: unmeasured
            .iter()
            .map(|file| relative_display(file, root))
            .collect(),
        incomplete: capped,
        hints,
    };
    ctx.print_narrowable(output, "narrow the diff with --path");
    Ok(())
}

/// Whether a file holds documentation or configuration (Markdown, MDX,
/// YAML, TOML, JSON), which search leaves out unless named. Their servers
/// list keys and headings as symbols, a row each that few of them can count
/// references for, and a lockfile holds thousands of them.
fn documents_or_configures(file: &Path) -> bool {
    let language = Language::from_path(file);
    language != Language::Unknown && !language.is_code()
}

/// Mark each counted row whose declaration lies in a workspace member the
/// Python server resolves elsewhere, or is not known to resolve here, with
/// that member's reason: its `refs_status`, and its `callers_status` where a
/// caller list was read, since both are then lower bounds no retry raises
/// ([`ReferenceCount::status_within`]). A row whose count was not taken keeps
/// why. The hint names the members and the remedy, once for the diff.
fn qualify_member_rows(app: &App, changes: &mut [ChangedSymbolImpact]) -> Option<String> {
    let root = app.output.root();
    let members = PythonMembers::read(root, &crate::config::LspRuntimeConfig::from(app.config()));
    let mut qualified: Vec<&MemberGap> = Vec::new();
    for change in changes.iter_mut().filter(|change| change.refs.is_some()) {
        let Some(gap) = members.gap_for(&root.join(&change.location.file)) else {
            continue;
        };
        change.refs_status = Some(gap.reason.as_str());
        if change.callers.is_some() {
            change.callers_status = Some(gap.reason.as_str());
        }
        if !qualified.contains(&gap) {
            qualified.push(gap);
        }
    }
    members.summary_hint(&qualified)
}

/// Why the analysis did not measure a changed file's lines, each the cause
/// of a hint that names the file.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
enum Unmeasured {
    /// symora recognizes no language in the file's name, so nothing reads
    /// its lines as declarations.
    NoLanguage,
    /// No grammar compiled into symora reads the file's language, which is
    /// what removed lines are read by.
    NoGrammar,
    /// A version of the file the grammar reads is not text.
    NotText,
    /// No reader answered for the lines the diff wrote: no grammar reads
    /// the file and its language server failed, in these words.
    Failed(String),
    /// The file's changes gave no row: the lines its hunks wrote lie outside
    /// every declaration its symbols list, and the lines they replaced held
    /// none either (a comment, a blank line, a docstring, an import or a
    /// statement at module level). A row stands for a declaration and its
    /// references, so there is none to give them; lines only removed are a
    /// `no_symbol_in_range` deletion instead, read from the base.
    OutsideDeclarations,
}

impl Unmeasured {
    /// Why a version of `file` the grammar was to read could not be: its
    /// name, or else its content.
    fn of_version(file: &Path) -> Self {
        match Language::from_path(file) {
            Language::Unknown => Self::NoLanguage,
            language if !SymbolExtractor::is_supported(language) => Self::NoGrammar,
            _ => Self::NotText,
        }
    }

    /// Why nothing read the declarations of the lines the diff wrote in
    /// `file`, where its language server answered `error`.
    fn of_lines(file: &Path, error: &crate::error::LspError) -> Self {
        match Language::from_path(file) {
            Language::Unknown => Self::NoLanguage,
            _ => Self::Failed(error.to_string()),
        }
    }

    fn hint(&self, files: &str) -> String {
        match self {
            Self::NoLanguage => format!(
                "symora reads no language in {files}, so none of their lines was read as a \
                 declaration; read their diff for what changed."
            ),
            Self::NoGrammar => format!(
                "No grammar compiled into symora reads {files}, so which declarations the diff \
                 removed there is unknown; read their diff for what changed."
            ),
            Self::NotText => format!(
                "A version of {files} is not text, so the grammar read no declarations from it."
            ),
            Self::Failed(error) => format!("Nothing read the declarations of {files}: {error}"),
            Self::OutsideDeclarations => format!(
                "The changes to {files} lie outside every declaration listed there (a comment, \
                 a docstring, an import or a statement at module level, for instance), so no \
                 row stands for them; read their diff for what changed."
            ),
        }
    }
}

/// A hint per cause, naming the files it left unmeasured.
fn unmeasured_hints(unread: &[(PathBuf, Unmeasured)], root: &Path) -> Vec<String> {
    let mut by_cause: BTreeMap<&Unmeasured, Vec<String>> = BTreeMap::new();
    for (file, cause) in unread {
        by_cause
            .entry(cause)
            .or_default()
            .push(relative_display(file, root));
    }
    by_cause
        .into_iter()
        .map(|(cause, files)| cause.hint(&name_some(&files)))
        .collect()
}

/// How many rows carry each status, the rows without one left out.
fn status_counts<'a>(statuses: impl Iterator<Item = Option<&'a str>>) -> BTreeMap<&'a str, usize> {
    let mut counts = BTreeMap::new();
    for status in statuses.flatten() {
        *counts.entry(status).or_insert(0) += 1;
    }
    counts
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

/// What a file's two versions say about the declarations its diff wrote:
/// whether the base revision held each one's identity.
enum Sides<'a> {
    /// The diff removed none of the file's lines, so every declaration the
    /// base held still stands, and one whose name the diff wrote is new.
    NoneRemoved,
    /// The grammar read both versions.
    Compared {
        /// The current version's declarations.
        after: &'a [Symbol],
        /// Where those the diff added are named, `(line, column)`.
        added: HashSet<(u32, u32)>,
    },
    /// A version the diff removed lines from could not be read, so whether
    /// it held a declaration whose name the diff wrote is not known.
    Unread,
}

impl Sides<'_> {
    /// A changed declaration's change type, and whether the base was
    /// checked for its identity. One whose name stands on a line the diff
    /// did not write stood there in the base, so it was modified. One whose
    /// name it wrote was added where the base did not hold it, and modified
    /// where it did, moved or edited in place. Where the grammar does not
    /// read that declaration on both sides, only the lines that wrote it
    /// are known: lines that replaced others, which reads as modified, or
    /// lines added beside them, which reads as added.
    fn change_type(&self, symbol: &Symbol, writing: &[&DiffHunk]) -> (ChangeType, bool) {
        let line = symbol.location.line;
        let Some(wrote) = writing
            .iter()
            .find(|hunk| (hunk.start_line..hunk.start_line + hunk.line_count).contains(&line))
        else {
            return (ChangeType::Modified, true);
        };
        let counterpart = match self {
            Self::NoneRemoved => return (ChangeType::Added, true),
            Self::Compared { after, added } => counterpart(symbol, after).map(|declared| {
                added.contains(&(declared.location.line, declared.location.column))
            }),
            Self::Unread => None,
        };
        match counterpart {
            Some(true) => (ChangeType::Added, true),
            Some(false) => (ChangeType::Modified, true),
            None if wrote.old_count == 0 => (ChangeType::Added, false),
            None => (ChangeType::Modified, false),
        }
    }
}

/// The grammar's declaration a changed symbol is, among the current
/// version's: the one it is, where the grammar answered, or the one a
/// language server's symbol names ([`names_one`]).
fn counterpart<'a>(symbol: &Symbol, after: &'a [Symbol]) -> Option<&'a Symbol> {
    let at = (symbol.location.line, symbol.location.column);
    after
        .iter()
        .find(|declared| (declared.location.line, declared.location.column) == at)
        .or_else(|| {
            after
                .iter()
                .find(|declared| names_one(symbol, declared, after))
        })
}

/// The declarations a file's diff removed and added, by identity: a
/// declaration on the lines it removed was deleted where the current version
/// declares fewer of its identity than the base, and one on the lines it
/// wrote was added where it declares more. Where same-identity declarations
/// outnumber those added or removed (overloads; same-named Go methods, whose
/// receiver the grammar does not record), one edited in place is declared on
/// both sides of its hunk, so those that are not are the ones added or
/// removed; past them, the first in file order are.
struct Compared<'b, 'h> {
    /// The base's declarations removed, each with the hunk that removed it,
    /// in file order.
    deleted: Vec<(&'h DiffHunk, &'b Symbol)>,
    /// Where the current declarations added are named, `(line, column)`.
    added: HashSet<(u32, u32)>,
}

fn compare<'b, 'h>(
    before: &'b [Symbol],
    after: &[Symbol],
    removing: &[&'h DiffHunk],
    writing: &[&'h DiffHunk],
) -> Compared<'b, 'h> {
    let base = counts(before.iter().map(identity));
    let current = counts(after.iter().map(identity));
    let removed_lines = |hunk: &DiffHunk| (hunk.old_start, hunk.old_count);
    let written_lines = |hunk: &DiffHunk| (hunk.start_line, hunk.line_count);
    let removed = chosen(
        before,
        removing,
        removed_lines,
        surplus(&base, &current),
        |hunk, symbol| declares(after, written_lines(hunk), symbol),
    );
    let written = chosen(
        after,
        writing,
        written_lines,
        surplus(&current, &base),
        |hunk, symbol| declares(before, removed_lines(hunk), symbol),
    );
    Compared {
        deleted: removed,
        added: written
            .into_iter()
            .map(|(_, symbol)| (symbol.location.line, symbol.location.column))
            .collect(),
    }
}

/// Of the declarations `symbols` names on each hunk's `lines`, those an
/// identity's `surplus` counts: first those the other side of their hunk
/// does not declare again (`in_place`), then the rest, each in file order.
fn chosen<'s, 'h>(
    symbols: &'s [Symbol],
    hunks: &[&'h DiffHunk],
    lines: impl Fn(&DiffHunk) -> (u32, u32),
    mut surplus: HashMap<Identity, usize>,
    in_place: impl Fn(&DiffHunk, &Symbol) -> bool,
) -> Vec<(&'h DiffHunk, &'s Symbol)> {
    let candidates: Vec<(&DiffHunk, &Symbol)> = hunks
        .iter()
        .flat_map(|&hunk| declared_on(symbols, lines(hunk)).map(move |symbol| (hunk, symbol)))
        .collect();
    let mut picked = vec![false; candidates.len()];
    for again in [false, true] {
        for (i, (hunk, symbol)) in candidates.iter().enumerate() {
            if in_place(hunk, symbol) != again {
                continue;
            }
            if let Some(n) = surplus.get_mut(&identity(symbol))
                && *n > 0
            {
                *n -= 1;
                picked[i] = true;
            }
        }
    }
    candidates
        .into_iter()
        .zip(picked)
        .filter_map(|(candidate, picked)| picked.then_some(candidate))
        .collect()
}

/// Whether `symbols` declare `symbol`'s identity on `lines`.
fn declares(symbols: &[Symbol], lines: (u32, u32), symbol: &Symbol) -> bool {
    declared_on(symbols, lines).any(|declared| identity(declared) == identity(symbol))
}

/// How many more of each identity `of` holds than `over`.
fn surplus(
    of: &HashMap<Identity, usize>,
    over: &HashMap<Identity, usize>,
) -> HashMap<Identity, usize> {
    of.iter()
        .filter_map(|(id, &n)| {
            let held = over.get(id).copied().unwrap_or(0);
            (n > held).then(|| (id.clone(), n - held))
        })
        .collect()
}

/// The changed symbols a diff's hunks were assigned to, those whose
/// references are counted bounded by `--max-symbols`, and the files whose
/// lines could not be assigned.
struct HunkAnalysis {
    changes: Vec<ChangedSymbolImpact>,
    /// The files whose lines' declarations could not be read, each once,
    /// with why.
    unmeasured: Vec<(PathBuf, Unmeasured)>,
    /// The files the cap left work undone in, in path order: those it
    /// refused a row of and those it stopped before reading.
    unfinished: Vec<PathBuf>,
    /// What the rows' counts and callers leave unsaid: where a constructor
    /// runs that no reference names.
    hints: Vec<String>,
}

/// A changed symbol's row, in the order it is reported: settled where the
/// diff alone answers it (a deletion), or a symbol of the current tree whose
/// references are still to be asked of the language server.
enum Row {
    Settled(ChangedSymbolImpact),
    Live {
        /// The file's declarations it was read from, in [`Plan::answers`].
        answer: usize,
        file: PathBuf,
        symbol: Symbol,
        change_type: ChangeType,
    },
}

/// The rows a diff's hunks were assigned to, before any reference is asked.
struct Plan {
    rows: Vec<Row>,
    answers: Vec<FileSymbols>,
    unmeasured: Vec<(PathBuf, Unmeasured)>,
    max_symbols: usize,
    /// The rows whose references are to be asked.
    live: usize,
    /// The rows the diff alone answers: deletions.
    settled: usize,
    /// The file being planned, until the cap refuses one of its rows.
    planning: Option<PathBuf>,
    /// The files the cap left work undone in.
    unfinished: Vec<PathBuf>,
    /// The rows whose change type the base could not be checked for
    /// ([`Sides::change_type`]), each `name (file:line)`.
    unchecked: Vec<String>,
}

impl Plan {
    /// Records a file whose lines' declarations could not be read, under
    /// the first cause found for it.
    fn unmeasure(&mut self, file: &Path, cause: Unmeasured) {
        if !self.unmeasured.iter().any(|(listed, _)| listed == file) {
            self.unmeasured.push((file.to_path_buf(), cause));
        }
    }

    /// Whether the cap leaves no room for another symbol whose references
    /// are counted.
    fn full(&self) -> bool {
        self.max_symbols > 0 && self.live >= self.max_symbols
    }

    /// Records the file being planned as one the cap left unfinished.
    fn refuse(&mut self) {
        if let Some(file) = self.planning.take() {
            self.unfinished.push(file);
        }
    }

    /// Plans a symbol whose references are to be counted, unless the cap
    /// leaves no room for it, when its file is recorded as unfinished and
    /// false is returned.
    fn measure(&mut self, row: Row) -> bool {
        if self.full() {
            self.refuse();
            return false;
        }
        self.live += 1;
        self.rows.push(row);
        true
    }

    /// Plans a row the diff alone answers, a deletion, which has no
    /// references to ask and so takes none of the room `measure` counts.
    /// The cap bounds these apart, so a diff that removes a whole module
    /// lists its first deletions rather than every one of them.
    fn settle(&mut self, impact: ChangedSymbolImpact) {
        if self.max_symbols > 0 && self.settled >= self.max_symbols {
            self.refuse();
            return;
        }
        self.settled += 1;
        self.rows.push(Row::Settled(impact));
    }
}

/// Where `--callers` reads each row's callers from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum CallerSource {
    /// The references the row's count was taken from: the callables whose
    /// code holds one, for a row that is itself a function, method or
    /// constructor. No query beyond the reference count is made.
    References,
    /// The language server's incoming calls, asked at the row's position:
    /// a second workspace search per row, beside the reference count's.
    CallHierarchy,
}

impl CallerSource {
    fn asked(args: &DiffImpactArgs) -> Option<Self> {
        if args.call_hierarchy {
            Some(Self::CallHierarchy)
        } else {
            args.callers.then_some(Self::References)
        }
    }
}

/// A row once its references were asked: what is still to be read off them.
struct Measured {
    impact: ChangedSymbolImpact,
    /// The callers the server named, where derived ones join them.
    named: Vec<CallHierarchyItem>,
    /// The references its callers are read off, where any are.
    derive_from: Option<Vec<Location>>,
    /// Where it runs, for a constructor run by calling its type.
    constructions: Option<Constructions>,
}

/// The diff's changed symbols, their live rows measured with a bounded
/// number of reference queries in flight, as `symbols --refs` measures a
/// file's: asked one after another, each row waits on every query before
/// it, and a server that answers in parallel spends the shared
/// `lsp.timeout_secs` budget on one query at a time. Rows keep the order
/// they were planned in. Callers derived from references are read once
/// every count is taken, each referencing file's declarations read once
/// for all of the rows its references are to.
async fn analyze_hunks(
    app: &App,
    hunks: &[DiffHunk],
    preimage_ref: &str,
    callers: Option<CallerSource>,
    max_symbols: usize,
) -> HunkAnalysis {
    let Plan {
        rows,
        answers,
        unmeasured,
        unfinished,
        unchecked,
        ..
    } = plan_hunks(app, hunks, preimage_ref, max_symbols).await;
    let mut hints = Vec::new();
    if !unchecked.is_empty() {
        hints.push(format!(
            "Whether the base revision declared {} is unknown: the grammar compiled into symora \
             did not read them in both versions of their file, so each change_type says only \
             whether the lines that wrote the declaration replaced removed lines (modified) or \
             not (added).",
            name_some(&unchecked)
        ));
    }
    let answers = &answers;
    let mut measured: Vec<Measured> = futures::stream::iter(rows)
        .map(|row| async move {
            match row {
                Row::Settled(impact) => Measured {
                    impact,
                    named: Vec::new(),
                    derive_from: None,
                    constructions: None,
                },
                Row::Live {
                    answer,
                    file,
                    symbol,
                    change_type,
                } => {
                    analyze_symbol_impact(
                        app,
                        &answers[answer],
                        &file,
                        &symbol,
                        change_type,
                        callers,
                    )
                    .await
                }
            }
        })
        .buffered(REFERENCE_COUNT_CONCURRENCY)
        .collect()
        .await;

    let referencing: Vec<PathBuf> = measured
        .iter()
        .filter_map(|row| row.derive_from.as_ref())
        .flatten()
        .map(|reference| reference.file.clone())
        .collect();
    let declarations = declarations_of(app, referencing).await;
    let root = app.output.root();
    let limit = app.config().lsp.calls_limit;
    for row in &mut measured {
        if let (Some(constructions), Some(name)) = (&row.constructions, &row.impact.name) {
            hints.push(format!(
                "{}:{}: {}.",
                row.impact.location.file,
                row.impact.location.line,
                constructions.hint(name)
            ));
        }
        let Some(references) = row.derive_from.take() else {
            continue;
        };
        match callers_from_references(&references, |file| {
            declarations.get(file).and_then(|read| read.as_deref())
        }) {
            Some(DerivedCallers {
                callers: derived,
                outside,
            }) => {
                let mut callers = std::mem::take(&mut row.named);
                for caller in derived {
                    if !callers
                        .iter()
                        .any(|named| named.location == caller.location)
                    {
                        callers.push(caller);
                    }
                }
                row.impact.callers = Some(with_cap(
                    Section::with_total(
                        callers
                            .iter()
                            .take(limit)
                            .map(|caller| CallHierarchyOutput::caller(caller, root))
                            .collect(),
                        callers.len(),
                    )
                    .with_hints(outside.hint().into_iter().collect()),
                    Cap::Config("lsp.calls_limit"),
                ));
            }
            // A referencing file no reader could read leaves its
            // references' callers unknown.
            None => row.impact.callers_status = Some(CoverageReason::Unavailable.as_str()),
        }
    }
    HunkAnalysis {
        changes: measured.into_iter().map(|row| row.impact).collect(),
        unmeasured,
        unfinished,
        hints,
    }
}

/// Each changed symbol's row, in path order. `--max-symbols` bounds the
/// rows whose references are counted and, apart from them, the deletions
/// listed, so a deletion never takes the place of a symbol whose references
/// would be counted. The analysis stops at the first file the cap leaves no
/// room for a counted symbol in, which with every file after it is not read
/// and is recorded as unfinished, as a file the cap refuses a row of is.
async fn plan_hunks(app: &App, hunks: &[DiffHunk], preimage_ref: &str, max_symbols: usize) -> Plan {
    let root = app.output.root();
    let mut by_file: BTreeMap<&PathBuf, Vec<&DiffHunk>> = BTreeMap::new();
    for hunk in hunks {
        by_file.entry(&hunk.file).or_default().push(hunk);
    }

    let mut analysis = Plan {
        rows: Vec::new(),
        answers: Vec::new(),
        unmeasured: Vec::new(),
        max_symbols,
        live: 0,
        settled: 0,
        planning: None,
        unfinished: Vec::new(),
        unchecked: Vec::new(),
    };
    // Kept apart from `analysis` until every file is read: a file's
    // declarations are borrowed from here while its rows join the plan.
    let mut answers: Vec<FileSymbols> = Vec::new();
    let mut files = by_file.into_iter();
    while let Some((file, hunks)) = files.next() {
        if analysis.full() {
            analysis.unfinished.push(file.clone());
            analysis
                .unfinished
                .extend(files.map(|(file, _)| file.clone()));
            break;
        }
        analysis.planning = Some(file.clone());
        let rows_before = analysis.rows.len();

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
        let served = match file_exists {
            true => Some(
                declared_in(app, file, FindSymbolsOptions::default())
                    .await
                    .map(|read| {
                        answers.push(read);
                        answers.len() - 1
                    }),
            ),
            false => None,
        };
        let declarations: Option<(usize, &[Symbol])> = served.as_ref().and_then(|served| {
            let answer = *served.as_ref().ok()?;
            let read = &answers[answer];
            if read.backend == SymbolBackend::Ast {
                on_disk.as_ref().map(|now| (answer, now.members.as_slice()))
            } else {
                Some((answer, read.symbols.as_slice()))
            }
        });
        let current_symbols = declarations.map(|(_, symbols)| symbols);
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
        let writing: Vec<&DiffHunk> = hunks.iter().filter(|h| h.line_count > 0).copied().collect();
        let preimage = removing
            .first()
            .map(|hunk| read_preimage(root, preimage_ref, &hunk.old_file));
        // The current version as the same grammar reads it: nothing where the
        // diff deleted the file.
        let after: Option<&[Symbol]> = match &on_disk {
            _ if !file_exists => Some(&[]),
            Some(now) => Some(now.members.as_slice()),
            None => None,
        };
        let mut modified_by_removal: Vec<&Symbol> = Vec::new();
        // The hunks whose removed lines a row speaks for: a declaration they
        // held, one holding them, or a deletion no declaration was read in.
        let mut answered: Vec<&DiffHunk> = Vec::new();
        let sides = match &preimage {
            None => Sides::NoneRemoved,
            Some(Preimage::Unparsed) => {
                analysis.unmeasure(file, Unmeasured::of_version(&removing[0].old_file));
                Sides::Unread
            }
            Some(Preimage::Unavailable) => {
                for hunk in &removing {
                    analysis.settle(unresolved_deletion(
                        DeletionResolution::PreimageUnavailable,
                        hunk,
                        root,
                    ));
                }
                answered.extend(&removing);
                Sides::Unread
            }
            Some(Preimage::Parsed(before)) => match after {
                // Neither a removed declaration nor a written one can be
                // checked against a version nothing read.
                None => {
                    analysis.unmeasure(file, Unmeasured::of_version(file));
                    Sides::Unread
                }
                Some(after) => {
                    let before = before.as_slice();
                    let Compared { deleted, added } = compare(before, after, &removing, &writing);
                    for hunk in &removing {
                        for (_, symbol) in deleted.iter().filter(|(of, _)| std::ptr::eq(*of, *hunk))
                        {
                            analysis.settle(deleted_row(symbol, &hunk.old_file, root));
                        }
                        // The lines of a declaration the hunk removed are that
                        // declaration's, wherever it now stands; the others
                        // belong to what held them.
                        let declared: Vec<&Symbol> = declared_in_removed(before, hunk).collect();
                        let enclosing: Vec<&Symbol> = current_symbols
                            .map(|current| {
                                removed_lines_owners(before, current, headers_now, hunk, &declared)
                            })
                            .unwrap_or_default();
                        if hunk.line_count == 0 && enclosing.is_empty() && declared.is_empty() {
                            analysis.settle(unresolved_deletion(
                                DeletionResolution::NoSymbolInRange,
                                hunk,
                                root,
                            ));
                        }
                        if !declared.is_empty() || !enclosing.is_empty() {
                            answered.push(hunk);
                        }
                        modified_by_removal.extend(enclosing);
                    }
                    Sides::Compared { after, added }
                }
            },
        };

        let Some((answer, symbols)) = declarations else {
            // Lines were written but no current symbols were read. If the
            // file is present, find_symbols errored — disclose it as an
            // unmeasured file (a lower bound) instead of silently dropping
            // its changes.
            if file_exists && !writing.is_empty() {
                let cause = match &served {
                    Some(Err(error)) => Unmeasured::of_lines(file, error),
                    _ => Unmeasured::of_version(file),
                };
                analysis.unmeasure(file, cause);
            }
            continue;
        };
        let written: Vec<(&DiffHunk, Vec<&Symbol>)> = writing
            .iter()
            .map(|&hunk| {
                let removed_a_declaration = match &preimage {
                    Some(Preimage::Parsed(before)) => {
                        declared_in_removed(before, hunk).next().is_some()
                    }
                    _ => false,
                };
                let owners = added_lines_owners(symbols, headers_now, hunk, removed_a_declaration);
                (hunk, owners)
            })
            .collect();
        // A hunk that wrote lines no declaration holds, and removed none a
        // row speaks for, changed nothing a row can stand for: no module
        // symbol owns a module's own lines.
        let outside = written.iter().any(|(hunk, owners)| {
            owners.is_empty() && !answered.iter().any(|row| std::ptr::eq(*row, *hunk))
        });
        let owners = modified_by_removal.into_iter().chain(
            written
                .iter()
                .flat_map(|(_, owners)| owners.iter().copied()),
        );
        for sym in owners {
            if !seen.insert((sym.location.line, sym.location.column)) {
                continue;
            }
            let (change_type, checked) = sides.change_type(sym, &writing);
            if !analysis.measure(Row::Live {
                answer,
                file: file.clone(),
                symbol: sym.clone(),
                change_type,
            }) {
                break;
            }
            if !checked {
                analysis.unchecked.push(format!(
                    "{} ({}:{})",
                    sym.name,
                    relative_display(file, root),
                    sym.location.line
                ));
            }
        }
        // A file is a row or an unmeasured file: one whose changes all lie
        // there gave no row, so it is listed with why. Beside a row, such a
        // hunk (an import, blank lines) goes unlisted, as most diffs hold one.
        if outside && analysis.rows.len() == rows_before && !analysis.unfinished.contains(file) {
            analysis.unmeasure(file, Unmeasured::OutsideDeclarations);
        }
    }

    analysis.answers = answers;
    analysis
}

/// A live changed symbol's row: its references asked where the language
/// server reads it, as `symbols --refs` asks them, and with `--callers` the
/// references to read its callers off, or with `--call-hierarchy` its
/// callers asked at that same position.
async fn analyze_symbol_impact(
    app: &App,
    answer: &FileSymbols,
    file: &Path,
    sym: &Symbol,
    change_type: ChangeType,
    callers: Option<CallerSource>,
) -> Measured {
    let lsp = app.lsp.as_ref();
    let root = app.output.root();
    let outcome = answer.references_of(lsp, file, sym, root).await;
    let count = ReferenceCount::of(&outcome, app.test_scope());

    let constructions = match &outcome {
        Ok(Some(analysis)) => analysis.constructions().cloned(),
        _ => None,
    };
    let sites = constructions
        .as_ref()
        .and_then(|constructions| constructions.sites.clone());
    let mut named = Vec::new();
    let mut derive_from = None;
    let (callers, callers_status) = match (callers, &outcome) {
        (None, _) => (None, None),
        (Some(CallerSource::References), _) if !sym.kind.is_callable() => {
            (None, Some(NOT_CALLABLE))
        }
        (Some(CallerSource::References), Ok(Some(analysis))) => {
            let mut references = analysis.references().to_vec();
            references.extend(sites.unwrap_or_default());
            derive_from = Some(references);
            (None, Some(REFERENCES_DERIVED))
        }
        (Some(CallerSource::CallHierarchy), Ok(Some(analysis))) => {
            let anchor = analysis.anchor();
            match lsp
                .incoming_calls(&anchor.file, anchor.line, anchor.column)
                .await
            {
                // Callers the constructions name join the server's, which
                // makes the list no longer the server's answer alone.
                Ok(calls) if sites.is_some() => {
                    named = calls.data;
                    derive_from = sites;
                    (None, Some(REFERENCES_DERIVED))
                }
                Ok(calls) => (
                    Some(with_cap(
                        Section::with_total(
                            calls
                                .data
                                .iter()
                                .take(app.config().lsp.calls_limit)
                                .map(|c| CallHierarchyOutput::caller(c, root))
                                .collect(),
                            calls.data.len(),
                        ),
                        Cap::Config("lsp.calls_limit"),
                    )),
                    calls.indexing.map(|_| INDEXING_DEGRADED),
                ),
                Err(error) => (None, Some(CoverageReason::of(&error).as_str())),
            }
        }
        (Some(_), _) => (None, Some(CoverageReason::Unavailable.as_str())),
    };

    let (refs, test_refs, prod_refs) = match count {
        ReferenceCount::Measured {
            total, test, prod, ..
        } => (Some(total), Some(test), Some(prod)),
        ReferenceCount::Missing(_) => (None, None, None),
    };
    Measured {
        impact: ChangedSymbolImpact {
            name: Some(sym.name.clone()),
            kind: Some(sym.kind.to_string()),
            location: LocationOutput::from_path(file, sym.location.line, sym.location.column, root),
            change_type,
            refs,
            test_refs,
            prod_refs,
            refs_status: count.status(),
            callers,
            callers_status,
            deletion: None,
        },
        named,
        derive_from,
        constructions,
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
    declared_on(before, (hunk.old_start, hunk.old_count))
}

/// The declarations named on `count` lines from `start`.
fn declared_on(symbols: &[Symbol], (start, count): (u32, u32)) -> impl Iterator<Item = &Symbol> {
    let lines = start..start.saturating_add(count);
    symbols
        .iter()
        .filter(move |symbol| lines.contains(&symbol.location.line))
}

/// What tells a declaration apart from the others in its file, as the
/// grammar reads both versions of it: its name path and its kind. Overloads
/// share one.
type Identity = (String, SymbolKind);

fn identity(symbol: &Symbol) -> Identity {
    (
        symbol
            .name_path
            .clone()
            .unwrap_or_else(|| symbol.name.clone()),
        symbol.kind,
    )
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
        location: LocationOutput::from_path(
            old_file,
            symbol.location.line,
            symbol.location.column,
            root,
        ),
        change_type: ChangeType::Deleted,
        refs: None,
        test_refs: None,
        prod_refs: None,
        refs_status: None,
        callers: None,
        callers_status: None,
        deletion: Some(DeletionResolution::Resolved),
    }
}

/// A removal whose declarations were not identified, disclosed rather than
/// dropped, at the first line it removed from the base's version of the file.
fn unresolved_deletion(
    resolution: DeletionResolution,
    hunk: &DiffHunk,
    root: &Path,
) -> ChangedSymbolImpact {
    ChangedSymbolImpact {
        name: None,
        kind: None,
        location: LocationOutput::from_path(&hunk.old_file, hunk.old_start, 1, root),
        change_type: ChangeType::Deleted,
        refs: None,
        test_refs: None,
        prod_refs: None,
        refs_status: None,
        callers: None,
        callers_status: None,
        deletion: Some(resolution),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cli::response::{CallSite, PositionOutput};
    use std::path::PathBuf;

    /// A line a hunk adds between a type's members goes with the member the
    /// hunk adds there, whichever members the answer lists: these are a
    /// server's, whatever the built-in grammar reads.
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
        };
        let owners: Vec<&str> = added_lines_owners(&symbols, &headers, &hunk, false)
            .iter()
            .map(|symbol| symbol.name.as_str())
            .collect();
        assert_eq!(owners, ["Idle"]);
    }

    /// A declaration's change type is whether its identity is declared on
    /// each side of the diff, whatever shape of hunk wrote or removed it: a
    /// function moved by an addition and a deletion is one declaration,
    /// modified; one renamed in place is the old name deleted and the new
    /// one added; one whose name line no hunk wrote was modified. A symbol
    /// the grammar has no counterpart for is read from the lines that wrote
    /// it, and says so.
    #[test]
    fn a_change_type_is_read_from_the_identities_each_side_declares() {
        let path = PathBuf::from("m.py");
        let read = |content: &str| {
            SymbolExtractor::new().extract_members(&path, content, Language::Python)
        };
        let before = read(
            "def keep():\n    return 1\n\n\ndef moved():\n    return 2\n\n\n\
             def old_name():\n    return 3\n",
        );
        let after = read(
            "def moved():\n    return 2\n\n\ndef keep():\n    return 10\n\n\n\
             def new_name():\n    return 3\n",
        );
        let hunk = |old_start, old_count, start_line, line_count| DiffHunk {
            file: path.clone(),
            old_file: path.clone(),
            start_line,
            line_count,
            old_start,
            old_count,
        };
        let hunks = [
            hunk(0, 0, 1, 4),
            hunk(2, 1, 6, 1),
            hunk(5, 4, 8, 0),
            hunk(9, 1, 9, 1),
        ];
        let removing: Vec<&DiffHunk> = hunks.iter().filter(|h| h.old_count > 0).collect();
        let writing: Vec<&DiffHunk> = hunks.iter().filter(|h| h.line_count > 0).collect();

        let Compared { deleted, added } = compare(&before, &after, &removing, &writing);
        let deleted: Vec<&str> = deleted.iter().map(|(_, s)| s.name.as_str()).collect();
        assert_eq!(deleted, ["old_name"]);
        let sides = Sides::Compared {
            after: &after,
            added,
        };
        let typed: Vec<(&str, ChangeType, bool)> = after
            .iter()
            .map(|symbol| {
                let (change_type, checked) = sides.change_type(symbol, &writing);
                (symbol.name.as_str(), change_type, checked)
            })
            .collect();
        assert_eq!(
            typed,
            [
                ("moved", ChangeType::Modified, true),
                ("keep", ChangeType::Modified, true),
                ("new_name", ChangeType::Added, true),
            ]
        );

        let unread = Symbol::new(
            "LIMIT".to_string(),
            SymbolKind::Constant,
            Location::point(path.clone(), 3, 1),
        );
        assert_eq!(
            sides.change_type(&unread, &writing),
            (ChangeType::Added, false)
        );
        assert_eq!(
            Sides::Unread.change_type(&after[2], &writing),
            (ChangeType::Modified, false)
        );
    }

    // The Deleted-row JSON contract: never a live neighbour's identity, never
    // a synthesized zero reference count.

    fn loc() -> LocationOutput {
        LocationOutput {
            file: "src/lib.rs".to_string(),
            line: 10,
            column: 1,
            end_line: None,
            end_column: None,
            snippet: None,
            degraded_column: None,
        }
    }

    #[test]
    fn deleted_resolved_row_omits_refs_and_discloses() {
        let row = ChangedSymbolImpact {
            name: Some("gone".to_string()),
            kind: Some("function".to_string()),
            location: loc(),
            change_type: ChangeType::Deleted,
            refs: None,
            test_refs: None,
            prod_refs: None,
            refs_status: None,
            callers: None,
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

    /// An unidentified removal names no declaration, and is placed where its
    /// lines were removed from the base's version of the file, never at a
    /// live neighbour.
    #[test]
    fn an_unidentified_removal_is_placed_at_its_first_removed_line() {
        let root = PathBuf::from("/repo");
        let hunk = DiffHunk {
            file: root.join("new.py"),
            old_file: root.join("old.py"),
            start_line: 3,
            line_count: 0,
            old_start: 7,
            old_count: 2,
        };
        for resolution in [
            DeletionResolution::NoSymbolInRange,
            DeletionResolution::PreimageUnavailable,
        ] {
            let v = serde_json::to_value(unresolved_deletion(resolution, &hunk, &root)).unwrap();
            assert_eq!(v["deletion"], serde_json::to_value(resolution).unwrap());
            assert!(v.get("name").is_none(), "{v}");
            assert_eq!(v["file"], "old.py", "{v}");
            assert_eq!(v["line"], 7, "{v}");
        }
    }

    #[test]
    fn modified_row_shape_is_unchanged() {
        // Added/Modified rows serialize refs as bare numbers and carry no
        // deletion key — byte-identical to the pre-change shape.
        let row = ChangedSymbolImpact {
            name: Some("touched".to_string()),
            kind: Some("function".to_string()),
            location: loc(),
            change_type: ChangeType::Modified,
            refs: Some(3),
            test_refs: Some(1),
            prod_refs: Some(2),
            refs_status: None,
            callers: None,
            callers_status: None,
            deletion: None,
        };
        let v = serde_json::to_value(row).unwrap();
        assert_eq!(v["refs"], 3);
        assert_eq!(v["test_refs"], 1);
        assert!(v.get("deletion").is_none());
    }

    /// A row's callers are the shared list: the count the server named, the
    /// items the calls limit kept, and `truncated` between them.
    #[test]
    fn a_rows_callers_are_a_section_cut_at_the_calls_limit() {
        let caller = CallHierarchyOutput {
            name: "use".to_string(),
            kind: "function".to_string(),
            location: loc(),
            call_site: Some(CallSite::Within(PositionOutput {
                line: 10,
                column: 1,
                degraded_column: None,
            })),
            body: None,
            body_omitted: None,
        };
        let row = ChangedSymbolImpact {
            name: Some("compute".to_string()),
            kind: Some("function".to_string()),
            location: loc(),
            change_type: ChangeType::Modified,
            refs: Some(3),
            test_refs: Some(0),
            prod_refs: Some(3),
            refs_status: None,
            callers: Some(Section::with_total(vec![caller], 3)),
            callers_status: None,
            deletion: None,
        };
        let v = serde_json::to_value(row).unwrap();
        assert_eq!(v["callers"]["count"], 3, "{v}");
        assert_eq!(v["callers"]["showing"], 1, "{v}");
        assert_eq!(v["callers"]["truncated"], true, "{v}");
        assert_eq!(v["callers"]["items"][0]["name"], "use", "{v}");
        assert_eq!(v["callers"]["items"][0]["call_site"]["line"], 10, "{v}");
    }
}
