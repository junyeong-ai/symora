//! AST pattern edits: one replacement per tree-sitter match.

use anyhow::Result;

use crate::app::App;
use crate::cli::OutputError;
use crate::cli::commands::search::{ast_query_error, parse_ast_language};
use crate::cli::response::{EditOutput, LineRange, Section};
use crate::infra::file_filter::FileFilter;
use crate::models::lsp::{Position, Range as LspRange, TextEdit};
use crate::services::ast_query::AstMatch;

use super::finish;
use super::splice::FileDocument;
use super::target::resolve_file_path;
use super::workspace_edit::{apply_resolved_edits, resolve_text_edits, text_edit_hunk};
use super::write::write_file;

/// AST pattern edit: a list operation, one `EditOutput` per affected
/// match, wrapped in the shared `Section` list shape.
pub(super) async fn pattern_edit(
    app: &App,
    file: &str,
    pattern: &str,
    lang: &str,
    text: &str,
    index: &str,
    dry_run: bool,
) -> Result<()> {
    let language = parse_ast_language(lang)?;

    let abs_path = resolve_file_path(app, file)?;
    let doc = FileDocument::load(&abs_path, dry_run)?;

    let answer = app
        .ast
        .query(
            pattern,
            language,
            &FileFilter::for_paths(app.root(), vec![abs_path.clone()]),
        )
        .await
        .map_err(|e| ast_query_error(language, e))?;
    // The one file this edit names is the whole search domain, so a path the
    // query could not read leaves nothing to say "no matches" about.
    if !answer.unread_paths.is_empty() {
        anyhow::bail!("Cannot read or parse {file} for AST replacement");
    }
    let matches = answer.matches;

    let replace_all = index.eq_ignore_ascii_case("all");
    let target_index: Option<usize> = if replace_all {
        None
    } else {
        Some(index.parse().map_err(|_| {
            OutputError::invalid(format!(
                "Invalid index '{index}': must be a number or 'all'"
            ))
        })?)
    };
    if let Some(idx) = target_index
        && !matches.is_empty()
        && idx >= matches.len()
    {
        anyhow::bail!(OutputError::invalid(format!(
            "Index {} out of range. Found {} matches.",
            idx,
            matches.len()
        )));
    }

    let selected: Vec<_> = matches
        .iter()
        .enumerate()
        .filter(|(i, _)| replace_all || target_index == Some(*i))
        .collect();
    if let Some((outer, inner)) = overlapping(&selected) {
        anyhow::bail!(
            OutputError::invalid(format!(
                "Matches {outer} and {inner} overlap; one replacement cannot be applied inside another"
            ))
            .with_hint("Select one of them with `--index N`, or narrow the pattern")
        );
    }

    if selected.is_empty() {
        let section: Section<EditOutput> =
            Section::new(vec![]).with_hints(vec![format!("No matches for pattern in {file}")]);
        app.output.print_success(section);
        return Ok(());
    }

    let replacement = text.replace("\r\n", "\n").replace('\n', doc.eol);
    let edits: Vec<_> = selected
        .iter()
        .map(|(_, m)| TextEdit {
            range: LspRange::new(
                Position::new(m.start_line - 1, m.start_column - 1),
                Position::new(m.end_line - 1, m.end_column - 1),
            ),
            new_text: replacement.clone(),
        })
        .collect();
    let resolved = resolve_text_edits(&doc.content, &edits)?;
    let new_content = apply_resolved_edits(&doc.content, &resolved);

    let mut outputs = Vec::with_capacity(resolved.len());
    for edit in &resolved {
        let (_, m) = selected[edit.order];
        outputs.push(EditOutput {
            operation: "pattern",
            file: app.output.relative_path(&abs_path),
            target_symbol: None,
            target_kind: None,
            lines: LineRange {
                start: m.start_line,
                end: m.end_line,
            },
            bytes_changed: edit.text.len() as i64 - (edit.end - edit.start) as i64,
            dry_run,
            preview: dry_run.then(|| text_edit_hunk(&doc.content, edit, m.start_line)),
            dangling_references: None,
            references_status: None,
            diagnostics: None,
            caller_verification: None,
        });
    }

    if !dry_run {
        write_file(&abs_path, &new_content, Some(&doc.content))?;
    }

    let mut section = Section::with_total(outputs, matches.len());
    if replace_all || matches.len() > 1 {
        section = section.with_hints(vec![format!(
            "{} of {} matches {}",
            selected.len(),
            matches.len(),
            if dry_run {
                "would be replaced"
            } else {
                "replaced"
            },
        )]);
    }
    app.output.print_success(section);
    finish(app, &abs_path, dry_run).await;
    Ok(())
}

/// The indices of two selected matches whose ranges overlap. A query matches a
/// node and the nodes inside it alike (`f(f(1))` holds two calls), and both
/// cannot be replaced.
fn overlapping(selected: &[(usize, &AstMatch)]) -> Option<(usize, usize)> {
    let start = |m: &AstMatch| (m.start_line, m.start_column);
    let end = |m: &AstMatch| (m.end_line, m.end_column);
    let mut ordered = selected.to_vec();
    ordered.sort_by_key(|(_, m)| (start(m), end(m)));
    let mut reach: Option<(usize, (u32, u32))> = None;
    for (index, m) in ordered {
        if let Some((held, until)) = reach
            && start(m) < until
        {
            return Some((held.min(index), held.max(index)));
        }
        if reach.is_none_or(|(_, until)| end(m) > until) {
            reach = Some((index, end(m)));
        }
    }
    None
}
