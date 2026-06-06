use std::path::{Path, PathBuf};

use std::collections::HashSet;

use crate::error::LspError;
use crate::infra::file_filter::{FileFilter, FileFilterConfig, matches_default_pattern};
use crate::infra::lsp::protocol::{LspLocation, LspSymbolKind, Position};
use crate::infra::lsp::{
    LspFeature, SupportLevel, get_alternative_suggestion, get_support_level, language_server_name,
};
use crate::models::lsp::{TypeHierarchyItem, uri_to_path};
use crate::models::symbol::{Language, Location, Symbol, SymbolKind};

use super::converters::convert_symbol_kind;

pub(super) async fn read_file_validated(
    file: &Path,
    max_file_size: u64,
) -> Result<String, LspError> {
    use tokio::io::AsyncReadExt;

    let max_size = max_file_size;
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

pub(super) use crate::utils::char_to_byte_index;

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

pub(super) fn find_project_entry(
    root: &Path,
    language: Language,
    config: &crate::config::LspRuntimeConfig,
) -> Option<PathBuf> {
    if let Some(custom_files) = config.entry_files_for(language) {
        for pattern in custom_files {
            if pattern.contains('*') {
                if let Some(found) = find_file_by_glob(root, pattern) {
                    return Some(found);
                }
            } else {
                let path = root.join(pattern);
                if path.exists() {
                    return Some(path);
                }
            }
        }
    }

    let config_files: &[&str] = match language {
        Language::TypeScript | Language::JavaScript | Language::Vue => &[
            "tsconfig.json",
            "jsconfig.json",
            "angular.json",
            "next.config.js",
            "next.config.mjs",
            "next.config.ts",
            "nuxt.config.ts",
            "nuxt.config.js",
            "vite.config.ts",
            "vite.config.js",
            "package.json",
            "turbo.json",
            "nx.json",
            "lerna.json",
            "pnpm-workspace.yaml",
        ],
        Language::Python => &[
            "pyproject.toml",
            "setup.py",
            "setup.cfg",
            "poetry.lock",
            "uv.lock",
            "pdm.lock",
        ],
        Language::Rust => &["Cargo.toml"],
        Language::Go => &["go.work", "go.mod"],
        Language::Cpp => &["CMakeLists.txt", "meson.build", "Makefile"],
        Language::Zig => &["build.zig"],
        Language::Java | Language::Kotlin => &[
            "settings.gradle.kts",
            "settings.gradle",
            "build.gradle.kts",
            "build.gradle",
            "pom.xml",
            "WORKSPACE",
        ],
        Language::Scala => &[
            "build.sbt",
            "settings.gradle.kts",
            "settings.gradle",
            "build.gradle.kts",
            "build.gradle",
            "pom.xml",
        ],
        Language::Clojure => &["deps.edn", "project.clj"],
        Language::CSharp => &["*.sln", "*.csproj", "global.json"],
        Language::FSharp => &["*.sln", "*.fsproj", "global.json"],
        Language::Ruby => &["Gemfile", "*.gemspec", "Rakefile"],
        Language::PHP => &["composer.json"],
        Language::Perl => &["Makefile.PL", "cpanfile", "dist.ini"],
        Language::Lua => &["*.rockspec"],
        Language::Bash => &["Makefile"],
        Language::PowerShell => &["*.psd1"],
        Language::Haskell => &["package.yaml", "*.cabal", "stack.yaml", "cabal.project"],
        Language::Elixir => &["mix.exs"],
        Language::Erlang => &["rebar.config", "Makefile"],
        Language::OCaml => &["dune-project", "*.opam"],
        Language::Elm => &["elm.json"],
        Language::Swift => &["Package.swift", "*.xcodeproj", "*.xcworkspace"],
        Language::Dart => &["pubspec.yaml"],
        Language::Terraform => &["*.tf"],
        Language::Nix => &["flake.nix", "shell.nix", "default.nix"],
        Language::Julia => &["Project.toml"],
        Language::R => &["DESCRIPTION", "*.Rproj"],
        Language::Fortran => &["CMakeLists.txt", "Makefile"],
        Language::Markdown | Language::Yaml | Language::Toml | Language::Rego => &[],
        _ => &[],
    };

    for pattern in config_files {
        if pattern.contains('*') {
            if let Some(found) = find_file_by_glob(root, pattern) {
                return Some(found);
            }
        } else {
            let path = root.join(pattern);
            if path.exists() {
                return Some(path);
            }
        }
    }

    find_first_file(root, language)
}

fn find_file_by_glob(root: &Path, pattern: &str) -> Option<PathBuf> {
    let entries = std::fs::read_dir(root).ok()?;
    let pattern_suffix = pattern.trim_start_matches('*');

    for entry in entries.filter_map(Result::ok) {
        let name = entry.file_name();
        let name_str = name.to_string_lossy();
        if name_str.ends_with(pattern_suffix) {
            return Some(entry.path());
        }
    }
    None
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

pub(super) fn parse_location_response(result: &serde_json::Value) -> Option<Vec<LspLocation>> {
    use crate::infra::lsp::protocol::LocationLink;

    if result.is_null() {
        return None;
    }

    if let Some(arr) = result.as_array() {
        if arr.is_empty() {
            return None;
        }
        // Check first element to determine format
        if arr[0].get("targetUri").is_some() {
            // LocationLink[] format (rust-analyzer, clangd)
            if let Ok(links) = serde_json::from_value::<Vec<LocationLink>>(result.clone()) {
                return Some(links.into_iter().map(|l| l.to_location()).collect());
            }
        }
        // Standard Location[] format
        serde_json::from_value::<Vec<LspLocation>>(result.clone())
            .ok()
            .filter(|locs| !locs.is_empty())
    } else {
        // Single object
        if result.get("targetUri").is_some()
            && let Ok(link) = serde_json::from_value::<LocationLink>(result.clone())
        {
            return Some(vec![link.to_location()]);
        }
        serde_json::from_value::<LspLocation>(result.clone())
            .ok()
            .map(|l| vec![l])
    }
}

pub(super) fn parse_type_hierarchy_item(item: &serde_json::Value) -> Option<TypeHierarchyItem> {
    let name = item.get("name")?.as_str()?.to_string();
    // One LSP-taxonomy decoder for the whole crate: the numeric code becomes
    // a typed `LspSymbolKind`, then `convert_symbol_kind` maps it like every
    // other LSP path. An out-of-range code degrades to Variable.
    let kind = serde_json::from_value::<LspSymbolKind>(item.get("kind")?.clone())
        .map(convert_symbol_kind)
        .unwrap_or(SymbolKind::Variable);
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

pub fn dedup_symbols(symbols: Vec<Symbol>) -> Vec<Symbol> {
    let mut seen = HashSet::new();
    symbols
        .into_iter()
        .filter(|s| {
            let key = (
                s.name.clone(),
                s.kind,
                s.location.file.clone(),
                s.location.line,
                s.location.column,
            );
            seen.insert(key)
        })
        .collect()
}

/// Source-priority tier for a navigation candidate, best first. `Vendored`
/// is inside the project but under an ignored component (`vendor/`,
/// `node_modules/`, `target/`, …); `External` is outside the project root
/// entirely.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
enum DefinitionTier {
    InProjectSource,
    Vendored,
    External,
}

fn definition_tier(path: &Path, project_root: &Path) -> DefinitionTier {
    let Ok(relative) = path.strip_prefix(project_root) else {
        return DefinitionTier::External;
    };
    // Component-wise on purpose: the shared ignore patterns match directory
    // names (`target`, `vendor`), not full-path substrings — a file named
    // `targets.rs` must not be demoted.
    let vendored = relative.components().any(|c| {
        matches!(c, std::path::Component::Normal(name)
            if name.to_str().is_some_and(matches_default_pattern))
    });
    if vendored {
        DefinitionTier::Vendored
    } else {
        DefinitionTier::InProjectSource
    }
}

pub(super) fn filter_locations_within_project(
    locations: Vec<Location>,
    project_root: &Path,
) -> Vec<Location> {
    locations
        .into_iter()
        .filter(|loc| definition_tier(&loc.file, project_root) == DefinitionTier::InProjectSource)
        .collect()
}

/// Order definition candidates by source priority: in-project source beats
/// vendored/generated paths inside the project, which beat locations outside
/// it. Within a tier, a TS/JS implementation beats its `.d.ts` declaration.
/// A lone vendored or external candidate is still returned — a real location
/// is more honest than an empty result (invariant #4).
pub(super) fn select_best_definition<'a>(
    locations: &'a [LspLocation],
    project_root: &Path,
) -> Option<&'a LspLocation> {
    // On ties `min_by_key` keeps the first candidate, preserving server order.
    locations.iter().min_by_key(|l| {
        let tier = definition_tier(&uri_to_path(&l.uri), project_root);
        (tier, l.uri.ends_with(".d.ts"))
    })
}

pub fn find_containing_callable(symbols: &[Symbol], target_line: u32) -> Option<&Symbol> {
    fn search_recursive<'a>(
        symbols: &'a [Symbol],
        target_line: u32,
        current_best: Option<&'a Symbol>,
    ) -> Option<&'a Symbol> {
        let mut best = current_best;

        for symbol in symbols {
            let start = symbol.location.line;
            let end = symbol.location.end_line.unwrap_or(start);

            if start <= target_line && target_line <= end {
                // This symbol contains the target line
                if symbol.kind.is_callable() {
                    // Update best if this is more specific (smaller range)
                    let should_update = best.is_none_or(|b| {
                        let best_start = b.location.line;
                        let best_end = b.location.end_line.unwrap_or(best_start);
                        (end - start) < (best_end - best_start)
                    });
                    if should_update {
                        best = Some(symbol);
                    }
                }

                // Recursively search children
                if !symbol.children.is_empty() {
                    best = search_recursive(&symbol.children, target_line, best);
                }
            }
        }

        best
    }

    search_recursive(symbols, target_line, None)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    #[test]
    fn test_to_lsp_position() {
        let pos = to_lsp_position(10, 5);
        assert_eq!(pos.line, 9);
        assert_eq!(pos.character, 4);
    }

    #[test]
    fn test_find_containing_callable() {
        use crate::models::symbol::{Symbol, SymbolKind};

        // Create a nested symbol structure
        let inner_fn = Symbol::new(
            "inner_fn".to_string(),
            SymbolKind::Function,
            Location::full(PathBuf::from("test.rs"), 5, 1, 5, 1, 10, 1),
        );

        let outer_fn = Symbol::new(
            "outer_fn".to_string(),
            SymbolKind::Function,
            Location::full(PathBuf::from("test.rs"), 1, 1, 1, 1, 15, 1),
        )
        .with_children(vec![inner_fn]);

        let class = Symbol::new(
            "MyClass".to_string(),
            SymbolKind::Class,
            Location::full(PathBuf::from("test.rs"), 1, 1, 1, 1, 20, 1),
        )
        .with_children(vec![outer_fn.clone()]);

        let symbols = vec![class];

        // Line 7 should find inner_fn (most specific callable)
        let result = find_containing_callable(&symbols, 7);
        assert!(result.is_some());
        assert_eq!(result.unwrap().name, "inner_fn");

        // Line 12 should find outer_fn (inner_fn doesn't contain it)
        let result = find_containing_callable(&symbols, 12);
        assert!(result.is_some());
        assert_eq!(result.unwrap().name, "outer_fn");

        // Line 18 should find nothing (outside all callables)
        let result = find_containing_callable(&symbols, 18);
        assert!(result.is_none());
    }

    fn lsp_location(uri: &str) -> LspLocation {
        LspLocation {
            uri: uri.to_string(),
            range: Default::default(),
        }
    }

    #[test]
    fn definition_tier_classifies_per_component_not_substring() {
        let root = Path::new("/proj");
        assert_eq!(
            definition_tier(Path::new("/proj/src/lib.rs"), root),
            DefinitionTier::InProjectSource
        );
        // Vendored directory components, across languages.
        for vendored in [
            "/proj/vendor/dep/lib.go",
            "/proj/.venv/lib/site.py",
            "/proj/node_modules/dep/index.js",
            "/proj/target/debug/gen.rs",
        ] {
            assert_eq!(
                definition_tier(Path::new(vendored), root),
                DefinitionTier::Vendored,
                "{vendored}"
            );
        }
        // A file *named* like a pattern is not a directory component match…
        assert_eq!(
            definition_tier(Path::new("/proj/src/targets.rs"), root),
            DefinitionTier::InProjectSource
        );
        // …and anything outside the root is external.
        assert_eq!(
            definition_tier(Path::new("/usr/lib/go/fmt/print.go"), root),
            DefinitionTier::External
        );
    }

    #[test]
    fn select_best_definition_prefers_in_project_source_for_any_language() {
        let root = Path::new("/proj");
        let locs = vec![
            lsp_location("file:///proj/vendor/dep/lib.go"),
            lsp_location("file:///usr/lib/go/fmt/print.go"),
            lsp_location("file:///proj/internal/svc/handler.go"),
        ];
        assert_eq!(
            select_best_definition(&locs, root).unwrap().uri,
            "file:///proj/internal/svc/handler.go"
        );
    }

    #[test]
    fn select_best_definition_returns_a_lone_external_candidate() {
        let root = Path::new("/proj");
        let locs = vec![lsp_location("file:///usr/lib/python3/site-packages/os.py")];
        // A real external location beats an empty answer (invariant #4).
        assert!(select_best_definition(&locs, root).is_some());
        assert!(select_best_definition(&[], root).is_none());
    }

    #[test]
    fn select_best_definition_demotes_d_ts_within_a_tier_only() {
        let root = Path::new("/proj");
        // Within in-project candidates the implementation wins over .d.ts…
        let locs = vec![
            lsp_location("file:///proj/src/api.d.ts"),
            lsp_location("file:///proj/src/api.ts"),
        ];
        assert_eq!(
            select_best_definition(&locs, root).unwrap().uri,
            "file:///proj/src/api.ts"
        );
        // …but an in-project .d.ts still beats a vendored implementation.
        let locs = vec![
            lsp_location("file:///proj/node_modules/dep/index.js"),
            lsp_location("file:///proj/types/api.d.ts"),
        ];
        assert_eq!(
            select_best_definition(&locs, root).unwrap().uri,
            "file:///proj/types/api.d.ts"
        );
    }

    #[test]
    fn filter_locations_keeps_only_in_project_source() {
        let root = Path::new("/proj");
        let locations = vec![
            Location::point(PathBuf::from("/proj/src/lib.rs"), 1, 1),
            Location::point(PathBuf::from("/proj/vendor/dep/lib.go"), 1, 1),
            Location::point(PathBuf::from("/proj/dist/bundle.js"), 1, 1),
            Location::point(PathBuf::from("/elsewhere/lib.rs"), 1, 1),
        ];
        let kept = filter_locations_within_project(locations, root);
        assert_eq!(kept.len(), 1);
        assert_eq!(kept[0].file, PathBuf::from("/proj/src/lib.rs"));
    }
}
