use std::path::{Path, PathBuf};

use crate::app::App;
use crate::cli::{CliInputError, ErrorCode, OutputError};

/// A query is a path-like pattern when it carries `/`, `*`, or `[` rather
/// than a plain identifier. The callers route these forms specially — a `*`
/// glob resolves against the index, other path-like forms fall through to the
/// LSP workspace-symbol lookup (which the index then supplements).
pub fn looks_like_symbol_path(query: &str) -> bool {
    query.contains('/') || query.contains('*') || query.contains('[')
}

/// Resolve `--path` arguments to the project-relative files or directories a
/// search is confined to, the project root itself as an empty path.
///
/// A named path is an assertion about the tree, so a typo fails rather than
/// searching a domain the caller did not mean and answering `0`, and every
/// missing path is named at once. A path must also lie inside the project and
/// outside its `.git` and `.symora` metadata.
pub(super) fn resolve_search_paths(
    app: &App,
    paths: &[PathBuf],
) -> Result<Vec<PathBuf>, OutputError> {
    if paths.is_empty() {
        return Err(OutputError::invalid(
            "path must contain at least one file or directory",
        ));
    }
    let io_error = |path: &Path, error: std::io::Error| {
        OutputError::new(ErrorCode::Io, format!("{}: {error}", path.display()))
    };
    let root = app
        .root()
        .canonicalize()
        .map_err(|error| io_error(app.root(), error))?;
    let mut missing = Vec::new();
    let mut resolved = Vec::new();
    for path in paths {
        let requested = app.root().join(path);
        let absolute = match requested.canonicalize() {
            Ok(absolute) => absolute,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                missing.push(path.display().to_string());
                continue;
            }
            Err(error) => return Err(io_error(&requested, error)),
        };
        let Ok(relative) = absolute.strip_prefix(&root) else {
            return Err(CliInputError::PathOutsideProject(absolute).into());
        };
        if relative
            .components()
            .any(|part| matches!(part.as_os_str().to_str(), Some(".git" | ".symora")))
        {
            return Err(OutputError::invalid(
                "Search excludes .git and .symora metadata",
            ));
        }
        resolved.push(relative.to_path_buf());
    }
    if !missing.is_empty() {
        return Err(
            OutputError::not_found(format!("Path not found: {}", missing.join(", ")))
                .with_hint("Check the --path arguments against the tree and retry."),
        );
    }
    Ok(resolved)
}
