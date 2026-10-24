//! Applying LSP-computed workspace edits, for rename and code actions.

use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};

use crate::models::lsp::{FileChangeWithEdits, TextEdit};
use crate::utils::char_to_byte_index;

use super::stale_revision;
use super::write::{validate_file_for_edit, write_file};

/// Apply workspace edits from LSP to actual files.
/// Used by rename and actions apply commands.
///
/// Returns the number of files modified and details of changes.
pub fn apply_workspace_edits(
    changes: &[FileChangeWithEdits],
    dry_run: bool,
    root: &Path,
) -> Result<Vec<AppliedFileChange>> {
    let root = root.canonicalize().unwrap_or_else(|_| root.to_path_buf());

    // Coalesce edits by the file they ultimately resolve to. A workspace
    // edit's ranges are all expressed against the original document, so two
    // edit groups naming the same file (distinct URI spellings, or a server
    // that splits one document across entries) must apply together against
    // one read — staging them independently would let the later write clobber
    // the earlier one's changes.
    let grouped = coalesce_changes_by_file(changes)?;

    // Two phases: validate and stage every file in memory first, write
    // only after the whole edit checked out. A stale range in file N
    // must not leave files 1..N-1 already rewritten while the command
    // reports failure.
    let mut staged = Vec::with_capacity(grouped.len());

    for GroupedEdits {
        file,
        edits,
        expected_hash,
    } in grouped
    {
        // The paths come from the language server, not the user — they
        // still don't get to write outside the project.
        let canonical = file
            .canonicalize()
            .with_context(|| format!("Cannot resolve path: {}", file.display()))?;
        if !canonical.starts_with(&root) {
            anyhow::bail!(crate::cli::CliInputError::PathOutsideProject(canonical));
        }

        validate_file_for_edit(&file, !dry_run)?;

        let content = fs::read_to_string(&file)
            .with_context(|| format!("Failed to read file: {}", file.display()))?;

        if expected_hash.is_some_and(|expected| crate::infra::hash_content(&content) != expected) {
            anyhow::bail!(stale_revision(format!(
                "{} changed since the language server edit was read",
                file.display()
            )));
        }
        let new_content = apply_text_edits(&content, &edits)
            .with_context(|| format!("Invalid edit for {}", file.display()))?;

        staged.push((file, content, new_content, edits.len()));
    }

    let mut results: Vec<AppliedFileChange> = Vec::with_capacity(staged.len());
    for (file, content, new_content, edit_count) in staged {
        if !dry_run && let Err(error) = write_file(&file, &new_content, Some(&content)) {
            if results.is_empty() {
                return Err(error);
            }
            let applied = results
                .iter()
                .map(|change| {
                    change
                        .file
                        .strip_prefix(&root)
                        .unwrap_or(&change.file)
                        .display()
                        .to_string()
                })
                .collect::<Vec<_>>()
                .join(", ");
            let error = crate::cli::OutputError::from(error).with_hint(format!("Already modified: {applied}. Inspect these files before retrying the remaining edit."));
            return Err(error.into());
        }

        results.push(AppliedFileChange {
            file,
            edit_count,
            applied: !dry_run,
        });
    }

    Ok(results)
}

/// Group workspace-edit entries by the file they resolve to, concatenating
/// the edits of any that target the same file while preserving first-seen
/// order. The key is the canonicalized path so distinct spellings of one
/// existing file coalesce; a path that can't be canonicalized (it doesn't
/// exist yet) falls back to its literal spelling, which still groups an
/// entry with itself.
struct GroupedEdits {
    file: PathBuf,
    edits: Vec<TextEdit>,
    expected_hash: Option<u64>,
}

fn coalesce_changes_by_file(changes: &[FileChangeWithEdits]) -> Result<Vec<GroupedEdits>> {
    let mut order = Vec::new();
    let mut groups: std::collections::HashMap<PathBuf, GroupedEdits> =
        std::collections::HashMap::new();
    for change in changes {
        let key = change
            .file
            .canonicalize()
            .unwrap_or_else(|_| change.file.clone());
        match groups.get_mut(&key) {
            Some(group) => {
                if group
                    .expected_hash
                    .zip(change.expected_hash)
                    .is_some_and(|(left, right)| left != right)
                {
                    anyhow::bail!(stale_revision(format!(
                        "Edits for {} refer to different file revisions",
                        change.file.display()
                    )));
                }
                group.expected_hash = group.expected_hash.or(change.expected_hash);
                group.edits.extend(change.edits.iter().cloned());
            }
            None => {
                order.push(key.clone());
                groups.insert(
                    key,
                    GroupedEdits {
                        file: change.file.clone(),
                        edits: change.edits.clone(),
                        expected_hash: change.expected_hash,
                    },
                );
            }
        }
    }
    Ok(order
        .into_iter()
        .filter_map(|key| groups.remove(&key))
        .collect())
}

/// Result of applying edits to a single file
#[derive(Debug, Clone)]
pub struct AppliedFileChange {
    pub file: PathBuf,
    pub edit_count: usize,
    pub applied: bool,
}

/// Apply text edits to `content`. Every range is resolved once against the
/// original document — a stale (past-EOF) or inverted range aborts the whole
/// application rather than letting a partial result report success.
///
/// The result is rebuilt left-to-right from the original content: edits are
/// taken in ascending start order and each contributes `content[prev..start]`
/// followed by its replacement text. Because nothing is ever applied against
/// bytes an earlier edit already moved, the original offsets stay valid for
/// every edit — including a zero-width insert that shares a boundary with an
/// adjacent replace. An edit that starts before the previous one ended is a
/// genuine overlap and is refused rather than silently misapplied. Several
/// inserts at one position keep the order they were given.
pub(crate) fn apply_text_edits(content: &str, edits: &[TextEdit]) -> Result<String> {
    let resolved = resolve_text_edits(content, edits)?;
    Ok(apply_resolved_edits(content, &resolved))
}

pub(super) fn resolve_text_edits<'a>(
    content: &str,
    edits: &'a [TextEdit],
) -> Result<Vec<ResolvedEdit<'a>>> {
    // Resolve to original-document byte offsets up front. `order` keeps
    // same-position inserts in the order they were given.
    let mut resolved: Vec<ResolvedEdit> = Vec::with_capacity(edits.len());
    for (order, edit) in edits.iter().enumerate() {
        let start = line_char_to_byte_offset(
            content,
            edit.range.start.line as usize,
            edit.range.start.character as usize,
        )?;
        let end = line_char_to_byte_offset(
            content,
            edit.range.end.line as usize,
            edit.range.end.character as usize,
        )?;
        if start > end {
            anyhow::bail!(stale_revision(format!(
                "LSP edit range {:?} is inverted; the edit was computed \
                 against a different revision — retry",
                edit.range,
            )));
        }
        resolved.push(ResolvedEdit {
            start,
            end,
            order,
            text: &edit.new_text,
        });
    }

    // Ascending by start, then end (a zero-width insert sorts before a
    // same-start replace), then given order (same-position inserts keep
    // their order). One ordering drives both overlap checking and assembly.
    resolved.sort_by(|a, b| {
        a.start
            .cmp(&b.start)
            .then(a.end.cmp(&b.end))
            .then(a.order.cmp(&b.order))
    });

    let mut pos = 0usize;
    for e in &resolved {
        if e.start < pos {
            anyhow::bail!(stale_revision(format!(
                "overlapping edit ranges near byte {}; select non-overlapping edits",
                e.start,
            )));
        }
        pos = e.end;
    }
    Ok(resolved)
}

pub(super) fn apply_resolved_edits(content: &str, edits: &[ResolvedEdit<'_>]) -> String {
    let mut result = String::with_capacity(content.len());
    let mut pos = 0usize;
    for edit in edits {
        result.push_str(&content[pos..edit.start]);
        result.push_str(edit.text);
        pos = edit.end;
    }
    result.push_str(&content[pos..]);
    result
}

/// One text edit resolved to original-document byte offsets. `order` retains
/// the input identity and the order of same-position inserts.
pub(super) struct ResolvedEdit<'a> {
    pub(super) start: usize,
    pub(super) end: usize,
    pub(super) order: usize,
    pub(super) text: &'a str,
}

pub(super) fn text_edit_hunk(content: &str, edit: &ResolvedEdit<'_>, start_line: u32) -> String {
    let line_start = content[..edit.start].rfind('\n').map_or(0, |at| at + 1);
    let ends_at_line_start = edit.end == 0 || content.as_bytes()[edit.end - 1] == b'\n';
    let preserves_line_boundary =
        edit.text.ends_with('\n') || (edit.start == line_start && edit.text.is_empty());
    let line_end = if ends_at_line_start && preserves_line_boundary {
        edit.end
    } else {
        content[edit.end..]
            .find('\n')
            .map_or(content.len(), |at| edit.end + at + 1)
    };
    let before = &content[line_start..line_end];
    let after = format!(
        "{}{}{}",
        &content[line_start..edit.start],
        edit.text,
        &content[edit.end..line_end],
    );
    let removed = before.split_inclusive('\n').count();
    let added = after.split_inclusive('\n').count();
    let old_line = if removed == 0 {
        start_line - 1
    } else {
        start_line
    };
    let new_line = if added == 0 {
        start_line - 1
    } else {
        start_line
    };
    let mut hunk = format!("@@ -{old_line},{removed} +{new_line},{added} @@\n");
    for (prefix, fragment) in [('-', before), ('+', after.as_str())] {
        for line in fragment.split_inclusive('\n') {
            hunk.push(prefix);
            hunk.push_str(line);
            if !line.ends_with('\n') {
                hunk.push_str("\n\\ No newline at end of file\n");
            }
        }
    }
    hunk
}

/// Byte offset for an LSP (0-indexed) line/character position.
///
/// A character past the end of its line clamps to the line end — that is
/// the LSP-specified meaning of such positions, not a guess. A line past
/// the end of the document has no specified meaning: it means the server
/// computed the edit against a newer revision than the one on disk, and
/// silently clamping it to EOF would apply the edit somewhere the server
/// never asked for.
fn line_char_to_byte_offset(content: &str, line: usize, character: usize) -> Result<usize> {
    let mut byte_offset = 0;
    let mut lines_seen = 0;

    for line_content in content.lines() {
        if lines_seen == line {
            return Ok(byte_offset + char_to_byte_index(line_content, character));
        }
        byte_offset += line_content.len();
        // Account for actual line ending bytes (\r\n = 2, \n = 1)
        let remaining = &content.as_bytes()[byte_offset..];
        if remaining.starts_with(b"\r\n") {
            byte_offset += 2;
        } else if remaining.starts_with(b"\n") {
            byte_offset += 1;
        }
        lines_seen += 1;
    }

    // Exactly one line past the last content line is the canonical LSP
    // end-of-document position (an append point), whether or not the file
    // ends in a newline. A line further past that is a different revision —
    // clamping it would land the edit somewhere the server never asked for.
    if line == lines_seen {
        return Ok(content.len());
    }

    anyhow::bail!(stale_revision(format!(
        "LSP edit range line {} exceeds the document's {} lines; the edit \
         was computed against a different revision — retry",
        line + 1,
        lines_seen,
    )))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::models::lsp::{Position, Range as LspRange};

    /// Workspace-edit ranges are validated, never clamped: the canonical
    /// end-of-document append position works, anything past it is stale.
    #[test]
    fn lsp_offsets_validate_instead_of_clamping() {
        // Char past line end clamps per the LSP spec.
        assert_eq!(line_char_to_byte_offset("ab\ncd\n", 0, 99).unwrap(), 2);
        // Canonical end-of-document position (after the final newline).
        assert_eq!(line_char_to_byte_offset("ab\ncd\n", 2, 0).unwrap(), 6);
        assert_eq!(line_char_to_byte_offset("", 0, 0).unwrap(), 0);
        // One line past the last content line is the EOF append point even
        // when the file has no trailing newline.
        assert_eq!(line_char_to_byte_offset("ab\ncd", 2, 0).unwrap(), 5);
        // Two or more lines past EOF is a stale revision, not an append.
        assert!(line_char_to_byte_offset("ab\ncd\n", 3, 0).is_err());
        assert!(line_char_to_byte_offset("ab\ncd", 3, 0).is_err());
    }

    #[test]
    fn stale_text_edit_aborts_instead_of_partially_applying() {
        use crate::models::lsp::{Position, Range as LspRange, TextEdit};
        let good = TextEdit {
            range: LspRange::new(Position::new(0, 0), Position::new(0, 2)),
            new_text: "xy".to_string(),
        };
        let stale = TextEdit {
            range: LspRange::new(Position::new(9, 0), Position::new(9, 1)),
            new_text: "z".to_string(),
        };
        assert_eq!(
            apply_text_edits("ab\ncd\n", std::slice::from_ref(&good)).unwrap(),
            "xy\ncd\n"
        );
        let err = apply_text_edits("ab\ncd\n", &[good, stale]).unwrap_err();
        assert!(err.to_string().contains("different revision"));
    }

    #[test]
    fn overlapping_edits_are_rejected_not_misapplied() {
        use crate::models::lsp::{Position, Range as LspRange, TextEdit};
        // Two edits whose ranges intersect in the original document can't both
        // be honored — applying one corrupts the other's coordinates.
        let a = TextEdit {
            range: LspRange::new(Position::new(0, 0), Position::new(0, 4)),
            new_text: "X".to_string(),
        };
        let b = TextEdit {
            range: LspRange::new(Position::new(0, 2), Position::new(0, 6)),
            new_text: "Y".to_string(),
        };
        let err = apply_text_edits("abcdef\n", &[a, b]).unwrap_err();
        assert!(err.to_string().contains("overlapping"));
        // The stale-revision guards surface as a branchable `Conflict`, not a
        // generic internal error — pinned here so the migration can't regress.
        let out: crate::cli::OutputError = err.into();
        assert!(matches!(out.code, crate::cli::ErrorCode::Conflict));
    }

    #[test]
    fn overlap_hidden_behind_an_interleaved_insert_is_still_rejected() {
        use crate::models::lsp::{Position, Range as LspRange, TextEdit};
        // A zero-width insert that sorts between two overlapping non-empty
        // ranges must not mask the overlap: validation scans all ranges, not
        // just adjacent ones.
        let wide = TextEdit {
            range: LspRange::new(Position::new(0, 0), Position::new(0, 10)),
            new_text: "R".to_string(),
        };
        let insert = TextEdit {
            range: LspRange::new(Position::new(0, 0), Position::new(0, 0)),
            new_text: "I".to_string(),
        };
        let inner = TextEdit {
            range: LspRange::new(Position::new(0, 5), Position::new(0, 6)),
            new_text: "X".to_string(),
        };
        let err = apply_text_edits("0123456789abc\n", &[wide, insert, inner]).unwrap_err();
        assert!(err.to_string().contains("overlapping"));
    }

    #[test]
    fn insert_sharing_a_start_with_a_replace_keeps_both() {
        use crate::models::lsp::{Position, Range as LspRange, TextEdit};
        // A zero-width insert at the same position as a replace's start must
        // land before the replacement, not be clobbered by it.
        let replace = TextEdit {
            range: LspRange::new(Position::new(0, 0), Position::new(0, 2)),
            new_text: "X".to_string(),
        };
        let insert = TextEdit {
            range: LspRange::new(Position::new(0, 0), Position::new(0, 0)),
            new_text: "I".to_string(),
        };
        assert_eq!(
            apply_text_edits("abcd\n", &[replace, insert]).unwrap(),
            "IXcd\n"
        );
    }

    #[test]
    fn edit_at_eof_sentinel_applies_without_a_trailing_newline() {
        use crate::models::lsp::{Position, Range as LspRange, TextEdit};
        // A formatter appending a trailing newline addresses the position one
        // line past the last content line; that must apply, not error, on a
        // file that doesn't already end in a newline.
        let append = TextEdit {
            range: LspRange::new(Position::new(1, 1), Position::new(2, 0)),
            new_text: "\n".to_string(),
        };
        assert_eq!(apply_text_edits("a\nb", &[append]).unwrap(), "a\nb\n");
    }

    #[test]
    fn adjacent_edits_apply_without_shifting_each_other() {
        use crate::models::lsp::{Position, Range as LspRange, TextEdit};
        let first = TextEdit {
            range: LspRange::new(Position::new(0, 0), Position::new(0, 2)),
            new_text: "XX".to_string(),
        };
        let second = TextEdit {
            range: LspRange::new(Position::new(0, 2), Position::new(0, 4)),
            new_text: "YY".to_string(),
        };
        // Given in document order, not sorted — the applier must order them.
        assert_eq!(
            apply_text_edits("abcd\n", &[first, second]).unwrap(),
            "XXYY\n"
        );
    }

    #[test]
    fn same_position_inserts_keep_given_order() {
        use crate::models::lsp::{Position, Range as LspRange, TextEdit};
        let insert = |text: &str| TextEdit {
            range: LspRange::new(Position::new(0, 0), Position::new(0, 0)),
            new_text: text.to_string(),
        };
        // Two zero-width inserts at the same point must land in the order
        // they were given (A before B), not reversed.
        assert_eq!(
            apply_text_edits("z\n", &[insert("A"), insert("B")]).unwrap(),
            "ABz\n"
        );
    }

    #[test]
    fn text_edit_hunks_preserve_line_joins_and_eof_inserts() {
        for (content, range, text, start_line, expected, hunk) in [
            (
                "one\ntail\n",
                LspRange::new(Position::new(0, 0), Position::new(1, 0)),
                "joined",
                1,
                "joinedtail\n",
                "@@ -1,2 +1,1 @@\n-one\n-tail\n+joinedtail\n",
            ),
            (
                "one\ntail\n",
                LspRange::new(Position::new(0, 0), Position::new(1, 0)),
                "new\n",
                1,
                "new\ntail\n",
                "@@ -1,1 +1,1 @@\n-one\n+new\n",
            ),
            (
                "one\n",
                LspRange::new(Position::new(1, 0), Position::new(1, 0)),
                "tail",
                2,
                "one\ntail",
                "@@ -1,0 +2,1 @@\n+tail\n\\ No newline at end of file\n",
            ),
        ] {
            let edits = [TextEdit {
                range,
                new_text: text.to_string(),
            }];
            let resolved = resolve_text_edits(content, &edits).unwrap();
            assert_eq!(apply_resolved_edits(content, &resolved), expected);
            assert_eq!(text_edit_hunk(content, &resolved[0], start_line), hunk);
        }
    }

    #[test]
    fn coalesce_merges_edits_for_the_same_file_in_order() {
        use crate::models::lsp::{Position, Range as LspRange, TextEdit};
        let edit = |line: u32, text: &str| TextEdit {
            range: LspRange::new(Position::new(line, 0), Position::new(line, 0)),
            new_text: text.to_string(),
        };
        // Two entries naming the same file (as a workspace edit's
        // documentChanges may) must collapse into one group carrying both
        // edits, so a later write can't clobber the earlier one's changes.
        let changes = vec![
            FileChangeWithEdits {
                expected_hash: None,
                file: PathBuf::from("a.rs"),
                edits: vec![edit(0, "x")],
            },
            FileChangeWithEdits {
                expected_hash: None,
                file: PathBuf::from("b.rs"),
                edits: vec![edit(0, "y")],
            },
            FileChangeWithEdits {
                expected_hash: None,
                file: PathBuf::from("a.rs"),
                edits: vec![edit(1, "z")],
            },
        ];
        let grouped = coalesce_changes_by_file(&changes).unwrap();
        assert_eq!(grouped.len(), 2, "a.rs must appear once");
        assert_eq!(grouped[0].file, PathBuf::from("a.rs"));
        assert_eq!(grouped[0].edits.len(), 2, "both a.rs edits retained");
        assert_eq!(grouped[1].file, PathBuf::from("b.rs"));
        assert_eq!(grouped[1].edits.len(), 1);
    }

    #[test]
    fn a_workspace_edit_rejects_changed_source_before_writing() {
        let directory = tempfile::tempdir().unwrap();
        let file = directory.path().join("a.py");
        fs::write(&file, "changed").unwrap();
        let changes = [FileChangeWithEdits {
            file: file.clone(),
            edits: vec![],
            expected_hash: Some(crate::infra::hash_content("original")),
        }];
        let error = apply_workspace_edits(&changes, false, directory.path()).unwrap_err();
        assert_eq!(
            crate::cli::OutputError::from(error).code,
            crate::cli::errors::ErrorCode::Conflict
        );
        assert_eq!(fs::read_to_string(file).unwrap(), "changed");
    }
}
