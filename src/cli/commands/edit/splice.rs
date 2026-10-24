//! The splice core: line and character changes planned against a file's
//! live content, and the document they are applied to.

use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};

use crate::app::App;
use crate::cli::response::{EditOutput, LineRange};
use crate::utils::char_to_byte_index;

use super::stale_revision;
use super::write::{validate_file_for_edit, write_file};

/// One planned, contiguous content change in line units: `removed`
/// original lines starting at `at` are replaced by `new_lines`. Inserts
/// are the `removed == 0` case. Symbol and explicit character-range edits
/// reduce to this form.
#[derive(Debug)]
pub(super) struct LineSplice {
    /// 0-indexed first affected line.
    pub(super) at: usize,
    /// Number of original lines replaced.
    pub(super) removed: usize,
    pub(super) new_lines: Vec<String>,
}

impl LineSplice {
    fn apply(&self, lines: &[String]) -> Vec<String> {
        let mut out = Vec::with_capacity(lines.len() - self.removed + self.new_lines.len());
        out.extend_from_slice(&lines[..self.at]);
        out.extend(self.new_lines.iter().cloned());
        out.extend_from_slice(&lines[self.at + self.removed..]);
        out
    }

    /// Exact unified hunk derived from the splice itself — no diff walk,
    /// so an insert can never smear change markers over untouched lines.
    fn unified_hunk(&self, lines: &[String]) -> String {
        let mut out = format!(
            "@@ -{},{} +{},{} @@\n",
            self.at + 1,
            self.removed,
            self.at + 1,
            self.new_lines.len(),
        );
        for line in &lines[self.at..self.at + self.removed] {
            out.push('-');
            out.push_str(line);
            out.push('\n');
        }
        for line in &self.new_lines {
            out.push('+');
            out.push_str(line);
            out.push('\n');
        }
        out
    }
}

/// A file loaded for editing: validated up front, split into lines once,
/// rendered back with its original line-ending style and trailing-newline
/// behaviour preserved.
pub(super) struct FileDocument {
    pub(super) file: PathBuf,
    pub(super) content: String,
    pub(super) lines: Vec<String>,
    pub(super) eol: &'static str,
}

impl FileDocument {
    /// Existence and the size cap are always enforced; writability only
    /// when the edit will actually hit disk.
    pub(super) fn load(file: &Path, dry_run: bool) -> Result<Self> {
        validate_file_for_edit(file, !dry_run)?;
        let content = fs::read_to_string(file)
            .with_context(|| format!("Failed to read file: {}", file.display()))?;
        let lines: Vec<String> = content.lines().map(String::from).collect();
        let eol = detect_line_ending(&content);
        Ok(Self {
            file: file.to_path_buf(),
            content,
            lines,
            eol,
        })
    }

    fn render(&self, lines: &[String]) -> String {
        let mut out = lines.join(self.eol);
        if self.content.ends_with('\n') && !out.is_empty() {
            out.push_str(self.eol);
        }
        out
    }

    /// Apply one splice: write (or preview) and build the typed output.
    pub(super) fn commit(
        &self,
        app: &App,
        operation: &'static str,
        splice: LineSplice,
        span: LineRange,
        target: Option<(String, String)>,
        dry_run: bool,
    ) -> Result<EditOutput> {
        if splice.at + splice.removed > self.lines.len() {
            anyhow::bail!(crate::cli::CliInputError::LineOutOfRange {
                line: (splice.at + splice.removed) as u32,
                total: self.lines.len(),
            });
        }

        let new_content = self.render(&splice.apply(&self.lines));
        let bytes_changed = new_content.len() as i64 - self.content.len() as i64;

        let preview = if dry_run {
            Some(splice.unified_hunk(&self.lines))
        } else {
            write_file(&self.file, &new_content, Some(&self.content))?;
            None
        };

        let (target_symbol, target_kind) = match target {
            Some((s, k)) => (Some(s), Some(k)),
            None => (None, None),
        };

        Ok(EditOutput {
            operation,
            file: app.output.relative_path(&self.file),
            target_symbol,
            target_kind,
            lines: span,
            bytes_changed,
            dry_run,
            preview,
            dangling_references: None,
            references_status: None,
            diagnostics: None,
            caller_verification: None,
        })
    }
}

/// Reduce a 1-indexed character range to a `LineSplice`. `end_col` is
/// exclusive; `None` means "through the end of `end_line`".
///
/// Positions are validated, never clamped: a column past the line end is
/// the caller addressing content that isn't there, and silently snapping
/// it to EOL would splice somewhere the caller didn't ask for.
pub(super) fn char_splice(
    lines: &[String],
    start_line: u32,
    start_col: u32,
    end_line: u32,
    end_col: Option<u32>,
    text: &str,
    expect: Option<&str>,
) -> Result<LineSplice> {
    // `--expect` is a staleness precondition: confirm the live text first,
    // bounds-tolerantly, so geometry that no longer exists (the file shrank
    // against the revision the caller read) surfaces as a `Conflict` to
    // re-read and retry — not as an `InvalidArgument` the agent won't retry.
    if let Some(expected) = expect {
        confirm_expected_region(lines, start_line, start_col, end_line, end_col, expected)?;
    }

    // An empty file has exactly one addressable position: 1:1.
    if lines.is_empty() {
        if start_line == 1 && end_line == 1 && start_col == 1 && end_col.unwrap_or(1) == 1 {
            return Ok(LineSplice {
                at: 0,
                removed: 0,
                new_lines: split_merged_lines(text),
            });
        }
        anyhow::bail!(crate::cli::CliInputError::LineOutOfRange {
            line: start_line,
            total: 0,
        });
    }

    let start_idx = (start_line.saturating_sub(1)) as usize;
    let end_idx = (end_line.saturating_sub(1)) as usize;
    for (label, idx) in [(start_line, start_idx), (end_line, end_idx)] {
        if idx >= lines.len() {
            anyhow::bail!(crate::cli::CliInputError::LineOutOfRange {
                line: label,
                total: lines.len(),
            });
        }
    }
    if end_idx < start_idx {
        anyhow::bail!(invalid_range(format!(
            "End line {end_line} precedes start line {start_line}"
        )));
    }

    let start_line_str = &lines[start_idx];
    ensure_column_in_line(start_col, start_line_str, start_line)?;
    let start_byte = char_to_byte_index(start_line_str, (start_col.saturating_sub(1)) as usize);
    let prefix = &start_line_str[..start_byte];

    let end_line_str = &lines[end_idx];
    let end_chars = match end_col {
        Some(col) => {
            ensure_column_in_line(col, end_line_str, end_line)?;
            if start_idx == end_idx && col < start_col {
                anyhow::bail!(invalid_range(format!(
                    "End column {col} precedes start column {start_col} on line {start_line}"
                )));
            }
            (col.saturating_sub(1)) as usize
        }
        None => end_line_str.chars().count(),
    };
    let end_byte = char_to_byte_index(end_line_str, end_chars);
    let suffix = &end_line_str[end_byte..];

    Ok(LineSplice {
        at: start_idx,
        removed: end_idx - start_idx + 1,
        new_lines: split_merged_lines(&format!("{prefix}{text}{suffix}")),
    })
}

/// Reconstruct the live text a character range covers, exactly as it sits
/// on disk. `FileDocument` stores lines terminator-stripped, so multi-line
/// spans join with `\n` — the canonical form `--expect` is normalized to.
fn spliced_region(
    lines: &[String],
    start_idx: usize,
    start_byte: usize,
    end_idx: usize,
    end_byte: usize,
) -> String {
    if start_idx == end_idx {
        return lines[start_idx][start_byte..end_byte].to_string();
    }
    let mut region = String::new();
    region.push_str(&lines[start_idx][start_byte..]);
    for line in &lines[start_idx + 1..end_idx] {
        region.push('\n');
        region.push_str(line);
    }
    region.push('\n');
    region.push_str(&lines[end_idx][..end_byte]);
    region
}

/// Bounds-tolerant region read: the live text the range covers, or `None`
/// when the range no longer fits the file (a line/column out of range, or an
/// inverted range). Used only by the `--expect` precondition, where being
/// unable to address the region means the file changed under the caller.
fn live_region(
    lines: &[String],
    start_line: u32,
    start_col: u32,
    end_line: u32,
    end_col: Option<u32>,
) -> Option<String> {
    if lines.is_empty() {
        // The only addressable region of an empty file is 1:1, spanning "".
        let origin =
            start_line == 1 && end_line == 1 && start_col == 1 && end_col.unwrap_or(1) == 1;
        return origin.then(String::new);
    }
    let start_idx = start_line.saturating_sub(1) as usize;
    let end_idx = end_line.saturating_sub(1) as usize;
    if start_idx >= lines.len() || end_idx >= lines.len() || end_idx < start_idx {
        return None;
    }
    let start_line_str = &lines[start_idx];
    let end_line_str = &lines[end_idx];
    let start_chars = start_col.saturating_sub(1) as usize;
    if start_chars > start_line_str.chars().count() {
        return None;
    }
    let end_chars = match end_col {
        Some(col) => {
            let chars = col.saturating_sub(1) as usize;
            if chars > end_line_str.chars().count() || (start_idx == end_idx && col < start_col) {
                return None;
            }
            chars
        }
        None => end_line_str.chars().count(),
    };
    let start_byte = char_to_byte_index(start_line_str, start_chars);
    let end_byte = char_to_byte_index(end_line_str, end_chars);
    Some(spliced_region(
        lines, start_idx, start_byte, end_idx, end_byte,
    ))
}

/// Confirm the live text spanned by the range equals `expected`, treating
/// *any* inability to read exactly that region — out-of-range geometry from a
/// file that shrank, as well as a content mismatch — as a stale-revision
/// `Conflict`. The comparison is exact, tolerating only `\r\n` vs `\n` (line
/// terminators carry no meaning in the splice model — lines are stored
/// stripped, so a CRLF file and an LF file address the same text); indentation
/// and every other byte must match, so a normalized compare can never
/// silently accept a different edit.
fn confirm_expected_region(
    lines: &[String],
    start_line: u32,
    start_col: u32,
    end_line: u32,
    end_col: Option<u32>,
    expected: &str,
) -> Result<()> {
    let want = normalize_newlines(expected);

    let matched = match live_region(lines, start_line, start_col, end_line, end_col) {
        Some(actual) => want == actual,
        None => false,
    };
    if matched {
        return Ok(());
    }

    let end = match end_col {
        Some(col) => col.to_string(),
        None => "eol".to_string(),
    };
    anyhow::bail!(stale_revision(format!(
        "Live text at {start_line}:{start_col}..{end_line}:{end} does not match --expect; \
         the file changed against a different revision — retry"
    )))
}

fn normalize_newlines(text: &str) -> String {
    text.split('\n')
        .map(|segment| segment.strip_suffix('\r').unwrap_or(segment))
        .collect::<Vec<_>>()
        .join("\n")
}

pub(super) fn invalid_range(message: String) -> anyhow::Error {
    anyhow::Error::new(crate::cli::OutputError::invalid(message))
}

/// Valid columns run 1..=chars+1 — one past the last character is the
/// zero-width position at EOL.
fn ensure_column_in_line(col: u32, line: &str, line_no: u32) -> Result<()> {
    let chars = line.chars().count();
    if (col as usize) > chars + 1 {
        anyhow::bail!(invalid_range(format!(
            "Column {col} exceeds line {line_no} length ({chars} characters)"
        )));
    }
    Ok(())
}

/// Split merged splice content into lines, normalizing CRLF in the
/// replacement text — `render` re-joins with the file's own line
/// ending, so a carried `\r` would corrupt CRLF files into `\r\r\n`.
fn split_merged_lines(text: &str) -> Vec<String> {
    text.split('\n')
        .map(|l| l.strip_suffix('\r').unwrap_or(l).to_string())
        .collect()
}

pub(super) fn to_lines(text: &str) -> Vec<String> {
    text.lines().map(String::from).collect()
}

/// Detect the line ending style used in content
fn detect_line_ending(content: &str) -> &'static str {
    if content.contains("\r\n") {
        "\r\n"
    } else {
        "\n"
    }
}

#[cfg(test)]
mod tests {
    use super::super::test_support::lines;
    use super::*;

    #[test]
    fn replace_body_splice_replaces_inclusive_span() {
        let original = lines(&["line1", "fn process() {", "    body", "}", "line5"]);
        let splice = LineSplice {
            at: 1,
            removed: 3,
            new_lines: lines(&["fn process() { new() }"]),
        };
        assert_eq!(
            splice.apply(&original),
            lines(&["line1", "fn process() { new() }", "line5"])
        );
    }

    #[test]
    fn insert_splices_do_not_remove_lines() {
        let original = lines(&["line1", "fn process() {", "}", "line4"]);
        let before = LineSplice {
            at: 1,
            removed: 0,
            new_lines: lines(&["// hello"]),
        };
        assert_eq!(
            before.apply(&original),
            lines(&["line1", "// hello", "fn process() {", "}", "line4"])
        );
        let after = LineSplice {
            at: 3,
            removed: 0,
            new_lines: lines(&["// trailer"]),
        };
        assert_eq!(
            after.apply(&original),
            lines(&["line1", "fn process() {", "}", "// trailer", "line4"])
        );
    }

    /// The hunk is derived from the splice, so an insert shows exactly
    /// the inserted lines — the positional-diff failure mode (every line
    /// after the insert marked changed) cannot occur.
    #[test]
    fn insert_hunk_contains_only_the_inserted_lines() {
        let original = lines(&["a", "b", "c", "d", "e"]);
        let splice = LineSplice {
            at: 2,
            removed: 0,
            new_lines: lines(&["x"]),
        };
        assert_eq!(splice.unified_hunk(&original), "@@ -3,0 +3,1 @@\n+x\n");
    }

    #[test]
    fn replace_hunk_lists_removed_then_added() {
        let original = lines(&["a", "old1", "old2", "d"]);
        let splice = LineSplice {
            at: 1,
            removed: 2,
            new_lines: lines(&["new"]),
        };
        assert_eq!(
            splice.unified_hunk(&original),
            "@@ -2,2 +2,1 @@\n-old1\n-old2\n+new\n"
        );
    }

    #[test]
    fn char_splice_is_character_indexed_not_byte_indexed() {
        // '한' and '글' are 3 bytes each; column 3 must mean the third
        // *character*.
        let original = lines(&["한글ab"]);
        let splice = char_splice(&original, 1, 3, 1, Some(4), "X", None).unwrap();
        assert_eq!(splice.new_lines, lines(&["한글Xb"]));
    }

    #[test]
    fn char_splice_without_end_col_replaces_through_eol() {
        let original = lines(&["keep: drop", "next"]);
        let splice = char_splice(&original, 1, 7, 1, None, "kept", None).unwrap();
        assert_eq!(splice.new_lines, lines(&["keep: kept"]));
        assert_eq!(splice.removed, 1);
    }

    #[test]
    fn char_splice_spanning_lines_merges_prefix_and_suffix() {
        let original = lines(&["start AAA", "BBB end"]);
        let splice = char_splice(&original, 1, 7, 2, Some(4), "X", None).unwrap();
        assert_eq!(splice.new_lines, lines(&["start X end"]));
        assert_eq!(splice.removed, 2);
    }

    /// Positions are validated, never clamped — a column past EOL is the
    /// caller addressing content that isn't there.
    #[test]
    fn char_splice_rejects_out_of_range_columns() {
        let original = lines(&["short"]);
        // chars+1 (= 6) is the valid zero-width EOL position; 7 is not.
        assert!(char_splice(&original, 1, 6, 1, None, "x", None).is_ok());
        let err = char_splice(&original, 1, 7, 1, None, "x", None).unwrap_err();
        assert!(err.to_string().contains("exceeds line 1 length"));
        let err = char_splice(&original, 1, 1, 1, Some(7), "x", None).unwrap_err();
        assert!(err.to_string().contains("exceeds line 1 length"));
    }

    /// An inverted same-line range would slice overlapping prefix and
    /// suffix and duplicate the text between them.
    #[test]
    fn char_splice_rejects_inverted_same_line_range() {
        let original = lines(&["abcdef"]);
        let err = char_splice(&original, 1, 4, 1, Some(2), "x", None).unwrap_err();
        assert!(err.to_string().contains("precedes start column"));
        // Equal start/end is a valid zero-width insert.
        assert!(char_splice(&original, 1, 4, 1, Some(4), "x", None).is_ok());
    }

    /// CRLF replacement text must not leak `\r` into the line array —
    /// `render` re-joins with the file's own ending.
    #[test]
    fn char_splice_normalizes_crlf_replacement_text() {
        let original = lines(&["old"]);
        let splice = char_splice(&original, 1, 1, 1, None, "a\r\nb", None).unwrap();
        assert_eq!(splice.new_lines, lines(&["a", "b"]));
    }

    /// An empty file has exactly one addressable position: 1:1.
    #[test]
    fn char_splice_accepts_insert_into_empty_file_at_origin() {
        let empty: Vec<String> = vec![];
        let splice = char_splice(&empty, 1, 1, 1, None, "hello\nworld", None).unwrap();
        assert_eq!(splice.at, 0);
        assert_eq!(splice.removed, 0);
        assert_eq!(splice.new_lines, lines(&["hello", "world"]));
        assert!(char_splice(&empty, 2, 1, 2, None, "x", None).is_err());
    }

    /// `--expect` matching the live text lets the splice proceed; a single-
    /// line range compares the spanned characters exactly.
    #[test]
    fn char_splice_expect_matches_live_text() {
        let original = lines(&["let x = 1;"]);
        // cols 5..6 (1-indexed, end exclusive) span "x".
        let splice = char_splice(&original, 1, 5, 1, Some(6), "y", Some("x")).unwrap();
        assert_eq!(splice.new_lines, lines(&["let y = 1;"]));
    }

    /// A mismatched `--expect` aborts as a `Conflict` — the branchable
    /// "re-read and retry" signal, never an internal error.
    #[test]
    fn char_splice_expect_mismatch_is_a_conflict() {
        use crate::cli::{ErrorCode, OutputError};
        let original = lines(&["let x = 1;"]);
        let err = char_splice(&original, 1, 5, 1, Some(6), "y", Some("z")).unwrap_err();
        let out: OutputError = err.into();
        assert!(matches!(out.code, ErrorCode::Conflict));
        assert!(out.message.contains("does not match --expect"));
    }

    /// `--expect` spanning lines reconstructs the region as `\n`-joined and
    /// treats a CRLF expectation as equal to the LF-stored text.
    #[test]
    fn char_splice_expect_tolerates_crlf_across_lines() {
        let original = lines(&["a", "b"]);
        // 1:1..2:2 spans "a\nb".
        assert!(char_splice(&original, 1, 1, 2, Some(2), "x", Some("a\r\nb")).is_ok());
        // Indentation differences are NOT tolerated.
        let indented = lines(&["  a", "b"]);
        assert!(char_splice(&indented, 1, 1, 2, Some(2), "x", Some("a\nb")).is_err());
    }

    /// On an empty file the only expectation the origin can satisfy is the
    /// empty string; anything else is a conflict.
    #[test]
    fn char_splice_expect_on_empty_file() {
        let empty: Vec<String> = vec![];
        assert!(char_splice(&empty, 1, 1, 1, None, "x", Some("")).is_ok());
        assert!(char_splice(&empty, 1, 1, 1, None, "x", Some("y")).is_err());
    }

    /// Geometry drift with `--expect` (the file shrank so the range no longer
    /// fits) is a `Conflict` to re-read and retry — NOT an `InvalidArgument`,
    /// which an agent would treat as a bad request and not retry. Without
    /// `--expect`, the same out-of-range column stays `InvalidArgument`.
    #[test]
    fn char_splice_expect_out_of_range_is_a_conflict() {
        use crate::cli::{ErrorCode, OutputError};
        let original = lines(&["short"]); // 5 chars; col 20 no longer exists
        let err = char_splice(&original, 1, 1, 1, Some(20), "x", Some("anything")).unwrap_err();
        let out: OutputError = err.into();
        assert!(
            matches!(out.code, ErrorCode::Conflict),
            "out-of-range with --expect must be a Conflict"
        );

        // The same geometry without --expect is a plain invalid-argument.
        let err = char_splice(&original, 1, 1, 1, Some(20), "x", None).unwrap_err();
        let out: OutputError = err.into();
        assert!(matches!(out.code, ErrorCode::InvalidArgument));
    }
}
