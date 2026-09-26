use std::collections::{BTreeMap, BTreeSet, HashSet};
use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{Context, Result};
use clap::Args;
use serde::Serialize;

use crate::app::App;
use crate::cli::analysis::LocationAnalysis;
use crate::cli::declared_in;
use crate::cli::errors::{ErrorCode, OutputError};
use crate::cli::response::disclosure::LowerBound;
use crate::cli::response::{CallHierarchyOutput, LocationOutput};
use crate::cli::utils::find_symbol_at_position;
use crate::models::lsp::FindSymbolsOptions;
use crate::services::TestScope;
use crate::services::lsp::LspService;
use crate::services::store::SymbolExtractor;

#[derive(Args, Debug)]
pub struct DiffImpactArgs {
    /// The commit the working tree is compared with — one revision, not a
    /// range (default: HEAD)
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
    pub changes: Vec<ChangedSymbolImpact>,
    /// Files whose changes could not be measured: nothing could read their
    /// symbols, git reports them as binary and names no lines, or — with
    /// `--staged` — unstaged edits sit over the staged ones, so the lines the
    /// diff names are not the lines on disk. Their changes are absent from
    /// `changes`, so the result is a lower bound for these files. `hints`
    /// names the binary and staged causes; a file listed without one is one
    /// whose symbols could not be read. Omitted when empty.
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
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub callers: Vec<CallHierarchyOutput>,
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
/// the pre-image (only body lines were removed), so the enclosing current symbol
/// can be reclassified to `Modified`; `PreimageUnavailable` could not be checked
/// at all, so what was deleted is unknown and the row stays a disclosed deletion.
#[derive(Debug, Clone, Copy, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum DeletionResolution {
    /// The deleted symbol was identified from the pre-image (old git tree); its
    /// references are not recomputed (it no longer exists).
    Resolved,
    /// The pre-image was read but declared no symbol in the deleted range — only
    /// body lines were removed, not a declaration.
    NoSymbolInRange,
    /// The pre-image itself could not be read (the `git show` of the old tree
    /// failed), so what was deleted is unknown — never guessed from diff text or
    /// a live neighbour, and never reclassified to a live `Modified`.
    PreimageUnavailable,
}

struct DiffHunk {
    file: PathBuf,
    /// The path the pre-image names the file by: `file`, unless the diff
    /// renamed it.
    old_file: PathBuf,
    /// New-file coordinates — used to locate Added/Modified symbols in the
    /// current tree.
    start_line: u32,
    line_count: u32,
    /// Old-file coordinates — used to locate Deleted symbols in the pre-image
    /// (the deleted symbol no longer exists in the current tree).
    old_start: u32,
    old_count: u32,
    change_type: ChangeType,
}

pub async fn execute(args: DiffImpactArgs, app: &App) -> Result<()> {
    let ctx = &app.output;
    let root = ctx.root();
    let test_scope = app.test_scope();

    let base = resolve_base(root, &args.revision)?;
    let ParsedDiff { mut hunks, binary } = parse_git_diff(root, base.tree_ish(), args.staged)?;
    let changed_files = changed_files(root, base.tree_ish(), args.staged)?;
    let mut hints = Vec::new();
    let mut unmeasured_files = Vec::new();

    if let Base::Commit(commit) = &base
        && !is_ancestor_of_head(root, commit)
    {
        hints.push(format!(
            "`{rev}` is not an ancestor of HEAD, so this diff also takes back what `{rev}` \
             gained after this branch left it; `symora diff-impact $(git merge-base {rev} HEAD)` \
             measures this branch alone.",
            rev = args.revision
        ));
    }

    let binary: BTreeSet<String> = binary.iter().map(|f| relative_display(f, root)).collect();
    if !binary.is_empty() {
        hints.push(format!(
            "git reports {} as binary, so which of their lines changed is not known; a `-diff` \
             or `binary` attribute does this to a text file.",
            binary.iter().cloned().collect::<Vec<_>>().join(", ")
        ));
        unmeasured_files.extend(binary);
    }

    if args.staged {
        let unstaged = unstaged_files(root)?;
        let (overlaid, staged): (Vec<_>, Vec<_>) =
            hunks.into_iter().partition(|h| unstaged.contains(&h.file));
        hunks = staged;
        let overlaid: BTreeSet<String> = overlaid
            .iter()
            .map(|h| relative_display(&h.file, root))
            .collect();
        if !overlaid.is_empty() {
            hints.push(format!(
                "Unstaged edits sit over the staged ones in {}, so the staged lines are not the \
                 lines on disk; stage or stash those edits to measure them.",
                overlaid.iter().cloned().collect::<Vec<_>>().join(", ")
            ));
            unmeasured_files.extend(overlaid);
        }
    }

    let (changes, unreadable, stopped_at_cap) = if hunks.is_empty() {
        (Vec::new(), Vec::new(), false)
    } else {
        analyze_hunks(
            app,
            &hunks,
            root,
            base.tree_ish(),
            test_scope,
            args.callers,
            args.max_symbols,
            app.config().lsp.calls_limit,
        )
        .await
    };
    unmeasured_files.extend(unreadable);
    if stopped_at_cap {
        hints.push(LowerBound::AnalysisCapped(args.max_symbols).hint());
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
        unmeasured_files,
        incomplete: stopped_at_cap,
        hints,
    });
    Ok(())
}

/// What the working tree is measured against.
enum Base {
    Commit(String),
    /// `HEAD` on a branch with no commit yet. Git diffs such a repository
    /// against the empty tree, so everything tracked reads as added.
    Unborn(String),
}

impl Base {
    fn tree_ish(&self) -> &str {
        match self {
            Self::Commit(id) | Self::Unborn(id) => id,
        }
    }
}

/// The base the diff is measured from, resolved once so the diff and every
/// pre-image read name the same tree. Only a single commit is accepted: the
/// other side is always read from the working tree (or the index), so a
/// range's second revision would name a tree nothing reads.
fn resolve_base(root: &Path, revision: &str) -> Result<Base> {
    let output = Command::new("git")
        .current_dir(root)
        .args([
            "rev-parse",
            "--verify",
            "--quiet",
            "--end-of-options",
            &format!("{revision}^{{commit}}"),
        ])
        .output()
        .context("Failed to run git rev-parse")?;
    if output.status.success() {
        return Ok(Base::Commit(
            String::from_utf8_lossy(&output.stdout).trim().to_string(),
        ));
    }
    // `--verify --quiet` exits 1 for an argument that is not one commit; any
    // other failure is git's own (no repository, a broken one).
    if output.status.code() != Some(1) {
        anyhow::bail!(
            "git rev-parse failed: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    if revision == "HEAD" && head_names_a_branch(root) {
        return Ok(Base::Unborn(git_stdout(
            root,
            &["hash-object", "-t", "tree", "--stdin"],
        )?));
    }
    Err(unresolvable_revision(revision).into())
}

/// Whether `HEAD` is a symbolic ref to a branch — which, for a `HEAD` that
/// names no commit, is a branch with no commit yet.
fn head_names_a_branch(root: &Path) -> bool {
    Command::new("git")
        .current_dir(root)
        .args(["symbolic-ref", "--quiet", "HEAD"])
        .stdout(std::process::Stdio::null())
        .status()
        .is_ok_and(|status| status.success())
}

/// A git command's trimmed stdout, with empty stdin.
fn git_stdout(root: &Path, args: &[&str]) -> Result<String> {
    let output = Command::new("git")
        .current_dir(root)
        .args(args)
        .stdin(std::process::Stdio::null())
        .output()
        .with_context(|| format!("Failed to run git {}", args[0]))?;
    if !output.status.success() {
        anyhow::bail!(
            "git {} failed: {}",
            args[0],
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    Ok(String::from_utf8_lossy(&output.stdout).trim().to_string())
}

/// A ref name cannot contain `..`, so a revision that does is range syntax.
fn unresolvable_revision(revision: &str) -> OutputError {
    match revision.split_once("..") {
        Some((left, _)) => {
            let from = if left.is_empty() { "HEAD" } else { left };
            OutputError::new(
                ErrorCode::InvalidArgument,
                format!(
                    "`{revision}` is a range; diff-impact compares the working tree with one revision"
                ),
            )
            .with_hint(format!(
                "`symora diff-impact {from}` measures everything since {from}; \
                 `symora diff-impact $(git merge-base {from} HEAD)` measures what this branch \
                 changed after it left {from}."
            ))
        }
        None => OutputError::new(
            ErrorCode::InvalidArgument,
            format!("`{revision}` does not name a commit in this repository"),
        )
        .with_hint("Pass a branch, tag, or commit id; `git log --oneline` lists recent commits."),
    }
}

fn is_ancestor_of_head(root: &Path, commit: &str) -> bool {
    Command::new("git")
        .current_dir(root)
        .args(["merge-base", "--is-ancestor", commit, "HEAD"])
        .status()
        .is_ok_and(|status| status.code() != Some(1))
}

/// Files whose working-tree content differs from the index.
fn unstaged_files(root: &Path) -> Result<HashSet<PathBuf>> {
    let output = Command::new("git")
        .current_dir(root)
        .args(["diff", "--relative", "--name-only", "-z"])
        .output()
        .context("Failed to run git diff")?;
    if !output.status.success() {
        anyhow::bail!(
            "git diff failed: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    Ok(output
        .stdout
        .split(|&b| b == 0)
        .filter(|name| !name.is_empty())
        .map(|name| root.join(String::from_utf8_lossy(name).as_ref()))
        .collect())
}

fn relative_display(file: &Path, root: &Path) -> String {
    file.strip_prefix(root)
        .unwrap_or(file)
        .display()
        .to_string()
}

/// Paths are relative to `root` and limited to it (`--relative`), which need
/// not be the repository's top level.
fn parse_git_diff(root: &Path, base: &str, staged: bool) -> Result<ParsedDiff> {
    let mut cmd = Command::new("git");
    cmd.current_dir(root);
    // Every option that shapes the patch text is set here, so the user's
    // diff configuration cannot reshape what the parser reads: an external
    // diff prints no patch, textconv moves lines off the file's own, other
    // prefixes rename the files, inter-hunk context — or context from
    // GIT_DIFF_OPTS, which outranks `--unified` — takes in unchanged lines,
    // copy or no rename detection changes which files are new, and a
    // submodule setting hides a changed submodule or prints it as a log.
    // The rename and submodule choices are git's defaults.
    cmd.env_remove("GIT_DIFF_OPTS");
    cmd.args([
        "-c",
        "core.quotepath=false",
        "diff",
        "--relative",
        "--unified=0",
        "--inter-hunk-context=0",
        "--no-color",
        "--no-ext-diff",
        "--no-textconv",
        "--src-prefix=a/",
        "--dst-prefix=b/",
        "--find-renames",
        "--submodule=short",
        "--ignore-submodules=none",
    ]);
    if staged {
        cmd.arg("--cached");
    }
    cmd.args([base, "--"]);

    let output = cmd.output().context("Failed to run git diff")?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        anyhow::bail!("git diff failed: {}", stderr.trim());
    }

    let diff_output = String::from_utf8_lossy(&output.stdout);
    Ok(parse_diff_output(&diff_output, root))
}

/// Every file the diff changes, from the same diff's `--numstat`. The patch
/// alone would miss those it names no lines for: an empty file, a mode
/// change, a pure rename, and a file git diffs as binary.
fn changed_files(root: &Path, base: &str, staged: bool) -> Result<Vec<PathBuf>> {
    let mut cmd = Command::new("git");
    cmd.current_dir(root);
    cmd.args([
        "diff",
        "--relative",
        "--numstat",
        "-z",
        "--no-ext-diff",
        "--no-textconv",
        "--find-renames",
        "--ignore-submodules=none",
    ]);
    if staged {
        cmd.arg("--cached");
    }
    cmd.args([base, "--"]);

    let output = cmd.output().context("Failed to run git diff")?;
    if !output.status.success() {
        anyhow::bail!(
            "git diff failed: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    Ok(parse_numstat(&output.stdout, root))
}

/// `--numstat -z` writes `added<TAB>deleted<TAB>path<NUL>`, or for a rename
/// `added<TAB>deleted<TAB><NUL>old<NUL>new<NUL>`.
fn parse_numstat(numstat: &[u8], root: &Path) -> Vec<PathBuf> {
    let mut fields = numstat.split(|&b| b == 0);
    let mut files = Vec::new();
    while let Some(record) = fields.next() {
        if record.is_empty() {
            continue;
        }
        let record = String::from_utf8_lossy(record);
        let path = match record.splitn(3, '\t').nth(2) {
            Some("") | None => {
                fields.next();
                fields
                    .next()
                    .map(|new| String::from_utf8_lossy(new).into_owned())
            }
            Some(path) => Some(path.to_string()),
        };
        if let Some(path) = path {
            files.push(root.join(path));
        }
    }
    files
}

/// What the patch says: the hunks it names, and the files whose content
/// changed but which git diffs as binary, naming no lines.
struct ParsedDiff {
    hunks: Vec<DiffHunk>,
    binary: Vec<PathBuf>,
}

fn parse_diff_output(diff: &str, root: &Path) -> ParsedDiff {
    let mut hunks = Vec::new();
    let mut binary = Vec::new();
    let mut old_file: Option<PathBuf> = None;
    let mut current_file: Option<PathBuf> = None;
    let mut block_file: Option<PathBuf> = None;
    let mut in_header = false;

    for line in diff.lines() {
        if let Some(rest) = line.strip_prefix("diff --git ") {
            // Each file block starts here; only its `---`/`+++` lines before
            // the first `@@` name the file. A `---`/`+++` line after a hunk is
            // deleted/added content (e.g. a Lua `--` comment becomes `--- …`)
            // and must not be mistaken for a header.
            in_header = true;
            old_file = None;
            current_file = None;
            block_file = same_path_header(rest).map(|p| root.join(p));
        } else if in_header && let Some(rest) = line.strip_prefix("rename to ") {
            block_file = git_path(rest).map(|(path, _)| root.join(path));
        } else if in_header && line.starts_with("Binary files ") {
            binary.extend(block_file.clone());
        } else if in_header && let Some(rest) = line.strip_prefix("--- ") {
            old_file = diff_header_path(rest).map(|p| root.join(p));
        } else if in_header && let Some(rest) = line.strip_prefix("+++ ") {
            // A fully deleted file's new side is `/dev/null`; fall back to the
            // old-side path so its hunk is attributed and the pre-image can be
            // read — never silently dropped.
            current_file = diff_header_path(rest)
                .map(|p| root.join(p))
                .or_else(|| old_file.clone());
        } else if line.starts_with("@@ ") {
            in_header = false;
            if let Some(ref file) = current_file
                && let Some(mut hunk) = parse_hunk_header(line, file.clone())
            {
                if let Some(old) = &old_file {
                    hunk.old_file = old.clone();
                }
                hunks.push(hunk);
            }
        }
    }

    ParsedDiff { hunks, binary }
}

/// The path of a `diff --git a/X b/X` header whose two sides are the same
/// path, as every block's are but a rename's (whose `rename to` line names
/// it). Split at its middle, a path holding spaces is still unambiguous.
fn same_path_header(rest: &str) -> Option<String> {
    if rest.starts_with('"') {
        let (old, after) = git_path(rest)?;
        let (new, _) = git_path(after.strip_prefix(' ')?)?;
        let old = old.strip_prefix("a/")?;
        return (new.strip_prefix("b/")? == old).then(|| old.to_string());
    }
    let rest = rest.strip_prefix("a/")?;
    let half = rest.len().checked_sub(3)? / 2;
    let (old, new) = (rest.get(..half)?, rest.get(half..)?);
    (new.strip_prefix(" b/")? == old).then(|| old.to_string())
}

/// A path as git prints it, and what follows it: verbatim, or — when the
/// path holds a double quote, a backslash or a control character, which
/// `core.quotepath=false` still quotes — in double quotes with C escapes.
/// A verbatim path runs to the end of `field`.
fn git_path(field: &str) -> Option<(String, &str)> {
    let Some(quoted) = field.strip_prefix('"') else {
        return Some((field.to_string(), ""));
    };
    let mut bytes = Vec::new();
    let mut chars = quoted.char_indices();
    while let Some((at, c)) = chars.next() {
        match c {
            '"' => {
                let path = String::from_utf8_lossy(&bytes).into_owned();
                return Some((path, &quoted[at + 1..]));
            }
            '\\' => {
                let (_, escaped) = chars.next()?;
                bytes.push(match escaped {
                    'a' => 0x07,
                    'b' => 0x08,
                    't' => b'\t',
                    'n' => b'\n',
                    'v' => 0x0b,
                    'f' => 0x0c,
                    'r' => b'\r',
                    '"' => b'"',
                    '\\' => b'\\',
                    '0'..='3' => {
                        let digit = |c: char| c.to_digit(8);
                        let (high, mid, low) = (
                            digit(escaped)?,
                            digit(chars.next()?.1)?,
                            digit(chars.next()?.1)?,
                        );
                        (high * 64 + mid * 8 + low) as u8
                    }
                    _ => return None,
                });
            }
            c => bytes.extend_from_slice(c.encode_utf8(&mut [0; 4]).as_bytes()),
        }
    }
    None
}

/// The path inside a `--- `/`+++ ` diff header: strips the `a/`/`b/` prefix and
/// any trailing tab-separated metadata (git appends a tab when the path holds a
/// space). `None` for `/dev/null`. Paths are literal because the diff is
/// produced with `core.quotepath=false`.
fn diff_header_path(rest: &str) -> Option<String> {
    let path = if rest.starts_with('"') {
        git_path(rest)?.0
    } else {
        rest.split('\t').next().unwrap_or(rest).to_string()
    };
    if path == "/dev/null" {
        return None;
    }
    Some(
        path.strip_prefix("a/")
            .or_else(|| path.strip_prefix("b/"))
            .unwrap_or(&path)
            .to_string(),
    )
}

fn parse_hunk_header(header: &str, file: PathBuf) -> Option<DiffHunk> {
    // Format: @@ -old_start,old_count +new_start,new_count @@
    let parts: Vec<&str> = header.split_whitespace().collect();
    if parts.len() < 3 {
        return None;
    }

    let old_range = parts[1].trim_start_matches('-');
    let new_range = parts[2].trim_start_matches('+');

    let (old_start, old_count) = parse_range(old_range)?;
    let (new_start, new_count) = parse_range(new_range)?;

    let change_type = if old_count == 0 {
        ChangeType::Added
    } else if new_count == 0 {
        ChangeType::Deleted
    } else {
        ChangeType::Modified
    };

    // Both coordinate systems are carried verbatim. Added/Modified resolve
    // against the new (current) tree via start_line/line_count; Deleted
    // resolves against the pre-image via old_start/old_count — never anchored
    // onto a current-tree line, which would map a deletion onto a live
    // neighbour and attribute its references to the deleted symbol.
    Some(DiffHunk {
        old_file: file.clone(),
        file,
        start_line: new_start,
        line_count: new_count,
        old_start,
        old_count,
        change_type,
    })
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

/// Parse a hunk range `start[,count]` into `(start, count)`. Returns `None` on
/// malformed digits rather than defaulting to 1 — a guessed coordinate would
/// silently mis-attribute the hunk; the caller drops the whole hunk instead. An
/// absent count is the git shorthand for 1.
fn parse_range(range: &str) -> Option<(u32, u32)> {
    let mut parts = range.split(',');
    let start = parts.next()?.parse().ok()?;
    let count = match parts.next() {
        Some(c) => c.parse().ok()?,
        None => 1,
    };
    // A hunk range is `start[,count]` and nothing more — a third field means the
    // header is malformed, so fail closed rather than silently ignore it.
    if parts.next().is_some() {
        return None;
    }
    Some((start, count))
}

/// Whether a changed line at `line` meaningfully attributes to `sym`. A callable
/// (Function/Method/Constructor) owns its whole body; a leaf symbol (no
/// children) owns all its lines; any symbol owns its own declaration line. A
/// non-callable CONTAINER (impl/class/module/namespace) spans its whole block
/// with members as children, so a line in the gap BETWEEN members resolves
/// innermost to the container yet carries none of its meaning — attributing it,
/// and the container's full reference set, to that line would be a false
/// positive, so it is excluded. Shared by the deletion and live (Added/Modified)
/// hunk paths so both attribute identically.
fn line_attributes_to_symbol(sym: &crate::models::symbol::Symbol, line: u32) -> bool {
    sym.kind.is_callable() || sym.children.is_empty() || line == sym.location.line
}

#[allow(clippy::too_many_arguments)]
async fn analyze_hunks(
    app: &App,
    hunks: &[DiffHunk],
    root: &Path,
    preimage_ref: &str,
    test_scope: &TestScope,
    include_callers: bool,
    max_symbols: usize,
    calls_limit: usize,
) -> (Vec<ChangedSymbolImpact>, Vec<String>, bool) {
    // Group hunks by file
    let mut file_hunks: BTreeMap<&PathBuf, Vec<&DiffHunk>> = BTreeMap::new();
    for hunk in hunks {
        file_hunks.entry(&hunk.file).or_default().push(hunk);
    }

    let mut changes = Vec::new();
    let mut unmeasured = Vec::new();
    let mut symbol_count = 0;
    let at_cap = |n: usize| max_symbols > 0 && n >= max_symbols;

    let mut stopped_at_cap = false;
    for (file, hunks) in file_hunks {
        if at_cap(symbol_count) {
            stopped_at_cap = true;
            break;
        }

        // Current-tree symbols, loaded once when the file survives — needed both
        // for live (Added/Modified) hunks and to recognise a deletion that only
        // removed body lines of a surviving symbol (a Modified, not a deletion).
        let file_exists = file.exists();
        let current_symbols = if file_exists {
            declared_in(app, file, FindSymbolsOptions::default())
                .await
                .ok()
                .map(|read| read.symbols)
        } else {
            None
        };

        // One dedup set per file, keyed by declaration identity (line, column),
        // NOT name: two distinct same-named declarations (impl A::new vs
        // impl B::new) both count, while one symbol touched by several hunks
        // counts once. Spans deletion-derived and live rows alike.
        let mut seen: std::collections::HashSet<(u32, u32)> = std::collections::HashSet::new();

        // Deletion hunks. A deletion whose old range held an actual declaration
        // is a deleted symbol, resolved against the pre-image. A deletion that
        // removed only body lines of a surviving symbol MODIFIED that symbol —
        // resolve it against the current tree instead of emitting a useless
        // Unresolved deletion (a still-existing function is not "deleted").
        for hunk in hunks
            .iter()
            .filter(|h| matches!(h.change_type, ChangeType::Deleted))
        {
            if at_cap(symbol_count) {
                stopped_at_cap = true;
                break;
            }
            let deleted_rows = resolve_deleted_hunk(root, preimage_ref, hunk);
            // Reclassify ONLY a verified body-line deletion: the pre-image was
            // read and declared no symbol in the deleted range, so a surviving
            // symbol was Modified. A `PreimageUnavailable` row is NOT eligible —
            // we could not check what was deleted, so guessing a live Modified
            // would paper over the gap; it stays a disclosed deletion.
            let body_only_deletion = deleted_rows.len() == 1
                && deleted_rows[0].deletion == Some(DeletionResolution::NoSymbolInRange);

            // Reclassify a body-line deletion as a Modified of the enclosing
            // CURRENT symbol, using the shared `line_attributes_to_symbol` rule
            // (callable body / leaf / own declaration line — never a container's
            // inter-member gap) plus a strict-interior check: the deletion point
            // must be before the symbol's last line, so a blank line AFTER it
            // (new_start == f_end) is not attributed. Both guard against a false
            // `Modified <container>` row that would carry the container's
            // irrelevant references.
            let enclosing = if body_only_deletion {
                current_symbols.as_ref().and_then(|s| {
                    let f = find_symbol_at_position(s, hunk.start_line, None)?;
                    let f_end = f.location.end_line.unwrap_or(f.location.line);
                    (line_attributes_to_symbol(f, hunk.start_line) && hunk.start_line < f_end)
                        .then_some(f)
                })
            } else {
                None
            };
            if let Some(sym) = enclosing {
                if seen.insert((sym.location.line, sym.location.column)) {
                    let impact = analyze_symbol_impact(
                        app.lsp.as_ref(),
                        file,
                        sym.clone(),
                        ChangeType::Modified,
                        root,
                        test_scope,
                        include_callers,
                        calls_limit,
                    )
                    .await;
                    changes.push(impact);
                    symbol_count += 1;
                }
                continue;
            }

            for row in deleted_rows {
                if at_cap(symbol_count) {
                    stopped_at_cap = true;
                    break;
                }
                changes.push(row);
                symbol_count += 1;
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
        let Some(symbols) = current_symbols.as_ref() else {
            // Added/Modified hunks exist but no current symbols. If the file is
            // present, find_symbols errored — disclose it as an unmeasured file
            // (a lower bound) instead of silently dropping its changes.
            if file_exists {
                unmeasured.push(relative_display(file, root));
            }
            continue;
        };

        for hunk in live_hunks {
            if at_cap(symbol_count) {
                stopped_at_cap = true;
                break;
            }

            // Find symbols affected by this hunk. Same attribution rule as the
            // deletion path (one shared helper): a changed line that resolves
            // innermost to a non-callable container gap (a comment/blank line
            // between members) carries none of the container's meaning, so it is
            // NOT attributed to the container with the container's references.
            let affected_symbols: Vec<_> = (hunk.start_line
                ..hunk.start_line + hunk.line_count.max(1))
                .filter_map(|line| {
                    find_symbol_at_position(symbols, line, None)
                        .filter(|sym| line_attributes_to_symbol(sym, line))
                })
                .collect();

            for sym in affected_symbols {
                if !seen.insert((sym.location.line, sym.location.column)) {
                    continue;
                }

                if at_cap(symbol_count) {
                    stopped_at_cap = true;
                    break;
                }

                let change_type = symbol_change_type(
                    &hunk.change_type,
                    sym.location.line,
                    hunk.start_line,
                    hunk.line_count,
                );

                let impact = analyze_symbol_impact(
                    app.lsp.as_ref(),
                    file,
                    sym.clone(),
                    change_type,
                    root,
                    test_scope,
                    include_callers,
                    calls_limit,
                )
                .await;

                changes.push(impact);
                symbol_count += 1;
            }
        }
    }

    (changes, unmeasured, stopped_at_cap)
}

async fn analyze_symbol_impact(
    lsp: &dyn LspService,
    file: &Path,
    sym: crate::models::symbol::Symbol,
    change_type: ChangeType,
    root: &Path,
    test_scope: &TestScope,
    include_callers: bool,
    calls_limit: usize,
) -> ChangedSymbolImpact {
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

    let classified = analysis.classify(test_scope);

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
                    .take(calls_limit)
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

/// Resolve a deleted hunk against the pre-image (old git tree) — never the
/// current tree. Returns one row per symbol declared inside the deleted line
/// range; if the pre-image is unavailable or declares no symbol there, one
/// `Unresolved` row, so a deletion is disclosed rather than silently dropped.
fn resolve_deleted_hunk(
    root: &Path,
    preimage_ref: &str,
    hunk: &DiffHunk,
) -> Vec<ChangedSymbolImpact> {
    let file = &hunk.old_file;
    let deletion_row = |resolution| ChangedSymbolImpact {
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
    };

    let relpath = file.strip_prefix(root).unwrap_or(file);
    let Some(content) = git_show(root, preimage_ref, relpath) else {
        return vec![deletion_row(DeletionResolution::PreimageUnavailable)];
    };

    let language = crate::models::symbol::Language::from_path(file);
    let lo = hunk.old_start;
    let hi = hunk.old_start.saturating_add(hunk.old_count.max(1));
    let matched: Vec<ChangedSymbolImpact> = SymbolExtractor::shared()
        .extract(file, &content, language)
        .into_iter()
        .filter(|s| s.location.line >= lo && s.location.line < hi)
        .map(|s| ChangedSymbolImpact {
            name: Some(s.name),
            kind: Some(s.kind.to_string()),
            // Old-file coordinates: the only honest position for a symbol that
            // no longer exists in the current tree.
            location: Some(LocationOutput::from_path(
                file,
                s.location.line,
                s.location.column,
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
        })
        .collect();

    if matched.is_empty() {
        vec![deletion_row(DeletionResolution::NoSymbolInRange)]
    } else {
        matched
    }
}

/// `git show <ref>:./<relpath>` — the file content at the pre-image revision.
/// The `./` resolves the path from `root` rather than the repository's top.
fn git_show(root: &Path, reference: &str, relpath: &Path) -> Option<String> {
    // git names paths with forward slashes on every platform, and a Unix
    // file name may itself hold a backslash, so the components are joined.
    let spec = format!(
        "{reference}:./{}",
        relpath
            .components()
            .map(|part| part.as_os_str().to_string_lossy())
            .collect::<Vec<_>>()
            .join("/")
    );
    let output = Command::new("git")
        .current_dir(root)
        .args(["show", &spec])
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    String::from_utf8(output.stdout).ok()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    #[test]
    fn every_changed_file_is_read_from_numstat() {
        let root = Path::new("/r");
        let numstat = b"1\t1\tm.py\x000\t0\tempty.py\x00-\t-\tlogo.png\x00-\t-\t\x00old.bin\x00new.bin\x002\t0\t\x00a.py\x00b.py\x00";
        assert_eq!(
            parse_numstat(numstat, root),
            ["m.py", "empty.py", "logo.png", "new.bin", "b.py"].map(|p| root.join(p))
        );
    }

    #[test]
    fn a_binary_file_is_one_whose_content_changed() {
        let root = Path::new("/r");
        let diff = "diff --git a/logo.png b/logo.png\n\
                    index 1111111..2222222 100644\n\
                    Binary files a/logo.png and b/logo.png differ\n\
                    diff --git a/icon.png b/icon.png\n\
                    old mode 100644\n\
                    new mode 100755\n\
                    diff --git a/old.bin b/new.bin\n\
                    similarity index 60%\n\
                    rename from old.bin\n\
                    rename to new.bin\n\
                    index 3333333..4444444\n\
                    Binary files a/old.bin and b/new.bin differ\n\
                    diff --git a/my file.png b/my file.png\n\
                    index 5555555..6666666 100644\n\
                    Binary files a/my file.png and b/my file.png differ\n\
                    diff --git a/add.png b/add.png\n\
                    new file mode 100644\n\
                    index 0000000..7777777\n\
                    Binary files /dev/null and b/add.png differ\n";
        assert_eq!(
            parse_diff_output(diff, root).binary,
            ["logo.png", "new.bin", "my file.png", "add.png"].map(|p| root.join(p))
        );
    }

    #[test]
    fn a_path_git_quotes_is_read_as_the_file_it_names() {
        let root = Path::new("/r");
        let diff = "diff --git \"a/we\\\"ird.py\" \"b/we\\\"ird.py\"\n\
                    index 1111111..2222222 100644\n\
                    --- \"a/we\\\"ird.py\"\n\
                    +++ \"b/we\\\"ird.py\"\n\
                    @@ -1 +1 @@\n\
                    diff --git \"a/bin\\\"ary.py\" \"b/bin\\\"ary.py\"\n\
                    index 3333333..4444444 100644\n\
                    Binary files \"a/bin\\\"ary.py\" and \"b/bin\\\"ary.py\" differ\n\
                    diff --git \"a/old\\\\x.py\" \"b/tab\\there.py\"\n\
                    similarity index 90%\n\
                    rename from \"old\\\\x.py\"\n\
                    rename to \"tab\\there.py\"\n\
                    --- \"a/old\\\\x.py\"\n\
                    +++ \"b/tab\\there.py\"\n\
                    @@ -3 +3 @@\n";
        let parsed = parse_diff_output(diff, root);
        assert_eq!(parsed.hunks[0].file, root.join("we\"ird.py"));
        assert_eq!(parsed.binary, [root.join("bin\"ary.py")]);
        assert_eq!(parsed.hunks[1].file, root.join("tab\there.py"));
        assert_eq!(parsed.hunks[1].old_file, root.join("old\\x.py"));
    }

    #[test]
    fn a_hunk_in_a_renamed_file_reads_its_preimage_from_the_old_path() {
        let root = Path::new("/r");
        let diff = "diff --git a/old.py b/new.py\n\
                    similarity index 80%\n\
                    rename from old.py\n\
                    rename to new.py\n\
                    --- a/old.py\n\
                    +++ b/new.py\n\
                    @@ -5,2 +4,0 @@\n";
        let hunks = parse_diff_output(diff, root).hunks;
        assert_eq!(hunks[0].file, root.join("new.py"));
        assert_eq!(hunks[0].old_file, root.join("old.py"));
    }

    // ---------------------------------------------------------------
    // parse_range tests
    // ---------------------------------------------------------------

    #[test]
    fn parse_range_with_start_and_count() {
        assert_eq!(parse_range("10,5"), Some((10, 5)));
    }

    #[test]
    fn parse_range_zero_count() {
        assert_eq!(parse_range("7,0"), Some((7, 0)));
    }

    #[test]
    fn parse_range_large_numbers() {
        assert_eq!(parse_range("99999,500"), Some((99999, 500)));
    }

    #[test]
    fn parse_range_absent_count_is_one() {
        assert_eq!(parse_range("5"), Some((5, 1)));
    }

    /// Malformed digits FAIL the parse (None) rather than defaulting to 1 — a
    /// guessed coordinate would silently mis-attribute the hunk; the caller
    /// drops it instead.
    #[test]
    fn parse_range_malformed_fails_closed() {
        assert_eq!(parse_range("abc,3"), None);
        assert_eq!(parse_range("5,abc"), None);
        assert_eq!(parse_range("xyz"), None);
        assert_eq!(parse_range(""), None);
        // A third comma-separated field is malformed — rejected, not ignored.
        assert_eq!(parse_range("10,5,garbage"), None);
        assert_eq!(parse_range("10,5,7"), None);
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

    // ---------------------------------------------------------------
    // parse_hunk_header tests
    // ---------------------------------------------------------------

    fn dummy_file() -> PathBuf {
        PathBuf::from("/tmp/test.rs")
    }

    #[test]
    fn parse_hunk_header_standard() {
        let hunk = parse_hunk_header("@@ -10,5 +12,7 @@ fn example()", dummy_file());
        let hunk = hunk.expect("should parse valid hunk header");
        assert_eq!(hunk.start_line, 12);
        assert_eq!(hunk.line_count, 7);
        assert!(matches!(hunk.change_type, ChangeType::Modified));
        assert_eq!(hunk.file, dummy_file());
    }

    #[test]
    fn parse_hunk_header_single_line() {
        let hunk = parse_hunk_header("@@ -1 +1 @@", dummy_file());
        let hunk = hunk.expect("should parse single-line hunk");
        assert_eq!(hunk.start_line, 1);
        assert_eq!(hunk.line_count, 1);
        assert!(matches!(hunk.change_type, ChangeType::Modified));
    }

    #[test]
    fn parse_hunk_header_with_context_text() {
        let hunk = parse_hunk_header("@@ -100,20 +150,30 @@ impl Foo {", dummy_file());
        let hunk = hunk.expect("should parse hunk with trailing context");
        assert_eq!(hunk.start_line, 150);
        assert_eq!(hunk.line_count, 30);
        assert!(matches!(hunk.change_type, ChangeType::Modified));
    }

    #[test]
    fn parse_hunk_header_added_lines() {
        // old_count=0 means pure addition
        let hunk = parse_hunk_header("@@ -5,0 +6,3 @@", dummy_file());
        let hunk = hunk.expect("should parse addition hunk");
        assert_eq!(hunk.start_line, 6);
        assert_eq!(hunk.line_count, 3);
        assert!(matches!(hunk.change_type, ChangeType::Added));
    }

    #[test]
    fn parse_hunk_header_deleted_lines_carry_old_coordinates() {
        // new_count=0 means pure deletion; the OLD coordinates locate the
        // deleted symbol in the pre-image — they are never anchored onto a
        // current-tree line.
        let hunk = parse_hunk_header("@@ -10,4 +9,0 @@", dummy_file());
        let hunk = hunk.expect("should parse deletion hunk");
        assert!(matches!(hunk.change_type, ChangeType::Deleted));
        assert_eq!(hunk.old_start, 10);
        assert_eq!(hunk.old_count, 4);
        // New coords are passed through verbatim (unused for deletions), not
        // re-anchored to a phantom (new_start.max(1), 1).
        assert_eq!(hunk.start_line, 9);
        assert_eq!(hunk.line_count, 0);
    }

    #[test]
    fn parse_hunk_header_deletion_at_file_start() {
        let hunk = parse_hunk_header("@@ -1,2 +0,0 @@", dummy_file());
        let hunk = hunk.expect("should parse deletion at line 0");
        assert!(matches!(hunk.change_type, ChangeType::Deleted));
        assert_eq!(hunk.old_start, 1);
        assert_eq!(hunk.old_count, 2);
        assert_eq!(hunk.start_line, 0);
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

    // ---------------------------------------------------------------
    // line_attributes_to_symbol — the shared attribution rule guarding
    // both the deletion-reclassification and live (Added/Modified) paths
    // ---------------------------------------------------------------

    #[test]
    fn line_attributes_to_symbol_excludes_container_gaps() {
        use crate::models::symbol::{Location, Symbol, SymbolKind};

        let pt = |line: u32| Location::point(PathBuf::from("x.rs"), line, 1);
        let method = Symbol::new("bar".to_string(), SymbolKind::Method, pt(3));
        let container = Symbol::new("Foo".to_string(), SymbolKind::Class, pt(1))
            .with_children(vec![method.clone()]);
        let leaf_field = Symbol::new("FIELD".to_string(), SymbolKind::Field, pt(12));

        // A callable owns its whole body — a body line attributes to it.
        assert!(line_attributes_to_symbol(&method, 4));
        // A non-callable container's inter-member gap line does NOT attribute to
        // the container — otherwise a blank line between members would surface a
        // spurious `Modified <container>` row.
        assert!(!line_attributes_to_symbol(&container, 7));
        // ...but editing the container's own declaration line does attribute.
        assert!(line_attributes_to_symbol(&container, 1));
        // A leaf symbol (no children) owns all of its lines.
        assert!(line_attributes_to_symbol(&leaf_field, 13));
    }

    #[test]
    fn parse_hunk_header_invalid_format_returns_none() {
        // Fewer than 3 whitespace-separated parts → None
        assert!(parse_hunk_header("@@", dummy_file()).is_none());
        assert!(parse_hunk_header("@@ -1", dummy_file()).is_none());
        assert!(parse_hunk_header("", dummy_file()).is_none());
    }

    #[test]
    fn parse_hunk_header_too_few_parts_returns_none() {
        assert!(parse_hunk_header("@@ -1", dummy_file()).is_none());
    }

    // ---------------------------------------------------------------
    // parse_diff_output tests
    // ---------------------------------------------------------------

    #[test]
    fn parse_diff_output_empty_input() {
        let root = Path::new("/project");
        let hunks = parse_diff_output("", root).hunks;
        assert!(hunks.is_empty());
    }

    #[test]
    fn parse_diff_output_single_file_one_hunk() {
        let root = Path::new("/project");
        let diff = "\
diff --git a/src/main.rs b/src/main.rs
--- a/src/main.rs
+++ b/src/main.rs
@@ -10,3 +10,5 @@ fn main() {";

        let hunks = parse_diff_output(diff, root).hunks;
        assert_eq!(hunks.len(), 1);
        assert_eq!(hunks[0].file, root.join("src/main.rs"));
        assert_eq!(hunks[0].start_line, 10);
        assert_eq!(hunks[0].line_count, 5);
        assert!(matches!(hunks[0].change_type, ChangeType::Modified));
    }

    #[test]
    fn parse_diff_output_single_file_multiple_hunks() {
        let root = Path::new("/project");
        let diff = "\
diff --git a/src/lib.rs b/src/lib.rs
--- a/src/lib.rs
+++ b/src/lib.rs
@@ -5,2 +5,4 @@ fn foo() {
+    added_line();
+    another_line();
@@ -20,3 +22,1 @@ fn bar() {
-    removed_line_1();
-    removed_line_2();";

        let hunks = parse_diff_output(diff, root).hunks;
        assert_eq!(hunks.len(), 2);

        assert_eq!(hunks[0].file, root.join("src/lib.rs"));
        assert_eq!(hunks[0].start_line, 5);
        assert_eq!(hunks[0].line_count, 4);

        assert_eq!(hunks[1].file, root.join("src/lib.rs"));
        assert_eq!(hunks[1].start_line, 22);
        assert_eq!(hunks[1].line_count, 1);
    }

    #[test]
    fn parse_diff_output_multiple_files() {
        let root = Path::new("/repo");
        let diff = "\
diff --git a/src/foo.rs b/src/foo.rs
--- a/src/foo.rs
+++ b/src/foo.rs
@@ -1,0 +1,5 @@
diff --git a/src/bar.rs b/src/bar.rs
--- a/src/bar.rs
+++ b/src/bar.rs
@@ -10,2 +10,3 @@ fn bar() {
@@ -30,5 +31,0 @@ fn baz() {";

        let hunks = parse_diff_output(diff, root).hunks;
        assert_eq!(hunks.len(), 3);

        // First file: pure addition
        assert_eq!(hunks[0].file, root.join("src/foo.rs"));
        assert_eq!(hunks[0].start_line, 1);
        assert_eq!(hunks[0].line_count, 5);
        assert!(matches!(hunks[0].change_type, ChangeType::Added));

        // Second file, first hunk: modification
        assert_eq!(hunks[1].file, root.join("src/bar.rs"));
        assert_eq!(hunks[1].start_line, 10);
        assert_eq!(hunks[1].line_count, 3);
        assert!(matches!(hunks[1].change_type, ChangeType::Modified));

        // Second file, second hunk: deletion — old coords locate the deleted
        // symbol in the pre-image; new coords pass through verbatim.
        assert_eq!(hunks[2].file, root.join("src/bar.rs"));
        assert_eq!(hunks[2].old_start, 30);
        assert_eq!(hunks[2].old_count, 5);
        assert_eq!(hunks[2].start_line, 31);
        assert_eq!(hunks[2].line_count, 0);
        assert!(matches!(hunks[2].change_type, ChangeType::Deleted));
    }

    #[test]
    fn parse_diff_output_ignores_lines_before_file_header() {
        let root = Path::new("/project");
        // Hunk line before any +++ line should be ignored
        let diff = "\
@@ -1,1 +1,1 @@
diff --git a/src/main.rs b/src/main.rs
--- a/src/main.rs
+++ b/src/main.rs
@@ -5,1 +5,2 @@ fn main() {";

        let hunks = parse_diff_output(diff, root).hunks;
        assert_eq!(hunks.len(), 1);
        assert_eq!(hunks[0].file, root.join("src/main.rs"));
        assert_eq!(hunks[0].start_line, 5);
    }

    #[test]
    fn parse_diff_output_whole_file_deletion_attributes_old_path() {
        let root = Path::new("/project");
        // A fully deleted file: git emits `+++ /dev/null`. The hunk must be
        // attributed to the old-side path (from `--- a/…`) so its pre-image can
        // be resolved — never silently dropped.
        let diff = "\
diff --git a/src/old.rs b/src/old.rs
--- a/src/old.rs
+++ /dev/null
@@ -1,10 +0,0 @@";

        let hunks = parse_diff_output(diff, root).hunks;
        assert_eq!(hunks.len(), 1);
        assert_eq!(hunks[0].file, root.join("src/old.rs"));
        assert_eq!(hunks[0].old_start, 1);
        assert_eq!(hunks[0].old_count, 10);
        assert!(matches!(hunks[0].change_type, ChangeType::Deleted));
    }

    #[test]
    fn parse_diff_output_non_ascii_and_spaced_paths() {
        let root = Path::new("/repo");
        // With core.quotepath=false a non-ASCII path is literal; a path holding
        // a space gets a trailing tab in the header. Both must map to the right
        // file rather than being dropped or attributed to the previous file.
        let diff = "\
diff --git a/src/모듈.rs b/src/모듈.rs
--- a/src/모듈.rs
+++ b/src/모듈.rs
@@ -3,2 +3,4 @@ fn 함수() {
diff --git a/src/with space.rs b/src/with space.rs
--- a/src/with space.rs\t
+++ b/src/with space.rs\t
@@ -1,1 +1,2 @@";

        let hunks = parse_diff_output(diff, root).hunks;
        assert_eq!(hunks.len(), 2);
        assert_eq!(hunks[0].file, root.join("src/모듈.rs"));
        assert_eq!(hunks[1].file, root.join("src/with space.rs"));
    }

    #[test]
    fn parse_diff_output_deleted_comment_line_is_not_a_header() {
        let root = Path::new("/project");
        // A deleted `--` comment line becomes `--- …` and an added one `+++ …`;
        // appearing after the `@@`, they are content, not a file header.
        let diff = "\
diff --git a/x.lua b/x.lua
--- a/x.lua
+++ b/x.lua
@@ -5,1 +5,1 @@ local function f()
--- old comment
+++ new comment";

        let hunks = parse_diff_output(diff, root).hunks;
        assert_eq!(hunks.len(), 1);
        assert_eq!(hunks[0].file, root.join("x.lua"));
    }
}
