use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};

use super::input_error::CliInputError;

#[derive(clap::Args, Debug)]
pub struct LocationArg {
    /// File path with position (file:line[:column])
    pub location: String,
}

impl LocationArg {
    pub fn parse(&self) -> Result<ParsedLocation> {
        ParsedLocation::parse(&self.location)
    }
}

#[derive(Debug, Clone)]
pub struct ParsedLocation {
    pub file: PathBuf,
    pub line: u32,
    /// Defaults to 1 when the input omitted it; `column_explicit` says
    /// which case this is.
    pub column: u32,
    /// Whether the input actually carried a column. Consumers for which a
    /// missing column means something different from column 1 — symbol
    /// resolution on an edit target prefers the symbol *declared* on the
    /// line over whatever block interior column 1 happens to fall in —
    /// branch on this instead of guessing from the value.
    pub column_explicit: bool,
}

impl ParsedLocation {
    /// Detect if input is a `file:line[:col]` location format.
    /// Handles Windows paths (C:\...) correctly by checking for :digit pattern.
    pub fn is_location_format(input: &str) -> bool {
        let input = input.trim();
        if input.is_empty() {
            return false;
        }

        // Check for Windows drive letter (e.g., "C:\path")
        let is_windows = input.len() > 2
            && input.as_bytes().get(1) == Some(&b':')
            && input
                .as_bytes()
                .first()
                .is_some_and(|b| b.is_ascii_alphabetic());

        let search_start = if is_windows { 2 } else { 0 };
        let search_range = &input[search_start..];

        // Look for :digit pattern which indicates line number
        for (byte_idx, ch) in search_range.char_indices() {
            if ch == ':' {
                let abs_pos = search_start + byte_idx;
                let after = &input[abs_pos + 1..];
                if after.chars().next().is_some_and(|c| c.is_ascii_digit()) {
                    return true;
                }
            }
        }

        false
    }

    pub fn parse(input: &str) -> Result<Self> {
        let input = input.trim();
        if input.is_empty() {
            bail!(CliInputError::LocationEmpty);
        }

        let (file_part, rest) = Self::split_path_and_position(input)?;
        let file = PathBuf::from(file_part);
        let (line, column) = Self::parse_position(rest)?;

        Ok(Self {
            file,
            line,
            column: column.unwrap_or(1),
            column_explicit: column.is_some(),
        })
    }

    fn split_path_and_position(input: &str) -> Result<(&str, &str)> {
        let is_windows = input.len() > 2
            && input.as_bytes().get(1) == Some(&b':')
            && input.as_bytes().first().map(|b| b.is_ascii_alphabetic()) == Some(true);

        let search_start = if is_windows { 2 } else { 0 };
        let search_range = &input[search_start..];

        let mut potential_splits: Vec<(usize, bool)> = Vec::new(); // (position, is_negative)
        for (byte_idx, ch) in search_range.char_indices() {
            if ch == ':' {
                let abs_pos = search_start + byte_idx;
                let after = &input[abs_pos + 1..];
                let first_char = after.chars().next();
                match first_char {
                    Some(c) if c.is_ascii_digit() => potential_splits.push((abs_pos, false)),
                    Some('-') => potential_splits.push((abs_pos, true)),
                    _ => {}
                }
            }
        }

        if potential_splits.is_empty() {
            bail!(CliInputError::LocationMalformed);
        }

        let (split_pos, is_negative) = potential_splits[0];
        if is_negative {
            bail!(CliInputError::InvalidLine("negative".to_string()));
        }

        Ok((&input[..split_pos], &input[split_pos + 1..]))
    }

    fn parse_position(rest: &str) -> Result<(u32, Option<u32>)> {
        let parts: Vec<&str> = rest.splitn(2, ':').collect();

        let line_str = parts.first().unwrap_or(&"");
        let line: u32 = line_str
            .parse()
            .map_err(|_| CliInputError::InvalidLine((*line_str).to_string()))?;

        let column: Option<u32> = match parts.get(1) {
            Some(col_str) => Some(
                col_str
                    .parse()
                    .map_err(|_| CliInputError::InvalidColumn((*col_str).to_string()))?,
            ),
            None => None,
        };

        if line == 0 {
            bail!(CliInputError::LineMustBePositive);
        }
        if column == Some(0) {
            bail!(CliInputError::ColumnMustBePositive);
        }

        Ok((line, column))
    }

    /// The position in its file: the path made absolute and canonical, and
    /// held to `project_root` where one is given, and the line and column
    /// held to what the file has. A position past the file's end is refused
    /// here, stating the file's extent, rather than handed to a language
    /// server, which answers one with an internal error or with nothing, as
    /// though the position were real and denoted nothing.
    pub fn resolve(&self, project_root: Option<&Path>) -> Result<Self> {
        let resolved = self.to_absolute_with_root(project_root)?;
        resolved.ensure_within_file(&self.file)?;
        Ok(resolved)
    }

    /// Convert to absolute path, optionally validating against project root
    fn to_absolute_with_root(&self, project_root: Option<&Path>) -> Result<Self> {
        let file = if self.file.is_absolute() {
            self.file.clone()
        } else {
            std::env::current_dir()
                .context("Failed to get current directory")?
                .join(&self.file)
        };

        // Canonicalize to resolve symlinks and .. components.
        let canonical = file.canonicalize().map_err(|_| {
            let path_str = self.file.to_string_lossy();
            if path_str.contains(':')
                && !path_str.starts_with('/')
                && path_str.chars().nth(1).is_none_or(|c| c != ':')
            {
                CliInputError::LocationMalformed
            } else {
                CliInputError::FileNotFound(self.file.clone())
            }
        })?;

        if let Some(root) = project_root {
            let canonical_root = root
                .canonicalize()
                .context("Failed to resolve project root")?;

            if !canonical.starts_with(&canonical_root) {
                bail!(CliInputError::PathOutsideProject(self.file.clone()));
            }
        }

        Ok(Self {
            file: canonical,
            line: self.line,
            column: self.column,
            column_explicit: self.column_explicit,
        })
    }

    /// Refuse a line past the file's last, or a column past one beyond its
    /// line's last character, the zero-width position at its end. Lines are
    /// counted as `str::lines` counts them, and an empty file still has the
    /// one position `1:1`. A line that is not UTF-8 is left to the reader that
    /// refuses the file as not text, since its characters cannot be counted.
    /// `named` is the path as it was given, which the refusal names.
    fn ensure_within_file(&self, named: &Path) -> Result<()> {
        use std::io::BufRead;

        let mut reader = std::io::BufReader::new(std::fs::File::open(&self.file)?);
        let mut text = Vec::new();
        let mut lines = 0u32;
        while lines < self.line {
            text.clear();
            if reader.read_until(b'\n', &mut text)? == 0 {
                break;
            }
            lines += 1;
        }
        if lines < self.line {
            if lines == 0 && self.line == 1 {
                text.clear();
            } else {
                bail!(CliInputError::LinePastEnd {
                    file: named.to_path_buf(),
                    line: self.line,
                    lines: lines as usize,
                });
            }
        }
        let text = match text.strip_suffix(b"\n") {
            Some(text) => text.strip_suffix(b"\r").unwrap_or(text),
            None => &text,
        };
        if let Ok(text) = std::str::from_utf8(text) {
            let chars = text.chars().count();
            if self.column as usize > chars + 1 {
                bail!(CliInputError::ColumnPastEnd {
                    file: named.to_path_buf(),
                    line: self.line,
                    column: self.column,
                    chars,
                });
            }
        }
        Ok(())
    }
}

impl std::fmt::Display for ParsedLocation {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}:{}:{}", self.file.display(), self.line, self.column)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_full_location() {
        let loc = ParsedLocation::parse("src/main.rs:10:5").unwrap();
        assert_eq!(loc.file, PathBuf::from("src/main.rs"));
        assert_eq!(loc.line, 10);
        assert_eq!(loc.column, 5);
        assert!(loc.column_explicit);
    }

    #[test]
    fn test_parse_without_column() {
        let loc = ParsedLocation::parse("src/main.rs:10").unwrap();
        assert_eq!(loc.file, PathBuf::from("src/main.rs"));
        assert_eq!(loc.line, 10);
        assert_eq!(loc.column, 1);
        assert!(!loc.column_explicit);
    }

    /// An explicit `:1` is an addressed column, not the omitted-column
    /// default — the two must stay distinguishable.
    #[test]
    fn test_parse_explicit_column_one() {
        let loc = ParsedLocation::parse("src/main.rs:10:1").unwrap();
        assert_eq!(loc.column, 1);
        assert!(loc.column_explicit);
    }

    #[test]
    fn test_parse_absolute_path() {
        let loc = ParsedLocation::parse("/Users/test/src/main.rs:10:5").unwrap();
        assert_eq!(loc.file, PathBuf::from("/Users/test/src/main.rs"));
        assert_eq!(loc.line, 10);
        assert_eq!(loc.column, 5);
    }

    #[test]
    fn test_parse_unicode_path() {
        let loc = ParsedLocation::parse("/tmp/한글_테스트.rs:10:5").unwrap();
        assert_eq!(loc.file, PathBuf::from("/tmp/한글_테스트.rs"));
        assert_eq!(loc.line, 10);
        assert_eq!(loc.column, 5);
    }

    #[test]
    fn test_parse_invalid() {
        assert!(ParsedLocation::parse("invalid").is_err());
        assert!(ParsedLocation::parse("file.rs").is_err());
        assert!(ParsedLocation::parse("file.rs:0:1").is_err());
        assert!(ParsedLocation::parse("").is_err());
    }

    #[test]
    fn test_parse_negative_line() {
        let err = ParsedLocation::parse("file.rs:-5:1").unwrap_err();
        assert!(err.to_string().contains("negative"));
    }

    #[test]
    fn test_parse_negative_column() {
        let err = ParsedLocation::parse("file.rs:5:-1").unwrap_err();
        assert!(err.to_string().contains("negative") || err.to_string().contains("Invalid"));
    }

    #[test]
    fn test_display() {
        let loc = ParsedLocation::parse("src/main.rs:10:5").unwrap();
        assert_eq!(loc.to_string(), "src/main.rs:10:5");
    }

    #[test]
    fn test_parse_windows_path() {
        let loc = ParsedLocation::parse("C:\\Users\\test\\file.rs:10:5").unwrap();
        assert_eq!(loc.file, PathBuf::from("C:\\Users\\test\\file.rs"));
        assert_eq!(loc.line, 10);
        assert_eq!(loc.column, 5);
    }

    /// The refusal, if any, for `position` in a file holding `content`.
    fn refusal(content: &str, position: &str) -> Option<String> {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("f.rs"), content).unwrap();
        let named = format!("{}/f.rs:{position}", dir.path().display());
        let err = ParsedLocation::parse(&named).unwrap().resolve(None).err()?;
        let message = err.to_string();
        Some(message.replace(&format!("{}/", dir.path().display()), ""))
    }

    /// A position is held to the file it names before any server is asked,
    /// and a refusal states the extent the position is past.
    #[test]
    fn a_position_past_the_files_end_is_refused_with_its_extent() {
        let content = "fn a() {}\n// 한글\n";
        assert_eq!(refusal(content, "2"), None);
        assert_eq!(refusal(content, "2:6"), None, "one past the last character");
        assert_eq!(refusal(content, "1:10"), None);
        assert_eq!(
            refusal(content, "3").as_deref(),
            Some("Line 3 is past the end of f.rs, which has 2 lines")
        );
        assert_eq!(
            refusal(content, "999:1").as_deref(),
            Some("Line 999 is past the end of f.rs, which has 2 lines")
        );
        assert_eq!(
            refusal(content, "2:7").as_deref(),
            Some("Column 7 is past the end of line 2 of f.rs, which has 5 characters"),
            "a column counts characters, not bytes"
        );
        assert_eq!(
            refusal("one\r\ntwo", "1:5").as_deref(),
            Some("Column 5 is past the end of line 1 of f.rs, which has 3 characters"),
            "a CRLF ending is no character of the line"
        );
        assert_eq!(
            refusal("one", "2").as_deref(),
            Some("Line 2 is past the end of f.rs, which has 1 line")
        );
        assert_eq!(
            refusal("", "1:1"),
            None,
            "an empty file has the position 1:1"
        );
        assert_eq!(
            refusal("", "1:2").as_deref(),
            Some("Column 2 is past the end of line 1 of f.rs, which has 0 characters")
        );
    }

    #[test]
    fn test_is_location_format() {
        // Valid location formats
        assert!(ParsedLocation::is_location_format("src/main.rs:10"));
        assert!(ParsedLocation::is_location_format("src/main.rs:10:5"));
        assert!(ParsedLocation::is_location_format("/path/to/file.rs:1:1"));
        assert!(ParsedLocation::is_location_format("file.rs:100"));
        assert!(ParsedLocation::is_location_format("C:\\path\\file.rs:10:5"));
        assert!(ParsedLocation::is_location_format("D:\\test.rs:1"));

        // Not location formats (plain file paths)
        assert!(!ParsedLocation::is_location_format("src/main.rs"));
        assert!(!ParsedLocation::is_location_format("/path/to/file.rs"));
        assert!(!ParsedLocation::is_location_format("C:\\path\\file.rs"));
        assert!(!ParsedLocation::is_location_format("file.rs"));
        assert!(!ParsedLocation::is_location_format(""));
        assert!(!ParsedLocation::is_location_format("file:name.rs")); // colon not followed by digit
    }
}
