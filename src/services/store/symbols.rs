use std::sync::Mutex;
use streaming_iterator::StreamingIterator;
use tree_sitter::{Parser, Query, QueryCursor};

use crate::models::symbol::{Language, SymbolKind};

pub struct ExtractedSymbol {
    pub name: String,
    pub container: Option<String>,
    pub name_path: Option<String>,
    pub kind: SymbolKind,
    pub line: u32,
    pub column: u32,
}

pub struct SymbolExtractor {
    rust: Mutex<Parser>,
    go: Mutex<Parser>,
    python: Mutex<Parser>,
    typescript: Mutex<Parser>,
    javascript: Mutex<Parser>,
    java: Mutex<Parser>,
    kotlin: Mutex<Parser>,
    cpp: Mutex<Parser>,
    csharp: Mutex<Parser>,
    php: Mutex<Parser>,
}

impl Default for SymbolExtractor {
    fn default() -> Self {
        Self::new()
    }
}

impl SymbolExtractor {
    pub fn new() -> Self {
        Self {
            rust: Mutex::new(Self::create_parser(tree_sitter_rust::LANGUAGE.into())),
            go: Mutex::new(Self::create_parser(tree_sitter_go::LANGUAGE.into())),
            python: Mutex::new(Self::create_parser(tree_sitter_python::LANGUAGE.into())),
            typescript: Mutex::new(Self::create_parser(
                tree_sitter_typescript::LANGUAGE_TSX.into(),
            )),
            javascript: Mutex::new(Self::create_parser(tree_sitter_javascript::LANGUAGE.into())),
            java: Mutex::new(Self::create_parser(tree_sitter_java::LANGUAGE.into())),
            kotlin: Mutex::new(Self::create_parser(tree_sitter_kotlin_sg::LANGUAGE.into())),
            cpp: Mutex::new(Self::create_parser(tree_sitter_cpp::LANGUAGE.into())),
            csharp: Mutex::new(Self::create_parser(tree_sitter_c_sharp::LANGUAGE.into())),
            php: Mutex::new(Self::create_parser(tree_sitter_php::LANGUAGE_PHP.into())),
        }
    }

    fn create_parser(language: tree_sitter::Language) -> Parser {
        let mut parser = Parser::new();
        parser.set_language(&language).ok();
        parser
    }

    pub fn extract(&self, content: &str, language: Language) -> Vec<ExtractedSymbol> {
        let Some((parser, ts_lang, query_str)) = self.get_parser_info(language) else {
            return Vec::new();
        };

        let Ok(mut parser_guard) = parser.lock() else {
            return Vec::new();
        };

        let Some(tree) = parser_guard.parse(content, None) else {
            return Vec::new();
        };

        let Ok(query) = Query::new(&ts_lang, query_str) else {
            return Vec::new();
        };

        let mut cursor = QueryCursor::new();
        let mut symbols = Vec::new();
        let mut matches = cursor.matches(&query, tree.root_node(), content.as_bytes());

        while let Some(m) = matches.next() {
            if let Some(symbol) = self.extract_from_match(m, content, language) {
                symbols.push(symbol);
            }
        }

        symbols
    }

    fn get_parser_info(
        &self,
        language: Language,
    ) -> Option<(&Mutex<Parser>, tree_sitter::Language, &'static str)> {
        match language {
            Language::Rust => Some((&self.rust, tree_sitter_rust::LANGUAGE.into(), RUST_QUERY)),
            Language::Go => Some((&self.go, tree_sitter_go::LANGUAGE.into(), GO_QUERY)),
            Language::Python => Some((
                &self.python,
                tree_sitter_python::LANGUAGE.into(),
                PYTHON_QUERY,
            )),
            Language::TypeScript => Some((
                &self.typescript,
                tree_sitter_typescript::LANGUAGE_TSX.into(),
                TYPESCRIPT_QUERY,
            )),
            Language::JavaScript => Some((
                &self.javascript,
                tree_sitter_javascript::LANGUAGE.into(),
                JAVASCRIPT_QUERY,
            )),
            Language::Java => Some((&self.java, tree_sitter_java::LANGUAGE.into(), JAVA_QUERY)),
            Language::Kotlin => Some((
                &self.kotlin,
                tree_sitter_kotlin_sg::LANGUAGE.into(),
                KOTLIN_QUERY,
            )),
            Language::Cpp => Some((&self.cpp, tree_sitter_cpp::LANGUAGE.into(), CPP_QUERY)),
            Language::CSharp => Some((
                &self.csharp,
                tree_sitter_c_sharp::LANGUAGE.into(),
                CSHARP_QUERY,
            )),
            Language::PHP => Some((&self.php, tree_sitter_php::LANGUAGE_PHP.into(), PHP_QUERY)),
            _ => None,
        }
    }

    fn extract_from_match(
        &self,
        m: &tree_sitter::QueryMatch,
        content: &str,
        language: Language,
    ) -> Option<ExtractedSymbol> {
        let capture = m.captures.first()?;
        let node = capture.node;

        let (name, kind) = self.extract_name_and_kind(node, content, language)?;
        let container = self.extract_container_path(node, content, language);
        let name_path = Some(match &container {
            Some(container) => format!("{container}/{name}"),
            None => name.clone(),
        });

        let start = node.start_position();

        Some(ExtractedSymbol {
            name,
            container,
            name_path,
            kind,
            line: start.row as u32 + 1,
            column: start.column as u32 + 1,
        })
    }

    fn extract_container_path(
        &self,
        mut node: tree_sitter::Node,
        content: &str,
        language: Language,
    ) -> Option<String> {
        let mut parts = Vec::new();

        while let Some(parent) = node.parent() {
            node = parent;
            if let Some((name, _)) = self.extract_name_and_kind(node, content, language)
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
        &self,
        node: tree_sitter::Node,
        content: &str,
        language: Language,
    ) -> Option<(String, SymbolKind)> {
        let node_type = node.kind();
        let kind = node_type_to_symbol_kind(node_type, language);

        // Find name child node based on language conventions
        let name_node = find_name_node(node, language)?;
        let name = content
            .get(name_node.start_byte()..name_node.end_byte())?
            .to_string();

        if name.is_empty() || name.starts_with('_') && name.len() == 1 {
            return None;
        }

        Some((name, kind))
    }
}

fn find_name_node(node: tree_sitter::Node, language: Language) -> Option<tree_sitter::Node> {
    // Language-specific name field extraction
    let name_field = match language {
        Language::Rust => node.child_by_field_name("name"),
        Language::Go => node.child_by_field_name("name"),
        Language::Python => node.child_by_field_name("name"),
        Language::TypeScript | Language::JavaScript => node.child_by_field_name("name"),
        Language::Java => node.child_by_field_name("name"),
        Language::Kotlin => node.child_by_field_name("name").or_else(|| {
            // Kotlin function uses simple_identifier
            node.child_by_field_name("simple_identifier")
        }),
        Language::Cpp => node
            .child_by_field_name("declarator")
            .and_then(|d| d.child_by_field_name("declarator"))
            .or_else(|| node.child_by_field_name("name")),
        Language::CSharp => node.child_by_field_name("name"),
        Language::PHP => node.child_by_field_name("name"),
        _ => None,
    };

    // Fallback: find identifier child
    name_field.or_else(|| {
        let count = node.child_count();
        for i in 0..count {
            if let Some(child) = node.child(i as u32) {
                let kind = child.kind();
                if kind == "identifier"
                    || kind == "name"
                    || kind == "simple_identifier"
                    || kind == "type_identifier"
                    || kind == "property_identifier"
                {
                    return Some(child);
                }
            }
        }
        None
    })
}

fn node_type_to_symbol_kind(node_type: &str, _language: Language) -> SymbolKind {
    match node_type {
        // Functions
        "function_item"
        | "function_definition"
        | "function_declaration"
        | "method_declaration"
        | "method_definition"
        | "arrow_function"
        | "function_expression"
        | "func_literal" => SymbolKind::Function,

        // Methods
        "method_item" => SymbolKind::Method,

        // Classes
        "class_declaration" | "class_definition" | "class_specifier" => SymbolKind::Class,

        // Structs
        "struct_item" | "struct_specifier" | "struct_type" => SymbolKind::Struct,

        // Enums
        "enum_item" | "enum_declaration" | "enum_specifier" => SymbolKind::Enum,

        // Interfaces/Traits
        "interface_declaration" | "interface_type" | "trait_item" | "protocol_declaration" => {
            SymbolKind::Interface
        }

        // Modules
        "mod_item" | "module_declaration" | "namespace_declaration" | "package_declaration" => {
            SymbolKind::Module
        }

        // Types
        "type_item" | "type_alias" | "type_alias_declaration" | "type_declaration" => {
            SymbolKind::TypeParameter
        }

        // Constants
        "const_item" | "const_declaration" => SymbolKind::Constant,

        // Variables
        "let_declaration"
        | "variable_declaration"
        | "var_declaration"
        | "short_var_declaration"
        | "static_item" => SymbolKind::Variable,

        // Fields/Properties
        "field_declaration" | "property_declaration" | "field_definition" => SymbolKind::Field,

        // Impl blocks (Rust-specific)
        "impl_item" => SymbolKind::Class,

        _ => SymbolKind::Variable,
    }
}

// Language-specific tree-sitter queries for symbol extraction

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
(type_declaration (type_spec)) @symbol
(const_declaration) @symbol
(var_declaration) @symbol
"#;

const PYTHON_QUERY: &str = r#"
(function_definition) @symbol
(class_definition) @symbol
"#;

const TYPESCRIPT_QUERY: &str = r#"
(function_declaration) @symbol
(class_declaration) @symbol
(interface_declaration) @symbol
(type_alias_declaration) @symbol
(enum_declaration) @symbol
(method_definition) @symbol
"#;

const JAVASCRIPT_QUERY: &str = r#"
(function_declaration) @symbol
(class_declaration) @symbol
(method_definition) @symbol
(variable_declarator) @symbol
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

    #[test]
    fn test_rust_symbol_extraction() {
        let extractor = SymbolExtractor::new();
        let content = r#"
fn main() {}
struct Foo {}
enum Bar { A, B }
trait Baz {}
impl Foo {}
"#;
        let symbols = extractor.extract(content, Language::Rust);
        assert!(symbols.iter().any(|s| s.name == "main"));
        assert!(symbols.iter().any(|s| s.name == "Foo"));
        assert!(symbols.iter().any(|s| s.name == "Bar"));
        assert!(symbols.iter().any(|s| s.name == "Baz"));
    }

    #[test]
    fn test_go_symbol_extraction() {
        let extractor = SymbolExtractor::new();
        let content = r#"
func main() {}
func (s *Server) Start() {}
type Config struct {}
"#;
        let symbols = extractor.extract(content, Language::Go);
        assert!(symbols.iter().any(|s| s.name == "main"));
    }

    #[test]
    fn test_python_symbol_extraction() {
        let extractor = SymbolExtractor::new();
        let content = r#"
def hello():
    pass

class MyClass:
    pass
"#;
        let symbols = extractor.extract(content, Language::Python);
        assert!(symbols.iter().any(|s| s.name == "hello"));
        assert!(symbols.iter().any(|s| s.name == "MyClass"));
    }
}
