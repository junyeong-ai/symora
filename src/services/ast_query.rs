//! AST Query Service

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use async_trait::async_trait;
use streaming_iterator::StreamingIterator;
use tree_sitter::{Language, Parser, Query, QueryCursor};

use crate::error::SearchError;
use crate::infra::file_filter::{FileFilter, FileFilterConfig};
use crate::models::symbol::Language as SymbolLanguage;

#[derive(Debug, Clone)]
pub struct AstMatch {
    pub file: PathBuf,
    pub start_line: u32,
    pub end_line: u32,
    pub start_column: u32,
    pub end_column: u32,
    pub text: String,
    pub captures: Vec<(String, String)>,
}

#[async_trait]
pub trait AstQueryService: Send + Sync {
    async fn query(
        &self,
        pattern: &str,
        language: SymbolLanguage,
        paths: &[PathBuf],
    ) -> Result<Vec<AstMatch>, SearchError>;
}

struct ParserEntry {
    parser: Mutex<Parser>,
    ts_language: Language,
}

pub struct DefaultAstQueryService {
    parsers: HashMap<SymbolLanguage, ParserEntry>,
    max_file_size_bytes: u64,
}

/// Adding a language is a single `register` call.
fn register(
    parsers: &mut HashMap<SymbolLanguage, ParserEntry>,
    language: SymbolLanguage,
    ts_language: Language,
) {
    // The grammar is compiled into the binary, so registration is
    // deterministic for a given build. A failure means a defective grammar
    // (an incompatible ABI) — skip that one language so an unrelated language
    // never loses AST queries over it. The registration-completeness test
    // turns such a regression into a loud, targeted CI failure before it can
    // ship.
    let mut parser = Parser::new();
    if parser.set_language(&ts_language).is_err() {
        return;
    }
    parsers.insert(
        language,
        ParserEntry {
            parser: Mutex::new(parser),
            ts_language,
        },
    );
}

impl DefaultAstQueryService {
    pub fn new(max_file_size_bytes: u64) -> Self {
        let mut parsers = HashMap::new();
        register(
            &mut parsers,
            SymbolLanguage::Python,
            tree_sitter_python::LANGUAGE.into(),
        );
        // TSX is a superset grammar covering both .ts and .tsx — switching to
        // LANGUAGE_TYPESCRIPT would silently drop .tsx coverage.
        register(
            &mut parsers,
            SymbolLanguage::TypeScript,
            tree_sitter_typescript::LANGUAGE_TSX.into(),
        );
        register(
            &mut parsers,
            SymbolLanguage::JavaScript,
            tree_sitter_javascript::LANGUAGE.into(),
        );
        register(
            &mut parsers,
            SymbolLanguage::Rust,
            tree_sitter_rust::LANGUAGE.into(),
        );
        register(
            &mut parsers,
            SymbolLanguage::Go,
            tree_sitter_go::LANGUAGE.into(),
        );
        register(
            &mut parsers,
            SymbolLanguage::Java,
            tree_sitter_java::LANGUAGE.into(),
        );
        register(
            &mut parsers,
            SymbolLanguage::Kotlin,
            tree_sitter_kotlin_sg::LANGUAGE.into(),
        );
        register(
            &mut parsers,
            SymbolLanguage::Cpp,
            tree_sitter_cpp::LANGUAGE.into(),
        );
        register(
            &mut parsers,
            SymbolLanguage::CSharp,
            tree_sitter_c_sharp::LANGUAGE.into(),
        );
        register(
            &mut parsers,
            SymbolLanguage::Bash,
            tree_sitter_bash::LANGUAGE.into(),
        );
        register(
            &mut parsers,
            SymbolLanguage::Ruby,
            tree_sitter_ruby::LANGUAGE.into(),
        );
        register(
            &mut parsers,
            SymbolLanguage::Lua,
            tree_sitter_lua::LANGUAGE.into(),
        );
        register(
            &mut parsers,
            SymbolLanguage::PHP,
            tree_sitter_php::LANGUAGE_PHP.into(),
        );
        register(
            &mut parsers,
            SymbolLanguage::Swift,
            tree_sitter_swift::LANGUAGE.into(),
        );
        register(
            &mut parsers,
            SymbolLanguage::Scala,
            tree_sitter_scala::LANGUAGE.into(),
        );
        register(
            &mut parsers,
            SymbolLanguage::Elixir,
            tree_sitter_elixir::LANGUAGE.into(),
        );
        register(
            &mut parsers,
            SymbolLanguage::Dart,
            tree_sitter_dart::LANGUAGE.into(),
        );
        register(
            &mut parsers,
            SymbolLanguage::Terraform,
            tree_sitter_hcl::LANGUAGE.into(),
        );
        Self {
            parsers,
            max_file_size_bytes,
        }
    }

    fn search_file(
        &self,
        file_path: &Path,
        content: &str,
        query: &Query,
        language: SymbolLanguage,
    ) -> Result<Vec<AstMatch>, SearchError> {
        let entry = self
            .parsers
            .get(&language)
            .ok_or(SearchError::UnsupportedLanguage(language))?;

        let mut parser = entry
            .parser
            .lock()
            .map_err(|_| SearchError::Failed("Parser lock poisoned".to_string()))?;

        let tree = parser
            .parse(content, None)
            .ok_or_else(|| SearchError::Failed("Failed to parse file".to_string()))?;

        let mut cursor = QueryCursor::new();
        let capture_names: Vec<String> = query
            .capture_names()
            .iter()
            .map(|s| s.to_string())
            .collect();

        let mut results = Vec::new();
        let mut query_matches = cursor.matches(query, tree.root_node(), content.as_bytes());

        while let Some(query_match) = query_matches.next() {
            let Some(capture) = query_match.captures.first() else {
                continue;
            };

            let node = capture.node;
            let start = node.start_position();
            let end = node.end_position();
            let text = content[node.start_byte()..node.end_byte()].to_string();

            let captures: Vec<(String, String)> = query_match
                .captures
                .iter()
                .map(|c| {
                    let name = capture_names
                        .get(c.index as usize)
                        .cloned()
                        .unwrap_or_else(|| "match".to_string());
                    let text = content[c.node.start_byte()..c.node.end_byte()].to_string();
                    (name, text)
                })
                .collect();

            results.push(AstMatch {
                file: file_path.to_path_buf(),
                start_line: start.row as u32 + 1,
                end_line: end.row as u32 + 1,
                start_column: char_column(content, node.start_byte(), start.column),
                end_column: char_column(content, node.end_byte(), end.column),
                text,
                captures,
            });
        }

        Ok(results)
    }
}

/// Convert a tree-sitter position (0-indexed byte column within a line) to
/// the 1-indexed character column the JSON contract uses everywhere else
/// (invariant #1). `byte_column` is the node's byte offset within its line;
/// the line begins at `byte_offset - byte_column`.
fn char_column(content: &str, byte_offset: usize, byte_column: usize) -> u32 {
    let line_start = byte_offset.saturating_sub(byte_column);
    content
        .get(line_start..byte_offset)
        .map(|prefix| prefix.chars().count() as u32)
        .unwrap_or(byte_column as u32)
        + 1
}

impl Default for DefaultAstQueryService {
    fn default() -> Self {
        Self::new(10 * 1024 * 1024)
    }
}

#[async_trait]
impl AstQueryService for DefaultAstQueryService {
    async fn query(
        &self,
        pattern: &str,
        language: SymbolLanguage,
        paths: &[PathBuf],
    ) -> Result<Vec<AstMatch>, SearchError> {
        let entry = self
            .parsers
            .get(&language)
            .ok_or(SearchError::UnsupportedLanguage(language))?;

        let pattern_with_capture = if pattern.contains('@') {
            pattern.to_string()
        } else {
            format!("{} @match", pattern.trim())
        };

        let query = Query::new(&entry.ts_language, &pattern_with_capture)
            .map_err(|e| SearchError::InvalidPattern(e.to_string()))?;

        let mut all_results = Vec::new();
        let extensions: Vec<&str> = language.extensions().to_vec();

        let max_size = self.max_file_size_bytes;

        for path in paths {
            if path.is_file() {
                if let Some(ext) = path.extension().and_then(|e| e.to_str())
                    && extensions.contains(&ext)
                {
                    if let Ok(meta) = tokio::fs::metadata(path).await
                        && meta.len() > max_size
                    {
                        tracing::warn!(
                            "Skipping large file ({}MB): {}",
                            meta.len() / 1024 / 1024,
                            path.display()
                        );
                        continue;
                    }
                    match tokio::fs::read_to_string(path).await {
                        Ok(content) => match self.search_file(path, &content, &query, language) {
                            Ok(matches) => all_results.extend(matches),
                            Err(e) => tracing::debug!("Search failed {}: {}", path.display(), e),
                        },
                        Err(e) => tracing::debug!("Cannot read {}: {}", path.display(), e),
                    }
                }
            } else if path.is_dir() {
                let filter = FileFilter::new(FileFilterConfig {
                    root: path.clone(),
                    respect_gitignore: true,
                    respect_symora_ignore: true,
                    include_hidden: false,
                    ..Default::default()
                });

                let files = filter.discover_files(&extensions);

                for file_path in files {
                    if let Ok(meta) = tokio::fs::metadata(&file_path).await
                        && meta.len() > max_size
                    {
                        tracing::warn!(
                            "Skipping large file ({}MB): {}",
                            meta.len() / 1024 / 1024,
                            file_path.display()
                        );
                        continue;
                    }
                    match tokio::fs::read_to_string(&file_path).await {
                        Ok(content) => {
                            match self.search_file(&file_path, &content, &query, language) {
                                Ok(matches) => all_results.extend(matches),
                                Err(e) => {
                                    tracing::debug!("Search failed {}: {}", file_path.display(), e)
                                }
                            }
                        }
                        Err(e) => tracing::debug!("Cannot read {}: {}", file_path.display(), e),
                    }
                }
            }
        }

        Ok(all_results)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn char_column_is_one_indexed_and_char_based() {
        // ASCII: in "abc.foo", `foo` starts at byte 4 → 1-indexed column 5.
        assert_eq!(char_column("abc.foo", 4, 4), 5);
        // Line start is column 1, never 0.
        assert_eq!(char_column("xyz", 0, 0), 1);
        // Multibyte: "café." is 5 chars but 6 bytes; `foo` at byte 6 must
        // report char column 6, not the byte column (which would give 7).
        let s = "café.foo";
        let start = s.find("foo").unwrap();
        assert_eq!(char_column(s, start, start), 6);
        // Exclusive end: `foo` ends one past its last char — char column 9
        // for the 8-character string, again char- not byte-based.
        assert_eq!(char_column(s, s.len(), s.len()), 9);
    }

    /// Every language with a compiled-in grammar. A grammar whose
    /// registration silently fails (defective ABI) drops out of the parser
    /// map; this list turns that into a loud, targeted failure.
    const PARSER_LANGUAGES: [SymbolLanguage; 18] = [
        SymbolLanguage::Python,
        SymbolLanguage::TypeScript,
        SymbolLanguage::JavaScript,
        SymbolLanguage::Rust,
        SymbolLanguage::Go,
        SymbolLanguage::Java,
        SymbolLanguage::Kotlin,
        SymbolLanguage::Cpp,
        SymbolLanguage::CSharp,
        SymbolLanguage::Bash,
        SymbolLanguage::Ruby,
        SymbolLanguage::Lua,
        SymbolLanguage::PHP,
        SymbolLanguage::Swift,
        SymbolLanguage::Scala,
        SymbolLanguage::Elixir,
        SymbolLanguage::Dart,
        SymbolLanguage::Terraform,
    ];

    #[test]
    fn every_supported_language_registers_a_parser() {
        let service = DefaultAstQueryService::default();

        for language in PARSER_LANGUAGES {
            assert!(
                service.parsers.contains_key(&language),
                "no parser registered for {language:?}"
            );
        }
        assert_eq!(service.parsers.len(), PARSER_LANGUAGES.len());
        assert!(!service.parsers.contains_key(&SymbolLanguage::Unknown));
    }

    /// The parser registry and the AST node-type catalogue
    /// (`node_types::supported_languages`) are separate data, but a language
    /// the engine can parse yet can't describe — or vice versa — would leave
    /// `search ast`/`search nodes` advertising a language it can't serve.
    /// Pin the two sets together so neither can drift.
    #[test]
    fn parser_registry_matches_the_ast_node_catalogue() {
        use std::collections::HashSet;

        let service = DefaultAstQueryService::default();
        let registered: HashSet<SymbolLanguage> = service.parsers.keys().copied().collect();
        let catalogued: HashSet<SymbolLanguage> =
            crate::infra::ast::node_types::supported_languages()
                .iter()
                .copied()
                .collect();
        assert_eq!(
            registered, catalogued,
            "AST parser registry and node-type catalogue disagree on supported languages"
        );
    }

    #[test]
    fn test_rust_function_query() {
        let service = DefaultAstQueryService::default();

        let code = r#"
fn hello() {
    println!("hello");
}

pub fn world() -> i32 {
    42
}
"#;

        let ts_lang = &service
            .parsers
            .get(&SymbolLanguage::Rust)
            .unwrap()
            .ts_language;
        let query = Query::new(ts_lang, "(function_item) @match").unwrap();

        let matches = service.search_file(Path::new("test.rs"), code, &query, SymbolLanguage::Rust);

        assert!(matches.is_ok());
        let matches = matches.unwrap();
        assert_eq!(matches.len(), 2);
    }

    #[test]
    fn terraform_queries_resolve_through_the_hcl_grammar() {
        // Terraform is the SymbolLanguage variant; the grammar crate is hcl.
        // This guards the variant→grammar aliasing from silently breaking.
        let service = DefaultAstQueryService::default();

        let code = r#"
resource "aws_s3_bucket" "logs" {
  bucket = "my-logs"
}
"#;

        let ts_lang = &service
            .parsers
            .get(&SymbolLanguage::Terraform)
            .unwrap()
            .ts_language;
        let query = Query::new(ts_lang, "(block) @match").unwrap();

        let matches = service
            .search_file(
                Path::new("main.tf"),
                code,
                &query,
                SymbolLanguage::Terraform,
            )
            .unwrap();
        assert!(!matches.is_empty());
    }
}
