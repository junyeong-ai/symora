use std::collections::HashMap;
use std::path::Path;
use std::sync::Mutex;

use streaming_iterator::StreamingIterator;
use tree_sitter::{Node, Parser, Query, QueryCursor};

use crate::models::symbol::{Language, Location, Symbol, SymbolKind};

/// A parser and its pre-compiled extraction query for one language. The
/// query is compiled once at startup, not per file — query compilation is
/// the dominant per-call cost otherwise.
struct LanguageEntry {
    parser: Mutex<Parser>,
    query: Query,
}

/// Tree-sitter symbol extraction for the indexed store. One registry maps
/// each supported language to its parser and compiled query; adding a
/// language is a single `register` call.
pub struct SymbolExtractor {
    languages: HashMap<Language, LanguageEntry>,
}

impl Default for SymbolExtractor {
    fn default() -> Self {
        Self::new()
    }
}

impl SymbolExtractor {
    pub fn new() -> Self {
        let mut languages = HashMap::new();
        register(
            &mut languages,
            Language::Rust,
            tree_sitter_rust::LANGUAGE.into(),
            RUST_QUERY,
        );
        register(
            &mut languages,
            Language::Go,
            tree_sitter_go::LANGUAGE.into(),
            GO_QUERY,
        );
        register(
            &mut languages,
            Language::Python,
            tree_sitter_python::LANGUAGE.into(),
            PYTHON_QUERY,
        );
        register(
            &mut languages,
            Language::TypeScript,
            tree_sitter_typescript::LANGUAGE_TSX.into(),
            TYPESCRIPT_QUERY,
        );
        register(
            &mut languages,
            Language::JavaScript,
            tree_sitter_javascript::LANGUAGE.into(),
            JAVASCRIPT_QUERY,
        );
        register(
            &mut languages,
            Language::Java,
            tree_sitter_java::LANGUAGE.into(),
            JAVA_QUERY,
        );
        register(
            &mut languages,
            Language::Kotlin,
            tree_sitter_kotlin_sg::LANGUAGE.into(),
            KOTLIN_QUERY,
        );
        register(
            &mut languages,
            Language::Cpp,
            tree_sitter_cpp::LANGUAGE.into(),
            CPP_QUERY,
        );
        register(
            &mut languages,
            Language::CSharp,
            tree_sitter_c_sharp::LANGUAGE.into(),
            CSHARP_QUERY,
        );
        register(
            &mut languages,
            Language::PHP,
            tree_sitter_php::LANGUAGE_PHP.into(),
            PHP_QUERY,
        );
        register(
            &mut languages,
            Language::Ruby,
            tree_sitter_ruby::LANGUAGE.into(),
            RUBY_QUERY,
        );
        register(
            &mut languages,
            Language::Bash,
            tree_sitter_bash::LANGUAGE.into(),
            BASH_QUERY,
        );
        register(
            &mut languages,
            Language::Lua,
            tree_sitter_lua::LANGUAGE.into(),
            LUA_QUERY,
        );
        register(
            &mut languages,
            Language::Swift,
            tree_sitter_swift::LANGUAGE.into(),
            SWIFT_QUERY,
        );
        register(
            &mut languages,
            Language::Scala,
            tree_sitter_scala::LANGUAGE.into(),
            SCALA_QUERY,
        );
        register(
            &mut languages,
            Language::Dart,
            tree_sitter_dart::LANGUAGE.into(),
            DART_QUERY,
        );
        Self { languages }
    }

    /// Languages with a compiled-in index extractor.
    pub fn supported_languages() -> &'static [Language] {
        &[
            Language::Rust,
            Language::Go,
            Language::Python,
            Language::TypeScript,
            Language::JavaScript,
            Language::Java,
            Language::Kotlin,
            Language::Cpp,
            Language::CSharp,
            Language::PHP,
            Language::Ruby,
            Language::Bash,
            Language::Lua,
            Language::Swift,
            Language::Scala,
            Language::Dart,
        ]
    }

    pub fn is_supported(language: Language) -> bool {
        Self::supported_languages().contains(&language)
    }

    /// The process-wide extractor. Grammars and their queries are compiled
    /// into the binary and their compilation is the dominant per-call cost,
    /// so they are built once rather than per store or per call.
    pub fn shared() -> &'static Self {
        static SHARED: std::sync::LazyLock<SymbolExtractor> =
            std::sync::LazyLock::new(SymbolExtractor::new);
        &SHARED
    }

    /// The declarations `content` makes, as the same [`Symbol`] a language
    /// server's document-symbol answer produces — one shape, so a caller
    /// reads either source through the same fields.
    ///
    /// The list is flat: containment is carried by `container` and
    /// `name_path`, which is what addresses a symbol everywhere else.
    pub fn extract(&self, path: &Path, content: &str, language: Language) -> Vec<Symbol> {
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

        let mut cursor = QueryCursor::new();
        let mut symbols = Vec::new();
        let mut matches = cursor.matches(&entry.query, tree.root_node(), content.as_bytes());
        while let Some(m) = matches.next() {
            if let Some(symbol) = extract_from_match(m, path, content, language) {
                symbols.push(symbol);
            }
        }
        symbols
    }
}

fn register(
    languages: &mut HashMap<Language, LanguageEntry>,
    language: Language,
    ts_language: tree_sitter::Language,
    query_src: &str,
) {
    // The grammar and its query are compiled into the binary, so registration
    // is deterministic for a given build. A failure means a defective grammar
    // (an incompatible ABI or a query whose node types moved) — skip that one
    // language so an unrelated language never loses indexing over it. The
    // registration- and extraction-completeness tests fail loudly when a
    // language is missing or under-extracted, so a defective grammar surfaces as
    // a targeted test failure rather than silent index loss.
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
        },
    );
}

fn extract_from_match(
    m: &tree_sitter::QueryMatch,
    path: &Path,
    content: &str,
    language: Language,
) -> Option<Symbol> {
    let node = m.captures.first()?.node;

    let (name, kind) = extract_name_and_kind(node, content, language)?;
    let container = extract_container_path(node, content, language);
    let name_path = Some(match &container {
        Some(container) => format!("{container}/{name}"),
        None => name.clone(),
    });

    // Anchor the symbol at its NAME, not the item start: `refs`/`def` on a
    // leading keyword (`pub`, `fn`, an attribute line) resolve to the wrong
    // symbol or nothing, and a name-span position also lets the index and the
    // LSP workspace pass dedup to a single row (both then point at the same
    // identifier). The declaration node supplies the surrounding range, so a
    // body is sliced from the same two fields a document-symbol answer fills.
    let anchor = name_position_node(node, language).unwrap_or(node);
    let (line, column) = scalar_position(content, anchor.start_byte(), anchor.start_position());
    let (name_end_line, name_end_column) =
        scalar_position(content, anchor.end_byte(), anchor.end_position());
    let (range_start_line, range_start_column) =
        scalar_position(content, node.start_byte(), node.start_position());
    let (end_line, end_column) = scalar_position(content, node.end_byte(), node.end_position());

    let location = Location::full(
        path.to_path_buf(),
        line,
        column,
        range_start_line,
        range_start_column,
        end_line,
        end_column,
    )
    .with_name_end(name_end_line, name_end_column);

    let mut symbol = Symbol::new(name, kind, location);
    symbol.name_path = name_path;
    symbol.container = container;
    Some(symbol)
}

/// A tree-sitter position as CLI/JSON positions are spelled: 1-indexed line,
/// 1-indexed Unicode-scalar column. tree-sitter reports the column as a byte
/// offset within the line, so multibyte text before a symbol would otherwise
/// misplace every follow-up `file:line:col`.
fn scalar_position(content: &str, byte: usize, position: tree_sitter::Point) -> (u32, u32) {
    let line_start = byte.saturating_sub(position.column);
    let column = content
        .get(line_start..byte)
        .map(|prefix| prefix.chars().count() as u32)
        .unwrap_or(position.column as u32)
        + 1;
    (position.row as u32 + 1, column)
}

/// The node whose start position the symbol should be addressed by — its name
/// identifier, so `file:line:col` lands on the thing `refs`/`def` resolve. An
/// impl is anchored at its self type (it has no name identifier of its own).
fn name_position_node<'a>(node: Node<'a>, language: Language) -> Option<Node<'a>> {
    if node.kind() == "impl_item" {
        return node.child_by_field_name("type");
    }
    find_name_node(node, language)
}

fn extract_container_path(mut node: Node, content: &str, language: Language) -> Option<String> {
    // A symbol is keyed by its IMMEDIATE container only — the nearest enclosing
    // type/impl — so a method reads `Type/method`, an enclosing outer type or
    // module never widens it to `Outer/Inner/method`, and a module-level item
    // stays bare. This matches what the LSP workspace surface can report (a
    // method's container is its nearest type; outer types and namespaces are
    // flattened away there), so a name_path round-trips across the index,
    // documentSymbol, and workspace surfaces. Modules/namespaces/packages
    // qualify nothing and are skipped on the way up to that nearest type.
    while let Some(parent) = node.parent() {
        node = parent;
        if let Some((name, kind)) = extract_name_and_kind(node, content, language)
            && !name.is_empty()
            && !kind.is_namespace_like()
        {
            return Some(name);
        }
    }
    None
}

fn extract_name_and_kind(
    node: Node,
    content: &str,
    language: Language,
) -> Option<(String, SymbolKind)> {
    // An impl block is named by its self type, reduced by the one shared rule
    // (`Symbol::self_type_segment`) the documentSymbol and workspace-symbol
    // producers also apply — so a method keyed under it gets the same
    // `Type/method` name_path on every surface (index, documentSymbol,
    // workspace), structural and primitive self types included. A self type
    // with no nominal name (e.g. `fn()`) reduces to an empty segment: the impl
    // is then a transparent container whose methods attach to the enclosing
    // path, never carrying a stray name.
    if node.kind() == "impl_item" {
        let type_node = node.child_by_field_name("type")?;
        let self_type = content.get(type_node.start_byte()..type_node.end_byte())?;
        let name = Symbol::self_type_segment(self_type);
        return (!name.is_empty()).then(|| (name, node_kind(node)));
    }

    // Resolve the name first: nameless parents (blocks, lists, the source
    // root) are walked during container resolution and must be skipped
    // before a kind is ever assigned to them.
    let name_node = find_name_node(node, language)?;
    let name = content
        .get(name_node.start_byte()..name_node.end_byte())?
        .to_string();
    if name.is_empty() || (name.starts_with('_') && name.len() == 1) {
        return None;
    }
    Some((name, node_kind(node)))
}

/// The first child of `node` with the given grammar kind. Grammars that wrap a
/// declaration's name in unnamed intermediate nodes are read through this
/// rather than by position, which moves whenever modifiers or attributes are
/// written before it.
fn child_of_kind<'a>(node: Node<'a>, kind: &str) -> Option<Node<'a>> {
    (0..node.child_count()).find_map(|i| node.child(i as u32).filter(|c| c.kind() == kind))
}

/// The identifier a captured declaration is named by.
///
/// A node this cannot name is dropped from extraction without a trace, so a
/// query and this function are one decision: several grammars spell a member's
/// name behind wrappers their neighbours do not use, and a query that captured
/// one of those read as a language simply not declaring that form. The
/// declaration fixtures are what hold the two together.
fn find_name_node(node: Node, language: Language) -> Option<Node> {
    let name_field = match language {
        Language::Kotlin => node
            .child_by_field_name("name")
            .or_else(|| node.child_by_field_name("simple_identifier"))
            .or_else(|| {
                child_of_kind(node, "variable_declaration")
                    .and_then(|d| child_of_kind(d, "simple_identifier"))
            }),
        Language::Cpp => node
            .child_by_field_name("declarator")
            .map(|declarator| {
                declarator
                    .child_by_field_name("declarator")
                    .unwrap_or(declarator)
            })
            .or_else(|| node.child_by_field_name("name")),
        Language::Java => node.child_by_field_name("name").or_else(|| {
            node.child_by_field_name("declarator")
                .and_then(|d| d.child_by_field_name("name"))
        }),
        Language::PHP => node.child_by_field_name("name").or_else(|| {
            child_of_kind(node, "property_element")
                .and_then(|e| child_of_kind(e, "variable_name"))
                .and_then(|v| child_of_kind(v, "name"))
        }),
        Language::CSharp => node.child_by_field_name("name").or_else(|| {
            child_of_kind(node, "variable_declaration")
                .and_then(|d| child_of_kind(d, "variable_declarator"))
                .and_then(|d| {
                    d.child_by_field_name("name")
                        .or_else(|| child_of_kind(d, "identifier"))
                })
        }),
        _ => node.child_by_field_name("name"),
    };

    name_field.or_else(|| {
        let count = node.child_count();
        for i in 0..count {
            if let Some(child) = node.child(i as u32) {
                let kind = child.kind();
                if matches!(
                    kind,
                    "identifier"
                        | "name"
                        | "simple_identifier"
                        | "type_identifier"
                        | "property_identifier"
                ) {
                    return Some(child);
                }
            }
        }
        None
    })
}

/// Map a captured declaration node to a [`SymbolKind`]. Every node kind the
/// extraction queries capture has an explicit arm; the trailing arm only
/// ever serves nameless container parents whose kind is discarded.
fn node_kind(node: Node) -> SymbolKind {
    match node.kind() {
        "function_item"
        | "function_definition"
        | "function_declaration"
        | "generator_function_declaration"
        | "function_signature"
        | "function_signature_item"
        | "macro_definition" => SymbolKind::Function,

        "method_item"
        | "method_declaration"
        | "method_definition"
        | "method"
        | "singleton_method"
        | "method_signature"
        | "abstract_method_signature"
        | "protocol_function_declaration" => SymbolKind::Method,

        "constructor_signature" => SymbolKind::Constructor,

        "class_declaration"
        | "abstract_class_declaration"
        | "record_declaration"
        | "delegate_declaration"
        | "class_definition"
        | "class_specifier"
        | "object_declaration"
        | "object_definition"
        | "extension_declaration"
        | "class"
        | "impl_item" => SymbolKind::Class,

        "struct_item" | "struct_specifier" | "struct_type" | "struct_declaration"
        | "union_item" => SymbolKind::Struct,

        "enum_item" | "enum_declaration" | "enum_specifier" => SymbolKind::Enum,

        "interface_declaration"
        | "interface_type"
        | "trait_item"
        | "trait_declaration"
        | "trait_definition"
        | "mixin_declaration"
        | "annotation_type_declaration"
        | "protocol_declaration" => SymbolKind::Interface,

        // Every grammar's namespace/module/package container. These organize
        // code but must NOT widen a member's name_path (see `is_namespace_like`
        // and `extract_container_path`): Rust `mod`, C/C++ `namespace`, Java
        // `package`, TS/JS `namespace`/`module` (both parse as `internal_module`,
        // plus ambient `module "x"`), C# block- and file-scoped `namespace`, and
        // PHP `namespace`.
        "mod_item"
        | "namespace_definition"
        | "package_declaration"
        | "internal_module"
        | "module"
        | "namespace_declaration"
        | "file_scoped_namespace_declaration" => SymbolKind::Module,

        // Go `type X = ...`: classify by the spec's underlying type.
        "type_spec" => go_type_kind(node),

        // A named type alias introduces a type, not a generic `<T>` param.
        "type_item" | "type_alias_declaration" | "type_alias" => SymbolKind::Class,

        "const_item" | "const_spec" | "val_definition" => SymbolKind::Constant,

        "static_item" | "var_spec" | "var_definition" => SymbolKind::Variable,

        // JS/TS `const f = () => {}` is a callable; classify by the
        // initializer rather than always Variable (which is_low_level would
        // drop under exclude_low_level).
        "variable_declarator" => declarator_kind(node),

        "property_declaration" => SymbolKind::Property,
        "field_declaration" => field_kind(node),

        _ => SymbolKind::Variable,
    }
}

/// Classify a Go `type_spec` by its underlying type: `struct` → Struct,
/// `interface` → Interface, anything else is a named alias (Class).
fn go_type_kind(node: Node) -> SymbolKind {
    match node.child_by_field_name("type").map(|t| t.kind()) {
        Some("struct_type") => SymbolKind::Struct,
        Some("interface_type") => SymbolKind::Interface,
        _ => SymbolKind::Class,
    }
}

/// Classify a member declaration by its declarator: C++ states a method
/// declaration with the same `field_declaration` node it states a data member
/// with, and only the declarator separates them. Java, whose fields use the
/// same node kind, declares methods elsewhere and is unaffected.
fn field_kind(node: Node) -> SymbolKind {
    match node.child_by_field_name("declarator").map(|d| d.kind()) {
        Some("function_declarator") => SymbolKind::Method,
        _ => SymbolKind::Field,
    }
}

/// Classify a JS/TS `variable_declarator` by its initializer: a function value
/// (arrow function, function expression, or generator function) is a callable
/// Function; anything else is a plain Variable. Mirrors `go_type_kind`'s
/// value-field dispatch and keeps the decision structural — no name heuristics.
fn declarator_kind(node: Node) -> SymbolKind {
    match node.child_by_field_name("value").map(|v| v.kind()) {
        Some("arrow_function") | Some("function_expression") | Some("generator_function") => {
            SymbolKind::Function
        }
        _ => SymbolKind::Variable,
    }
}

// Language-specific tree-sitter queries for symbol extraction.

const RUST_QUERY: &str = r#"
(function_item) @symbol
(function_signature_item) @symbol
(macro_definition) @symbol
(struct_item) @symbol
(union_item) @symbol
(enum_item) @symbol
(trait_item) @symbol
(impl_item) @symbol
(mod_item) @symbol
(type_item) @symbol
(const_item) @symbol
(static_item) @symbol
"#;

const GO_QUERY: &str = r#"
(function_declaration) @symbol
(method_declaration) @symbol
(type_declaration (type_spec) @symbol)
(type_declaration (type_alias) @symbol)
(const_declaration (const_spec) @symbol)
(var_declaration (var_spec) @symbol)
"#;

const PYTHON_QUERY: &str = r#"
(function_definition) @symbol
(class_definition) @symbol
"#;

// Module-scope `const f = () => {}` / `export const f = function () {}` is the
// dominant TS/JS function form, but it parses as a variable_declarator, not a
// function_declaration. Capture it — anchored to module scope and filtered to a
// function-valued initializer — so it is indexed without dragging in nested
// locals, loop counters, or destructuring patterns. Both lexical_declaration
// (const/let) and variable_declaration (var), bare and export-wrapped.
const TYPESCRIPT_QUERY: &str = r#"
(function_declaration) @symbol
(generator_function_declaration) @symbol
(class_declaration) @symbol
(abstract_class_declaration) @symbol
(internal_module) @symbol
(interface_declaration) @symbol
(type_alias_declaration) @symbol
(enum_declaration) @symbol
(method_definition) @symbol
(interface_declaration (interface_body (method_signature) @symbol))
(class_body (abstract_method_signature) @symbol)
(program (lexical_declaration (variable_declarator value: [(arrow_function) (function_expression) (generator_function)]) @symbol))
(program (variable_declaration (variable_declarator value: [(arrow_function) (function_expression) (generator_function)]) @symbol))
(program (export_statement (lexical_declaration (variable_declarator value: [(arrow_function) (function_expression) (generator_function)]) @symbol)))
(program (export_statement (variable_declaration (variable_declarator value: [(arrow_function) (function_expression) (generator_function)]) @symbol)))
"#;

const JAVASCRIPT_QUERY: &str = r#"
(function_declaration) @symbol
(generator_function_declaration) @symbol
(class_declaration) @symbol
(method_definition) @symbol
(program (lexical_declaration (variable_declarator value: [(arrow_function) (function_expression) (generator_function)]) @symbol))
(program (variable_declaration (variable_declarator value: [(arrow_function) (function_expression) (generator_function)]) @symbol))
(program (export_statement (lexical_declaration (variable_declarator value: [(arrow_function) (function_expression) (generator_function)]) @symbol)))
(program (export_statement (variable_declaration (variable_declarator value: [(arrow_function) (function_expression) (generator_function)]) @symbol)))
"#;

const JAVA_QUERY: &str = r#"
(package_declaration) @symbol
(class_declaration) @symbol
(record_declaration) @symbol
(interface_declaration) @symbol
(annotation_type_declaration) @symbol
(enum_declaration) @symbol
(method_declaration) @symbol
(field_declaration) @symbol
"#;

const KOTLIN_QUERY: &str = r#"
(class_declaration) @symbol
(object_declaration) @symbol
(function_declaration) @symbol
(property_declaration) @symbol
"#;

const CPP_QUERY: &str = r#"
(function_definition) @symbol
(class_specifier) @symbol
(struct_specifier) @symbol
(enum_specifier) @symbol
(namespace_definition) @symbol
(field_declaration) @symbol
"#;

const CSHARP_QUERY: &str = r#"
(namespace_declaration) @symbol
(file_scoped_namespace_declaration) @symbol
(class_declaration) @symbol
(record_declaration) @symbol
(delegate_declaration) @symbol
(interface_declaration) @symbol
(struct_declaration) @symbol
(enum_declaration) @symbol
(method_declaration) @symbol
(property_declaration) @symbol
(field_declaration) @symbol
"#;

const RUBY_QUERY: &str = r#"
(module) @symbol
(class) @symbol
(method) @symbol
(singleton_method) @symbol
"#;

const BASH_QUERY: &str = r#"
(function_definition) @symbol
"#;

const LUA_QUERY: &str = r#"
(function_declaration) @symbol
"#;

// A Swift `let`/`var` inside a function body parses as the same
// `property_declaration` a stored property does, so members are matched
// where they are declared — directly in a type body or at file scope.
const SWIFT_QUERY: &str = r#"
(class_declaration) @symbol
(protocol_declaration) @symbol
(function_declaration) @symbol
(protocol_function_declaration) @symbol
(class_body (property_declaration) @symbol)
(source_file (property_declaration) @symbol)
"#;

// `val`/`var` share one node kind with their function-local counterparts;
// the template body is what separates a member from a local.
const SCALA_QUERY: &str = r#"
(class_definition) @symbol
(object_definition) @symbol
(trait_definition) @symbol
(function_definition) @symbol
(function_declaration) @symbol
(template_body (val_definition) @symbol)
(template_body (var_definition) @symbol)
"#;

// Dart names a function on its signature, not on the declaration that
// wraps it, and a method's signature nests the same node.
const DART_QUERY: &str = r#"
(class_declaration) @symbol
(enum_declaration) @symbol
(mixin_declaration) @symbol
(extension_declaration) @symbol
(function_signature) @symbol
(constructor_signature) @symbol
"#;

const PHP_QUERY: &str = r#"
(namespace_definition) @symbol
(function_definition) @symbol
(class_declaration) @symbol
(enum_declaration) @symbol
(interface_declaration) @symbol
(trait_declaration) @symbol
(method_declaration) @symbol
(property_declaration) @symbol
"#;

#[cfg(test)]
mod tests {
    use super::*;

    /// What a language declares, written from the LANGUAGE rather than from
    /// the query that reads it.
    ///
    /// Every other extraction test states the forms its query already
    /// captures, so a declaration form nobody thought of is invisible to all
    /// of them — which is how abstract classes, records, unions, namespaces
    /// and trait method signatures went missing at once. A source here is a
    /// tour of one grammar's declaration forms, and `declares` is the whole
    /// answer, so a form that stops being extracted fails rather than
    /// quietly narrowing what a search speaks for.
    ///
    /// The set is types, callables, and the module/namespace declarations
    /// that hold them — the categories every language here already claims.
    /// Which named VALUES a language admits differs between them and is
    /// recorded as each one behaves, not levelled.
    struct DeclarationFixture {
        language: Language,
        path: &'static str,
        source: &'static str,
        declares: &'static [&'static str],
    }

    const DECLARATION_FIXTURES: &[DeclarationFixture] = &[
        DeclarationFixture {
            language: Language::Rust,
            path: "a.rs",
            source: r#"
pub trait T { fn tm(&self); }
pub union U { a: u32 }
macro_rules! mac { () => {} }
pub type Al = u32;
pub const CX: u32 = 1;
pub static ST: u32 = 2;
pub enum E { A }
pub mod inner { pub fn nested() {} }
pub struct S;
impl S { pub fn m(&self) { let local = 1; } }
"#,
            declares: &[
                "interface:T",
                "function:T/tm",
                "struct:U",
                "function:mac",
                "class:Al",
                "constant:CX",
                "variable:ST",
                "enum:E",
                "module:inner",
                "function:nested",
                "struct:S",
                "class:S",
                "function:S/m",
            ],
        },
        DeclarationFixture {
            language: Language::Go,
            path: "a.go",
            source: r#"
package m

type Al = int
type S struct{ F int }
type I interface{ Do() }

const CX = 1
var VY = 2

func F() {}
func (s *S) M() {}
"#,
            declares: &[
                "class:Al",
                "struct:S",
                "interface:I",
                "constant:CX",
                "variable:VY",
                "function:F",
                "method:M",
            ],
        },
        DeclarationFixture {
            language: Language::Python,
            path: "a.py",
            source: r#"
class A:
    def m(self): pass

def f(): pass

async def af(): pass
"#,
            declares: &["class:A", "function:A/m", "function:f", "function:af"],
        },
        DeclarationFixture {
            language: Language::TypeScript,
            path: "a.ts",
            source: r#"
export abstract class Abs { abstract doIt(): void; }
export namespace NS {}
export interface Shape { area(): number; }
export type Alias = string;
export enum Color { Red }
export class C { m(): void {} }
export function f(): void {}
export const arrow = () => 1;
"#,
            declares: &[
                "class:Abs",
                "method:Abs/doIt",
                "module:NS",
                "interface:Shape",
                "method:Shape/area",
                "class:Alias",
                "enum:Color",
                "class:C",
                "method:C/m",
                "function:f",
                "function:arrow",
            ],
        },
        DeclarationFixture {
            language: Language::JavaScript,
            path: "a.js",
            source: r#"
export class C { m() {} }
export function f() {}
export const arrow = () => 1;
function* gen() {}
"#,
            declares: &[
                "class:C",
                "method:C/m",
                "function:f",
                "function:arrow",
                "function:gen",
            ],
        },
        DeclarationFixture {
            language: Language::Java,
            path: "A.java",
            source: r#"
package p;

record R(int a) {}

@interface Ann {}

interface I { void im(); }

enum E { X }

class C {
    int field;
    void m() {}
}
"#,
            declares: &[
                "module:p",
                "class:R",
                "interface:Ann",
                "interface:I",
                "method:I/im",
                "enum:E",
                "class:C",
                "field:C/field",
                "method:C/m",
            ],
        },
        DeclarationFixture {
            language: Language::CSharp,
            path: "a.cs",
            source: r#"
namespace N {
    public record Rec(int A);
    public delegate void D();
    public interface I { void M(); }
    public struct St { public int F; }
    public enum E { X }
    class C {
        public int P { get; set; }
        void M() {}
    }
}
"#,
            declares: &[
                "module:N",
                "class:Rec",
                "class:D",
                "interface:I",
                "method:I/M",
                "struct:St",
                "field:St/F",
                "enum:E",
                "class:C",
                "property:C/P",
                "method:C/M",
            ],
        },
        DeclarationFixture {
            language: Language::PHP,
            path: "a.php",
            source: r#"
<?php
namespace N;

enum E { case X; }

trait T { public function tm() {} }

interface I { public function im(); }

abstract class A {
    public $prop;
    abstract public function am();
}

function f() {}
"#,
            declares: &[
                "module:N",
                "enum:E",
                "interface:T",
                "method:T/tm",
                "interface:I",
                "method:I/im",
                "class:A",
                "property:A/prop",
                "method:A/am",
                "function:f",
            ],
        },
        DeclarationFixture {
            language: Language::Kotlin,
            path: "a.kt",
            source: r#"
package demo

interface I {
    fun im()
}

object Registry {
    fun register() {}
}

class Widget {
    val name = "w"
    fun render() {}
}

fun top() {}
"#,
            declares: &[
                "class:I",
                "function:I/im",
                "class:Registry",
                "function:Registry/register",
                "class:Widget",
                "property:Widget/name",
                "function:Widget/render",
                "function:top",
            ],
        },
        DeclarationFixture {
            language: Language::Cpp,
            path: "a.cpp",
            source: r#"
namespace N {

struct S { int f; };

class C {
public:
    void m();
};

enum E { X };

void f() {}

}
"#,
            declares: &[
                "module:N",
                "struct:S",
                "field:S/f",
                "class:C",
                "method:C/m",
                "enum:E",
                "function:f",
            ],
        },
        DeclarationFixture {
            language: Language::Ruby,
            path: "a.rb",
            source: r#"
module M
  class C
    def m; end
    def self.cm; end
  end
end
"#,
            declares: &["module:M", "class:C", "method:C/m", "method:C/cm"],
        },
        DeclarationFixture {
            language: Language::Bash,
            path: "a.sh",
            source: r#"
greet() { echo hi; }

function other { echo hi; }
"#,
            declares: &["function:greet", "function:other"],
        },
        DeclarationFixture {
            language: Language::Lua,
            path: "a.lua",
            source: r#"
function top() end

local function helper() end
"#,
            declares: &["function:top", "function:helper"],
        },
        DeclarationFixture {
            language: Language::Swift,
            path: "a.swift",
            source: r#"
protocol P {
    func pm()
}

class C {
    var p = 1
    func cm() {}
}

func top() {}
"#,
            declares: &[
                "interface:P",
                "method:P/pm",
                "class:C",
                "property:C/p",
                "function:C/cm",
                "function:top",
            ],
        },
        DeclarationFixture {
            language: Language::Scala,
            path: "a.scala",
            source: r#"
trait T {
  def tm(): Unit
}

object O {
  val v = 1
}

class C {
  def cm(): Unit = {}
}
"#,
            declares: &[
                "interface:T",
                "function:T/tm",
                "class:O",
                "constant:O/v",
                "class:C",
                "function:C/cm",
            ],
        },
        DeclarationFixture {
            language: Language::Dart,
            path: "a.dart",
            source: r#"
mixin M {}

enum E { x }

abstract class A {
  void am();
}

class C {
  C();
  void cm() {}
}

void top() {}
"#,
            declares: &[
                "interface:M",
                "enum:E",
                "class:A",
                "function:A/am",
                "class:C",
                "constructor:C/C",
                "function:C/cm",
                "function:top",
            ],
        },
    ];

    /// Pins the static `supported_languages` answer to runtime registration:
    /// a grammar bump that breaks one language's ABI or extraction query
    /// fails here loudly instead of degrading to silent empty results.
    #[test]
    fn every_supported_language_registers_an_extractor() {
        let extractor = SymbolExtractor::new();
        for language in SymbolExtractor::supported_languages() {
            assert!(
                extractor.languages.contains_key(language),
                "{language:?} failed to register — its grammar ABI or extraction query is broken"
            );
        }
        assert_eq!(
            extractor.languages.len(),
            SymbolExtractor::supported_languages().len()
        );
    }

    #[test]
    fn every_declaration_form_a_fixture_writes_is_extracted() {
        let extractor = SymbolExtractor::new();
        for fixture in DECLARATION_FIXTURES {
            let symbols =
                extractor.extract(Path::new(fixture.path), fixture.source, fixture.language);
            let mut found: Vec<String> = symbols
                .iter()
                .map(|s| format!("{}:{}", s.kind, s.name_path.as_deref().unwrap_or(&s.name)))
                .collect();
            let mut expected: Vec<String> =
                fixture.declares.iter().map(|d| d.to_string()).collect();
            found.sort();
            expected.sort();
            assert_eq!(
                found, expected,
                "{:?} extraction does not match what {} declares",
                fixture.language, fixture.path
            );
        }
    }

    /// A language whose extractor no fixture describes is one whose gaps
    /// nothing can find, so registering a grammar and writing its tour are
    /// one step.
    #[test]
    fn every_extractor_language_has_a_declaration_fixture() {
        for language in SymbolExtractor::supported_languages() {
            assert!(
                DECLARATION_FIXTURES
                    .iter()
                    .any(|fixture| fixture.language == *language),
                "{language:?} extracts symbols with no fixture saying which forms it must reach"
            );
        }
    }

    /// Extraction reads a grammar's declaration nodes, so it can never reach
    /// a language AST search does not parse. The converse is not an omission:
    /// a grammar that states declarations as generic calls or blocks carries
    /// no name for the shared resolution to read, and `doctor` says so per
    /// language rather than the set being asserted here.
    #[test]
    fn extraction_never_exceeds_ast_coverage() {
        for language in SymbolExtractor::supported_languages() {
            assert!(
                crate::infra::ast::is_supported(*language),
                "{language:?} extracts symbols from a grammar AST search does not parse"
            );
        }
    }

    /// Every position the extractor emits is a character column, not a byte
    /// offset — the two agree for ASCII, so only a line carrying multibyte
    /// text before a symbol can tell a missed conversion from a correct one.
    /// A wrong column here misplaces every `file:line:col` taken from it.
    #[test]
    fn positions_are_character_columns_on_every_span() {
        let extractor = SymbolExtractor::new();
        let content = "class 주문Handler:\n    def 처리(self):\n        return 1\n";
        let symbols = extractor.extract(Path::new("a.py"), content, Language::Python);

        let class = symbols.iter().find(|s| s.name == "주문Handler").unwrap();
        assert_eq!((class.location.line, class.location.column), (1, 7));
        assert_eq!(class.location.name_end_column, Some(16));

        let method = symbols.iter().find(|s| s.name == "처리").unwrap();
        assert_eq!((method.location.line, method.location.column), (2, 9));
        assert_eq!(method.location.range_start_column, Some(5));
        assert_eq!(method.location.end_line, Some(3));

        let mut with_body = vec![method.clone()];
        Symbol::attach_bodies(&mut with_body, content);
        assert_eq!(
            with_body[0].body.as_deref(),
            Some("    def 처리(self):\n        return 1")
        );
    }

    #[test]
    fn ruby_symbols_carry_the_name_the_file_spells() {
        let extractor = SymbolExtractor::new();
        let content = r#"
module Billing
  class Invoice
    def valid?
      true
    end
    def self.build(x) = new(x)
  end
end
"#;
        let symbols = extractor.extract(Path::new("a"), content, Language::Ruby);
        let found = |path: &str| {
            symbols
                .iter()
                .find(|s| s.name_path.as_deref() == Some(path))
                .map(|s| s.kind)
        };
        assert_eq!(found("Billing"), Some(SymbolKind::Module));
        assert_eq!(found("Invoice"), Some(SymbolKind::Class));
        assert_eq!(found("Invoice/valid?"), Some(SymbolKind::Method));
        assert_eq!(found("Invoice/build"), Some(SymbolKind::Method));
    }

    #[test]
    fn shell_and_lua_extract_their_function_forms() {
        let extractor = SymbolExtractor::new();
        let shell = extractor.extract(
            Path::new("a"),
            "deploy() { :; }\nfunction rollback { :; }\n",
            Language::Bash,
        );
        assert_eq!(shell.len(), 2);
        assert!(shell.iter().all(|s| s.kind == SymbolKind::Function));

        let lua = extractor.extract(
            Path::new("a"),
            "local function helper() end\nfunction M.render() end\n",
            Language::Lua,
        );
        let names: Vec<&str> = lua.iter().map(|s| s.name.as_str()).collect();
        assert_eq!(names, ["helper", "M.render"]);
    }

    #[test]
    fn dart_names_a_function_on_its_signature() {
        let extractor = SymbolExtractor::new();
        let content = r#"
class Order {
  Order(this.id);
  void pay() {}
}
enum Status { open }
mixin Loggable { void log() {} }
void topLevel() {}
"#;
        let symbols = extractor.extract(Path::new("a"), content, Language::Dart);
        let found = |path: &str| {
            symbols
                .iter()
                .find(|s| s.name_path.as_deref() == Some(path))
                .map(|s| s.kind)
        };
        assert_eq!(found("Order"), Some(SymbolKind::Class));
        assert_eq!(found("Order/Order"), Some(SymbolKind::Constructor));
        assert_eq!(found("Order/pay"), Some(SymbolKind::Function));
        assert_eq!(found("Status"), Some(SymbolKind::Enum));
        assert_eq!(found("Loggable"), Some(SymbolKind::Interface));
        assert_eq!(found("topLevel"), Some(SymbolKind::Function));
    }

    /// Swift and Scala give a function-local binding the same node kind as a
    /// stored member, so the queries match members where they are declared.
    /// Indexing a local would put a name in the index that names nothing a
    /// caller can reach.
    #[test]
    fn a_function_local_binding_is_not_a_member() {
        let extractor = SymbolExtractor::new();
        let swift = extractor.extract(
            Path::new("a"),
            r#"
class Cart {
    var items: [Int] = []
    func total() -> Int {
        let base = 10
        return base
    }
}
"#,
            Language::Swift,
        );
        let swift_names: Vec<&str> = swift.iter().map(|s| s.name.as_str()).collect();
        assert!(swift_names.contains(&"items"));
        assert!(swift_names.contains(&"total"));
        assert!(!swift_names.contains(&"base"));

        let scala = extractor.extract(
            Path::new("a"),
            r#"
class Order {
  val id = 1
  def total(): Int = {
    val base = 10
    base
  }
}
"#,
            Language::Scala,
        );
        let scala_names: Vec<&str> = scala.iter().map(|s| s.name.as_str()).collect();
        assert!(scala_names.contains(&"id"));
        assert!(scala_names.contains(&"total"));
        assert!(!scala_names.contains(&"base"));
    }

    #[test]
    fn rust_symbols_carry_correct_kinds() {
        let extractor = SymbolExtractor::new();
        let content = r#"
fn main() {}
struct Foo {}
enum Bar { A, B }
trait Baz {}
type Alias = Foo;
"#;
        let symbols = extractor.extract(Path::new("a"), content, Language::Rust);
        let kind = |name: &str| symbols.iter().find(|s| s.name == name).map(|s| s.kind);
        assert_eq!(kind("main"), Some(SymbolKind::Function));
        assert_eq!(kind("Foo"), Some(SymbolKind::Struct));
        assert_eq!(kind("Bar"), Some(SymbolKind::Enum));
        assert_eq!(kind("Baz"), Some(SymbolKind::Interface));
        // A type alias is a named type, never a generic parameter.
        assert_eq!(kind("Alias"), Some(SymbolKind::Class));
    }

    /// The cross-surface invariant: the tree-sitter index extractor, the LSP
    /// self-type normalizer (`Symbol::normalize_symbol_name`), and the
    /// workspace-symbol path all key an impl method under the SAME container —
    /// the single `Symbol::self_type_segment` rule the index now calls too — or
    /// a `name_path` copied from one surface fails against another
    /// (`symbols`/`edit`). Pins every self-type shape: nominal, generic,
    /// scoped/unscoped trait, structural (tuple/array/pointer/qualified),
    /// primitive-element structural (no separate AST descent to diverge on
    /// anymore), a nominal path that merely starts with `fn`, and a truly
    /// nameless self type (`fn()`/`()`) that reduces to a transparent container
    /// so the method is keyed bare. `ra_label` is the matching rust-analyzer
    /// impl label; `expected` is the shared container segment, `None` for the
    /// transparent case.
    #[test]
    fn impl_method_container_agrees_with_lsp_normalizer() {
        let extractor = SymbolExtractor::new();
        let cases: [(&str, &str, Option<&str>); 13] = [
            ("impl Foo { fn m(&self) {} }", "impl Foo", Some("Foo")),
            (
                "impl<T> Wrap<T> { fn m(&self) {} }",
                "impl<T> Wrap<T>",
                Some("Wrap"),
            ),
            (
                "impl std::fmt::Display for Foo { fn m(&self) {} }",
                "impl std::fmt::Display for Foo",
                Some("Foo"),
            ),
            (
                "impl FromStr for Foo { fn m(&self) {} }",
                "impl FromStr for Foo",
                Some("Foo"),
            ),
            (
                "impl Tr for (A, B) { fn m(&self) {} }",
                "impl Tr for (A, B)",
                Some("A"),
            ),
            (
                "impl Tr for [Elem; 4] { fn m(&self) {} }",
                "impl Tr for [Elem; 4]",
                Some("Elem"),
            ),
            (
                "impl Tr for *const Ptr { fn m(&self) {} }",
                "impl Tr for *const Ptr",
                Some("Ptr"),
            ),
            (
                "impl Tr for <Qual as Baz>::Out { fn m(&self) {} }",
                "impl Tr for <Qual as Baz>::Out",
                Some("Qual"),
            ),
            // a nominal path whose head merely starts with "fn" is not a fn-pointer
            (
                "impl Tr for fn_mod::Named { fn m(&self) {} }",
                "impl Tr for fn_mod::Named",
                Some("Named"),
            ),
            // primitive-element structural types: the one rule keeps the first
            // nominal word — the AST `type_identifier`-only descent that used to
            // skip primitives (and diverge here) is gone.
            (
                "impl Tr for [u8; 4] { fn m(&self) {} }",
                "impl Tr for [u8; 4]",
                Some("u8"),
            ),
            (
                "impl Tr for fn(u8) -> u8 { fn m(&self) {} }",
                "impl Tr for fn(u8) -> u8",
                Some("u8"),
            ),
            // truly nameless self types — transparent container, method keyed bare
            (
                "impl Tr for fn() { fn m(&self) {} }",
                "impl Tr for fn()",
                None,
            ),
            ("impl Tr for () { fn m(&self) {} }", "impl Tr for ()", None),
        ];
        for (src, ra_label, expected) in cases {
            let symbols = extractor.extract(Path::new("a"), src, Language::Rust);
            let method = symbols
                .iter()
                .find(|s| s.name == "m")
                .unwrap_or_else(|| panic!("method not extracted from {src:?}"));
            assert_eq!(
                method.container.as_deref(),
                expected,
                "index container for {src:?}"
            );
            // The LSP normalizer reduces the same self type; an empty segment is
            // the transparent (no-container) case the index represents as `None`.
            let norm = Symbol::normalize_symbol_name(ra_label);
            let lsp_container = (!norm.is_empty()).then_some(norm.as_str());
            assert_eq!(lsp_container, expected, "LSP normalizer for {ra_label:?}");
            // The stored name_path is the round-trip key the workspace producer
            // rebuilds from the same container segment.
            let expected_path = match expected {
                Some(c) => format!("{c}/m"),
                None => "m".to_string(),
            };
            assert_eq!(
                method.name_path.as_deref(),
                Some(expected_path.as_str()),
                "index name_path for {src:?}"
            );
        }
    }

    /// Modules organize but never qualify the addressing path: a method of a
    /// type nested in modules is keyed `Type/method` and a module-level free
    /// function bare — matching rust-analyzer's workspace-symbol container
    /// (which omits the enclosing module) so the index path round-trips against
    /// `symbols`/`edit`. The module prefix the AST carries is dropped here.
    #[test]
    fn index_drops_enclosing_module_from_name_path() {
        let extractor = SymbolExtractor::new();
        let src = "mod a { mod b { struct Deep; impl Deep { fn m(&self) {} } fn free() {} } }";
        let symbols = extractor.extract(Path::new("a"), src, Language::Rust);
        let path = |name: &str| {
            symbols
                .iter()
                .find(|s| s.name == name)
                .unwrap_or_else(|| panic!("{name} not extracted from {src:?}"))
                .name_path
                .clone()
        };
        assert_eq!(path("m"), Some("Deep/m".to_string()));
        assert_eq!(path("free"), Some("free".to_string()));
        // No producer keeps the enclosing module in the addressing path.
        for s in &symbols {
            let np = s.name_path.as_deref().unwrap_or_default();
            assert!(
                !np.starts_with("a/") && !np.contains("/a/") && !np.contains("/b/"),
                "module leaked into name_path: {np:?}"
            );
        }
    }

    /// A member of a type nested inside another type (and a namespace) is keyed
    /// by its IMMEDIATE container only — `Inner/method`, never
    /// `ns/Outer/Inner/method` — matching what clangd's workspace surface
    /// reports (`ns::Outer::Inner` → reduced to the nearest type `Inner`) so the
    /// index path round-trips. The namespace drops out and the outer type does
    /// not widen the path; the enclosing type still qualifies the inner type
    /// itself (`Outer/Inner`).
    #[test]
    fn index_keys_nested_type_member_by_immediate_container() {
        let extractor = SymbolExtractor::new();
        let src = "namespace ns { class Outer { class Inner { void method(); }; void om(); }; }";
        let symbols = extractor.extract(Path::new("a"), src, Language::Cpp);
        let path = |name: &str| {
            symbols
                .iter()
                .find(|s| s.name == name)
                .unwrap_or_else(|| panic!("{name} not extracted from {src:?}"))
                .name_path
                .clone()
        };
        assert_eq!(path("method"), Some("Inner/method".to_string()));
        assert_eq!(path("om"), Some("Outer/om".to_string()));
        assert_eq!(path("Inner"), Some("Outer/Inner".to_string()));
        // The namespace never appears in any addressing path.
        for s in &symbols {
            let np = s.name_path.as_deref().unwrap_or_default();
            assert!(
                !np.contains("ns/"),
                "namespace leaked into name_path: {np:?}"
            );
        }
    }

    /// A `namespace`/`module` in every grammar must be path-transparent, like a
    /// Rust `mod` — a class directly inside it stays bare on the index just as it
    /// does on the workspace/documentSymbol surfaces (which never report the
    /// namespace), so a copied path round-trips. Covers the node kinds beyond
    /// Rust/C++/Java: TS/JS `internal_module` (`namespace`/`module`) and C#
    /// `namespace_declaration`.
    #[test]
    fn index_keeps_namespace_path_transparent_across_languages() {
        let extractor = SymbolExtractor::new();
        let path = |symbols: &[Symbol], name: &str| {
            symbols
                .iter()
                .find(|s| s.name == name)
                .and_then(|s| s.name_path.clone())
        };

        // TypeScript: `namespace` and `module` both parse as `internal_module`.
        let ts = "namespace NS { export class Outer { method(): void {} } \
                  export function freeFn(): void {} } \
                  module Ambient { export class Thing { go(): void {} } }";
        let ts_syms = extractor.extract(Path::new("a"), ts, Language::TypeScript);
        assert_eq!(path(&ts_syms, "Outer"), Some("Outer".to_string()));
        assert_eq!(path(&ts_syms, "method"), Some("Outer/method".to_string()));
        assert_eq!(path(&ts_syms, "freeFn"), Some("freeFn".to_string()));
        assert_eq!(path(&ts_syms, "Thing"), Some("Thing".to_string()));

        // C#: block-scoped `namespace`. An enclosing type still qualifies the
        // inner type, but the namespace never does.
        let cs = "namespace MyApp { public class Outer { public void Method() {} \
                  public class Inner { public void InnerMethod() {} } } }";
        let cs_syms = extractor.extract(Path::new("a"), cs, Language::CSharp);
        assert_eq!(path(&cs_syms, "Outer"), Some("Outer".to_string()));
        assert_eq!(path(&cs_syms, "Method"), Some("Outer/Method".to_string()));
        assert_eq!(path(&cs_syms, "Inner"), Some("Outer/Inner".to_string()));
        assert_eq!(
            path(&cs_syms, "InnerMethod"),
            Some("Inner/InnerMethod".to_string())
        );

        for s in ts_syms.iter().chain(cs_syms.iter()) {
            let np = s.name_path.as_deref().unwrap_or_default();
            assert!(
                !np.starts_with("NS/") && !np.starts_with("Ambient/") && !np.starts_with("MyApp/"),
                "namespace leaked into name_path: {np:?}"
            );
        }
    }

    /// A symbol's recorded position must land on its NAME identifier, not the
    /// item's leading keyword — otherwise `refs`/`def` on the indexed position
    /// resolve to the wrong symbol (or nothing), and the index/LSP workspace
    /// passes can't dedup to one row.
    #[test]
    fn index_anchors_symbols_at_their_name() {
        let extractor = SymbolExtractor::new();
        let src = "pub fn alpha() {}\nstruct Bravo;\nimpl Bravo { pub fn charlie(&self) {} }\n";
        let syms = extractor.extract(Path::new("a"), src, Language::Rust);
        let on_name = |name: &str| {
            let s = syms
                .iter()
                .find(|s| s.name == name)
                .unwrap_or_else(|| panic!("{name} not extracted"));
            let line = src.lines().nth((s.location.line - 1) as usize).unwrap();
            let col0 = (s.location.column - 1) as usize;
            line[col0..].starts_with(name)
        };
        assert!(on_name("alpha"), "function anchored off its name");
        assert!(on_name("Bravo"), "struct anchored off its name");
        assert!(on_name("charlie"), "method anchored off its name");
    }

    #[test]
    fn go_type_declaration_classifies_by_underlying_type() {
        let extractor = SymbolExtractor::new();
        let content = r#"
func main() {}
type Config struct {}
type Reader interface {}
type Celsius float64
"#;
        let symbols = extractor.extract(Path::new("a"), content, Language::Go);
        let kind = |name: &str| symbols.iter().find(|s| s.name == name).map(|s| s.kind);
        assert_eq!(kind("main"), Some(SymbolKind::Function));
        assert_eq!(kind("Config"), Some(SymbolKind::Struct));
        assert_eq!(kind("Reader"), Some(SymbolKind::Interface));
        assert_eq!(kind("Celsius"), Some(SymbolKind::Class));
    }

    #[test]
    fn kotlin_object_is_a_class_not_a_variable() {
        let extractor = SymbolExtractor::new();
        let content = r#"
class Widget {}
object Singleton {}
fun build() {}
"#;
        let symbols = extractor.extract(Path::new("a"), content, Language::Kotlin);
        let kind = |name: &str| symbols.iter().find(|s| s.name == name).map(|s| s.kind);
        assert_eq!(kind("Widget"), Some(SymbolKind::Class));
        assert_eq!(kind("Singleton"), Some(SymbolKind::Class));
        assert_eq!(kind("build"), Some(SymbolKind::Function));
    }

    #[test]
    fn python_symbol_extraction() {
        let extractor = SymbolExtractor::new();
        let content = r#"
def hello():
    pass

class MyClass:
    pass
"#;
        let symbols = extractor.extract(Path::new("a"), content, Language::Python);
        let kind = |name: &str| symbols.iter().find(|s| s.name == name).map(|s| s.kind);
        assert_eq!(kind("hello"), Some(SymbolKind::Function));
        assert_eq!(kind("MyClass"), Some(SymbolKind::Class));
    }

    #[test]
    fn typescript_symbol_extraction() {
        let extractor = SymbolExtractor::new();
        let content = r#"
function greet() {}
class Service {}
interface Shape {}
enum Color { Red, Blue }
"#;
        let symbols = extractor.extract(Path::new("a"), content, Language::TypeScript);
        let kind = |name: &str| symbols.iter().find(|s| s.name == name).map(|s| s.kind);
        assert_eq!(kind("greet"), Some(SymbolKind::Function));
        assert_eq!(kind("Service"), Some(SymbolKind::Class));
        assert_eq!(kind("Shape"), Some(SymbolKind::Interface));
        assert_eq!(kind("Color"), Some(SymbolKind::Enum));
    }

    #[test]
    fn typescript_module_scope_function_declarators_are_functions() {
        let extractor = SymbolExtractor::new();
        let content = r#"
const greet = (x: number) => x;
export const handler = async () => {};
const fexpr = function named() {};
var legacy = () => {};
const gen = function* () {};
function* topgen() {}
const config = makeConfig();
const VERSION = "1.0";
const klass = class {};
const { a, b } = obj;
const [c, d] = arr;
function outer() {
    const inner = () => {};
    for (let i = 0; i < 10; i++) {}
}
"#;
        let symbols = extractor.extract(Path::new("a"), content, Language::TypeScript);
        let kind = |name: &str| symbols.iter().find(|s| s.name == name).map(|s| s.kind);
        // Module-scope function-valued declarators are Functions (callable),
        // not low-level Variables.
        assert_eq!(kind("greet"), Some(SymbolKind::Function));
        assert_eq!(kind("handler"), Some(SymbolKind::Function));
        assert_eq!(kind("fexpr"), Some(SymbolKind::Function));
        assert_eq!(kind("legacy"), Some(SymbolKind::Function));
        // A generator expression is a callable function value, indexed like the
        // arrow and function-expression forms.
        assert_eq!(kind("gen"), Some(SymbolKind::Function));
        // A top-level generator declaration (`function* g(){}`) is a distinct
        // node kind from `function_declaration` — indexed as a Function too.
        assert_eq!(kind("topgen"), Some(SymbolKind::Function));
        // Non-function initializers are never captured by the value-filtered
        // query (they would only ever be Variables, which are not indexed here).
        assert_eq!(kind("config"), None);
        assert_eq!(kind("VERSION"), None);
        assert_eq!(kind("klass"), None);
        // Destructuring patterns never emit a brace-named symbol.
        assert_eq!(kind("a"), None);
        assert_eq!(kind("b"), None);
        // Nested locals and loop counters are excluded by the module-scope
        // anchor, so JS/TS indexing stays free of per-statement noise.
        assert_eq!(kind("inner"), None);
        assert_eq!(kind("i"), None);
        // A function-valued declarator is classified Function, which is callable
        // (not is_low_level), so it survives exclude_low_level — a plain Variable
        // would not.
        assert!(!SymbolKind::Function.is_low_level());
        assert!(kind("greet").is_some_and(|k| !k.is_low_level()));
    }

    #[test]
    fn javascript_module_scope_const_arrows_match_typescript() {
        let extractor = SymbolExtractor::new();
        let content = r#"
const greet = () => {};
export const handler = function () {};
function outer() {
    const inner = () => {};
}
"#;
        let symbols = extractor.extract(Path::new("a"), content, Language::JavaScript);
        let kind = |name: &str| symbols.iter().find(|s| s.name == name).map(|s| s.kind);
        assert_eq!(kind("greet"), Some(SymbolKind::Function));
        assert_eq!(kind("handler"), Some(SymbolKind::Function));
        // The bare (variable_declarator) capture is gone: no nested-local noise.
        assert_eq!(kind("inner"), None);
    }

    #[test]
    fn javascript_symbol_extraction() {
        let extractor = SymbolExtractor::new();
        let content = r#"
function greet() {}
class Service {}
"#;
        let symbols = extractor.extract(Path::new("a"), content, Language::JavaScript);
        let kind = |name: &str| symbols.iter().find(|s| s.name == name).map(|s| s.kind);
        assert_eq!(kind("greet"), Some(SymbolKind::Function));
        assert_eq!(kind("Service"), Some(SymbolKind::Class));
    }

    #[test]
    fn java_symbol_extraction() {
        let extractor = SymbolExtractor::new();
        let content = r#"
class Service {}
interface Shape {}
enum Color { RED, BLUE }
"#;
        let symbols = extractor.extract(Path::new("a"), content, Language::Java);
        let kind = |name: &str| symbols.iter().find(|s| s.name == name).map(|s| s.kind);
        assert_eq!(kind("Service"), Some(SymbolKind::Class));
        assert_eq!(kind("Shape"), Some(SymbolKind::Interface));
        assert_eq!(kind("Color"), Some(SymbolKind::Enum));
    }

    #[test]
    fn cpp_symbol_extraction() {
        let extractor = SymbolExtractor::new();
        let content = r#"
class Widget {};
struct Point {};
void run() {}
"#;
        let symbols = extractor.extract(Path::new("a"), content, Language::Cpp);
        let kind = |name: &str| symbols.iter().find(|s| s.name == name).map(|s| s.kind);
        assert_eq!(kind("Widget"), Some(SymbolKind::Class));
        assert_eq!(kind("Point"), Some(SymbolKind::Struct));
        assert_eq!(kind("run"), Some(SymbolKind::Function));
    }

    #[test]
    fn csharp_symbol_extraction() {
        let extractor = SymbolExtractor::new();
        let content = r#"
class Service {}
interface IShape {}
struct Point {}
enum Color { Red }
"#;
        let symbols = extractor.extract(Path::new("a"), content, Language::CSharp);
        let kind = |name: &str| symbols.iter().find(|s| s.name == name).map(|s| s.kind);
        assert_eq!(kind("Service"), Some(SymbolKind::Class));
        assert_eq!(kind("IShape"), Some(SymbolKind::Interface));
        assert_eq!(kind("Point"), Some(SymbolKind::Struct));
        assert_eq!(kind("Color"), Some(SymbolKind::Enum));
    }

    #[test]
    fn php_symbol_extraction() {
        let extractor = SymbolExtractor::new();
        let content = r#"<?php
function greet() {}
class Service {}
interface Shape {}
"#;
        let symbols = extractor.extract(Path::new("a"), content, Language::PHP);
        let kind = |name: &str| symbols.iter().find(|s| s.name == name).map(|s| s.kind);
        assert_eq!(kind("greet"), Some(SymbolKind::Function));
        assert_eq!(kind("Service"), Some(SymbolKind::Class));
        assert_eq!(kind("Shape"), Some(SymbolKind::Interface));
    }
}
