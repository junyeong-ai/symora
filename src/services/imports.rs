//! The module references a source file makes, read from its parse tree.
//!
//! A reference is only a reference where the grammar says one is. Scanning
//! lines for `import` and quotes cannot tell an import from a string that
//! spells one, and a Go constant holding a package path was enough to
//! fabricate a graph edge into that package — structure invented out of
//! text, which whatever ranks the graph then amplifies. Asking the parser
//! removes the whole class: a `const` is not an `import_spec`, however it
//! is spelled.
//!
//! Reading the tree also settles what line scanning could only approximate.
//! An import that spans lines is one node. An import inside a comment or a
//! string is not a node at all. And a grouped Rust `use` names each of its
//! targets rather than the module they share, so `use crate::commands::{a,
//! b}` reaches `a` and `b` instead of pointing at the directory above them.

use std::collections::HashMap;
use std::sync::Mutex;

use streaming_iterator::StreamingIterator;
use tree_sitter::{Node, Parser, Query, QueryCursor};

use crate::models::symbol::Language;

/// A parser and its compiled import query for one language, both built once.
struct LanguageEntry {
    parser: Mutex<Parser>,
    query: Query,
    dialect: Dialect,
}

impl LanguageEntry {
    fn capture_name(&self, capture: &tree_sitter::QueryCapture) -> Option<&str> {
        self.query
            .capture_names()
            .get(capture.index as usize)
            .copied()
    }
}

/// How a captured node's text spells the reference it carries.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Dialect {
    /// A Rust use tree, whose braces name several targets at once.
    RustUse,
    /// A bare identifier already in its final form.
    Plain,
    /// A path wrapped in quotes.
    Quoted,
    /// A statement whose path is the quoted or trailing portion.
    Statement,
}

pub struct ImportExtractor {
    languages: HashMap<Language, LanguageEntry>,
}

impl Default for ImportExtractor {
    fn default() -> Self {
        Self::new()
    }
}

impl ImportExtractor {
    pub fn new() -> Self {
        let mut languages = HashMap::new();
        register(
            &mut languages,
            Language::Rust,
            tree_sitter_rust::LANGUAGE.into(),
            RUST_QUERY,
            Dialect::RustUse,
        );
        register(
            &mut languages,
            Language::Python,
            tree_sitter_python::LANGUAGE.into(),
            PYTHON_QUERY,
            Dialect::Statement,
        );
        register(
            &mut languages,
            Language::Go,
            tree_sitter_go::LANGUAGE.into(),
            GO_QUERY,
            Dialect::Quoted,
        );
        register(
            &mut languages,
            Language::JavaScript,
            tree_sitter_javascript::LANGUAGE.into(),
            JAVASCRIPT_QUERY,
            Dialect::Quoted,
        );
        register(
            &mut languages,
            Language::TypeScript,
            tree_sitter_typescript::LANGUAGE_TSX.into(),
            JAVASCRIPT_QUERY,
            Dialect::Quoted,
        );
        register(
            &mut languages,
            Language::Java,
            tree_sitter_java::LANGUAGE.into(),
            JAVA_QUERY,
            Dialect::Plain,
        );
        register(
            &mut languages,
            Language::Kotlin,
            tree_sitter_kotlin_sg::LANGUAGE.into(),
            KOTLIN_QUERY,
            Dialect::Plain,
        );
        register(
            &mut languages,
            Language::Scala,
            tree_sitter_scala::LANGUAGE.into(),
            SCALA_QUERY,
            Dialect::Plain,
        );
        register(
            &mut languages,
            Language::Swift,
            tree_sitter_swift::LANGUAGE.into(),
            SWIFT_QUERY,
            Dialect::Statement,
        );
        register(
            &mut languages,
            Language::Elixir,
            tree_sitter_elixir::LANGUAGE.into(),
            ELIXIR_QUERY,
            Dialect::Plain,
        );
        register(
            &mut languages,
            Language::Dart,
            tree_sitter_dart::LANGUAGE.into(),
            DART_QUERY,
            Dialect::Statement,
        );
        register(
            &mut languages,
            Language::Terraform,
            tree_sitter_hcl::LANGUAGE.into(),
            TERRAFORM_QUERY,
            Dialect::Quoted,
        );
        Self { languages }
    }

    /// The module references `content` makes, in source order.
    pub fn extract(&self, content: &str, language: Language) -> Vec<String> {
        let Some(entry) = self.languages.get(&language) else {
            return Vec::new();
        };
        let tree = {
            let Ok(mut parser) = entry.parser.lock() else {
                return Vec::new();
            };
            match parser.parse(content, None) {
                Some(tree) => tree,
                None => return Vec::new(),
            }
        };

        let source = content.as_bytes();
        let mut references = Vec::new();
        let mut cursor = QueryCursor::new();
        let mut matches = cursor.matches(&entry.query, tree.root_node(), source);
        while let Some(m) = matches.next() {
            references.extend(references_of(m.captures, source, entry));
        }
        references
    }
}

fn register(
    languages: &mut HashMap<Language, LanguageEntry>,
    language: Language,
    ts_language: tree_sitter::Language,
    query_src: &str,
    dialect: Dialect,
) {
    // The grammar and its query are compiled into the binary, so
    // registration is deterministic for a given build. A failure means a
    // defective grammar — an incompatible ABI, or a query whose node types
    // moved — so that one language loses its edges rather than taking the
    // rest down with it. The coverage test below fails loudly when one goes
    // missing.
    let mut parser = Parser::new();
    if parser.set_language(&ts_language).is_err() {
        return;
    }
    let Ok(query) = Query::new(&ts_language, query_src) else {
        return;
    };
    languages.insert(
        language,
        LanguageEntry {
            parser: Mutex::new(parser),
            query,
            dialect,
        },
    );
}

/// The references one matched import names.
fn references_of(
    captures: &[tree_sitter::QueryCapture],
    source: &[u8],
    entry: &LanguageEntry,
) -> Vec<String> {
    // A `require` match carries its callee alongside the path so the callee
    // can be checked; every other match carries the path alone.
    if captures.len() == 2 {
        let callee = text_of(captures[0].node, source);
        return match callee {
            "require" => vec![unquote(text_of(captures[1].node, source)).to_string()],
            _ => Vec::new(),
        };
    }
    let Some(capture) = captures.first() else {
        return Vec::new();
    };
    let text = text_of(capture.node, source);
    // `mod x;` and `use self::x;` name the same file, and only the second
    // spelling says so. Normalising to it keeps the anchor a property of
    // the reference rather than of which syntax was used to write it.
    if entry.capture_name(capture) == Some("module") {
        return vec![format!("self::{}", text.trim())];
    }
    match entry.dialect {
        Dialect::RustUse => use_tree_targets(text),
        Dialect::Plain => vec![text.trim().to_string()],
        Dialect::Quoted => vec![unquote(text).to_string()],
        Dialect::Statement => vec![statement_path(text)],
    }
}

/// Every target a Rust use tree names.
///
/// `use a::{b, c::d}` names `a::b` and `a::c::d`. Reading only the text
/// before the brace would name `a` instead — the directory holding them —
/// and a group of modules would collapse onto one edge into their parent.
fn use_tree_targets(argument: &str) -> Vec<String> {
    fn walk(prefix: &str, tree: &str, out: &mut Vec<String>) {
        let tree = tree.trim();
        let Some(brace) = tree.find('{') else {
            let target = tree.split(" as ").next().unwrap_or(tree).trim();
            let joined = join(prefix, target);
            if !joined.is_empty() {
                out.push(joined);
            }
            return;
        };

        let head = join(prefix, tree[..brace].trim().trim_end_matches(':'));
        let mut depth = 0usize;
        let mut group = String::new();
        for ch in tree[brace + 1..].chars() {
            match ch {
                '{' => depth += 1,
                '}' if depth == 0 => break,
                '}' => depth -= 1,
                ',' if depth == 0 => {
                    walk(&head, &std::mem::take(&mut group), out);
                    continue;
                }
                _ => {}
            }
            group.push(ch);
        }
        if !group.trim().is_empty() {
            walk(&head, &group, out);
        }
    }

    fn join(prefix: &str, tail: &str) -> String {
        match (prefix.is_empty(), tail.is_empty() || tail == "self") {
            (true, _) => tail.to_string(),
            (false, true) => prefix.to_string(),
            (false, false) => format!("{prefix}::{tail}"),
        }
    }

    let mut out = Vec::new();
    walk("", argument, &mut out);
    out.retain(|target| target != "*" && !target.is_empty());
    out
}

/// The path an import statement carries: its quoted portion when it has
/// one, otherwise everything after the leading keyword.
fn statement_path(text: &str) -> String {
    if let Some(quoted) = first_quoted(text) {
        return quoted.to_string();
    }
    text.split_whitespace()
        .last()
        .unwrap_or_default()
        .trim_end_matches(';')
        .to_string()
}

fn first_quoted(text: &str) -> Option<&str> {
    let bytes = text.as_bytes();
    let open = bytes
        .iter()
        .position(|b| matches!(b, b'"' | b'\'' | b'`'))?;
    let quote = bytes[open];
    let rest = &text[open + 1..];
    let close = rest.as_bytes().iter().position(|b| *b == quote)?;
    Some(&rest[..close])
}

fn unquote(text: &str) -> &str {
    text.trim()
        .trim_matches(|c: char| c == '"' || c == '\'' || c == '`')
}

fn text_of<'a>(node: Node, source: &'a [u8]) -> &'a str {
    node.utf8_text(source).unwrap_or_default()
}

/// A `mod x;` with no body names a sibling file; one with a body is an
/// inline module and names nothing on disk.
const RUST_QUERY: &str = r#"
(use_declaration argument: (_) @path)
(mod_item name: (identifier) @module !body)
"#;

/// `from . import x` carries its depth in the module name and its target in
/// the imported name, so both are captured; the resolver joins them.
const PYTHON_QUERY: &str = r#"
(import_statement name: (dotted_name) @path)
(import_from_statement module_name: (_) @path)
"#;

const GO_QUERY: &str = r#"
(import_spec path: (interpreted_string_literal) @path)
"#;

const JAVASCRIPT_QUERY: &str = r#"
(import_statement source: (string) @path)
(export_statement source: (string) @path)
(call_expression
  function: (identifier) @callee
  arguments: (arguments (string) @path))
"#;

const JAVA_QUERY: &str = r#"
(import_declaration (scoped_identifier) @path)
"#;

const KOTLIN_QUERY: &str = r#"
(import_header (identifier) @path)
"#;

const SCALA_QUERY: &str = r#"
(import_declaration (stable_identifier) @path)
(import_declaration (identifier) @path)
"#;

const SWIFT_QUERY: &str = r#"
(import_declaration) @path
"#;

const ELIXIR_QUERY: &str = r#"
(alias) @path
"#;

const DART_QUERY: &str = r#"
(import_specification) @path
"#;

/// Terraform addresses another module through a block's `source`.
const TERRAFORM_QUERY: &str = r#"
(attribute
  (identifier) @callee
  (expression (literal_value (string_lit) @path)))
"#;

#[cfg(test)]
mod tests {
    use super::*;

    fn extract(content: &str, language: Language) -> Vec<String> {
        ImportExtractor::new().extract(content, language)
    }

    /// The defect the parse tree removes: a string that spells a package
    /// path is not an import, however exactly it matches one.
    #[test]
    fn a_string_that_spells_a_path_is_not_an_import() {
        let go = "package main\n\
                  import (\n\t\"fmt\"\n\t\"github.com/acme/w/internal/store\"\n)\n\
                  const Repo = \"github.com/acme/w/internal/api\"\n";
        assert_eq!(
            extract(go, Language::Go),
            vec!["fmt", "github.com/acme/w/internal/store"]
        );

        let rust = "use crate::cli::output;\nconst P: &str = \"crate::services::pack\";\n";
        assert_eq!(extract(rust, Language::Rust), vec!["crate::cli::output"]);

        let ts = "import { A } from \"./real\";\nconst s = \"./fake\";\n";
        assert_eq!(extract(ts, Language::TypeScript), vec!["./real"]);
    }

    /// A grouped `use` names each of its targets. Reading the text before
    /// the brace would name the module holding them instead, collapsing a
    /// group of modules onto one edge into their parent.
    #[test]
    fn a_grouped_use_names_each_target() {
        let source = "use crate::commands::{actions, bench};\n\
                      use crate::models::symbol::{Language, Location};\n\
                      use crate::services::{lsp, store::db};\n\
                      use crate::cli::{self, output as out};\n";
        assert_eq!(
            extract(source, Language::Rust),
            vec![
                "crate::commands::actions",
                "crate::commands::bench",
                "crate::models::symbol::Language",
                "crate::models::symbol::Location",
                "crate::services::lsp",
                "crate::services::store::db",
                "crate::cli",
                "crate::cli::output",
            ]
        );
    }

    /// An import spanning lines is one node, so nothing has to be stitched
    /// back together.
    #[test]
    fn an_import_spanning_lines_is_read_whole() {
        let source = "use crate::cli::response::{\n    Section,\n    SymbolOutput,\n};\n";
        assert_eq!(
            extract(source, Language::Rust),
            vec![
                "crate::cli::response::Section",
                "crate::cli::response::SymbolOutput"
            ]
        );
    }

    /// An import written inside a comment or a string is not a node.
    #[test]
    fn an_import_that_is_not_code_is_not_an_import() {
        let source = "// use crate::commented::out;\n\
                      /// use crate::documented::example;\n\
                      const S: &str = \"use crate::quoted::path;\";\n\
                      use crate::real::one;\n";
        assert_eq!(extract(source, Language::Rust), vec!["crate::real::one"]);
    }

    #[test]
    fn a_mod_declaration_names_a_file_only_when_it_has_no_body() {
        let source = "mod auth;\nmod tests {\n    fn helper() {}\n}\n";
        assert_eq!(extract(source, Language::Rust), vec!["self::auth"]);
    }

    #[test]
    fn python_carries_both_the_package_depth_and_the_module() {
        assert_eq!(
            extract("from . import helper\n", Language::Python),
            vec!["."]
        );
        assert_eq!(
            extract("from ..pkg.thing import Widget\n", Language::Python),
            vec!["..pkg.thing"]
        );
        assert_eq!(
            extract("import os.path\n", Language::Python),
            vec!["os.path"]
        );
    }

    #[test]
    fn javascript_covers_both_module_forms() {
        let source = "import { A } from \"./services/pack\";\n\
                      export { B } from \"../shared/util\";\n\
                      const legacy = require(\"./legacy\");\n\
                      const noise = format(\"./not-an-import\");\n";
        assert_eq!(
            extract(source, Language::JavaScript),
            vec!["./services/pack", "../shared/util", "./legacy"]
        );
    }

    #[test]
    fn jvm_languages_read_their_package_paths() {
        assert_eq!(
            extract("package a;\nimport com.acme.core.Thing;\n", Language::Java),
            vec!["com.acme.core.Thing"]
        );
        assert_eq!(
            extract("import com.acme.core.Thing\n", Language::Kotlin),
            vec!["com.acme.core.Thing"]
        );
    }

    #[test]
    fn every_language_pack_extracts_for_is_registered() {
        let extractor = ImportExtractor::new();
        for language in [
            Language::Rust,
            Language::Python,
            Language::Go,
            Language::JavaScript,
            Language::TypeScript,
            Language::Java,
            Language::Kotlin,
            Language::Scala,
            Language::Swift,
            Language::Elixir,
            Language::Dart,
            Language::Terraform,
        ] {
            assert!(
                extractor.languages.contains_key(&language),
                "{language:?} lost its import query — a defective grammar or a moved node type"
            );
        }
    }

    #[test]
    fn an_unregistered_language_yields_nothing() {
        assert!(extract("#include <vector>\n", Language::Cpp).is_empty());
        assert!(extract("", Language::Rust).is_empty());
    }
}
