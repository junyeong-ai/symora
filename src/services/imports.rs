//! Static module references read from language grammars.
//!
//! Import paths are read from syntax nodes. Resolving them to modules still
//! depends on the language, source roots, and build configuration.

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
    /// A Rust use tree, whose braces name several targets at once.
    RustUse,
    Terraform,
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
        Self { languages }
    }

    /// The parser and query of `language`, compiled the first time it is
    /// asked for ([`compile`]); `None` for a language pack reads no imports
    /// of, or one this build cannot compile.
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
        let mut rust = Vec::new();
        let mut named_class = None;
        let mut callables = false;
        while let Some(m) = matches.next() {
            let captured = |name| {
                m.captures()
                    .iter()
                    .find(|capture| entry.capture_name(capture) == Some(name))
            };
            if let Some(module) = captured("declared") {
                let Some(item) = module.node.parent() else {
                    continue;
                };
                let (scope, in_block) = rust_scope(item, source);
                if in_block {
                    continue;
                }
                let declaration = ModuleDeclaration {
                    scope,
                    name: rust_identifier_name(module.node, source).to_string(),
                    inline: item.child_by_field_name("body").is_some(),
                    path: rust_module_path(item, source),
                };
                if !declaration.inline {
                    rust.push(RustReference::Module {
                        scope: declaration.scope.clone(),
                        name: declaration.name.clone(),
                    });
                }
                imports.modules.push(declaration);
                continue;
            }
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
                callables |= captured("callable").is_some();
                continue;
            }
            if let Some(name) = captured("jvm_name") {
                named_class = named_class.or(Some(kotlin_string(name.node, source)));
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
            match captured("path") {
                Some(path) if entry.dialect == Dialect::RustUse => {
                    let (scope, _) = rust_scope(path.node, source);
                    rust.extend(references.into_iter().map(|target| RustReference::Use {
                        scope: scope.clone(),
                        target,
                    }));
                }
                _ => imports.references.extend(references),
            }
        }
        imports.references.extend(
            rust.into_iter()
                .map(|reference| reference.relative_to_the_file(&imports.modules)),
        );
        // A `@file:JvmName` whose string is no plain text names a class this
        // reading cannot spell.
        imports.file_class = match (callables, named_class) {
            (false, _) | (true, Some(None)) => None,
            (true, None) => Some(FileClass::OfFileName),
            (true, Some(Some(name))) => Some(FileClass::Named(name)),
        };
        imports
    }
}

/// The value a Kotlin string literal spells, where it is plain text: no
/// template, no escape.
fn kotlin_string(literal: Node<'_>, source: &[u8]) -> Option<String> {
    let mut cursor = literal.walk();
    let mut parts = literal.named_children(&mut cursor);
    // The grammar keeps an escape inside the text it is part of.
    match (parts.next(), parts.next()) {
        (Some(part), None) if part.kind() == "string_content" => {
            let text = text_of(part, source);
            (!text.contains('\\')).then(|| text.to_string())
        }
        _ => None,
    }
}

/// A module reference a Rust file makes, with the inline modules it is
/// written in.
enum RustReference {
    /// A `use` path.
    Use { scope: Vec<String>, target: String },
    /// A module declared without a body, which names its file.
    Module { scope: Vec<String>, name: String },
}

impl RustReference {
    /// The reference as written from the file's own module: `self::` and
    /// `super::` hops, and a path from a module declared where the reference
    /// is written, taken through the inline modules enclosing it, so that
    /// `super::x` in `mod a { mod b { … } }` reads `self::a::x`. Any other
    /// path is a `crate::` path or one from an extern crate's name.
    fn relative_to_the_file(self, declarations: &[ModuleDeclaration]) -> String {
        let (scope, segments) = match &self {
            Self::Module { scope, name } => (scope, vec![name.as_str()]),
            Self::Use { scope, target } => (scope, target.split("::").collect()),
        };
        let within = |kept: &[String], rest: &[&str]| {
            std::iter::once("self")
                .chain(kept.iter().map(String::as_str))
                .chain(rest.iter().copied())
                .collect::<Vec<_>>()
                .join("::")
        };
        match segments.first().copied() {
            Some("self" | "super") => {
                let hops = segments
                    .iter()
                    .take_while(|segment| matches!(**segment, "self" | "super"))
                    .count();
                let up = segments[..hops]
                    .iter()
                    .filter(|segment| **segment == "super")
                    .count();
                if up <= scope.len() {
                    within(&scope[..scope.len() - up], &segments[hops..])
                } else {
                    std::iter::repeat_n("super", up - scope.len())
                        .chain(segments[hops..].iter().copied())
                        .collect::<Vec<_>>()
                        .join("::")
                }
            }
            Some(first)
                if matches!(self, Self::Module { .. })
                    || declarations
                        .iter()
                        .any(|declared| declared.scope == *scope && declared.name == first) =>
            {
                within(scope, &segments)
            }
            _ => segments.join("::"),
        }
    }
}

/// The inline modules enclosing a Rust node, outermost first, and whether a
/// block (a function's body) encloses it.
fn rust_scope(node: Node<'_>, source: &[u8]) -> (Vec<String>, bool) {
    let mut scope = Vec::new();
    let mut in_block = false;
    let mut current = node.parent();
    while let Some(ancestor) = current {
        match ancestor.kind() {
            "block" => in_block = true,
            "declaration_list" => {
                if let Some(name) = ancestor
                    .parent()
                    .filter(|item| item.kind() == "mod_item")
                    .and_then(|item| item.child_by_field_name("name"))
                {
                    scope.push(rust_identifier_name(name, source).to_string());
                }
            }
            _ => {}
        }
        current = ancestor.parent();
    }
    scope.reverse();
    (scope, in_block)
}

/// Where the attributes written before a `mod` item put its file: the
/// first `path` attribute among them, or none; a `cfg_attr` that can set
/// one first leaves it open.
fn rust_module_path(item: Node<'_>, source: &[u8]) -> ModulePath {
    let mut attributes = Vec::new();
    let mut sibling = item.prev_named_sibling();
    while let Some(node) = sibling {
        match node.kind() {
            "attribute_item" => attributes.extend(node.named_child(0)),
            "line_comment" | "block_comment" => {}
            _ => break,
        }
        sibling = node.prev_named_sibling();
    }
    for attribute in attributes.into_iter().rev() {
        let Some(name) = attribute
            .named_child(0)
            .filter(|name| name.kind() == "identifier")
        else {
            continue;
        };
        match text_of(name, source) {
            "path" => {
                return attribute
                    .child_by_field_name("value")
                    .and_then(|value| rust_string(value, source))
                    .map_or(ModulePath::Undetermined, ModulePath::Attribute);
            }
            "cfg_attr" => {
                let sets_path =
                    attribute
                        .child_by_field_name("arguments")
                        .is_some_and(|arguments| {
                            let mut cursor = arguments.walk();
                            arguments.named_children(&mut cursor).any(|argument| {
                                argument.kind() == "identifier"
                                    && text_of(argument, source) == "path"
                            })
                        });
                if sets_path {
                    return ModulePath::Undetermined;
                }
            }
            _ => {}
        }
    }
    ModulePath::Default
}

/// The value a Rust string literal spells, where it carries no escape.
fn rust_string(literal: Node<'_>, source: &[u8]) -> Option<String> {
    if !matches!(literal.kind(), "string_literal" | "raw_string_literal") {
        return None;
    }
    let mut cursor = literal.walk();
    let mut value = String::new();
    for part in literal.named_children(&mut cursor) {
        match part.kind() {
            "string_content" => value.push_str(text_of(part, source)),
            _ => return None,
        }
    }
    Some(value)
}

/// What a file's imports say: the package it declares itself in, where its
/// language names one in the file (`package com.acme.app`), the names it
/// declares at its top level, which an import of that package names it by —
/// the ones that can hold what a longer import names through them (a JVM
/// class, object or Scala `val`), and the functions and properties, which an
/// import names only whole — the class its top-level functions and
/// properties compile into, the modules it declares (a Rust `mod`), and the
/// module references it makes, in source order: those written as imports,
/// and apart from them those a TypeScript `import x = require("…")` writes.
#[derive(Debug, Clone, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct FileImports {
    pub package: Option<String>,
    pub declares: Vec<String>,
    pub members: Vec<String>,
    pub file_class: Option<FileClass>,
    pub modules: Vec<ModuleDeclaration>,
    pub references: Vec<String>,
    pub requires: Vec<String>,
}

/// The JVM class a Kotlin file's top-level functions and properties compile
/// into, which Java imports them through; a file declaring none has none.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum FileClass {
    /// The one its `@file:JvmName("…")` names.
    Named(String),
    /// The one Kotlin names for the file.
    OfFileName,
}

/// A module a Rust file declares, `mod name;` or `mod name { … }`, outside
/// any block: one a block declares is no module of the file's tree.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct ModuleDeclaration {
    /// The inline modules the declaration is written in, outermost first.
    pub scope: Vec<String>,
    pub name: String,
    /// Whether its body is written inline rather than in a file of its own.
    pub inline: bool,
    pub path: ModulePath,
}

/// Where a module's file, or an inline module's directory, is.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum ModulePath {
    /// Where Rust's default rules put it.
    Default,
    /// Where its first `#[path = "…"]` attribute puts it.
    Attribute(String),
    /// Where its source does not say: a path a `#[cfg_attr(…)]` sets, or a
    /// path string with an escape.
    Undetermined,
}

fn compile(
    ts_language: tree_sitter::Language,
    query_src: &str,
    dialect: Dialect,
) -> Option<LanguageEntry> {
    // The grammar and its query are compiled into the binary, so
    // compilation is deterministic for a given build. A failure means a
    // defective grammar — an incompatible ABI, or a query whose node types
    // moved — so that one language loses its edges rather than taking the
    // rest down with it. The coverage test below fails loudly when one goes
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
        Dialect::RustUse => use_tree_targets(path, source),
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
            "string_literal_single_quotes"
                | "string_literal_double_quotes"
                | "raw_string_literal_single_quotes"
                | "raw_string_literal_double_quotes"
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

/// A `use` declaration's paths, and each module a file declares: one without
/// a body names the file Rust's module rules and the attributes written
/// before it place, and any can start a path the module declaring it writes.
const RUST_QUERY: &str = r#"
(use_declaration argument: (_) @path)
(mod_item name: (identifier) @declared)
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
/// a function or property — and compile into the class a `@file:JvmName`
/// names.
const KOTLIN_QUERY: &str = r#"
(package_header (identifier) @package)
(source_file (class_declaration (type_identifier) @declares))
(source_file (object_declaration (type_identifier) @declares))
(source_file (type_alias (type_identifier) @declares))
(source_file (function_declaration (simple_identifier) @member) @callable)
(source_file (property_declaration (variable_declaration (simple_identifier) @member)) @callable)
(source_file
  (file_annotation
    (constructor_invocation
      (user_type (type_identifier) @annotation)
      (value_arguments . (value_argument (string_literal) @jvm_name) .)))
  (#eq? @annotation "JvmName"))
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

/// Elixir and Swift are absent on purpose: neither ties a module's name to the
/// files that define it — Mix compiles every file under `lib/`, and a Swift
/// module is whatever files a package manifest or an Xcode project assigns
/// it — so a reference to one could only land on a file by a guess, and pack
/// discloses each as a language whose imports it does not read.
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

    /// A module declared without a body names its file, from the inline
    /// modules it is written in, and a `#[path]` attribute before it says
    /// where that file is; one declared in a block is no module of the
    /// file's tree.
    #[test]
    fn a_mod_declaration_names_a_file_only_when_it_has_no_body() {
        let source = "mod auth;\nmod r#type;\nmod tests {\n    fn helper() { mod local; }\n    \
                      /// docs\n    #[cfg(unix)]\n    #[path = \"in/ner.rs\"]\n    mod inner;\n}\n\
                      #[cfg_attr(unix, path = \"u.rs\")]\nmod cond;\n";
        assert_eq!(
            extract(source, Language::Rust),
            vec![
                "self::auth",
                "self::type",
                "self::tests::inner",
                "self::cond"
            ]
        );
        let declared = |scope: &[&str], name: &str, inline, path| ModuleDeclaration {
            scope: scope.iter().map(|part| part.to_string()).collect(),
            name: name.to_string(),
            inline,
            path,
        };
        assert_eq!(
            ImportExtractor::shared()
                .extract(source, Language::Rust)
                .modules,
            vec![
                declared(&[], "auth", false, ModulePath::Default),
                declared(&[], "type", false, ModulePath::Default),
                declared(&[], "tests", true, ModulePath::Default),
                declared(
                    &["tests"],
                    "inner",
                    false,
                    ModulePath::Attribute("in/ner.rs".to_string())
                ),
                declared(&[], "cond", false, ModulePath::Undetermined),
            ]
        );
    }

    /// A Rust `use` path is read as written from the file's own module:
    /// `self::` and `super::` hops and a path from a module declared where
    /// the `use` is written go through the inline modules enclosing it.
    #[test]
    fn a_rust_use_path_is_read_from_the_file_module() {
        let source = "mod a {\n    mod b {\n        use super::x;\n        use super::super::super::y;\n\
                      use self::z;\n        mod c;\n        use c::C;\n        use ext::E;\n    }\n}\n\
                      use a::A;\nfn f() { use super::w; }\n";
        assert_eq!(
            extract(source, Language::Rust),
            vec![
                "self::a::x",
                "super::y",
                "self::a::b::z",
                "self::a::b::c",
                "self::a::b::c::C",
                "ext::E",
                "self::a::A",
                "super::w"
            ]
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

    /// A Kotlin file's top-level functions and properties compile into the
    /// class its `@file:JvmName` names, or else one Kotlin names for the
    /// file; a file declaring none has no such class.
    #[test]
    fn a_kotlin_file_class_holds_its_top_level_functions_and_properties() {
        let class = |content: &str| {
            ImportExtractor::shared()
                .extract(content, Language::Kotlin)
                .file_class
        };
        assert_eq!(
            class("@file:Suppress(\"x\")\n@file:JvmName(\"Utils\")\npackage a\nfun f() = 1\n"),
            Some(FileClass::Named("Utils".to_string()))
        );
        assert_eq!(
            class("@file:Suppress(\"x\")\npackage a\nval p = 1\n"),
            Some(FileClass::OfFileName)
        );
        assert_eq!(
            class("@file:JvmName(\"Utils\")\npackage a\nclass C\n"),
            None
        );
        for unspelled in ["\"U\\u0074ils\"", "\"U$x\""] {
            assert_eq!(
                class(&format!(
                    "@file:JvmName({unspelled})\npackage a\nfun f() = 1\n"
                )),
                None,
                "{unspelled}"
            );
        }
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
        (Language::Rust, "use crate::a::b;\nmod auth;\n"),
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
            "@file:JvmName(\"B\")\npackage a.b\nimport kotlin.collections.List\nclass C\nobject O\n\
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
        (
            Language::Terraform,
            "module \"m\" {\n  source = \"./m\"\n}\n",
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
            Language::Dart,
            Language::Terraform,
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
        assert!(extract("", Language::Rust).is_empty());
        // Neither ties a module's name to a file, so pack discloses them
        // rather than reading references nothing could resolve.
        for language in [Language::Elixir, Language::Swift] {
            assert!(!ImportExtractor::is_supported(language), "{language:?}");
        }
    }
}
