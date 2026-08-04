//! Line ranges a language's own rules exclude from a production build.
//!
//! A reference landing inside such a range is test code regardless of what
//! the file is named, which is what separates this from the path-based
//! file classification it complements: Rust's dominant idiom puts tests in
//! a `#[cfg(test)] mod tests` inside the very file they exercise, so a
//! path-only answer reports a heavily-tested symbol as having no coverage
//! at all.
//!
//! Only rules the LANGUAGE defines belong here. `#[cfg(test)]` and
//! `#[test]` items are compiled solely into the test harness, so the
//! classification is a compiler fact with no false positives. A framework
//! naming convention (`describe(`, a `TestCase` base class, a `[Test]`
//! attribute) is a guess, and a guess that marks production code as test
//! code silently deflates every coverage and risk signal built on top of
//! it — the opposite of the honest-degradation contract. Languages with no
//! such rule are never parsed and return nothing.

use std::ops::RangeInclusive;

use tree_sitter::{Node, Parser};

use crate::models::symbol::Language;

/// 1-indexed inclusive line ranges of `content` that `language` compiles
/// only under test.
pub fn test_regions(content: &str, language: Language) -> Vec<RangeInclusive<u32>> {
    match language {
        Language::Rust => rust_test_regions(content),
        _ => Vec::new(),
    }
}

/// True when `language` defines test-only regions at all — lets a caller
/// skip reading a file it could learn nothing from.
pub fn has_test_regions(language: Language) -> bool {
    matches!(language, Language::Rust)
}

fn rust_test_regions(content: &str) -> Vec<RangeInclusive<u32>> {
    let mut parser = Parser::new();
    if parser
        .set_language(&tree_sitter_rust::LANGUAGE.into())
        .is_err()
    {
        return Vec::new();
    }
    let Some(tree) = parser.parse(content, None) else {
        return Vec::new();
    };

    let source = content.as_bytes();
    let mut regions = Vec::new();
    collect_rust_regions(tree.root_node(), source, &mut regions);
    regions
}

fn collect_rust_regions(node: Node, source: &[u8], out: &mut Vec<RangeInclusive<u32>>) {
    if let Some(region) = rust_region_at(node, source) {
        out.push(region);
    }
    let mut cursor = node.walk();
    for child in node.named_children(&mut cursor) {
        collect_rust_regions(child, source, out);
    }
}

/// The span an attribute makes test-only, if it does.
///
/// An outer attribute is a sibling preceding the item it decorates, so the
/// span runs from the attribute through the end of the item it lands on.
/// Reaching that item means passing whatever may sit between the two:
/// further attributes, since they stack, and comments, which the grammar
/// admits anywhere as extras. An inner attribute (`#![cfg(test)]`) applies
/// to the item that encloses it instead.
fn rust_region_at(node: Node, source: &[u8]) -> Option<RangeInclusive<u32>> {
    let kind = node.kind();
    if kind != "attribute_item" && kind != "inner_attribute_item" {
        return None;
    }
    if !attribute_is_test_only(node, source) {
        return None;
    }

    let start = node.start_position().row as u32 + 1;
    let end = if kind == "inner_attribute_item" {
        node.parent().unwrap_or(node).end_position().row as u32 + 1
    } else {
        let mut sibling = node.next_named_sibling();
        while sibling.is_some_and(|s| s.kind() == "attribute_item" || s.is_extra()) {
            sibling = sibling.and_then(|s| s.next_named_sibling());
        }
        sibling.unwrap_or(node).end_position().row as u32 + 1
    };

    Some(start..=end.max(start))
}

fn attribute_is_test_only(item: Node, source: &[u8]) -> bool {
    let mut cursor = item.walk();
    let Some(attribute) = item
        .named_children(&mut cursor)
        .find(|c| c.kind() == "attribute")
    else {
        return false;
    };

    let mut parts = attribute.walk();
    let children: Vec<Node> = attribute.named_children(&mut parts).collect();
    let Some(name) = children.first().filter(|n| n.kind() == "identifier") else {
        return false;
    };

    match text_of(*name, source) {
        // The harness attributes: such an item exists only in a test build.
        "test" | "bench" => children.len() == 1,
        "cfg" => children
            .get(1)
            .filter(|n| n.kind() == "token_tree")
            .is_some_and(|args| predicate_is_test_only(*args, source)),
        _ => false,
    }
}

/// Whether a `cfg` predicate entails `test` — i.e. whether the item it
/// guards can only ever be compiled under test.
///
/// `all(..)` entails test when any operand does; `any(..)` only when every
/// operand does. Everything else, `not(..)` included, resolves to false:
/// the evaluation is deliberately one-sided so an unrecognised predicate
/// under-classifies rather than marking production code as test code.
fn predicate_is_test_only(args: Node, source: &[u8]) -> bool {
    match split_operands(args).as_slice() {
        [only] => operand_is_test_only(only, source),
        _ => false,
    }
}

/// One comma-separated operand of a `cfg` predicate: a bare identifier
/// (`test`), a call (`all(..)`), or a key/value (`feature = "x"`).
struct Operand<'a> {
    name: Option<Node<'a>>,
    args: Option<Node<'a>>,
    tokens: usize,
}

fn operand_is_test_only(operand: &Operand, source: &[u8]) -> bool {
    let Some(name) = operand.name else {
        return false;
    };
    match text_of(name, source) {
        "test" => operand.tokens == 1,
        "all" => operand.args.is_some_and(|args| {
            split_operands(args)
                .iter()
                .any(|o| operand_is_test_only(o, source))
        }),
        "any" => operand.args.is_some_and(|args| {
            let operands = split_operands(args);
            !operands.is_empty() && operands.iter().all(|o| operand_is_test_only(o, source))
        }),
        _ => false,
    }
}

/// Split a `token_tree`'s contents on top-level commas. Tree-sitter keeps a
/// `cfg` predicate's interior as raw tokens, so the grouping happens here
/// rather than through a grammar rule.
fn split_operands<'a>(args: Node<'a>) -> Vec<Operand<'a>> {
    let mut operands = Vec::new();
    let mut current = Operand {
        name: None,
        args: None,
        tokens: 0,
    };

    let mut cursor = args.walk();
    for token in args.children(&mut cursor) {
        match token.kind() {
            "(" | ")" => continue,
            "," => {
                if current.tokens > 0 {
                    operands.push(std::mem::replace(
                        &mut current,
                        Operand {
                            name: None,
                            args: None,
                            tokens: 0,
                        },
                    ));
                }
            }
            "identifier" => {
                if current.name.is_none() {
                    current.name = Some(token);
                }
                current.tokens += 1;
            }
            "token_tree" => {
                if current.args.is_none() {
                    current.args = Some(token);
                }
                current.tokens += 1;
            }
            _ => current.tokens += 1,
        }
    }
    if current.tokens > 0 {
        operands.push(current);
    }
    operands
}

fn text_of<'a>(node: Node, source: &'a [u8]) -> &'a str {
    node.utf8_text(source).unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn regions(src: &str) -> Vec<(u32, u32)> {
        test_regions(src, Language::Rust)
            .into_iter()
            .map(|r| (*r.start(), *r.end()))
            .collect()
    }

    fn covers(src: &str, line: u32) -> bool {
        test_regions(src, Language::Rust)
            .iter()
            .any(|r| r.contains(&line))
    }

    #[test]
    fn cfg_test_module_covers_its_whole_body() {
        let src = "fn prod() {}\n\n#[cfg(test)]\nmod tests {\n    fn helper() {}\n}\n";
        assert_eq!(regions(src), vec![(3, 6)]);
        assert!(!covers(src, 1));
        assert!(covers(src, 5));
    }

    /// Anything the grammar admits between an attribute and its item —
    /// further attributes, comments, doc comments — is passed over on the
    /// way to the item, or the region would end at whatever sat in between
    /// and leave the tests it guards counted as production code.
    #[test]
    fn the_region_reaches_its_item_past_whatever_sits_between() {
        let line = "#[cfg(test)]\n// a note\nmod tests {\n    fn helper() {}\n}\n";
        assert_eq!(regions(line), vec![(1, 5)]);
        assert!(covers(line, 4));

        let doc = "#[cfg(test)]\n/// a note\nmod tests {\n    fn helper() {}\n}\n";
        assert!(covers(doc, 4));

        let block = "#[cfg(test)]\n/* a\n   note */\nmod tests {\n    fn helper() {}\n}\n";
        assert!(covers(block, 5));

        let mixed =
            "#[cfg(test)]\n// a note\n#[allow(dead_code)]\nmod tests {\n    fn helper() {}\n}\n";
        assert!(covers(mixed, 5));
    }

    #[test]
    fn stacked_attributes_resolve_to_the_decorated_item() {
        let src = "#[cfg(test)]\n#[allow(dead_code)]\nmod tests {\n    fn helper() {}\n}\n";
        assert_eq!(regions(src), vec![(1, 5)]);
    }

    #[test]
    fn test_attribute_marks_the_function() {
        let src = "#[test]\nfn a_test() {\n    assert!(true);\n}\n";
        assert!(covers(src, 3));
    }

    #[test]
    fn all_entails_test_but_any_and_not_do_not() {
        assert!(covers("#[cfg(all(test, unix))]\nfn f() {}\n", 2));
        assert!(covers(
            "#[cfg(all(unix, all(test, feature = \"x\")))]\nfn f() {}\n",
            2
        ));
        assert!(!covers(
            "#[cfg(any(test, feature = \"x\"))]\nfn f() {}\n",
            2
        ));
        assert!(!covers("#[cfg(not(test))]\nfn f() {}\n", 2));
        assert!(covers("#[cfg(any(test, all(test, unix)))]\nfn f() {}\n", 2));
    }

    #[test]
    fn unrelated_attributes_leave_production_code_alone() {
        assert!(!covers("#[derive(Debug)]\nstruct S;\n", 2));
        assert!(!covers("#[cfg(feature = \"test-utils\")]\nfn f() {}\n", 2));
        assert!(!covers("#[cfg_attr(test, derive(Debug))]\nstruct S;\n", 2));
        assert!(!covers("fn test() {}\n", 1));
    }

    #[test]
    fn inner_attribute_covers_the_enclosing_item() {
        let src = "#![cfg(test)]\n\nfn helper() {}\n";
        assert!(covers(src, 3));
    }

    #[test]
    fn nested_items_inside_a_test_module_are_covered_once_each() {
        let src = "#[cfg(test)]\nmod tests {\n    #[test]\n    fn a() {}\n}\n";
        assert!(covers(src, 4));
    }

    #[test]
    fn languages_without_a_rule_are_never_parsed() {
        let src = "def test_thing():\n    assert True\n";
        assert!(test_regions(src, Language::Python).is_empty());
        assert!(test_regions(src, Language::TypeScript).is_empty());
        assert!(!has_test_regions(Language::Python));
        assert!(has_test_regions(Language::Rust));
    }

    /// Tree-sitter always yields a tree, ERROR nodes included, so malformed
    /// input must degrade to "no claim" rather than panicking or inventing a
    /// region that swallows the rest of the file.
    #[test]
    fn malformed_source_makes_no_claim() {
        assert!(regions("fn (((").is_empty());
        assert!(regions("").is_empty());
        assert!(!covers("#[derive(", 1));
    }
}
