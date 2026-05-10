//! Symbol-aware writing — `replace_symbol_body`, `insert_before`,
//! `insert_after`. The CLI surface that mutates source files lives here
//! so an agent can grant a narrower set of permissions than the
//! read-only commands.
//!
//! Every operation:
//!   1. Resolves the target symbol via the LSP's `documentSymbol`,
//!      using the user-supplied `file:line:column`.
//!   2. Uses the symbol's `range_start_line` / `end_line` (1-indexed,
//!      half-inclusive at the bounds) to splice or anchor.
//!   3. Optionally dry-runs so the user/agent can preview a unified diff
//!      before any write hits disk.

use std::io::Read;
use std::path::Path;

use anyhow::Result;
use clap::{Args, Subcommand};
use serde::Serialize;

use crate::app::App;
use crate::cli::analysis::LocationAnalysis;
use crate::cli::{LocationArg, OutputError};
use crate::models::symbol::{Location, Symbol};

#[derive(Args, Debug)]
#[command(
    after_long_help = "Symbol-aware mutation. Each subcommand resolves the LSP target at \n\
                       file:line:column, then splices source code by the symbol's range.\n\
                       \n\
                       Examples:\n  \
                       symora write replace-body src/main.rs:42:1 --body \"$(cat new_fn.rs)\"\n  \
                       symora write insert-after src/main.rs:42:1 --code 'fn extra() {}' --dry-run"
)]
pub struct WriteArgs {
    #[command(subcommand)]
    pub command: WriteCommand,
}

#[derive(Subcommand, Debug)]
pub enum WriteCommand {
    /// Replace the resolved symbol's body with new source code.
    ReplaceBody(ReplaceBodyArgs),
    /// Insert source code immediately before the resolved symbol.
    InsertBefore(InsertArgs),
    /// Insert source code immediately after the resolved symbol.
    InsertAfter(InsertArgs),
}

#[derive(Args, Debug)]
pub struct ReplaceBodyArgs {
    /// file:line:col identifying the symbol to replace.
    #[command(flatten)]
    pub loc: LocationArg,

    /// New source for the symbol. Pass `-` to read from stdin.
    #[arg(long)]
    pub body: String,

    /// Preview the change without writing to disk.
    #[arg(long)]
    pub dry_run: bool,
}

#[derive(Args, Debug)]
pub struct InsertArgs {
    /// file:line:col identifying the anchor symbol.
    #[command(flatten)]
    pub loc: LocationArg,

    /// Source code to insert. Pass `-` to read from stdin.
    #[arg(long)]
    pub code: String,

    /// Preview the change without writing to disk.
    #[arg(long)]
    pub dry_run: bool,
}

#[derive(Debug, Serialize)]
struct WriteResult {
    operation: &'static str,
    file: String,
    target_symbol: String,
    target_kind: String,
    target_lines: Range,
    bytes_changed: i64,
    dry_run: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    preview: Option<String>,
}

#[derive(Debug, Serialize)]
struct Range {
    start: u32,
    end: u32,
}

pub async fn execute(args: WriteArgs, app: &App) -> Result<()> {
    let ctx = &app.output;
    match args.command {
        WriteCommand::ReplaceBody(a) => {
            let body = read_payload(&a.body)?;
            run_mutation("replace_body", a.loc, a.dry_run, app, |sym, lines| {
                replace_body_lines(sym, lines, &body)
            })
            .await
        }
        WriteCommand::InsertBefore(a) => {
            let code = read_payload(&a.code)?;
            run_mutation("insert_before", a.loc, a.dry_run, app, |sym, lines| {
                insert_at_anchor(sym, lines, &code, Anchor::Before)
            })
            .await
        }
        WriteCommand::InsertAfter(a) => {
            let code = read_payload(&a.code)?;
            run_mutation("insert_after", a.loc, a.dry_run, app, |sym, lines| {
                insert_at_anchor(sym, lines, &code, Anchor::After)
            })
            .await
        }
    }
    .inspect_err(|e| {
        ctx.print_error(OutputError::internal(e.to_string()));
    })
    .or_else(|_| Ok(()))
}

#[derive(Copy, Clone)]
enum Anchor {
    Before,
    After,
}

async fn run_mutation<F>(
    op: &'static str,
    loc: LocationArg,
    dry_run: bool,
    app: &App,
    splice: F,
) -> Result<()>
where
    F: FnOnce(&Symbol, Vec<&str>) -> Result<(Vec<String>, Range)>,
{
    let ctx = &app.output;
    let parsed = loc.parse()?.to_absolute()?;
    let analysis = LocationAnalysis::at(app.lsp.as_ref(), parsed.clone()).await?;
    let symbol = match analysis.target {
        Some(s) => s,
        None => {
            ctx.print_error(OutputError::not_found(format!(
                "No symbol resolved at {}:{}:{}",
                parsed.file.display(),
                parsed.line,
                parsed.column,
            )));
            return Ok(());
        }
    };

    let original = std::fs::read_to_string(&parsed.file)?;
    let original_lines: Vec<&str> = original.lines().collect();

    let (new_lines, target_range) = splice(&symbol, original_lines)?;
    let new_content = join_lines(&new_lines, &original);
    let bytes_changed = new_content.len() as i64 - original.len() as i64;

    let preview = if dry_run {
        Some(unified_preview(&original, &new_content, &parsed.file))
    } else {
        std::fs::write(&parsed.file, &new_content)?;
        None
    };

    ctx.print_success(WriteResult {
        operation: op,
        file: ctx.relative_path(&parsed.file),
        target_symbol: symbol.path().to_string(),
        target_kind: symbol.kind.to_string(),
        target_lines: target_range,
        bytes_changed,
        dry_run,
        preview,
    });
    Ok(())
}

fn read_payload(value: &str) -> Result<String> {
    if value == "-" {
        let mut buf = String::new();
        std::io::stdin().read_to_string(&mut buf)?;
        Ok(buf)
    } else {
        Ok(value.to_string())
    }
}

fn replace_body_lines(
    symbol: &Symbol,
    lines: Vec<&str>,
    new_body: &str,
) -> Result<(Vec<String>, Range)> {
    let (start, end) = symbol_range(&symbol.location, lines.len())?;
    let head_end = start.saturating_sub(1) as usize;
    let mut out: Vec<String> = lines[..head_end].iter().map(|s| s.to_string()).collect();
    out.extend(new_body.lines().map(|l| l.to_string()));
    let tail_start = end as usize;
    if tail_start < lines.len() {
        out.extend(lines[tail_start..].iter().map(|s| s.to_string()));
    }
    Ok((out, Range { start, end }))
}

fn insert_at_anchor(
    symbol: &Symbol,
    lines: Vec<&str>,
    code: &str,
    anchor: Anchor,
) -> Result<(Vec<String>, Range)> {
    let (start, end) = symbol_range(&symbol.location, lines.len())?;
    let pivot = match anchor {
        Anchor::Before => start.saturating_sub(1) as usize,
        Anchor::After => end as usize,
    };

    let mut out: Vec<String> = lines[..pivot].iter().map(|s| s.to_string()).collect();
    out.extend(code.lines().map(|l| l.to_string()));
    out.extend(lines[pivot..].iter().map(|s| s.to_string()));
    Ok((out, Range { start, end }))
}

fn symbol_range(loc: &Location, total_lines: usize) -> Result<(u32, u32)> {
    let start = loc
        .range_start_line
        .or(Some(loc.line))
        .unwrap_or(loc.line)
        .max(1);
    let end = loc.end_line.unwrap_or(loc.line);
    if (end as usize) > total_lines.max(1) {
        anyhow::bail!(
            "Symbol end line {end} exceeds file length {total_lines}; LSP range is stale, retry"
        );
    }
    Ok((start, end))
}

fn join_lines(lines: &[String], original: &str) -> String {
    let mut out = lines.join("\n");
    // Preserve a trailing newline if the original had one (POSIX text-file
    // convention). Avoids spurious whitespace diffs in tools like `git diff`.
    if original.ends_with('\n') && !out.ends_with('\n') {
        out.push('\n');
    }
    out
}

fn unified_preview(before: &str, after: &str, file: &Path) -> String {
    let before_lines: Vec<&str> = before.lines().collect();
    let after_lines: Vec<&str> = after.lines().collect();
    let mut out = String::new();
    out.push_str(&format!("--- {} (before)\n", file.display()));
    out.push_str(&format!("+++ {} (after)\n", file.display()));

    // Tiny line-diff: walk both sides, emit context, +, -. Not a real
    // Myers diff — good enough for human + LLM review of a single splice.
    let len = before_lines.len().max(after_lines.len());
    for i in 0..len {
        match (before_lines.get(i), after_lines.get(i)) {
            (Some(b), Some(a)) if b == a => {} // unchanged
            (Some(b), Some(a)) => {
                out.push_str(&format!("- {b}\n"));
                out.push_str(&format!("+ {a}\n"));
            }
            (Some(b), None) => out.push_str(&format!("- {b}\n")),
            (None, Some(a)) => out.push_str(&format!("+ {a}\n")),
            (None, None) => break,
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::models::symbol::SymbolKind;
    use std::path::PathBuf;

    fn sample_symbol(start: u32, end: u32) -> Symbol {
        Symbol::new(
            "process".to_string(),
            SymbolKind::Function,
            Location::full(PathBuf::from("/tmp/foo.rs"), start, 1, start, 1, end, 1),
        )
    }

    #[test]
    fn replace_body_replaces_inclusive_range() {
        let lines = vec!["line1", "fn process() {", "    body", "}", "line5"];
        let sym = sample_symbol(2, 4);
        let (out, range) = replace_body_lines(&sym, lines, "fn process() { new() }").unwrap();
        assert_eq!(out, vec!["line1", "fn process() { new() }", "line5"]);
        assert_eq!(range.start, 2);
        assert_eq!(range.end, 4);
    }

    #[test]
    fn insert_before_pushes_lines_in_above_anchor() {
        let lines = vec!["line1", "fn process() {", "}", "line4"];
        let sym = sample_symbol(2, 3);
        let (out, _) = insert_at_anchor(&sym, lines, "// hello", Anchor::Before).unwrap();
        assert_eq!(
            out,
            vec!["line1", "// hello", "fn process() {", "}", "line4"]
        );
    }

    #[test]
    fn insert_after_appends_lines_below_anchor() {
        let lines = vec!["line1", "fn process() {", "}", "line4"];
        let sym = sample_symbol(2, 3);
        let (out, _) = insert_at_anchor(&sym, lines, "// trailer", Anchor::After).unwrap();
        assert_eq!(
            out,
            vec!["line1", "fn process() {", "}", "// trailer", "line4"]
        );
    }

    #[test]
    fn symbol_range_rejects_stale_lsp_data() {
        let loc = Location::full(PathBuf::from("/tmp/x.rs"), 1, 1, 1, 1, 100, 1);
        let err = symbol_range(&loc, 5).unwrap_err();
        assert!(err.to_string().contains("exceeds file length"));
    }

    #[test]
    fn join_lines_preserves_trailing_newline() {
        let lines = vec!["a".to_string(), "b".to_string()];
        let result = join_lines(&lines, "a\nb\n");
        assert!(result.ends_with('\n'));
    }
}
