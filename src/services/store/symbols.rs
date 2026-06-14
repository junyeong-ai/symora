use std::collections::HashMap;
use std::sync::Mutex;

use streaming_iterator::StreamingIterator;
use tree_sitter::{Node, Parser, Query, QueryCursor};

use crate::models::symbol::{Language, Symbol, SymbolKind};

pub struct ExtractedSymbol {
    pub name: String,
    pub container: Option<String>,
    pub name_path: Option<String>,
    pub kind: SymbolKind,
    pub line: u32,
    pub column: u32,
}

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
        ]
    }

    pub fn is_supported(language: Language) -> bool {
        Self::supported_languages().contains(&language)
    }

    pub fn extract(&self, content: &str, language: Language) -> Vec<ExtractedSymbol> {
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
            if let Some(symbol) = extract_from_match(m, content, language) {
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
    content: &str,
    language: Language,
) -> Option<ExtractedSymbol> {
    let node = m.captures.first()?.node;

    let (name, kind) = extract_name_and_kind(node, content, language)?;
    let container = extract_container_path(node, content, language);
    let name_path = Some(match &container {
        Some(container) => format!("{container}/{name}"),
        None => name.clone(),
    });

    // tree-sitter reports the column as a byte offset within the line, but
    // CLI/JSON positions are character columns (matching the LSP side). Count
    // characters across the line prefix so multibyte text before a symbol
    // doesn't misplace follow-up `file:line:col` navigation.
    let start = node.start_position();
    let line_start = node.start_byte() - start.column;
    let column = content
        .get(line_start..node.start_byte())
        .map(|prefix| prefix.chars().count() as u32)
        .unwrap_or(start.column as u32)
        + 1;
    Some(ExtractedSymbol {
        name,
        container,
        name_path,
        kind,
        line: start.row as u32 + 1,
        column,
    })
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

fn find_name_node(node: Node, language: Language) -> Option<Node> {
    let name_field = match language {
        Language::Kotlin => node
            .child_by_field_name("name")
            .or_else(|| node.child_by_field_name("simple_identifier")),
        Language::Cpp => node
            .child_by_field_name("declarator")
            .and_then(|d| d.child_by_field_name("declarator"))
            .or_else(|| node.child_by_field_name("name")),
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
        | "generator_function_declaration" => SymbolKind::Function,

        "method_item" | "method_declaration" | "method_definition" => SymbolKind::Method,

        "class_declaration" | "class_definition" | "class_specifier" | "object_declaration"
        | "impl_item" => SymbolKind::Class,

        "struct_item" | "struct_specifier" | "struct_type" | "struct_declaration" => {
            SymbolKind::Struct
        }

        "enum_item" | "enum_declaration" | "enum_specifier" => SymbolKind::Enum,

        "interface_declaration"
        | "interface_type"
        | "trait_item"
        | "trait_declaration"
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
        "type_item" | "type_alias_declaration" => SymbolKind::Class,

        "const_item" | "const_spec" => SymbolKind::Constant,

        "static_item" | "var_spec" => SymbolKind::Variable,

        // JS/TS `const f = () => {}` is a callable; classify by the
        // initializer rather than always Variable (which is_low_level would
        // drop under exclude_low_level).
        "variable_declarator" => declarator_kind(node),

        "property_declaration" => SymbolKind::Property,
        "field_declaration" => SymbolKind::Field,

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
(struct_item) @symbol
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
(interface_declaration) @symbol
(type_alias_declaration) @symbol
(enum_declaration) @symbol
(method_definition) @symbol
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
(class_declaration) @symbol
(interface_declaration) @symbol
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
(class_declaration) @symbol
(interface_declaration) @symbol
(struct_declaration) @symbol
(enum_declaration) @symbol
(method_declaration) @symbol
(property_declaration) @symbol
(field_declaration) @symbol
"#;

const PHP_QUERY: &str = r#"
(function_definition) @symbol
(class_declaration) @symbol
(interface_declaration) @symbol
(trait_declaration) @symbol
(method_declaration) @symbol
(property_declaration) @symbol
"#;

#[cfg(test)]
mod tests {
    use super::*;

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
    fn extractor_support_is_static_and_distinct_from_ast() {
        assert!(SymbolExtractor::is_supported(Language::Rust));
        assert!(!SymbolExtractor::is_supported(Language::Ruby));
        assert!(crate::infra::ast::is_supported(Language::Ruby));
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
        let symbols = extractor.extract(content, Language::Rust);
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
            let symbols = extractor.extract(src, Language::Rust);
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
        let symbols = extractor.extract(src, Language::Rust);
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
        let symbols = extractor.extract(src, Language::Cpp);
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
        let path = |symbols: &[ExtractedSymbol], name: &str| {
            symbols
                .iter()
                .find(|s| s.name == name)
                .and_then(|s| s.name_path.clone())
        };

        // TypeScript: `namespace` and `module` both parse as `internal_module`.
        let ts = "namespace NS { export class Outer { method(): void {} } \
                  export function freeFn(): void {} } \
                  module Ambient { export class Thing { go(): void {} } }";
        let ts_syms = extractor.extract(ts, Language::TypeScript);
        assert_eq!(path(&ts_syms, "Outer"), Some("Outer".to_string()));
        assert_eq!(path(&ts_syms, "method"), Some("Outer/method".to_string()));
        assert_eq!(path(&ts_syms, "freeFn"), Some("freeFn".to_string()));
        assert_eq!(path(&ts_syms, "Thing"), Some("Thing".to_string()));

        // C#: block-scoped `namespace`. An enclosing type still qualifies the
        // inner type, but the namespace never does.
        let cs = "namespace MyApp { public class Outer { public void Method() {} \
                  public class Inner { public void InnerMethod() {} } } }";
        let cs_syms = extractor.extract(cs, Language::CSharp);
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

    #[test]
    fn go_type_declaration_classifies_by_underlying_type() {
        let extractor = SymbolExtractor::new();
        let content = r#"
func main() {}
type Config struct {}
type Reader interface {}
type Celsius float64
"#;
        let symbols = extractor.extract(content, Language::Go);
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
        let symbols = extractor.extract(content, Language::Kotlin);
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
        let symbols = extractor.extract(content, Language::Python);
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
        let symbols = extractor.extract(content, Language::TypeScript);
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
        let symbols = extractor.extract(content, Language::TypeScript);
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
        let symbols = extractor.extract(content, Language::JavaScript);
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
        let symbols = extractor.extract(content, Language::JavaScript);
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
        let symbols = extractor.extract(content, Language::Java);
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
        let symbols = extractor.extract(content, Language::Cpp);
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
        let symbols = extractor.extract(content, Language::CSharp);
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
        let symbols = extractor.extract(content, Language::PHP);
        let kind = |name: &str| symbols.iter().find(|s| s.name == name).map(|s| s.kind);
        assert_eq!(kind("greet"), Some(SymbolKind::Function));
        assert_eq!(kind("Service"), Some(SymbolKind::Class));
        assert_eq!(kind("Shape"), Some(SymbolKind::Interface));
    }
}
