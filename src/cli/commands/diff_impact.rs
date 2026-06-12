use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{Context, Result};
use clap::Args;
use serde::Serialize;

use crate::app::App;
use crate::cli::analysis::LocationAnalysis;
use crate::cli::response::{CallHierarchyOutput, LocationOutput};
use crate::cli::utils::{TestMatcher, find_symbol_at_position};
use crate::models::lsp::FindSymbolsOptions;
use crate::services::lsp::LspService;

#[derive(Args, Debug)]
pub struct DiffImpactArgs {
    /// Git revision to compare against (default: HEAD)
    #[arg(default_value = "HEAD")]
    pub revision: String,

    /// Only analyze staged changes
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
}

/// Test coverage summary for diff analysis (aggregate over all changed symbols)
#[derive(Debug, Serialize)]
pub struct DiffCoverage {
    pub with_tests: usize,
    pub without_tests: usize,
    pub ratio: f32,
}

/// Changed symbol impact data (pure fact)
#[derive(Debug, Serialize)]
pub struct ChangedSymbolImpact {
    pub name: String,
    pub kind: String,
    pub location: LocationOutput,
    pub change_type: ChangeType,
    /// Total reference count
    pub refs: usize,
    /// Test code references
    pub test_refs: usize,
    /// Production code references
    pub prod_refs: usize,
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

struct DiffHunk {
    file: PathBuf,
    start_line: u32,
    line_count: u32,
    change_type: ChangeType,
}

pub async fn execute(args: DiffImpactArgs, app: &App) -> Result<()> {
    let ctx = &app.output;
    let root = ctx.root();
    let test_matcher = app.test_matcher();

    let hunks = parse_git_diff(root, &args.revision, args.staged)?;

    if hunks.is_empty() {
        ctx.print_success(DiffImpactOutput {
            revision: args.revision,
            changed_files_count: 0,
            changed_symbols_count: 0,
            total_references: 0,
            coverage: DiffCoverage {
                with_tests: 0,
                without_tests: 0,
                ratio: 1.0,
            },
            changes: vec![],
        });
        return Ok(());
    }

    let calls_limit = app.config().lsp.calls_limit;

    let changes = analyze_hunks(
        app.lsp.as_ref(),
        &hunks,
        root,
        test_matcher,
        args.callers,
        args.max_symbols,
        calls_limit,
    )
    .await;

    let changed_files: std::collections::HashSet<_> = hunks.iter().map(|h| &h.file).collect();
    let total_refs: usize = changes.iter().map(|c| c.refs).sum();
    let with_tests = changes.iter().filter(|c| c.test_refs > 0).count();
    let without_tests = changes.len().saturating_sub(with_tests);
    let coverage_ratio = if changes.is_empty() {
        1.0
    } else {
        with_tests as f32 / changes.len() as f32
    };

    let response = DiffImpactOutput {
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
    };

    ctx.print_success(response);
    Ok(())
}

fn parse_git_diff(root: &Path, revision: &str, staged: bool) -> Result<Vec<DiffHunk>> {
    let mut cmd = Command::new("git");
    cmd.current_dir(root);
    cmd.args(["diff", "--unified=0", "--no-color"]);

    if staged {
        cmd.arg("--cached");
    } else {
        cmd.arg(revision);
    }

    let output = cmd.output().context("Failed to run git diff")?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        anyhow::bail!("git diff failed: {}", stderr.trim());
    }

    let diff_output = String::from_utf8_lossy(&output.stdout);
    Ok(parse_diff_output(&diff_output, root))
}

fn parse_diff_output(diff: &str, root: &Path) -> Vec<DiffHunk> {
    let mut hunks = Vec::new();
    let mut current_file: Option<PathBuf> = None;

    for line in diff.lines() {
        if let Some(path_str) = line.strip_prefix("+++ b/") {
            current_file = Some(root.join(path_str));
        } else if line.starts_with("@@ ")
            && let Some(ref file) = current_file
            && let Some(hunk) = parse_hunk_header(line, file.clone())
        {
            hunks.push(hunk);
        }
    }

    hunks
}

fn parse_hunk_header(header: &str, file: PathBuf) -> Option<DiffHunk> {
    // Format: @@ -old_start,old_count +new_start,new_count @@
    let parts: Vec<&str> = header.split_whitespace().collect();
    if parts.len() < 3 {
        return None;
    }

    let old_range = parts[1].trim_start_matches('-');
    let new_range = parts[2].trim_start_matches('+');

    let (_old_start, old_count) = parse_range(old_range);
    let (new_start, new_count) = parse_range(new_range);

    let change_type = if old_count == 0 {
        ChangeType::Added
    } else if new_count == 0 {
        ChangeType::Deleted
    } else {
        ChangeType::Modified
    };

    // Always use new file coordinates. For deletions, new_start is the
    // adjacent line in the current file; use count=1 to anchor the lookup.
    let (start_line, line_count) = if matches!(change_type, ChangeType::Deleted) {
        (new_start.max(1), 1)
    } else {
        (new_start, new_count)
    };

    Some(DiffHunk {
        file,
        start_line,
        line_count,
        change_type,
    })
}

fn parse_range(range: &str) -> (u32, u32) {
    let parts: Vec<&str> = range.split(',').collect();
    let start = parts[0].parse().unwrap_or(1);
    let count = if parts.len() > 1 {
        parts[1].parse().unwrap_or(1)
    } else {
        1
    };
    (start, count)
}

async fn analyze_hunks(
    lsp: &dyn LspService,
    hunks: &[DiffHunk],
    root: &Path,
    test_matcher: &TestMatcher,
    include_callers: bool,
    max_symbols: usize,
    calls_limit: usize,
) -> Vec<ChangedSymbolImpact> {
    // Group hunks by file
    let mut file_hunks: BTreeMap<&PathBuf, Vec<&DiffHunk>> = BTreeMap::new();
    for hunk in hunks {
        file_hunks.entry(&hunk.file).or_default().push(hunk);
    }

    let mut changes = Vec::new();
    let mut symbol_count = 0;

    for (file, hunks) in file_hunks {
        if max_symbols > 0 && symbol_count >= max_symbols {
            break;
        }

        if !file.exists() {
            continue;
        }

        let symbols = match lsp.find_symbols(file, FindSymbolsOptions::default()).await {
            Ok(s) => s,
            Err(_) => continue,
        };

        for hunk in hunks {
            if max_symbols > 0 && symbol_count >= max_symbols {
                break;
            }

            // Find symbols affected by this hunk
            let affected_symbols: Vec<_> = (hunk.start_line
                ..hunk.start_line + hunk.line_count.max(1))
                .filter_map(|line| find_symbol_at_position(&symbols, line, None))
                .collect();

            // Deduplicate by symbol name
            let mut seen_names = std::collections::HashSet::new();
            for sym in affected_symbols {
                if !seen_names.insert(sym.name.clone()) {
                    continue;
                }

                if max_symbols > 0 && symbol_count >= max_symbols {
                    break;
                }

                let impact = analyze_symbol_impact(
                    lsp,
                    file,
                    sym.clone(),
                    hunk.change_type.clone(),
                    root,
                    test_matcher,
                    include_callers,
                    calls_limit,
                )
                .await;

                changes.push(impact);
                symbol_count += 1;
            }
        }
    }

    changes
}

async fn analyze_symbol_impact(
    lsp: &dyn LspService,
    file: &Path,
    sym: crate::models::symbol::Symbol,
    change_type: ChangeType,
    root: &Path,
    test_matcher: &TestMatcher,
    include_callers: bool,
    calls_limit: usize,
) -> ChangedSymbolImpact {
    let name = sym.name.clone();
    let kind = sym.kind.to_string();
    let line = sym.location.line;
    let column = sym.location.column;

    let analysis = match LocationAnalysis::for_symbol(lsp, file, sym).await {
        Ok(a) => a,
        Err(_) => {
            return ChangedSymbolImpact {
                name,
                kind,
                location: LocationOutput::from_path(file, line, column, root),
                change_type,
                refs: 0,
                test_refs: 0,
                prod_refs: 0,
                callers: vec![],
            };
        }
    };

    let classified = analysis.classify(root, test_matcher, true);

    let callers = if include_callers {
        lsp.incoming_calls(file, line, column)
            .await
            .map(|calls| {
                calls
                    .data
                    .iter()
                    .take(calls_limit)
                    .map(|c| CallHierarchyOutput::from_item(c, root))
                    .collect()
            })
            .unwrap_or_default()
    } else {
        vec![]
    };

    ChangedSymbolImpact {
        name,
        kind,
        location: LocationOutput::from_path(file, line, column, root),
        change_type,
        refs: classified.total,
        test_refs: classified.test,
        prod_refs: classified.prod,
        callers,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    // ---------------------------------------------------------------
    // parse_range tests
    // ---------------------------------------------------------------

    #[test]
    fn parse_range_with_start_and_count() {
        assert_eq!(parse_range("10,5"), (10, 5));
    }

    #[test]
    fn parse_range_single_value_defaults_count_to_one() {
        assert_eq!(parse_range("42"), (42, 1));
    }

    #[test]
    fn parse_range_zero_count() {
        assert_eq!(parse_range("7,0"), (7, 0));
    }

    #[test]
    fn parse_range_large_numbers() {
        assert_eq!(parse_range("99999,500"), (99999, 500));
    }

    #[test]
    fn parse_range_invalid_start_defaults_to_one() {
        assert_eq!(parse_range("abc,3"), (1, 3));
    }

    #[test]
    fn parse_range_invalid_count_defaults_to_one() {
        assert_eq!(parse_range("5,abc"), (5, 1));
    }

    #[test]
    fn parse_range_completely_invalid_defaults_both() {
        assert_eq!(parse_range("xyz"), (1, 1));
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
    fn parse_hunk_header_deleted_lines() {
        // new_count=0 means pure deletion; function adjusts to (new_start.max(1), 1)
        let hunk = parse_hunk_header("@@ -10,4 +9,0 @@", dummy_file());
        let hunk = hunk.expect("should parse deletion hunk");
        assert_eq!(hunk.start_line, 9);
        assert_eq!(hunk.line_count, 1);
        assert!(matches!(hunk.change_type, ChangeType::Deleted));
    }

    #[test]
    fn parse_hunk_header_deleted_at_line_zero_clamps_to_one() {
        let hunk = parse_hunk_header("@@ -1,2 +0,0 @@", dummy_file());
        let hunk = hunk.expect("should parse deletion at line 0");
        assert_eq!(hunk.start_line, 1); // max(0, 1) = 1
        assert_eq!(hunk.line_count, 1);
        assert!(matches!(hunk.change_type, ChangeType::Deleted));
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
        let hunks = parse_diff_output("", root);
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

        let hunks = parse_diff_output(diff, root);
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

        let hunks = parse_diff_output(diff, root);
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

        let hunks = parse_diff_output(diff, root);
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

        // Second file, second hunk: deletion (new_count=0 → adjusted)
        assert_eq!(hunks[2].file, root.join("src/bar.rs"));
        assert_eq!(hunks[2].start_line, 31);
        assert_eq!(hunks[2].line_count, 1);
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

        let hunks = parse_diff_output(diff, root);
        assert_eq!(hunks.len(), 1);
        assert_eq!(hunks[0].file, root.join("src/main.rs"));
        assert_eq!(hunks[0].start_line, 5);
    }

    #[test]
    fn parse_diff_output_dev_null_prefix_not_matched() {
        let root = Path::new("/project");
        // +++ /dev/null appears for deleted files; it doesn't start with "+++ b/"
        // so current_file stays None and no hunks are produced for that section
        let diff = "\
diff --git a/src/old.rs b/src/old.rs
--- a/src/old.rs
+++ /dev/null
@@ -1,10 +0,0 @@";

        let hunks = parse_diff_output(diff, root);
        assert!(hunks.is_empty());
    }
}
