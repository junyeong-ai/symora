//! AST Query Service

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use async_trait::async_trait;
use streaming_iterator::StreamingIterator;
use tree_sitter::{Language, Parser, Query, QueryCursor};

use crate::error::SearchError;
use crate::infra::file_filter::FileFilter;
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

/// The roots to search, given what the caller asked for.
///
/// Every named path is a root of its own, because naming one is what reaches
/// a corner the ignore policy excludes: a directory walked as a root applies
/// that policy relative to ITSELF, so `--path build` searches a `build/` the
/// project ignores. Dropping it because another argument contains it would
/// make the same argument mean different things depending on what stands
/// beside it, and answer a smaller domain than the caller named without
/// saying so.
///
/// What overlap must not do is count twice, and that is settled per FILE (see
/// `query`) rather than by discarding a root. Resolving to canonical paths is
/// what makes both possible: it collapses spellings of one path into one, and
/// gives the walk absolute names a later file can be compared against. A path
/// that will not resolve is counted as unread here, because it is a path the
/// search could not read.
fn search_roots(paths: &[PathBuf], unread: &mut Vec<PathBuf>) -> Vec<PathBuf> {
    // Deduplicate what was ASKED FOR before resolving it, so a path repeated
    // verbatim is one path even when it does not resolve. Two different
    // spellings of the same unresolvable path stay two: nothing can tell them
    // apart, precisely because nothing can resolve them.
    let mut given: Vec<&PathBuf> = paths.iter().collect();
    given.sort();
    given.dedup();

    let mut roots: Vec<PathBuf> = Vec::new();
    for path in given {
        match std::fs::canonicalize(path) {
            Ok(path) => roots.push(path),
            Err(e) => {
                if crate::infra::hides_content(&e) {
                    unread.push(path.clone());
                }
            }
        }
    }
    // `Ord for Path` compares components, not bytes, so the order is
    // structural rather than by spelling, and an ancestor precedes what it
    // contains — which keeps the emitted matches in one order however the
    // arguments were given.
    roots.sort();
    roots.dedup();
    roots
}

/// A query's answer: the matches, and how many paths it could not search.
///
/// A pattern search over a tree is only exact over the files it actually read,
/// so the two travel together — a caller publishing `matches.len()` as the
/// count has to know whether it is the whole one. A file excluded by the
/// configured size cap is not counted here: that is a stated policy with a
/// user-facing knob (`search.max_file_size_mb`), the same way a binary file is
/// outside a content search's domain. Only failures are.
#[derive(Debug, Default)]
pub struct AstAnswer {
    pub matches: Vec<AstMatch>,
    pub unread_paths: Vec<std::path::PathBuf>,
}

#[async_trait]
pub trait AstQueryService: Send + Sync {
    async fn query(
        &self,
        pattern: &str,
        language: SymbolLanguage,
        paths: &[PathBuf],
    ) -> Result<AstAnswer, SearchError>;
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
    // never loses AST queries over it. The registration-completeness test fails
    // loudly when a language is unregistered, so a defective grammar surfaces as
    // a targeted test failure rather than silent loss of AST queries.
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
    /// Search one file, once, recording what the answer ends up short of.
    ///
    /// An explicit path and one the walk found reach this the same way, so the
    /// two cannot draw the domain differently: a file over the size cap is
    /// outside the search rather than a hole in it, while a read that fails
    /// leaves what the file holds unknown and shortens the answer. `searched`
    /// is what keeps overlapping roots from matching one file twice.
    async fn search_one(
        &self,
        path: &Path,
        query: &Query,
        language: SymbolLanguage,
        searched: &mut HashSet<PathBuf>,
        answer: &mut AstAnswer,
    ) {
        if !searched.insert(path.to_path_buf()) {
            return;
        }
        if let Ok(meta) = tokio::fs::metadata(path).await
            && meta.len() > self.max_file_size_bytes
        {
            tracing::warn!(
                "Skipping large file ({}MB): {}",
                meta.len() / 1024 / 1024,
                path.display()
            );
            return;
        }
        match tokio::fs::read_to_string(path).await {
            Ok(content) => match self.search_file(path, &content, query, language) {
                Ok(matches) => answer.matches.extend(matches),
                Err(e) => {
                    answer.unread_paths.push(path.to_path_buf());
                    tracing::debug!("Search failed {}: {}", path.display(), e);
                }
            },
            Err(e) => {
                if crate::infra::hides_text(&e) {
                    answer.unread_paths.push(path.to_path_buf());
                }
                tracing::debug!("Cannot read {}: {}", path.display(), e);
            }
        }
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
    ) -> Result<AstAnswer, SearchError> {
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

        let mut answer = AstAnswer::default();
        let extensions: Vec<&str> = language.extensions().to_vec();
        // Roots can overlap — `--path pkg --path pkg/inner`, or a directory
        // spelled two ways — and a file reached from two of them would be
        // matched twice. `count` is the number of matches found, so that is
        // wrong in the output before it is wasted work. Canonical names make
        // one file one entry however it was reached.
        let mut searched: HashSet<PathBuf> = HashSet::new();

        for path in search_roots(paths, &mut answer.unread_paths) {
            let path = path.as_path();
            // A metadata error makes both predicates false, so an explicit
            // path the query could not stat would otherwise fall through
            // every branch and leave an unqualified zero behind — unless the
            // path went away between the walk and the stat, which leaves
            // nothing for the answer to be short of.
            let metadata = match std::fs::metadata(path) {
                Ok(metadata) => metadata,
                Err(e) => {
                    if crate::infra::hides_content(&e) {
                        answer.unread_paths.push(path.to_path_buf());
                    }
                    continue;
                }
            };
            if metadata.is_file() {
                if let Some(ext) = path.extension().and_then(|e| e.to_str())
                    && extensions.contains(&ext)
                {
                    self.search_one(path, &query, language, &mut searched, &mut answer)
                        .await;
                }
            } else if metadata.is_dir() {
                let filter = FileFilter::new(path);
                let discovery = filter.discover_files(&extensions);
                answer.unread_paths.extend(discovery.unreadable);

                for file_path in discovery.files {
                    self.search_one(&file_path, &query, language, &mut searched, &mut answer)
                        .await;
                }
            }
        }

        // One total order over the matches, because the emitted page is a
        // prefix of them: a walk hands back whatever order the filesystem
        // stored a directory in, so without this `--limit 5` means a different
        // five on another machine, and a different five here as soon as an
        // unrelated file is added. By file, then by position — which is also
        // the order a reader expects to read them in.
        answer.matches.sort_by(|a, b| {
            a.file
                .cmp(&b.file)
                .then_with(|| (a.start_line, a.start_column).cmp(&(b.start_line, b.start_column)))
                .then_with(|| (a.end_line, a.end_column).cmp(&(b.end_line, b.end_column)))
        });
        // Two roots that both reach a directory nobody can enter each report
        // it, and one hole is one hole.
        answer.unread_paths.sort();
        answer.unread_paths.dedup();
        Ok(answer)
    }
}

#[cfg(test)]
mod tests {

    /// A named path is a root of its own — that is what reaches a corner the
    /// ignore policy excludes — so none is dropped for standing under another.
    /// Overlap is settled per file instead, because `count` is the number of
    /// matches found and one file matched twice is wrong in the output before
    /// it is wasted work.
    #[test]
    fn every_named_path_is_a_root_and_no_file_is_searched_twice() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        std::fs::create_dir_all(root.join("pkg/inner")).unwrap();
        std::fs::write(root.join("pkg/inner/a.rs"), "fn a() {}\n").unwrap();

        let mut unread = Vec::new();
        let roots = search_roots(
            &[
                root.join("pkg"),
                root.join("pkg/inner"),
                root.join("./pkg"),
                root.join("pkg/inner/a.rs"),
            ],
            &mut unread,
        );
        assert_eq!(
            roots.len(),
            3,
            "one spelling each, and a nested name is still its own root: {roots:?}"
        );
        assert!(roots[0].ends_with("pkg"), "an ancestor leads: {roots:?}");
        assert!(unread.is_empty());

        // The file under both of them is matched once, not once per root.
        let service = DefaultAstQueryService::default();
        let answer = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap()
            .block_on(service.query(
                "(function_item) @f",
                SymbolLanguage::Rust,
                &[
                    root.join("pkg"),
                    root.join("pkg/inner"),
                    root.join("pkg/inner/a.rs"),
                ],
            ))
            .unwrap();
        assert_eq!(answer.matches.len(), 1, "one file, one match: {answer:?}");

        // Two trees that do not contain each other stay two, and a name that
        // shares a prefix without being under it is not swallowed.
        std::fs::create_dir_all(root.join("pkgtools")).unwrap();
        let mut unread = Vec::new();
        let roots = search_roots(&[root.join("pkg"), root.join("pkgtools")], &mut unread);
        assert_eq!(roots.len(), 2, "{roots:?}");

        // A path that is simply not there settles the domain: the caller is
        // told so before the walk starts, and one that vanishes after holds
        // nothing for the answer to be short of.
        let mut unread = Vec::new();
        let roots = search_roots(&[root.join("nope")], &mut unread);
        assert!(roots.is_empty());
        assert!(unread.is_empty());

        // A path the walk cannot resolve MIGHT hold matches, so it is the
        // shortfall it looks like — and one asked for twice is still one path.
        use std::os::unix::fs::PermissionsExt;
        let blocked = root.join("blocked");
        std::fs::create_dir(&blocked).unwrap();
        std::fs::write(blocked.join("b.rs"), "fn b() {}\n").unwrap();
        std::fs::set_permissions(&blocked, std::fs::Permissions::from_mode(0o000)).unwrap();
        let hidden = blocked.join("b.rs");

        // Probed against the tree, never read off the value under test: a guard
        // that consults the answer would take a defect for the environment.
        let mode_bites = std::fs::read_to_string(&hidden).is_err();

        let mut unread = Vec::new();
        search_roots(std::slice::from_ref(&hidden), &mut unread);
        let mut twice = Vec::new();
        search_roots(&[hidden.clone(), hidden], &mut twice);
        std::fs::set_permissions(&blocked, std::fs::Permissions::from_mode(0o755)).unwrap();

        if mode_bites {
            assert_eq!(unread.len(), 1);
            assert_eq!(twice.len(), 1, "one path asked for twice is one path");
        }
    }

    /// A capped page is a prefix, so the order it is a prefix OF has to be the
    /// answer's own and not the filesystem's. Directory read order is neither
    /// alphabetical nor stable across machines, which would make `--limit N`
    /// mean a different N in each place the same repository is checked out.
    #[test]
    fn matches_are_emitted_in_one_order_whatever_order_the_walk_found_them() {
        let dir = tempfile::tempdir().unwrap();
        let root = std::fs::canonicalize(dir.path()).unwrap();
        let root = root.as_path();
        // Created out of alphabetical order on purpose.
        for name in ["c", "a", "b"] {
            std::fs::create_dir_all(root.join(name)).unwrap();
            std::fs::write(
                root.join(name).join("x.rs"),
                format!("fn {name}1() {{}}\nfn {name}2() {{}}\n"),
            )
            .unwrap();
        }

        let service = DefaultAstQueryService::default();
        let answer = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap()
            .block_on(service.query(
                "(function_item) @f",
                SymbolLanguage::Rust,
                &[root.to_path_buf()],
            ))
            .unwrap();

        let emitted: Vec<(String, u32)> = answer
            .matches
            .iter()
            .map(|m| {
                (
                    m.file.strip_prefix(root).unwrap().display().to_string(),
                    m.start_line,
                )
            })
            .collect();
        let mut expected = emitted.clone();
        expected.sort();
        assert_eq!(emitted, expected, "by file, then by position");
    }

    /// Naming a path the project ignores is how a caller reaches it, and that
    /// cannot depend on what else was named: dropping it because another
    /// argument contains it answers a smaller domain than was asked for and
    /// says nothing about having done so.
    #[test]
    fn a_named_path_the_ignore_policy_excludes_is_searched_beside_its_ancestor() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        std::fs::create_dir_all(root.join("src")).unwrap();
        std::fs::create_dir_all(root.join("generated")).unwrap();
        std::fs::write(root.join(".gitignore"), "generated/\n").unwrap();
        std::fs::write(root.join("src/kept.rs"), "fn kept() {}\n").unwrap();
        std::fs::write(root.join("generated/g.rs"), "fn generated() {}\n").unwrap();

        let service = DefaultAstQueryService::default();
        let run = |paths: Vec<PathBuf>| {
            tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .unwrap()
                .block_on(service.query("(function_item) @f", SymbolLanguage::Rust, &paths))
                .unwrap()
        };

        let alone = run(vec![root.join("generated")]);
        assert_eq!(
            alone.matches.len(),
            1,
            "naming an ignored directory is what reaches it: {alone:?}"
        );

        let beside = run(vec![root.join("."), root.join("generated")]);
        let files: Vec<String> = beside
            .matches
            .iter()
            .map(|m| m.file.display().to_string())
            .collect();
        assert!(
            files.iter().any(|f| f.ends_with("generated/g.rs")),
            "an ancestor standing beside it must not swallow it: {files:?}"
        );
        assert!(
            files.iter().any(|f| f.ends_with("src/kept.rs")),
            "{files:?}"
        );
        assert_eq!(files.len(), 2, "and neither is counted twice: {files:?}");
    }

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
