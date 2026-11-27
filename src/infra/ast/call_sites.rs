//! The names a file calls, as its syntax tree reads them.
//!
//! A reference a language server reports is a position and nothing else: it
//! does not say whether the name there is called, annotated, imported or
//! passed along. Where a question turns on that — which references to a
//! Python class construct it, and so run its `__init__` — the parse tree
//! answers it, never the text around the position, which cannot tell
//! `Circle(` in a call from the same characters in a string, a comment or a
//! decorator's argument list.
//!
//! Only languages with a written rule are read; every other one returns
//! `None`, so a caller can tell "no call here" from "not read".

use std::collections::HashSet;

use tree_sitter::{Node, Parser};

use crate::models::symbol::Language;

/// The 1-indexed line and Unicode-scalar column of every name `content`
/// calls: `f` in `f(x)`, `Circle` in `shapes.Circle(2.0)` and in
/// `Box[int](x)`. `None` where no rule for `language` is written, or the
/// file does not parse.
pub fn called_names(content: &str, language: Language) -> Option<HashSet<(u32, u32)>> {
    match language {
        Language::Python => python_called_names(content),
        _ => None,
    }
}

fn python_called_names(content: &str) -> Option<HashSet<(u32, u32)>> {
    let mut parser = Parser::new();
    parser
        .set_language(&tree_sitter_python::LANGUAGE.into())
        .ok()?;
    let tree = parser.parse(content, None)?;
    let mut called = HashSet::new();
    let mut pending = vec![tree.root_node()];
    while let Some(node) = pending.pop() {
        if node.kind() == "call"
            && let Some(name) = node.child_by_field_name("function").and_then(python_callee)
        {
            called.insert(scalar_start(content, name));
        }
        let mut cursor = node.walk();
        pending.extend(node.named_children(&mut cursor));
    }
    Some(called)
}

/// The name a Python call's `function` calls: itself where it is a name, the
/// attribute a member access ends in, and what a subscript (a generic's
/// parameters) is applied to. A call of anything else — a call's result, a
/// lambda — calls no name.
fn python_callee(function: Node) -> Option<Node> {
    match function.kind() {
        "identifier" => Some(function),
        "attribute" => function
            .child_by_field_name("attribute")
            .filter(|name| name.kind() == "identifier"),
        "subscript" => function
            .child_by_field_name("value")
            .and_then(python_callee),
        _ => None,
    }
}

/// Where a node starts, as a 1-indexed line and Unicode-scalar column.
fn scalar_start(content: &str, node: Node) -> (u32, u32) {
    let point = node.start_position();
    let byte = node.start_byte();
    let line_start = byte.saturating_sub(point.column);
    let column = content
        .get(line_start..byte)
        .map(|prefix| prefix.chars().count() as u32)
        .unwrap_or(point.column as u32)
        + 1;
    (point.row as u32 + 1, column)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn called(content: &str) -> Vec<(u32, u32)> {
        let mut names: Vec<(u32, u32)> = called_names(content, Language::Python)
            .expect("python has a rule")
            .into_iter()
            .collect();
        names.sort();
        names
    }

    /// A class called by its name, through a module or with generic
    /// parameters is called; the same name annotated, imported, passed or
    /// written in a string or a comment is not.
    #[test]
    fn a_python_name_is_called_only_where_a_call_expression_calls_it() {
        let content = "from shapes import Circle\n\
                       import shapes\n\
                       def make(x: Circle) -> Circle:\n\
                       \x20   register(Circle)\n\
                       \x20   s = \"Circle(1)\"  # Circle(2)\n\
                       \x20   return Circle(1)\n\
                       a = shapes.Circle(2)\n\
                       b = Box[int](3)\n\
                       c = make(Circle)(4)\n";
        assert_eq!(
            called(content),
            [(4, 5), (6, 12), (7, 12), (8, 5), (9, 5)],
            "register, Circle, shapes.Circle, Box and make"
        );
    }

    /// The column is counted in Unicode scalars, as a server's position is
    /// decoded, not in the bytes before the name.
    #[test]
    fn a_called_names_column_counts_characters_not_bytes() {
        assert_eq!(called("é = 1; Circle(1)\n"), [(1, 8)]);
    }

    #[test]
    fn a_language_without_a_rule_is_not_read() {
        assert!(called_names("fn main() { f(); }\n", Language::Rust).is_none());
    }
}
