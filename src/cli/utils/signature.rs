//! Heuristic signature extraction — pulls the declaration line out of a
//! symbol body so we can show "fn process(...)" without the body weight.

use crate::models::symbol::Symbol;

/// Extract a function/method/type signature from the symbol's body, read
/// from the line that names it. Falls back to the first non-empty line when
/// no language keyword matches.
pub fn extract_signature(symbol: &Symbol) -> Option<String> {
    let body = symbol.body_from_name()?;

    for line in body.lines() {
        let trimmed = line.trim();
        if trimmed.is_empty() || trimmed.starts_with("//") || trimmed.starts_with('#') {
            continue;
        }

        if trimmed.contains("fn ")
            || trimmed.contains("func ")
            || trimmed.contains("def ")
            || trimmed.contains("function ")
            || trimmed.contains("async ")
            || trimmed.contains("pub ")
            || trimmed.contains("class ")
            || trimmed.contains("struct ")
            || trimmed.contains("enum ")
            || trimmed.contains("interface ")
            || trimmed.contains("trait ")
            || trimmed.contains("impl ")
            || trimmed.contains("type ")
            || trimmed.contains("const ")
        {
            let sig = if let Some(brace_pos) = trimmed.find('{') {
                trimmed[..brace_pos].trim()
            } else if let Some(arrow_pos) = trimmed.find("=>") {
                trimmed[..arrow_pos].trim()
            } else if let Some(stripped) = trimmed.strip_suffix(':') {
                stripped
            } else {
                trimmed
            };

            return Some(sig.to_string());
        }
    }

    body.lines()
        .find(|l| !l.trim().is_empty())
        .map(|l| l.trim().to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::models::symbol::{Location, SymbolKind};
    use std::path::PathBuf;

    #[test]
    fn a_signature_is_read_from_the_line_that_names_the_symbol() {
        let declared = |body: &str, name_line: u32| {
            Symbol::new("f".to_string(), SymbolKind::Method, {
                let end = body.lines().count() as u32;
                Location::full(PathBuf::from("a"), name_line, 5, 1, 1, end, 2)
            })
            .with_body(body)
        };

        let java = declared(
            "@Override\npublic String toString() {\n    return \"s\";\n}",
            2,
        );
        assert_eq!(
            extract_signature(&java).as_deref(),
            Some("public String toString() {")
        );
        let rust = declared(
            "/// Size.\n#[inline]\npub fn len(&self) -> usize {\n    0\n}",
            3,
        );
        assert_eq!(
            extract_signature(&rust).as_deref(),
            Some("pub fn len(&self) -> usize")
        );
    }
}
