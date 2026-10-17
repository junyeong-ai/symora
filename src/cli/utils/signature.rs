use crate::models::symbol::{Language, Symbol};
use crate::services::store::SymbolExtractor;

pub fn extract_signature(symbol: &Symbol) -> Option<String> {
    let content = symbol.body.as_deref()?;
    let line = symbol
        .location
        .line
        .checked_sub(symbol.location.effective_start().0)?
        + 1;
    SymbolExtractor::shared()
        .extract_signatures(
            &symbol.location.file,
            content,
            Language::from_path(&symbol.location.file),
        )
        .into_iter()
        .find(|(declared, _)| declared.location.line == line && declared.name == symbol.name)
        .map(|(_, signature)| signature)
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
                Location::full(PathBuf::from("a.rs"), name_line, 5, 1, 1, end, 2)
            })
            .with_body(body)
        };

        let mut java = declared(
            "@Override\npublic String toString() {\n    return \"s\";\n}",
            2,
        );
        java.name = "toString".to_string();
        java.location.file = PathBuf::from("a.java");
        assert_eq!(
            extract_signature(&java).as_deref(),
            Some("@Override\npublic String toString()")
        );
        let mut rust = declared(
            "/// Size.\n#[inline]\npub fn len(&self) -> usize {\n    0\n}",
            3,
        );
        rust.name = "len".to_string();
        assert_eq!(
            extract_signature(&rust).as_deref(),
            Some("pub fn len(&self) -> usize")
        );
    }
}
