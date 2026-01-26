//! Diff-impact command - analyze impact of git diff changes
//!
//! Parses git diff output to identify changed functions/methods,
//! then uses LSP to find all references and callers for each change.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{Context, Result};
use clap::Args;
use serde::Serialize;

use crate::app::App;
use crate::cli::response::{CallHierarchyOutput, LocationOutput};
use crate::cli::utils::{TestMatcher, find_symbol_at_line};
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
pub struct DiffImpactResponse {
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
        ctx.print_success_flat(DiffImpactResponse {
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

    let changes = analyze_hunks(
        app.lsp.as_ref(),
        &hunks,
        root,
        &test_matcher,
        args.callers,
        args.max_symbols,
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

    let response = DiffImpactResponse {
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

    ctx.print_success_flat(response);
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
    parse_diff_output(&diff_output, root)
}

fn parse_diff_output(diff: &str, root: &Path) -> Result<Vec<DiffHunk>> {
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

    Ok(hunks)
}

fn parse_hunk_header(header: &str, file: PathBuf) -> Option<DiffHunk> {
    // Format: @@ -old_start,old_count +new_start,new_count @@
    let parts: Vec<&str> = header.split_whitespace().collect();
    if parts.len() < 3 {
        return None;
    }

    let old_range = parts[1].trim_start_matches('-');
    let new_range = parts[2].trim_start_matches('+');

    let (old_start, old_count) = parse_range(old_range);
    let (new_start, new_count) = parse_range(new_range);

    let change_type = if old_count == 0 {
        ChangeType::Added
    } else if new_count == 0 {
        ChangeType::Deleted
    } else {
        ChangeType::Modified
    };

    let (start_line, line_count) = if matches!(change_type, ChangeType::Deleted) {
        (old_start, old_count)
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
) -> Vec<ChangedSymbolImpact> {
    // Group hunks by file
    let mut file_hunks: HashMap<&PathBuf, Vec<&DiffHunk>> = HashMap::new();
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
                .filter_map(|line| find_symbol_at_line(&symbols, line))
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
                    sym,
                    hunk.change_type.clone(),
                    root,
                    test_matcher,
                    include_callers,
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
    sym: &crate::models::symbol::Symbol,
    change_type: ChangeType,
    root: &Path,
    test_matcher: &TestMatcher,
    include_callers: bool,
) -> ChangedSymbolImpact {
    let line = sym.location.line;
    let column = sym.location.column;

    let refs = lsp
        .find_references(file, line, column)
        .await
        .unwrap_or_default();

    let mut test_count = 0;
    let mut prod_count = 0;

    for r in &refs {
        if r.file == file && r.line == line {
            continue;
        }
        if test_matcher.is_test_file(&r.file) {
            test_count += 1;
        } else {
            prod_count += 1;
        }
    }

    let callers = if include_callers {
        lsp.incoming_calls(file, line, column)
            .await
            .map(|calls| {
                calls
                    .iter()
                    .take(10)
                    .map(|c| CallHierarchyOutput::from_item(c, root))
                    .collect()
            })
            .unwrap_or_default()
    } else {
        vec![]
    };

    ChangedSymbolImpact {
        name: sym.name.clone(),
        kind: sym.kind.to_string(),
        location: LocationOutput::from_path(file, line, column, root),
        change_type,
        refs: test_count + prod_count,
        test_refs: test_count,
        prod_refs: prod_count,
        callers,
    }
}
