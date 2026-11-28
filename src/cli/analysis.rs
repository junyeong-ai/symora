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

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};

use crate::cli::ParsedLocation;
use crate::cli::response::disclosure::OutsideCallables;
use crate::cli::utils::{
    AnchorResolution, RefsClassification, SymbolResolution, Unresolved, ambiguity_hint,
    column_addressed_symbol, enclosing_callable, find_named_at_position, holder_of,
    line_addressed_symbol,
};
use crate::error::LspError;
use crate::infra::ast::called_names;
use crate::models::lsp::{CallHierarchyItem, FindSymbolsOptions, IndexingDegradation};
use crate::models::symbol::{Language, Location, Symbol, SymbolKind};
use crate::services::lsp::LspService;
use crate::services::test_scope::TestScope;

/// Where a symbol-level query anchors: the declaration the input position
/// addresses, resolved once and the same way for every surface — references,
/// callers, callees, implementations, impact, context.
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
    /// several declarations, or a column on the header several names share:
    /// the first was chosen, and this names the rest.
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

    /// The anchor of an input whose resolution could not be completed, for
    /// `cause`: the position as given, disclosed as unavailable, for the
    /// surfaces that query the language server at it anyway.
    pub fn unavailable(input: &ParsedLocation, cause: Unresolved) -> Self {
        Self::raw(input, AnchorResolution::Unavailable(cause))
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
    /// a local, a parameter, a generated item, one the server's outline
    /// omits. The analysis anchors at that
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
/// returned as the error it was, for the caller to classify: a surface that
/// goes on to query the server at the raw position discloses the anchor as
/// [`Anchor::unavailable`] where the failure is the read's own, one that needs
/// the resolution itself reports the failure.
pub async fn resolve_anchor(
    lsp: &dyn LspService,
    input: &ParsedLocation,
    options: FindSymbolsOptions,
) -> Result<Anchor, LspError> {
    let symbols = lsp.find_symbols(&input.file, options.clone()).await?;
    if !input.column_explicit {
        return Ok(anchor_on_line(input, &symbols));
    }
    anchor_at(lsp, input, &symbols, input.line, input.column, options).await
}

/// The anchor a column-less input addresses in `symbols`, its file's tree:
/// the declaration on its line, else the one enclosing it
/// ([`line_addressed_symbol`]).
pub fn anchor_on_line(input: &ParsedLocation, symbols: &[Symbol]) -> Anchor {
    match line_addressed_symbol(symbols, input.line) {
        SymbolResolution::Match(symbol) => {
            Anchor::declared(input, &input.file, symbol.clone(), None)
        }
        SymbolResolution::Ambiguous(declared) => {
            let hint = ambiguity_hint(input.line, &declared);
            Anchor::declared(input, &input.file, declared[0].clone(), Some(hint))
        }
        SymbolResolution::NotFound => Anchor::raw(input, AnchorResolution::NotASymbol),
    }
}

/// The anchor a column at `line:column` of the input's file addresses, given
/// `symbols`, the language server's tree of that file; the input is kept as
/// it was given.
pub async fn anchor_at(
    lsp: &dyn LspService,
    input: &ParsedLocation,
    symbols: &[Symbol],
    line: u32,
    column: u32,
    options: FindSymbolsOptions,
) -> Result<Anchor, LspError> {
    if let Some(symbol) = find_named_at_position(symbols, line, column) {
        return Ok(Anchor::declared(input, &input.file, symbol.clone(), None));
    }
    // A definition that names no other position — none at all, or the
    // server answering that the queried token is its own definition —
    // leaves the header reading: the declaration whose header the position
    // sits on. A self-definition off any header keeps one more fact a
    // missing definition lacks: the position DOES declare something — a
    // binding queried at its own declaration (a parameter, a local) — and
    // is disclosed as that binding, never as "not a symbol". A server still
    // loading its workspace defines nothing it has not read yet, so where
    // its readiness wait ran out, a missing definition resolves nothing.
    let answer = lsp.goto_definition(&input.file, line, column).await?;
    let loading = answer.indexing == Some(IndexingDegradation::TimedOut);
    let definition = answer.data;
    let self_declaration = definition
        .as_ref()
        .filter(|definition| definition.is_self)
        .map(|definition| definition.location.clone());
    let Some(definition) = definition.filter(|definition| !definition.is_self) else {
        return Ok(match column_addressed_symbol(symbols, line, column) {
            SymbolResolution::Match(symbol) => {
                Anchor::declared(input, &input.file, symbol.clone(), None)
            }
            SymbolResolution::Ambiguous(declared) => {
                let hint = ambiguity_hint(line, &declared);
                Anchor::declared(input, &input.file, declared[0].clone(), Some(hint))
            }
            SymbolResolution::NotFound => match self_declaration {
                Some(location) => Anchor::binding(input, &location),
                None if loading => Anchor::unavailable(input, Unresolved::Loading),
                None => Anchor::raw(input, AnchorResolution::NotASymbol),
            },
        });
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
        find_named_at_position(symbols, definition.line, definition.column).cloned()
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
            column_addressed_symbol(symbols, line, column),
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

/// Where a constructor its language runs by calling its type
/// ([`runs_through_its_type`]) is run: the calls of that type.
#[derive(Debug, Clone)]
pub struct Constructions {
    /// The type the constructor belongs to.
    pub type_name: String,
    /// The type's references the syntax tree reads as the name a call
    /// calls, in the references' order; `None` where the type's references
    /// or a file holding them could not be read, which leaves them unknown.
    pub sites: Option<Vec<Location>>,
}

impl Constructions {
    /// What a list of callers read off these constructions leaves out, said
    /// wherever one is given: they are a lower bound.
    pub fn hint(&self, member: &str) -> String {
        let type_name = &self.type_name;
        match &self.sites {
            Some(sites) => format!(
                "`{member}` runs where `{type_name}` is called, which its server does not \
                 report as a reference to `{member}`: the {} call(s) of `{type_name}` the \
                 syntax tree holds are read as its callers. A construction through an alias, \
                 `cls(…)`, `type(self)(…)` or a subclass is not among them, so they are a \
                 lower bound",
                sites.len()
            ),
            None => format!(
                "`{member}` runs where `{type_name}` is called, which its server does not \
                 report as a reference to `{member}`, and the calls of `{type_name}` could \
                 not be read, so what runs it is unknown"
            ),
        }
    }
}

/// Whether `language` runs a member by calling the type that declares it
/// rather than by naming the member, so that the server finds no reference
/// to the member where it runs: Python runs `__new__` and `__init__` for
/// `Circle(…)`, which pyright reports as a reference to `Circle` alone.
/// tsserver (tsc 7), clangd and rust-analyzer report `new Circle(…)`,
/// `Circle(…)` and `Circle::new(…)` as references to the constructor or the
/// `new` function itself, so they need no reading of the type's; other
/// languages are left out until a server's answer for them is checked.
fn runs_through_its_type(language: Language, name: &str) -> bool {
    language == Language::Python && matches!(name, "__init__" | "__new__")
}

/// The constructions that run `member`, declared in `file`, where its
/// language runs it by calling its type ([`runs_through_its_type`]): that
/// type's references the syntax tree reads as the callee of a call. `None`
/// for any other member, and where no type the server's tree lists holds it.
pub async fn constructions_of(
    lsp: &dyn LspService,
    file: &Path,
    member: &Symbol,
    root: &Path,
) -> Option<Constructions> {
    if !runs_through_its_type(Language::from_path(file), &member.name) {
        return None;
    }
    let symbols = lsp
        .find_symbols(file, FindSymbolsOptions::default().with_depth(10))
        .await
        .ok()?;
    let declaring = declaring_type_of(&symbols, member.location.line, member.location.column)
        .filter(|holder| holder.kind == SymbolKind::Class)?;
    let sites = match lsp
        .find_references(file, declaring.location.line, declaring.location.column)
        .await
    {
        Ok(references) => {
            let name = ParsedLocation {
                file: file.to_path_buf(),
                line: declaring.location.line,
                column: declaring.location.column,
                column_explicit: true,
            };
            let declared = declared_at(lsp, name, Some(declaring.kind)).await;
            called_among(usages_of(references.data, root, &declared))
        }
        Err(_) => None,
    };
    Some(Constructions {
        type_name: declaring.name,
        sites,
    })
}

/// The references the syntax tree of their files reads as the name a call
/// calls; `None` where one of their files could not be read.
fn called_among(references: Vec<Location>) -> Option<Vec<Location>> {
    let mut called: HashMap<PathBuf, HashSet<(u32, u32)>> = HashMap::new();
    let mut sites = Vec::new();
    for reference in references {
        if !called.contains_key(&reference.file) {
            let content = std::fs::read_to_string(&reference.file).ok()?;
            let names = called_names(&content, Language::from_path(&reference.file))?;
            called.insert(reference.file.clone(), names);
        }
        if called[&reference.file].contains(&(reference.line, reference.column)) {
            sites.push(reference);
        }
    }
    Some(sites)
}

pub struct LocationAnalysis {
    /// The declaration the input addressed — target, position, and how it
    /// resolved. Its `resolution.as_status()` is the `anchor_status` marker
    /// refs/impact/context emit, the same vocabulary the other surfaces use.
    pub(crate) anchor: Anchor,
    /// The name a binding anchor is written with: the text of the reference
    /// the server reports at it, which is its declaration. `None` for a
    /// listed symbol, and for a binding whose references hold none there.
    pub(crate) binding_name: Option<String>,
    pub(crate) language: Language,
    pub(crate) references: Vec<Location>,
    /// The indexing state the reference query ran under, captured at
    /// computation time by the service layer — the `indexing` output
    /// marker derives from this, never from a racy after-the-fact read.
    pub(crate) indexing: Option<IndexingDegradation>,
    pub(crate) declaring_type: Option<DeclaringType>,
    /// Where the anchor runs, for a constructor its language runs by
    /// calling its type, whose server reports none of those calls.
    pub(crate) constructions: Option<Constructions>,
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

    pub fn binding_name(&self) -> Option<&str> {
        self.binding_name.as_deref()
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
        let name = self.anchor.symbol.as_ref().map(|s| s.name.as_str())?;
        if let Some(constructions) = &self.constructions {
            return Some(constructions.hint(name));
        }
        let declaring = self.declaring_type.as_ref()?;
        Some(member_reach(name, declaring))
    }

    /// Where the anchor runs although no reference names it: see
    /// [`Constructions`].
    pub fn constructions(&self) -> Option<&Constructions> {
        self.constructions.as_ref()
    }

    /// How the anchor resolved — see [`AnchorResolution`]. Surfaces render
    /// its `as_status()` as the `anchor_status` disclosure marker.
    pub fn anchor_resolution(&self) -> AnchorResolution {
        self.anchor.resolution
    }

    /// Whether the language server's reference set contradicts the input: the
    /// input named a position, a usage — it resolved to this declaration
    /// through its definition, from somewhere else — yet no reference covers
    /// it; a column-less input names a line, which no reference covers. The
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
            && input.column_explicit
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

/// What an empty reference set leaves unsaid for a member of a type.
fn member_reach(name: &str, declaring: &DeclaringType) -> String {
    match declaring.references {
        0 => format!(
            "No reference to `{name}` was found, nor to `{}`, which declares it",
            declaring.name
        ),
        n => format!(
            "No reference to `{name}` was found, but `{}`, which declares it, is used {n} \
             time(s) — a call made through the type is not always found as a reference to the \
             member, so this is not evidence that `{name}` is unreachable",
            declaring.name
        ),
    }
}

/// [`LocationAnalysis::member_reach_hint`] for a declaration whose references
/// the caller counted itself and found none of.
pub async fn member_reach_hint_of(
    lsp: &dyn LspService,
    file: &Path,
    symbol: &Symbol,
    root: &Path,
) -> Option<String> {
    if let Some(constructions) = constructions_of(lsp, file, symbol, root).await {
        return Some(constructions.hint(&symbol.name));
    }
    let declaring = declaring_type_reach(
        lsp,
        file,
        symbol.location.line,
        symbol.location.column,
        root,
    )
    .await?;
    Some(member_reach(&symbol.name, &declaring))
}

/// The type a member is declared in, found by what holds it rather than by
/// its name — an outer function declares a nested one, and a call there has to
/// name it, so only a type weakens the member's zero. An impl is where a
/// Rust type's members are declared, and a Swift extension's; kotlin-lsp
/// sends a Kotlin `object` as an object.
fn declaring_type_of(symbols: &[Symbol], line: u32, column: u32) -> Option<Symbol> {
    holder_of(symbols, line, column)
        .filter(|holder| {
            matches!(
                holder.kind,
                SymbolKind::Class
                    | SymbolKind::Struct
                    | SymbolKind::Interface
                    | SymbolKind::Enum
                    | SymbolKind::Impl
                    | SymbolKind::Object
            )
        })
        .cloned()
}

/// How often the type declaring the member at a position is referenced, asked
/// only when the member's own set is empty — the one answer a caller reads as
/// absence.
async fn declaring_type_reach(
    lsp: &dyn LspService,
    file: &Path,
    line: u32,
    column: u32,
    root: &Path,
) -> Option<DeclaringType> {
    let symbols = lsp
        .find_symbols(file, FindSymbolsOptions::default().with_depth(10))
        .await
        .ok()?;
    let declaring = declaring_type_of(&symbols, line, column)?;
    let references = lsp
        .find_references(file, declaring.location.line, declaring.location.column)
        .await
        .ok()?;
    let name = ParsedLocation {
        file: file.to_path_buf(),
        line: declaring.location.line,
        column: declaring.location.column,
        column_explicit: true,
    };
    let declared = declared_at(lsp, name, Some(declaring.kind)).await;
    Some(DeclaringType {
        name: declaring.name,
        references: usages_of(references.data, root, &declared).len(),
    })
}

/// Where what a reference query at a declaration's name answers for is
/// declared, so its references can leave the declaration out: at that name,
/// except for an impl, whose name is the type it implements, declared
/// elsewhere.
async fn declared_at(
    lsp: &dyn LspService,
    name: ParsedLocation,
    kind: Option<SymbolKind>,
) -> Vec<ParsedLocation> {
    let mut declared = Vec::new();
    if kind == Some(SymbolKind::Impl)
        && let Ok(found) = lsp
            .goto_definition(&name.file, name.line, name.column)
            .await
        && let Some(definition) = found.data
        && !definition.is_self
    {
        declared.push(ParsedLocation {
            file: definition.location.file,
            line: definition.location.line,
            column: definition.location.column,
            column_explicit: true,
        });
    }
    declared.push(name);
    declared
}

impl LocationAnalysis {
    /// The references of the declaration an input addresses, taken from its
    /// resolved `anchor` so that a line-only, declaration-start, or
    /// usage-site input does not silently answer a different question.
    ///
    /// Hard-fails on `find_references` because every caller needs the refs
    /// list.
    pub async fn for_anchor(
        lsp: &dyn LspService,
        anchor: Anchor,
        root: &Path,
    ) -> Result<Self, LspError> {
        let language = Language::from_path(&anchor.file);
        let references = lsp
            .find_references(&anchor.file, anchor.line, anchor.column)
            .await?;
        let declared = match anchor.is_declaration() {
            true => {
                let kind = anchor.symbol.as_ref().map(|symbol| symbol.kind);
                declared_at(lsp, anchor.location(), kind).await
            }
            false => Vec::new(),
        };
        let binding_name = match anchor.resolution {
            AnchorResolution::Binding => references
                .data
                .iter()
                .find(|r| {
                    r.file == anchor.file && r.line == anchor.line && r.column == anchor.column
                })
                .and_then(written_name),
            _ => None,
        };
        let usages = usages_of(references.data, root, &declared);
        let constructions = match &anchor.symbol {
            Some(symbol) => constructions_of(lsp, &anchor.file, symbol, root).await,
            None => None,
        };
        let declaring_type = match usages.is_empty() && constructions.is_none() {
            true => declaring_type_reach(lsp, &anchor.file, anchor.line, anchor.column, root).await,
            false => None,
        };
        Ok(Self {
            anchor,
            binding_name,
            language,
            references: usages,
            indexing: references.indexing,
            declaring_type,
            constructions,
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
        let kind = symbol.kind;
        let anchor = Anchor::declared(&input, file, symbol, None);
        let language = Language::from_path(file);
        let references = lsp
            .find_references(&anchor.file, anchor.line, anchor.column)
            .await?;
        let declared = declared_at(lsp, input, Some(kind)).await;
        let usages = usages_of(references.data, root, &declared);
        let constructions = match &anchor.symbol {
            Some(symbol) => constructions_of(lsp, file, symbol, root).await,
            None => None,
        };
        Ok(Self {
            anchor,
            binding_name: None,
            language,
            references: usages,
            indexing: references.indexing,
            declaring_type: None,
            constructions,
        })
    }

    pub fn classify<'a>(&'a self, test_scope: &TestScope) -> RefsClassification<'a> {
        RefsClassification::of(&self.references, test_scope)
    }

    pub fn is_exported(&self) -> Option<bool> {
        if !matches!(
            self.language,
            Language::Rust
                | Language::Go
                | Language::Java
                | Language::Kotlin
                | Language::CSharp
                | Language::TypeScript
                | Language::JavaScript
        ) {
            return None;
        }
        let target = self.target()?;
        let content = std::fs::read_to_string(&self.anchor.file).ok()?;
        crate::services::symbols::SymbolExtractor::shared().declaration_visibility(
            &self.anchor.file,
            &content,
            self.language,
            target,
        )
    }
}

/// The text a one-line reference spans: the name as it is written there.
fn written_name(reference: &Location) -> Option<String> {
    let end = reference
        .end_column
        .filter(|_| reference.end_line == Some(reference.line))?;
    let content = std::fs::read_to_string(&reference.file).ok()?;
    let line: Vec<char> = content
        .lines()
        .nth(reference.line.checked_sub(1)? as usize)?
        .chars()
        .collect();
    let name: String = line
        .get(reference.column.checked_sub(1)? as usize..end.checked_sub(1)? as usize)?
        .iter()
        .collect();
    (!name.is_empty()).then_some(name)
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
fn usages_of(references: Vec<Location>, root: &Path, declared: &[ParsedLocation]) -> Vec<Location> {
    let mut usages: Vec<Location> = references
        .into_iter()
        .filter(|r| r.file.starts_with(root))
        .filter(|r| {
            !declared
                .iter()
                .any(|d| r.file == d.file && r.line == d.line && r.column == d.column)
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

/// The callables holding `references`: each function, method or
/// constructor whose code holds one, the innermost, once, with the first
/// reference it holds, in the order of the references. A reference outside
/// every callable — an import or annotation at module level, a statement in
/// a class body — is held by none.
///
/// `declarations` gives each referencing file's symbols; `None` where it has
/// none for one, which leaves what holds that reference unknown.
pub fn callables_holding<'a, 'r>(
    references: impl IntoIterator<Item = &'r Location>,
    declarations: impl Fn(&Path) -> Option<&'a [Symbol]>,
) -> Option<Vec<(&'a Symbol, &'r Location)>> {
    holding(references, declarations).map(|held| held.callables)
}

/// The callables holding references ([`callables_holding`]), and how many
/// of the references none holds.
struct Held<'a, 'r> {
    callables: Vec<(&'a Symbol, &'r Location)>,
    outside: OutsideCallables,
}

fn holding<'a, 'r>(
    references: impl IntoIterator<Item = &'r Location>,
    declarations: impl Fn(&Path) -> Option<&'a [Symbol]>,
) -> Option<Held<'a, 'r>> {
    let mut seen = HashSet::new();
    let mut callables = Vec::new();
    let mut outside = OutsideCallables::default();
    for reference in references {
        let symbols = declarations(&reference.file)?;
        outside.references += 1;
        let Some(callable) = enclosing_callable(symbols, reference.line, Some(reference.column))
        else {
            outside.outside += 1;
            continue;
        };
        let declared = &callable.location;
        if seen.insert((declared.file.clone(), declared.line, declared.column)) {
            callables.push((callable, reference));
        }
    }
    Some(Held { callables, outside })
}

/// The callers read off references ([`callers_from_references`]), and the
/// references that named none.
pub struct DerivedCallers {
    pub callers: Vec<CallHierarchyItem>,
    pub outside: OutsideCallables,
}

/// The callers references name: the callables holding them
/// ([`callables_holding`]), each at the first reference it holds as its
/// call site, and how many references lie outside every callable, which
/// name none. A reference inside one is a call or a use of the symbol as a
/// value there, a callback it registers, which these do not tell apart:
/// they are the callers a call hierarchy names where it counts every use
/// inside a callable, as rust-analyzer's does, and more than one that counts
/// calls alone, as pyright's does. One that names the module it runs in as
/// its caller, as pyright's does for a call at module level, names a caller
/// these do not.
pub fn callers_from_references<'a>(
    references: &[Location],
    declarations: impl Fn(&Path) -> Option<&'a [Symbol]>,
) -> Option<DerivedCallers> {
    let Held { callables, outside } = holding(references, declarations)?;
    Some(DerivedCallers {
        callers: callables
            .into_iter()
            .map(|(caller, reference)| CallHierarchyItem {
                name: caller.name.clone(),
                kind: caller.kind,
                location: caller.location.clone(),
                call_site: Some(reference.clone()),
            })
            .collect(),
        outside,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    use std::collections::HashMap;
    use std::path::PathBuf;

    use async_trait::async_trait;

    use crate::models::lsp::{
        ApplyActionResult, CallHierarchyItem, CodeAction, Definition, HoverInfo, Indexed,
        PrepareRenameResult, RenameResult,
    };
    use crate::models::symbol::SymbolKind;

    /// A reference names the innermost callable whose code holds it, once,
    /// at the first reference it holds; one in a type's header or body
    /// outside its methods, or at module level, names none, and is counted
    /// as such; and one in a file with no declarations to read leaves the
    /// callers unknown.
    #[test]
    fn references_name_the_callables_that_hold_them() {
        let file = PathBuf::from("/repo/m.py");
        let declared = |name: &str, kind, (line, column), (start, end): (u32, u32)| {
            Symbol::new(
                name.to_string(),
                kind,
                Location::full(file.clone(), line, column, start, 1, end, 20),
            )
        };
        let symbols =
            vec![
                declared("C", SymbolKind::Class, (1, 7), (1, 10)).with_children(vec![
                    declared("run", SymbolKind::Method, (3, 9), (3, 6)).with_children(vec![
                        declared("inner", SymbolKind::Function, (4, 13), (4, 5)),
                    ]),
                ]),
                declared("f", SymbolKind::Function, (12, 5), (12, 15)),
            ];
        let at = |line, column| Location::point(file.clone(), line, column);
        let references = [
            at(1, 9),
            at(2, 5),
            at(5, 9),
            at(6, 9),
            at(13, 5),
            at(14, 5),
            at(20, 1),
        ];
        let DerivedCallers { callers, outside } = callers_from_references(&references, |path| {
            (path == file.as_path()).then_some(symbols.as_slice())
        })
        .expect("the file's declarations were read");
        assert_eq!(
            outside,
            OutsideCallables {
                references: 7,
                outside: 3
            }
        );
        let named: Vec<(&str, u32)> = callers
            .iter()
            .map(|caller| {
                (
                    caller.name.as_str(),
                    caller.call_site.as_ref().unwrap().line,
                )
            })
            .collect();
        assert_eq!(named, [("inner", 5), ("run", 6), ("f", 13)]);
        assert_eq!(
            (callers[0].location.line, callers[0].location.column),
            (4, 13)
        );

        let elsewhere = [Location::point(PathBuf::from("/repo/n.py"), 3, 1)];
        assert!(
            callers_from_references(&elsewhere, |path| {
                (path == file.as_path()).then_some(symbols.as_slice())
            })
            .is_none()
        );
    }

    /// Serves canned per-file symbol trees and per-position definitions —
    /// the two reads anchor resolution makes. Everything else is unreachable
    /// from `resolve_anchor` and panics loudly if that ever changes.
    struct AnchorStub {
        symbols_by_file: HashMap<PathBuf, Vec<Symbol>>,
        definitions: HashMap<(PathBuf, u32, u32), Definition>,
        references: HashMap<(PathBuf, u32, u32), Vec<Location>>,
        /// The indexing state the definitions are answered under.
        indexing: Option<IndexingDegradation>,
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

    /// A definition the server names alone.
    fn definition(location: Location, is_self: bool) -> Definition {
        Definition {
            candidates: vec![location.clone()],
            location,
            is_self,
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
            Ok(Indexed::new(
                self.definitions
                    .get(&(file.to_path_buf(), line, column))
                    .cloned(),
                self.indexing,
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
            file: &Path,
            line: u32,
            column: u32,
        ) -> Result<Indexed<Vec<Location>>, LspError> {
            let references = self
                .references
                .get(&(file.to_path_buf(), line, column))
                .unwrap_or_else(|| unreachable!("no references at {line}:{column}"));
            Ok(Indexed::complete(references.clone()))
        }
        async fn goto_type_definition(
            &self,
            _file: &Path,
            _line: u32,
            _column: u32,
        ) -> Result<Indexed<Vec<Location>>, LspError> {
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
        // `var lo, hi = pick(…)` over lines 5 to 8: one statement, the whole
        // declaration of both names.
        symbols_by_file.insert(
            PathBuf::from("vars.go"),
            [("lo", 5), ("hi", 9)]
                .into_iter()
                .map(|(name, column)| {
                    Symbol::new(
                        name.to_string(),
                        SymbolKind::Variable,
                        Location::full(PathBuf::from("vars.go"), 5, column, 5, 1, 8, 2)
                            .with_name_end(5, column + 2),
                    )
                })
                .collect(),
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
            definition(Location::point(PathBuf::from("svc.rs"), 5, 8), false),
        );
        definitions.insert(
            (PathBuf::from("routes.rs"), 33, 9),
            definition(Location::point(PathBuf::from("routes.rs"), 25, 9), false),
        );
        definitions.insert(
            (PathBuf::from("routes.rs"), 22, 3),
            definition(Location::point(PathBuf::from("ext.rs"), 1, 12), false),
        );
        definitions.insert(
            (PathBuf::from("routes.rs"), 50, 1),
            definition(Location::point(PathBuf::from("types.rs"), 3, 12), false),
        );
        definitions.insert(
            (PathBuf::from("routes.rs"), 23, 5),
            definition(Location::point(PathBuf::from("routes.rs"), 23, 5), true),
        );
        definitions.insert(
            (PathBuf::from("routes.rs"), 23, 6),
            definition(Location::point(PathBuf::from("routes.rs"), 23, 5), true),
        );
        definitions.insert(
            (PathBuf::from("routes.rs"), 20, 12),
            definition(Location::point(PathBuf::from("models/mod.rs"), 1, 1), false),
        );
        definitions.insert(
            (PathBuf::from("routes.rs"), 35, 5),
            definition(Location::point(PathBuf::from("routes.rs"), 23, 1), false),
        );
        definitions.insert(
            (PathBuf::from("routes.rs"), 23, 3),
            definition(Location::point(PathBuf::from("routes.rs"), 23, 1), false),
        );
        definitions.insert(
            (PathBuf::from("routes.rs"), 25, 9),
            definition(Location::point(PathBuf::from("routes.rs"), 25, 9), true),
        );
        AnchorStub {
            symbols_by_file,
            definitions,
            references: HashMap::new(),
            indexing: None,
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

    /// A column on the keyword several names share anchors at the first of
    /// them and names the rest; a column on a name anchors at that name.
    #[tokio::test]
    async fn a_column_on_a_shared_header_anchors_at_the_first_name_and_says_so() {
        let keyword = resolve(at("vars.go", 5, 1)).await.unwrap();
        assert_eq!(keyword.symbol.as_ref().map(|s| s.name.as_str()), Some("lo"));
        assert!(
            keyword
                .hint
                .as_deref()
                .is_some_and(|hint| hint.contains("hi")),
            "{:?}",
            keyword.hint
        );
        let named = resolve(at("vars.go", 5, 9)).await.unwrap();
        assert_eq!(named.symbol.as_ref().map(|s| s.name.as_str()), Some("hi"));
        assert!(named.hint.is_none());
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

    /// A server still loading its workspace defines nothing it has not read,
    /// so where its readiness wait ran out a position it defines nothing at
    /// is unavailable, not "not a symbol". A server that reports no loading
    /// state answers definitions from what it has read of the file.
    #[tokio::test]
    async fn a_position_a_loading_server_defines_nothing_at_is_unavailable() {
        for (indexing, resolution) in [
            (
                IndexingDegradation::TimedOut,
                AnchorResolution::Unavailable(Unresolved::Loading),
            ),
            (
                IndexingDegradation::Unconfirmed,
                AnchorResolution::NotASymbol,
            ),
        ] {
            let stub = AnchorStub {
                indexing: Some(indexing),
                ..project()
            };
            let input = at("routes.rs", 35, 1);
            let anchor = resolve_anchor(&stub, &input, FindSymbolsOptions::default())
                .await
                .unwrap();
            assert_eq!(anchor.resolution, resolution, "{indexing:?}");
        }
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
        let anchor = Anchor::unavailable(&input, Unresolved::ReadFailed);
        assert_eq!(
            anchor.resolution,
            AnchorResolution::Unavailable(Unresolved::ReadFailed)
        );
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
            constructions: None,
            anchor,
            binding_name: None,
            language: Language::Rust,
            references,
            indexing: None,
        }
    }

    /// A member's zero is weakened by the type that declares it, which a Rust
    /// method finds in its `impl` block, and a function nested in another
    /// never does: a call there names it.
    #[test]
    fn a_member_is_declared_by_its_type_or_impl_block_and_never_by_a_function() {
        let holder = |kind: SymbolKind| {
            Symbol::new(
                "Widget".to_string(),
                kind,
                Location::full(PathBuf::from("lib.rs"), 1, 6, 1, 1, 5, 2),
            )
            .with_children(vec![func("lib.rs", "render", 2, 8, 4)])
        };
        for kind in [
            SymbolKind::Impl,
            SymbolKind::Object,
            SymbolKind::Struct,
            SymbolKind::Class,
        ] {
            assert_eq!(
                declaring_type_of(&[holder(kind)], 2, 8).map(|symbol| symbol.kind),
                Some(kind)
            );
        }
        assert!(declaring_type_of(&[holder(SymbolKind::Function)], 2, 8).is_none());
    }

    /// An `impl` block's name is the type it implements, so a reference query
    /// there answers for that type, declared elsewhere. Its declaration is
    /// left out with the block's own name: in the block's count, and in the
    /// reach a member's empty set reports for the type that declares it.
    #[tokio::test]
    async fn an_impl_block_leaves_out_the_declaration_of_the_type_it_implements() {
        let lib = PathBuf::from("lib.rs");
        let declared = Location::point(lib.clone(), 1, 12);
        let render = func("lib.rs", "render", 4, 12, 6);
        let block = Symbol::new(
            "Widget".to_string(),
            SymbolKind::Impl,
            Location::full(lib.clone(), 3, 6, 3, 1, 7, 2),
        )
        .with_children(vec![render.clone()]);
        let stub = AnchorStub {
            symbols_by_file: HashMap::from([(lib.clone(), vec![block.clone()])]),
            definitions: HashMap::from([(
                (lib.clone(), 3, 6),
                definition(declared.clone(), false),
            )]),
            references: HashMap::from([(
                (lib.clone(), 3, 6),
                vec![declared, Location::point(lib.clone(), 3, 6)],
            )]),
            indexing: None,
        };

        let analysis = LocationAnalysis::for_symbol(&stub, &lib, block, Path::new(""))
            .await
            .unwrap();
        assert!(analysis.references.is_empty(), "{:?}", analysis.references);
        assert_eq!(
            member_reach_hint_of(&stub, &lib, &render, Path::new(""))
                .await
                .as_deref(),
            Some("No reference to `render` was found, nor to `Widget`, which declares it")
        );
    }

    /// A binding is named as its declaration writes it: by the reference the
    /// server reports there, in scalar columns, even where its definition
    /// spans more than the name (pyright's spans a parameter's annotation).
    /// References that hold nothing there leave it unnamed, never named by a
    /// usage.
    #[tokio::test]
    async fn a_binding_is_named_by_the_reference_at_its_declaration() {
        let directory = tempfile::tempdir().unwrap();
        let file = directory.path().join("main.py");
        std::fs::write(&file, "t = \"🎉\"; radius: float = 2\nprint(radius)\n").unwrap();
        let path = file.to_str().unwrap();
        let declaration = reference(path, 1, 10, 16);
        let usage = reference(path, 2, 7, 13);
        let definition = Location {
            end_column: Some(23),
            ..declaration.clone()
        };
        let stub = |references| AnchorStub {
            symbols_by_file: HashMap::new(),
            definitions: HashMap::new(),
            references: HashMap::from([((file.clone(), 1, 10), references)]),
            indexing: None,
        };
        let anchor = || Anchor::binding(&at(path, 2, 7), &definition);

        let named = LocationAnalysis::for_anchor(
            &stub(vec![declaration, usage.clone()]),
            anchor(),
            directory.path(),
        )
        .await
        .unwrap();
        assert_eq!(named.binding_name(), Some("radius"));
        assert_eq!(named.references, vec![usage.clone()]);
        let unnamed = LocationAnalysis::for_anchor(&stub(vec![usage]), anchor(), directory.path())
            .await
            .unwrap();
        assert_eq!(unnamed.binding_name(), None);
    }

    #[test]
    fn visibility_matches_the_exact_declaration_in_the_source_file() {
        let directory = tempfile::tempdir().unwrap();
        let file = directory.path().join("lib.rs");
        let source = "/// Size.\n#[inline]\npub fn len() -> usize {\n    0\n}";
        std::fs::write(&file, source).unwrap();
        let mut anchor = Anchor::raw(
            &at(file.to_str().unwrap(), 3, 8),
            AnchorResolution::Resolved,
        );
        anchor.symbol = Some(
            Symbol::new(
                "len".to_string(),
                SymbolKind::Function,
                Location::full(file, 3, 8, 1, 1, 5, 2),
            )
            .with_body("/// Size.\n#[inline]\npub fn len() -> usize {\n    0\n}"),
        );
        assert_eq!(analysis_of(anchor, vec![]).is_exported(), Some(true));
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
