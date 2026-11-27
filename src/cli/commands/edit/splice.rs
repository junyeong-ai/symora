//! The splice core: line changes planned against a file's live content,
//! and the document they are applied to.

use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};

use crate::app::App;
use crate::cli::response::{EditOutput, LineRange};

use super::write::{validate_file_for_edit, write_file};

/// One planned, contiguous content change in line units: `removed`
/// original lines starting at `at` are replaced by `new_lines`. Inserts
/// are the `removed == 0` case. Symbol edits reduce to this form.
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
}
