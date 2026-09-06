//! Bundled LSP analysis at a precise location.
//!
//! The single home for anchor resolution — what a `file:line[:column]`
//! addresses, for every symbol-level surface — and for the `resolve anchor +
//! find_references + classify` sequence that `refs`, `impact`, `context`, and
//! `diff_impact` share.
//!
//! It also fixes what "the references of a symbol" means. The language
//! server is asked for them with the declaration included, so the
//! declaration can be recognised and dropped here: a symbol's own
//! declaration is not a usage of it, and results outside the project are
//! not the project's business. Every surface projects from the set this
//! type holds, so the count `refs`, `impact`, and `context` publish under
//! the same name is the same number.

use std::path::{Path, PathBuf};

use crate::cli::ParsedLocation;
use crate::cli::utils::{
    AnchorResolution, RefsClassification, SymbolResolution, ambiguity_hint,
    column_addressed_symbol, find_named_at_position, line_addressed_symbol,
};
use crate::error::LspError;
use crate::models::lsp::{FindSymbolsOptions, IndexingDegradation};
use crate::models::symbol::{Language, Location, Symbol, SymbolKind};
use crate::services::lsp::LspService;
use crate::services::test_scope::TestScope;

/// Where a symbol-level query anchors: the declaration the input position
/// addresses, resolved once and the same way for every surface — references,
/// callers, callees, implementations, type hierarchy, impact, context.
///
/// An explicit column is a precise address. On a symbol's name it is that
/// symbol; anywhere else it is the token there — a call, a type, a receiver,
/// an attribute — resolved through the language server's definition of it,
/// exactly as `def`, `hover`, and `rename` read the same position, and the
/// analysis anchors at that declaration, wherever it lives, so a usage and
/// its declaration are the same question with the same answer. A position
/// on no token at all — the keyword or whitespace before a name — addresses
/// the declaration on that line. An omitted column addresses the symbol
/// declared on the line, a body line falling back to the enclosing symbol.
pub struct Anchor {
    /// The position as it was given.
    pub input: ParsedLocation,
    pub file: PathBuf,
    pub line: u32,
    pub column: u32,
    /// The symbol declared at the anchor, when the symbol tree lists it.
    pub symbol: Option<Symbol>,
    /// Whether the input was a usage that resolved to the anchor through its
    /// definition — as opposed to being on the declaration itself.
    pub via_definition: bool,
    /// The multi-declaration disclosure for a line-only input that hit
    /// several declarations: the first was chosen, and this names the rest.
    pub hint: Option<String>,
    pub resolution: AnchorResolution,
}

impl Anchor {
    fn raw(input: &ParsedLocation, resolution: AnchorResolution) -> Self {
        Self {
            input: input.clone(),
            file: input.file.clone(),
            line: input.line,
            column: input.column,
            symbol: None,
            via_definition: false,
            hint: None,
            resolution,
        }
    }

    /// The anchor of an input whose resolution could not be completed: the
    /// position as given, disclosed as unavailable, for the surfaces that
    /// query the language server at it anyway.
    pub fn unavailable(input: &ParsedLocation) -> Self {
        Self::raw(input, AnchorResolution::Unavailable)
    }

    fn declared(input: &ParsedLocation, file: &Path, symbol: Symbol, hint: Option<String>) -> Self {
        Self {
            input: input.clone(),
            file: file.to_path_buf(),
            line: symbol.location.line,
            column: symbol.location.column,
            symbol: Some(symbol),
            via_definition: false,
            hint,
            resolution: AnchorResolution::Resolved,
        }
    }

    /// A usage whose definition the symbol tree lists: the same anchor as
    /// addressing that declaration directly, reached through the token.
    fn defined(input: &ParsedLocation, file: &Path, symbol: Symbol) -> Self {
        Self {
            via_definition: true,
            ..Self::declared(input, file, symbol, None)
        }
    }

    /// A position that denotes a declaration the symbol tree does not list —
    /// a local, a parameter, a generated item. The analysis anchors at that
    /// declaration so its reference set is exact, and the target is disclosed
    /// as a binding rather than dressed up as a symbol.
    fn binding(input: &ParsedLocation, definition: &Location) -> Self {
        Self {
            input: input.clone(),
            file: definition.file.clone(),
            line: definition.line,
            column: definition.column,
            symbol: None,
            via_definition: true,
            hint: None,
            resolution: AnchorResolution::Binding,
        }
    }

    /// Whether the input resolved to a listed symbol.
    pub fn is_resolved(&self) -> bool {
        self.resolution.is_resolved()
    }

    /// Whether the anchor position is a declaration — a reference reported
    /// there is the declaration itself, not a usage.
    pub fn is_declaration(&self) -> bool {
        self.resolution.is_declaration()
    }

    /// The anchor as a location, for the surfaces that carry one forward.
    pub fn location(&self) -> ParsedLocation {
        ParsedLocation {
            file: self.file.clone(),
            line: self.line,
            column: self.column,
            column_explicit: true,
        }
    }
}

/// Resolve the anchor for a symbol-level query — see [`Anchor`]. `options`
/// shape the symbol read (a surface that renders the target's body asks for
/// it here); a same-file definition is resolved from that read, a cross-file
/// definition from its own file's. A read the resolution needs failing is
/// returned as the error it was: a surface that goes on to query the server
/// at the raw position discloses the anchor as [`Anchor::unavailable`], one
/// that needs the resolution itself reports the failure.
pub async fn resolve_anchor(
    lsp: &dyn LspService,
    input: &ParsedLocation,
    options: FindSymbolsOptions,
) -> Result<Anchor, LspError> {
    let symbols = lsp.find_symbols(&input.file, options.clone()).await?;
    if !input.column_explicit {
        return Ok(match line_addressed_symbol(&symbols, input.line) {
            SymbolResolution::Match(symbol) => {
                Anchor::declared(input, &input.file, symbol.clone(), None)
            }
            SymbolResolution::Ambiguous(declared) => {
                let hint = ambiguity_hint(input.line, &declared);
                Anchor::declared(input, &input.file, declared[0].clone(), Some(hint))
            }
            SymbolResolution::NotFound => Anchor::raw(input, AnchorResolution::NotASymbol),
        });
    }

    if let Some(symbol) = find_named_at_position(&symbols, input.line, input.column) {
        return Ok(Anchor::declared(input, &input.file, symbol.clone(), None));
    }
    // A definition that names no other position — none at all, or the
    // server answering that the queried token is its own definition —
    // leaves the header reading: the declaration whose header the position
    // sits on. A self-definition off any header keeps one more fact a
    // missing definition lacks: the position DOES declare something — a
    // binding queried at its own declaration (a parameter, a local) — and
    // is disclosed as that binding, never as "not a symbol".
    let definition = lsp
        .goto_definition(&input.file, input.line, input.column)
        .await?
        .data;
    let self_declaration = definition
        .as_ref()
        .filter(|definition| definition.is_self)
        .map(|definition| definition.location.clone());
    let Some(definition) = definition.filter(|definition| !definition.is_self) else {
        return Ok(
            match column_addressed_symbol(&symbols, input.line, input.column) {
                SymbolResolution::Match(symbol) => {
                    Anchor::declared(input, &input.file, symbol.clone(), None)
                }
                _ => match self_declaration {
                    Some(location) => Anchor::binding(input, &location),
                    None => Anchor::raw(input, AnchorResolution::NotASymbol),
                },
            },
        );
    };
    let definition = definition.location;
    // A definition names a declaration by its name position — that is what
    // every server returns for a symbol — so it is matched by the name span
    // ONLY: a definition elsewhere that no name occupies is a binding the
    // tree does not list (a module lands on the file's first position, a
    // generic parameter or a Go receiver sits inside another declaration's
    // header), and reading it by the header it happens to fall on would
    // answer for the wrong symbol.
    let symbol = if definition.file == input.file {
        find_named_at_position(&symbols, definition.line, definition.column).cloned()
    } else {
        find_named_at_position(
            &lsp.find_symbols(&definition.file, options).await?,
            definition.line,
            definition.column,
        )
        .cloned()
    };
    let Some(symbol) = symbol else {
        return Ok(Anchor::binding(input, &definition));
    };
    // The input was on the declaration itself — not a usage of it — when its
    // own position reads as the same declaration's header.
    let on_own_declaration = definition.file == input.file
        && matches!(
            column_addressed_symbol(&symbols, input.line, input.column),
            SymbolResolution::Match(header) if header.location == symbol.location
        );
    Ok(if on_own_declaration {
        Anchor::declared(input, &definition.file, symbol, None)
    } else {
        Anchor::defined(input, &definition.file, symbol)
    })
}

/// The type that declares the anchored symbol, and how often that type is
/// referenced.
///
/// A call reaches a member through the type — construction, dispatch, a
/// protocol satisfied structurally — and none of those name the member. So an
/// empty reference set is reachability for a free item and a weaker claim for
/// a member, and the two must not read alike. Read only when the set IS empty,
/// which is the only time a caller reads it as absence.
pub struct DeclaringType {
    pub name: String,
    pub references: usize,
}

pub struct LocationAnalysis {
    /// The declaration the input addressed — target, position, and how it
    /// resolved. Its `resolution.as_status()` is the `anchor_status` marker
    /// refs/impact/context emit, the same vocabulary the other surfaces use.
    pub(crate) anchor: Anchor,
    pub(crate) language: Language,
    pub(crate) references: Vec<Location>,
    /// The indexing state the reference query ran under, captured at
    /// computation time by the service layer — the `indexing` output
    /// marker derives from this, never from a racy after-the-fact read.
    pub(crate) indexing: Option<IndexingDegradation>,
    pub(crate) declaring_type: Option<DeclaringType>,
}

impl LocationAnalysis {
    pub fn anchor(&self) -> &Anchor {
        &self.anchor
    }

    pub fn language(&self) -> Language {
        self.language
    }

    pub fn target(&self) -> Option<&Symbol> {
        self.anchor.symbol.as_ref()
    }

    pub fn references(&self) -> &[Location] {
        &self.references
    }

    pub fn indexing(&self) -> Option<IndexingDegradation> {
        self.indexing
    }

    /// What an empty reference set leaves unsaid, when the anchor is a member.
    ///
    /// One sentence for every surface, because the reading it corrects is the
    /// same one: a zero taken for "nothing reaches this".
    pub fn member_reach_hint(&self) -> Option<String> {
        let declaring = self.declaring_type.as_ref()?;
        let name = self.anchor.symbol.as_ref().map(|s| s.name.as_str())?;
        Some(match declaring.references {
            0 => format!(
                "Nothing names `{name}`, and nothing names `{}`, which declares it",
                declaring.name
            ),
            n => format!(
                "Nothing names `{name}`, but `{}`, which declares it, is used {n} time(s) — a \
                 call through the type does not name the member, so this is not evidence that \
                 `{name}` is unreachable",
                declaring.name
            ),
        })
    }

    /// How the anchor resolved — see [`AnchorResolution`]. Surfaces render
    /// its `as_status()` as the `anchor_status` disclosure marker.
    pub fn anchor_resolution(&self) -> AnchorResolution {
        self.anchor.resolution
    }

    /// Whether the language server's reference set contradicts the input: the
    /// input position was a usage — it resolved to this declaration through
    /// its definition, from somewhere else — yet no reference covers it. The
    /// set is then a lower bound; some servers omit the usages of certain
    /// bindings (rust-analyzer does for the parameters of async functions),
    /// and reporting their zero as the count would present a known-incomplete
    /// answer as complete.
    pub fn omits_input(&self, root: &Path) -> bool {
        let input = &self.anchor.input;
        let elsewhere = input.file != self.anchor.file
            || input.line != self.anchor.line
            || input.column != self.anchor.column;
        // The reference set is project-local, so only an input inside the
        // project can be expected in it — an outside usage proves nothing.
        input.file.starts_with(root)
            && self.anchor.via_definition
            && elsewhere
            && !self.references.iter().any(|r| {
                r.file == input.file
                    && r.line == input.line
                    && r.column <= input.column
                    && r.end_column.is_none_or(|end| input.column <= end)
            })
    }
}

/// The type a member is declared in, found by tree parentage rather than by
/// its name — an outer function declares a nested one, and a call there has to
/// name it, so only a type weakens the member's zero.
fn declaring_type_of(symbols: &[Symbol], anchor: &Anchor) -> Option<Symbol> {
    fn walk(nodes: &[Symbol], parent: Option<&Symbol>, at: (u32, u32)) -> Option<Symbol> {
        for node in nodes {
            if (node.location.line, node.location.column) == at {
                return parent
                    .filter(|p| {
                        matches!(
                            p.kind,
                            SymbolKind::Class
                                | SymbolKind::Struct
                                | SymbolKind::Interface
                                | SymbolKind::Enum
                        )
                    })
                    .cloned();
            }
            if let Some(found) = walk(&node.children, Some(node), at) {
                return Some(found);
            }
        }
        None
    }

    walk(symbols, None, (anchor.line, anchor.column))
}

/// How often the type declaring the anchor is referenced, asked only when the
/// member's own set is empty — the one answer a caller reads as absence.
async fn declaring_type_reach(
    lsp: &dyn LspService,
    anchor: &Anchor,
    root: &Path,
) -> Option<DeclaringType> {
    let symbols = lsp
        .find_symbols(&anchor.file, FindSymbolsOptions::default().with_depth(10))
        .await
        .ok()?;
    let declaring = declaring_type_of(&symbols, anchor)?;
    let references = lsp
        .find_references(
            &anchor.file,
            declaring.location.line,
            declaring.location.column,
        )
        .await
        .ok()?;
    let declaration = ParsedLocation {
        file: anchor.file.clone(),
        line: declaring.location.line,
        column: declaring.location.column,
        column_explicit: true,
    };
    Some(DeclaringType {
        name: declaring.name,
        references: usages_of(references.data, root, Some(&declaration)).len(),
    })
}

impl LocationAnalysis {
    /// Resolve the anchor and then fetch references from it.
    ///
    /// Soft-fails on the symbol read (target stays `None`); hard-fails on
    /// `find_references` because every caller needs the refs list.
    pub async fn at(
        lsp: &dyn LspService,
        input: ParsedLocation,
        root: &Path,
    ) -> Result<Self, LspError> {
        // Resolve the anchor first so references and blast radius are taken
        // from the declaration the input addresses, which keeps a line-only,
        // declaration-start, or usage-site input from silently answering a
        // different question. Serialized on purpose — the resolution must
        // precede the reference lookup.
        let anchor = resolve_anchor(
            lsp,
            &input,
            FindSymbolsOptions::default().with_body().with_depth(10),
        )
        .await
        .unwrap_or_else(|_| Anchor::unavailable(&input));
        let language = Language::from_path(&anchor.file);
        let references = lsp
            .find_references(&anchor.file, anchor.line, anchor.column)
            .await?;
        let declaration = anchor.location();
        let usages = usages_of(
            references.data,
            root,
            anchor.is_declaration().then_some(&declaration),
        );
        let declaring_type = match usages.is_empty() {
            true => declaring_type_reach(lsp, &anchor, root).await,
            false => None,
        };
        Ok(Self {
            anchor,
            language,
            references: usages,
            indexing: references.indexing,
            declaring_type,
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
        let input = ParsedLocation {
            file: file.to_path_buf(),
            line: symbol.location.line,
            column: symbol.location.column,
            column_explicit: true,
        };
        let anchor = Anchor::declared(&input, file, symbol, None);
        let language = Language::from_path(file);
        let references = lsp
            .find_references(&anchor.file, anchor.line, anchor.column)
            .await?;
        let usages = usages_of(references.data, root, Some(&input));
        Ok(Self {
            anchor,
            language,
            references: usages,
            indexing: references.indexing,
            declaring_type: None,
        })
    }

    pub fn classify<'a>(&'a self, test_scope: &TestScope) -> RefsClassification<'a> {
        RefsClassification::of(&self.references, test_scope)
    }

    pub fn is_exported(&self) -> Option<bool> {
        let body = self.target().and_then(|s| s.body.as_deref())?;
        Some(detect_exported(body, self.language))
    }
}

/// Reduce a raw `find_references` result to the symbol's usages: inside the
/// project, never the declaration the anchor resolved to, and in source
/// order.
///
/// A language server returns references in whatever order it found them, so
/// a list capped by `--limit` would otherwise show a different five every
/// run and a different five on either side of the daemon socket. Ordering
/// by position makes which usages survive the cap a property of the code
/// rather than of the answer — the same reason the call-graph walk sorts
/// each frontier before its fan-out cap applies.
///
/// `declaration` is `None` when the anchor resolved to no symbol — there is
/// then no declaration to recognise, and dropping the anchor position anyway
/// would remove a genuine usage.
fn usages_of(
    references: Vec<Location>,
    root: &Path,
    declaration: Option<&ParsedLocation>,
) -> Vec<Location> {
    let mut usages: Vec<Location> = references
        .into_iter()
        .filter(|r| r.file.starts_with(root))
        .filter(|r| {
            !declaration
                .is_some_and(|d| r.file == d.file && r.line == d.line && r.column == d.column)
        })
        .collect();
    usages.sort_by(|a, b| {
        a.file
            .cmp(&b.file)
            .then(a.line.cmp(&b.line))
            .then(a.column.cmp(&b.column))
    });
    usages
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

    use std::collections::HashMap;
    use std::path::PathBuf;

    use async_trait::async_trait;

    use crate::models::lsp::{
        ApplyActionResult, CallHierarchyItem, CodeAction, CodeLens, Definition, FoldingRange,
        HoverInfo, Indexed, InlayHint, PrepareRenameResult, RenameResult, SelectionRange,
        ServerStatus, SignatureHelp, TextEdit, TypeHierarchyItem,
    };
    use crate::models::symbol::SymbolKind;

    /// Serves canned per-file symbol trees and per-position definitions —
    /// the two reads anchor resolution makes. Everything else is unreachable
    /// from `resolve_anchor` and panics loudly if that ever changes.
    struct AnchorStub {
        symbols_by_file: HashMap<PathBuf, Vec<Symbol>>,
        definitions: HashMap<(PathBuf, u32, u32), Definition>,
    }

    fn func(file: &str, name: &str, line: u32, name_col: u32, end_line: u32) -> Symbol {
        Symbol::new(
            name.to_string(),
            SymbolKind::Function,
            Location::full(PathBuf::from(file), line, name_col, line, 1, end_line, 1)
                .with_name_end(line, name_col + name.len() as u32),
        )
    }

    fn at(file: &str, line: u32, column: u32) -> ParsedLocation {
        ParsedLocation {
            file: PathBuf::from(file),
            line,
            column,
            column_explicit: true,
        }
    }

    fn on_line(file: &str, line: u32) -> ParsedLocation {
        ParsedLocation {
            column_explicit: false,
            ..at(file, line, 1)
        }
    }

    #[async_trait]
    impl LspService for AnchorStub {
        async fn find_symbols(
            &self,
            file: &Path,
            _options: FindSymbolsOptions,
        ) -> Result<Vec<Symbol>, LspError> {
            self.symbols_by_file
                .get(file)
                .cloned()
                .ok_or_else(|| LspError::server_error_friendly(-1, "no symbols".to_string()))
        }
        async fn goto_definition(
            &self,
            file: &Path,
            line: u32,
            column: u32,
        ) -> Result<Indexed<Option<Definition>>, LspError> {
            Ok(Indexed::complete(
                self.definitions
                    .get(&(file.to_path_buf(), line, column))
                    .cloned(),
            ))
        }

        async fn workspace_symbols(
            &self,
            _query: &str,
            _language: Language,
        ) -> Result<Indexed<Vec<Symbol>>, LspError> {
            unreachable!()
        }
        async fn find_references(
            &self,
            _file: &Path,
            _line: u32,
            _column: u32,
        ) -> Result<Indexed<Vec<Location>>, LspError> {
            unreachable!()
        }
        async fn goto_type_definition(
            &self,
            _file: &Path,
            _line: u32,
            _column: u32,
        ) -> Result<Indexed<Option<Location>>, LspError> {
            unreachable!()
        }
        async fn find_implementations(
            &self,
            _file: &Path,
            _line: u32,
            _column: u32,
        ) -> Result<Indexed<Vec<Location>>, LspError> {
            unreachable!()
        }
        async fn hover(
            &self,
            _file: &Path,
            _line: u32,
            _column: u32,
        ) -> Result<Indexed<Option<HoverInfo>>, LspError> {
            unreachable!()
        }
        async fn signature_help(
            &self,
            _file: &Path,
            _line: u32,
            _column: u32,
        ) -> Result<Indexed<Option<SignatureHelp>>, LspError> {
            unreachable!()
        }
        async fn diagnostics(
            &self,
            _file: &Path,
        ) -> Result<crate::models::diagnostic::DiagnosticsReport, LspError> {
            unreachable!()
        }
        async fn prepare_rename(
            &self,
            _file: &Path,
            _line: u32,
            _column: u32,
        ) -> Result<Option<PrepareRenameResult>, LspError> {
            unreachable!()
        }
        async fn rename(
            &self,
            _file: &Path,
            _line: u32,
            _column: u32,
            _new_name: &str,
        ) -> Result<Indexed<Option<RenameResult>>, LspError> {
            unreachable!()
        }
        async fn incoming_calls(
            &self,
            _file: &Path,
            _line: u32,
            _column: u32,
        ) -> Result<Indexed<Vec<CallHierarchyItem>>, LspError> {
            unreachable!()
        }
        async fn outgoing_calls(
            &self,
            _file: &Path,
            _line: u32,
            _column: u32,
        ) -> Result<Indexed<Vec<CallHierarchyItem>>, LspError> {
            unreachable!()
        }
        async fn supertypes(
            &self,
            _file: &Path,
            _line: u32,
            _column: u32,
        ) -> Result<Indexed<Vec<TypeHierarchyItem>>, LspError> {
            unreachable!()
        }
        async fn subtypes(
            &self,
            _file: &Path,
            _line: u32,
            _column: u32,
        ) -> Result<Indexed<Vec<TypeHierarchyItem>>, LspError> {
            unreachable!()
        }
        async fn inlay_hints(
            &self,
            _file: &Path,
            _start_line: u32,
            _end_line: u32,
        ) -> Result<Vec<InlayHint>, LspError> {
            unreachable!()
        }
        async fn folding_ranges(&self, _file: &Path) -> Result<Vec<FoldingRange>, LspError> {
            unreachable!()
        }
        async fn selection_ranges(
            &self,
            _file: &Path,
            _positions: Vec<(u32, u32)>,
        ) -> Result<Vec<SelectionRange>, LspError> {
            unreachable!()
        }
        async fn code_lenses(&self, _file: &Path) -> Result<Vec<CodeLens>, LspError> {
            unreachable!()
        }
        async fn code_actions(
            &self,
            _file: &Path,
            _line: u32,
            _column: u32,
        ) -> Result<Vec<CodeAction>, LspError> {
            unreachable!()
        }
        async fn apply_code_action(
            &self,
            _file: &Path,
            _action: &CodeAction,
        ) -> Result<ApplyActionResult, LspError> {
            unreachable!()
        }
        async fn format(&self, _file: &Path) -> Result<Vec<TextEdit>, LspError> {
            unreachable!()
        }
        async fn is_available(&self, _language: Language) -> bool {
            unreachable!()
        }
        async fn server_status(&self, _language: Language) -> ServerStatus {
            unreachable!()
        }
    }

    /// `routes.rs` calls `process_order` (declared in `svc.rs`) from inside
    /// `handle`, whose `#[instrument]` attribute occupies line 22 above the
    /// name; the call-site token at 31:28 defines to `svc.rs:5:8`, the local
    /// at 33:9 defines to its own `let` at 25:9, which the tree does not list,
    /// the attribute at 22:3 defines to a macro in `ext.rs`, whose symbols
    /// cannot be read. Line 50 declares `Foo make()`: the return type `Foo`
    /// occupies columns 1..3 of the name's line and defines to `types.rs`.
    fn project() -> AnchorStub {
        let mut symbols_by_file = HashMap::new();
        symbols_by_file.insert(
            PathBuf::from("routes.rs"),
            vec![
                Symbol::new(
                    "handle".to_string(),
                    SymbolKind::Function,
                    Location::full(PathBuf::from("routes.rs"), 23, 8, 22, 1, 40, 1)
                        .with_name_end(23, 14),
                ),
                Symbol::new(
                    "make".to_string(),
                    SymbolKind::Function,
                    Location::full(PathBuf::from("routes.rs"), 50, 5, 50, 1, 55, 1)
                        .with_name_end(50, 9),
                ),
            ],
        );
        symbols_by_file.insert(
            PathBuf::from("svc.rs"),
            vec![func("svc.rs", "process_order", 5, 8, 12)],
        );
        symbols_by_file.insert(
            PathBuf::from("types.rs"),
            vec![func("types.rs", "Foo", 3, 12, 9)],
        );
        // `models/mod.rs` opens with `pub mod config;` — a declaration whose
        // range starts at (1,1), the very position a module definition names.
        symbols_by_file.insert(
            PathBuf::from("models/mod.rs"),
            vec![Symbol::new(
                "config".to_string(),
                SymbolKind::Module,
                Location::full(PathBuf::from("models/mod.rs"), 1, 9, 1, 1, 1, 16)
                    .with_name_end(1, 15),
            )],
        );
        let mut definitions = HashMap::new();
        definitions.insert(
            (PathBuf::from("routes.rs"), 31, 28),
            Definition {
                location: Location::point(PathBuf::from("svc.rs"), 5, 8),
                is_self: false,
            },
        );
        definitions.insert(
            (PathBuf::from("routes.rs"), 33, 9),
            Definition {
                location: Location::point(PathBuf::from("routes.rs"), 25, 9),
                is_self: false,
            },
        );
        definitions.insert(
            (PathBuf::from("routes.rs"), 22, 3),
            Definition {
                location: Location::point(PathBuf::from("ext.rs"), 1, 12),
                is_self: false,
            },
        );
        definitions.insert(
            (PathBuf::from("routes.rs"), 50, 1),
            Definition {
                location: Location::point(PathBuf::from("types.rs"), 3, 12),
                is_self: false,
            },
        );
        definitions.insert(
            (PathBuf::from("routes.rs"), 23, 5),
            Definition {
                location: Location::point(PathBuf::from("routes.rs"), 23, 5),
                is_self: true,
            },
        );
        definitions.insert(
            (PathBuf::from("routes.rs"), 23, 6),
            Definition {
                location: Location::point(PathBuf::from("routes.rs"), 23, 5),
                is_self: true,
            },
        );
        definitions.insert(
            (PathBuf::from("routes.rs"), 20, 12),
            Definition {
                location: Location::point(PathBuf::from("models/mod.rs"), 1, 1),
                is_self: false,
            },
        );
        definitions.insert(
            (PathBuf::from("routes.rs"), 35, 5),
            Definition {
                location: Location::point(PathBuf::from("routes.rs"), 23, 1),
                is_self: false,
            },
        );
        definitions.insert(
            (PathBuf::from("routes.rs"), 23, 3),
            Definition {
                location: Location::point(PathBuf::from("routes.rs"), 23, 1),
                is_self: false,
            },
        );
        definitions.insert(
            (PathBuf::from("routes.rs"), 25, 9),
            Definition {
                location: Location::point(PathBuf::from("routes.rs"), 25, 9),
                is_self: true,
            },
        );
        AnchorStub {
            symbols_by_file,
            definitions,
        }
    }

    async fn resolve(input: ParsedLocation) -> Result<Anchor, LspError> {
        resolve_anchor(&project(), &input, FindSymbolsOptions::default()).await
    }

    /// A column on a declaration's header — its keyword, its name, the end of
    /// its name — is that symbol, anchored at its name, and was never a usage
    /// of it: not when no token is there, and not when the server defines a
    /// keyword to the keyword's start from any position inside it
    /// (rust-analyzer does for `fn` and `async` — the input and its
    /// definition both read as the same declaration's header).
    #[tokio::test]
    async fn a_column_on_a_declaration_anchors_there() {
        for column in [1, 5, 6, 8, 14] {
            let anchor = resolve(at("routes.rs", 23, column)).await.unwrap();
            assert_eq!(
                anchor.symbol.as_ref().map(|s| s.name.as_str()),
                Some("handle")
            );
            assert_eq!((anchor.line, anchor.column), (23, 8));
            assert!(anchor.is_declaration());
            assert!(anchor.is_resolved());
            assert!(!anchor.via_definition);
        }
    }

    /// A token on the declaration's own line before the name — a return type,
    /// a receiver — is what it denotes, read through its definition like any
    /// other usage; only a position on no token there (the space after it)
    /// falls to the declaration whose header it is on.
    #[tokio::test]
    async fn a_token_before_the_name_is_read_before_the_header_claims_it() {
        let on_type = resolve(at("routes.rs", 50, 1)).await.unwrap();
        assert_eq!(
            on_type.symbol.as_ref().map(|s| s.name.as_str()),
            Some("Foo")
        );
        assert_eq!(on_type.file, PathBuf::from("types.rs"));
        assert!(on_type.via_definition);

        let on_space = resolve(at("routes.rs", 50, 4)).await.unwrap();
        assert_eq!(
            on_space.symbol.as_ref().map(|s| s.name.as_str()),
            Some("make")
        );
        assert!(!on_space.via_definition);
    }

    /// A column on a call site is the symbol called: the anchor moves to that
    /// declaration, in the file where it lives, so the analysis of a usage
    /// and of its declaration are the same analysis.
    #[tokio::test]
    async fn a_column_on_a_usage_anchors_at_the_definition() {
        let anchor = resolve(at("routes.rs", 31, 28)).await.unwrap();
        assert_eq!(
            anchor.symbol.as_ref().map(|s| s.name.as_str()),
            Some("process_order")
        );
        assert_eq!(anchor.file, PathBuf::from("svc.rs"));
        assert_eq!((anchor.line, anchor.column), (5, 8));
        assert!(anchor.is_declaration());
        assert!(anchor.is_resolved());
        assert!(anchor.via_definition);
        assert_eq!(anchor.input.line, 31);
    }

    /// An attribute above a declaration is a token of its own — a usage of
    /// the macro or decorator it names — and reads as such: through its
    /// definition, exactly as `def` and `hover` read it, never as the symbol
    /// it decorates. When that definition's file cannot be read, the failure
    /// is the answer, not a guess about what was found there.
    #[tokio::test]
    async fn an_attribute_column_reads_as_the_token_it_is() {
        assert!(matches!(
            resolve(at("routes.rs", 22, 3)).await,
            Err(LspError::ServerError { .. })
        ));
    }

    /// A definition elsewhere in the same file that no name occupies — a Go
    /// receiver between a keyword and a method name, a `crate::` segment
    /// naming a file root that opens with a declaration — is a binding at
    /// that position. The header it falls on belongs to another symbol;
    /// only a self-definition reads as the header's declaration.
    #[tokio::test]
    async fn a_same_file_definition_off_any_name_is_a_binding_not_the_header() {
        let anchor = resolve(at("routes.rs", 35, 5)).await.unwrap();
        assert!(anchor.symbol.is_none());
        assert_eq!(anchor.file, PathBuf::from("routes.rs"));
        assert_eq!((anchor.line, anchor.column), (23, 1));
        assert_eq!(anchor.resolution, AnchorResolution::Binding);
    }

    /// Even a usage INSIDE a declaration's header stays a binding when its
    /// definition names another position — `impl<T: Iterator<Item = T>>`:
    /// the second `T` sits on the impl's header yet denotes the parameter,
    /// not the impl. Only a self-definition reads as the header.
    #[tokio::test]
    async fn a_header_interior_usage_of_a_binding_stays_that_binding() {
        let anchor = resolve(at("routes.rs", 23, 3)).await.unwrap();
        assert!(anchor.symbol.is_none());
        assert_eq!((anchor.line, anchor.column), (23, 1));
        assert_eq!(anchor.resolution, AnchorResolution::Binding);
    }

    /// A module path segment defines to the file's first position, which no
    /// name occupies: it is a binding at that position, never the declaration
    /// that happens to open the file.
    #[tokio::test]
    async fn a_module_definition_is_not_the_declaration_that_opens_its_file() {
        let anchor = resolve(at("routes.rs", 20, 12)).await.unwrap();
        assert!(anchor.symbol.is_none());
        assert_eq!(anchor.file, PathBuf::from("models/mod.rs"));
        assert_eq!((anchor.line, anchor.column), (1, 1));
        assert_eq!(anchor.resolution, AnchorResolution::Binding);
    }

    /// A binding queried AT its own declaration — the server answers a
    /// self-definition on no header — is that binding, exactly as querying
    /// one of its usages is: same anchor, same disclosure, same counts.
    #[tokio::test]
    async fn a_binding_queried_at_its_own_declaration_is_that_binding() {
        let anchor = resolve(at("routes.rs", 25, 9)).await.unwrap();
        assert!(anchor.symbol.is_none());
        assert_eq!((anchor.line, anchor.column), (25, 9));
        assert!(anchor.is_declaration());
        assert_eq!(anchor.resolution, AnchorResolution::Binding);
    }

    /// A token whose definition the symbol tree does not list — a local, a
    /// parameter — still anchors at that declaration, disclosed as a binding
    /// rather than substituted with whatever encloses it.
    #[tokio::test]
    async fn a_column_on_a_binding_anchors_at_its_declaration_and_says_so() {
        let anchor = resolve(at("routes.rs", 33, 9)).await.unwrap();
        assert!(anchor.symbol.is_none());
        assert_eq!((anchor.line, anchor.column), (25, 9));
        assert!(anchor.is_declaration());
        assert_eq!(anchor.resolution, AnchorResolution::Binding);
    }

    /// A column the language server resolves to nothing addresses nothing:
    /// the anchor stays where it was given and is not a declaration.
    #[tokio::test]
    async fn a_column_on_nothing_stays_raw() {
        let anchor = resolve(at("routes.rs", 35, 1)).await.unwrap();
        assert!(anchor.symbol.is_none());
        assert_eq!((anchor.line, anchor.column), (35, 1));
        assert!(!anchor.is_declaration());
        assert_eq!(anchor.resolution, AnchorResolution::NotASymbol);
    }

    /// A line-only input reads the tree alone: a body line is the enclosing
    /// symbol, an attribute line above the name is that declaration's too, and
    /// a line with nothing declared or enclosing is not a symbol — never a
    /// definition lookup, since no token was named.
    #[tokio::test]
    async fn a_line_only_input_reads_the_tree_alone() {
        for line in [22, 31] {
            let anchor = resolve(on_line("routes.rs", line)).await.unwrap();
            assert_eq!(
                anchor.symbol.as_ref().map(|s| s.name.as_str()),
                Some("handle")
            );
        }

        let outside = resolve(on_line("routes.rs", 70)).await.unwrap();
        assert!(outside.symbol.is_none());
        assert_eq!(outside.resolution, AnchorResolution::NotASymbol);
        assert!(!outside.is_declaration());
    }

    /// A failed symbol read is the resolver's error, so a surface that needs
    /// the resolution reports it; one that queries the server anyway anchors
    /// at the raw position, disclosed as unavailable — never as "not a
    /// symbol", since nothing was checked.
    #[tokio::test]
    async fn an_unreadable_file_is_unavailable() {
        let input = at("missing.rs", 1, 1);
        assert!(resolve(input.clone()).await.is_err());
        let anchor = Anchor::unavailable(&input);
        assert_eq!(anchor.resolution, AnchorResolution::Unavailable);
        assert!(!anchor.is_declaration());
        assert_eq!((anchor.line, anchor.column), (1, 1));
    }

    fn reference(file: &str, line: u32, column: u32, end_column: u32) -> Location {
        Location::full(
            PathBuf::from(file),
            line,
            column,
            line,
            column,
            line,
            end_column,
        )
    }

    fn analysis_of(anchor: Anchor, references: Vec<Location>) -> LocationAnalysis {
        LocationAnalysis {
            declaring_type: None,
            anchor,
            language: Language::Rust,
            references,
            indexing: None,
        }
    }

    /// An input that resolved to a declaration through its definition is a
    /// usage of it, so a reference set that does not cover the input is
    /// contradicted by the question itself; one that does — even mid-token —
    /// is consistent. An input on the declaration itself, whether on its
    /// name or its keyword, was never a usage and claims nothing.
    #[tokio::test]
    async fn a_reference_set_that_omits_the_queried_usage_is_a_lower_bound() {
        let root = Path::new("");
        let at_usage = resolve(at("routes.rs", 33, 9)).await.unwrap();
        assert!(analysis_of(at_usage, vec![]).omits_input(root));

        let at_usage = resolve(at("routes.rs", 33, 9)).await.unwrap();
        let covering = vec![reference("routes.rs", 33, 7, 12)];
        assert!(!analysis_of(at_usage, covering).omits_input(root));

        for column in [1, 5, 8] {
            let at_declaration = resolve(at("routes.rs", 23, column)).await.unwrap();
            assert!(!analysis_of(at_declaration, vec![]).omits_input(root));
        }

        // An input outside the project cannot appear in the project-local
        // reference set, so its absence claims nothing.
        let outside = resolve(at("routes.rs", 33, 9)).await.unwrap();
        assert!(!analysis_of(outside, vec![]).omits_input(Path::new("/elsewhere")));
    }
}
