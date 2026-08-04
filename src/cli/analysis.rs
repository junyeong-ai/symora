//! Bundled LSP analysis at a precise location.
//!
//! The single home for the `find_symbols + resolve target + find_references +
//! classify` sequence that `refs`, `impact`, `context`, and `diff_impact`
//! share.
//!
//! It also fixes what "the references of a symbol" means. The language
//! server is asked for them with the declaration included, so the
//! declaration can be recognised and dropped here: a symbol's own
//! declaration is not a usage of it, and results outside the project are
//! not the project's business. Every surface projects from the set this
//! type holds, so the count `refs`, `impact`, and `context` publish under
//! the same name is the same number.

use std::path::Path;

use crate::cli::ParsedLocation;
use crate::cli::utils::{
    AnchorResolution, RefsClassification, SymbolResolution, ambiguity_hint,
    column_addressed_symbol, line_addressed_symbol,
};
use crate::error::LspError;
use crate::models::lsp::{FindSymbolsOptions, IndexingDegradation};
use crate::models::symbol::{Language, Location, Symbol};
use crate::services::lsp::LspService;
use crate::services::test_scope::TestScope;

pub struct LocationAnalysis {
    pub(crate) anchor: ParsedLocation,
    pub(crate) language: Language,
    pub(crate) target: Option<Symbol>,
    pub(crate) references: Vec<Location>,
    /// The indexing state the reference query ran under, captured at
    /// computation time by the service layer — the `indexing` output
    /// marker derives from this, never from a racy after-the-fact read.
    pub(crate) indexing: Option<IndexingDegradation>,
    /// Disclosure for a line-only anchor that hit a multi-declaration
    /// line: the first declaration was analyzed, and this hint names the
    /// alternatives (picking silently would violate invariant 4; erroring
    /// on the ambiguity instead of disclosing it helps nobody).
    pub(crate) ambiguity: Option<String>,
    /// How the anchor resolved: `Resolved`, `NotASymbol` (read OK, no symbol at
    /// the position), or `Unavailable` (the symbol read failed). The single
    /// source of the unresolved-anchor disclosure — surfaced via `as_status()`
    /// so refs/impact/context emit the same `anchor_status` marker the other
    /// surfaces do, never a bare two-state bool.
    pub(crate) anchor_resolution: AnchorResolution,
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

    pub fn indexing(&self) -> Option<IndexingDegradation> {
        self.indexing
    }

    pub fn ambiguity_hint(&self) -> Option<&str> {
        self.ambiguity.as_deref()
    }

    /// How the anchor resolved (Resolved / NotASymbol / Unavailable). Surfaces
    /// render its `as_status()` as the `anchor_status` disclosure marker.
    pub fn anchor_resolution(&self) -> AnchorResolution {
        self.anchor_resolution
    }
}

impl LocationAnalysis {
    /// Resolve the target symbol and then fetch references from its
    /// anchor.
    ///
    /// Soft-fails on `find_symbols` (target stays `None`); hard-fails on
    /// `find_references` because every caller needs the refs list.
    pub async fn at(
        lsp: &dyn LspService,
        anchor: ParsedLocation,
        root: &Path,
    ) -> Result<Self, LspError> {
        let language = Language::from_path(&anchor.file);
        // Resolve the symbol first so the anchor can snap to its name
        // position; references and blast radius are then taken from the same
        // place, which keeps a line-only or declaration-start input from
        // silently under-counting. Serialized on purpose — the snap must
        // precede the reference lookup.
        // Keep the read outcome: a failed read (Unavailable) is not the same as
        // a position that is verifiably not a symbol (NotASymbol) — surfaces
        // disclose the difference rather than collapsing both into "unresolved".
        let symbols_result = lsp
            .find_symbols(
                &anchor.file,
                FindSymbolsOptions::default().with_body().with_depth(10),
            )
            .await;
        let symbols = symbols_result.ok();
        let (target, ambiguity) = match symbols.as_ref() {
            Some(symbols) => resolve_navigation_target(symbols, &anchor),
            None => (None, None),
        };
        let anchor_resolution = if symbols.is_none() {
            AnchorResolution::Unavailable
        } else if target.is_some() {
            AnchorResolution::Resolved
        } else {
            AnchorResolution::NotASymbol
        };
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
        let usages = usages_of(references.data, root, target.is_some().then_some(&anchor));
        Ok(Self {
            anchor,
            language,
            target,
            references: usages,
            indexing: references.indexing,
            ambiguity,
            anchor_resolution,
        })
    }

    /// Analysis for a symbol whose location is already known. Used when
    /// scanning many symbols (e.g., diff_impact) — skips the heavier
    /// `find_symbols` roundtrip.
    pub async fn for_symbol(
        lsp: &dyn LspService,
        file: &Path,
        symbol: Symbol,
        root: &Path,
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
        let usages = usages_of(references.data, root, Some(&anchor));
        Ok(Self {
            anchor,
            language,
            target: Some(symbol),
            references: usages,
            indexing: references.indexing,
            ambiguity: None,
            anchor_resolution: AnchorResolution::Resolved,
        })
    }

    pub fn classify<'a>(&'a self, test_scope: &TestScope) -> RefsClassification<'a> {
        RefsClassification::of(&self.references, test_scope)
    }

    pub fn is_exported(&self) -> Option<bool> {
        let body = self.target.as_ref().and_then(|s| s.body.as_deref())?;
        Some(detect_exported(body, self.language))
    }
}

/// Reduce a raw `find_references` result to the symbol's usages: inside the
/// project, and never the declaration the anchor snapped to.
///
/// `declaration` is `None` when the anchor resolved to no symbol — there is
/// then no declaration to recognise, and dropping the anchor position anyway
/// would remove a genuine usage.
fn usages_of(
    references: Vec<Location>,
    root: &Path,
    declaration: Option<&ParsedLocation>,
) -> Vec<Location> {
    references
        .into_iter()
        .filter(|r| r.file.starts_with(root))
        .filter(|r| {
            !declaration
                .is_some_and(|d| r.file == d.file && r.line == d.line && r.column == d.column)
        })
        .collect()
}

/// Resolve a navigation anchor through the same line/column addressing
/// rules the edit surface uses (`cli::utils::symbol_nav`): a column
/// addresses the position precisely; an omitted column addresses the
/// symbol DECLARED on the line, with body lines falling back to the
/// enclosing symbol. The same user intent — "the symbol on this line" —
/// must resolve identically whether it is being read or rewritten.
///
/// Where the surfaces differ is ambiguity: a multi-declaration line makes
/// an edit refuse (a guessed write is destructive), while navigation
/// analyzes the line's FIRST declaration and returns a disclosure hint
/// naming the alternatives — the resolved `target` is echoed in the
/// output, so the choice is visible, never silent.
fn resolve_navigation_target(
    symbols: &[Symbol],
    anchor: &ParsedLocation,
) -> (Option<Symbol>, Option<String>) {
    let resolution = if anchor.column_explicit {
        column_addressed_symbol(symbols, anchor.line, anchor.column)
    } else {
        line_addressed_symbol(symbols, anchor.line)
    };
    match resolution {
        SymbolResolution::Match(symbol) => (Some(symbol.clone()), None),
        SymbolResolution::NotFound => (None, None),
        SymbolResolution::Ambiguous(declared) => {
            let hint = ambiguity_hint(anchor.line, &declared);
            (Some(declared[0].clone()), Some(hint))
        }
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
