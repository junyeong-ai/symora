use std::path::{Path, PathBuf};

use std::collections::HashSet;

use crate::error::LspError;
use crate::infra::file_filter::{FileFilter, matches_default_pattern};
use crate::infra::lsp::protocol::{LspLocation, Position, PositionEncoding, error_codes};
use crate::infra::lsp::{
    LspClient, LspFeature, LspManager, SupportLevel, get_alternative_suggestion, get_support_level,
};
use crate::models::lsp::uri_to_path;
use crate::models::symbol::{Language, Location, Symbol, SymbolKind};

use super::position::scalar_to_wire;

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

    // A null byte in the first 8KB is the same cheap test git uses to call a
    // file binary, and it settles the common case without decoding megabytes.
    let check_len = bytes.len().min(8192);
    let not_text = || LspError::FileNotText {
        path: file.display().to_string(),
    };
    if bytes[..check_len].contains(&0) {
        return Err(not_text());
    }

    String::from_utf8(bytes).map_err(|_| not_text())
}

pub(super) fn verify_document_snapshot(
    file: &Path,
    content: &str,
    expected_hash: Option<u64>,
) -> Result<(), LspError> {
    if expected_hash.is_none_or(|expected| crate::infra::hash_content(content) != expected) {
        return Err(LspError::ServerError {
            code: error_codes::CONTENT_MODIFIED,
            message: format!(
                "Edit request document changed: {}. Request fresh edits.",
                file.display()
            ),
        });
    }
    Ok(())
}

pub(super) fn verify_workspace_edit_target(
    file: &Path,
    expected_hash: Option<u64>,
    snapshots: &std::collections::HashMap<PathBuf, String>,
) -> Result<(), LspError> {
    let canonical = file.canonicalize()?;
    let mut found = false;
    for (path, content) in snapshots {
        if path == file || path.canonicalize()? == canonical {
            verify_document_snapshot(file, content, expected_hash)?;
            found = true;
        }
    }
    if !found {
        verify_document_snapshot(file, &std::fs::read_to_string(file)?, expected_hash)?;
    }
    Ok(())
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

/// Build an LSP request position from a symora-native 1-indexed scalar
/// `(line, column)`, encoding the column for the wire so a position after a
/// non-BMP char on the line lands where the server expects it.
pub(super) fn to_lsp_position(
    line: u32,
    column: u32,
    content: &str,
    encoding: PositionEncoding,
) -> Position {
    let line0 = line.saturating_sub(1);
    let line_text = content.lines().nth(line0 as usize).unwrap_or("");
    let character = scalar_to_wire(encoding, line_text, column.saturating_sub(1));
    Position::new(line0, character)
}

/// The `FeatureNotSupported` error for a feature, naming the server and a
/// working alternative — the one place that shape is built, shared by the
/// live pre-flight gate and the post-flight reclassifier.
pub(super) fn unsupported_error(
    manager: &LspManager,
    file: &Path,
    feature: LspFeature,
) -> LspError {
    let lang = Language::from_path(file);
    LspError::feature_not_supported(
        lang,
        manager
            .config(lang)
            .map_or("unknown", |config| &config.display_name),
        feature.display_name(),
        &get_alternative_suggestion(lang, feature),
    )
}

/// Pre-flight gate that consults the live server. The static table is only a
/// hint: when it says None but the connected server advertises the provider,
/// the request proceeds. This turns a static false-negative into a correct
/// attempt; it never refuses a server the caps advertise, and never downgrades
/// a statically-supported feature — a stale supported row costs one request,
/// whose -32601 `unsupported_if_unimplemented` reports as this same error.
pub(super) async fn check_feature_support_live(
    manager: &LspManager,
    client: &LspClient,
    file: &Path,
    feature: LspFeature,
) -> Result<(), LspError> {
    let lang = Language::from_path(file);
    if get_support_level(lang, feature) != SupportLevel::None
        || client.feature_advertised(feature).await
    {
        return Ok(());
    }
    Err(unsupported_error(manager, file, feature))
}

/// Post-flight reclassifier for a request the server rejected as JSON-RPC
/// MethodNotFound: the server has just stated it lacks `feature`, so the error
/// becomes the same `FeatureNotSupported` the gates build, naming the server
/// and the alternative, whatever the static table claimed. Every other error
/// passes through unchanged.
pub(super) fn unsupported_if_unimplemented(
    err: LspError,
    manager: &LspManager,
    file: &Path,
    feature: LspFeature,
) -> LspError {
    match err {
        LspError::ServerError { code, .. } if code == error_codes::METHOD_NOT_FOUND => {
            unsupported_error(manager, file, feature)
        }
        other => other,
    }
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
    let filter = FileFilter::new(root);
    let extensions = language.extensions();
    let files = filter.discover_files(extensions).files;
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

/// The index of the definition candidate, among those at `uris`, that
/// navigation follows, by source priority: in-project source beats
/// vendored/generated paths inside the project, which beat locations outside
/// it. Within a tier, a TS/JS implementation beats its `.d.ts` declaration.
/// A lone vendored or external candidate is still chosen — a real location
/// is more honest than an empty result (invariant #4).
pub(super) fn select_best_definition<'a>(
    uris: impl IntoIterator<Item = &'a str>,
    project_root: &Path,
) -> Option<usize> {
    // On ties `min_by_key` keeps the first candidate, preserving server order.
    uris.into_iter()
        .enumerate()
        .min_by_key(|(_, uri)| {
            let tier = definition_tier(&uri_to_path(uri), project_root);
            (tier, uri.ends_with(".d.ts"))
        })
        .map(|(index, _)| index)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    #[test]
    fn edit_response_keeps_the_request_baseline_after_an_overlay_refresh() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("main.go");
        let request_content = "abc()\n";
        std::fs::write(&file, request_content).unwrap();
        let expected_hash = Some(crate::infra::hash_content(request_content));
        let mut snapshots =
            std::collections::HashMap::from([(file.to_path_buf(), request_content.to_string())]);
        assert!(verify_workspace_edit_target(&file, expected_hash, &snapshots).is_ok());

        snapshots.insert(file.to_path_buf(), "prefixabc()\n".to_string());
        let error = verify_workspace_edit_target(&file, expected_hash, &snapshots).unwrap_err();
        assert_eq!(error.error_code(), error_codes::CONTENT_MODIFIED);
        assert!(error.to_string().contains("main.go"));
    }

    #[cfg(unix)]
    #[test]
    fn edit_response_checks_every_alias_of_the_request_document() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("main.go");
        let alias = dir.path().join("alias.go");
        std::fs::write(&file, "abc()\n").unwrap();
        std::os::unix::fs::symlink(&file, &alias).unwrap();
        let expected = Some(crate::infra::hash_content("abc()\n"));
        let snapshots = std::collections::HashMap::from([
            (file.clone(), "abc()\n".to_string()),
            (alias, "prefixabc()\n".to_string()),
        ]);
        assert_eq!(
            verify_workspace_edit_target(&file, expected, &snapshots)
                .unwrap_err()
                .error_code(),
            error_codes::CONTENT_MODIFIED
        );
    }

    #[test]
    fn edits_to_other_files_still_validate_the_request_document() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("main.go");
        let other = dir.path().join("other.go");
        std::fs::write(&file, "abc()\n").unwrap();
        std::fs::write(&other, "other()\n").unwrap();
        let expected = Some(crate::infra::hash_content("abc()\n"));
        let snapshots = std::collections::HashMap::from([(other, "other()\n".to_string())]);
        assert!(verify_workspace_edit_target(&file, expected, &snapshots).is_ok());
        std::fs::write(&file, "prefixabc()\n").unwrap();
        assert_eq!(
            verify_workspace_edit_target(&file, expected, &snapshots)
                .unwrap_err()
                .error_code(),
            error_codes::CONTENT_MODIFIED
        );
    }

    #[test]
    fn to_lsp_position_encodes_the_column_for_the_wire() {
        // ASCII line: scalar column and wire character coincide (1-indexed in,
        // 0-indexed out).
        let pos = to_lsp_position(1, 5, "fn main() {}", PositionEncoding::Utf16);
        assert_eq!(pos.line, 0);
        assert_eq!(pos.character, 4);

        // A non-BMP char before the column shifts the utf-16 wire offset: the
        // `;` after `let x = "😀"` is scalar column 11, utf-16 character 11.
        let line = "let x = \"😀\";";
        let pos = to_lsp_position(1, 11, line, PositionEncoding::Utf16);
        assert_eq!(pos.character, 11);
        // utf-8 wire offset for the same column is the byte offset, 13.
        let pos = to_lsp_position(1, 11, line, PositionEncoding::Utf8);
        assert_eq!(pos.character, 13);
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
        let uris = [
            "file:///proj/vendor/dep/lib.go",
            "file:///usr/lib/go/fmt/print.go",
            "file:///proj/internal/svc/handler.go",
        ];
        assert_eq!(select_best_definition(uris, root), Some(2));
    }

    #[test]
    fn select_best_definition_returns_a_lone_external_candidate() {
        let root = Path::new("/proj");
        // A real external location beats an empty answer (invariant #4).
        assert_eq!(
            select_best_definition(["file:///usr/lib/python3/site-packages/os.py"], root),
            Some(0)
        );
        assert_eq!(select_best_definition([], root), None);
    }

    #[test]
    fn select_best_definition_demotes_d_ts_within_a_tier_only() {
        let root = Path::new("/proj");
        // Within in-project candidates the implementation wins over .d.ts…
        let uris = ["file:///proj/src/api.d.ts", "file:///proj/src/api.ts"];
        assert_eq!(select_best_definition(uris, root), Some(1));
        // …but an in-project .d.ts still beats a vendored implementation.
        let uris = [
            "file:///proj/node_modules/dep/index.js",
            "file:///proj/types/api.d.ts",
        ];
        assert_eq!(select_best_definition(uris, root), Some(1));
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

    /// Bytes that are not UTF-8 text are named as that, not reported as a
    /// protocol failure: an agent walking a tree has to be able to tell a file
    /// it should skip from a tool that broke. Source carrying multibyte text is
    /// the case the check must not take for one.
    #[tokio::test]
    async fn bytes_that_are_not_text_are_named_rather_than_called_a_protocol_failure() {
        let dir = tempfile::tempdir().unwrap();

        let utf8 = dir.path().join("ok.py");
        std::fs::write(&utf8, "def 처리():\n    return '값'\n").unwrap();
        assert_eq!(
            read_file_validated(&utf8, u64::MAX).await.unwrap(),
            "def 처리():\n    return '값'\n"
        );

        for (name, bytes) in [
            (
                "legacy.py",
                b"# \xc7\xd1\xb1\xdb\ndef a():\n    pass\n".to_vec(),
            ),
            ("image.png", b"\x89PNG\r\n\x1a\n\x00\x00\x00\rIHDR".to_vec()),
        ] {
            let path = dir.path().join(name);
            std::fs::write(&path, &bytes).unwrap();
            match read_file_validated(&path, u64::MAX).await {
                Err(LspError::FileNotText { path: reported }) => {
                    assert_eq!(reported, path.display().to_string())
                }
                other => panic!("{name} was not reported as non-text: {other:?}"),
            }
        }
    }
}
