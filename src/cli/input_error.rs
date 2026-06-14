use std::path::{Path, PathBuf};

use thiserror::Error;

use super::errors::{ErrorCode, OutputError};

/// Resolve a bare file argument against the project root and confirm it is a
/// real file before any language-server call. File-only commands (`symbols`,
/// `map file`, `inlay-hints`, `folding`, `selection`, `code-lens`, `format`,
/// `diagnostics`, `edit`) otherwise hand a mistyped path straight to the
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
        Err(CliInputError::FileNotFound(resolved))
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

    #[error("Column {column} exceeds line length ({chars} chars) at line {line}")]
    ColumnOutOfRange {
        column: u32,
        chars: usize,
        line: u32,
    },

    #[error("File not found: {0}")]
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
        let code = match err {
            CliInputError::FileNotFound(_) => ErrorCode::NotFound,
            CliInputError::FileTooLarge { .. } => ErrorCode::FileTooLarge,
            _ => ErrorCode::InvalidArgument,
        };
        OutputError::new(code, message)
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
