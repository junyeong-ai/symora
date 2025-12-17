//! Location parsing for CLI commands

use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};

#[derive(Debug, Clone)]
pub struct ParsedLocation {
    pub file: PathBuf,
    pub line: u32,
    pub column: u32,
}

impl ParsedLocation {
    /// Parse location string and convert to absolute path in one step
    pub fn parse_absolute(input: &str) -> Result<Self> {
        Self::parse(input)?.to_absolute()
    }

    pub fn parse(input: &str) -> Result<Self> {
        let input = input.trim();
        if input.is_empty() {
            bail!("Location cannot be empty");
        }

        let (file_part, rest) = Self::split_path_and_position(input)?;
        let file = PathBuf::from(file_part);
        let (line, column) = Self::parse_position(rest)?;

        Ok(Self { file, line, column })
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
            bail!(
                "Invalid location format. Expected: file:line[:column]\nExample: src/main.rs:10:5"
            )
        }

        let (split_pos, is_negative) = potential_splits[0];
        if is_negative {
            bail!(
                "Invalid line number: negative values not allowed. Line numbers are 1-indexed positive integers.\nExample: src/main.rs:10:5"
            )
        }

        Ok((&input[..split_pos], &input[split_pos + 1..]))
    }

    fn parse_position(rest: &str) -> Result<(u32, u32)> {
        let parts: Vec<&str> = rest.splitn(2, ':').collect();

        let line_str = parts.first().unwrap_or(&"");
        let line: u32 = line_str.parse().map_err(|_| {
            anyhow::anyhow!(
                "Invalid line number '{}': must be a positive integer (1-indexed)",
                line_str
            )
        })?;

        let column: u32 = if let Some(col_str) = parts.get(1) {
            col_str.parse().map_err(|_| {
                anyhow::anyhow!(
                    "Invalid column number '{}': must be a positive integer (1-indexed)",
                    col_str
                )
            })?
        } else {
            1
        };

        if line == 0 {
            bail!("Line number must be >= 1 (got 0). Line numbers are 1-indexed.");
        }
        if column == 0 {
            bail!("Column number must be >= 1 (got 0). Column numbers are 1-indexed.");
        }

        Ok((line, column))
    }

    /// Convert to absolute path with security validation
    pub fn to_absolute(&self) -> Result<Self> {
        self.to_absolute_with_root(None)
    }

    /// Convert to absolute path, optionally validating against project root
    pub fn to_absolute_with_root(&self, project_root: Option<&Path>) -> Result<Self> {
        let file = if self.file.is_absolute() {
            self.file.clone()
        } else {
            std::env::current_dir()
                .context("Failed to get current directory")?
                .join(&self.file)
        };

        // Canonicalize to resolve symlinks and .. components
        let canonical = file.canonicalize().map_err(|_| {
            // Check if the path looks like a malformed location (e.g., "file:abc" from "file:abc:1")
            let path_str = self.file.to_string_lossy();
            if path_str.contains(':') && !path_str.starts_with('/') && path_str.chars().nth(1).is_none_or(|c| c != ':') {
                // Likely a malformed location like "file:abc" from input "file:abc:1"
                anyhow::anyhow!(
                    "Invalid location format: '{}'. Expected 'file:line[:column]' with numeric line/column values.\n\
                     Example: src/main.rs:10:5",
                    path_str
                )
            } else {
                anyhow::anyhow!("File not found: {}", file.display())
            }
        })?;

        // Validate against project boundary if provided
        if let Some(root) = project_root {
            let canonical_root = root
                .canonicalize()
                .context("Failed to resolve project root")?;

            if !canonical.starts_with(&canonical_root) {
                bail!(
                    "Access denied: {} is outside project boundary",
                    self.file.display()
                );
            }
        }

        Ok(Self {
            file: canonical,
            line: self.line,
            column: self.column,
        })
    }

    /// Validate position is within file bounds (async version with streaming)
    pub async fn validate_position_async(&self) -> Result<()> {
        use tokio::io::{AsyncBufReadExt, BufReader};

        let file = tokio::fs::File::open(&self.file)
            .await
            .with_context(|| format!("Failed to open file: {}", self.file.display()))?;

        let reader = BufReader::new(file);
        let mut lines = reader.lines();
        let mut line_num = 0u32;

        while let Some(line) = lines.next_line().await? {
            line_num += 1;
            if line_num == self.line {
                let col_max = line.len() + 1;
                if self.column as usize > col_max {
                    bail!(
                        "Column {} exceeds line length ({} chars) at line {}",
                        self.column,
                        line.len(),
                        self.line
                    );
                }
                return Ok(());
            }
        }

        let line_count = line_num.max(1);
        bail!(
            "Line {} exceeds file length ({} lines)",
            self.line,
            line_count
        )
    }

    /// Validate position with pre-read content
    pub fn validate_position_with_content(&self, content: &str) -> Result<()> {
        let lines: Vec<&str> = content.lines().collect();
        let line_count = lines.len().max(1);

        if self.line as usize > line_count {
            bail!(
                "Line {} exceeds file length ({} lines)",
                self.line,
                line_count
            );
        }

        if let Some(line_content) = lines.get((self.line - 1) as usize) {
            let col_max = line_content.len() + 1;
            if self.column as usize > col_max {
                bail!(
                    "Column {} exceeds line length ({} chars) at line {}",
                    self.column,
                    line_content.len(),
                    self.line
                );
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
    }

    #[test]
    fn test_parse_without_column() {
        let loc = ParsedLocation::parse("src/main.rs:10").unwrap();
        assert_eq!(loc.file, PathBuf::from("src/main.rs"));
        assert_eq!(loc.line, 10);
        assert_eq!(loc.column, 1);
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

    #[test]
    fn test_validate_position_with_content() {
        let loc = ParsedLocation {
            file: PathBuf::from("test.rs"),
            line: 2,
            column: 5,
        };

        let content = "line1\nline2\nline3";
        assert!(loc.validate_position_with_content(content).is_ok());

        let loc_invalid = ParsedLocation {
            file: PathBuf::from("test.rs"),
            line: 10,
            column: 1,
        };
        assert!(loc_invalid.validate_position_with_content(content).is_err());
    }
}
