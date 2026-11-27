//! Static module references read from language grammars.
//!
//! Import paths are read from syntax nodes, never from lines that spell
//! them. Which file a path names still depends on the language, its source
//! roots and its build configuration; `map file` decides that for the tests
//! it lists (`cli/commands/map/tests_of.rs`).

use std::collections::HashMap;
use std::sync::{Mutex, OnceLock};

use streaming_iterator::StreamingIterator;
use tree_sitter::{Node, Parser, Query, QueryCursor};

use crate::models::symbol::Language;

/// A parser and its compiled import query for one language, both built the
/// first time a file of that language is read.
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
    /// A bare identifier already in its final form.
    Plain,
    /// A path wrapped in quotes.
    Quoted,
    Dart,
    /// A Java or Kotlin import: a path of identifiers (which a Kotlin
    /// backtick can quote), and a trailing `*` that imports all a package or
    /// class holds.
    JvmImport,
    /// A Scala import, which spells each path as its segments and can list
    /// several paths and select several names under one.
    ScalaImport,
}

pub struct ImportExtractor {
    languages: HashMap<Language, OnceLock<Option<LanguageEntry>>>,
    /// Python's `from` imports read for the names they take
    /// ([`ImportExtractor::python_module_names`]), beside the module
    /// references `extract` reads.
    python_from: OnceLock<Option<LanguageEntry>>,
}

impl Default for ImportExtractor {
    fn default() -> Self {
        Self::new()
    }
}

impl ImportExtractor {
    pub fn new() -> Self {
        let languages = IMPORT_EXTRACTORS
            .iter()
            .map(|&(language, _, _, _)| (language, OnceLock::new()))
            .collect();
        Self {
            languages,
            python_from: OnceLock::new(),
        }
    }

    /// The parser and query of `language`, compiled the first time it is
    /// asked for ([`compile`]); `None` for a language whose imports this
    /// does not read, or one this build cannot compile.
    fn entry(&self, language: Language) -> Option<&LanguageEntry> {
        self.languages
            .get(&language)?
            .get_or_init(|| {
                let &(_, grammar, query, dialect) = IMPORT_EXTRACTORS
                    .iter()
                    .find(|(registered, _, _, _)| *registered == language)?;
                compile(grammar(), query, dialect)
            })
            .as_ref()
    }

    pub fn shared() -> &'static Self {
        static SHARED: std::sync::LazyLock<ImportExtractor> =
            std::sync::LazyLock::new(ImportExtractor::new);
        &SHARED
    }

    /// Every module name a Python file's imports may name: the module each
    /// import and `from` import names, as [`extract`](Self::extract) reads
    /// them, and each name a `from` import takes from its module, spelled as
    /// the submodule it would be — `from pkg import a, b as c` reads `pkg`,
    /// `pkg.a` and `pkg.b`, `from . import a` reads `.` and `.a`. Whether a
    /// taken name is a submodule or a value its module defines is the files'
    /// to say; `*` takes none. The file is parsed once for both.
    pub fn python_module_names(&self, content: &str) -> Vec<String> {
        let (Some(modules), Some(taken)) = (
            self.entry(Language::Python),
            self.python_from
                .get_or_init(|| {
                    compile(
                        tree_sitter_python::LANGUAGE.into(),
                        PYTHON_FROM_NAMES_QUERY,
                        Dialect::Plain,
                    )
                })
                .as_ref(),
        ) else {
            return Vec::new();
        };
        let tree = {
            let Ok(mut parser) = modules.parser.lock() else {
                return Vec::new();
            };
            match parser.parse(content, None) {
                Some(tree) => tree,
                None => return Vec::new(),
            }
        };
        let source = content.as_bytes();
        let mut names = Vec::new();
        let mut cursor = QueryCursor::new();
        let mut matches = cursor.matches(&modules.query, tree.root_node(), source);
        while let Some(m) = matches.next() {
            names.extend(references_of(m.captures(), source, modules));
        }
        let mut cursor = QueryCursor::new();
        let mut matches = cursor.matches(&taken.query, tree.root_node(), source);
        while let Some(m) = matches.next() {
            let captured = |name| {
                m.captures()
                    .iter()
                    .find(|capture| taken.capture_name(capture) == Some(name))
                    .map(|capture| text_of(capture.node, source))
            };
            let (Some(module), Some(name)) = (captured("module"), captured("name")) else {
                continue;
            };
            names.push(if module.ends_with('.') {
                format!("{module}{name}")
            } else {
                format!("{module}.{name}")
            });
        }
        names
    }

    /// The package `content` declares and the module references it makes.
    pub fn extract(&self, content: &str, language: Language) -> FileImports {
        let mut imports = FileImports::default();
        let Some(entry) = self.entry(language) else {
            return imports;
        };
        let tree = {
            let Ok(mut parser) = entry.parser.lock() else {
                return imports;
            };
            match parser.parse(content, None) {
                Some(tree) => tree,
                None => return imports,
            }
        };

        let source = content.as_bytes();
        let mut cursor = QueryCursor::new();
        let mut matches = cursor.matches(&entry.query, tree.root_node(), source);
        while let Some(m) = matches.next() {
            let captured = |name| {
                m.captures()
                    .iter()
                    .find(|capture| entry.capture_name(capture) == Some(name))
            };
            if let Some(name) = captured("declares") {
                imports
                    .declares
                    .push(backquoted_name(name.node, source).to_string());
                continue;
            }
            if let Some(name) = captured("member") {
                imports
                    .members
                    .push(backquoted_name(name.node, source).to_string());
                continue;
            }
            if let Some(package) = captured("package") {
                // Scala chains its package clauses: `package a.b` then
                // `package c` declares `a.b.c`.
                let name = jvm_name(package.node, source);
                imports.package = Some(match imports.package.take() {
                    Some(outer) => format!("{outer}.{name}"),
                    None => name,
                });
                continue;
            }
            let references = references_of(m.captures(), source, entry);
            if captured("required").is_some() {
                imports.requires.extend(references);
                continue;
            }
            imports.references.extend(references);
        }
        imports
    }
}

/// What a file's imports say: the package it declares itself in, where its
/// language names one in the file (`package com.acme.app`), the names it
/// declares at its top level, which an import of that package names it by —
/// the ones that can hold what a longer import names through them (a JVM
/// class, object or Scala `val`), and the functions and properties, which an
/// import names only whole — and the module references it makes, in source
/// order: those written as imports, and apart from them those a TypeScript
/// `import x = require("…")` writes.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct FileImports {
    pub package: Option<String>,
    pub declares: Vec<String>,
    pub members: Vec<String>,
    pub references: Vec<String>,
    pub requires: Vec<String>,
}

fn compile(
    ts_language: tree_sitter::Language,
    query_src: &str,
    dialect: Dialect,
) -> Option<LanguageEntry> {
    // The grammar and its query are compiled into the binary, so
    // compilation is deterministic for a given build. A failure means a
    // defective grammar — an incompatible ABI, or a query whose node types
    // moved — so that one language loses its references rather than taking
    // the rest down with it. The coverage test below fails loudly when one goes
    // missing.
    let mut parser = Parser::new();
    parser.set_language(&ts_language).ok()?;
    let query = Query::new(&ts_language, query_src).ok()?;
    Some(LanguageEntry {
        parser: Mutex::new(parser),
        query,
        dialect,
    })
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
    let Some(path) = captured("path") else {
        return Vec::new();
    };
    match entry.dialect {
        Dialect::JvmImport => {
            let mut name = None;
            let mut wildcard = false;
            let mut cursor = path.walk();
            for child in path.named_children(&mut cursor) {
                match child.kind() {
                    "scoped_identifier" | "identifier" if name.is_none() => {
                        name = Some(jvm_name(child, source));
                    }
                    "asterisk" | "wildcard_import" => wildcard = true,
                    _ => {}
                }
            }
            name.filter(|name| !name.is_empty())
                .map(|name| if wildcard { format!("{name}.*") } else { name })
                .into_iter()
                .collect()
        }
        Dialect::ScalaImport => scala_import_targets(path, source),
        Dialect::Plain => vec![text_of(path, source).to_string()],
        Dialect::Quoted | Dialect::Dart => {
            if has_unhandled_string_syntax(path)
                || (entry.dialect == Dialect::Dart && !is_plain_dart_string(path))
            {
                return Vec::new();
            }
            let text = text_of(path, source);
            let raw = text
                .strip_prefix('r')
                .filter(|_| entry.dialect == Dialect::Dart);
            vec![unquote(raw.unwrap_or(text)).to_string()]
        }
    }
}

/// The modules a Scala import names: each path it lists (`import a.b, c.d`),
/// each name its braces select under a path (`import a.b.{C, D => E}`), the
/// name it renames (`import a.b.C as D`), and everything under the path
/// (`a.b.*`) where a wildcard or `given` takes what is in it. A leading
/// `_root_` names the root package rather than a package in it.
fn scala_import_targets(import: Node<'_>, source: &[u8]) -> Vec<String> {
    let mut targets = Vec::new();
    let mut path: Vec<&str> = Vec::new();
    let mut cursor = import.walk();
    for (at, child) in import.children(&mut cursor).enumerate() {
        if child.is_named() && import.field_name_for_child(at as u32) == Some("path") {
            let segment = backquoted_name(child, source);
            if !(path.is_empty() && segment == "_root_") {
                path.push(segment);
            }
            continue;
        }
        match child.kind() {
            "," if !path.is_empty() => targets.push(path.join(".")),
            "namespace_wildcard" => targets.push(format!("{}.*", path.join("."))),
            "as_renamed_identifier" => {
                targets.extend(
                    child.child_by_field_name("name").map(|name| {
                        format!("{}.{}", path.join("."), backquoted_name(name, source))
                    }),
                );
            }
            "namespace_selectors" => {
                let mut cursor = child.walk();
                for selector in child.named_children(&mut cursor) {
                    let name = match selector.kind() {
                        "identifier" => Some(selector),
                        "arrow_renamed_identifier" | "as_renamed_identifier" => {
                            selector.child_by_field_name("name")
                        }
                        _ => None,
                    };
                    targets.push(match name {
                        Some(name) => {
                            format!("{}.{}", path.join("."), backquoted_name(name, source))
                        }
                        None => format!("{}.*", path.join(".")),
                    });
                }
            }
            _ => continue,
        }
        path.clear();
    }
    if !path.is_empty() {
        targets.push(path.join("."));
    }
    targets
}

/// The dotted name a JVM identifier path spells, read from its identifiers so
/// that neither whitespace nor a comment between them becomes part of a name.
fn jvm_name(path: Node<'_>, source: &[u8]) -> String {
    fn segments<'a>(node: Node<'_>, source: &'a [u8], out: &mut Vec<&'a str>) {
        if matches!(node.kind(), "identifier" | "simple_identifier")
            && node.named_child_count() == 0
        {
            out.push(backquoted_name(node, source));
            return;
        }
        let mut cursor = node.walk();
        for child in node.named_children(&mut cursor) {
            segments(child, source, out);
        }
    }
    let mut out = Vec::new();
    segments(path, source, &mut out);
    out.join(".")
}

/// The name a Kotlin or Scala identifier spells: backticks quote a name, so
/// `` `type` `` is `type`.
fn backquoted_name<'a>(identifier: Node<'_>, source: &'a [u8]) -> &'a str {
    let text = text_of(identifier, source);
    text.strip_prefix('`')
        .and_then(|name| name.strip_suffix('`'))
        .unwrap_or(text)
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
            "string_literal_single_quotes"
                | "string_literal_double_quotes"
                | "raw_string_literal_single_quotes"
                | "raw_string_literal_double_quotes"
        )
}

fn text_of<'a>(node: Node, source: &'a [u8]) -> &'a str {
    node.utf8_text(source).unwrap_or_default()
}

/// Python from-imports name a module; imported attributes are not assumed to be modules.
const PYTHON_QUERY: &str = r#"
(import_statement name: (dotted_name) @path)
(import_statement name: (aliased_import name: (dotted_name) @path))
(import_from_statement module_name: (_) @path)
"#;

/// The names a `from` import takes, beside the module it takes them from.
const PYTHON_FROM_NAMES_QUERY: &str = r#"
(import_from_statement module_name: (_) @module name: (dotted_name) @name)
(import_from_statement module_name: (_) @module name: (aliased_import name: (dotted_name) @name))
"#;

const GO_QUERY: &str = r#"
(import_spec path: [(interpreted_string_literal) (raw_string_literal)] @path)
"#;

const JAVASCRIPT_QUERY: &str = r#"
(import_statement source: (string) @path)
(export_statement source: (string) @path)
"#;

/// TypeScript also imports a CommonJS module by name, `import x =
/// require("./m")`, which it resolves as a `require` whatever the file's
/// module format.
const TYPESCRIPT_QUERY: &str = r#"
(import_statement source: (string) @path)
(import_statement (import_require_clause source: (string) @path) @required)
(export_statement source: (string) @path)
"#;

/// A JVM file declares the package it belongs to and the names it holds at
/// its top level, which together are what an import names it by, and imports
/// classes and whole packages by those names.
const JAVA_QUERY: &str = r#"
(package_declaration [(scoped_identifier) (identifier)] @package)
(program (class_declaration name: (identifier) @declares))
(program (interface_declaration name: (identifier) @declares))
(program (enum_declaration name: (identifier) @declares))
(program (record_declaration name: (identifier) @declares))
(program (annotation_type_declaration name: (identifier) @declares))
(import_declaration) @path
"#;

/// A Kotlin file declares classes, objects and type aliases at its top
/// level, and functions and properties, which are imported by their whole
/// name — Kotlin reads an import's qualifier as a package or a class, never
/// a function or property.
const KOTLIN_QUERY: &str = r#"
(package_header (identifier) @package)
(source_file (class_declaration (type_identifier) @declares))
(source_file (object_declaration (type_identifier) @declares))
(source_file (type_alias (type_identifier) @declares))
(source_file (function_declaration (simple_identifier) @member))
(source_file (property_declaration (variable_declaration (simple_identifier) @member)))
(import_header) @path
"#;

/// A package clause with a body declares a package for that body alone, so
/// only what the compilation unit holds itself is in the file's package. An
/// import reaches through a stable path — a package, an object, a `val` —
/// so a `def`, `var`, `given` or extension method is imported only whole.
const SCALA_QUERY: &str = r#"
(compilation_unit (package_clause name: (package_identifier) @package !body))
(compilation_unit (class_definition name: (identifier) @declares))
(compilation_unit (trait_definition name: (identifier) @declares))
(compilation_unit (object_definition name: (identifier) @declares))
(compilation_unit (enum_definition name: (identifier) @declares))
(compilation_unit (package_object name: (identifier) @declares))
(compilation_unit (type_definition name: (type_identifier) @declares))
(compilation_unit (function_definition name: (identifier) @member))
(compilation_unit (given_definition name: (identifier) @member))
(compilation_unit (val_definition pattern: (identifier) @declares))
(compilation_unit (var_definition pattern: (identifier) @member))
(compilation_unit (extension_definition body: (function_definition name: (identifier) @member)))
(import_declaration) @path
"#;

/// A library names another through its imports and exports, and its parts
/// and the library a part belongs to through `part` and `part of`. The grammar
/// writes a deferred import's URI without the `configurable_uri` around it.
const DART_QUERY: &str = r#"
(import_specification uri: (uri (string_literal) @path))
(import_specification uri: (configurable_uri (uri (string_literal) @path)))
(import_specification uri: (configurable_uri (configuration_uri (uri (string_literal) @path))))
(library_export uri: (configurable_uri (uri (string_literal) @path)))
(library_export uri: (configurable_uri (configuration_uri (uri (string_literal) @path))))
(part_directive uri: (uri (string_literal) @path))
(part_of_directive (uri (string_literal) @path))
"#;

type ImportExtractorDefinition = (
    Language,
    fn() -> tree_sitter::Language,
    &'static str,
    Dialect,
);

/// The languages whose tests `map file` reads from their imports. Elixir and
/// Swift are absent on purpose: neither ties a module's name to the files
/// that define it — Mix compiles every file under `lib/`, and a Swift module
/// is whatever files a package manifest or an Xcode project assigns it — so
/// a reference to one could only land on a file by a guess. Rust's tests sit
/// in the file they test, which `infra::ast::test_regions` reads instead.
const IMPORT_EXTRACTORS: &[ImportExtractorDefinition] = &[
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
        TYPESCRIPT_QUERY,
        Dialect::Quoted,
    ),
    (
        Language::Java,
        || tree_sitter_java::LANGUAGE.into(),
        JAVA_QUERY,
        Dialect::JvmImport,
    ),
    (
        Language::Kotlin,
        || tree_sitter_kotlin_sg::LANGUAGE.into(),
        KOTLIN_QUERY,
        Dialect::JvmImport,
    ),
    (
        Language::Scala,
        || tree_sitter_scala::LANGUAGE.into(),
        SCALA_QUERY,
        Dialect::ScalaImport,
    ),
    (
        Language::Dart,
        || tree_sitter_dart::LANGUAGE.into(),
        DART_QUERY,
        Dialect::Dart,
    ),
];

#[cfg(test)]
mod tests {
    use super::*;

    fn extract(content: &str, language: Language) -> Vec<String> {
        ImportExtractor::shared()
            .extract(content, language)
            .references
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

        let ts = "import { A } from \"./real\";\nconst s = \"./fake\";\n";
        assert_eq!(extract(ts, Language::TypeScript), vec!["./real"]);
    }

    /// An import spanning lines is one node, so nothing has to be stitched
    /// back together.
    #[test]
    fn an_import_spanning_lines_is_read_whole() {
        let source = "import {\n  Section,\n  SymbolOutput,\n} from \"./response\";\n";
        assert_eq!(extract(source, Language::TypeScript), vec!["./response"]);
        let source = "from app.response import (\n    Section,\n    SymbolOutput,\n)\n";
        assert_eq!(extract(source, Language::Python), vec!["app.response"]);
    }

    /// An import written inside a comment or a string is not a node.
    #[test]
    fn an_import_that_is_not_code_is_not_an_import() {
        let source = "# import commented.out\n\
                      \"\"\"\nimport documented.example\n\"\"\"\n\
                      S = \"from quoted import path\"\n\
                      import real.one\n";
        assert_eq!(extract(source, Language::Python), vec!["real.one"]);
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
        // TypeScript's `import x = require()` is a reference of its own form,
        // which resolves as a `require`.
        let required = ImportExtractor::shared().extract(
            "import legacy = require(\"./legacy\");\nimport m from \"./m\";\n",
            Language::TypeScript,
        );
        assert_eq!(
            (required.references, required.requires),
            (vec!["./m".to_string()], vec!["./legacy".to_string()])
        );
    }

    #[test]
    fn jvm_languages_read_their_package_paths() {
        assert_eq!(
            extract("package a;\nimport com.acme.core.Thing;\n", Language::Java),
            vec!["com.acme.core.Thing"]
        );
        assert_eq!(
            extract(
                "import com.acme.core.Thing\nimport `when`.x.`in`\n",
                Language::Kotlin
            ),
            vec!["com.acme.core.Thing", "when.x.in"]
        );
        assert_eq!(
            extract(
                "import com.acme.util.*;\nimport static com.acme.util.Strings.*;\n\
                 import static com.acme.util.Strings.join;\n",
                Language::Java
            ),
            vec![
                "com.acme.util.*",
                "com.acme.util.Strings.*",
                "com.acme.util.Strings.join"
            ]
        );
        assert_eq!(
            extract("import a.b.*\nimport a.b.C as D\n", Language::Kotlin),
            vec!["a.b.*", "a.b.C"]
        );
        assert_eq!(
            extract(
                "import com.acme.core.Thing\nimport a.b.{C, D => E, given}, x.y._\n\
                 import p.q.*\nimport p.q.given\n",
                Language::Scala
            ),
            vec![
                "com.acme.core.Thing",
                "a.b.C",
                "a.b.D",
                "a.b.*",
                "x.y.*",
                "p.q.*",
                "p.q.*"
            ]
        );
        assert_eq!(
            extract(
                "import a.b.C as D\nimport a.b.{E as F, G}\nimport _root_.scala.util\n\
                 import a.`type`.H\n",
                Language::Scala
            ),
            vec!["a.b.C", "a.b.E", "a.b.G", "scala.util", "a.type.H"]
        );
    }

    /// A JVM file's package is what its declaration names — Scala's chained
    /// clauses together, a clause with a body declaring only for the body —
    /// and a file without one is in the unnamed package.
    #[test]
    fn jvm_languages_read_the_package_a_file_declares() {
        let package =
            |content: &str, language| ImportExtractor::shared().extract(content, language).package;
        for (language, content) in [
            (
                Language::Java,
                "package com . acme /* app */ . app;\nimport a.B;\n",
            ),
            (Language::Kotlin, "package com.acme.`app`\n\nimport a.B\n"),
            (
                Language::Scala,
                "package com.acme\npackage app\n\nimport a.B\npackage inner { class C }\n",
            ),
        ] {
            assert_eq!(
                package(content, language).as_deref(),
                Some("com.acme.app"),
                "{language:?}"
            );
        }
        assert_eq!(
            package("package app;\n", Language::Java).as_deref(),
            Some("app")
        );
        assert_eq!(package("import a.B;\nclass C {}\n", Language::Java), None);
        assert_eq!(package("package app\n", Language::Python), None);
    }

    /// A JVM file declares at its top level what an import of its package
    /// names it by — classes of every kind, objects, and in Kotlin and Scala
    /// functions and values too, apart from the names an import can reach
    /// through — and never what those declarations nest.
    #[test]
    fn jvm_languages_read_the_names_a_file_declares_at_its_top_level() {
        let declares = |content: &str, language| {
            let imports = ImportExtractor::shared().extract(content, language);
            assert!(
                language != Language::Java || imports.members.is_empty(),
                "{content}"
            );
            imports.declares
        };
        let members =
            |content: &str, language| ImportExtractor::shared().extract(content, language).members;
        assert_eq!(
            declares(
                "package a;\npublic class C { class Inner {} void m() {} }\ninterface I {}\n\
                 enum E { A }\nrecord R(int x) {}\n@interface N {}\n",
                Language::Java
            ),
            vec!["C", "I", "E", "R", "N"]
        );
        assert_eq!(
            declares(
                "package a\nclass C {\n    class Inner\n    fun m() {}\n}\ninterface I\n\
                 object O {\n    val inner = 1\n}\nenum class E { A }\ntypealias T = Int\nfun f() {}\n\
                 fun String.ext() = 1\nval p = 1\nvar `q` = 2\n",
                Language::Kotlin
            ),
            vec!["C", "I", "O", "E", "T"]
        );
        assert_eq!(
            members(
                "package a\nclass C {\n    fun m() {}\n}\nobject O {\n    val inner = 1\n}\n\
                 fun f() {}\nfun String.ext() = 1\nval p = 1\nvar `q` = 2\n",
                Language::Kotlin
            ),
            vec!["f", "ext", "p", "q"]
        );
        assert_eq!(
            declares(
                "package a\nclass C { def m = 1 }\ntrait T\nobject O\ncase class D(x: Int)\n\
                 enum E { case A }\npackage object po { def z = 1 }\ntype Al = Int\ndef f = 1\n\
                 given g: Int = 1\nval v = 1\nvar w = 2\nextension (s: String) def ex = 1\n\
                 package inner { class Hidden }\n",
                Language::Scala
            ),
            vec!["C", "T", "O", "D", "E", "po", "Al", "v"]
        );
        assert_eq!(
            members(
                "package a\nclass C { def m = 1 }\ndef f = 1\ngiven g: Int = 1\nval v = 1\n\
                 var w = 2\nextension (s: String) def ex = 1\n",
                Language::Scala
            ),
            vec!["f", "g", "w", "ex"]
        );
    }

    #[test]
    fn a_dart_library_names_its_imports_exports_and_parts() {
        assert_eq!(
            extract(
                "import 'a.dart';\nimport 'b.dart' if (dart.library.io) 'c.dart';\n\
                 import 'd.dart' deferred as d;\nexport 'e.dart' show X;\npart 'p.dart';\n",
                Language::Dart
            ),
            vec!["a.dart", "b.dart", "c.dart", "d.dart", "e.dart", "p.dart"]
        );
        assert_eq!(
            extract("import r'raw.dart';\n", Language::Dart),
            vec!["raw.dart"]
        );
        assert_eq!(
            extract("part of 'lib.dart';\n", Language::Dart),
            vec!["lib.dart"]
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

    /// The names a `from` import takes are read beside its module, so a
    /// submodule imported from its package (`from pkg import mod`) can be
    /// told from the package; a wildcard takes none. The modules are the
    /// references `extract` reads.
    #[test]
    fn python_from_names_are_spelled_as_the_submodules_they_would_be() {
        let source = "from pkg import a, b as c\nfrom . import d\nfrom ..up import (e,\n    f)\n\
                      from star import *\nimport plain\ndef g():\n    from local import h\n";
        assert_eq!(
            ImportExtractor::shared().python_module_names(source),
            vec![
                "pkg", ".", "..up", "star", "plain", "local", "pkg.a", "pkg.b", ".d", "..up.e",
                "..up.f", "local.h"
            ],
        );
        assert_eq!(
            ImportExtractor::shared().python_module_names(source)[..6],
            extract(source, Language::Python)[..]
        );
        let extractor = ImportExtractor::new();
        extractor.python_module_names("");
        let entry = extractor.python_from.get().unwrap().as_ref().unwrap();
        let tree = entry.parser.lock().unwrap().parse(source, None).unwrap();
        let mut matched = vec![false; entry.query.pattern_count()];
        let mut cursor = QueryCursor::new();
        let mut matches = cursor.matches(&entry.query, tree.root_node(), source.as_bytes());
        while let Some(found) = matches.next() {
            matched[found.pattern_index] = true;
        }
        assert!(matched.iter().all(|m| *m), "{matched:?}");
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
            extract(
                "import 'core.dart' as core; // 'not-an-import'\n",
                Language::Dart
            ),
            vec!["core.dart"],
        );
        for source in [
            "import './he\\u006cper.dart';\n",
            "import './${name}.dart';\n",
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

    /// A pattern that matches nothing reads no edge and passes every test of
    /// the edges read, so each pattern of each query matches a form its
    /// language's fixture writes.
    const IMPORT_FIXTURES: &[(Language, &str)] = &[
        (
            Language::Python,
            "import os.path\nimport numpy.linalg as la\nfrom . import x\n",
        ),
        (
            Language::Go,
            "package p\n\nimport (\n\t\"fmt\"\n\t`raw/path`\n)\n",
        ),
        (
            Language::JavaScript,
            "import a from \"./a\";\nexport * from \"./b\";\n",
        ),
        (
            Language::TypeScript,
            "import a from \"./a\";\nimport r = require(\"./r\");\nexport * from \"./b\";\n",
        ),
        (
            Language::Java,
            "package a.b;\nimport java.util.List;\nclass C {}\ninterface I {}\nenum E { A }\n\
             record R(int x) {}\n@interface N {}\n",
        ),
        (
            Language::Kotlin,
            "package a.b\nimport kotlin.collections.List\nclass C\nobject O\n\
             typealias T = Int\nfun f() {}\nval p = 1\n",
        ),
        (
            Language::Scala,
            "package a.b\nimport scala.collection.mutable\nclass C\ntrait T\nobject O\n\
             enum E { case A }\npackage object po {}\ntype A = Int\ndef f = 1\ngiven g: Int = 1\n\
             val v = 1\nvar w = 2\nextension (s: String) def x = 1\n",
        ),
        (
            Language::Dart,
            "import 'package:a/a.dart' if (dart.library.io) 'io.dart';\n\
             import 'd.dart' deferred as d;\n\
             export 'e.dart' if (x) 'f.dart';\npart 'p.dart';\npart of 'lib.dart';\n",
        ),
    ];

    #[test]
    fn every_import_pattern_reads_a_form_its_fixture_writes() {
        let extractor = ImportExtractor::new();
        let mut unread = Vec::new();
        for (language, _, query_src, _) in IMPORT_EXTRACTORS {
            let entry = extractor.entry(*language).unwrap();
            let (_, source) = IMPORT_FIXTURES
                .iter()
                .find(|(fixture, _)| fixture == language)
                .unwrap_or_else(|| panic!("{language:?} has no import fixture"));
            let tree = entry.parser.lock().unwrap().parse(source, None).unwrap();
            let mut matched = vec![false; entry.query.pattern_count()];
            let mut cursor = QueryCursor::new();
            let mut matches = cursor.matches(&entry.query, tree.root_node(), source.as_bytes());
            while let Some(found) = matches.next() {
                matched[found.pattern_index] = true;
            }
            unread.extend(
                (0..entry.query.pattern_count())
                    .filter(|&p| !matched[p])
                    .map(|p| {
                        let text = query_src[entry.query.start_byte_for_pattern(p)..]
                            .lines()
                            .next()
                            .unwrap_or_default();
                        format!("{language:?}: {text}")
                    }),
            );
        }
        assert!(
            unread.is_empty(),
            "patterns no fixture form matches: {unread:#?}"
        );
    }

    #[test]
    fn every_language_map_reads_is_registered() {
        let extractor = ImportExtractor::new();
        for language in [
            Language::Python,
            Language::Go,
            Language::JavaScript,
            Language::TypeScript,
            Language::Java,
            Language::Kotlin,
            Language::Scala,
            Language::Dart,
        ] {
            assert!(
                extractor.entry(language).is_some(),
                "{language:?} lost its import query — a defective grammar or a moved node type"
            );
        }
    }

    #[test]
    fn an_unregistered_language_yields_nothing() {
        assert!(extract("#include <vector>\n", Language::Cpp).is_empty());
        assert!(extract("use crate::cli::output;\n", Language::Rust).is_empty());
        // Neither ties a module's name to a file, so neither is read: a
        // reference to one could only land on a file by a guess.
        assert!(extract("alias App.Repo\nimport Ecto.Query\n", Language::Elixir).is_empty());
        assert!(extract("import Foundation\n", Language::Swift).is_empty());
    }
}
