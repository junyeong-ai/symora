use std::collections::HashMap;
use std::sync::Mutex;

use streaming_iterator::StreamingIterator;
use tree_sitter::{Node, Parser, Query, QueryCursor};

use crate::models::symbol::{Language, SymbolKind};

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
    let mut parts = Vec::new();
    while let Some(parent) = node.parent() {
        node = parent;
        if let Some((name, _)) = extract_name_and_kind(node, content, language)
            && !name.is_empty()
        {
            parts.push(name);
        }
    }
    parts.reverse();
    if parts.is_empty() {
        None
    } else {
        Some(parts.join("/"))
    }
}

fn extract_name_and_kind(
    node: Node,
    content: &str,
    language: Language,
) -> Option<(String, SymbolKind)> {
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
    // An impl block's container name is its implementing TYPE — the `type`
    // field, never the trait it implements — so a method's path reads
    // `Type/method` regardless of whether the trait is written with a module
    // path, matching the LSP self-type normalization.
    if node.kind() == "impl_item" {
        return node
            .child_by_field_name("type")
            .and_then(first_type_identifier);
    }

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

/// The first `type_identifier` in a type node's subtree — the bare type name
/// of a self type, descending through `generic_type`/`reference_type`/
/// `scoped_type_identifier`/`dynamic_type` wrappers (`Foo<T>`→Foo,
/// `crate::Foo`→Foo, `&Foo`→Foo).
fn first_type_identifier(node: Node) -> Option<Node> {
    if node.kind() == "type_identifier" {
        return Some(node);
    }
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        if let Some(found) = first_type_identifier(child) {
            return Some(found);
        }
    }
    None
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

        "mod_item" | "namespace_definition" | "package_declaration" => SymbolKind::Module,

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

    /// The cross-surface invariant: the tree-sitter index extractor and the
    /// LSP self-type normalizer (`Symbol::normalize_symbol_name`) must key an
    /// impl method under the SAME container, or a `name_path` copied from
    /// `search` would fail against `symbols`/`edit`. This pins both to the same
    /// "first nominal type identifier" rule across nominal, generic, scoped,
    /// reference, trait, and structural (tuple/array/pointer/qualified) self
    /// types. `($display)` is the matching rust-analyzer impl label.
    #[test]
    fn impl_method_container_agrees_with_lsp_normalizer() {
        use crate::models::symbol::Symbol;
        let extractor = SymbolExtractor::new();
        let cases = [
            ("impl Foo { fn m(&self) {} }", "impl Foo", "Foo"),
            (
                "impl<T> Wrap<T> { fn m(&self) {} }",
                "impl<T> Wrap<T>",
                "Wrap",
            ),
            (
                "impl std::fmt::Display for Foo { fn m(&self) {} }",
                "impl std::fmt::Display for Foo",
                "Foo",
            ),
            (
                "impl FromStr for Foo { fn m(&self) {} }",
                "impl FromStr for Foo",
                "Foo",
            ),
            (
                "impl Tr for (A, B) { fn m(&self) {} }",
                "impl Tr for (A, B)",
                "A",
            ),
            (
                "impl Tr for [Elem; 4] { fn m(&self) {} }",
                "impl Tr for [Elem; 4]",
                "Elem",
            ),
            (
                "impl Tr for *const Ptr { fn m(&self) {} }",
                "impl Tr for *const Ptr",
                "Ptr",
            ),
            (
                "impl Tr for <Qual as Baz>::Out { fn m(&self) {} }",
                "impl Tr for <Qual as Baz>::Out",
                "Qual",
            ),
        ];
        for (src, ra_label, expected) in cases {
            let symbols = extractor.extract(src, Language::Rust);
            let method = symbols
                .iter()
                .find(|s| s.name == "m")
                .unwrap_or_else(|| panic!("method not extracted from {src:?}"));
            let index_container = method.container.as_deref();
            assert_eq!(
                index_container,
                Some(expected),
                "index container for {src:?}"
            );
            assert_eq!(
                Symbol::normalize_symbol_name(ra_label),
                expected,
                "LSP normalizer for {ra_label:?}"
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
