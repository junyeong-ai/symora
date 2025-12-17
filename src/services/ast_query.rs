//! AST Query Service

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

pub struct DefaultAstQueryService {
    python: Mutex<Parser>,
    typescript: Mutex<Parser>, // Uses TSX grammar (superset) to handle both .ts and .tsx
    javascript: Mutex<Parser>,
    rust: Mutex<Parser>,
    go: Mutex<Parser>,
    java: Mutex<Parser>,
    kotlin: Mutex<Parser>,
    cpp: Mutex<Parser>,
    csharp: Mutex<Parser>,
    bash: Mutex<Parser>,
    ruby: Mutex<Parser>,
    lua: Mutex<Parser>,
    php: Mutex<Parser>,
}

impl DefaultAstQueryService {
    pub fn new() -> Result<Self, SearchError> {
        Ok(Self {
            python: Mutex::new(Self::create_parser(tree_sitter_python::LANGUAGE.into())?),
            typescript: Mutex::new(Self::create_parser(
                tree_sitter_typescript::LANGUAGE_TSX.into(), // TSX is a superset
            )?),
            javascript: Mutex::new(Self::create_parser(
                tree_sitter_javascript::LANGUAGE.into(),
            )?),
            rust: Mutex::new(Self::create_parser(tree_sitter_rust::LANGUAGE.into())?),
            go: Mutex::new(Self::create_parser(tree_sitter_go::LANGUAGE.into())?),
            java: Mutex::new(Self::create_parser(tree_sitter_java::LANGUAGE.into())?),
            kotlin: Mutex::new(Self::create_parser(tree_sitter_kotlin_sg::LANGUAGE.into())?),
            cpp: Mutex::new(Self::create_parser(tree_sitter_cpp::LANGUAGE.into())?),
            csharp: Mutex::new(Self::create_parser(tree_sitter_c_sharp::LANGUAGE.into())?),
            bash: Mutex::new(Self::create_parser(tree_sitter_bash::LANGUAGE.into())?),
            ruby: Mutex::new(Self::create_parser(tree_sitter_ruby::LANGUAGE.into())?),
            lua: Mutex::new(Self::create_parser(tree_sitter_lua::LANGUAGE.into())?),
            php: Mutex::new(Self::create_parser(tree_sitter_php::LANGUAGE_PHP.into())?),
        })
    }

    fn create_parser(language: Language) -> Result<Parser, SearchError> {
        let mut parser = Parser::new();
        parser
            .set_language(&language)
            .map_err(|e| SearchError::Failed(e.to_string()))?;
        Ok(parser)
    }

    fn get_parser_and_language(
        &self,
        language: SymbolLanguage,
    ) -> Option<(&Mutex<Parser>, Language)> {
        match language {
            SymbolLanguage::Python => Some((&self.python, tree_sitter_python::LANGUAGE.into())),
            SymbolLanguage::TypeScript => Some((
                &self.typescript,
                tree_sitter_typescript::LANGUAGE_TSX.into(),
            )),
            SymbolLanguage::JavaScript => {
                Some((&self.javascript, tree_sitter_javascript::LANGUAGE.into()))
            }
            SymbolLanguage::Rust => Some((&self.rust, tree_sitter_rust::LANGUAGE.into())),
            SymbolLanguage::Go => Some((&self.go, tree_sitter_go::LANGUAGE.into())),
            SymbolLanguage::Java => Some((&self.java, tree_sitter_java::LANGUAGE.into())),
            SymbolLanguage::Kotlin => Some((&self.kotlin, tree_sitter_kotlin_sg::LANGUAGE.into())),
            SymbolLanguage::Cpp => Some((&self.cpp, tree_sitter_cpp::LANGUAGE.into())),
            SymbolLanguage::CSharp => Some((&self.csharp, tree_sitter_c_sharp::LANGUAGE.into())),
            SymbolLanguage::Bash => Some((&self.bash, tree_sitter_bash::LANGUAGE.into())),
            SymbolLanguage::Ruby => Some((&self.ruby, tree_sitter_ruby::LANGUAGE.into())),
            SymbolLanguage::Lua => Some((&self.lua, tree_sitter_lua::LANGUAGE.into())),
            SymbolLanguage::PHP => Some((&self.php, tree_sitter_php::LANGUAGE_PHP.into())),
            _ => None,
        }
    }

    fn search_file(
        &self,
        file_path: &Path,
        content: &str,
        query: &Query,
        language: SymbolLanguage,
    ) -> Result<Vec<AstMatch>, SearchError> {
        let (parser_mutex, _) = self
            .get_parser_and_language(language)
            .ok_or(SearchError::UnsupportedLanguage(language))?;

        let mut parser = parser_mutex
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
                start_column: start.column as u32,
                end_column: end.column as u32,
                text,
                captures,
            });
        }

        Ok(results)
    }
}

impl Default for DefaultAstQueryService {
    fn default() -> Self {
        Self::new().expect("Failed to create AST query service")
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
        let (_, ts_language) = self
            .get_parser_and_language(language)
            .ok_or(SearchError::UnsupportedLanguage(language))?;

        let pattern_with_capture = if pattern.contains('@') {
            pattern.to_string()
        } else {
            format!("{} @match", pattern.trim())
        };

        let query = Query::new(&ts_language, &pattern_with_capture)
            .map_err(|e| SearchError::InvalidPattern(e.to_string()))?;

        let mut all_results = Vec::new();
        let extensions: Vec<&str> = language.extensions().to_vec();

        let max_size = super::max_file_size_bytes();

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
    fn test_service_creation() {
        let service = DefaultAstQueryService::new();
        assert!(service.is_ok());
    }

    #[test]
    fn test_supported_languages() {
        let service = DefaultAstQueryService::default();

        assert!(
            service
                .get_parser_and_language(SymbolLanguage::Python)
                .is_some()
        );
        assert!(
            service
                .get_parser_and_language(SymbolLanguage::Rust)
                .is_some()
        );
        assert!(
            service
                .get_parser_and_language(SymbolLanguage::Kotlin)
                .is_some()
        );
        assert!(
            service
                .get_parser_and_language(SymbolLanguage::PHP)
                .is_some()
        );
        assert!(
            service
                .get_parser_and_language(SymbolLanguage::Bash)
                .is_some()
        );
        assert!(
            service
                .get_parser_and_language(SymbolLanguage::Ruby)
                .is_some()
        );
        assert!(
            service
                .get_parser_and_language(SymbolLanguage::Lua)
                .is_some()
        );
        assert!(
            service
                .get_parser_and_language(SymbolLanguage::Unknown)
                .is_none()
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

        let (_, ts_lang) = service
            .get_parser_and_language(SymbolLanguage::Rust)
            .unwrap();
        let query = Query::new(&ts_lang, "(function_item) @match").unwrap();

        let matches = service.search_file(Path::new("test.rs"), code, &query, SymbolLanguage::Rust);

        assert!(matches.is_ok());
        let matches = matches.unwrap();
        assert_eq!(matches.len(), 2);
    }
}
