//! Tree-sitter Node Type Mappings
//!
//! Verified node types from official tree-sitter grammar repositories.
//! Each mapping is sourced from the respective `src/node-types.json`.

use crate::models::symbol::Language;

/// Node type mapping entry
#[derive(Debug, Clone, Copy)]
pub struct NodeType {
    /// User-friendly category name
    pub category: &'static str,
    /// Actual tree-sitter node type
    pub node_type: &'static str,
    /// Example syntax
    pub example: &'static str,
}

impl NodeType {
    const fn new(category: &'static str, node_type: &'static str, example: &'static str) -> Self {
        Self {
            category,
            node_type,
            example,
        }
    }
}

/// Get node type mappings for a language
pub fn get_node_types(language: Language) -> &'static [NodeType] {
    match language {
        Language::Python => PYTHON,
        Language::TypeScript => TYPESCRIPT,
        Language::JavaScript => JAVASCRIPT,
        Language::Rust => RUST,
        Language::Go => GO,
        Language::Java => JAVA,
        Language::Kotlin => KOTLIN,
        Language::Cpp => CPP,
        Language::CSharp => CSHARP,
        Language::Bash => BASH,
        Language::Ruby => RUBY,
        Language::Lua => LUA,
        Language::PHP => PHP,
        _ => &[],
    }
}

/// Check if AST search is supported
pub fn is_supported(language: Language) -> bool {
    !get_node_types(language).is_empty()
}

/// Get all AST-supported languages
pub fn supported_languages() -> &'static [Language] {
    &[
        Language::Python,
        Language::TypeScript,
        Language::JavaScript,
        Language::Rust,
        Language::Go,
        Language::Java,
        Language::Kotlin,
        Language::Cpp,
        Language::CSharp,
        Language::Bash,
        Language::Ruby,
        Language::Lua,
        Language::PHP,
    ]
}

/// Format query error with helpful hints
pub fn format_query_error(language: Language, error: &str) -> String {
    let nodes = get_node_types(language);

    if nodes.is_empty() {
        return format!(
            "AST search not supported for {:?}.\n\nSupported languages: {}",
            language,
            supported_languages()
                .iter()
                .map(|l| l.lsp_id())
                .collect::<Vec<_>>()
                .join(", ")
        );
    }

    let examples: String = nodes
        .iter()
        .take(5)
        .map(|n| format!("  ({:<24}) # {}: {}", n.node_type, n.category, n.example))
        .collect::<Vec<_>>()
        .join("\n");

    format!(
        "Invalid query: {}\n\nCommon node types for {}:\n{}\n\nUse: symora search nodes -l {}",
        error,
        language.lsp_id(),
        examples,
        language.lsp_id()
    )
}

// =============================================================================
// Python - tree-sitter-python/src/node-types.json
// =============================================================================
const PYTHON: &[NodeType] = &[
    NodeType::new("class", "class_definition", "class MyClass:"),
    NodeType::new("function", "function_definition", "def my_func():"),
    NodeType::new("decorator", "decorated_definition", "@decorator def/class"),
    NodeType::new("import", "import_statement", "import module"),
    NodeType::new("import", "import_from_statement", "from x import y"),
    NodeType::new("assignment", "assignment", "x = value"),
    NodeType::new("if", "if_statement", "if condition:"),
    NodeType::new("for", "for_statement", "for x in items:"),
    NodeType::new("while", "while_statement", "while condition:"),
    NodeType::new("try", "try_statement", "try: ... except:"),
    NodeType::new("with", "with_statement", "with ctx as x:"),
    NodeType::new("lambda", "lambda", "lambda x: x"),
];

// =============================================================================
// TypeScript - tree-sitter-typescript/typescript/src/node-types.json
// =============================================================================
const TYPESCRIPT: &[NodeType] = &[
    NodeType::new("class", "class_declaration", "class MyClass {}"),
    NodeType::new("class", "abstract_class_declaration", "abstract class X {}"),
    NodeType::new("function", "function_declaration", "function myFunc() {}"),
    NodeType::new("function", "arrow_function", "const fn = () => {}"),
    NodeType::new("method", "method_definition", "myMethod() {}"),
    NodeType::new("interface", "interface_declaration", "interface I {}"),
    NodeType::new("type", "type_alias_declaration", "type T = ..."),
    NodeType::new("enum", "enum_declaration", "enum E {}"),
    NodeType::new("import", "import_statement", "import x from 'y'"),
    NodeType::new("export", "export_statement", "export { x }"),
    NodeType::new("variable", "lexical_declaration", "const/let x = ..."),
    NodeType::new("variable", "variable_declaration", "var x = ..."),
];

// =============================================================================
// JavaScript - tree-sitter-javascript/src/node-types.json
// =============================================================================
const JAVASCRIPT: &[NodeType] = &[
    NodeType::new("class", "class_declaration", "class MyClass {}"),
    NodeType::new("function", "function_declaration", "function myFunc() {}"),
    NodeType::new("function", "arrow_function", "const fn = () => {}"),
    NodeType::new(
        "function",
        "generator_function_declaration",
        "function* gen() {}",
    ),
    NodeType::new("method", "method_definition", "myMethod() {}"),
    NodeType::new("import", "import_statement", "import x from 'y'"),
    NodeType::new("export", "export_statement", "export { x }"),
    NodeType::new("variable", "lexical_declaration", "const/let x = ..."),
    NodeType::new("variable", "variable_declaration", "var x = ..."),
    NodeType::new("if", "if_statement", "if (cond) {}"),
    NodeType::new("for", "for_statement", "for (;;) {}"),
    NodeType::new("for", "for_in_statement", "for (x in obj) {}"),
];

// =============================================================================
// Rust - tree-sitter-rust/src/node-types.json
// =============================================================================
const RUST: &[NodeType] = &[
    NodeType::new("struct", "struct_item", "struct S {}"),
    NodeType::new("enum", "enum_item", "enum E {}"),
    NodeType::new("function", "function_item", "fn my_func() {}"),
    NodeType::new("trait", "trait_item", "trait T {}"),
    NodeType::new("impl", "impl_item", "impl T for S {}"),
    NodeType::new("module", "mod_item", "mod my_mod {}"),
    NodeType::new("use", "use_declaration", "use crate::x;"),
    NodeType::new("type", "type_item", "type T = ...;"),
    NodeType::new("const", "const_item", "const X: T = ...;"),
    NodeType::new("static", "static_item", "static X: T = ...;"),
    NodeType::new("macro", "macro_definition", "macro_rules! m {}"),
    NodeType::new("attribute", "attribute_item", "#[derive(...)]"),
];

// =============================================================================
// Go - tree-sitter-go/src/node-types.json
// =============================================================================
const GO: &[NodeType] = &[
    NodeType::new("struct", "struct_type", "type S struct {}"),
    NodeType::new("interface", "interface_type", "type I interface {}"),
    NodeType::new("function", "function_declaration", "func myFunc() {}"),
    NodeType::new("method", "method_declaration", "func (r *R) M() {}"),
    NodeType::new("type", "type_declaration", "type T ..."),
    NodeType::new("type", "type_alias", "type T = Other"),
    NodeType::new("import", "import_declaration", "import \"pkg\""),
    NodeType::new("const", "const_declaration", "const X = ..."),
    NodeType::new("var", "var_declaration", "var x = ..."),
    NodeType::new("var", "short_var_declaration", "x := value"),
    NodeType::new("if", "if_statement", "if cond {}"),
    NodeType::new("for", "for_statement", "for i := range x {}"),
];

// =============================================================================
// Java - tree-sitter-java/src/node-types.json
// =============================================================================
const JAVA: &[NodeType] = &[
    NodeType::new("class", "class_declaration", "class MyClass {}"),
    NodeType::new("interface", "interface_declaration", "interface I {}"),
    NodeType::new("enum", "enum_declaration", "enum E {}"),
    NodeType::new("record", "record_declaration", "record R() {}"),
    NodeType::new("method", "method_declaration", "void m() {}"),
    NodeType::new("constructor", "constructor_declaration", "MyClass() {}"),
    NodeType::new("field", "field_declaration", "private int x;"),
    NodeType::new("import", "import_declaration", "import java.util.*;"),
    NodeType::new(
        "annotation",
        "annotation_type_declaration",
        "@interface A {}",
    ),
    NodeType::new("annotation", "annotation", "@Override"),
    NodeType::new("if", "if_statement", "if (cond) {}"),
    NodeType::new("for", "for_statement", "for (;;) {}"),
];

// =============================================================================
// Kotlin - fwcd/tree-sitter-kotlin/src/node-types.json
// =============================================================================
const KOTLIN: &[NodeType] = &[
    NodeType::new("class", "class_declaration", "class MyClass {}"),
    NodeType::new("object", "object_declaration", "object Singleton {}"),
    NodeType::new("companion", "companion_object", "companion object {}"),
    NodeType::new("function", "function_declaration", "fun myFunc() {}"),
    NodeType::new("function", "anonymous_function", "fun() { ... }"),
    NodeType::new("property", "property_declaration", "val/var x = ..."),
    NodeType::new("import", "import_header", "import pkg.Class"),
    NodeType::new("type", "type_alias", "typealias T = ..."),
    NodeType::new("enum", "enum_entry", "enum class entries"),
    NodeType::new("annotation", "annotation", "@Annotation"),
    NodeType::new("if", "if_expression", "if (cond) {} else {}"),
    NodeType::new("when", "when_expression", "when (x) {}"),
];

// =============================================================================
// C++ - tree-sitter-cpp/src/node-types.json
// =============================================================================
const CPP: &[NodeType] = &[
    NodeType::new("class", "class_specifier", "class MyClass {}"),
    NodeType::new("struct", "struct_specifier", "struct S {}"),
    NodeType::new("function", "function_definition", "void f() {}"),
    NodeType::new("namespace", "namespace_definition", "namespace ns {}"),
    NodeType::new("template", "template_declaration", "template<T> ..."),
    NodeType::new("enum", "enum_specifier", "enum E {}"),
    NodeType::new("field", "field_declaration", "int member;"),
    NodeType::new("typedef", "type_definition", "typedef ... T;"),
    NodeType::new("concept", "concept_definition", "concept C = ..."),
    NodeType::new("using", "using_declaration", "using ns::name;"),
    NodeType::new("if", "if_statement", "if (cond) {}"),
    NodeType::new("for", "for_statement", "for (;;) {}"),
];

// =============================================================================
// C# - tree-sitter-c-sharp/src/node-types.json
// =============================================================================
const CSHARP: &[NodeType] = &[
    NodeType::new("class", "class_declaration", "class MyClass {}"),
    NodeType::new("interface", "interface_declaration", "interface I {}"),
    NodeType::new("struct", "struct_declaration", "struct S {}"),
    NodeType::new("enum", "enum_declaration", "enum E {}"),
    NodeType::new("record", "record_declaration", "record R() {}"),
    NodeType::new("method", "method_declaration", "void Method() {}"),
    NodeType::new("constructor", "constructor_declaration", "MyClass() {}"),
    NodeType::new("property", "property_declaration", "public int X { get; }"),
    NodeType::new("field", "field_declaration", "private int _x;"),
    NodeType::new("namespace", "namespace_declaration", "namespace Ns {}"),
    NodeType::new("using", "using_directive", "using System;"),
    NodeType::new("delegate", "delegate_declaration", "delegate void D();"),
];

// =============================================================================
// Bash - tree-sitter-bash/src/node-types.json
// =============================================================================
const BASH: &[NodeType] = &[
    NodeType::new("function", "function_definition", "my_func() {}"),
    NodeType::new("variable", "variable_assignment", "VAR=value"),
    NodeType::new("if", "if_statement", "if cond; then"),
    NodeType::new("case", "case_statement", "case $x in"),
    NodeType::new("for", "for_statement", "for x in items; do"),
    NodeType::new("while", "while_statement", "while cond; do"),
    NodeType::new("for", "c_style_for_statement", "for ((i=0;;))"),
    NodeType::new("pipeline", "pipeline", "cmd1 | cmd2"),
    NodeType::new("command", "command", "echo hello"),
    NodeType::new("redirect", "redirected_statement", "cmd > file"),
];

// =============================================================================
// Ruby - tree-sitter-ruby/src/node-types.json
// =============================================================================
const RUBY: &[NodeType] = &[
    NodeType::new("class", "class", "class MyClass"),
    NodeType::new("module", "module", "module MyModule"),
    NodeType::new("method", "method", "def my_method"),
    NodeType::new("method", "singleton_method", "def self.method"),
    NodeType::new("lambda", "lambda", "-> { ... }"),
    NodeType::new("block", "block", "do |x| ... end"),
    NodeType::new("block", "do_block", "do ... end"),
    NodeType::new("assignment", "assignment", "x = value"),
    NodeType::new("if", "if", "if condition"),
    NodeType::new("case", "case", "case value"),
    NodeType::new("while", "while", "while condition"),
    NodeType::new("for", "for", "for x in items"),
];

// =============================================================================
// Lua - tree-sitter-lua/src/node-types.json
// =============================================================================
const LUA: &[NodeType] = &[
    NodeType::new("function", "function_definition", "function f() end"),
    NodeType::new("function", "function_declaration", "local function f()"),
    NodeType::new("variable", "variable_declaration", "local x = ..."),
    NodeType::new("assignment", "assignment_statement", "x = value"),
    NodeType::new("table", "table_constructor", "{ key = val }"),
    NodeType::new("if", "if_statement", "if cond then end"),
    NodeType::new("for", "for_statement", "for i=1,n do end"),
    NodeType::new("for", "for_in_statement", "for k,v in pairs()"),
    NodeType::new("while", "while_statement", "while cond do end"),
    NodeType::new("repeat", "repeat_statement", "repeat until cond"),
];

// =============================================================================
// PHP - tree-sitter-php/php/src/node-types.json
// =============================================================================
const PHP: &[NodeType] = &[
    NodeType::new("class", "class_declaration", "class MyClass {}"),
    NodeType::new("interface", "interface_declaration", "interface I {}"),
    NodeType::new("trait", "trait_declaration", "trait T {}"),
    NodeType::new("enum", "enum_declaration", "enum E {}"),
    NodeType::new("function", "function_definition", "function f() {}"),
    NodeType::new("method", "method_declaration", "public function m()"),
    NodeType::new("namespace", "namespace_definition", "namespace Ns;"),
    NodeType::new("use", "namespace_use_declaration", "use Ns\\Class;"),
    NodeType::new("property", "property_declaration", "public $x;"),
    NodeType::new("const", "const_declaration", "const X = ...;"),
    NodeType::new("if", "if_statement", "if ($cond) {}"),
    NodeType::new("foreach", "foreach_statement", "foreach ($x as $y)"),
];

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_supported_languages() {
        assert!(is_supported(Language::Python));
        assert!(is_supported(Language::Rust));
        assert!(is_supported(Language::TypeScript));
        assert!(is_supported(Language::CSharp));
        assert!(!is_supported(Language::Unknown));
    }

    #[test]
    fn test_node_types_not_empty() {
        for lang in supported_languages() {
            let nodes = get_node_types(*lang);
            assert!(!nodes.is_empty(), "{:?} should have node types", lang);
        }
    }

    #[test]
    fn test_format_query_error() {
        let msg = format_query_error(Language::Python, "syntax error");
        assert!(msg.contains("syntax error"));
        assert!(msg.contains("class_definition"));
    }
}
