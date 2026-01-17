//! SearchIndex - BM25 ranked search with persistent SQLite FTS5

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::UNIX_EPOCH;

use tokio::sync::RwLock;

use super::db::SearchDb;
use super::types::{
    ContentSearchResult, IndexOptions, IndexStats, SearchConfig, SymbolIndexEntry,
    SymbolSearchResult,
};
use crate::error::SearchError;
use crate::infra::file_filter::{FileFilter, FileFilterConfig};
use crate::models::symbol::{Language, SymbolKind};

pub struct SearchIndex {
    db: RwLock<Option<SearchDb>>,
    project_root: PathBuf,
    config: SearchConfig,
    is_indexing: AtomicBool,
    progress: RwLock<Option<f32>>,
}

impl SearchIndex {
    pub fn new(project_root: &Path, config: SearchConfig) -> Self {
        Self {
            db: RwLock::new(None),
            project_root: project_root.to_path_buf(),
            config,
            is_indexing: AtomicBool::new(false),
            progress: RwLock::new(None),
        }
    }

    fn db_path(&self) -> PathBuf {
        self.project_root.join(".symora").join("search.db")
    }

    pub async fn init(&self) -> Result<(), SearchError> {
        if self.db.read().await.is_some() {
            return Ok(());
        }

        let db = SearchDb::open(&self.db_path()).await?;
        *self.db.write().await = Some(db);
        Ok(())
    }

    pub async fn search_symbols(
        &self,
        query: &str,
        limit: Option<usize>,
        kind_filter: Option<&[SymbolKind]>,
    ) -> Result<Vec<SymbolSearchResult>, SearchError> {
        let db = self.db.read().await;
        let db = db.as_ref().ok_or(SearchError::not_initialized())?;

        let limit = limit.unwrap_or(self.config.max_results);
        db.search_symbols(query, limit, kind_filter).await
    }

    pub async fn search_content(
        &self,
        query: &str,
        limit: Option<usize>,
        language: Option<Language>,
    ) -> Result<Vec<ContentSearchResult>, SearchError> {
        let db = self.db.read().await;
        let db = db.as_ref().ok_or(SearchError::not_initialized())?;

        let limit = limit.unwrap_or(self.config.max_results);
        db.search_content(query, limit, language).await
    }

    pub async fn index(&self, options: IndexOptions) -> Result<IndexStats, SearchError> {
        if self.is_indexing.swap(true, Ordering::SeqCst) {
            return Err(SearchError::already_indexing());
        }

        let result = self.do_index(options).await;

        self.is_indexing.store(false, Ordering::SeqCst);
        *self.progress.write().await = None;

        result
    }

    async fn do_index(&self, options: IndexOptions) -> Result<IndexStats, SearchError> {
        let db = self.db.read().await;
        let db = db.as_ref().ok_or(SearchError::not_initialized())?;

        let files = self.collect_files(&options).await;
        let total = files.len();

        if total == 0 {
            return self.stats().await;
        }

        let mut last_progress = 0;
        for (i, (path, lang)) in files.iter().enumerate() {
            let current_progress = (i * 100) / total;
            if current_progress >= last_progress + 5 {
                *self.progress.write().await = Some(i as f32 / total as f32);
                last_progress = current_progress;
            }

            let mtime = file_mtime(path);

            if !options.force && !db.needs_reindex(path, mtime).await? {
                continue;
            }

            if let Err(e) = self.index_file(db, path, mtime, *lang).await {
                tracing::debug!("Skip indexing {}: {}", path.display(), e);
            }
        }

        self.stats().await
    }

    async fn collect_files(&self, options: &IndexOptions) -> Vec<(PathBuf, Option<Language>)> {
        let filter = FileFilter::new(FileFilterConfig {
            root: self.project_root.clone(),
            respect_gitignore: true,
            respect_symora_ignore: true,
            include_hidden: false,
            ..Default::default()
        });

        let default_extensions: Vec<&str> = vec![
            "rs", "go", "py", "ts", "tsx", "js", "jsx", "java", "kt", "scala", "c", "cpp",
            "cc", "h", "hpp", "cs", "rb", "php", "swift", "lua", "sh", "bash",
        ];

        let extensions: Vec<&str> = match &options.languages {
            Some(langs) => langs
                .iter()
                .flat_map(|l| l.extensions().iter().copied())
                .collect(),
            None => default_extensions,
        };

        let paths = filter.discover_files(&extensions);

        paths
            .into_iter()
            .filter_map(|p| {
                if let Some(paths) = &options.paths {
                    if !paths.iter().any(|prefix| p.starts_with(prefix)) {
                        return None;
                    }
                }
                let lang = Language::from_path(&p);
                Some((p, if lang == Language::Unknown { None } else { Some(lang) }))
            })
            .collect()
    }

    async fn index_file(
        &self,
        db: &SearchDb,
        path: &Path,
        mtime: u64,
        language: Option<Language>,
    ) -> Result<(), SearchError> {
        db.delete_file(path).await?;

        let content = tokio::fs::read_to_string(path)
            .await
            .map_err(SearchError::Io)?;
        let file_id = db.get_or_create_file(path, mtime, language).await?;

        let symbols = self.extract_symbols(&content, language);
        if !symbols.is_empty() {
            db.insert_symbols(file_id, symbols).await?;
        }

        if self.config.index_content {
            let lines: Vec<(u32, String)> = content
                .lines()
                .enumerate()
                .map(|(i, line)| ((i + 1) as u32, line.to_string()))
                .collect();
            db.insert_content_lines(file_id, lines).await?;
        }

        Ok(())
    }

    fn extract_symbols(&self, content: &str, language: Option<Language>) -> Vec<SymbolIndexEntry> {
        let mut symbols = Vec::new();
        let lang = match language {
            Some(l) => l,
            None => return symbols,
        };

        for (line_idx, line) in content.lines().enumerate() {
            let line_num = (line_idx + 1) as u32;
            let trimmed = line.trim();

            if let Some(sym) = parse_symbol_from_line(trimmed, lang, line_num) {
                symbols.push(sym);
            }
        }

        symbols
    }

    pub async fn invalidate_file(&self, path: &Path) {
        if let Some(db) = self.db.read().await.as_ref() {
            let _ = db.delete_file(path).await;
        }
    }

    pub async fn stats(&self) -> Result<IndexStats, SearchError> {
        let db = self.db.read().await;
        let db = db.as_ref().ok_or(SearchError::not_initialized())?;

        let mut stats = db.stats().await?;
        stats.is_indexing = self.is_indexing.load(Ordering::Relaxed);
        stats.progress = *self.progress.read().await;

        if let Ok(meta) = tokio::fs::metadata(self.db_path()).await {
            stats.index_size_bytes = meta.len();
        }

        Ok(stats)
    }

    pub async fn clear(&self) -> Result<(), SearchError> {
        if let Some(db) = self.db.read().await.as_ref() {
            db.clear().await?;
        }
        Ok(())
    }

    pub async fn cleanup_expired(&self) -> usize {
        if self.config.ttl_secs == 0 {
            return 0;
        }

        if let Some(db) = self.db.read().await.as_ref() {
            db.cleanup_expired(self.config.ttl_secs).await.unwrap_or(0)
        } else {
            0
        }
    }

    pub async fn optimize(&self) -> Result<(), SearchError> {
        if let Some(db) = self.db.read().await.as_ref() {
            db.optimize().await?;
        }
        Ok(())
    }
}

fn parse_symbol_from_line(line: &str, lang: Language, line_num: u32) -> Option<SymbolIndexEntry> {
    let patterns: &[(&str, SymbolKind)] = match lang {
        Language::Rust => &[
            ("pub fn ", SymbolKind::Function),
            ("fn ", SymbolKind::Function),
            ("pub struct ", SymbolKind::Struct),
            ("struct ", SymbolKind::Struct),
            ("pub enum ", SymbolKind::Enum),
            ("enum ", SymbolKind::Enum),
            ("pub trait ", SymbolKind::Interface),
            ("trait ", SymbolKind::Interface),
            ("impl ", SymbolKind::Class),
            ("pub const ", SymbolKind::Constant),
            ("const ", SymbolKind::Constant),
            ("pub type ", SymbolKind::TypeParameter),
            ("type ", SymbolKind::TypeParameter),
            ("pub mod ", SymbolKind::Module),
            ("mod ", SymbolKind::Module),
        ],
        Language::Go => &[
            ("func ", SymbolKind::Function),
            ("type ", SymbolKind::Struct),
            ("const ", SymbolKind::Constant),
            ("var ", SymbolKind::Variable),
        ],
        Language::Python => &[
            ("def ", SymbolKind::Function),
            ("class ", SymbolKind::Class),
            ("async def ", SymbolKind::Function),
        ],
        Language::TypeScript | Language::JavaScript => &[
            ("function ", SymbolKind::Function),
            ("class ", SymbolKind::Class),
            ("interface ", SymbolKind::Interface),
            ("type ", SymbolKind::TypeParameter),
            ("const ", SymbolKind::Constant),
            ("let ", SymbolKind::Variable),
            ("export function ", SymbolKind::Function),
            ("export class ", SymbolKind::Class),
            ("export interface ", SymbolKind::Interface),
            ("export type ", SymbolKind::TypeParameter),
            ("export const ", SymbolKind::Constant),
        ],
        Language::Java => &[
            ("public class ", SymbolKind::Class),
            ("class ", SymbolKind::Class),
            ("public interface ", SymbolKind::Interface),
            ("interface ", SymbolKind::Interface),
            ("public enum ", SymbolKind::Enum),
            ("enum ", SymbolKind::Enum),
        ],
        Language::Kotlin => &[
            ("class ", SymbolKind::Class),
            ("data class ", SymbolKind::Class),
            ("object ", SymbolKind::Class),
            ("interface ", SymbolKind::Interface),
            ("enum class ", SymbolKind::Enum),
            ("fun ", SymbolKind::Function),
        ],
        _ => return None,
    };

    for (prefix, kind) in patterns {
        if line.starts_with(prefix) {
            let rest = line.split(prefix).last()?;
            let name = extract_identifier(rest)?;
            if !name.is_empty() && name.len() < 200 {
                return Some(SymbolIndexEntry {
                    name,
                    kind: *kind,
                    container: None,
                    line: line_num,
                    column: 1,
                });
            }
        }
    }

    None
}

fn extract_identifier(s: &str) -> Option<String> {
    let mut chars = s.trim_start().chars().peekable();
    let mut name = String::new();

    while let Some(&ch) = chars.peek() {
        if ch.is_alphanumeric() || ch == '_' {
            name.push(ch);
            chars.next();
        } else {
            break;
        }
    }

    if name.is_empty() {
        None
    } else {
        Some(name)
    }
}

fn file_mtime(path: &Path) -> u64 {
    std::fs::metadata(path)
        .and_then(|m| m.modified())
        .ok()
        .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extract_identifier_basic() {
        assert_eq!(extract_identifier("foo"), Some("foo".to_string()));
        assert_eq!(extract_identifier("foo_bar"), Some("foo_bar".to_string()));
        assert_eq!(extract_identifier("foo123"), Some("foo123".to_string()));
        assert_eq!(extract_identifier("_private"), Some("_private".to_string()));
    }

    #[test]
    fn extract_identifier_stops_at_non_ident() {
        assert_eq!(extract_identifier("foo(bar)"), Some("foo".to_string()));
        assert_eq!(extract_identifier("foo<T>"), Some("foo".to_string()));
        assert_eq!(extract_identifier("foo::bar"), Some("foo".to_string()));
        assert_eq!(extract_identifier("foo bar"), Some("foo".to_string()));
    }

    #[test]
    fn extract_identifier_trims_leading_whitespace() {
        assert_eq!(extract_identifier("  foo"), Some("foo".to_string()));
        assert_eq!(extract_identifier("\tfoo"), Some("foo".to_string()));
    }

    #[test]
    fn extract_identifier_returns_none_for_invalid() {
        assert_eq!(extract_identifier(""), None);
        assert_eq!(extract_identifier("   "), None);
        assert_eq!(extract_identifier("(foo)"), None);
        assert_eq!(extract_identifier("<T>"), None);
    }

    #[test]
    fn parse_rust_function() {
        let sym = parse_symbol_from_line("fn main() {", Language::Rust, 1).unwrap();
        assert_eq!(sym.name, "main");
        assert_eq!(sym.kind, SymbolKind::Function);
        assert_eq!(sym.line, 1);

        let sym = parse_symbol_from_line("pub fn execute() {", Language::Rust, 10).unwrap();
        assert_eq!(sym.name, "execute");
        assert_eq!(sym.kind, SymbolKind::Function);
    }

    #[test]
    fn parse_rust_struct() {
        let sym = parse_symbol_from_line("struct Foo {", Language::Rust, 1).unwrap();
        assert_eq!(sym.name, "Foo");
        assert_eq!(sym.kind, SymbolKind::Struct);

        let sym = parse_symbol_from_line("pub struct Bar<T> {", Language::Rust, 1).unwrap();
        assert_eq!(sym.name, "Bar");
        assert_eq!(sym.kind, SymbolKind::Struct);
    }

    #[test]
    fn parse_rust_enum() {
        let sym = parse_symbol_from_line("enum Status {", Language::Rust, 1).unwrap();
        assert_eq!(sym.name, "Status");
        assert_eq!(sym.kind, SymbolKind::Enum);
    }

    #[test]
    fn parse_rust_trait() {
        let sym = parse_symbol_from_line("trait Handler {", Language::Rust, 1).unwrap();
        assert_eq!(sym.name, "Handler");
        assert_eq!(sym.kind, SymbolKind::Interface);

        let sym = parse_symbol_from_line("pub trait Service {", Language::Rust, 1).unwrap();
        assert_eq!(sym.name, "Service");
        assert_eq!(sym.kind, SymbolKind::Interface);
    }

    #[test]
    fn parse_go_function() {
        let sym = parse_symbol_from_line("func main() {", Language::Go, 1).unwrap();
        assert_eq!(sym.name, "main");
        assert_eq!(sym.kind, SymbolKind::Function);

        let sym = parse_symbol_from_line("func (s *Server) Handle() {", Language::Go, 1);
        assert!(sym.is_none()); // Method receiver - doesn't start with "func "
    }

    #[test]
    fn parse_python_function() {
        let sym = parse_symbol_from_line("def hello():", Language::Python, 1).unwrap();
        assert_eq!(sym.name, "hello");
        assert_eq!(sym.kind, SymbolKind::Function);

        let sym = parse_symbol_from_line("async def fetch():", Language::Python, 1).unwrap();
        assert_eq!(sym.name, "fetch");
        assert_eq!(sym.kind, SymbolKind::Function);
    }

    #[test]
    fn parse_python_class() {
        let sym = parse_symbol_from_line("class MyClass:", Language::Python, 1).unwrap();
        assert_eq!(sym.name, "MyClass");
        assert_eq!(sym.kind, SymbolKind::Class);

        let sym = parse_symbol_from_line("class Derived(Base):", Language::Python, 1).unwrap();
        assert_eq!(sym.name, "Derived");
        assert_eq!(sym.kind, SymbolKind::Class);
    }

    #[test]
    fn parse_typescript_function() {
        let sym = parse_symbol_from_line("function handleRequest() {", Language::TypeScript, 1).unwrap();
        assert_eq!(sym.name, "handleRequest");
        assert_eq!(sym.kind, SymbolKind::Function);

        let sym = parse_symbol_from_line("export function main() {", Language::TypeScript, 1).unwrap();
        assert_eq!(sym.name, "main");
        assert_eq!(sym.kind, SymbolKind::Function);
    }

    #[test]
    fn parse_typescript_class() {
        let sym = parse_symbol_from_line("class Server {", Language::TypeScript, 1).unwrap();
        assert_eq!(sym.name, "Server");
        assert_eq!(sym.kind, SymbolKind::Class);

        let sym = parse_symbol_from_line("export class Handler {", Language::TypeScript, 1).unwrap();
        assert_eq!(sym.name, "Handler");
        assert_eq!(sym.kind, SymbolKind::Class);
    }

    #[test]
    fn parse_java_class() {
        let sym = parse_symbol_from_line("public class Main {", Language::Java, 1).unwrap();
        assert_eq!(sym.name, "Main");
        assert_eq!(sym.kind, SymbolKind::Class);

        let sym = parse_symbol_from_line("class Helper {", Language::Java, 1).unwrap();
        assert_eq!(sym.name, "Helper");
        assert_eq!(sym.kind, SymbolKind::Class);
    }

    #[test]
    fn parse_java_interface() {
        let sym = parse_symbol_from_line("public interface Service {", Language::Java, 1).unwrap();
        assert_eq!(sym.name, "Service");
        assert_eq!(sym.kind, SymbolKind::Interface);

        let sym = parse_symbol_from_line("interface Handler {", Language::Java, 1).unwrap();
        assert_eq!(sym.name, "Handler");
        assert_eq!(sym.kind, SymbolKind::Interface);
    }

    #[test]
    fn parse_kotlin_class() {
        let sym = parse_symbol_from_line("class Server {", Language::Kotlin, 1).unwrap();
        assert_eq!(sym.name, "Server");
        assert_eq!(sym.kind, SymbolKind::Class);

        let sym = parse_symbol_from_line("data class User(val name: String)", Language::Kotlin, 1).unwrap();
        assert_eq!(sym.name, "User");
        assert_eq!(sym.kind, SymbolKind::Class);
    }

    #[test]
    fn parse_kotlin_function() {
        let sym = parse_symbol_from_line("fun main() {", Language::Kotlin, 1).unwrap();
        assert_eq!(sym.name, "main");
        assert_eq!(sym.kind, SymbolKind::Function);
    }

    #[test]
    fn parse_ignores_indented_lines() {
        assert!(parse_symbol_from_line("    fn nested() {", Language::Rust, 1).is_none());
        assert!(parse_symbol_from_line("        def inner():", Language::Python, 1).is_none());
    }

    #[test]
    fn parse_unsupported_language() {
        assert!(parse_symbol_from_line("fn main() {", Language::Unknown, 1).is_none());
    }
}
