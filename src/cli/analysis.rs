//! Bundled LSP analysis at a precise location.
//!
//! Replaces the inline `find_symbols + resolve target + find_references +
//! classify` sequence that `impact`, `context`, and `diff_impact` all
//! repeated. Exposes one `classify()` helper and an `is_exported()`
//! predicate, so each command focuses on shaping its own response shape.

use std::path::Path;

use crate::cli::ParsedLocation;
use crate::cli::utils::{RefsClassification, TestMatcher, classify_refs, find_symbol_at_position};
use crate::error::LspError;
use crate::models::lsp::FindSymbolsOptions;
use crate::models::symbol::{Language, Location, Symbol};
use crate::services::lsp::LspService;

pub struct LocationAnalysis {
    pub(crate) anchor: ParsedLocation,
    pub(crate) language: Language,
    pub(crate) target: Option<Symbol>,
    pub(crate) references: Vec<Location>,
}

impl LocationAnalysis {
    pub fn anchor(&self) -> &ParsedLocation {
        &self.anchor
    }

    pub fn language(&self) -> Language {
        self.language
    }

    pub fn target(&self) -> Option<&Symbol> {
        self.target.as_ref()
    }

    pub fn references(&self) -> &[Location] {
        &self.references
    }
}

impl LocationAnalysis {
    /// Resolve the target symbol and fetch references in parallel.
    ///
    /// Soft-fails on `find_symbols` (target stays `None`); hard-fails on
    /// `find_references` because every caller needs the refs list.
    pub async fn at(lsp: &dyn LspService, anchor: ParsedLocation) -> Result<Self, LspError> {
        let language = Language::from_path(&anchor.file);
        // Resolve the symbol first so the anchor can snap to its name
        // position; references and blast radius are then taken from the same
        // place, which keeps a line-only or declaration-start input from
        // silently under-counting. Serialized on purpose — the snap must
        // precede the reference lookup.
        let symbols = lsp
            .find_symbols(
                &anchor.file,
                FindSymbolsOptions::default().with_body().with_depth(10),
            )
            .await
            .ok();
        let target = symbols.as_ref().and_then(|symbols| {
            find_symbol_at_position(symbols, anchor.line, Some(anchor.column))
                .or_else(|| find_symbol_at_position(symbols, anchor.line, None))
                .cloned()
        });
        let anchor = match &target {
            Some(symbol) => ParsedLocation {
                file: anchor.file,
                line: symbol.location.line,
                column: symbol.location.column,
                column_explicit: true,
            },
            None => anchor,
        };
        let references = lsp
            .find_references(&anchor.file, anchor.line, anchor.column)
            .await?;
        Ok(Self {
            anchor,
            language,
            target,
            references,
        })
    }

    /// Analysis for a symbol whose location is already known. Used when
    /// scanning many symbols (e.g., diff_impact) — skips the heavier
    /// `find_symbols` roundtrip.
    pub async fn for_symbol(
        lsp: &dyn LspService,
        file: &Path,
        symbol: Symbol,
    ) -> Result<Self, LspError> {
        let anchor = ParsedLocation {
            file: file.to_path_buf(),
            line: symbol.location.line,
            column: symbol.location.column,
            column_explicit: true,
        };
        let language = Language::from_path(file);
        let references = lsp
            .find_references(&anchor.file, anchor.line, anchor.column)
            .await?;
        Ok(Self {
            anchor,
            language,
            target: Some(symbol),
            references,
        })
    }

    pub fn classify<'a>(
        &'a self,
        root: &Path,
        test_matcher: &TestMatcher,
        skip_self: bool,
    ) -> RefsClassification<'a> {
        let (self_file, self_line) = if skip_self {
            (Some(self.anchor.file.as_path()), Some(self.anchor.line))
        } else {
            (None, None)
        };
        classify_refs(&self.references, root, self_file, self_line, test_matcher)
    }

    pub fn is_exported(&self) -> Option<bool> {
        let body = self.target.as_ref().and_then(|s| s.body.as_deref())?;
        Some(detect_exported(body, self.language))
    }
}

/// Decide whether a body's first declaration line exports the symbol.
///
/// Exported means:
/// - Rust: `pub ` without restricted visibility (`pub(crate)`, `pub(super)`, `pub(in ...)`)
/// - Go: declared name starts with an uppercase letter
/// - Java/Kotlin/C#: `public ` modifier (NOT `internal` for C#)
/// - TypeScript/JavaScript: `export ` keyword
/// - Python: name does not start with underscore
pub fn detect_exported(body: &str, lang: Language) -> bool {
    let first_line = body.lines().next().unwrap_or("");
    match lang {
        Language::Rust => detect_rust(first_line),
        Language::Java | Language::Kotlin | Language::CSharp => first_line.contains("public "),
        Language::TypeScript | Language::JavaScript => first_line.contains("export "),
        Language::Go => detect_go(first_line),
        Language::Python => detect_python(first_line),
        _ => false,
    }
}

fn detect_rust(line: &str) -> bool {
    if !line.contains("pub ") {
        return false;
    }
    !line.contains("pub(crate)")
        && !line.contains("pub(super)")
        && !line.contains("pub(self)")
        && !line.contains("pub(in ")
}

fn detect_go(line: &str) -> bool {
    let candidate = if let Some(pos) = line.find("func ") {
        let after = &line[pos + 5..];
        if after.starts_with('(') {
            after.find(") ").map(|i| &after[i + 2..]).unwrap_or(after)
        } else {
            after
        }
    } else if let Some(pos) = line.find("type ") {
        &line[pos + 5..]
    } else {
        line
    };
    candidate
        .chars()
        .find(|c| c.is_alphabetic())
        .map(|c| c.is_uppercase())
        .unwrap_or(false)
}

fn detect_python(line: &str) -> bool {
    let name_start = line
        .find("def ")
        .map(|i| i + 4)
        .or_else(|| line.find("class ").map(|i| i + 6));
    if let Some(start) = name_start {
        !line[start..].trim_start().starts_with('_')
    } else {
        !line.trim_start().starts_with('_')
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn detect_rust_exported_and_restricted() {
        assert!(detect_exported("pub fn process()", Language::Rust));
        assert!(detect_exported("pub struct Foo {}", Language::Rust));
        assert!(!detect_exported("pub(crate) fn process()", Language::Rust));
        assert!(!detect_exported("pub(super) fn process()", Language::Rust));
        assert!(!detect_exported("pub(self) fn process()", Language::Rust));
        assert!(!detect_exported(
            "pub(in crate::foo) fn process()",
            Language::Rust
        ));
        assert!(!detect_exported("fn process()", Language::Rust));
    }

    #[test]
    fn detect_go_exported_uppercase_name() {
        assert!(detect_exported("func Process()", Language::Go));
        assert!(detect_exported("type Handler struct", Language::Go));
        assert!(!detect_exported("func process()", Language::Go));
        assert!(!detect_exported("type handler struct", Language::Go));
    }

    #[test]
    fn detect_go_method_receiver() {
        assert!(detect_exported("func (h *Handler) Process()", Language::Go));
        assert!(!detect_exported(
            "func (h *Handler) process()",
            Language::Go
        ));
        assert!(detect_exported("func (s Service) Export()", Language::Go));
    }

    #[test]
    fn detect_typescript_export_keyword() {
        assert!(detect_exported(
            "export function process()",
            Language::TypeScript
        ));
        assert!(detect_exported(
            "export const foo = 1",
            Language::TypeScript
        ));
        assert!(!detect_exported("function process()", Language::TypeScript));
        assert!(!detect_exported("const foo = 1", Language::TypeScript));
    }

    #[test]
    fn detect_python_underscore_is_private() {
        assert!(detect_exported("def process():", Language::Python));
        assert!(detect_exported("class Handler:", Language::Python));
        assert!(!detect_exported("def _process():", Language::Python));
        assert!(!detect_exported("class _Handler:", Language::Python));
    }

    #[test]
    fn detect_java_public_modifier() {
        assert!(detect_exported("public void process()", Language::Java));
        assert!(detect_exported("public class Handler", Language::Java));
        assert!(!detect_exported("private void process()", Language::Java));
        assert!(!detect_exported("void process()", Language::Java));
    }

    #[test]
    fn detect_csharp_only_public_is_exported() {
        assert!(detect_exported("public void Process()", Language::CSharp));
        assert!(detect_exported("public class Handler", Language::CSharp));
        // internal is NOT exported (assembly-private)
        assert!(!detect_exported(
            "internal void Process()",
            Language::CSharp
        ));
        assert!(!detect_exported("private void Process()", Language::CSharp));
    }
}
