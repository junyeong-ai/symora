//! Helper functions for LSP operations

use std::path::{Path, PathBuf};

use crate::error::LspError;
use crate::infra::file_filter::{FileFilter, FileFilterConfig};
use crate::infra::lsp::protocol::{LspLocation, Position};
use crate::infra::lsp::{
    LspFeature, SupportLevel, get_alternative_suggestion, get_support_level, language_server_name,
};
use crate::models::lsp::{TypeHierarchyItem, uri_to_path};
use crate::models::symbol::{Language, Location, Symbol, SymbolKind};

/// Read file content with validation in a single pass
///
/// Optimized for typical use: single file open, pre-allocated buffer.
/// Binary detection via null byte check in first 8KB (more reliable than UTF-8 decode errors).
pub(super) async fn read_file_validated(file: &Path) -> Result<String, LspError> {
    use tokio::io::AsyncReadExt;

    let max_size = crate::services::max_file_size_bytes();
    let mut f = tokio::fs::File::open(file).await?;
    let metadata = f.metadata().await?;
    let file_size = metadata.len();

    if max_size != u64::MAX && file_size > max_size {
        return Err(LspError::FileTooLarge {
            path: file.display().to_string(),
            size_mb: file_size / 1024 / 1024,
            limit_mb: max_size / 1024 / 1024,
        });
    }

    // Pre-allocate buffer and read entire file
    let mut bytes = Vec::with_capacity(file_size as usize);
    f.read_to_end(&mut bytes).await?;

    // Check for null bytes in first 8KB (binary file indicator)
    let check_len = bytes.len().min(8192);
    if bytes[..check_len].contains(&0) {
        return Err(LspError::Protocol(format!(
            "Cannot process binary file: {}",
            file.display()
        )));
    }

    // Convert to string (validates UTF-8)
    String::from_utf8(bytes)
        .map_err(|_| LspError::Protocol(format!("Cannot process binary file: {}", file.display())))
}

pub(super) async fn read_line_streaming(file: &Path, target_line: u32) -> Option<String> {
    use tokio::io::{AsyncBufReadExt, BufReader};

    let file = tokio::fs::File::open(file).await.ok()?;
    let reader = BufReader::new(file);
    let mut lines = reader.lines();
    let mut line_num = 0u32;

    while let Ok(Some(line)) = lines.next_line().await {
        if line_num == target_line {
            return Some(line);
        }
        line_num += 1;
    }

    None
}

pub(super) fn to_lsp_position(line: u32, column: u32) -> Position {
    Position::new(line.saturating_sub(1), column.saturating_sub(1))
}

pub(super) fn check_feature_support(
    file: &Path,
    feature: LspFeature,
) -> Result<SupportLevel, LspError> {
    let lang = Language::from_path(file);
    let level = get_support_level(lang, feature);

    if level == SupportLevel::None {
        return Err(LspError::feature_not_supported(
            lang,
            language_server_name(lang),
            feature.display_name(),
            &get_alternative_suggestion(lang, feature),
        ));
    }

    Ok(level)
}

pub(super) fn find_project_entry(root: &Path, language: Language) -> Option<PathBuf> {
    let config_files: &[&str] = match language {
        Language::TypeScript | Language::JavaScript => {
            &["tsconfig.json", "jsconfig.json", "package.json"]
        }
        Language::Python => &["pyproject.toml", "setup.py", "setup.cfg"],
        Language::Rust => &["Cargo.toml"],
        Language::Kotlin => &["build.gradle.kts", "build.gradle", "settings.gradle.kts"],
        Language::Java => &["pom.xml", "build.gradle", "build.gradle.kts"],
        Language::Go => &["go.mod"],
        _ => &[],
    };

    for config in config_files {
        let path = root.join(config);
        if path.exists() {
            return Some(path);
        }
    }

    find_first_file(root, language)
}

pub(super) fn find_first_file(root: &Path, language: Language) -> Option<PathBuf> {
    let filter = FileFilter::new(FileFilterConfig {
        root: root.to_path_buf(),
        respect_gitignore: true,
        respect_symora_ignore: true,
        include_hidden: false,
        ..Default::default()
    });

    let extensions = language.extensions();
    let files = filter.discover_files(extensions);
    files.into_iter().next()
}

/// Parse LSP location response which can be:
/// - null
/// - Location (single)
/// - Location[] (array)
/// - LocationLink[] (rust-analyzer, clangd use this format)
pub(super) fn parse_location_response(result: &serde_json::Value) -> Option<Vec<LspLocation>> {
    use crate::infra::lsp::protocol::LocationLink;

    if result.is_null() {
        return None;
    }

    // Try Location[] first (most common for multi-location responses)
    if let Some(locations) = serde_json::from_value::<Vec<LspLocation>>(result.clone())
        .ok()
        .filter(|locs| !locs.is_empty())
    {
        return Some(locations);
    }

    // Try LocationLink[] (used by rust-analyzer, clangd)
    if let Some(links) = serde_json::from_value::<Vec<LocationLink>>(result.clone())
        .ok()
        .filter(|links| !links.is_empty())
    {
        return Some(links.into_iter().map(|l| l.to_location()).collect());
    }

    // Try single Location
    if let Ok(loc) = serde_json::from_value::<LspLocation>(result.clone()) {
        return Some(vec![loc]);
    }

    // Try single LocationLink
    if let Ok(link) = serde_json::from_value::<LocationLink>(result.clone()) {
        return Some(vec![link.to_location()]);
    }

    None
}

/// Parse a type hierarchy item from LSP response
pub(super) fn parse_type_hierarchy_item(item: &serde_json::Value) -> Option<TypeHierarchyItem> {
    let name = item.get("name")?.as_str()?.to_string();
    let kind_num = item.get("kind")?.as_u64()? as u32;
    let kind = SymbolKind::from_lsp(kind_num);
    let uri = item.get("uri")?.as_str()?;
    let range = item.get("selectionRange")?;
    let start = range.get("start")?;
    let line = start.get("line")?.as_u64()? as u32 + 1;
    let column = start.get("character")?.as_u64()? as u32 + 1;
    let detail = item
        .get("detail")
        .and_then(|d| d.as_str())
        .map(String::from);

    Some(TypeHierarchyItem {
        name,
        kind,
        location: Location::point(uri_to_path(uri), line, column),
        detail,
    })
}

/// Create a file-level symbol as fallback when no symbols are found.
/// This provides graceful degradation when LSP doesn't return any symbols.
pub(super) fn create_file_level_symbol(file: &Path) -> Symbol {
    let name = file
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("unknown")
        .to_string();

    Symbol::new(
        name,
        SymbolKind::File,
        Location::point(file.to_path_buf(), 1, 1),
    )
}

/// Filter locations to only include those within the project root.
/// Excludes external packages, stdlib paths, and other out-of-repository references.
pub(super) fn filter_locations_within_project(
    locations: Vec<Location>,
    project_root: &Path,
) -> Vec<Location> {
    locations
        .into_iter()
        .filter(|loc| {
            // Must be within project root
            if !loc.file.starts_with(project_root) {
                return false;
            }

            let path_str = loc.file.to_string_lossy();

            // Exclude common external/generated paths
            !path_str.contains("node_modules")
                && !path_str.contains(".venv")
                && !path_str.contains("__pycache__")
                && !path_str.contains("/vendor/")
                && !path_str.contains("/target/debug/")
                && !path_str.contains("/target/release/")
                && !path_str.contains("/.git/")
        })
        .collect()
}

/// Select the best definition from multiple locations.
/// For TypeScript/JavaScript: prefer source files over node_modules and .d.ts files.
pub(super) fn select_best_definition(
    locations: &[LspLocation],
    language: Language,
) -> Option<&LspLocation> {
    if locations.is_empty() {
        return None;
    }

    // Only apply filtering for TypeScript/JavaScript
    if !matches!(language, Language::TypeScript | Language::JavaScript) {
        return locations.first();
    }

    // Priority 1: Source files outside node_modules (not .d.ts)
    if let Some(loc) = locations.iter().find(|l| {
        let uri = &l.uri;
        !uri.contains("node_modules") && !uri.ends_with(".d.ts")
    }) {
        return Some(loc);
    }

    // Priority 2: Any file outside node_modules
    if let Some(loc) = locations.iter().find(|l| !l.uri.contains("node_modules")) {
        return Some(loc);
    }

    // Priority 3: Non-.d.ts file in node_modules (actual source)
    if let Some(loc) = locations.iter().find(|l| !l.uri.ends_with(".d.ts")) {
        return Some(loc);
    }

    // Fallback: first result
    locations.first()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_to_lsp_position() {
        let pos = to_lsp_position(10, 5);
        assert_eq!(pos.line, 9);
        assert_eq!(pos.character, 4);
    }
}
