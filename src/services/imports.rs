//! Static module references read from language grammars.
//!
//! Import paths are read from syntax nodes. Resolving them to modules still
//! depends on the language, source roots, and build configuration.

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
    Terraform,
    /// A bare identifier already in its final form.
    Plain,
    /// A path wrapped in quotes.
    Quoted,
    Dart,
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
        for &(language, grammar, query, dialect) in IMPORT_EXTRACTORS {
            register(&mut languages, language, grammar(), query, dialect);
        }
        Self { languages }
    }

    pub fn shared() -> &'static Self {
        static SHARED: std::sync::LazyLock<ImportExtractor> =
            std::sync::LazyLock::new(ImportExtractor::new);
        &SHARED
    }

    pub fn is_supported(language: Language) -> bool {
        IMPORT_EXTRACTORS
            .iter()
            .any(|definition| definition.0 == language)
    }

    /// What this binary's import extraction would make of a file, as one
    /// value, as [`SymbolExtractor::extraction_digest`] describes it.
    ///
    /// [`SymbolExtractor::extraction_digest`]: crate::services::symbols::SymbolExtractor::extraction_digest
    pub fn extraction_digest() -> u64 {
        let mut described = String::from(env!("CARGO_PKG_VERSION"));
        for &(language, grammar, query, dialect) in IMPORT_EXTRACTORS {
            let grammar = grammar();
            described.push_str(&format!(
                "\u{1}{}:{:?}:{}:{:?}:{query}",
                language.lsp_id(),
                dialect,
                grammar.abi_version(),
                grammar.metadata().map(|metadata| (
                    metadata.major_version,
                    metadata.minor_version,
                    metadata.patch_version
                )),
            ));
        }
        crate::infra::hash_content(&described)
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
            references.extend(references_of(m.captures(), source, entry));
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
    let captured = |name| {
        captures
            .iter()
            .find(|capture| entry.capture_name(capture) == Some(name))
            .map(|capture| capture.node)
    };
    if let Some(module) = captured("module") {
        return vec![format!("self::{}", rust_identifier_name(module, source))];
    }
    let Some(path) = captured("path") else {
        return Vec::new();
    };
    match entry.dialect {
        Dialect::RustUse => use_tree_targets(path, source),
        Dialect::Plain => vec![text_of(path, source).to_string()],
        Dialect::Quoted | Dialect::Dart => {
            if has_unhandled_string_syntax(path)
                || (entry.dialect == Dialect::Dart && !is_plain_dart_string(path))
            {
                return Vec::new();
            }
            vec![unquote(text_of(path, source)).to_string()]
        }
        Dialect::Terraform => {
            if captured("block_type").is_none_or(|block| text_of(block, source) != "module")
                || captured("attribute").is_none_or(|attr| text_of(attr, source) != "source")
                || has_terraform_string_escapes(path, source)
            {
                return Vec::new();
            }
            vec![unquote(text_of(path, source)).to_string()]
        }
    }
}

fn use_tree_targets(node: Node<'_>, source: &[u8]) -> Vec<String> {
    fn join(prefix: &str, tail: &str) -> String {
        if prefix.is_empty() {
            tail.to_string()
        } else if tail.is_empty() || tail == "self" {
            prefix.to_string()
        } else {
            format!("{prefix}::{tail}")
        }
    }

    fn path(node: Node<'_>, source: &[u8]) -> Option<String> {
        match node.kind() {
            "identifier" => Some(rust_identifier_name(node, source).to_string()),
            "crate" | "self" | "super" => Some(text_of(node, source).to_string()),
            "scoped_identifier" => {
                let prefix = match node.child_by_field_name("path") {
                    Some(prefix) => path(prefix, source)?,
                    None => String::new(),
                };
                let name = path(node.child_by_field_name("name")?, source)?;
                Some(join(&prefix, &name))
            }
            _ => None,
        }
    }

    fn walk(node: Node<'_>, prefix: &str, source: &[u8], out: &mut Vec<String>) {
        if node.has_error() || node.is_missing() {
            return;
        }
        match node.kind() {
            "use_list" => {
                let mut cursor = node.walk();
                for child in node.named_children(&mut cursor) {
                    walk(child, prefix, source, out);
                }
            }
            "scoped_use_list" => {
                let head = match node.child_by_field_name("path") {
                    Some(head) => match path(head, source) {
                        Some(head) => head,
                        None => return,
                    },
                    None => String::new(),
                };
                if let Some(list) = node.child_by_field_name("list") {
                    walk(list, &join(prefix, &head), source, out);
                }
            }
            "use_as_clause" => {
                if let Some(aliased) = node.child_by_field_name("path") {
                    walk(aliased, prefix, source, out);
                }
            }
            "use_wildcard" => {
                let mut cursor = node.walk();
                let module = match node
                    .named_children(&mut cursor)
                    .find(|node| !node.is_extra())
                {
                    Some(module) => match path(module, source) {
                        Some(module) => module,
                        None => return,
                    },
                    None => String::new(),
                };
                let target = join(prefix, &module);
                if !target.is_empty() {
                    out.push(target);
                }
            }
            _ => {
                if let Some(target) = path(node, source) {
                    out.push(join(prefix, &target));
                }
            }
        }
    }

    let mut targets = Vec::new();
    walk(node, "", source, &mut targets);
    targets
}

fn unquote(text: &str) -> &str {
    for quote in ['"', '\'', '`'] {
        if let Some(inner) = text
            .strip_prefix(quote)
            .and_then(|inner| inner.strip_suffix(quote))
        {
            return inner;
        }
    }
    text
}

fn has_unhandled_string_syntax(node: Node<'_>) -> bool {
    if matches!(node.kind(), "escape_sequence" | "template_substitution") {
        return true;
    }
    let mut cursor = node.walk();
    node.named_children(&mut cursor)
        .any(has_unhandled_string_syntax)
}

fn is_plain_dart_string(node: Node<'_>) -> bool {
    let mut cursor = node.walk();
    let mut parts = node.named_children(&mut cursor);
    let Some(part) = parts.next() else {
        return false;
    };
    parts.next().is_none()
        && matches!(
            part.kind(),
            "string_literal_single_quotes" | "string_literal_double_quotes"
        )
}

fn has_terraform_string_escapes(node: Node<'_>, source: &[u8]) -> bool {
    let mut cursor = node.walk();
    node.named_children(&mut cursor)
        .filter(|child| child.kind() == "template_literal")
        .any(|literal| {
            let text = text_of(literal, source);
            text.contains('\\') || text.contains("$${") || text.contains("%%{")
        })
}

fn text_of<'a>(node: Node, source: &'a [u8]) -> &'a str {
    node.utf8_text(source).unwrap_or_default()
}

fn rust_identifier_name<'a>(node: Node, source: &'a [u8]) -> &'a str {
    let name = text_of(node, source);
    name.strip_prefix("r#").unwrap_or(name)
}

/// Out-of-line declarations name modules; inline modules add lexical context.
const RUST_QUERY: &str = r#"
(use_declaration argument: (_) @path)
(mod_item name: (identifier) @module !body)
"#;

/// Python from-imports name a module; imported attributes are not assumed to be modules.
const PYTHON_QUERY: &str = r#"
(import_statement name: (dotted_name) @path)
(import_statement name: (aliased_import name: (dotted_name) @path))
(import_from_statement module_name: (_) @path)
"#;

const GO_QUERY: &str = r#"
(import_spec path: [(interpreted_string_literal) (raw_string_literal)] @path)
"#;

const JAVASCRIPT_QUERY: &str = r#"
(import_statement source: (string) @path)
(export_statement source: (string) @path)
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
(import_declaration (identifier) @path)
"#;

const ELIXIR_QUERY: &str = r#"
(alias) @path
"#;

const DART_QUERY: &str = r#"
(import_specification uri: (uri (string_literal) @path))
(import_specification uri: (configurable_uri (uri (string_literal) @path)))
"#;

/// Terraform addresses another module through a block's `source`.
const TERRAFORM_QUERY: &str = r#"
(block
  (identifier) @block_type
  (body
    (attribute
      (identifier) @attribute
      (expression (literal_value (string_lit) @path)))))
"#;

type ImportExtractorDefinition = (
    Language,
    fn() -> tree_sitter::Language,
    &'static str,
    Dialect,
);

const IMPORT_EXTRACTORS: &[ImportExtractorDefinition] = &[
    (
        Language::Rust,
        || tree_sitter_rust::LANGUAGE.into(),
        RUST_QUERY,
        Dialect::RustUse,
    ),
    (
        Language::Python,
        || tree_sitter_python::LANGUAGE.into(),
        PYTHON_QUERY,
        Dialect::Plain,
    ),
    (
        Language::Go,
        || tree_sitter_go::LANGUAGE.into(),
        GO_QUERY,
        Dialect::Quoted,
    ),
    (
        Language::JavaScript,
        || tree_sitter_javascript::LANGUAGE.into(),
        JAVASCRIPT_QUERY,
        Dialect::Quoted,
    ),
    (
        Language::TypeScript,
        || tree_sitter_typescript::LANGUAGE_TSX.into(),
        JAVASCRIPT_QUERY,
        Dialect::Quoted,
    ),
    (
        Language::Java,
        || tree_sitter_java::LANGUAGE.into(),
        JAVA_QUERY,
        Dialect::Plain,
    ),
    (
        Language::Kotlin,
        || tree_sitter_kotlin_sg::LANGUAGE.into(),
        KOTLIN_QUERY,
        Dialect::Plain,
    ),
    (
        Language::Scala,
        || tree_sitter_scala::LANGUAGE.into(),
        SCALA_QUERY,
        Dialect::Plain,
    ),
    (
        Language::Swift,
        || tree_sitter_swift::LANGUAGE.into(),
        SWIFT_QUERY,
        Dialect::Plain,
    ),
    (
        Language::Elixir,
        || tree_sitter_elixir::LANGUAGE.into(),
        ELIXIR_QUERY,
        Dialect::Plain,
    ),
    (
        Language::Dart,
        || tree_sitter_dart::LANGUAGE.into(),
        DART_QUERY,
        Dialect::Dart,
    ),
    (
        Language::Terraform,
        || tree_sitter_hcl::LANGUAGE.into(),
        TERRAFORM_QUERY,
        Dialect::Terraform,
    ),
];

#[cfg(test)]
mod tests {
    use super::*;

    fn extract(content: &str, language: Language) -> Vec<String> {
        ImportExtractor::shared().extract(content, language)
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
        let source = "mod auth;\nmod r#type;\nmod tests {\n    fn helper() {}\n}\n";
        assert_eq!(
            extract(source, Language::Rust),
            vec!["self::auth", "self::type"]
        );
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
    fn javascript_reads_declarations_without_guessing_call_targets() {
        let source = "import { A } from \"./services/pack\";\n\
                      export { B } from \"../shared/util\";\n\
                      import { C } from './he\\u006cper';\n\
                      export { D } from './he\\x6cper';\n\
                      const legacy = require(\"./legacy\");\n\
                      const noise = format(\"./not-an-import\");\n";
        for language in [Language::JavaScript, Language::TypeScript] {
            assert_eq!(
                extract(source, language),
                vec!["./services/pack", "../shared/util"],
                "{language:?}",
            );
        }
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
    fn rust_use_paths_ignore_comments_and_preserve_nested_groups() {
        assert_eq!(
            extract(
                "use crate::{ /* }, fake, { */ api::{self, Client as ApiClient}, store::Record };\n\
                 use super::{helpers::*, prelude::{*}};\n\
                 use crate::r#type::{r#match as local, r#const::*};\n",
                Language::Rust,
            ),
            vec![
                "crate::api",
                "crate::api::Client",
                "crate::store::Record",
                "super::helpers",
                "super::prelude",
                "crate::type::match",
                "crate::type::const",
            ],
        );
    }

    #[test]
    fn terraform_sources_belong_to_module_blocks() {
        let source = r#"
module "network" {
  source = "./modules/network"
  description = "./not-a-module"
}
resource "example" "record" {
  source = "./resource-data"
}
module "wrapper" {
  source = "./wrapper"
  nested { source = "./nested-data" }
}
module "unicode" { source = "./he\u006cper" }
module "interpolation" { source = "./$${helper}" }
module "directive" { source = "./%%{helper}" }
module "ordinary-dollar" { source = "./cash$" }
module "ordinary-percent" { source = "./100%" }
"#;
        assert_eq!(
            extract(source, Language::Terraform),
            vec!["./modules/network", "./wrapper", "./cash$", "./100%"],
        );
    }

    #[test]
    fn go_imports_accept_both_string_literal_forms() {
        assert_eq!(
            extract("package p\nimport `example.com/core`\n", Language::Go),
            vec!["example.com/core"],
        );
        assert!(
            extract(
                "package p\nimport \"example.com/he\\u006cper\"\n",
                Language::Go
            )
            .is_empty()
        );
        assert_eq!(
            extract(
                "package p\nimport `example.com/he\\u006cper`\n",
                Language::Go
            ),
            vec!["example.com/he\\u006cper"],
        );
    }

    #[test]
    fn python_import_aliases_preserve_the_module_path() {
        assert_eq!(
            extract("import os.path as path, json as codec\n", Language::Python),
            vec!["os.path", "json"],
        );
    }

    #[test]
    fn import_modifiers_and_comments_do_not_change_the_path() {
        assert_eq!(
            extract("import Foundation // 'not-an-import'\n", Language::Swift),
            vec!["Foundation"],
        );
        assert_eq!(
            extract(
                "import 'core.dart' as core; // 'not-an-import'\n",
                Language::Dart
            ),
            vec!["core.dart"],
        );
        for source in [
            "import './he\\u006cper.dart';\n",
            "import './${name}.dart';\n",
            "import r'./helper.dart';\n",
            "import './he' 'lper.dart';\n",
            "import '''./helper.dart''';\n",
        ] {
            assert!(extract(source, Language::Dart).is_empty(), "{source}");
        }
        assert_eq!(
            extract(
                "import \"./helper'\";\nexport * from \"./helper`\";\n\
                 import \"./ helper \";\nimport \" leading and trailing \";\n",
                Language::JavaScript,
            ),
            vec![
                "./helper'",
                "./helper`",
                "./ helper ",
                " leading and trailing "
            ],
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
