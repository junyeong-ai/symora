//! AST pattern edits: one replacement per tree-sitter match.

use anyhow::{Context, Result};

use crate::app::App;
use crate::cli::response::{EditOutput, LineRange, Section};
use crate::infra::file_filter::FileFilter;
use crate::models::lsp::{Position, Range as LspRange, TextEdit};
use crate::models::symbol::Language;

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
    let language = Language::parse_or_default(lang);
    if language == Language::Unknown {
        anyhow::bail!(
            "Unsupported language: {}. Run 'symora search nodes --list' for supported languages.",
            lang
        );
    }

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
        .map_err(|e| anyhow::anyhow!("AST query failed: {}", e))?;
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
        Some(
            index
                .parse()
                .context("Invalid index: must be a number or 'all'")?,
        )
    };
    if let Some(idx) = target_index
        && !matches.is_empty()
        && idx >= matches.len()
    {
        anyhow::bail!(
            "Index {} out of range. Found {} matches.",
            idx,
            matches.len()
        );
    }

    let selected: Vec<_> = matches
        .iter()
        .enumerate()
        .filter(|(i, _)| replace_all || target_index == Some(*i))
        .collect();

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
