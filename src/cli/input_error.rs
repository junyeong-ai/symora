use std::path::{Path, PathBuf};

use thiserror::Error;

use super::errors::{ErrorCode, OutputError};

/// Resolve a bare file argument against the project root and confirm it is a
/// real file before any language-server call. File-only commands (`symbols`,
/// `map file`, `diagnostics`, `edit`) otherwise hand a mistyped path straight to the
/// server, which surfaces it as an opaque `internal` "Protocol error: No such
/// file" — indistinguishable from a real tool failure. This is the file-only
/// counterpart to `LocationArg`'s position validation, mapping a bad path to
/// the actionable `not_found` an agent can branch on.
pub fn resolve_project_file(file: &Path, root: &Path) -> Result<PathBuf, CliInputError> {
    let resolved = if file.is_absolute() {
        file.to_path_buf()
    } else {
        root.join(file)
    };
    if resolved.is_file() {
        Ok(resolved)
    } else {
        Err(CliInputError::FileNotFound(file.to_path_buf()))
    }
}

/// User-input validation errors raised by the CLI front-end. Every variant
/// carries the structured information needed to render a stable
/// `{code, message, hint?}` JSON envelope without string-pattern matching.
#[derive(Debug, Clone, Error)]
pub enum CliInputError {
    #[error("Location cannot be empty")]
    LocationEmpty,

    #[error("Invalid location format. Expected: file:line[:column]\nExample: src/main.rs:10:5")]
    LocationMalformed,

    #[error("Invalid line number '{0}': must be a positive integer (1-indexed)")]
    InvalidLine(String),

    #[error("Invalid column number '{0}': must be a positive integer (1-indexed)")]
    InvalidColumn(String),

    #[error("Line number must be >= 1 (got 0). Line numbers are 1-indexed.")]
    LineMustBePositive,

    #[error("Column number must be >= 1 (got 0). Column numbers are 1-indexed.")]
    ColumnMustBePositive,

    #[error("Line {line} exceeds file length ({total} lines)")]
    LineOutOfRange { line: u32, total: usize },

    /// A position named a line the file does not have. `file` is the path as
    /// it was given.
    #[error(
        "Line {line} is past the end of {}, which has {lines} {}",
        file.display(),
        if *lines == 1 { "line" } else { "lines" }
    )]
    LinePastEnd {
        file: PathBuf,
        line: u32,
        lines: usize,
    },

    /// A position named a column more than one past its line's last
    /// character. `file` is the path as it was given.
    #[error(
        "Column {column} is past the end of line {line} of {}, which has {chars} {}",
        file.display(),
        if *chars == 1 { "character" } else { "characters" }
    )]
    ColumnPastEnd {
        file: PathBuf,
        line: u32,
        column: u32,
        chars: usize,
    },

    /// A path names no file. It is the path as it was given, so a relative
    /// one is refused in the terms it was asked in.
    #[error("File not found: {}", .0.display())]
    FileNotFound(PathBuf),

    #[error("Access denied: {0} is outside project boundary")]
    PathOutsideProject(PathBuf),

    #[error("File too large for editing ({size_mb} MB). Maximum: {limit_mb} MB")]
    FileTooLarge { size_mb: u64, limit_mb: u64 },

    #[error("File is not writable: {0}. Check permissions.")]
    FileNotWritable(PathBuf),
}

impl From<CliInputError> for OutputError {
    fn from(err: CliInputError) -> Self {
        let message = err.to_string();
        match err {
            // A mistyped or moved path, or one read from another directory:
            // what the project holds is one call away.
            CliInputError::FileNotFound(_) => OutputError::new(ErrorCode::NotFound, message)
                .with_hint(
                    "A relative path is read from the project root. Its top-level directories \
                     and packages: symora map summary",
                ),
            CliInputError::FileTooLarge { .. } => {
                OutputError::new(ErrorCode::FileTooLarge, message)
            }
            // A position read from the file before it last changed, or
            // counted from 0: what the file declares now, and where, is one
            // call away.
            CliInputError::LinePastEnd { file, .. } | CliInputError::ColumnPastEnd { file, .. } => {
                OutputError::invalid(message).with_hint(format!(
                    "Lines and columns are 1-indexed, a column counting characters; a position \
                     taken before the file last changed may be out of date. Its declarations \
                     and their positions now: symora symbols -- {}",
                    super::utils::shell_arg(&file.display().to_string())
                ))
            }
            _ => OutputError::new(ErrorCode::InvalidArgument, message),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resolve_project_file_rejects_missing_and_accepts_real() {
        let root = std::path::Path::new("/");
        // A missing path is a typed not-found, never an opaque internal error.
        let err = resolve_project_file(
            std::path::Path::new("nonexistent_symora_probe_xyz.rs"),
            std::path::Path::new("/tmp"),
        )
        .unwrap_err();
        assert!(matches!(err, CliInputError::FileNotFound(_)));
        let out: OutputError = err.into();
        assert!(matches!(out.code, ErrorCode::NotFound));
        // A directory is not a usable file either.
        assert!(matches!(
            resolve_project_file(std::path::Path::new("tmp"), root),
            Err(CliInputError::FileNotFound(_))
        ));
        // An existing absolute file resolves through unchanged.
        let exe = std::env::current_exe().unwrap();
        assert_eq!(resolve_project_file(&exe, root).unwrap(), exe);
    }

    #[test]
    fn file_not_found_maps_to_not_found_code() {
        let err: OutputError = CliInputError::FileNotFound(PathBuf::from("a.rs")).into();
        assert!(matches!(err.code, ErrorCode::NotFound));
        assert!(err.message.contains("File not found"));
    }

    #[test]
    fn malformed_location_maps_to_invalid_argument() {
        let err: OutputError = CliInputError::LocationMalformed.into();
        assert!(matches!(err.code, ErrorCode::InvalidArgument));
    }

    #[test]
    fn file_too_large_maps_to_dedicated_code() {
        let err: OutputError = CliInputError::FileTooLarge {
            size_mb: 200,
            limit_mb: 100,
        }
        .into();
        assert!(matches!(err.code, ErrorCode::FileTooLarge));
        assert!(err.message.contains("200 MB"));
    }

    #[test]
    fn a_position_past_the_end_names_the_declarations_that_are_there() {
        for err in [
            CliInputError::LinePastEnd {
                file: PathBuf::from("src/a b.rs"),
                line: 99,
                lines: 10,
            },
            CliInputError::ColumnPastEnd {
                file: PathBuf::from("src/a b.rs"),
                line: 3,
                column: 40,
                chars: 12,
            },
        ] {
            let out: OutputError = err.into();
            assert!(matches!(out.code, ErrorCode::InvalidArgument));
            let hint = out.hint.expect("a remedy");
            assert!(hint.ends_with("symora symbols -- 'src/a b.rs'"), "{hint}");
            crate::cli::assert_named_commands_parse(&hint);
        }
    }

    #[test]
    fn line_out_of_range_carries_context() {
        let err: OutputError = CliInputError::LineOutOfRange {
            line: 99,
            total: 10,
        }
        .into();
        assert!(matches!(err.code, ErrorCode::InvalidArgument));
        assert!(err.message.contains("99"));
        assert!(err.message.contains("10"));
    }
}
