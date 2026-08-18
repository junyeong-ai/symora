use std::fmt;
use std::path::PathBuf;

use serde::{Deserialize, Serialize};

/// Source code location (1-indexed).
///
/// For symbols: `line`/`column` = name position, `name_end_*` = where the
/// name span ends (the server's selection range, when it states one),
/// `range_start_*`/`end_*` = full declaration range.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct Location {
    pub file: PathBuf,
    pub line: u32,
    pub column: u32,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub name_end_line: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub name_end_column: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub range_start_line: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub range_start_column: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub end_line: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub end_column: Option<u32>,
    /// Present (and `true`) only when `column` was DEGRADED — decoded from a
    /// target line that could not be read, so it is the raw wire offset rather
    /// than a transcoded scalar and may be wrong on a multibyte line. Omitted
    /// (the common case) when the column was transcoded normally, so an agent
    /// trusts the column absolutely unless this says otherwise.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub degraded_column: Option<bool>,
}

impl Location {
    /// Single position (no range).
    pub fn point(file: PathBuf, line: u32, column: u32) -> Self {
        Self {
            file,
            line,
            column,
            name_end_line: None,
            name_end_column: None,
            range_start_line: None,
            range_start_column: None,
            end_line: None,
            end_column: None,
            degraded_column: None,
        }
    }

    /// Full symbol location with name position and declaration range.
    pub fn full(
        file: PathBuf,
        line: u32,
        column: u32,
        range_start_line: u32,
        range_start_column: u32,
        end_line: u32,
        end_column: u32,
    ) -> Self {
        Self {
            file,
            line,
            column,
            name_end_line: None,
            name_end_column: None,
            range_start_line: Some(range_start_line),
            range_start_column: Some(range_start_column),
            end_line: Some(end_line),
            end_column: Some(end_column),
            degraded_column: None,
        }
    }

    /// Record where the name span ends, so a position can be told to be on
    /// the name rather than merely inside the declaration.
    pub fn with_name_end(mut self, line: u32, column: u32) -> Self {
        self.name_end_line = Some(line);
        self.name_end_column = Some(column);
        self
    }

    /// Mark the column as degraded (a wire-offset guess) when `degraded`; a
    /// no-op otherwise, so the field stays omitted in the common case.
    pub fn with_degraded_column(mut self, degraded: bool) -> Self {
        if degraded {
            self.degraded_column = Some(true);
        }
        self
    }

    /// Effective start position (range_start if available, else name position).
    pub fn effective_start(&self) -> (u32, u32) {
        (
            self.range_start_line.unwrap_or(self.line),
            self.range_start_column.unwrap_or(self.column),
        )
    }
}

impl fmt::Display for Location {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}:{}:{}", self.file.display(), self.line, self.column)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn location_display_uses_file_line_column() {
        let loc = Location::point(PathBuf::from("/test/file.rs"), 10, 5);
        assert_eq!(loc.to_string(), "/test/file.rs:10:5");
    }

    #[test]
    fn effective_start_falls_back_to_name_position() {
        let loc = Location::point(PathBuf::from("a.rs"), 5, 8);
        assert_eq!(loc.effective_start(), (5, 8));
    }

    #[test]
    fn effective_start_prefers_range_start_when_set() {
        let loc = Location::full(PathBuf::from("a.rs"), 10, 4, 8, 1, 12, 1);
        assert_eq!(loc.effective_start(), (8, 1));
    }
}
