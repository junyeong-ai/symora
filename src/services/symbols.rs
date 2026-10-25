use std::collections::{BTreeSet, HashMap, HashSet};
use std::path::Path;
use std::sync::Mutex;

use streaming_iterator::StreamingIterator;
use tree_sitter::{Node, Parser, Query, QueryCursor, Tree};

use crate::models::symbol::{Language, Location, Symbol, SymbolKind};

/// A parser and its pre-compiled extraction query for one language. The
/// query is compiled once at startup, not per file — query compilation is
/// the dominant per-call cost otherwise.
struct LanguageEntry {
    parser: Mutex<Parser>,
    query: Query,
    /// The query's `@symbol` capture, the declaration a match reads; a
    /// pattern can capture more to compare in a predicate.
    declaration: u32,
}

/// Tree-sitter declaration extraction: the index's symbol rows, and the
/// grammar's answer wherever a language server does not read a file. One
/// registry maps each supported language to its parser and compiled query;
/// adding a language is a single `register` call.
pub struct SymbolExtractor {
    languages: HashMap<Language, LanguageEntry>,
}

impl Default for SymbolExtractor {
    fn default() -> Self {
        Self::new()
    }
}

/// Every language this binary extracts symbols from, with the grammar that
/// parses it and the query that reads its declarations. One row per language:
/// adding one is a row, and the digest below is taken from the same place, so
/// a query cannot change without the indexes it produced being retired.
const EXTRACTORS: &[(Language, fn() -> tree_sitter::Language, &str)] = &[
    (
        Language::Rust,
        || tree_sitter_rust::LANGUAGE.into(),
        RUST_QUERY,
    ),
    (Language::Go, || tree_sitter_go::LANGUAGE.into(), GO_QUERY),
    (
        Language::Python,
        || tree_sitter_python::LANGUAGE.into(),
        PYTHON_QUERY,
    ),
    (
        Language::TypeScript,
        || tree_sitter_typescript::LANGUAGE_TSX.into(),
        TYPESCRIPT_QUERY,
    ),
    (
        Language::JavaScript,
        || tree_sitter_javascript::LANGUAGE.into(),
        JAVASCRIPT_QUERY,
    ),
    (
        Language::Java,
        || tree_sitter_java::LANGUAGE.into(),
        JAVA_QUERY,
    ),
    (
        Language::Kotlin,
        || tree_sitter_kotlin_sg::LANGUAGE.into(),
        KOTLIN_QUERY,
    ),
    (
        Language::Cpp,
        || tree_sitter_cpp::LANGUAGE.into(),
        CPP_QUERY,
    ),
    (
        Language::CSharp,
        || tree_sitter_c_sharp::LANGUAGE.into(),
        CSHARP_QUERY,
    ),
    (
        Language::PHP,
        || tree_sitter_php::LANGUAGE_PHP.into(),
        PHP_QUERY,
    ),
    (
        Language::Ruby,
        || tree_sitter_ruby::LANGUAGE.into(),
        RUBY_QUERY,
    ),
    (
        Language::Bash,
        || tree_sitter_bash::LANGUAGE.into(),
        BASH_QUERY,
    ),
    (
        Language::Lua,
        || tree_sitter_lua::LANGUAGE.into(),
        LUA_QUERY,
    ),
    (
        Language::Swift,
        || tree_sitter_swift::LANGUAGE.into(),
        SWIFT_QUERY,
    ),
    (
        Language::Scala,
        || tree_sitter_scala::LANGUAGE.into(),
        SCALA_QUERY,
    ),
    (
        Language::Dart,
        || tree_sitter_dart::LANGUAGE.into(),
        DART_QUERY,
    ),
    (
        Language::Terraform,
        || tree_sitter_hcl::LANGUAGE.into(),
        TERRAFORM_QUERY,
    ),
];

impl SymbolExtractor {
    pub fn new() -> Self {
        let mut languages = HashMap::new();
        for (language, grammar, query) in EXTRACTORS {
            register(&mut languages, *language, grammar(), query);
        }
        Self { languages }
    }

    /// What this binary's extraction would make of a file, as one value.
    ///
    /// The index records it, because rows are only as good as the extractor
    /// that produced them: a file whose bytes have not changed is skipped by a
    /// rebuild, so an index built by another extractor would keep serving its
    /// rows — an answer short by exactly the declaration forms the new one was
    /// written to reach, with nothing to say so. The extractor is its grammars,
    /// its queries and the code that reads their matches, which the release
    /// stands for, so a build made by another release or under another query
    /// is retired.
    pub fn extraction_digest() -> u64 {
        let described = EXTRACTORS
            .iter()
            .map(|(language, grammar, query)| {
                let grammar = grammar();
                format!(
                    "{}\u{1}{}\u{1}{}\u{1}{:?}\u{1}{query}",
                    env!("CARGO_PKG_VERSION"),
                    language.lsp_id(),
                    grammar.abi_version(),
                    grammar.metadata().map(|metadata| (
                        metadata.major_version,
                        metadata.minor_version,
                        metadata.patch_version
                    )),
                )
            })
            .collect::<Vec<_>>()
            .join("\u{2}");
        crate::infra::hash_content(&described)
    }

    /// Languages with a compiled-in index extractor.
    pub fn supported_languages() -> &'static [Language] {
        static LANGUAGES: std::sync::LazyLock<Vec<Language>> = std::sync::LazyLock::new(|| {
            EXTRACTORS
                .iter()
                .map(|(language, _, _)| *language)
                .collect()
        });
        &LANGUAGES
    }

    pub fn is_supported(language: Language) -> bool {
        Self::supported_languages().contains(&language)
    }

    /// The process-wide extractor. Grammars and their queries are compiled
    /// into the binary and their compilation is the dominant per-call cost,
    /// so they are built once rather than per store or per call.
    pub fn shared() -> &'static Self {
        static SHARED: std::sync::LazyLock<SymbolExtractor> =
            std::sync::LazyLock::new(SymbolExtractor::new);
        &SHARED
    }

    /// The declarations `content` makes, as the same [`Symbol`] a language
    /// server's document-symbol answer produces — one shape, so a caller
    /// reads either source through the same fields.
    ///
    /// The list is flat: containment is carried by `container` and
    /// `name_path`, which is what addresses a symbol everywhere else.
    pub fn extract(&self, path: &Path, content: &str, language: Language) -> Vec<Symbol> {
        self.extract_where(path, content, language, |_, _| true)
    }

    /// Visibility on this declaration, without resolving its containing API or re-exports.
    pub fn declaration_visibility(
        &self,
        path: &Path,
        content: &str,
        language: Language,
        target: &Symbol,
    ) -> Option<bool> {
        self.with_declarations(path, content, language, |declarations| {
            let (node, symbol) = declarations.iter().find(|(_, symbol)| {
                symbol.name == target.name
                    && symbol.location.line == target.location.line
                    && symbol.location.column == target.location.column
            })?;
            let mut root = *node;
            while let Some(parent) = root.parent() {
                root = parent;
            }
            if root.has_error() {
                return None;
            }
            let declared = declarations.iter().map(|(node, _)| node.id()).collect();
            declaration_visibility(*node, symbol, content, language, &declared)
        })?
    }

    /// The declarations of [`extract`](Self::extract) that stand for
    /// themselves: none that belongs to a body a declaration stands for
    /// ([`is_local`]). What is declared inside such a body is its own, and a
    /// change to it is a change to the body.
    pub fn extract_members(&self, path: &Path, content: &str, language: Language) -> Vec<Symbol> {
        self.extract_where(path, content, language, |node, declared| {
            !is_local(node, declared)
        })
    }

    pub fn extract_signatures(
        &self,
        path: &Path,
        content: &str,
        language: Language,
    ) -> Vec<(Symbol, String)> {
        let Some(tree) = self.parse(content, language) else {
            return Vec::new();
        };
        let lines = LineStarts::new(content);
        let read = self.declarations_in(&tree, path, content, language);
        let declared = read.iter().map(|(node, _)| node.id()).collect();
        read.into_iter()
            .filter(|(node, _)| !is_local(*node, &declared))
            .filter_map(|(_, symbol)| {
                self.signature_at(&tree, &lines, language, &symbol.location)
                    .map(|text| (symbol, text))
            })
            .collect()
    }

    /// The declarations [`extract`](Self::extract) reads, each with its
    /// signature ([`describe`](Self::describe)), from one parse.
    pub fn extract_with_signatures(
        &self,
        path: &Path,
        content: &str,
        language: Language,
    ) -> Vec<Symbol> {
        let Some(tree) = self.parse(content, language) else {
            return Vec::new();
        };
        let mut symbols: Vec<Symbol> = self
            .declarations_in(&tree, path, content, language)
            .into_iter()
            .map(|(_, symbol)| symbol)
            .collect();
        self.describe(&mut symbols, &tree, &LineStarts::new(content), language);
        symbols
    }

    /// Where each type's own declaration is written, outside the body that
    /// holds its members ([`type_body`]): its decorators or attributes, name,
    /// parameters, bases and clauses, on one line or several. A comment is
    /// part of neither, and a type with no body (an alias, a unit struct) is
    /// all header.
    pub fn type_headers(&self, path: &Path, content: &str, language: Language) -> TypeHeaders {
        self.with_declarations(path, content, language, |read| {
            let mut headers = TypeHeaders::default();
            for (node, symbol) in &read {
                if !symbol.kind.holds_members() {
                    continue;
                }
                let body = type_body(*node, language);
                let opens = declaration_start(declaration_node(*node, language), language);
                headers.lines.extend(
                    opens.start_position().row as u32 + 1..=node.start_position().row as u32 + 1,
                );
                let mut cursor = node.walk();
                for child in node.children(&mut cursor) {
                    if !child.is_extra() && body.is_none_or(|body| body.id() != child.id()) {
                        headers
                            .lines
                            .extend(line_span(child.start_position(), child.end_position()));
                    }
                }
                let (to_byte, to_point) = body
                    .map_or((node.end_byte(), node.end_position()), |body| {
                        (body.start_byte(), body.start_position())
                    });
                headers.spans.push(HeaderSpan {
                    name: (symbol.location.line, symbol.location.column),
                    from: scalar_position(content, opens.start_byte(), opens.start_position()),
                    to: scalar_position(content, to_byte, to_point),
                });
            }
            headers
        })
        .unwrap_or_default()
    }

    /// Give each of `symbols` — a language server's answer for `content` —
    /// the extent of the declaration this grammar reads at its name, where
    /// that extent is wider, and the signature it is written with
    /// ([`describe`](Self::describe)).
    ///
    /// A server's range can leave out syntax that is part of the declaration:
    /// clangd a `template` header, an attribute on its own line and a class's
    /// closing `;`, tsserver the `const` and `;` of a `const f = () => {}`,
    /// rust-analyzer a field's `,`. Every surface that splices, slices or
    /// attributes lines by a symbol's range reads it from the answer, so the
    /// answer carries the whole declaration whichever server gave it. The two
    /// readings are joined by where the name starts or, failing that, where it
    /// ends: a qualified C++ name (`Widget::set`) starts at its qualifier in
    /// the grammar and at its last part in clangd, and ends at the same place
    /// in both. Each is a position one token occupies, so no server's names or
    /// kinds are compared with the grammar's; a symbol with no grammar reading
    /// there keeps the server's range, and so does each end of one the grammar
    /// read there only by recovering from a syntax error ([`read_as_written`]).
    pub fn complete_from_declarations(
        &self,
        symbols: &mut [Symbol],
        path: &Path,
        content: &str,
        language: Language,
    ) {
        type Extent = (Option<(u32, u32)>, Option<(u32, u32)>);
        struct Extents {
            by_start: HashMap<(u32, u32), Extent>,
            by_end: HashMap<(u32, u32), Extent>,
        }
        fn widen(symbols: &mut [Symbol], extents: &Extents) {
            for symbol in symbols {
                let location = &mut symbol.location;
                let named = extents
                    .by_start
                    .get(&(location.line, location.column))
                    .or_else(|| {
                        let end = (location.name_end_line?, location.name_end_column?);
                        extents.by_end.get(&end)
                    });
                if let Some(&(start, end)) = named {
                    if let Some(start) = start
                        && start < location.effective_start()
                    {
                        location.range_start_line = Some(start.0);
                        location.range_start_column = Some(start.1);
                    }
                    let ends = (
                        location.end_line.unwrap_or(location.line),
                        location.end_column.unwrap_or(location.column),
                    );
                    if let Some(end) = end
                        && end > ends
                    {
                        location.end_line = Some(end.0);
                        location.end_column = Some(end.1);
                    }
                }
                widen(&mut symbol.children, extents);
            }
        }

        let Some(tree) = self.parse(content, language) else {
            return;
        };
        let mut extents = Extents {
            by_start: HashMap::new(),
            by_end: HashMap::new(),
        };
        for (node, declared) in self.declarations_in(&tree, path, content, language) {
            let written = read_as_written(node, language);
            let location = declared.location;
            let (Some(end_line), Some(end_column)) = (location.end_line, location.end_column)
            else {
                continue;
            };
            let extent = (
                written.start.then(|| location.effective_start()),
                written.end.then_some((end_line, end_column)),
            );
            extents
                .by_start
                .insert((location.line, location.column), extent);
            if let (Some(line), Some(column)) = (location.name_end_line, location.name_end_column) {
                extents.by_end.insert((line, column), extent);
            }
            // clangd names a structured binding `(anonymous)` at its `[`, where
            // the grammar reads each name the binding writes.
            if let Some(binding) =
                std::iter::successors(node.child_by_field_name("declarator"), |declarator| {
                    declarator.child_by_field_name("declarator")
                })
                .find(|declarator| declarator.kind() == "structured_binding_declarator")
            {
                extents.by_start.insert(
                    scalar_position(content, binding.start_byte(), binding.start_position()),
                    extent,
                );
            }
        }
        widen(symbols, &extents);
        self.describe(symbols, &tree, &LineStarts::new(content), language);
    }

    /// The declarations `keep` accepts, given each one's node and the nodes
    /// every declaration was read from.
    fn extract_where(
        &self,
        path: &Path,
        content: &str,
        language: Language,
        keep: impl Fn(Node, &HashSet<usize>) -> bool,
    ) -> Vec<Symbol> {
        self.with_declarations(path, content, language, |read| {
            let declared = read.iter().map(|(node, _)| node.id()).collect();
            read.into_iter()
                .filter(|(node, _)| keep(*node, &declared))
                .map(|(_, symbol)| symbol)
                .collect()
        })
        .unwrap_or_default()
    }

    /// Every declaration the grammar reads in `content`, with the node it was
    /// read from, handed to `read` while the tree they belong to is alive.
    /// `None` when the language has no grammar here or the parse failed.
    fn with_declarations<R>(
        &self,
        path: &Path,
        content: &str,
        language: Language,
        read: impl FnOnce(Vec<(Node, Symbol)>) -> R,
    ) -> Option<R> {
        let tree = self.parse(content, language)?;
        Some(read(self.declarations_in(&tree, path, content, language)))
    }

    /// Give each of `symbols`, and each symbol under them, the signature it is
    /// written with in `tree` ([`signature_at`](Self::signature_at)).
    fn describe(
        &self,
        symbols: &mut [Symbol],
        tree: &Tree,
        lines: &LineStarts,
        language: Language,
    ) {
        for symbol in symbols {
            symbol.signature = self.signature_at(tree, lines, language, &symbol.location);
            self.describe(&mut symbol.children, tree, lines, language);
        }
    }

    /// What the declaration at `location` is written with ahead of its body or
    /// value: its name, parameters, type and modifiers.
    ///
    /// It is read where the declaration is written in the whole file, inside
    /// whatever holds it — a method read out of its class is no declaration in
    /// TypeScript, Java or C#, nor is PHP without its opening tag — and from the
    /// declaration's own place rather than from what a query reads there, so a
    /// declaration no query captures (a Python assignment under an `if`) and
    /// one a server places by another token (clangd a destructor by its `~`)
    /// have one too ([`read_signature`]).
    ///
    /// A recovered reading is not one of the text as written: where the whole
    /// file reads the signature only by recovering from a syntax error — a
    /// macro the grammar cannot expand, read as the type of what follows it —
    /// the declaration's own text, as its range bounds it, is read alone
    /// ([`code_opening`]), and where that does not read as written either
    /// there is no signature.
    fn signature_at(
        &self,
        tree: &Tree,
        lines: &LineStarts,
        language: Language,
        location: &Location,
    ) -> Option<String> {
        let content = lines.content;
        let name = lines.byte(location.line, location.column)?;
        let (start_line, start_column) = location.effective_start();
        let start = lines.byte(start_line, start_column)?.min(name);
        let end = match (location.end_line, location.end_column) {
            (Some(line), Some(column)) => lines.byte(line, column)?,
            _ => name,
        };
        match read_signature(tree.root_node(), content, language, name, start, end)? {
            Read::AsWritten(text) => Some(text),
            Read::Recovered => {
                let opening = code_opening(language);
                let own = format!("{opening}{}", content.get(start..end.max(name))?);
                let tree = self.parse(&own, language)?;
                let at = opening.len();
                match read_signature(
                    tree.root_node(),
                    &own,
                    language,
                    name - start + at,
                    at,
                    own.len(),
                )? {
                    Read::AsWritten(text) => Some(text),
                    Read::Recovered => None,
                }
            }
        }
    }

    /// `content` read by this language's grammar, or `None` where it has no
    /// grammar here or the parse failed. C and C++ are read again past the
    /// macros a class definition holds where the grammar took it for a function
    /// ([`misread_class_macros`]), each spelled as blanks, so every offset
    /// still addresses `content`.
    fn parse(&self, content: &str, language: Language) -> Option<Tree> {
        let entry = self.languages.get(&language)?;
        let mut parser = entry.parser.lock().ok()?;
        let mut tree = parser.parse(content, None)?;
        if language == Language::Cpp {
            let mut blanked: Option<Vec<u8>> = None;
            loop {
                let source = blanked.as_deref().unwrap_or(content.as_bytes());
                let macros = misread_class_macros(tree.root_node(), source);
                if macros.is_empty() {
                    break;
                }
                let bytes = blanked.get_or_insert_with(|| content.as_bytes().to_vec());
                for spelled in macros {
                    bytes[spelled].fill(b' ');
                }
                tree = parser.parse(&*bytes, None)?;
            }
        }
        Some(tree)
    }

    /// Every declaration the grammar reads in `tree`, with the node it was
    /// read from.
    fn declarations_in<'t>(
        &self,
        tree: &'t Tree,
        path: &Path,
        content: &str,
        language: Language,
    ) -> Vec<(Node<'t>, Symbol)> {
        let Some(entry) = self.languages.get(&language) else {
            return Vec::new();
        };
        let mut cursor = QueryCursor::new();
        let mut nodes = Vec::new();
        let mut declared = HashSet::new();
        let fragments = match language {
            Language::Cpp => opened_by_an_error(tree.root_node(), content),
            _ => HashSet::new(),
        };
        let mut matches = cursor.matches(&entry.query, tree.root_node(), content.as_bytes());
        while let Some(m) = matches.next() {
            for capture in m.captures() {
                if capture.index == entry.declaration
                    && declares(capture.node, content, language)
                    && !std::iter::successors(Some(capture.node), Node::parent)
                        .any(|node| fragments.contains(&node.id()))
                    && declared.insert(capture.node.id())
                {
                    nodes.push(capture.node);
                }
            }
        }
        let mut bound = HashSet::new();
        nodes
            .into_iter()
            .flat_map(|node| {
                extract_declaration(node, &declared, path, content, language)
                    .into_iter()
                    .map(move |symbol| (node, symbol))
            })
            .filter(|(node, symbol)| {
                language != Language::Python
                    || node.kind() != "assignment"
                    || bound.insert((binding_scope(*node), symbol.name.clone()))
            })
            .collect()
    }
}

/// The nodes of C or C++ source the grammar read as fragments of a construct
/// a syntax error began rather than as declarations of their own, by the
/// brackets the error left loose:
///
/// - what an error holds after a function's header and the `{` opening its
///   body (a local of a function whose body the grammar lost), up to a `}`
///   it holds loose or a function definition the grammar read whole, which
///   C cannot nest, so the body has ended before it;
/// - the node right after one that leaves a bracket open, which closes it
///   (the parameters a function-pointer return type writes after the
///   function's own, read as a declaration ending in a stray `)`);
/// - the node right after a declaration the grammar ended by supplying its
///   missing `;`, which holds the rest of it up to the `;` the source writes
///   (`MACRO_A MACRO_B;` after `int f(int x)`, read as a declaration of
///   `MACRO_B`), unless the declaration invoked a macro, whose rest holds
///   what it names ([`continued_macro`]).
///
/// No other loose bracket opens anything: the grammar strands brackets whose
/// partners it absorbed elsewhere (a macro body it failed to read as one),
/// and counting those would unread the rest of the file. The brace of
/// `extern "C" {` or `namespace n {` encloses declarations of the file's
/// scope. Where a lost body ends is read from the source (`body_end`), since
/// the grammar lost it by giving its `}` to a brace a preprocessor branch
/// opened; a body whose end the source does not settle either is unread up to
/// the next function definition, rather than misread.
fn opened_by_an_error(root: Node, content: &str) -> HashSet<usize> {
    /// What a bracket a syntax error holds does to the brackets open before
    /// it: opens one that holds fragments, opens one that holds the file's
    /// declarations, or closes the last.
    enum Bracket {
        Opaque,
        Transparent,
        Close,
    }
    fn loose(token: Node) -> Option<Bracket> {
        if token.is_named() || token.is_missing() {
            return None;
        }
        let before = || {
            std::iter::successors(token.prev_sibling(), Node::prev_sibling)
                .filter(|before| !before.is_extra())
        };
        match token.kind() {
            "{" if before().next().is_some_and(|linkage| {
                linkage.kind() == "string_literal"
                    && linkage
                        .prev_sibling()
                        .is_some_and(|keyword| keyword.kind() == "extern")
            }) || before()
                .take(2)
                .any(|keyword| keyword.kind() == "namespace") =>
            {
                Some(Bracket::Transparent)
            }
            "(" | "{" => Some(Bracket::Opaque),
            ")" | "}" => Some(Bracket::Close),
            _ => None,
        }
    }
    /// Applies to `open` what the brackets `node` holds loose do: its own
    /// where an error holds it, and those of every error it holds.
    fn apply(open: &mut Vec<bool>, node: Node, held_by_an_error: bool) {
        if held_by_an_error {
            match loose(node) {
                Some(Bracket::Opaque) => open.push(true),
                Some(Bracket::Transparent) => open.push(false),
                Some(Bracket::Close) => {
                    open.pop();
                }
                None => {}
            }
        }
        if node.has_error() {
            let mut cursor = node.walk();
            for child in node.children(&mut cursor) {
                apply(open, child, node.is_error());
            }
        }
    }
    let opens_a_body = |token: Node| {
        token.kind() == "{"
            && !token.is_missing()
            && std::iter::successors(token.prev_sibling(), Node::prev_sibling)
                .find(|before| !before.is_extra())
                .is_some_and(|header| header.kind() == "function_declarator")
    };
    let mut inside = HashSet::new();
    let mut pending = vec![root];
    while let Some(node) = pending.pop() {
        if !node.has_error() {
            continue;
        }
        let mut lost_body_until = None;
        let mut left_open = Vec::new();
        let mut continues = false;
        let mut cursor = node.walk();
        for child in node.children(&mut cursor) {
            if child.is_extra() && !child.is_error() {
                pending.push(child);
                continue;
            }
            let mut closed = left_open.clone();
            apply(&mut closed, child, node.is_error());
            if left_open.contains(&true) && closed.len() < left_open.len()
                || std::mem::replace(
                    &mut continues,
                    ends_on_a_supplied_semicolon(child) && !invokes_a_macro(child),
                )
            {
                inside.insert(child.id());
            }
            if node.is_error() {
                match child.kind() {
                    "function_definition" => lost_body_until = None,
                    "}" if !child.is_missing() => lost_body_until = None,
                    _ if lost_body_until.is_some_and(|end| child.start_byte() < end) => {
                        inside.insert(child.id());
                    }
                    _ => {
                        lost_body_until = opens_a_body(child)
                            .then(|| body_end(content, child.start_byte()).unwrap_or(usize::MAX));
                    }
                }
            }
            left_open.clear();
            apply(&mut left_open, child, node.is_error());
            pending.push(child);
        }
    }
    inside
}

/// The macros of each C or C++ class definition the grammar read as a
/// function definition: one whose declarator names no function, as only a
/// type's can, with a class key before its block (`class EXPORT Widget {`,
/// `OPENSSL_EXPORT struct ssl_st {`, `ABSL_NAMESPACE_BEGIN class Clock {`). An
/// identifier written before the key, or between the key and the name the
/// class head ends with (qualified, specialized, then `final`), is no part of
/// the grammar there, so it is a macro; one inside brackets is an argument
/// (`__declspec(dllexport)`), and the head ends where a base clause begins.
fn misread_class_macros(root: Node, source: &[u8]) -> Vec<std::ops::Range<usize>> {
    let text = |node: Node| source.get(node.byte_range()).unwrap_or_default();
    let is_identifier = |node: Node| {
        matches!(
            node.kind(),
            "identifier" | "type_identifier" | "field_identifier" | "namespace_identifier"
        )
    };
    let mut macros = Vec::new();
    let mut pending = vec![root];
    while let Some(node) = pending.pop() {
        let mut cursor = node.walk();
        pending.extend(node.children(&mut cursor));
        if node.kind() != "function_definition"
            || node
                .child_by_field_name("declarator")
                .is_none_or(|declarator| {
                    member_kind(declarator_name(declarator)) == SymbolKind::Method
                })
        {
            continue;
        }
        let Some(body) = node.child_by_field_name("body") else {
            continue;
        };
        let mut leaves = Vec::new();
        let mut walk = vec![node];
        while let Some(part) = walk.pop() {
            if part.start_byte() >= body.start_byte() || part.is_extra() && !part.is_error() {
                continue;
            }
            if part.child_count() == 0 {
                if !part.is_missing() {
                    leaves.push(part);
                }
                continue;
            }
            let mut cursor = part.walk();
            let children: Vec<_> = part.children(&mut cursor).collect();
            walk.extend(children.into_iter().rev());
        }
        let mut depth = 0usize;
        let mut outside = Vec::with_capacity(leaves.len());
        for leaf in &leaves {
            match leaf.kind() {
                "(" | "[" => depth += 1,
                ")" | "]" => depth = depth.saturating_sub(1),
                _ => {}
            }
            outside.push(depth == 0 && !matches!(leaf.kind(), ")" | "]"));
        }
        let head_end = (0..leaves.len())
            .find(|&at| outside[at] && leaves[at].kind() == ":")
            .unwrap_or(leaves.len());
        let Some(key) = (0..head_end)
            .rev()
            .find(|&at| outside[at] && matches!(text(leaves[at]), b"class" | b"struct" | b"union"))
        else {
            continue;
        };
        let mut name = head_end;
        if name > key + 1 && text(leaves[name - 1]) == b"final" {
            name -= 1;
        }
        if name > key + 1 && leaves[name - 1].kind() == ">" {
            let mut open = 0usize;
            while name > key + 1 {
                name -= 1;
                match leaves[name].kind() {
                    ">" => open += 1,
                    "<" => open -= 1,
                    _ => {}
                }
                if open == 0 {
                    break;
                }
            }
        }
        if name <= key + 1 || !is_identifier(leaves[name - 1]) {
            continue;
        }
        name -= 1;
        while name >= key + 3 && leaves[name - 1].kind() == "::" && is_identifier(leaves[name - 2])
        {
            name -= 2;
        }
        macros.extend(
            (0..name)
                .filter(|&at| at != key && outside[at] && is_identifier(leaves[at]))
                .map(|at| leaves[at].byte_range()),
        );
    }
    macros
}

/// Whether the grammar ended a C or C++ declaration by supplying the `;` it
/// is missing: the source goes on past where the grammar could read it.
fn ends_on_a_supplied_semicolon(node: Node) -> bool {
    matches!(
        node.kind(),
        "declaration" | "field_declaration" | "type_definition"
    ) && node
        .child(node.child_count().saturating_sub(1))
        .is_some_and(|last| last.kind() == ";" && last.is_missing())
}

/// Whether a C or C++ declaration invokes a macro the grammar cannot expand:
/// a declarator in parentheses right after the type is the macro's argument
/// list (`Array(T)`, `__attribute__((aligned(1)))`), and the name the
/// declaration declares follows it.
fn invokes_a_macro(node: Node) -> bool {
    node.child_by_field_name("declarator")
        .is_some_and(|declarator| declarator.kind() == "parenthesized_declarator")
}

/// The name a C or C++ declaration writes alone, with no declarator, which
/// the grammar reads as a type (`items;`).
fn lone_name(node: Node) -> Option<Node> {
    node.child_by_field_name("type").filter(|written| {
        written.kind() == "type_identifier" && node.child_by_field_name("declarator").is_none()
    })
}

/// The declaration a C or C++ declaration is the rest of: the one before it,
/// which invoked a macro and which the grammar ended by supplying its `;`, so
/// the source's one declaration reads as two (`Array(T)` and `items;`,
/// `typedef __attribute__((aligned(1)))` and `U16 unalign16;`). The rest of a
/// typedef always is, since a typedef names its type; after another
/// declaration, a name alone is, where a declaration complete on its own may
/// follow a macro that wrote its own `;`.
fn continued_macro(node: Node) -> Option<Node> {
    if !matches!(node.kind(), "declaration" | "field_declaration") {
        return None;
    }
    std::iter::successors(node.prev_sibling(), Node::prev_sibling)
        .find(|before| !before.is_extra())
        .filter(|head| {
            ends_on_a_supplied_semicolon(*head)
                && invokes_a_macro(*head)
                && (head.kind() == "type_definition" || lone_name(node).is_some())
        })
}

/// The byte after the `}` that closes the `{` at `open`, counted as one
/// configuration of the preprocessor reads the source: each conditional's
/// first branch, the others skipped. A program compiles in every
/// configuration, so each balances its own brackets, where reading every
/// branch at once counts a brace each of them opens. Comments and string and
/// character literals hold no brackets, and a directive's own text is no
/// part of the body. A `{` a directive writes (a `#define` body) belongs to
/// that directive and ends with it at the latest. None where nothing closes
/// the brace.
fn body_end(content: &str, open: usize) -> Option<usize> {
    let bytes = content.as_bytes();
    let starts_line =
        |at: usize| at == 0 || (bytes[at - 1] == b'\n' && (at < 2 || bytes[at - 2] != b'\\'));
    let directive_at = |at: usize| {
        let mut at = at;
        while at < bytes.len() && matches!(bytes[at], b' ' | b'\t') {
            at += 1;
        }
        (bytes.get(at) == Some(&b'#')).then_some(at + 1)
    };
    let logical_line_end = |from: usize| {
        let mut at = from;
        while at < bytes.len() {
            if bytes[at] == b'\n' && (at == 0 || bytes[at - 1] != b'\\') {
                return at;
            }
            at += 1;
        }
        at
    };
    // The identifier or number a quote follows: a raw string's prefix, or the
    // number a digit separator stands inside.
    let token_before = |at: usize| {
        let start = content[..at]
            .rfind(|c: char| !(c.is_ascii_alphanumeric() || c == '_'))
            .map_or(0, |before| before + 1);
        &content[start..at]
    };
    let mut line = open;
    while !starts_line(line) {
        line -= 1;
    }
    let directive_end = directive_at(line).map(|_| logical_line_end(open));
    let end = directive_end.unwrap_or(bytes.len());
    // One entry per open conditional: whether its taken branch has passed.
    let mut conditionals: Vec<bool> = Vec::new();
    let mut depth = 0usize;
    let mut at = open;
    while at < end {
        if directive_end.is_none()
            && starts_line(at)
            && let Some(name) = directive_at(at)
        {
            let word: String = content[name..]
                .trim_start_matches([' ', '\t'])
                .chars()
                .take_while(|c| c.is_ascii_alphabetic())
                .collect();
            match word.as_str() {
                "if" | "ifdef" | "ifndef" => conditionals.push(false),
                "elif" | "elifdef" | "elifndef" | "else" => {
                    if let Some(passed) = conditionals.last_mut() {
                        *passed = true;
                    }
                }
                "endif" => {
                    conditionals.pop();
                }
                _ => {}
            }
            at = logical_line_end(at) + 1;
            continue;
        }
        let skipped = conditionals.contains(&true);
        match bytes[at] {
            b'/' if bytes.get(at + 1) == Some(&b'/') => {
                at = logical_line_end(at);
                continue;
            }
            b'/' if bytes.get(at + 1) == Some(&b'*') => {
                at = content[at + 2..]
                    .find("*/")
                    .map_or(end, |close| at + 2 + close + 2);
                continue;
            }
            b'"' if matches!(token_before(at), "R" | "u8R" | "uR" | "UR" | "LR") => {
                let delimiter_end = content[at + 1..].find('(').map_or(end, |p| at + 1 + p);
                let closing = format!("){}\"", &content[at + 1..delimiter_end.min(end)]);
                at = content[delimiter_end..]
                    .find(&closing)
                    .map_or(end, |close| delimiter_end + close + closing.len());
                continue;
            }
            b'\'' if token_before(at).starts_with(|c: char| c.is_ascii_digit()) => {}
            quote @ (b'"' | b'\'') => {
                at += 1;
                while at < end && bytes[at] != quote && bytes[at] != b'\n' {
                    at += if bytes[at] == b'\\' { 2 } else { 1 };
                }
            }
            b'{' if !skipped => depth += 1,
            b'}' if !skipped => {
                depth -= 1;
                if depth == 0 {
                    return Some(at + 1);
                }
            }
            _ => {}
        }
        at += 1;
    }
    directive_end
}

/// The class key of a C or C++ declaration, as the source writes it, where
/// a macro read as the type strands it: set aside as an error before the
/// declarator (`QT_BEGIN_NAMESPACE` before `class QWidget;`), or read as the
/// declarator itself (`EXPORT struct stat st;` declares `struct`).
fn stranded_class_key<'a>(node: Node, content: &'a str) -> Option<(&'a str, bool)> {
    let class_key = |node: Node| {
        content
            .get(node.start_byte()..node.end_byte())
            .and_then(|text| text.split_whitespace().last())
            .filter(|word| matches!(*word, "class" | "struct" | "union" | "enum"))
    };
    let declarator = node.child_by_field_name("declarator")?;
    class_key(declarator_name(declarator))
        .map(|key| (key, true))
        .or_else(|| {
            std::iter::successors(declarator.prev_sibling(), Node::prev_sibling)
                .take_while(|before| before.is_error() || before.is_extra())
                .filter(|before| before.is_error())
                .find_map(class_key)
                .map(|key| (key, false))
        })
}

/// Whether the grammar read a C or C++ declaration's class key as its
/// declarator, which leaves the name it declares a guess.
fn misreads_a_class_key(node: Node, content: &str) -> bool {
    stranded_class_key(node, content).is_some_and(|(_, as_name)| as_name)
}

/// The kind of type a C or C++ declaration whose class key a macro set aside
/// declares: the declarator names that type (`QT_BEGIN_NAMESPACE` before
/// `class QWidget;`), not a variable or a function.
fn set_aside_class_kind(node: Node, content: &str) -> Option<SymbolKind> {
    match stranded_class_key(node, content)? {
        (_, true) => None,
        ("class", _) => Some(SymbolKind::Class),
        ("enum", _) => Some(SymbolKind::Enum),
        _ => Some(SymbolKind::Struct),
    }
}

/// The class or module whose namespace a Python assignment binds a name in.
/// A namespace holds one variable per name, so only the first assignment to
/// it declares it and the rest rebind it (`HAS_JSON = True` under `try:`,
/// `HAS_JSON = False` under `except`), as pyright lists one.
fn binding_scope(assignment: Node) -> Option<usize> {
    std::iter::successors(assignment.parent(), Node::parent)
        .find(|scope| matches!(scope.kind(), "class_definition" | "module"))
        .map(|scope| scope.id())
}

/// Whether a captured node declares anything: a Python assignment, or a C
/// or C++ declaration a preprocessor conditional holds, binds a local where
/// a function's scope holds it, however deep in the function's statements,
/// and declares where a class's, a namespace's or the file's does; a type
/// alias is a type, read wherever it is declared. A C or C++ declaration the
/// grammar ended by supplying its missing `;` was cut short by a macro it
/// cannot expand, so what it would name is a guess, unless it declares a
/// function: its parameters complete its declarator, and what the grammar
/// could not read after them is a macro written there (`int f(int x)
/// G_GNUC_PURE;`). One that invoked a macro names nothing itself, and its rest
/// declares ([`continued_macro`]); a name written alone declares nothing
/// otherwise (`PADDING;`), since no declaration is a name alone. A class, struct,
/// union or enum without a body declares only standing alone, a forward
/// declaration: as the type of another declaration it names a type that
/// declaration uses (`struct stat st`, `struct item *it`).
fn declares(node: Node, content: &str, language: Language) -> bool {
    if language == Language::Cpp
        && (ends_on_a_supplied_semicolon(node)
            && (invokes_a_macro(node)
                || !node
                    .child_by_field_name("declarator")
                    .is_some_and(|declarator| {
                        member_kind(declarator_name(declarator)) == SymbolKind::Method
                    }))
            || lone_name(node).is_some() && continued_macro(node).is_none()
            || misreads_a_class_key(node, content)
            || matches!(
                node.kind(),
                "class_specifier" | "struct_specifier" | "union_specifier" | "enum_specifier"
            ) && node.child_by_field_name("body").is_none()
                && node.parent().is_some_and(|user| {
                    user.child_by_field_name("type")
                        .is_some_and(|used| used.id() == node.id())
                        && (user.child_by_field_name("declarator").is_some()
                            || !matches!(user.kind(), "declaration" | "field_declaration"))
                }))
    {
        return false;
    }
    let (local, declaring): (&[&str], &[&str]) = match (language, node.kind()) {
        (Language::Python, "assignment") => (
            &["function_definition", "lambda"],
            &["class_definition", "module"],
        ),
        (Language::Cpp, "declaration" | "type_definition" | "alias_declaration") => (
            &["compound_statement"],
            &[
                "translation_unit",
                "declaration_list",
                "field_declaration_list",
            ],
        ),
        _ => return true,
    };
    let mut ancestor = node.parent();
    while let Some(scope) = ancestor {
        if local.contains(&scope.kind()) {
            return false;
        }
        if declaring.contains(&scope.kind()) {
            return true;
        }
        ancestor = scope.parent();
    }
    true
}

/// Where a file's types are declared outside their bodies
/// ([`SymbolExtractor::type_headers`]), in CLI positions.
#[derive(Debug, Default)]
pub struct TypeHeaders {
    /// The lines the headers are written on.
    pub lines: BTreeSet<u32>,
    spans: Vec<HeaderSpan>,
}

/// One type's header: from where its declaration opens to where its body
/// does, and where its name stands.
#[derive(Debug)]
struct HeaderSpan {
    name: (u32, u32),
    from: (u32, u32),
    to: (u32, u32),
}

impl TypeHeaders {
    /// Whether a name at `at` is written in the header of a type other than
    /// the one it names — a generic type's parameter — rather than declared
    /// in a body or on its own, as a type alias or an associated type is.
    pub fn holds_parameter(&self, at: (u32, u32)) -> bool {
        self.spans
            .iter()
            .any(|span| span.name != at && span.from <= at && at < span.to)
    }
}

/// The child of a type's node that holds its members, whether or not the
/// grammar reads any of them: the grammar's `body`, or where a grammar gives
/// it no field, a Go type's literal (`struct {…}`, `interface {…}`), a Kotlin
/// class's or object's body and an HCL block's.
fn type_body(node: Node, language: Language) -> Option<Node> {
    match language {
        Language::Go => node.child_by_field_name("type"),
        Language::Terraform => child_of_kind(node, "body"),
        Language::Kotlin => {
            let mut cursor = node.walk();
            node.named_children(&mut cursor)
                .find(|child| matches!(child.kind(), "class_body" | "enum_class_body"))
        }
        // `declare global { … }` holds its block under no field.
        Language::TypeScript if node.kind() == "ambient_declaration" => node
            .child_by_field_name("body")
            .or_else(|| child_of_kind(node, "statement_block")),
        _ => node.child_by_field_name("body"),
    }
}

/// The 1-indexed lines a node spans from `start` to its exclusive `end`: an
/// end at the start of a line leaves the node on the line before it.
fn line_span(start: tree_sitter::Point, end: tree_sitter::Point) -> std::ops::RangeInclusive<u32> {
    let last = if end.column == 0 && end.row > start.row {
        end.row - 1
    } else {
        end.row
    };
    start.row as u32 + 1..=last as u32 + 1
}

/// Node kinds that open a body of code without being a declaration the
/// grammars read as callable: closures, lambdas, function expressions,
/// accessors and initializer blocks. A kind is listed once for
/// every grammar that names a body by it; a kind some grammar uses for
/// anything else (`block`, a Python class body too) is not, and neither is a
/// Ruby block, whose `def` defines a method on the class that runs it.
const ANONYMOUS_BODIES: &[&str] = &[
    // TypeScript, JavaScript, PHP
    "arrow_function",
    "function_expression",
    "generator_function",
    "class_static_block",
    // PHP, Kotlin
    "anonymous_function",
    // Java, C++, C#
    "lambda_expression",
    "anonymous_method_expression",
    // Go, Rust
    "func_literal",
    "closure_expression",
    // Java, C#, Kotlin
    "static_initializer",
    "accessor_declaration",
    "anonymous_initializer",
    "lambda_literal",
    "getter",
    "setter",
];

/// A signature as [`read_signature`] found it: the text, or a reading the
/// grammar made only by recovering from a syntax error.
enum Read {
    AsWritten(String),
    Recovered,
}

/// The signature of the declaration named at byte `name` in the tree under
/// `root`, whose range runs from `start` to `end`.
///
/// The declaration is the outermost node over its name that ends within the
/// range, or past it only by a terminator (`;`, `,`) a server leaves out or
/// by the other declarators of a statement the range starts (clangd ranges
/// `int a = 1, b = 2;` for `a` as `int a = 1`). Its signature starts where
/// both it and the range have started, so an attribute or doc comment
/// outside the node stays out, and ends where the first body or value above
/// the name opens: a value is a variable's body, and an initializer can be
/// any size, so every value the statement writes before that stays out as
/// well ([`declared_text`]). The rest of a C or C++ declaration that invoked
/// a macro was read apart from where it begins ([`continued_macro`]), so it
/// is a recovered reading.
fn read_signature(
    root: Node,
    content: &str,
    language: Language,
    name: usize,
    start: usize,
    end: usize,
) -> Option<Read> {
    let first = name + content.get(name..)?.chars().next()?.len_utf8();
    let named = root.named_descendant_for_byte_range(name, first)?;
    let within = |node: Node| {
        node.end_byte() <= end.max(first)
            || content.get(end..node.end_byte()).is_some_and(|past| {
                let past = past.trim();
                !past.is_empty() && past.chars().all(|c| matches!(c, ';' | ','))
                    || node.start_byte() >= start && past.starts_with(',')
            })
    };
    let mut declaration = named;
    while let Some(parent) = declaration.parent()
        && within(parent)
    {
        declaration = parent;
    }
    if language == Language::Cpp && continued_macro(declaration).is_some() {
        return Some(Read::Recovered);
    }
    // A JS/TS statement declaring several names is the declaration of each, as
    // other grammars hold it, though its grammar holds each name apart: `B` in
    // `export const A = 1, B = 2` is declared by `export const A, B`.
    let mut start = start;
    if matches!(language, Language::TypeScript | Language::JavaScript)
        && let Some(statement) = std::iter::successors(Some(named), Node::parent)
            .take_while(|node| node.id() != declaration.id())
            .chain(std::iter::once(declaration))
            .find(|node| node.kind() == "variable_declarator")
            .and_then(|declarator| declarator.parent())
            .filter(|statement| {
                matches!(
                    statement.kind(),
                    "lexical_declaration" | "variable_declaration"
                )
            })
    {
        declaration = declaration_node(statement, language);
        start = start.min(declaration.start_byte());
    }
    let mut node = named;
    let opens = loop {
        if let Some(body) = body_of(node, language).filter(|body| body.start_byte() > name) {
            break header_end(body, language);
        }
        if let Some(value) = value_opening(node, name) {
            break value.opens;
        }
        if let Some(close) = empty_body_close(node).filter(|close| close.start_byte() > name) {
            break header_end(close, language);
        }
        if node.id() == declaration.id() {
            break declaration.end_byte();
        }
        node = node.parent()?;
    };
    let from = start.max(declaration.start_byte());
    if errs_within(declaration, from, opens) {
        return Some(Read::Recovered);
    }
    let text = declared_text(content, named, declaration, from, opens.max(from))?;
    let text = text.trim_end_matches(|c: char| c.is_whitespace() || matches!(c, ';' | ','));
    let text = text.trim_start();
    (!text.is_empty()).then(|| Read::AsWritten(text.to_string()))
}

/// Whether `node` holds a syntax error or a missing token between bytes
/// `from` and `to`.
fn errs_within(node: Node, from: usize, to: usize) -> bool {
    if !node.has_error() || node.end_byte() <= from || node.start_byte() >= to {
        return false;
    }
    if node.is_error() || node.is_missing() {
        return true;
    }
    let mut cursor = node.walk();
    node.children(&mut cursor)
        .any(|child| errs_within(child, from, to))
}

/// The node holding a declaration's body, if `node` declares one: its
/// `body`, where a grammar names a type's or a function's body otherwise
/// ([`type_body`]), a property's accessors where they hold a body (C#'s
/// `{ get; set; }` is the shape it declares), the expression it returns
/// (`int P => 1`), its observers or delegate, the token opening the rules of
/// a `macro_rules!` or the statements of a Kotlin secondary constructor, a
/// function value's body (`const f = () => {}`), or the body of the
/// declaration a wrapper holds (`export`, a decorated definition). A C++
/// constructor's body begins at its member initializers, which the standard
/// counts part of it (`F(int p) : a_(compute(p)) {}`). Only a function value
/// opens its body after a header the variable shares: a `match` or `switch`
/// value is a value like any other.
fn body_of(node: Node, language: Language) -> Option<Node> {
    let held_as = |kinds: &[&str]| kinds.iter().find_map(|kind| child_of_kind(node, kind));
    (language == Language::Cpp)
        .then(|| child_of_kind(node, "field_initializer_list"))
        .flatten()
        .or_else(|| node.child_by_field_name("body"))
        .or_else(|| match language {
            Language::Kotlin => held_as(&[
                "function_body",
                "getter",
                "setter",
                "property_delegate",
                "{",
            ])
            .or_else(|| type_body(node, language)),
            Language::Swift => held_as(&["computed_property", "willset_didset_block"])
                .or_else(|| type_body(node, language)),
            Language::CSharp => node
                .child_by_field_name("accessors")
                .filter(|accessors| {
                    let mut cursor = accessors.walk();
                    accessors
                        .named_children(&mut cursor)
                        .any(|accessor| accessor.child_by_field_name("body").is_some())
                })
                .or_else(|| {
                    node.child_by_field_name("value")
                        .filter(|value| value.kind() == "arrow_expression_clause")
                })
                .or_else(|| type_body(node, language)),
            Language::Go => {
                let literal = node.child_by_field_name("type")?;
                if !matches!(literal.kind(), "struct_type" | "interface_type") {
                    return None;
                }
                let mut cursor = literal.walk();
                literal
                    .children(&mut cursor)
                    .find(|child| matches!(child.kind(), "{" | "field_declaration_list"))
            }
            Language::Rust if node.kind() == "macro_definition" => {
                node.child_by_field_name("name")?.next_sibling()
            }
            Language::Dart if node.kind() == "local_function_declaration" => {
                child_of_kind(node, "function_body")
            }
            Language::Lua => lua_function_value(node)
                .and_then(|function| {
                    function
                        .child_by_field_name("body")
                        .or_else(|| child_of_kind(function, "end"))
                })
                .or_else(|| type_body(node, language)),
            _ => type_body(node, language),
        })
        .or_else(|| {
            ["value", "right"]
                .into_iter()
                .find_map(|field| node.child_by_field_name(field))
                .filter(|value| {
                    ["parameters", "parameter", "declarator"]
                        .into_iter()
                        .any(|header| value.child_by_field_name(header).is_some())
                })
                .and_then(|value| value.child_by_field_name("body"))
        })
        .or_else(|| {
            ["declaration", "definition"]
                .into_iter()
                .find_map(|field| node.child_by_field_name(field))
                .and_then(|held| body_of(held, language))
        })
}

/// The value `node` gives the name at byte `name`, if it gives one: the
/// first value ([`given_value`]) after the name and before the next
/// declarator, so `int a, b = 2` gives `a` none and a Go spec's names before
/// the last share its one.
fn value_opening(node: Node, name: usize) -> Option<Value> {
    let mut cursor = node.walk();
    node.children(&mut cursor)
        .filter(|child| child.start_byte() > name)
        .take_while(|child| child.kind() != ",")
        .find_map(given_value)
}

/// A declarator's value as a statement writes it.
struct Value {
    /// Where the value opens: its `=`, or where a value written without one
    /// starts.
    opens: usize,
    /// The bytes the value takes from the statement, from the end of what it
    /// follows to its own end.
    taken: std::ops::Range<usize>,
}

/// The declarator's value `node` opens, if it opens one: an `=`, Go's `:=` or
/// Ruby's `||=` and the value after it, or a value a grammar writes without
/// one — C++'s `int h{3}`, a JavaScript object's `key: value`, `export default
/// value`. A parenthesized C++ initializer is none, since the grammar reads a
/// function whose parameters it cannot parse (`void f(STACK_OF(T) *sk)`) as
/// one. A parameter's default (Swift lists it beside its parameter, and an
/// enum case's associated value beside its type), a destructuring pattern's
/// default (`const { c = 1 } = obj`), what an alias names and a C++
/// concept's constraint, which is what the concept is, are no declarator's
/// value, nor is what follows the `=` ending C++'s `operator=`.
fn given_value(node: Node) -> Option<Value> {
    let parent = node.parent()?;
    if ALIASES.contains(&parent.kind())
        || parent.kind() == "concept_definition"
        || is_pattern_default(parent)
    {
        return None;
    }
    let opening = |token: Node| !token.is_named() && matches!(token.kind(), "=" | ":=" | "||=");
    if opening(node) {
        let follows = std::iter::successors(node.prev_sibling(), Node::prev_sibling)
            .find(|before| !before.is_extra())?;
        if follows.kind() == "parameter" || parent.kind() == "enum_type_parameters" {
            return None;
        }
        let value = std::iter::successors(node.next_sibling(), Node::next_sibling)
            .find(|next| next.is_named() && !next.is_extra())?;
        let from = node
            .prev_sibling()
            .map_or(node.start_byte(), |before| before.end_byte());
        return Some(Value {
            opens: node.start_byte(),
            taken: from..value.end_byte(),
        });
    }
    let unwritten = match parent.kind() {
        "init_declarator" | "field_declaration" => node.kind() == "initializer_list",
        "pair" | "export_statement" => children_of_field(parent, "value").contains(&node),
        _ => false,
    };
    let before = node.prev_sibling()?;
    if !unwritten || opening(before) {
        return None;
    }
    let follows = std::iter::successors(Some(before), Node::prev_sibling)
        .find(|before| before.is_named() && !before.is_extra())
        .unwrap_or(before);
    Some(Value {
        opens: follows
            .next_sibling()
            .map_or(node.start_byte(), |next| next.start_byte()),
        taken: follows.end_byte()..node.end_byte(),
    })
}

/// The text `declaration` declares `named` with, from `from` to `to`:
/// without the values it gives its declarators — those beside it in a list
/// (`int c = 1, *p`), those written beside its name (Swift's `let a: Int = 1,
/// b: Int = 2`, Go's `var a, b int = 1, 2`), so `p` reads `int c, *p`, and a
/// property's own before the observers that end its header — and without the
/// body of a type it defines in passing (`typedef struct { … } Foo` reads
/// `typedef struct Foo`). A parameter's default is no declarator's value and
/// stays.
fn declared_text(
    content: &str,
    named: Node,
    declaration: Node,
    from: usize,
    to: usize,
) -> Option<String> {
    let name = named.start_byte();
    let mut given = Vec::new();
    let mut held = named;
    while held.id() != declaration.id()
        && let Some(holder) = held.parent()
    {
        let mut cursor = holder.walk();
        for sibling in holder.children(&mut cursor) {
            if sibling.id() == held.id() {
                continue;
            }
            let mut cursor = sibling.walk();
            given.extend(
                std::iter::once(sibling)
                    .chain(sibling.children(&mut cursor))
                    .filter_map(given_value)
                    .map(|value| (value.taken.start, value.taken.end)),
            );
        }
        given.extend(
            holder
                .child_by_field_name("type")
                .filter(|defined| {
                    matches!(
                        defined.kind(),
                        "class_specifier"
                            | "struct_specifier"
                            | "union_specifier"
                            | "enum_specifier"
                    )
                })
                .and_then(|defined| defined.child_by_field_name("body"))
                .map(|body| {
                    let before = body
                        .prev_sibling()
                        .map_or(body.start_byte(), |before| before.end_byte());
                    (before, body.end_byte())
                }),
        );
        held = holder;
    }
    given.retain(|&(start, end)| from <= start && end <= to && !(start..end).contains(&name));
    given.sort_unstable();
    let mut text = String::new();
    let mut at = from;
    for (start, end) in given {
        if start >= at {
            text.push_str(content.get(at..start)?);
            at = end;
        }
    }
    text.push_str(content.get(at..to)?);
    Some(text)
}

/// Whether `node` gives a name a destructuring pattern binds its default.
fn is_pattern_default(node: Node) -> bool {
    matches!(
        node.kind(),
        "assignment_pattern" | "object_assignment_pattern"
    )
}

/// Declarations that name what they declare by what follows their `=` — a
/// type, or a C++ namespace — or by nothing where they only declare it (a
/// trait's associated type).
const ALIASES: &[&str] = &[
    // Rust
    "type_item",
    "associated_type",
    // TypeScript
    "type_alias_declaration",
    // Go, Kotlin, Dart
    "type_alias",
    // Python
    "type_alias_statement",
    // Swift
    "typealias_declaration",
    "associatedtype_declaration",
    // C++
    "alias_declaration",
    // Scala
    "type_definition",
    // C++
    "namespace_alias_definition",
];

/// Where each line of a file starts, to turn a 1-indexed line and
/// Unicode-scalar column into the byte its syntax tree is addressed by.
struct LineStarts<'a> {
    content: &'a str,
    starts: Vec<usize>,
}

impl<'a> LineStarts<'a> {
    fn new(content: &'a str) -> Self {
        let starts = std::iter::once(0)
            .chain(content.match_indices('\n').map(|(newline, _)| newline + 1))
            .collect();
        Self { content, starts }
    }

    /// The byte at `column` of `line`, or the end of the line one past its
    /// last character; `None` past that.
    fn byte(&self, line: u32, column: u32) -> Option<usize> {
        let start = *self.starts.get(line.checked_sub(1)? as usize)?;
        let rest = &self.content[start..];
        let text = &rest[..rest.find('\n').unwrap_or(rest.len())];
        text.char_indices()
            .map(|(byte, _)| byte)
            .chain([text.len()])
            .nth(column.checked_sub(1)? as usize)
            .map(|byte| start + byte)
    }
}

/// Whether a declaration belongs to a body rather than standing for itself:
/// a function or method encloses it, or an [`ANONYMOUS_BODIES`] body does that
/// a declaration in turn encloses (a const's arrow function, a class's `init`
/// block). A body no declaration encloses (a callback passed at the top of a
/// file, a function called where it is written) absorbs nothing, since
/// nothing would stand for what it holds. Callable nodes reached from the
/// declaration through callable nodes alone are the declaration itself —
/// Dart's query reads a method's signature, which its method signature and
/// declaration wrap — while a body holds a declaration in a block of
/// statements. A callable's parameters are its header rather than its body,
/// so a property a TypeScript constructor parameter declares is its class's.
fn is_local(node: Node, declared: &std::collections::HashSet<usize>) -> bool {
    is_local_within(node, None, declared)
}

/// Whether a declaration belongs to a body below the declaration `scope`
/// holding it, as [`is_local`] judges one: a method of a class a function
/// declares belongs to its class, and only what a body inside the class
/// holds is local to it.
fn is_local_below(node: Node, scope: Node, declared: &std::collections::HashSet<usize>) -> bool {
    is_local_within(node, Some(scope), declared)
}

fn is_local_within(
    node: Node,
    scope: Option<Node>,
    declared: &std::collections::HashSet<usize>,
) -> bool {
    let mut own = true;
    let mut in_body = false;
    let mut inner = node;
    for outer in std::iter::successors(node.parent(), Node::parent)
        .take_while(|outer| scope.is_none_or(|scope| scope.id() != outer.id()))
    {
        if node_kind(outer).is_callable() {
            if !own && !in_header_of(inner, outer) {
                return true;
            }
        } else {
            own = false;
            if ANONYMOUS_BODIES.contains(&outer.kind()) {
                in_body = true;
            } else if in_body && declared.contains(&outer.id()) {
                return true;
            }
        }
        inner = outer;
    }
    false
}

/// Whether a Rust function's parameters take `self`, as `&self` or with its
/// type written out (`self: Pin<&mut Self>`), which makes it a method.
fn takes_self(parameters: Node) -> bool {
    let mut cursor = parameters.walk();
    parameters.named_children(&mut cursor).any(|parameter| {
        parameter.kind() == "self_parameter"
            || parameter
                .child_by_field_name("pattern")
                .is_some_and(|pattern| pattern.kind() == "self")
    })
}

fn register(
    languages: &mut HashMap<Language, LanguageEntry>,
    language: Language,
    ts_language: tree_sitter::Language,
    query_src: &str,
) {
    // The grammar and its query are compiled into the binary, so registration
    // is deterministic for a given build. A failure means a defective grammar
    // (an incompatible ABI or a query whose node types moved) — skip that one
    // language so an unrelated language never loses indexing over it. The
    // registration- and extraction-completeness tests fail loudly when a
    // language is missing or under-extracted, so a defective grammar surfaces as
    // a targeted test failure rather than silent index loss.
    let mut parser = Parser::new();
    if parser.set_language(&ts_language).is_err() {
        return;
    }
    let Ok(query) = Query::new(&ts_language, query_src) else {
        return;
    };
    let Some(declaration) = query.capture_index_for_name("symbol") else {
        return;
    };
    languages.insert(
        language,
        LanguageEntry {
            parser: Mutex::new(parser),
            query,
            declaration,
        },
    );
}

/// The symbols the declaration `node` declares: its one name, or each name a
/// statement declaring several states ([`declared_names`]) once, however
/// often it binds it (`lo, hi = hi, top = …`), all over the statement's one
/// range — or each over its name alone where the statement also assigns to
/// something else ([`assigns_beyond_its_names`]).
fn extract_declaration(
    node: Node,
    declared: &HashSet<usize>,
    path: &Path,
    content: &str,
    language: Language,
) -> Vec<Symbol> {
    let names = declared_names(node, language);
    let named: Vec<(String, SymbolKind, Node)> = if names.len() > 1 {
        let mut stated = HashSet::new();
        names
            .into_iter()
            .filter_map(|name| {
                let text = content.get(name.start_byte()..name_end_node(name).end_byte())?;
                let kind = match language {
                    Language::Cpp if matches!(node.kind(), "field_declaration" | "declaration") => {
                        declared_as(node, name)
                    }
                    _ => node_kind(node),
                };
                (!names_nothing(text, language) && stated.insert(text))
                    .then(|| (text.to_string(), kind, name))
            })
            .collect()
    } else {
        extract_name_and_kind(node, content, language)
            .map(|(name, kind)| {
                (
                    name,
                    kind,
                    name_position_node(node, language).unwrap_or(node),
                )
            })
            .into_iter()
            .collect()
    };
    if named.is_empty() {
        return Vec::new();
    }
    let container = extract_container_path(node, declared, content, language);

    // Anchor the symbol at its NAME, not the item start: `refs`/`def` on a
    // leading keyword (`pub`, `fn`, an attribute line) resolve to the wrong
    // symbol or nothing, and a name-span position also lets the index and the
    // LSP workspace pass dedup to a single row (both then point at the same
    // identifier). The declaration node supplies the surrounding range, so a
    // body is sliced from the same two fields a document-symbol answer fills.
    let declaration = declaration_node(node, language);
    let statement = (
        declaration_start(declaration, language),
        declaration_end(declaration, language),
    );
    let own_name = assigns_beyond_its_names(node, language);

    named
        .into_iter()
        .map(|(name, kind, anchor)| {
            let (line, column) =
                scalar_position(content, anchor.start_byte(), anchor.start_position());
            let name_end = name_end_node(anchor);
            let (name_end_line, name_end_column) =
                scalar_position(content, name_end.end_byte(), name_end.end_position());
            let (start, end) = if own_name {
                (anchor, anchor)
            } else {
                statement
            };
            let (range_start_line, range_start_column) =
                scalar_position(content, start.start_byte(), start.start_position());
            let (end_line, end_column) =
                scalar_position(content, end.end_byte(), end.end_position());
            let location = Location::full(
                path.to_path_buf(),
                line,
                column,
                range_start_line,
                range_start_column,
                end_line,
                end_column,
            )
            .with_name_end(name_end_line, name_end_column);
            let name_path = Some(match &container {
                Some(container) => format!("{container}/{name}"),
                None => name.clone(),
            });
            let kind = placed_kind(kind, node, &name, declared, content, language);
            let mut symbol = Symbol::new(name, kind, location);
            symbol.name_path = name_path;
            symbol.container = container.clone();
            symbol
        })
        .collect()
}

fn declaration_visibility(
    node: Node,
    symbol: &Symbol,
    content: &str,
    language: Language,
    declared: &HashSet<usize>,
) -> Option<bool> {
    let modifier = |keyword| {
        let mut cursor = node.walk();
        node.named_children(&mut cursor)
            .filter(|child| {
                matches!(
                    child.kind(),
                    "modifiers" | "modifier" | "visibility_modifier"
                )
            })
            .any(|modifiers| syntax_keyword(modifiers, keyword))
    };
    let in_interface = || {
        node.parent()
            .and_then(|body| body.parent())
            .is_some_and(|parent| {
                matches!(
                    parent.kind(),
                    "interface_declaration" | "annotation_type_declaration"
                )
            })
    };
    match language {
        Language::Rust => {
            if matches!(
                node.kind(),
                "impl_item" | "macro_definition" | "enum_variant" | "function_signature_item"
            ) {
                return None;
            }
            // A trait's items, a trait impl's and a variant's fields carry no
            // visibility of their own.
            if let Some(parent) = node.parent().and_then(|body| body.parent())
                && (matches!(parent.kind(), "trait_item" | "enum_variant")
                    || (parent.kind() == "impl_item"
                        && parent.child_by_field_name("trait").is_some()))
            {
                return None;
            }
            Some(
                child_of_kind(node, "visibility_modifier").is_some_and(|visibility| {
                    syntax_keyword(visibility, "pub") && !syntax_keyword(visibility, "(")
                }),
            )
        }
        Language::Go => {
            if node.kind() != "field_declaration" && is_local(node, declared) {
                return Some(false);
            }
            let name = declared_names(node, language)
                .into_iter()
                .find(|name| {
                    content.get(name.start_byte()..name.end_byte()) == Some(symbol.name.as_str())
                })
                .or_else(|| find_name_node(node, language))?;
            static UPPERCASE_NAME: std::sync::LazyLock<Query> = std::sync::LazyLock::new(|| {
                Query::new(
                    &tree_sitter_go::LANGUAGE.into(),
                    r#"((_) @name (#match? @name "^\\p{Lu}"))"#,
                )
                .expect("Go uppercase-name query is valid")
            });
            Some(
                QueryCursor::new()
                    .matches(&UPPERCASE_NAME, name, content.as_bytes())
                    .next()
                    .is_some(),
            )
        }
        Language::Java | Language::CSharp => {
            if matches!(
                node.kind(),
                "package_declaration"
                    | "namespace_declaration"
                    | "file_scoped_namespace_declaration"
            ) {
                return None;
            }
            // Java declares an enum's constants `public static final`, and C#
            // lets an enum member carry no modifier: both are the enum's API.
            if matches!(node.kind(), "enum_constant" | "enum_member_declaration") {
                return Some(true);
            }
            // A C# record's component is a public property; a Java record's is
            // a private field read through a public accessor.
            if is_record_component(node) {
                return (language == Language::CSharp).then_some(true);
            }
            if modifier("public") {
                return Some(true);
            }
            if ["private", "protected", "internal"]
                .into_iter()
                .any(modifier)
            {
                return Some(false);
            }
            if in_interface() {
                return if language == Language::Java
                    || (node.kind() == "method_declaration"
                        && node.child_by_field_name("body").is_none())
                {
                    Some(true)
                } else {
                    None
                };
            }
            Some(false)
        }
        Language::Kotlin => {
            if is_local(node, declared) {
                return Some(false);
            }
            if modifier("public") {
                Some(true)
            } else if ["private", "protected", "internal"]
                .into_iter()
                .any(modifier)
            {
                Some(false)
            } else if modifier("override") {
                None
            } else {
                Some(true)
            }
        }
        Language::TypeScript | Language::JavaScript => {
            let mut declaration = declaration_node(node, language);
            while let Some(parent) = declaration.parent() {
                if matches!(
                    parent.kind(),
                    "lexical_declaration" | "variable_declaration" | "ambient_declaration"
                ) {
                    declaration = parent;
                } else {
                    break;
                }
            }
            if declaration.kind() == "export_statement"
                || declaration
                    .parent()
                    .is_some_and(|parent| parent.kind() == "export_statement")
            {
                Some(true)
            } else if matches!(
                node.kind(),
                "method_definition"
                    | "method_signature"
                    | "property_signature"
                    | "public_field_definition"
                    | "field_definition"
                    | "abstract_method_signature"
                    | "required_parameter"
                    | "optional_parameter"
                    | "enum_assignment"
                    | "property_identifier"
            ) {
                Some(false)
            } else {
                None
            }
        }
        _ => None,
    }
}

fn syntax_keyword(node: Node, keyword: &str) -> bool {
    if node.kind() == keyword {
        return true;
    }
    let mut cursor = node.walk();
    node.children(&mut cursor)
        .any(|child| syntax_keyword(child, keyword))
}

/// The node that spans the whole of a declaration: the one the grammar
/// gives it, or a wrapper that holds it with more of its syntax — a TS/JS
/// `export` statement (decorators on it included), a TypeScript `declare`, a
/// `const`, `let` or `var` statement declaring it alone, a Python
/// `decorated_definition`, a C++ `template` header or `extern "C"`, a Go
/// `type`, `const` or `var` declaring it alone outside a parenthesized
/// group, and a Dart function, method or class member, which holds its
/// annotations, modifiers and body beside the signature the query reads.
fn declaration_node(mut node: Node, language: Language) -> Node {
    while let Some(parent) = node.parent() {
        let holds = |field| {
            parent
                .child_by_field_name(field)
                .is_some_and(|held| held.id() == node.id())
        };
        let alone = || {
            let mut cursor = parent.walk();
            parent
                .named_children(&mut cursor)
                .filter(|child| !child.is_extra())
                .count()
                == 1
        };
        let wraps = match (language, parent.kind()) {
            (Language::TypeScript | Language::JavaScript, "export_statement") => {
                holds("declaration")
            }
            (
                Language::TypeScript | Language::JavaScript,
                "lexical_declaration" | "variable_declaration",
            ) => alone(),
            (Language::TypeScript, "ambient_declaration") => true,
            (Language::Python, "decorated_definition") => holds("definition"),
            (Language::Cpp, "template_declaration") => true,
            (Language::Cpp, "linkage_specification") => holds("body"),
            (Language::Go, "type_declaration" | "const_declaration" | "var_declaration") => {
                alone() && child_of_kind(parent, "(").is_none()
            }
            (
                Language::Dart,
                "function_declaration"
                | "method_declaration"
                | "getter_declaration"
                | "setter_declaration"
                | "external_function_declaration"
                | "external_getter_declaration"
                | "external_setter_declaration",
            ) => holds("signature"),
            (
                Language::Dart,
                "method_signature" | "class_member" | "declaration" | "local_function_declaration",
            ) => true,
            _ => false,
        };
        if !wraps {
            break;
        }
        node = parent;
    }
    node
}

/// The names `node` declares, in the order they are written, where its
/// grammar can state several in one node: `var a, b = 1, 2`, `int a, b;`,
/// `a = b = None`, `public $a, $b;`, Swift's `let a = 1, b = 2`. The first
/// is the name [`find_name_node`] reads. A statement that declares several
/// names, and assigns to nothing else, is the whole declaration of each, so
/// [`extract_declaration`] reads each as a symbol of its own over that one
/// range. Listed per language, as each grammar holds the names differently.
fn declared_names(node: Node, language: Language) -> Vec<Node> {
    // A name is a named node; a field can also hold the punctuation between
    // names, as Go's `const A, B = 1, 2` holds its comma.
    let fields = |field| {
        let mut cursor = node.walk();
        node.children_by_field_name(field, &mut cursor)
            .filter(Node::is_named)
            .collect::<Vec<_>>()
    };
    match language {
        Language::Go => fields("name"),
        // A Swift function's return type carries the `name` field too, so
        // only a property declaration or an enum case states several names
        // in it (`case up, down`). A property names a pattern, which binds
        // its identifier after any `var` it is written with.
        Language::Swift
            if matches!(
                node.kind(),
                "property_declaration" | "protocol_property_declaration" | "enum_entry"
            ) =>
        {
            fields("name")
                .into_iter()
                .flat_map(swift_bound_names)
                .collect()
        }
        Language::Java => fields("declarator")
            .into_iter()
            .filter_map(|declarator| declarator.child_by_field_name("name"))
            .collect(),
        Language::Cpp => fields("declarator")
            .into_iter()
            .map(declarator_name)
            .chain(
                node.child_by_field_name("name")
                    .filter(|name| name.kind() == "nested_namespace_specifier"),
            )
            .flat_map(cpp_bound_names)
            .collect(),
        Language::CSharp => child_of_kind(node, "variable_declaration")
            .map(|list| children_of_kind(list, "variable_declarator"))
            .unwrap_or_default()
            .into_iter()
            .filter_map(|declarator| {
                declarator
                    .child_by_field_name("name")
                    .or_else(|| child_of_kind(declarator, "identifier"))
            })
            .collect(),
        Language::TypeScript | Language::JavaScript => node
            .child_by_field_name("name")
            .filter(|name| matches!(name.kind(), "object_pattern" | "array_pattern"))
            .map(bound_names)
            .unwrap_or_default(),
        Language::Scala => node
            .child_by_field_name("pattern")
            .filter(|pattern| pattern.kind() != "identifier")
            .map(bound_names)
            .unwrap_or_default(),
        Language::Python | Language::Ruby => assignment_targets(node)
            .filter(|target| binds_a_name(*target, language))
            .collect(),
        Language::PHP => children_of_kind(node, "property_element")
            .into_iter()
            .filter_map(|element| {
                child_of_kind(element, "variable_name").and_then(|v| child_of_kind(v, "name"))
            })
            .chain(
                children_of_kind(node, "const_element")
                    .into_iter()
                    .filter_map(|element| child_of_kind(element, "name")),
            )
            .collect(),
        Language::Dart => [
            ("initialized_identifier_list", "initialized_identifier"),
            ("static_final_declaration_list", "static_final_declaration"),
        ]
        .into_iter()
        .filter_map(|(list, item)| Some(children_of_kind(child_of_kind(node, list)?, item)))
        .flatten()
        .filter_map(|bound| child_of_kind(bound, "identifier"))
        .chain(
            child_of_kind(node, "identifier_list")
                .map(|list| children_of_kind(list, "identifier"))
                .unwrap_or_default(),
        )
        .collect(),
        Language::Lua => {
            let statement = child_of_kind(node, "assignment_statement").unwrap_or(node);
            let held = |list, field| {
                child_of_kind(statement, list)
                    .map_or_else(Vec::new, |list| children_of_field(list, field))
            };
            let names = held("variable_list", "name");
            if node.kind() != "assignment_statement" {
                return names;
            }
            names
                .into_iter()
                .zip(held("expression_list", "value"))
                .filter(|(name, value)| {
                    name.kind() == "dot_index_expression" && value.kind() == "function_definition"
                })
                .map(|(name, _)| name)
                .collect()
        }
        _ => Vec::new(),
    }
}

/// The function a Lua declaration or assignment gives its one name (`local
/// f = function() end`, `M.f = function() end`), which declares that name as
/// the function statement would.
fn lua_function_value(node: Node) -> Option<Node> {
    let statement = child_of_kind(node, "assignment_statement").unwrap_or(node);
    let [_] = children_of_field(child_of_kind(statement, "variable_list")?, "name")[..] else {
        return None;
    };
    let [value] = children_of_field(child_of_kind(statement, "expression_list")?, "value")[..]
    else {
        return None;
    };
    (value.kind() == "function_definition").then_some(value)
}

/// The name of the type a Go struct embeds, which names the field: the type
/// itself, the type a package qualifies, or the type a generic instantiates.
fn embedded_type_name(embedded: Node) -> Option<Node> {
    match embedded.kind() {
        "type_identifier" => Some(embedded),
        "qualified_type" => embedded.child_by_field_name("name"),
        "generic_type" => embedded
            .child_by_field_name("type")
            .and_then(embedded_type_name),
        _ => None,
    }
}

/// The names a Swift `name` binds: an enum case's own, a pattern's bound
/// identifier (`let x`), or each a tuple pattern binds (`let (x, (y, _))`).
fn swift_bound_names(name: Node) -> Vec<Node> {
    if name.kind() != "pattern" {
        return vec![name];
    }
    if let Some(bound) = name.child_by_field_name("bound_identifier") {
        return vec![bound];
    }
    let mut cursor = name.walk();
    let held: Vec<Node> = name.named_children(&mut cursor).collect();
    if held.iter().any(|child| child.kind() == "pattern") {
        return held
            .into_iter()
            .filter(|child| child.kind() == "pattern")
            .flat_map(swift_bound_names)
            .collect();
    }
    held.into_iter()
        .filter(|child| child.kind() == "simple_identifier")
        .collect()
}

/// The names a JavaScript, TypeScript or Scala destructuring pattern binds:
/// `{ a, b: renamed, c = 1, ...rest }` binds `a`, `renamed`, `c` and `rest`,
/// `[x, [y]]` binds `x` and `y`, Scala's `val a, b` and `val (c, d: Int)`
/// each name they list, and `val c @ Some(x)`, `val head :: tail` the names
/// their patterns hold. A key it reads from, a type it matches and an
/// operator are no binding, and `_` binds nothing.
fn bound_names(pattern: Node) -> Vec<Node> {
    let held = |field| pattern.child_by_field_name(field).map(bound_names);
    match pattern.kind() {
        "identifier" | "shorthand_property_identifier_pattern" => vec![pattern],
        "pair_pattern" => held("value").unwrap_or_default(),
        "assignment_pattern" | "object_assignment_pattern" => held("left").unwrap_or_default(),
        "typed_pattern" => held("pattern").unwrap_or_default(),
        "infix_pattern" => [held("left"), held("right")]
            .into_iter()
            .flatten()
            .flatten()
            .collect(),
        "case_class_pattern" | "capture_pattern" => {
            let mut cursor = pattern.walk();
            pattern
                .children_by_field_name("pattern", &mut cursor)
                .chain(pattern.child_by_field_name("name"))
                .flat_map(bound_names)
                .collect()
        }
        "object_pattern" | "array_pattern" | "rest_pattern" | "identifiers" | "tuple_pattern" => {
            let mut cursor = pattern.walk();
            pattern
                .named_children(&mut cursor)
                .flat_map(bound_names)
                .collect()
        }
        _ => Vec::new(),
    }
}

/// What a Python or Ruby assignment assigns to, through a chain (`a = b =
/// None`), in the order it is written.
fn assignment_targets(node: Node) -> impl Iterator<Item = Node> {
    std::iter::successors(Some(node), |assignment| {
        assignment
            .child_by_field_name("right")
            .filter(|right| right.kind() == "assignment")
    })
    .filter_map(|assignment| assignment.child_by_field_name("left"))
    .flat_map(destructured)
}

/// The targets one assignment target lists: itself, or each target a
/// destructuring bind holds (`a, b = …`, `(a, [b, c]) = …`, `a, *rest = …`,
/// Ruby's `a, *b = …`).
fn destructured(target: Node) -> Vec<Node> {
    match target.kind() {
        "pattern_list"
        | "tuple_pattern"
        | "list_pattern"
        | "list_splat_pattern"
        | "left_assignment_list"
        | "destructured_left_assignment"
        | "rest_assignment" => {
            let mut cursor = target.walk();
            target
                .named_children(&mut cursor)
                .flat_map(destructured)
                .collect()
        }
        _ => vec![target],
    }
}

/// Whether an assignment's target is a name it declares, rather than an
/// attribute, a subscript or a Ruby instance variable it assigns to: an
/// instance variable belongs to whichever object runs the assignment, while
/// a class variable belongs to the class whose body binds it.
fn binds_a_name(target: Node, language: Language) -> bool {
    match language {
        Language::Python => target.kind() == "identifier",
        Language::Ruby => matches!(target.kind(), "constant" | "identifier" | "class_variable"),
        _ => false,
    }
}

/// Whether `node` assigns to something besides the names it declares: a
/// chained assignment through an attribute, a subscript or an instance
/// variable (`LIMIT = obj.attr = 5`, `@cache = TABLE = {}`). Such a statement
/// is no declaration of its names alone, and nothing in the answer stands for
/// the rest of it, so each name's range is the name itself: a whole-line edit
/// of it is refused rather than taking the other assignment with it.
fn assigns_beyond_its_names(node: Node, language: Language) -> bool {
    matches!(language, Language::Python | Language::Ruby)
        && node.kind() == "assignment"
        && assignment_targets(node).any(|target| !binds_a_name(target, language))
}

/// Where a declaration's text begins: its node ([`declaration_node`]), or
/// the lines before it among its siblings that belong to it — a TypeScript
/// class member's decorators, a Rust item's outer attributes and doc comments
/// (`///` is `#[doc]`). A comment between those lines is inside the
/// declaration; one above them is not. The rest of a C or C++ declaration that
/// invoked a macro begins where that declaration does ([`continued_macro`]).
fn declaration_start(node: Node, language: Language) -> Node {
    if language == Language::Cpp
        && let Some(head) = continued_macro(node)
    {
        return head;
    }
    let mut start = node;
    let mut before = node.prev_sibling();
    while let Some(sibling) = before {
        let leads = match (language, sibling.kind()) {
            (Language::TypeScript | Language::JavaScript, "decorator") => true,
            (Language::Rust, "attribute_item") => true,
            (Language::Rust, "line_comment" | "block_comment") => {
                sibling.child_by_field_name("outer").is_some()
            }
            _ => false,
        };
        if leads {
            start = sibling;
        } else if !sibling.is_extra() {
            break;
        }
        before = sibling.prev_sibling();
    }
    start
}

/// Which ends of `node`'s declaration, from [`declaration_start`] to
/// [`declaration_end`], the grammar read without recovering from a syntax
/// error. A recovered reading is not one of the text as written: a macro the
/// grammar cannot expand (`QT_BEGIN_NAMESPACE` on the line before a function)
/// reads as the function's return type, and a class missing its `;` as the
/// return type of the function after it. The start rests on the syntax
/// before the declaration's body and the end on all of it, so an error inside
/// a body (a macro statement with no `;`) leaves the start as written and the
/// end not. A macro that happens to read as valid syntax of another shape
/// leaves no error to tell by.
fn read_as_written(node: Node, language: Language) -> AsWritten {
    let declaration = declaration_node(node, language);
    let end = declaration_end(declaration, language);
    let parts: Vec<Node> =
        std::iter::successors(Some(declaration_start(declaration, language)), |part| {
            (part.id() != end.id())
                .then(|| part.next_sibling())
                .flatten()
        })
        .collect();
    let body = node
        .child_by_field_name("body")
        .map_or(usize::MAX, |body| body.start_byte());
    AsWritten {
        start: !parts.iter().any(|part| errs_before(*part, body)),
        end: !parts.iter().any(|part| part.has_error()),
    }
}

/// Which ends of a declaration's extent [`read_as_written`] finds read as
/// written.
struct AsWritten {
    start: bool,
    end: bool,
}

/// Whether `node` holds a syntax error that begins before byte `limit`.
fn errs_before(node: Node, limit: usize) -> bool {
    if node.start_byte() >= limit || !node.has_error() {
        return false;
    }
    if node.is_error() || node.is_missing() {
        return true;
    }
    let mut cursor = node.walk();
    node.children(&mut cursor)
        .any(|child| errs_before(child, limit))
}

/// Where a declaration's text ends: its node ([`declaration_node`]), or the
/// token right after it that ends it — a C++ type's `;`, a TypeScript or
/// JavaScript member's `;` or `,`, a Rust field's or variant's `,`.
fn declaration_end(node: Node, language: Language) -> Node {
    let Some(next) = node.next_sibling() else {
        return node;
    };
    let ends = match (language, next.kind()) {
        (Language::Cpp, ";") => matches!(
            node.kind(),
            "class_specifier" | "struct_specifier" | "union_specifier" | "enum_specifier"
        ),
        (Language::TypeScript | Language::JavaScript, ";" | ",") => {
            node.parent().is_some_and(|body| {
                matches!(body.kind(), "class_body" | "interface_body" | "object_type")
            })
        }
        (Language::Rust, ",") => true,
        _ => false,
    };
    if ends { next } else { node }
}

/// A tree-sitter position as CLI/JSON positions are spelled: 1-indexed line,
/// 1-indexed Unicode-scalar column. tree-sitter reports the column as a byte
/// offset within the line, so multibyte text before a symbol would otherwise
/// misplace every follow-up `file:line:col`.
fn scalar_position(content: &str, byte: usize, position: tree_sitter::Point) -> (u32, u32) {
    let line_start = byte.saturating_sub(position.column);
    let column = content
        .get(line_start..byte)
        .map(|prefix| prefix.chars().count() as u32)
        .unwrap_or(position.column as u32)
        + 1;
    (position.row as u32 + 1, column)
}

/// The node whose start position the symbol should be addressed by — its name
/// identifier, so `file:line:col` lands on the thing `refs`/`def` resolve. An
/// impl is anchored at its self type (it has no name identifier of its own).
fn name_position_node<'a>(node: Node<'a>, language: Language) -> Option<Node<'a>> {
    if node.kind() == "impl_item" {
        return node.child_by_field_name("type");
    }
    find_name_node(node, language)
}

fn extract_container_path(
    node: Node,
    declared: &HashSet<usize>,
    content: &str,
    language: Language,
) -> Option<String> {
    // HCL qualifies a name with the type label written before it
    // (`google_storage_bucket.logs`), which is what tells two resources named
    // `this` apart. Nothing encloses a top-level block to walk up to.
    if language == Language::Terraform {
        return hcl_type_label(node, content);
    }

    // A symbol is keyed by its IMMEDIATE container only — the nearest enclosing
    // declaration — so a method reads `Type/method`, an enclosing outer type or
    // module never widens it to `Outer/Inner/method`, and a module-level item
    // stays bare. This matches what the LSP workspace surface can report (a
    // method's container is its nearest type; outer types and namespaces are
    // flattened away there), so a name_path round-trips across the index,
    // documentSymbol, and workspace surfaces. Modules/namespaces/packages
    // qualify nothing and are skipped on the way up to that nearest type.
    //
    // Only a declaration the query reads contains: a statement, a body or an
    // expression holds names that declare nothing (the target of a `for`, an
    // enum's bare members), and a declaration contains nothing its header
    // declares: a TypeScript constructor's parameter property is its class's,
    // and the members of an unnamed C++ enum a field is typed with are what
    // encloses the field's.
    std::iter::successors(enclosing_declaration(node, declared, language), |outer| {
        enclosing_declaration(*outer, declared, language)
    })
    .find_map(|outer| {
        extract_name_and_kind(outer, content, language)
            .filter(|(name, kind)| !name.is_empty() && !kind.is_namespace_like())
            .map(|(name, _)| name)
    })
}

/// The nearest declaration the query read that encloses `node`, other than
/// through its header ([`in_header_of`]). A Dart function holds its body
/// beside the signature the query reads ([`declaration_node`]), so what the
/// body declares is enclosed by that signature ([`signature_beside`]).
fn enclosing_declaration<'t>(
    node: Node<'t>,
    declared: &HashSet<usize>,
    language: Language,
) -> Option<Node<'t>> {
    let mut inner = node;
    let mut ancestor = node.parent();
    while let Some(outer) = ancestor {
        if declared.contains(&outer.id()) && !in_header_of(inner, outer) {
            return Some(outer);
        }
        if language == Language::Dart
            && let Some(signature) = signature_beside(inner, declared)
        {
            return Some(signature);
        }
        inner = outer;
        ancestor = outer.parent();
    }
    None
}

/// The signature the query read for the Dart function whose body is `body`:
/// the one written before it, or the one a method's signature holds.
fn signature_beside<'t>(body: Node<'t>, declared: &HashSet<usize>) -> Option<Node<'t>> {
    if body.kind() != "function_body" {
        return None;
    }
    let written = std::iter::successors(body.prev_named_sibling(), Node::prev_named_sibling)
        .find(|before| !before.is_extra())?;
    std::iter::successors(Some(written), |signature| {
        (signature.kind() == "method_signature")
            .then(|| signature.named_child(0))
            .flatten()
    })
    .find(|signature| declared.contains(&signature.id()))
}

/// The kind a declaration's place settles beyond its own node: a function a
/// type declares as a member is a method, as language servers read one,
/// wherever the type itself is declared, except a Rust associated function,
/// which is a method only where it takes `self` ([`takes_self`]); a JavaScript or TypeScript class's `constructor`,
/// unless `static`, is its constructor; a Swift property no type holds is a
/// global variable; a C++ member function defined in its class without a
/// type is a constructor or destructor, as one declared there is
/// ([`declared_as`]); a C++ declaration outside a class body
/// ([`in_class_body`]) states a function or a variable; and one whose class
/// key a macro set aside declares that type ([`set_aside_class_kind`]).
/// What holds a declaration is the nearest enclosing one with a name, as for
/// its path ([`extract_container_path`]): a Dart member's signature sits in a
/// nameless declaration of the same member.
fn placed_kind(
    kind: SymbolKind,
    node: Node,
    name: &str,
    declared: &HashSet<usize>,
    content: &str,
    language: Language,
) -> SymbolKind {
    let container =
        std::iter::successors(enclosing_declaration(node, declared, language), |outer| {
            enclosing_declaration(*outer, declared, language)
        })
        .find_map(|outer| {
            extract_name_and_kind(outer, content, language)
                .filter(|(name, _)| !name.is_empty())
                .map(|(_, kind)| (outer, kind))
        });
    let enclosing = container.map(|(_, kind)| kind);
    if language == Language::Cpp
        && let Some(kind) = set_aside_class_kind(node, content).or_else(|| {
            continued_macro(node)
                .filter(|head| head.kind() == "type_definition")
                .map(node_kind)
        })
    {
        return kind;
    }
    match kind {
        SymbolKind::Function
            if language == Language::Cpp
                && node.kind() == "function_definition"
                && in_class_body(node)
                && node.child_by_field_name("type").is_none()
                && node
                    .child_by_field_name("declarator")
                    .is_some_and(|declarator| {
                        declarator_name(declarator).kind() != "operator_cast"
                    }) =>
        {
            SymbolKind::Constructor
        }
        SymbolKind::Function
            if container.is_some_and(|(outer, kind)| {
                kind.holds_members()
                    && !kind.is_namespace_like()
                    && !is_local_below(node, outer, declared)
            }) && (language != Language::Rust
                || node
                    .child_by_field_name("parameters")
                    .is_some_and(takes_self)) =>
        {
            SymbolKind::Method
        }
        SymbolKind::Method
            if matches!(language, Language::TypeScript | Language::JavaScript)
                && (node.kind() == "method_definition"
                    || node.kind() == "method_signature"
                        && node
                            .parent()
                            .is_some_and(|body| body.kind() == "class_body"))
                && name == "constructor"
                && child_of_kind(node, "static").is_none() =>
        {
            SymbolKind::Constructor
        }
        SymbolKind::Property if language == Language::Swift && enclosing.is_none() => {
            SymbolKind::Variable
        }
        SymbolKind::Method | SymbolKind::Constructor | SymbolKind::Field
            if language == Language::Cpp
                && matches!(node.kind(), "field_declaration" | "declaration")
                && !in_class_body(node) =>
        {
            if kind == SymbolKind::Field {
                SymbolKind::Variable
            } else {
                SymbolKind::Function
            }
        }
        kind => kind,
    }
}

/// Whether `inner`, a child of `outer`, is the header the declaration
/// `outer` is written with rather than its body: its parameter list, or the
/// type of what it declares. A type's own declaration holds its members
/// there instead: its definition (Go's `type S struct { … }`) or its
/// components (Java's `record R(int a)`). A record type a declaration
/// defines in its type without a name of its own is the declaration's, so
/// its members are too: `typedef struct { … } T` holds the members of `T`,
/// and Go's `inner struct{ Z int }` holds `Z`. An unscoped enum's
/// enumerators belong to the scope around it, and an alias only names any
/// other type, so C's `typedef struct S { … } T` holds neither `S` nor its
/// members.
fn in_header_of(inner: Node, outer: Node) -> bool {
    let holds = |field| {
        outer
            .child_by_field_name(field)
            .is_some_and(|held| held.id() == inner.id())
    };
    if holds("type")
        && inner.child_by_field_name("name").is_none()
        && match inner.kind() {
            "class_specifier" | "struct_specifier" | "union_specifier" => {
                inner.child_by_field_name("body").is_some()
            }
            "struct_type" | "interface_type" => true,
            _ => false,
        }
    {
        return false;
    }
    (holds("parameters") || holds("type"))
        && (ALIASES.contains(&outer.kind()) || !node_kind(outer).holds_members())
}

/// The tokens that open a declaration's body where the grammar leaves them
/// outside the body node: Python's `:`, Scala's and Ruby's `=`, the `with`
/// opening a Scala given's members, and an HCL block's `{`.
fn body_openers(language: Language) -> &'static [&'static str] {
    match language {
        Language::Python => &[":"],
        Language::Scala => &["=", "with"],
        Language::Ruby => &["="],
        Language::Terraform => &["block_start"],
        _ => &[],
    }
}

/// Where the header written before `next`, a body or the token closing an
/// empty one, ends: at the end of what precedes it, past a comment between
/// them and the token that opens the body ([`body_openers`]), so `def === =
/// true` keeps its name. A syntax error the grammar set aside there is part
/// of the header, which then reads only by recovering.
fn header_end(next: Node, language: Language) -> usize {
    let openers = body_openers(language);
    std::iter::successors(next.prev_sibling(), Node::prev_sibling)
        .find(|before| {
            !(before.is_extra() && !before.has_error()) && !openers.contains(&before.kind())
        })
        .map_or(next.start_byte(), |before| before.end_byte())
}

/// The token closing the body of `node` where it declares one with nothing
/// in it and the grammar leaves no node for it: the `end` of a Ruby or Lua
/// definition (`def h; end`) and an HCL block's `}`.
fn empty_body_close(node: Node) -> Option<Node> {
    let last = node.child(node.child_count().checked_sub(1)?)?;
    matches!(last.kind(), "end" | "block_end").then_some(last)
}

/// What a file opens with before its grammar reads code: PHP's opening tag,
/// without which a declaration's text reads as inline HTML.
fn code_opening(language: Language) -> &'static str {
    match language {
        Language::PHP => "<?php ",
        _ => "",
    }
}

fn extract_name_and_kind(
    node: Node,
    content: &str,
    language: Language,
) -> Option<(String, SymbolKind)> {
    // An impl block is named by its self type, reduced by the one shared rule
    // (`Symbol::self_type_segment`) the documentSymbol and workspace-symbol
    // producers also apply — so a method keyed under it gets the same
    // `Type/method` name_path on every surface (index, documentSymbol,
    // workspace), structural and primitive self types included. A self type
    // with no nominal name (e.g. `fn()`) reduces to an empty segment: the impl
    // is then a transparent container whose methods attach to the enclosing
    // path, never carrying a stray name.
    if node.kind() == "impl_item" {
        let type_node = node.child_by_field_name("type")?;
        let self_type = content.get(type_node.start_byte()..type_node.end_byte())?;
        let name = Symbol::self_type_segment(self_type);
        return (!name.is_empty()).then(|| (name, node_kind(node)));
    }

    // HCL names a declaration with a quoted label rather than an identifier,
    // and states what it declares in the block type written before that label
    // — neither of which the shared identifier scan and node-kind table read.
    if language == Language::Terraform {
        let kind = hcl_kind(node, content)?;
        let name_node = hcl_declared_name(node)?;
        let name = content.get(name_node.start_byte()..name_node.end_byte())?;
        return (!name.is_empty()).then(|| (name.to_string(), kind));
    }

    // Resolve the name first: nameless parents (blocks, lists, the source
    // root) are walked during container resolution and must be skipped
    // before a kind is ever assigned to them.
    let name_node = find_name_node(node, language)?;
    let name = content
        .get(name_node.start_byte()..name_end_node(name_node).end_byte())?
        .to_string();
    if names_nothing(&name, language) {
        return None;
    }
    Some((name, node_kind(node)))
}

/// The node a declared name ends with: its own, the name a Dart named
/// constructor gives itself after its class's (`Widget.named`), the class a
/// C# destructor names after its `~`, the type a C# conversion operator names
/// after `operator`, or the token before the parameters a C++ conversion
/// function's name holds, so `operator bool() const` is named `operator bool`
/// and `operator const char*()` `operator const char*`.
fn name_end_node(name: Node) -> Node {
    match (name.kind(), name.parent().map(|parent| parent.kind())) {
        ("~", Some("destructor_declaration")) => return name.next_sibling().unwrap_or(name),
        ("operator", Some("conversion_operator_declaration")) => {
            return name
                .parent()
                .and_then(|operator| operator.child_by_field_name("type"))
                .unwrap_or(name);
        }
        _ => {}
    }
    if let Some(signature) = name.parent().filter(|signature| {
        matches!(
            signature.kind(),
            "constructor_signature"
                | "constant_constructor_signature"
                | "factory_constructor_signature"
                | "redirecting_factory_constructor_signature"
        )
    }) {
        let mut cursor = signature.walk();
        return signature
            .children_by_field_name("name", &mut cursor)
            .last()
            .unwrap_or(name);
    }
    let mut conversion = name;
    while conversion.kind() == "qualified_identifier" {
        match conversion.child_by_field_name("name") {
            Some(inner) => conversion = inner,
            None => return name,
        }
    }
    if conversion.kind() != "operator_cast" {
        return name;
    }
    let mut declarator = conversion;
    while let Some(inner) = declarator.child_by_field_name("declarator") {
        if inner.kind() == "abstract_function_declarator" {
            return inner.prev_sibling().unwrap_or(name);
        }
        declarator = inner;
    }
    name
}

/// Whether a declared name is no name at all: empty, the blank `_` that
/// discards what is bound to it, or in C and C++ a word both languages
/// reserve, which a grammar recovering from a macro reads where a name goes
/// (`PyAPI_FUNC(int) Py_IsInitialized(void)` declares `void`).
fn names_nothing(name: &str, language: Language) -> bool {
    name.is_empty() || name == "_" || language == Language::Cpp && C_KEYWORDS.contains(&name)
}

/// The keywords C and C++ both reserve.
const C_KEYWORDS: &[&str] = &[
    "auto", "break", "case", "char", "const", "continue", "default", "do", "double", "else",
    "enum", "extern", "float", "for", "goto", "if", "inline", "int", "long", "register", "return",
    "short", "signed", "sizeof", "static", "struct", "switch", "typedef", "union", "unsigned",
    "void", "volatile", "while",
];

/// The first child of `node` with the given grammar kind. Grammars that wrap a
/// declaration's name in unnamed intermediate nodes are read through this
/// rather than by position, which moves whenever modifiers or attributes are
/// written before it.
fn child_of_kind<'a>(node: Node<'a>, kind: &str) -> Option<Node<'a>> {
    (0..node.child_count()).find_map(|i| node.child(i).filter(|c| c.kind() == kind))
}

/// Every child of `node` its grammar holds under `field`, in order.
fn children_of_field<'a>(node: Node<'a>, field: &str) -> Vec<Node<'a>> {
    let mut cursor = node.walk();
    node.children_by_field_name(field, &mut cursor).collect()
}

/// Every child of `node` with the given grammar kind, in order.
fn children_of_kind<'a>(node: Node<'a>, kind: &str) -> Vec<Node<'a>> {
    (0..node.child_count())
        .filter_map(|i| node.child(i).filter(|c| c.kind() == kind))
        .collect()
}

/// The last child of `node` with the given grammar kind, for grammars that
/// state a declaration as a run of same-kind children whose last one names it.
fn last_child_of_kind<'a>(node: Node<'a>, kind: &str) -> Option<Node<'a>> {
    (0..node.child_count())
        .rev()
        .find_map(|i| node.child(i).filter(|c| c.kind() == kind))
}

/// The node an HCL declaration is named by.
///
/// A block carries its name in quoted labels: `resource "google_storage_bucket"
/// "logs"` is addressed as `google_storage_bucket.logs`, so the last label
/// names it and an earlier one qualifies it. A label's `template_literal` is
/// the text inside the quotes, which is what the name has to be for its span
/// to be the name's span. An attribute is named by its identifier, and a block
/// with no label — `terraform`, `locals`, a resource's nested argument block —
/// declares nothing to name.
fn hcl_declared_name(node: Node) -> Option<Node> {
    match node.kind() {
        "block" => last_child_of_kind(node, "string_lit")
            .and_then(|label| child_of_kind(label, "template_literal")),
        "attribute" => child_of_kind(node, "identifier"),
        _ => None,
    }
}

/// Whether an HCL attribute's own name is addressable, which is what separates
/// a declaration from an argument. A file's top-level attributes are what it
/// declares (a `.tfvars` setting), and a `locals` block's are addressed as
/// `local.x`. Everywhere else an attribute configures the block around it, and
/// `name = "x"` inside a resource declares no `name`.
fn hcl_attribute_declares(node: Node, content: &str) -> bool {
    let Some(container) = node.parent().and_then(|body| body.parent()) else {
        return false;
    };
    match container.kind() {
        "config_file" => true,
        "block" => {
            child_of_kind(container, "identifier")
                .and_then(|id| content.get(id.start_byte()..id.end_byte()))
                == Some("locals")
        }
        _ => false,
    }
}

/// The label qualifying an HCL block's name: the first of two or more, which
/// is the type a resource or data source is an instance of. A block with a
/// single label is named outright and qualified by nothing.
fn hcl_type_label(node: Node, content: &str) -> Option<String> {
    let first = child_of_kind(node, "string_lit")?;
    if first.id() == last_child_of_kind(node, "string_lit")?.id() {
        return None;
    }
    let text = child_of_kind(first, "template_literal")?;
    content
        .get(text.start_byte()..text.end_byte())
        .map(str::to_string)
}

/// What an HCL declaration states, read from the block type it opens with: a
/// `variable` or `output` declares a value and a `module` a unit of
/// composition. Every other labeled block — a resource, a data source, a
/// provider, another HCL dialect's own type — declares a named typed object,
/// and a file-level attribute a setting.
fn hcl_kind(node: Node, content: &str) -> Option<SymbolKind> {
    if node.kind() == "attribute" {
        return hcl_attribute_declares(node, content).then_some(SymbolKind::Property);
    }
    Some(
        match child_of_kind(node, "identifier")
            .and_then(|id| content.get(id.start_byte()..id.end_byte()))
        {
            Some("variable" | "output") => SymbolKind::Property,
            Some("module") => SymbolKind::Module,
            _ => SymbolKind::Struct,
        },
    )
}

/// The identifier a captured declaration is named by.
///
/// A node this cannot name is dropped from extraction without a trace, so a
/// query and this function are one decision: several grammars spell a member's
/// name behind wrappers their neighbours do not use, and a query that captured
/// one of those read as a language simply not declaring that form. The
/// declaration fixtures are what hold the two together.
fn find_name_node(node: Node, language: Language) -> Option<Node> {
    // An HCL block's identifier child is its type, so the shared scan below
    // would read `resource` as the name of every resource in the file.
    if language == Language::Terraform {
        return hcl_declared_name(node);
    }
    // An embedded Go field is named by its type, without the package that
    // qualifies it (`pkg.Embedded` is field `Embedded`).
    if language == Language::Go
        && node.kind() == "field_declaration"
        && node.child_by_field_name("name").is_none()
    {
        return node
            .child_by_field_name("type")
            .and_then(embedded_type_name);
    }
    // A destructuring pattern is named by the names it binds alone, so one
    // binding none (`const {} = obj`) names nothing.
    if matches!(language, Language::TypeScript | Language::JavaScript)
        && let Some(pattern) = node
            .child_by_field_name("name")
            .filter(|name| matches!(name.kind(), "object_pattern" | "array_pattern"))
    {
        return bound_names(pattern).into_iter().next();
    }
    if matches!(
        node.kind(),
        "required_parameter" | "optional_parameter" | "class_parameter"
    ) {
        if !declares_property(node) {
            return None;
        }
        return node
            .child_by_field_name("pattern")
            .or_else(|| node.child_by_field_name("name"))
            .or_else(|| child_of_kind(node, "simple_identifier"));
    }
    if node.kind() == "property_promotion_parameter" {
        return child_of_kind(node.child_by_field_name("name")?, "name");
    }
    // A Swift deinitializer and a Kotlin secondary constructor are named by
    // their keyword alone.
    if node.kind() == "deinit_declaration" {
        return child_of_kind(node, "deinit");
    }
    if node.kind() == "secondary_constructor" {
        return child_of_kind(node, "constructor");
    }
    // An operator is named by its symbol (Dart's `operator ==`, C#'s
    // `operator +`), a C# conversion operator by its keyword through the
    // type it converts to (`operator int`), an indexer by `this` and a
    // destructor through its class (`~C`).
    match node.kind() {
        "operator_signature" | "operator_declaration" => {
            return node.child_by_field_name("operator");
        }
        "conversion_operator_declaration" => return child_of_kind(node, "operator"),
        "indexer_declaration" => return child_of_kind(node, "this"),
        "destructor_declaration" => return child_of_kind(node, "~"),
        _ => {}
    }
    // An assignment declares the names its targets bind and nothing else:
    // `obj.attr = value` declares neither `obj` nor `value`.
    if matches!(language, Language::Python | Language::Ruby)
        && matches!(node.kind(), "assignment" | "operator_assignment")
    {
        return declared_names(node, language).into_iter().next();
    }
    if node.kind() == "type_alias_statement" {
        let mut left = node.child_by_field_name("left")?;
        while left.kind() != "identifier" {
            left = left.named_child(0)?;
        }
        return Some(left);
    }

    let name_field = match language {
        Language::Kotlin => node
            .child_by_field_name("name")
            .or_else(|| node.child_by_field_name("simple_identifier"))
            .or_else(|| {
                child_of_kind(node, "variable_declaration")
                    .and_then(|d| child_of_kind(d, "simple_identifier"))
            }),
        Language::Cpp => declared_names(node, language)
            .into_iter()
            .next()
            .or_else(|| node.child_by_field_name("name"))
            .or_else(|| lone_name(node)),
        Language::TypeScript | Language::JavaScript => declared_names(node, language)
            .into_iter()
            .next()
            .or_else(|| node.child_by_field_name("name"))
            .or_else(|| node.child_by_field_name("property")),
        Language::Swift | Language::Lua => declared_names(node, language)
            .into_iter()
            .next()
            .or_else(|| node.child_by_field_name("name")),
        Language::Scala => match node.child_by_field_name("pattern") {
            Some(pattern) => return bound_names(pattern).into_iter().next(),
            None => node.child_by_field_name("name"),
        },
        _ => node
            .child_by_field_name("name")
            .or_else(|| declared_names(node, language).into_iter().next()),
    };

    let is_name = |node: &Node| {
        matches!(
            node.kind(),
            "identifier" | "name" | "simple_identifier" | "type_identifier" | "property_identifier"
        )
    };
    name_field
        .or_else(|| {
            let mut cursor = node.walk();
            node.children(&mut cursor).find(is_name)
        })
        // A TypeScript enum member without a value is its name alone.
        .or_else(|| is_name(&node).then_some(node))
}

/// Whether a constructor parameter declares a property of its class as well:
/// a TypeScript accessibility, `readonly` or `override` modifier says so, as
/// does a Kotlin or Scala `val` or `var`. Every parameter in the first list of
/// a Scala case class is one.
fn declares_property(parameter: Node) -> bool {
    let mut cursor = parameter.walk();
    let marked = parameter.children(&mut cursor).any(|child| {
        matches!(
            child.kind(),
            "accessibility_modifier"
                | "override_modifier"
                | "readonly"
                | "binding_pattern_kind"
                | "val"
                | "var"
        )
    });
    marked
        || parameter.parent().is_some_and(|list| {
            list.parent().is_some_and(|class| {
                class.kind() == "class_definition"
                    && child_of_kind(class, "case").is_some()
                    && class
                        .child_by_field_name("class_parameters")
                        .is_some_and(|first| first.id() == list.id())
            })
        })
}

/// Map a captured declaration node to a [`SymbolKind`]. Every node kind the
/// extraction queries capture has an explicit arm; the trailing arm only
/// ever serves nameless container parents whose kind is discarded.
fn node_kind(node: Node) -> SymbolKind {
    match node.kind() {
        "function_item"
        | "function_definition"
        | "function_declaration"
        | "generator_function_declaration"
        | "function_signature"
        | "function_signature_item"
        | "macro_definition" => SymbolKind::Function,

        "method_item"
        | "method_declaration"
        | "method_definition"
        | "method"
        | "singleton_method"
        | "method_signature"
        | "abstract_method_signature"
        | "protocol_function_declaration"
        | "method_elem" => SymbolKind::Method,

        "constructor_signature"
        | "constructor_declaration"
        | "compact_constructor_declaration"
        | "secondary_constructor"
        | "destructor_declaration"
        | "constant_constructor_signature"
        | "factory_constructor_signature"
        | "redirecting_factory_constructor_signature"
        | "init_declaration"
        | "deinit_declaration" => SymbolKind::Constructor,

        // A Java interface's fields, which the grammar names constants for
        // being implicitly `static final`, are fields as a class's `static
        // final` ones are; an annotation's elements are methods it declares.
        "constant_declaration" => SymbolKind::Field,
        "annotation_type_element_declaration" => SymbolKind::Method,

        "event_field_declaration" | "event_declaration" => SymbolKind::Event,
        "operator_declaration" | "conversion_operator_declaration" => SymbolKind::Method,
        "indexer_declaration" => SymbolKind::Property,
        "local_function_statement" => SymbolKind::Function,

        // Swift states a struct, an enum, an actor, a class and an extension
        // with one node, and its keyword says which.
        "class_declaration" if node.child_by_field_name("declaration_kind").is_some() => {
            match node
                .child_by_field_name("declaration_kind")
                .map(|keyword| keyword.kind())
            {
                Some("struct") => SymbolKind::Struct,
                Some("enum") => SymbolKind::Enum,
                // As sourcekit-lsp's answer reads (see `convert_symbol_kind`).
                Some("extension") => SymbolKind::Object,
                _ => SymbolKind::Class,
            }
        }

        "class_declaration"
        | "abstract_class_declaration"
        | "record_declaration"
        | "delegate_declaration"
        | "class_definition"
        | "class_specifier"
        | "object_declaration"
        | "companion_object"
        | "object_definition"
        | "package_object"
        | "extension_declaration"
        | "extension_type_declaration"
        | "class" => SymbolKind::Class,

        // An impl block adds members to a type declared elsewhere, as
        // rust-analyzer reads it.
        "impl_item" => SymbolKind::Object,

        "struct_item" | "struct_specifier" | "struct_type" | "struct_declaration"
        | "union_item" | "union_specifier" => SymbolKind::Struct,

        "enum_item" | "enum_declaration" | "enum_specifier" | "enum_definition" => SymbolKind::Enum,

        "enum_variant"
        | "enumerator"
        | "enum_constant"
        | "enum_member_declaration"
        | "enum_entry"
        | "enum_case"
        | "simple_enum_case"
        | "full_enum_case" => SymbolKind::EnumMember,

        "interface_declaration"
        | "interface_type"
        | "trait_item"
        | "trait_declaration"
        | "trait_definition"
        | "mixin_declaration"
        | "annotation_type_declaration"
        | "protocol_declaration" => SymbolKind::Interface,

        // Every grammar's namespace/module/package container. These organize
        // code but must NOT widen a member's name_path (see `is_namespace_like`
        // and `extract_container_path`): Rust `mod`, C/C++ `namespace`, Java
        // `package`, TS/JS `namespace`/`module` (both parse as `internal_module`,
        // plus ambient `module "x"`), C# block- and file-scoped `namespace`, and
        // PHP `namespace`.
        "mod_item" | "internal_module" | "module" => SymbolKind::Module,
        "namespace_definition"
        | "namespace_declaration"
        | "file_scoped_namespace_declaration"
        | "namespace_alias_definition" => SymbolKind::Namespace,
        // A C++ concept names a predicate over types, as clangd lists it.
        "concept_definition" => SymbolKind::Variable,
        "package_declaration" => SymbolKind::Package,

        // Go `type X = ...`: classify by the spec's underlying type.
        "type_spec" => go_type_kind(node),

        // A named type alias introduces a type, not a generic `<T>` param.
        kind if ALIASES.contains(&kind) => SymbolKind::Class,

        "const_item" | "const_spec" | "val_definition" | "val_declaration"
        | "const_declaration" => SymbolKind::Constant,

        "static_item" | "var_spec" | "var_definition" | "var_declaration" => SymbolKind::Variable,

        // Scala reads a given as what it stands for: one defining members as
        // an object, one taking parameters as a method, and an alias as a
        // lazy `val`.
        "given_definition" => {
            if node
                .child_by_field_name("body")
                .is_some_and(|body| body.kind() == "with_template_body")
            {
                SymbolKind::Class
            } else if ["parameters", "type_parameters"]
                .into_iter()
                .any(|field| node.child_by_field_name(field).is_some())
            {
                SymbolKind::Function
            } else {
                SymbolKind::Constant
            }
        }

        // JS/TS `const f = () => {}` is a callable; classify by the
        // initializer rather than always Variable (which is_low_level would
        // drop under exclude_low_level).
        "variable_declarator" => declarator_kind(node),

        // Python states a named value with a bare binding: no keyword separates
        // a constant from a variable, so nothing structural does either. Ruby's
        // `||=` binds a name that is not yet bound.
        "assignment" | "operator_assignment" => SymbolKind::Variable,

        "property_declaration"
        | "public_field_definition"
        | "field_definition"
        | "property_signature"
        | "required_parameter"
        | "optional_parameter"
        | "class_parameter"
        | "property_promotion_parameter"
        | "protocol_property_declaration" => SymbolKind::Property,
        // A record's components: C# declares a property for each, Java a
        // field.
        "parameter" if is_record_component(node) => SymbolKind::Property,
        "formal_parameter" if is_record_component(node) => SymbolKind::Field,

        "enum_assignment" => SymbolKind::EnumMember,
        "property_identifier" if node.parent().is_some_and(|body| body.kind() == "enum_body") => {
            SymbolKind::EnumMember
        }
        "field_declaration" | "declaration" => field_kind(node),
        "extension_type_representation" => SymbolKind::Field,
        "top_level_variable_declaration" | "external_variable_declaration" => SymbolKind::Variable,
        "getter_signature" | "setter_signature" => SymbolKind::Property,
        "operator_signature" => SymbolKind::Method,
        "variable_declaration" | "assignment_statement" if lua_function_value(node).is_some() => {
            SymbolKind::Function
        }
        "assignment_statement" if child_of_kind(node, "variable_list").is_some() => {
            SymbolKind::Function
        }

        _ => SymbolKind::Variable,
    }
}

fn is_record_component(node: Node) -> bool {
    matches!(node.kind(), "parameter" | "formal_parameter")
        && node
            .parent()
            .and_then(|list| list.parent())
            .is_some_and(|owner| owner.kind() == "record_declaration")
}

/// Classify a Go `type_spec` by its underlying type: `struct` → Struct,
/// `interface` → Interface, anything else is a named alias (Class).
fn go_type_kind(node: Node) -> SymbolKind {
    match node.child_by_field_name("type").map(|t| t.kind()) {
        Some("struct_type") => SymbolKind::Struct,
        Some("interface_type") => SymbolKind::Interface,
        _ => SymbolKind::Class,
    }
}

/// Classify a member declaration by its declarator ([`declared_as`]). Java,
/// whose fields use the same node kind, declares methods elsewhere and is
/// unaffected.
fn field_kind(node: Node) -> SymbolKind {
    node.child_by_field_name("declarator")
        .map(declarator_name)
        .map_or(SymbolKind::Field, |name| declared_as(node, name))
}

/// What the C or C++ member declaration `statement` declares `name` as. C++
/// states a method with the same node it states a data member with, and only
/// the declarator separates them ([`member_kind`]); a member function
/// declared without a type is a constructor or a destructor, unless its name
/// states the type it converts to (`operator bool()`). Outside a class the
/// same declarations state functions and variables ([`placed_kind`]).
fn declared_as(statement: Node, name: Node) -> SymbolKind {
    match member_kind(name) {
        SymbolKind::Method
            if statement.child_by_field_name("type").is_none()
                && name.kind() != "operator_cast" =>
        {
            SymbolKind::Constructor
        }
        kind => kind,
    }
}

/// Whether a C++ declaration is written in a class body, through a member
/// template's header, a preprocessor conditional or the syntax error the
/// grammar recovered from around it.
fn in_class_body(statement: Node) -> bool {
    std::iter::successors(statement.parent(), Node::parent)
        .find(|holder| {
            !matches!(holder.kind(), "template_declaration" | "ERROR")
                && !holder.kind().starts_with("preproc_")
        })
        .is_some_and(|holder| holder.kind() == "field_declaration_list")
}

/// Whether the C or C++ declarator around `name` declares a method or a data
/// member. A declarator wraps its name in what makes its type and is read
/// inside out, so a name declares a function where a function declarator is
/// the first to enclose it: `int& at(int)` is a method returning a reference,
/// and `int (*fp)(int)` a member pointing to a function. A conversion
/// function's name holds its function declarator itself.
fn member_kind(name: Node) -> SymbolKind {
    if name.kind() == "operator_cast" {
        return SymbolKind::Method;
    }
    let mut node = macro_wrapper(name).unwrap_or(name);
    while let Some(parent) = node.parent() {
        match parent.kind() {
            "parenthesized_declarator" | "attributed_declarator" | "qualified_identifier" => {
                node = parent
            }
            "function_declarator" => return SymbolKind::Method,
            _ => break,
        }
    }
    SymbolKind::Field
}

/// What a qualified C++ name declares under the scope the grammar made by
/// supplying a missing `::`, at whatever depth: a macro read as the type and
/// the type read as a scope (`EXPORT Code f()`, `EXPORT std::string f()`),
/// so the name is what follows it. A name every `::` of which is written is
/// the qualified name itself (`Widget::~Widget`), and this gives none.
fn supplied_scope(name: Node) -> Option<Node> {
    let mut level = name;
    loop {
        if child_of_kind(level, "::").is_some_and(|scope| scope.is_missing()) {
            return level.child_by_field_name("name");
        }
        level = level
            .child_by_field_name("name")
            .filter(|inner| inner.kind() == "qualified_identifier")?;
    }
}

/// Each name a C++ declarator or namespace name binds: every one a structured
/// binding (`auto [a, b]`) or a nested namespace (`a::b::c`) writes.
fn cpp_bound_names(name: Node) -> Vec<Node> {
    match name.kind() {
        "structured_binding_declarator" | "nested_namespace_specifier" => {
            let mut cursor = name.walk();
            name.named_children(&mut cursor)
                .flat_map(cpp_bound_names)
                .collect()
        }
        _ => vec![name],
    }
}

/// The name a C or C++ declarator declares, inside whatever wraps it: `*`,
/// `&`, `[3]`, `(int)`, parentheses, attributes or an initializer. Three of
/// those hold the declarator they wrap under no field, beside whatever the
/// grammar set aside as an error (the `Repo::` of `(Repo::*member_fn)()`),
/// and a conversion function's name holds the declarator of its own
/// parameters. A qualified name is the name ([`supplied_scope`]). A function
/// declarator the grammar set aside as an error right before the one it read
/// is the declarator: its parameters complete it, so what it read after them
/// is a macro written there (`int f(int x) G_GNUC_PURE;`, `name() const
/// ABSL_ATTRIBUTE_LIFETIME_BOUND`). A macro's argument list stands for no name
/// of its own ([`macro_argument_name`], [`macro_wrapped_name`]).
fn declarator_name(declarator: Node) -> Node {
    let mut node = declarator;
    loop {
        if let Some(named) = macro_argument_name(node).or_else(|| macro_wrapped_name(node)) {
            return named;
        }
        if let Some(set_aside) = node
            .prev_sibling()
            .filter(|before| before.is_error() && before.named_child_count() == 1)
            .and_then(|before| before.named_child(0))
            .filter(|held| held.kind() == "function_declarator")
        {
            node = set_aside;
        }
        let inner = match node.kind() {
            "operator_cast" => None,
            "qualified_identifier" => supplied_scope(node),
            "reference_declarator" | "parenthesized_declarator" | "attributed_declarator" => {
                let mut cursor = node.walk();
                node.named_children(&mut cursor).find(|child| {
                    !matches!(
                        child.kind(),
                        "attribute_declaration" | "ms_call_modifier" | "ERROR"
                    )
                })
            }
            _ => node.child_by_field_name("declarator"),
        };
        match inner {
            Some(inner) => node = inner,
            None => return node,
        }
    }
}

/// The name a C or C++ declarator writes after a macro's argument list: a
/// name or type in parentheses that the grammar found another name after, and
/// set that aside as an error, is the macro's argument, and the name after it
/// the declarator (`typedef Array(T) Ts;`, `Array(uint32_t) ids[4];`).
fn macro_argument_name(declarator: Node) -> Option<Node> {
    let identifier = |node: &Node| {
        matches!(
            node.kind(),
            "identifier" | "type_identifier" | "field_identifier"
        )
    };
    (declarator.kind() == "parenthesized_declarator"
        && declarator
            .named_child(0)
            .is_some_and(|inside| identifier(&inside) || inside.kind() == "primitive_type"))
    .then(|| declarator.next_sibling())
    .flatten()
    .filter(|after| after.is_error() && after.named_child_count() == 1)
    .and_then(|after| after.named_child(0))
    .filter(identifier)
}

/// The name a C or C++ function declarator applies to through a macro around
/// it. Neither language lets a function return a function, so a function
/// declarator applied to another whose one parameter is a name alone applies
/// to that name: a macro wraps it (`GIT_CALLBACK(read)(void *)`), or, where
/// what the grammar read as the inner function is a keyword, parentheses do
/// (`int (lua_gettop)(lua_State *L)`).
fn macro_wrapped_name(declarator: Node) -> Option<Node> {
    if declarator.kind() != "function_declarator" {
        return None;
    }
    let wrapper = declarator
        .child_by_field_name("declarator")
        .filter(|inner| inner.kind() == "function_declarator")?;
    let parameters = wrapper.child_by_field_name("parameters")?;
    let mut cursor = parameters.walk();
    let mut held = parameters
        .named_children(&mut cursor)
        .filter(|held| !held.is_extra());
    let parameter = held.next().filter(|parameter| {
        parameter.kind() == "parameter_declaration"
            && parameter.child_by_field_name("declarator").is_none()
    })?;
    if held.next().is_some() {
        return None;
    }
    parameter
        .child_by_field_name("type")
        .filter(|name| matches!(name.kind(), "type_identifier" | "identifier"))
}

/// The function declarator that stands where a macro-wrapped name
/// ([`macro_wrapped_name`]) would: the wrapper the macro is read as.
fn macro_wrapper(name: Node) -> Option<Node> {
    let wrapper = name.parent()?.parent()?.parent()?;
    let applied = wrapper.parent()?;
    (macro_wrapped_name(applied).is_some_and(|wrapped| wrapped.id() == name.id()))
        .then_some(wrapper)
}

/// Classify a JS/TS `variable_declarator` by its initializer and keyword: a
/// function value (arrow function, function expression, or generator
/// function) is a callable Function, and any other value a Constant where
/// `const` declares it and a Variable where `let` or `var` does.
fn declarator_kind(node: Node) -> SymbolKind {
    match node.child_by_field_name("value").map(|v| v.kind()) {
        Some("arrow_function") | Some("function_expression") | Some("generator_function") => {
            SymbolKind::Function
        }
        _ if node
            .parent()
            .and_then(|declaration| declaration.child_by_field_name("kind"))
            .is_some_and(|keyword| keyword.kind() == "const") =>
        {
            SymbolKind::Constant
        }
        _ => SymbolKind::Variable,
    }
}

// Language-specific tree-sitter queries for symbol extraction.

const RUST_QUERY: &str = r#"
(function_item) @symbol
(function_signature_item) @symbol
(macro_definition) @symbol
(struct_item) @symbol
(union_item) @symbol
(struct_item (field_declaration_list (field_declaration) @symbol))
(union_item (field_declaration_list (field_declaration) @symbol))
(enum_item (enum_variant_list (enum_variant) @symbol))
(enum_variant (field_declaration_list (field_declaration) @symbol))
(enum_item) @symbol
(trait_item) @symbol
(associated_type) @symbol
(impl_item) @symbol
(mod_item) @symbol
(type_item) @symbol
(const_item) @symbol
(static_item) @symbol
"#;

const GO_QUERY: &str = r#"
(function_declaration) @symbol
(method_declaration) @symbol
(type_declaration (type_spec) @symbol)
(type_declaration (type_alias) @symbol)
(type_spec (struct_type (field_declaration_list (field_declaration) @symbol)))
(field_declaration type: (struct_type (field_declaration_list (field_declaration) @symbol)))
(type_spec (interface_type (method_elem) @symbol))
(source_file (const_declaration (const_spec) @symbol))
(source_file (var_declaration (var_spec) @symbol))
(source_file (var_declaration (var_spec_list (var_spec) @symbol)))
"#;

// An assignment declares at module and class scope, under an `if`, a `try`
// or a `with` as much as directly, and binds a local inside a function (see
// `declares`). The names are the identifiers the assignment's `left` binds,
// through a chain and a destructuring bind as well (`a = b = None`, `a, b =
// …`, see `declared_names`).
const PYTHON_QUERY: &str = r#"
(function_definition) @symbol
(class_definition) @symbol
(type_alias_statement) @symbol
(module (expression_statement (assignment) @symbol))
(block (expression_statement (assignment) @symbol))
"#;

// A module's variables are the declarators no body encloses: at the top of a
// file, exported, ambient, or in a namespace. Anchoring there keeps a
// function's locals and loop counters out, and a destructuring declarator
// declares each name its pattern binds (see `declared_names`). A constructor
// parameter declares a property where a modifier says so (see
// `find_name_node`).
const TYPESCRIPT_QUERY: &str = r#"
(function_declaration) @symbol
(generator_function_declaration) @symbol
(class_declaration) @symbol
(abstract_class_declaration) @symbol
(internal_module) @symbol
(interface_declaration) @symbol
(type_alias_declaration) @symbol
(enum_declaration) @symbol
(method_definition) @symbol
(interface_declaration (interface_body (method_signature) @symbol))
(interface_declaration (interface_body (property_signature) @symbol))
(class_body (abstract_method_signature) @symbol)
(class_body (method_signature) @symbol)
(class_body (public_field_definition) @symbol)
(method_definition
  name: (property_identifier) @constructor
  parameters: (formal_parameters [(required_parameter) (optional_parameter)] @symbol)
  (#eq? @constructor "constructor"))
(function_signature) @symbol
(module) @symbol
(enum_body name: (property_identifier) @symbol)
(enum_body (enum_assignment) @symbol)
(program (lexical_declaration (variable_declarator) @symbol))
(program (variable_declaration (variable_declarator) @symbol))
(export_statement (lexical_declaration (variable_declarator) @symbol))
(export_statement (variable_declaration (variable_declarator) @symbol))
(ambient_declaration (lexical_declaration (variable_declarator) @symbol))
(ambient_declaration (variable_declaration (variable_declarator) @symbol))
(internal_module body: (statement_block
  [(lexical_declaration (variable_declarator) @symbol)
   (variable_declaration (variable_declarator) @symbol)]))
(module body: (statement_block
  [(lexical_declaration (variable_declarator) @symbol)
   (variable_declaration (variable_declarator) @symbol)]))
"#;

const JAVASCRIPT_QUERY: &str = r#"
(function_declaration) @symbol
(generator_function_declaration) @symbol
(class_declaration) @symbol
(method_definition) @symbol
(class_body (field_definition) @symbol)
(program (lexical_declaration (variable_declarator) @symbol))
(program (variable_declaration (variable_declarator) @symbol))
(export_statement (lexical_declaration (variable_declarator) @symbol))
(export_statement (variable_declaration (variable_declarator) @symbol))
"#;

const JAVA_QUERY: &str = r#"
(package_declaration) @symbol
(class_declaration) @symbol
(record_declaration) @symbol
(interface_declaration) @symbol
(annotation_type_declaration) @symbol
(enum_declaration) @symbol
(enum_body (enum_constant) @symbol)
(record_declaration parameters: (formal_parameters (formal_parameter) @symbol))
(constructor_declaration) @symbol
(compact_constructor_declaration) @symbol
(method_declaration) @symbol
(field_declaration) @symbol
(interface_body (constant_declaration) @symbol)
(annotation_type_body (annotation_type_element_declaration) @symbol)
"#;

// A local `val` is the same `property_declaration` a member is, so members
// are matched where they are declared: in a type body or at file scope. A
// body written on one line leaves its last member inside a syntax error, for
// want of the separator the grammar expects before `}`.
const KOTLIN_QUERY: &str = r#"
(class_declaration) @symbol
(enum_class_body (enum_entry) @symbol)
(primary_constructor (class_parameter) @symbol)
(secondary_constructor) @symbol
(object_declaration) @symbol
(companion_object) @symbol
(function_declaration) @symbol
(source_file [(property_declaration) (type_alias)] @symbol)
(class_body (property_declaration) @symbol)
(enum_class_body (property_declaration) @symbol)
(class_body (ERROR (property_declaration) @symbol))
(enum_class_body (ERROR (property_declaration) @symbol))
"#;

// A member declared in a class body is a `field_declaration`, and one a
// `template` header heads is a plain `declaration` inside the template. One
// declared without a type is a plain `declaration` too: a destructor, a
// conversion function, or a constructor, which is named for its class. A
// macro invoked in a class body reads as one of those with another name.
// Outside a class, a `declaration` states a function or a variable where the
// file, a namespace, `extern "C"` or a template holds it, and under a
// preprocessor conditional — a header's include guard — wherever no function
// body does (see `declares`).
const CPP_QUERY: &str = r#"
(function_definition) @symbol
(class_specifier) @symbol
(struct_specifier) @symbol
(union_specifier) @symbol
(enum_specifier) @symbol
(translation_unit [(declaration type: (_)) (type_definition) (alias_declaration) (namespace_alias_definition)] @symbol)
(declaration_list [(declaration type: (_)) (type_definition) (alias_declaration) (namespace_alias_definition)] @symbol)
(preproc_if [(declaration type: (_)) (type_definition) (alias_declaration) (namespace_alias_definition)] @symbol)
(preproc_ifdef [(declaration type: (_)) (type_definition) (alias_declaration) (namespace_alias_definition)] @symbol)
(preproc_elif [(declaration type: (_)) (type_definition) (alias_declaration) (namespace_alias_definition)] @symbol)
(preproc_elifdef [(declaration type: (_)) (type_definition) (alias_declaration) (namespace_alias_definition)] @symbol)
(preproc_else [(declaration type: (_)) (type_definition) (alias_declaration) (namespace_alias_definition)] @symbol)
(ERROR [(declaration type: (_)) (type_definition) (alias_declaration) (namespace_alias_definition)] @symbol)
(linkage_specification body: (declaration type: (_)) @symbol)
(template_declaration [(declaration type: (_)) (alias_declaration) (concept_definition)] @symbol)
(field_declaration_list [(type_definition) (alias_declaration)] @symbol)
(enumerator_list (enumerator) @symbol)
(namespace_definition) @symbol
(field_declaration) @symbol
(field_declaration_list (template_declaration (declaration) @symbol))
(field_declaration_list
  (declaration declarator: (function_declarator declarator: (destructor_name))) @symbol)
(field_declaration_list (declaration declarator: (operator_cast)) @symbol)
(class_specifier
  name: (type_identifier) @class
  body: (field_declaration_list
    (declaration declarator: (function_declarator declarator: (identifier) @name)) @symbol)
  (#eq? @class @name))
(struct_specifier
  name: (type_identifier) @class
  body: (field_declaration_list
    (declaration declarator: (function_declarator declarator: (identifier) @name)) @symbol)
  (#eq? @class @name))
"#;

const CSHARP_QUERY: &str = r#"
(namespace_declaration) @symbol
(file_scoped_namespace_declaration) @symbol
(class_declaration) @symbol
(record_declaration) @symbol
(delegate_declaration) @symbol
(interface_declaration) @symbol
(struct_declaration) @symbol
(enum_declaration) @symbol
(enum_member_declaration_list (enum_member_declaration) @symbol)
(record_declaration (parameter_list (parameter) @symbol))
(constructor_declaration) @symbol
(method_declaration) @symbol
(property_declaration) @symbol
(field_declaration) @symbol
(event_field_declaration) @symbol
(event_declaration) @symbol
(local_function_statement) @symbol
(operator_declaration) @symbol
(conversion_operator_declaration) @symbol
(indexer_declaration) @symbol
(destructor_declaration) @symbol
"#;

const RUBY_QUERY: &str = r#"
(module) @symbol
(class) @symbol
(method) @symbol
(singleton_method) @symbol
(program [(assignment) (operator_assignment operator: "||=")] @symbol)
(module (body_statement [(assignment) (operator_assignment operator: "||=")] @symbol))
(class (body_statement [(assignment) (operator_assignment operator: "||=")] @symbol))
"#;

const BASH_QUERY: &str = r#"
(function_definition) @symbol
"#;

// Lua defines its function statement as the assignment of a function
// (`function M.f() end` is `M.f = function() end`), so a chunk's field
// assigned a function is declared as the statement declares it; a field
// assigned any other value is a table entry, not a declaration.
const LUA_QUERY: &str = r#"
(function_declaration) @symbol
(chunk (variable_declaration) @symbol)
(chunk (assignment_statement (variable_list name: (dot_index_expression)) (expression_list value: (function_definition))) @symbol)
"#;

// A Swift `let`/`var` inside a function body parses as the same
// `property_declaration` a stored property does, so members are matched
// where they are declared — directly in a type body or at file scope.
const SWIFT_QUERY: &str = r#"
(class_declaration) @symbol
(protocol_declaration) @symbol
(function_declaration) @symbol
(protocol_function_declaration) @symbol
(init_declaration) @symbol
(deinit_declaration) @symbol
(typealias_declaration) @symbol
(associatedtype_declaration) @symbol
(enum_class_body (enum_entry) @symbol)
(class_body (property_declaration) @symbol)
(enum_class_body (property_declaration) @symbol)
(protocol_body (protocol_property_declaration) @symbol)
(source_file (property_declaration) @symbol)
"#;

// `val`/`var` share one node kind with their function-local counterparts;
// the template body is what separates a member from a local.
const SCALA_QUERY: &str = r#"
(class_definition) @symbol
(object_definition) @symbol
(package_object) @symbol
(trait_definition) @symbol
(enum_definition) @symbol
(enum_case_definitions (simple_enum_case) @symbol)
(enum_case_definitions (full_enum_case) @symbol)
(function_definition) @symbol
(function_declaration) @symbol
(compilation_unit [(val_definition) (var_definition) (type_definition) (given_definition)] @symbol)
(template_body [(val_definition) (var_definition) (val_declaration) (var_declaration) (type_definition) (given_definition)] @symbol)
(with_template_body [(val_definition) (var_definition) (type_definition) (given_definition)] @symbol)
(class_definition class_parameters: (class_parameters (class_parameter) @symbol))
(enum_definition class_parameters: (class_parameters (class_parameter) @symbol))
"#;

// Dart names a function on its signature, not on the declaration that
// wraps it, and a method's signature nests the same node.
const DART_QUERY: &str = r#"
(class_declaration) @symbol
(enum_declaration) @symbol
(enum_body (enum_constant) @symbol)
(mixin_declaration) @symbol
(extension_declaration) @symbol
(extension_type_declaration) @symbol
(extension_type_representation) @symbol
(source_file [(type_alias) (top_level_variable_declaration) (external_variable_declaration)] @symbol)
(function_signature) @symbol
(getter_signature) @symbol
(setter_signature) @symbol
(operator_signature) @symbol
(constructor_signature) @symbol
(constant_constructor_signature) @symbol
(factory_constructor_signature) @symbol
(redirecting_factory_constructor_signature) @symbol
(class_body (_ (declaration) @symbol))
(enum_body (_ (declaration) @symbol))
(extension_body (_ (declaration) @symbol))
"#;

// HCL states a declaration as a block carrying quoted labels — `variable "x"`,
// `resource "type" "name"` — at a file's top level, where Terraform addresses
// it by them. A nested block configures the block holding it, labeled or not:
// `dynamic "ingress"` and `provisioner "local-exec"` name the kind of block
// they make, as `backend "gcs"` names a backend's type. Which attributes
// declare is not a shape (see `hcl_attribute_declares`), so they are all
// offered and read there.
const TERRAFORM_QUERY: &str = r#"
(config_file (body (block (string_lit)) @symbol))
(body (attribute) @symbol)
"#;

const PHP_QUERY: &str = r#"
(namespace_definition) @symbol
(function_definition) @symbol
(class_declaration) @symbol
(program (const_declaration) @symbol)
(namespace_definition body: (compound_statement (const_declaration) @symbol))
(declaration_list (const_declaration) @symbol)
(enum_declaration_list (const_declaration) @symbol)
(enum_declaration) @symbol
(enum_declaration_list (enum_case) @symbol)
(interface_declaration) @symbol
(trait_declaration) @symbol
(method_declaration) @symbol
(property_declaration) @symbol
(formal_parameters (property_promotion_parameter) @symbol)
"#;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn declaration_visibility_uses_syntax_and_name_identity() {
        type VisibilityCase = (
            Language,
            &'static str,
            &'static [(&'static str, Option<bool>)],
        );
        let cases: &[VisibilityCase] = &[
            (
                Language::Rust,
                "fn hidden() { let _ = \"pub \"; }\npub\nfn visible() { let _ = \"pub(crate)\"; }\npub(crate) fn restricted() {}\nmod inner { pub fn member() {} }",
                &[
                    ("hidden", Some(false)),
                    ("visible", Some(true)),
                    ("restricted", Some(false)),
                    ("member", Some(true)),
                ],
            ),
            (
                Language::Go,
                "package p\nvar Exported, hidden = 1, 2\nconst Number = 1\ntype T struct { Field int; private int }\nfunc(receiver *T) Method(){}\nfunc scope(){ type Local int; type local struct { InnerField int } }\nvar ᴬmodifier = 0\nvar Kelvin = 0\nvar 𝔄math = 0\n",
                &[
                    ("Exported", Some(true)),
                    ("hidden", Some(false)),
                    ("Number", Some(true)),
                    ("Field", Some(true)),
                    ("private", Some(false)),
                    ("Method", Some(true)),
                    ("Local", Some(false)),
                    ("local", Some(false)),
                    ("InnerField", Some(true)),
                    ("ᴬmodifier", Some(false)),
                    ("Kelvin", Some(true)),
                    ("𝔄math", Some(true)),
                ],
            ),
            (
                Language::TypeScript,
                "export\nclass Api { method(){ return \"export \"; } } function hidden(){ return \"export \"; } export function\nvisible(){}",
                &[
                    ("Api", None),
                    ("method", Some(false)),
                    ("hidden", None),
                    ("visible", Some(true)),
                ],
            ),
            (
                Language::TypeScript,
                "export class C { constructor(private dep: string) {} }\nexport enum Color { Red, Green = 2 }",
                &[
                    ("C/dep", Some(false)),
                    ("Color/Red", Some(false)),
                    ("Color/Green", Some(false)),
                ],
            ),
            (
                Language::Java,
                "enum E { Value }",
                &[("E/Value", Some(true))],
            ),
            (
                Language::Java,
                "public record R(int a) {\n    public int doubled() { return a * 2; }\n    private static int helper() { return 1; }\n}",
                &[
                    ("R/a", None),
                    ("R/doubled", Some(true)),
                    ("R/helper", Some(false)),
                ],
            ),
            (
                Language::CSharp,
                "public record R(int A) {\n    private int _cache;\n    private static void Helper() {}\n}",
                &[
                    ("R/A", Some(true)),
                    ("R/_cache", Some(false)),
                    ("R/Helper", Some(false)),
                ],
            ),
            (
                Language::Rust,
                "pub enum Shape {\n    Circle { radius: f64 },\n    Square(f64),\n}",
                &[("Circle/radius", None)],
            ),
            (
                Language::CSharp,
                "enum E { Value }",
                &[("E/Value", Some(true))],
            ),
            (
                Language::JavaScript,
                "function hidden(){ return \"export \"; } export function\nvisible(){}",
                &[("hidden", None), ("visible", Some(true))],
            ),
            (
                Language::Java,
                "public interface Api { void run(); private void hidden(){ String s = \"public \"; } int LIMIT = 1; }\n@interface Tag { String value(); }\nclass C { public\nvoid visible(){} void local(){} }",
                &[
                    ("run", Some(true)),
                    ("LIMIT", Some(true)),
                    ("value", Some(true)),
                    ("hidden", Some(false)),
                    ("visible", Some(true)),
                    ("local", Some(false)),
                ],
            ),
            (
                Language::CSharp,
                "interface Api { void Run(); void Default(){} } class C { private void Hidden(){ var s = \"public \"; } public\nvoid Visible(){ int Local() => 1; } public event System.Action Changed; event System.Action Raised { add {} remove {} } }",
                &[
                    ("Run", Some(true)),
                    ("Default", None),
                    ("Hidden", Some(false)),
                    ("Visible", Some(true)),
                    ("Local", Some(false)),
                    ("Changed", Some(true)),
                    ("Raised", Some(false)),
                ],
            ),
            (
                Language::Kotlin,
                "private fun hidden() = \"public \"\nfun visible() {}\nopen class Base {\n    protected open fun work() {}\n}\nclass Derived: Base() {\n    override fun work() {}\n}\n",
                &[
                    ("hidden", Some(false)),
                    ("visible", Some(true)),
                    ("Base/work", Some(false)),
                    ("Derived/work", None),
                ],
            ),
            (
                Language::Kotlin,
                "class SingleLine { fun work() {} }",
                &[("work", None)],
            ),
            (
                Language::Python,
                "def hidden():\n    return 'public export pub '\n",
                &[("hidden", None)],
            ),
        ];
        let extractor = SymbolExtractor::new();
        for (language, source, expectations) in cases {
            let path = Path::new("fixture");
            let symbols = extractor.extract(path, source, *language);
            for (name, expected) in *expectations {
                let symbol = symbols
                    .iter()
                    .find(|symbol| {
                        symbol.name == *name || symbol.name_path.as_deref() == Some(*name)
                    })
                    .unwrap_or_else(|| panic!("{language:?} did not declare {name}: {symbols:?}"));
                assert_eq!(
                    extractor.declaration_visibility(path, source, *language, symbol),
                    *expected,
                    "{language:?} {name}: {}",
                    extractor
                        .languages
                        .get(language)
                        .unwrap()
                        .parser
                        .lock()
                        .unwrap()
                        .parse(source, None)
                        .unwrap()
                        .root_node()
                        .to_sexp()
                );
            }
        }
        let path = Path::new("fixture.rs");
        let source = "pub fn visible() {}";
        let target = extractor.extract(path, source, Language::Rust).remove(0);
        assert_eq!(
            extractor.declaration_visibility(path, "pub fn changed() {}", Language::Rust, &target),
            None
        );
        assert_eq!(
            extractor.declaration_visibility(path, "pub fn visible( {", Language::Rust, &target),
            None
        );
        let package = Symbol::new(
            "Upper".to_string(),
            SymbolKind::Module,
            Location::point(path.to_path_buf(), 1, 9),
        );
        assert_eq!(
            extractor.declaration_visibility(path, "package Upper\n", Language::Go, &package),
            None,
            "a Go package has no visibility declaration in this registry"
        );
    }

    /// What a language declares, written from the LANGUAGE rather than from
    /// the query that reads it.
    ///
    /// Every other extraction test states the forms its query already
    /// captures, so a declaration form nobody thought of is invisible to all
    /// of them — which is how abstract classes, records, unions, namespaces
    /// and trait method signatures went missing at once. A source here is a
    /// tour of one grammar's declaration forms, and `declares` is the whole
    /// answer, so a form that stops being extracted fails rather than
    /// quietly narrowing what a search speaks for.
    ///
    /// The set is types, callables, and the module/namespace declarations
    /// that hold them — the categories every language here already claims.
    /// Which named VALUES a language admits differs between them and is
    /// recorded as each one behaves, not levelled.
    struct DeclarationFixture {
        language: Language,
        path: &'static str,
        source: &'static str,
        declares: &'static [&'static str],
    }

    const DECLARATION_FIXTURES: &[DeclarationFixture] = &[
        DeclarationFixture {
            language: Language::Cpp,
            path: "opened.h",
            source: r#"
#ifdef __cplusplus
extern "C" {
#endif
int opened_for_cpp(void);
"#,
            declares: &["function:opened_for_cpp"],
        },
        DeclarationFixture {
            language: Language::CSharp,
            path: "b.cs",
            source: r#"
namespace F;

class K {
    K() {}
}
"#,
            declares: &["namespace:F", "class:K", "constructor:K/K"],
        },
        DeclarationFixture {
            language: Language::PHP,
            path: "b.php",
            source: r#"
<?php
namespace B {
    const IN_B = 1;
}
"#,
            declares: &["namespace:B", "constant:IN_B"],
        },
        DeclarationFixture {
            language: Language::Rust,
            path: "a.rs",
            source: r#"
pub trait T { type Out; fn tm(&self); }
pub union U { a: u32 }
macro_rules! mac { () => {} }
pub type Al = u32;
pub const CX: u32 = 1;
pub static ST: u32 = 2;
pub enum E { A, B { b_field: u32 } }
pub mod inner { pub fn nested() {} }
pub struct S { pub inner_field: u32 }
impl S { pub fn m(&self) { let local = 1; } pub fn new() -> Self { S { inner_field: 0 } } }
impl S { fn boxed(self: Box<Self>) {} }
fn factory() { struct Made; impl Made { fn made(&self) {} } }
"#,
            declares: &[
                "interface:T",
                "class:T/Out",
                "method:T/tm",
                "struct:U",
                "function:mac",
                "class:Al",
                "constant:CX",
                "variable:ST",
                "enum:E",
                "enum_member:E/A",
                "enum_member:E/B",
                "field:B/b_field",
                "field:U/a",
                "module:inner",
                "function:nested",
                "struct:S",
                "field:S/inner_field",
                "object:S",
                "method:S/m",
                "function:S/new",
                "object:S",
                "method:S/boxed",
                "function:factory",
                "struct:factory/Made",
                "object:factory/Made",
                "method:Made/made",
            ],
        },
        DeclarationFixture {
            language: Language::Go,
            path: "a.go",
            source: r#"
package m

type Al = int
type S struct{ F int }
type I interface{ Do() }
type J interface {
	Run() error
	io.Reader
}
type E struct {
	pkg.Embedded
	*Ptr
	inner struct{ Z int }
}

const CX = 1
var VY = 2
var (
	VG = 1
	VH, VI = 2, 3
)

func F() {
	var local = 1
	const limit = 2
	_ = local + limit
}
func (s *S) M() {}
"#,
            declares: &[
                "class:Al",
                "struct:S",
                "field:S/F",
                "interface:I",
                "method:I/Do",
                "interface:J",
                "method:J/Run",
                "struct:E",
                "field:E/Embedded",
                "field:E/Ptr",
                "field:E/inner",
                "field:inner/Z",
                "constant:CX",
                "variable:VY",
                "variable:VG",
                "variable:VH",
                "variable:VI",
                "function:F",
                "method:M",
            ],
        },
        DeclarationFixture {
            language: Language::Python,
            path: "a.py",
            source: r#"
MODULE_CONST = 2
annotated: int = 3
bare_ann: int
first, second = pair()
obj.attr = value
if TYPE_CHECKING:
    GUARDED = 4
type Alias = list[int]

class A:
    attr: int = 1
    try:
        tried = 2
    except ImportError:
        pass

    def m(self):
        local = 5
        if local:
            nested = 6
        return local

def f(): pass

async def af(): pass

try:
    HAS_JSON = True
except ImportError:
    HAS_JSON = False

def scoped():
    type LocalAlias = int
    class Made:
        def made(self):
            def helper(): pass
"#,
            declares: &[
                "variable:MODULE_CONST",
                "variable:annotated",
                "variable:bare_ann",
                "variable:first",
                "variable:second",
                "variable:GUARDED",
                "class:Alias",
                "class:A",
                "variable:A/attr",
                "variable:A/tried",
                "method:A/m",
                "function:f",
                "function:af",
                "variable:HAS_JSON",
                "function:scoped",
                "class:scoped/LocalAlias",
                "class:scoped/Made",
                "method:Made/made",
                "function:made/helper",
            ],
        },
        DeclarationFixture {
            language: Language::TypeScript,
            path: "a.ts",
            source: r#"
export abstract class Abs { abstract doIt(): void; }
export namespace NS {}
export interface Shape {
    area(): number;
    readonly id: string;
}
export type Alias = string;
export enum Color { Red, Green = 2 }
export class C {
    static SC = 1;
    private priv: number = 2;
    constructor(private readonly dep: string, plain: number) {}
    m(): void {}
}
export function f(): void {}
export const arrow = () => 1;
export const MAX = 3;
let counter = 0;
const { picked, renamed: alias, ...others } = source;
const {} = unused;
export function over(a: string): string;
export function over(a: any): any { return a; }
declare module "ext" { export function fromExt(): void; }
namespace Inner { export const held = 1; }
function* gen() {}
var legacy = 1;
export var exported = 2;
declare const ambientConst: number;
declare var ambientVar: number;
namespace Plain { const inPlain = 1; }
declare module "bare" { const inBare: number; }
export declare class Decl {
    method(a: string): void;
    static create(): Decl;
    constructor(x: number);
}
"#,
            declares: &[
                "class:Abs",
                "method:Abs/doIt",
                "module:NS",
                "interface:Shape",
                "method:Shape/area",
                "property:Shape/id",
                "class:Alias",
                "enum:Color",
                "enum_member:Color/Red",
                "enum_member:Color/Green",
                "class:C",
                "property:C/SC",
                "property:C/priv",
                "constructor:C/constructor",
                "property:C/dep",
                "method:C/m",
                "function:f",
                "function:arrow",
                "constant:MAX",
                "variable:counter",
                "constant:picked",
                "constant:alias",
                "constant:others",
                "function:over",
                "function:over",
                "module:\"ext\"",
                "function:fromExt",
                "module:Inner",
                "constant:held",
                "function:gen",
                "variable:legacy",
                "variable:exported",
                "constant:ambientConst",
                "variable:ambientVar",
                "module:Plain",
                "constant:inPlain",
                "module:\"bare\"",
                "constant:inBare",
                "class:Decl",
                "method:Decl/method",
                "method:Decl/create",
                "constructor:Decl/constructor",
            ],
        },
        DeclarationFixture {
            language: Language::JavaScript,
            path: "a.js",
            source: r#"
export class C {
    static SC = 1;
    inst = 2;
    #hidden = 3;
    m() {}
}
export function f() {}
export const arrow = () => 1;
function* gen() {}
export const MAX = 3;
let counter = 0;
const { picked } = source;
var legacy = 1;
export var exported = 2;
"#,
            declares: &[
                "class:C",
                "property:C/SC",
                "property:C/inst",
                "property:C/#hidden",
                "method:C/m",
                "function:f",
                "function:arrow",
                "function:gen",
                "constant:MAX",
                "variable:counter",
                "constant:picked",
                "variable:legacy",
                "variable:exported",
            ],
        },
        DeclarationFixture {
            language: Language::Java,
            path: "A.java",
            source: r#"
package p;

record R(int a) {
    R {}
}

@interface Ann { String value() default ""; }

interface I { void im(); int VERSION = 1; }

enum E { X, Y }

class C {
    int field;
    C() {}
    void m() {}
}
"#,
            declares: &[
                "package:p",
                "class:R",
                "field:R/a",
                "constructor:R/R",
                "interface:Ann",
                "method:Ann/value",
                "interface:I",
                "method:I/im",
                "field:I/VERSION",
                "enum:E",
                "enum_member:E/X",
                "enum_member:E/Y",
                "class:C",
                "field:C/field",
                "constructor:C/C",
                "method:C/m",
            ],
        },
        DeclarationFixture {
            language: Language::CSharp,
            path: "a.cs",
            source: r#"
namespace N {
    public record Rec(int A);
    public delegate void D();
    public interface I { void M(); }
    public struct St { public int F; }
    public enum E { X }
    class C {
        public int P { get; set; }
        public event System.EventHandler Changed;
        event System.EventHandler Raised { add {} remove {} }
        void M() { int Local(int x) { return x; } }
        public static C operator +(C a, C b) => a;
        public static implicit operator int(C c) => 0;
        public int this[int i] => i;
        ~C() {}
    }
}
"#,
            declares: &[
                "namespace:N",
                "class:Rec",
                "property:Rec/A",
                "class:D",
                "interface:I",
                "method:I/M",
                "struct:St",
                "field:St/F",
                "enum:E",
                "enum_member:E/X",
                "class:C",
                "property:C/P",
                "event:C/Changed",
                "event:C/Raised",
                "method:C/M",
                "function:M/Local",
                "method:C/+",
                "method:C/operator int",
                "property:C/this",
                "constructor:C/~C",
            ],
        },
        DeclarationFixture {
            language: Language::PHP,
            path: "a.php",
            source: r#"
<?php
namespace N;

const TOP = 0;

enum E { case X; const EX = 1; }

trait T { const TX = 2; public function tm() {} }

interface I { const IX = 3; public function im(); }

abstract class A {
    const AX = 1;
    public $prop;
    abstract public function am();
    public function __construct(private int $promoted, $plain) {}
}

function f() {}
"#,
            declares: &[
                "namespace:N",
                "constant:TOP",
                "enum:E",
                "enum_member:E/X",
                "constant:E/EX",
                "interface:T",
                "constant:T/TX",
                "method:T/tm",
                "interface:I",
                "constant:I/IX",
                "method:I/im",
                "class:A",
                "constant:A/AX",
                "property:A/prop",
                "method:A/am",
                "method:A/__construct",
                "property:A/promoted",
                "function:f",
            ],
        },
        DeclarationFixture {
            language: Language::Kotlin,
            path: "a.kt",
            source: r#"
package demo

interface I {
    fun im()
}

object Registry {
    fun register() {}
}

enum class Color { RED, GREEN; val hex = 0 }

data class User(val id: String, plain: Int)

class Widget {
    val name = "w"
    constructor(name: String) : this()
    fun render() {
        val local = 1
    }
    companion object Factory {
        fun create() = Widget()
    }
}

class Plain {
    companion object {
        fun make() = Plain()
    }
}

fun top() {}

typealias Name = String

class OneLine { val x = 1 }

enum class Flag { ON; val z = 1 }
"#,
            declares: &[
                "class:I",
                "method:I/im",
                "class:Registry",
                "method:Registry/register",
                "class:Color",
                "enum_member:Color/RED",
                "enum_member:Color/GREEN",
                "property:Color/hex",
                "class:User",
                "property:User/id",
                "class:Widget",
                "property:Widget/name",
                "constructor:Widget/constructor",
                "method:Widget/render",
                "class:Widget/Factory",
                "method:Factory/create",
                "class:Plain",
                "method:Plain/make",
                "function:top",
                "class:Name",
                "class:OneLine",
                "property:OneLine/x",
                "class:Flag",
                "enum_member:Flag/ON",
                "property:Flag/z",
            ],
        },
        DeclarationFixture {
            language: Language::Cpp,
            path: "a.cpp",
            source: r#"
namespace N {

struct S { int f; enum { Anon } kind; };

class C {
public:
    C();
    explicit C(int n);
    ~C();
    DISALLOW_COPY(C);
    void m();
    int& at(int i);
    const char* name() const;
    int (*callback)(int);
    operator bool() const;
    template <typename U>
    U make();
    friend void befriended();
    void (C::*handler)();
    typedef int Id;
    C(const C&) = default;
    virtual ~C() = default;
    void inline_method() {}
};

C::operator bool() const { return true; }

enum E { X };

void f() {}

int in_namespace = 0;

}

#ifndef A_H
#define A_H
int parse(const char *s);
extern int counter;
typedef unsigned long size_type;
using Handle = int;
union U { int a; };
extern "C" { int c_linked(void); }
template <class T> T largest(T a, T b);
typedef struct { int pa; } Pair;
extern "C" int c_single(void);
struct P { P(); };
#if defined(X)
int in_if;
#elif defined(Y)
int in_elif;
#else
int in_else;
#endif
#ifdef Z
int in_ifdef;
#elifdef W
int in_elifdef;
#endif
#endif

int unguarded = 0;

struct Forward;
static int uses(struct Forward *f, enum Mode m) { struct stat st; return 0; }

void g() {
#ifdef Q
    int local = 1;
#endif
    auto [l1, l2] = pair_value;
    namespace lf = N;
}

class EXPORT Exported {
    int size() const;
    int count_;
    int inline_count() const { return count_; }
};

namespace fsys = N;
namespace outer::inner::innermost { int nested_var; }
template <class T> concept Small = sizeof(T) < 4;
auto [first_of, second_of] = pair_value;
"#,
            declares: &[
                "namespace:N",
                "struct:S",
                "field:S/f",
                "field:S/kind",
                "enum_member:S/Anon",
                "class:C",
                "constructor:C/C",
                "constructor:C/C",
                "constructor:C/~C",
                "constructor:C/C",
                "constructor:C/~C",
                "method:C/inline_method",
                "method:C/m",
                "method:C/at",
                "method:C/name",
                "field:C/callback",
                "field:C/handler",
                "method:C/operator bool",
                "method:C/make",
                "function:C::operator bool",
                "class:C/Id",
                "enum:E",
                "enum_member:E/X",
                "function:f",
                "variable:in_namespace",
                "function:parse",
                "variable:counter",
                "class:size_type",
                "class:Handle",
                "struct:U",
                "field:U/a",
                "function:c_linked",
                "function:largest",
                "class:Pair",
                "field:Pair/pa",
                "function:c_single",
                "struct:P",
                "constructor:P/P",
                "variable:in_if",
                "variable:in_elif",
                "variable:in_else",
                "variable:in_ifdef",
                "variable:in_elifdef",
                "variable:unguarded",
                "struct:Forward",
                "function:uses",
                "function:g",
                "class:Exported",
                "method:Exported/size",
                "field:Exported/count_",
                "method:Exported/inline_count",
                "namespace:fsys",
                "namespace:outer",
                "namespace:inner",
                "namespace:innermost",
                "variable:nested_var",
                "variable:Small",
                "variable:first_of",
                "variable:second_of",
            ],
        },
        DeclarationFixture {
            language: Language::Ruby,
            path: "a.rb",
            source: r#"
TOP_CONST = 1
obj.attr = value

module M
  IN_MODULE = 2
  CACHE ||= {}

  class C
    IN_CLASS = 3
    @@count = 0
    @@count += 1
    @registry = {}

    def m; end
    def self.cm; end
  end
end
"#,
            declares: &[
                "variable:TOP_CONST",
                "module:M",
                "variable:IN_MODULE",
                "variable:CACHE",
                "class:C",
                "variable:C/IN_CLASS",
                "variable:C/@@count",
                "method:C/m",
                "method:C/cm",
            ],
        },
        DeclarationFixture {
            language: Language::Bash,
            path: "a.sh",
            source: r#"
greet() { echo hi; }

function other { echo hi; }
"#,
            declares: &["function:greet", "function:other"],
        },
        DeclarationFixture {
            language: Language::Lua,
            path: "a.lua",
            source: r#"
local M = {}
M.version = 1

function top() end

local function helper()
  local hidden = 1
end

local handler = function(event) end

function M.run() end

M.stop = function() end

M.count, M.reset = 0, function() end

if M then
  local scoped = 1
end

return M
"#,
            declares: &[
                "variable:M",
                "function:top",
                "function:helper",
                "function:handler",
                "function:M.run",
                "function:M.stop",
                "function:M.reset",
            ],
        },
        DeclarationFixture {
            language: Language::Swift,
            path: "a.swift",
            source: r#"
protocol P {
    associatedtype Item
    var required: Int { get }
    func pm()
}

class C {
    var p = 1
    typealias Pair = (Int, Int)
    init() {}
    deinit {}
    func cm() {}
}

enum Dir {
    case up, down
    case left(Int)
    var opposite: Dir { self }
}

struct Point {
    var x = 0
}

extension Point {
    func moved() {}
}

func top() {}

let global = 1
let (gx, (gy, _)) = (1, (2, 3))
"#,
            declares: &[
                "interface:P",
                "class:P/Item",
                "property:P/required",
                "method:P/pm",
                "class:C/Pair",
                "constructor:C/init",
                "constructor:C/deinit",
                "struct:Point",
                "property:Point/x",
                "object:Point",
                "method:Point/moved",
                "enum:Dir",
                "enum_member:Dir/up",
                "enum_member:Dir/down",
                "enum_member:Dir/left",
                "property:Dir/opposite",
                "class:C",
                "property:C/p",
                "method:C/cm",
                "function:top",
                "variable:global",
                "variable:gx",
                "variable:gy",
            ],
        },
        DeclarationFixture {
            language: Language::Scala,
            path: "a.scala",
            source: r#"
trait T {
  def tm(): Unit
  val tv: Int
  var tw: Int
}

object O {
  val v = 1
  val (pa, pb: Int) = (1, 2)
  val Some(inner) = opt
  val whole @ Some(_) = opt
  val head :: tail = items
  val _ = sideEffect
  var m, n = 0
  type Id = Int
  given ord: Ordering[Int] = Ordering.Int
  given rank[A](using o: Ordering[A]): Ordering[List[A]] = ???
  given shown: Show[Int] with {
    val prefix = ""
  }
}

type Top = Int
val top = 1
given topGiven: Int = 1

class C {
  def cm(): Unit = {}
}

enum Color(val rgb: Int) {
  case Red, Green
  case Mix(r: Int)
}

case class P(x: Int)(z: Int)

class Q(val a: Int, b: Int)

package object pkg {
  val pv = 1
  def pf(): Int = 1
}
"#,
            declares: &[
                "enum:Color",
                "property:Color/rgb",
                "enum_member:Color/Red",
                "enum_member:Color/Green",
                "enum_member:Color/Mix",
                "class:P",
                "property:P/x",
                "class:Q",
                "property:Q/a",
                "interface:T",
                "method:T/tm",
                "constant:T/tv",
                "variable:T/tw",
                "class:O",
                "constant:O/ord",
                "method:O/rank",
                "class:O/shown",
                "constant:shown/prefix",
                "constant:O/v",
                "constant:O/pa",
                "constant:O/pb",
                "constant:O/inner",
                "constant:O/whole",
                "constant:O/head",
                "constant:O/tail",
                "variable:O/m",
                "variable:O/n",
                "class:O/Id",
                "class:Top",
                "constant:top",
                "constant:topGiven",
                "class:C",
                "method:C/cm",
                "class:pkg",
                "constant:pkg/pv",
                "method:pkg/pf",
            ],
        },
        DeclarationFixture {
            language: Language::Dart,
            path: "a.dart",
            source: r#"
mixin M {}

enum E {
  x(1);
  final int weight;
  const E(this.weight);
}

abstract class A {
  void am();
  abstract int af, ag;
  external int ef;
}

class C {
  int field = 1;
  static const sc = 2;
  C();
  C.named();
  factory C.make() => C();
  factory C.alias() = C.named;
  void cm() {
    void inner() {}
  }
  int get size => 1;
  set size(int v) {}
  bool operator ==(Object o) => true;
}

extension Ext on String {
  static const limit = 1;
  void ext() {}
}

extension type Meters(int value) {}

void top() {
  int helper() => 1;
}

final topLevel = 1;

typedef Callback = void Function();

external final int topExternal, otherExternal;
"#,
            declares: &[
                "interface:M",
                "enum:E",
                "enum_member:E/x",
                "field:E/weight",
                "constructor:E/E",
                "class:A",
                "method:A/am",
                "field:A/af",
                "field:A/ag",
                "field:A/ef",
                "class:C",
                "field:C/field",
                "field:C/sc",
                "constructor:C/C",
                "constructor:C/C.named",
                "constructor:C/C.make",
                "constructor:C/C.alias",
                "method:C/cm",
                "function:cm/inner",
                "property:C/size",
                "property:C/size",
                "method:C/==",
                "class:Ext",
                "field:Ext/limit",
                "method:Ext/ext",
                "class:Meters",
                "field:Meters/value",
                "function:top",
                "function:top/helper",
                "variable:topLevel",
                "class:Callback",
                "variable:topExternal",
                "variable:otherExternal",
            ],
        },
        DeclarationFixture {
            language: Language::Terraform,
            path: "a.tf",
            source: r#"
terraform {
  backend "gcs" {
    prefix = "state"
  }
}

provider "google" {
  region = "us-central1"
}

variable "project_id" {
  type = string
}

locals {
  name_prefix = "aix"
}

data "google_project" "current" {}

resource "google_storage_bucket" "logs" {
  lifecycle {
    prevent_destroy = true
  }
  dynamic "cors" {
    for_each = var.origins
    content {
      origin = [cors.value]
    }
  }
  provisioner "local-exec" {
    command = "echo created"
  }
}

module "network" {
  source = "./modules/network"
}

output "bucket_url" {
  value = google_storage_bucket.logs.url
}
"#,
            declares: &[
                "struct:google",
                "property:project_id",
                "property:name_prefix",
                "struct:google_project/current",
                "struct:google_storage_bucket/logs",
                "module:network",
                "property:bucket_url",
            ],
        },
        DeclarationFixture {
            language: Language::Terraform,
            path: "a.tfvars",
            source: r#"
project_id = "aix"
region     = "us-central1"
"#,
            declares: &["property:project_id", "property:region"],
        },
    ];

    /// Pins the static `supported_languages` answer to runtime registration:
    /// a grammar bump that breaks one language's ABI or extraction query
    /// fails here loudly instead of degrading to silent empty results.
    #[test]
    fn every_supported_language_registers_an_extractor() {
        let extractor = SymbolExtractor::new();
        for language in SymbolExtractor::supported_languages() {
            assert!(
                extractor.languages.contains_key(language),
                "{language:?} failed to register — its grammar ABI or extraction query is broken"
            );
        }
        assert_eq!(
            extractor.languages.len(),
            SymbolExtractor::supported_languages().len()
        );
    }

    #[test]
    fn every_declaration_form_a_fixture_writes_is_extracted() {
        let extractor = SymbolExtractor::new();
        for fixture in DECLARATION_FIXTURES {
            let symbols =
                extractor.extract(Path::new(fixture.path), fixture.source, fixture.language);
            let mut found: Vec<String> = symbols
                .iter()
                .map(|s| format!("{}:{}", s.kind, s.name_path.as_deref().unwrap_or(&s.name)))
                .collect();
            let mut expected: Vec<String> =
                fixture.declares.iter().map(|d| d.to_string()).collect();
            found.sort();
            expected.sort();
            assert_eq!(
                found, expected,
                "{:?} extraction does not match what {} declares",
                fixture.language, fixture.path
            );
        }
    }

    /// A language whose extractor no fixture describes is one whose gaps
    /// nothing can find, so registering a grammar and writing its tour are
    /// one step.
    /// `file:line:col` on a symbol has to land on the symbol's name — it is
    /// what `refs`, `def` and `edit` resolve from, and a position on a leading
    /// keyword resolves to something else or to nothing. The name is read
    /// through one path and the anchor through another, so each language's
    /// tour of declaration forms is also what proves the two agree.
    #[test]
    fn every_extracted_symbol_is_anchored_at_its_name() {
        let extractor = SymbolExtractor::new();
        for fixture in DECLARATION_FIXTURES {
            for symbol in
                extractor.extract(Path::new(fixture.path), fixture.source, fixture.language)
            {
                let line = fixture
                    .source
                    .lines()
                    .nth((symbol.location.line - 1) as usize)
                    .unwrap_or_else(|| {
                        panic!(
                            "{} line {} is past the source",
                            fixture.path, symbol.location.line
                        )
                    });
                let column = (symbol.location.column - 1) as usize;
                let at_name = line
                    .chars()
                    .skip(column)
                    .take(symbol.name.chars().count())
                    .eq(symbol.name.chars());
                assert!(
                    at_name,
                    "{:?} anchors {} off its name: {}:{} is {line:?}",
                    fixture.language, symbol.name, symbol.location.line, symbol.location.column
                );
            }
        }
    }

    /// A pattern that matches nothing reads nothing, and passes every test
    /// that checks only what was read: `([(a) (b)] body: …)` is two sibling
    /// patterns, not a parent holding a body. Each pattern of a language's
    /// query reads a form its fixtures write.
    #[test]
    fn every_query_pattern_reads_a_form_its_fixtures_write() {
        let extractor = SymbolExtractor::new();
        let mut unread = Vec::new();
        for (language, _, query_src) in EXTRACTORS {
            let query = &extractor.languages[language].query;
            let mut matched = vec![false; query.pattern_count()];
            for fixture in DECLARATION_FIXTURES
                .iter()
                .filter(|fixture| fixture.language == *language)
            {
                let tree = extractor.parse(fixture.source, *language).unwrap();
                let mut cursor = QueryCursor::new();
                let mut matches =
                    cursor.matches(query, tree.root_node(), fixture.source.as_bytes());
                while let Some(found) = matches.next() {
                    matched[found.pattern_index] = true;
                }
            }
            unread.extend(
                (0..query.pattern_count())
                    .filter(|&pattern| !matched[pattern])
                    .map(|pattern| {
                        let text = query_src[query.start_byte_for_pattern(pattern)..]
                            .lines()
                            .next()
                            .unwrap_or_default();
                        format!("{language:?}: {text}")
                    }),
            );
        }
        assert!(
            unread.is_empty(),
            "patterns no fixture form matches: {unread:#?}"
        );
    }

    #[test]
    fn every_extractor_language_has_a_declaration_fixture() {
        for language in SymbolExtractor::supported_languages() {
            assert!(
                DECLARATION_FIXTURES
                    .iter()
                    .any(|fixture| fixture.language == *language),
                "{language:?} extracts symbols with no fixture saying which forms it must reach"
            );
        }
    }

    /// The grammars in the binary that read no declarations, and why. Elixir
    /// states every one of them as a generic `call`, so which calls declare is
    /// its macro vocabulary rather than anything the grammar marks — and a
    /// `def` written inside `quote` is the same node as one that declares.
    const NO_EXTRACTOR: &[Language] = &[Language::Elixir];

    /// Extraction reads a grammar's declaration nodes, so it can never reach a
    /// language AST search does not parse. Which of those it does reach is a
    /// decision, and every grammar in the binary makes it here: a new one
    /// either extracts declarations or is named above with the reason it
    /// cannot. Left implicit, a grammar added for `search ast` answers
    /// `symbols` and `search symbols` with nothing and says only that its
    /// language server is missing.
    #[test]
    fn every_parsed_language_decides_whether_it_extracts() {
        let extracts = SymbolExtractor::supported_languages();
        let parsed = crate::infra::ast::node_types::supported_languages();

        for language in extracts {
            assert!(
                parsed.contains(language),
                "{language:?} extracts symbols from a grammar AST search does not parse"
            );
            assert!(
                !NO_EXTRACTOR.contains(language),
                "{language:?} both extracts declarations and declines to"
            );
        }
        for language in parsed {
            assert!(
                extracts.contains(language) || NO_EXTRACTOR.contains(language),
                "{language:?} parses for AST search but neither extracts declarations nor \
                 says why it cannot"
            );
        }
    }

    /// Every position the extractor emits is a character column, not a byte
    /// offset — the two agree for ASCII, so only a line carrying multibyte
    /// text before a symbol can tell a missed conversion from a correct one.
    /// A wrong column here misplaces every `file:line:col` taken from it.
    #[test]
    fn positions_are_character_columns_on_every_span() {
        let extractor = SymbolExtractor::new();
        let content = "class 주문Handler:\n    def 처리(self):\n        return 1\n";
        let symbols = extractor.extract(Path::new("a.py"), content, Language::Python);

        let class = symbols.iter().find(|s| s.name == "주문Handler").unwrap();
        assert_eq!((class.location.line, class.location.column), (1, 7));
        assert_eq!(class.location.name_end_column, Some(16));

        let method = symbols.iter().find(|s| s.name == "처리").unwrap();
        assert_eq!((method.location.line, method.location.column), (2, 9));
        assert_eq!(method.location.range_start_column, Some(5));
        assert_eq!(method.location.end_line, Some(3));

        let mut with_body = vec![method.clone()];
        Symbol::attach_bodies(&mut with_body, content);
        assert_eq!(
            with_body[0].body.as_deref(),
            Some("    def 처리(self):\n        return 1")
        );
    }

    #[test]
    fn a_declaration_opens_on_the_syntax_that_belongs_to_it() {
        let extractor = SymbolExtractor::new();
        let start = |path: &str, content: &str, language: Language, name: &str| {
            extractor
                .extract(Path::new(path), content, language)
                .into_iter()
                .find(|symbol| symbol.name == name)
                .map(|symbol| symbol.location.effective_start())
                .unwrap()
        };

        let rust = "//! The crate.\n\n// Above.\n/// Doubles.\n// Between.\n#[inline]\n\
                    pub fn double(x: u32) -> u32 {\n    x * 2\n}\n\nimpl S {\n    /// Size.\n    \
                    fn len(&self) -> usize {\n        0\n    }\n}\n";
        assert_eq!(start("a.rs", rust, Language::Rust, "double"), (4, 1));
        assert_eq!(start("a.rs", rust, Language::Rust, "len"), (12, 5));

        let python = "# Above.\n@first\n@second(1)\ndef run():\n    pass\n\n\n@dataclass\n\
                      class Row:\n    pass\n";
        assert_eq!(start("a.py", python, Language::Python, "run"), (2, 1));
        assert_eq!(start("a.py", python, Language::Python, "Row"), (8, 1));

        let typescript = "class Svc {\n  // Above.\n  @log(\"a\")\n  stop() { return 1; }\n}\n\n\
                          @Component()\nexport class Top {}\n\nexport function go() {}\n";
        assert_eq!(
            start("a.ts", typescript, Language::TypeScript, "stop"),
            (3, 3)
        );
        assert_eq!(
            start("a.ts", typescript, Language::TypeScript, "Top"),
            (7, 1)
        );
        assert_eq!(
            start("a.ts", typescript, Language::TypeScript, "go"),
            (10, 1)
        );

        let dart = "class Shape {\n  @override\n  String toString() {\n    return \"s\";\n  }\n}\n\n\
                    int top() {\n  int local() {\n    return 1;\n  }\n  return local();\n}\n\n\
                    int get answer {\n  return 42;\n}\n";
        let spans = |name: &str| {
            extractor
                .extract(Path::new("a.dart"), dart, Language::Dart)
                .into_iter()
                .find(|symbol| symbol.name == name)
                .map(|symbol| (symbol.location.effective_start(), symbol.location.end_line))
                .unwrap()
        };
        assert_eq!(spans("toString"), ((2, 3), Some(5)));
        assert_eq!(spans("top"), ((8, 1), Some(13)));
        assert_eq!(spans("local"), ((9, 3), Some(11)));
        assert_eq!(spans("answer"), ((15, 1), Some(17)));
    }

    #[test]
    fn a_declaration_spans_the_statement_and_token_that_hold_it() {
        let extractor = SymbolExtractor::new();
        let spans = |path: &str, content: &str, language: Language| {
            extractor
                .extract(Path::new(path), content, language)
                .into_iter()
                .map(|symbol| {
                    let location = &symbol.location;
                    (
                        symbol.name.clone(),
                        location.effective_start(),
                        (location.end_line.unwrap(), location.end_column.unwrap()),
                    )
                })
                .collect::<Vec<_>>()
        };
        let span = |all: &[(String, (u32, u32), (u32, u32))], name: &str| {
            all.iter()
                .find(|(named, _, _)| named == name)
                .map(|(_, start, end)| (*start, *end))
                .unwrap_or_else(|| panic!("no {name} in {all:?}"))
        };

        let cpp = spans(
            "a.cpp",
            "template <typename T>\nT twice(T x) {\n    return x;\n}\n\n\
             extern \"C\" int cfun() {\n    return 2;\n}\n\n\
             extern \"C\" {\nint g() { return 1; }\n}\n\n\
             template <typename T>\nclass Box {\n    class Inner {};\n    \
             template <typename U>\n    U pick(U u) { return u; }\n};\n\n\
             struct S {\n    int a;\n};\n\nstruct T {\n    int b;\n} t;\n",
            Language::Cpp,
        );
        assert_eq!(span(&cpp, "twice"), ((1, 1), (4, 2)));
        assert_eq!(span(&cpp, "cfun"), ((6, 1), (8, 2)));
        assert_eq!(span(&cpp, "g"), ((11, 1), (11, 22)));
        assert_eq!(span(&cpp, "Box"), ((14, 1), (19, 3)));
        assert_eq!(span(&cpp, "Inner"), ((16, 5), (16, 20)));
        assert_eq!(span(&cpp, "pick"), ((17, 5), (18, 30)));
        assert_eq!(span(&cpp, "S"), ((21, 1), (23, 3)));
        assert_eq!(span(&cpp, "T"), ((25, 1), (27, 2)));
        let member = spans(
            "b.cpp",
            "class W {\n    template <typename U>\n    static U make();\n};\n",
            Language::Cpp,
        );
        assert_eq!(span(&member, "make"), ((2, 5), (3, 21)));

        let typescript = spans(
            "a.ts",
            "export const handler = async () => {\n  return 1;\n};\n\
             const a = () => 1, b = () => 2;\ndeclare class X {}\n\
             interface Runnable {\n  run(): void;\n  name: string,\n}\n\
             class Svc {\n  count = 0;\n}\n",
            Language::TypeScript,
        );
        assert_eq!(span(&typescript, "handler"), ((1, 1), (3, 3)));
        assert_eq!(span(&typescript, "a"), ((4, 7), (4, 18)));
        assert_eq!(span(&typescript, "b"), ((4, 20), (4, 31)));
        assert_eq!(span(&typescript, "X"), ((5, 1), (5, 19)));
        assert_eq!(span(&typescript, "run"), ((7, 3), (7, 15)));
        assert_eq!(span(&typescript, "name"), ((8, 3), (8, 16)));
        assert_eq!(span(&typescript, "count"), ((11, 3), (11, 13)));

        let rust = spans(
            "a.rs",
            "pub struct Store {\n    pub items: Vec<u32>,\n}\n\npub enum Mode {\n    Fast,\n    Slow\n}\n",
            Language::Rust,
        );
        assert_eq!(span(&rust, "items"), ((2, 5), (2, 25)));
        assert_eq!(span(&rust, "Fast"), ((6, 5), (6, 10)));
        assert_eq!(span(&rust, "Slow"), ((7, 5), (7, 9)));

        let go = spans(
            "a.go",
            "package p\n\ntype Foo struct {\n\tA int\n}\n\ntype (\n\tB int\n\tC int\n)\n\
             const X = 1\nvar y = 2\nconst (\n\tZ = 3\n)\n",
            Language::Go,
        );
        assert_eq!(span(&go, "Foo"), ((3, 1), (5, 2)));
        assert_eq!(span(&go, "B"), ((8, 2), (8, 7)));
        assert_eq!(span(&go, "X"), ((11, 1), (11, 12)));
        assert_eq!(span(&go, "y"), ((12, 1), (12, 10)));
        assert_eq!(span(&go, "Z"), ((14, 2), (14, 7)));
    }

    /// A constructor is a declaration of the type it constructs.
    #[test]
    fn a_constructor_is_declared_by_its_type() {
        let extractor = SymbolExtractor::new();
        for (path, language, content) in [
            (
                "J.java",
                Language::Java,
                "class J {\n  J() {}\n  J(int a) {}\n}\n",
            ),
            (
                "k.cs",
                Language::CSharp,
                "class J {\n  public J() {}\n  static J() {}\n}\n",
            ),
        ] {
            let constructors: Vec<(String, u32)> = extractor
                .extract(Path::new(path), content, language)
                .into_iter()
                .filter(|symbol| symbol.kind == SymbolKind::Constructor)
                .map(|symbol| {
                    (
                        symbol.name_path.unwrap_or(symbol.name),
                        symbol.location.line,
                    )
                })
                .collect();
            assert_eq!(
                constructors,
                [("J/J".to_string(), 2), ("J/J".to_string(), 3)],
                "{path}"
            );
        }
    }

    #[test]
    fn a_statement_declaring_several_names_is_the_whole_declaration_of_each() {
        let extractor = SymbolExtractor::new();
        for (path, language, content, several, one) in [
            (
                "a.go",
                Language::Go,
                "package p\n\nvar unused, shared = 1, 2\nvar one = 1\n",
                &["unused", "shared"][..],
                "one",
            ),
            (
                "a.go",
                Language::Go,
                "package p\n\nvar lo, hi = pick(\n\t1,\n\t2,\n)\nvar one = 1\n",
                &["lo", "hi"],
                "one",
            ),
            (
                "a.go",
                Language::Go,
                "package p\n\nconst lo, hi = 0, 9\nconst one = 1\n",
                &["lo", "hi"],
                "one",
            ),
            (
                "a.go",
                Language::Go,
                "package p\n\ntype S struct {\n\tA, B int\n\tC int\n}\n",
                &["A", "B"],
                "C",
            ),
            (
                "A.java",
                Language::Java,
                "class A {\n    int unused, shared;\n    int one;\n}\n",
                &["unused", "shared"],
                "one",
            ),
            (
                "a.cpp",
                Language::Cpp,
                "class A {\n    int unused, *shared;\n    int one;\n};\n",
                &["unused", "shared"],
                "one",
            ),
            (
                "a.cs",
                Language::CSharp,
                "class A {\n    public int unused, shared;\n    public int one;\n}\n",
                &["unused", "shared"],
                "one",
            ),
            (
                "a.py",
                Language::Python,
                "unused = shared = None\none = 1\n",
                &["unused", "shared"],
                "one",
            ),
            (
                "a.py",
                Language::Python,
                "unused, shared = 1, 2\none = 1\n",
                &["unused", "shared"],
                "one",
            ),
            (
                "a.py",
                Language::Python,
                "class A:\n    (unused, [shared, other]) = (1, [2, 3])\n    one = 1\n",
                &["unused", "shared", "other"],
                "one",
            ),
            (
                "a.py",
                Language::Python,
                "unused, *shared = items\none = 1\n",
                &["unused", "shared"],
                "one",
            ),
            (
                "a.py",
                Language::Python,
                "unused, shared = shared, other = 1, 2\none = 1\n",
                &["unused", "shared", "other"],
                "one",
            ),
            (
                "a.rb",
                Language::Ruby,
                "UNUSED = SHARED = 1\nONE = 1\n",
                &["UNUSED", "SHARED"],
                "ONE",
            ),
            (
                "a.rb",
                Language::Ruby,
                "UNUSED, (SHARED, OTHER) = 1, [2, 3]\nONE = 1\n",
                &["UNUSED", "SHARED", "OTHER"],
                "ONE",
            ),
            (
                "a.rb",
                Language::Ruby,
                "UNUSED, *SHARED = 1, 2\nONE = 1\n",
                &["UNUSED", "SHARED"],
                "ONE",
            ),
            (
                "a.php",
                Language::PHP,
                "<?php\nclass A {\n    public $unused, $shared;\n    public $one;\n}\n",
                &["unused", "shared"],
                "one",
            ),
            (
                "a.php",
                Language::PHP,
                "<?php\nclass A {\n    const X = 1, Y = 2;\n    const Z = 3;\n}\n",
                &["X", "Y"],
                "Z",
            ),
            (
                "a.swift",
                Language::Swift,
                "class A {\n  let unused = 1, shared = 2\n  let one = 1\n}\n",
                &["unused", "shared"],
                "one",
            ),
            (
                "a.dart",
                Language::Dart,
                "class A {\n  int unused = 1, shared = 2;\n  int one = 1;\n}\n",
                &["unused", "shared"],
                "one",
            ),
            (
                "a.dart",
                Language::Dart,
                "class A {\n  static const unused = 1, shared = 2;\n  static const one = 1;\n}\n",
                &["unused", "shared"],
                "one",
            ),
        ] {
            let symbols = extractor.extract(Path::new(path), content, language);
            let mut declared: Vec<&str> = symbols
                .iter()
                .filter(|symbol| !symbol.kind.holds_members())
                .map(|symbol| symbol.name.as_str())
                .collect();
            let mut expected: Vec<&str> = several.iter().copied().chain([one]).collect();
            declared.sort_unstable();
            expected.sort_unstable();
            assert_eq!(declared, expected, "{path}: {content:?}");
            let located = |name: &str| {
                let location = &symbols
                    .iter()
                    .find(|symbol| symbol.name == name)
                    .unwrap_or_else(|| panic!("{path}: no {name}"))
                    .location;
                (
                    (location.line, location.column),
                    (location.name_end_line, location.name_end_column),
                    location.effective_start(),
                    (location.end_line, location.end_column),
                )
            };
            let spans_statement = |(name, name_end, start, end): ((u32, u32), _, (u32, u32), _)| {
                start < name || end != name_end
            };
            let first = located(several[0]);
            assert!(
                spans_statement(first),
                "{path}: {} spans the statement",
                several[0]
            );
            for name in &several[1..] {
                let other = located(name);
                assert_ne!(other.0, first.0, "{path}: {name} is read at its own name");
                assert_eq!((other.2, other.3), (first.2, first.3), "{path}: {name}");
            }
            assert!(
                spans_statement(located(one)),
                "{path}: {one} declared alone spans its statement"
            );
        }

        let blank = extractor.extract(
            Path::new("a.go"),
            "package p\n\nvar _, only = pick()\n",
            Language::Go,
        );
        let names: Vec<(&str, (u32, u32))> = blank
            .iter()
            .map(|symbol| (symbol.name.as_str(), symbol.location.effective_start()))
            .collect();
        assert_eq!(names, [("only", (3, 1))]);
    }

    #[test]
    fn a_chained_assignment_to_more_than_names_is_none_of_its_names_alone() {
        let extractor = SymbolExtractor::new();
        for (path, language, content, names) in [
            (
                "a.py",
                Language::Python,
                "LIMIT = obj.attr = 5\nobj.flag = COUNT = 7\nTABLE = cache['k'] = {}\n",
                &["LIMIT", "COUNT", "TABLE"][..],
            ),
            (
                "a.rb",
                Language::Ruby,
                "LEVEL = CONF.level = 3\n@cache = TABLE = {}\n",
                &["LEVEL", "TABLE"],
            ),
        ] {
            let symbols = extractor.extract(Path::new(path), content, language);
            for name in names {
                let location = &symbols
                    .iter()
                    .find(|symbol| symbol.name == *name)
                    .unwrap_or_else(|| panic!("{path}: no {name}"))
                    .location;
                assert_eq!(
                    (
                        location.effective_start(),
                        location.end_line,
                        location.end_column
                    ),
                    (
                        (location.line, location.column),
                        location.name_end_line,
                        location.name_end_column,
                    ),
                    "{path}: {name} spans its name alone"
                );
            }
        }
    }

    /// A C header's macros can leave the grammar recovering around its
    /// declarations: a struct holding a macro member puts what follows inside
    /// a syntax error, where a prototype reads as a member, and a macro type
    /// cuts a typedef short of its name. What the grammar read whole is read
    /// where it stands, as a function and a type; what it cut short names
    /// nothing.
    /// A body ends where one configuration of the preprocessor closes it,
    /// whatever brackets comments, literals and other branches hold, and a
    /// brace a directive writes ends with that directive at the latest.
    #[test]
    fn a_body_ends_where_one_preprocessor_configuration_closes_it() {
        fn end(content: &str) -> Option<&str> {
            let open = content.find('{').unwrap();
            body_end(content, open).map(|end| &content[..end])
        }
        assert_eq!(
            end("{\n#if A\n  if (x) {\n#else\n  {\n#endif\n  }\n}\nint after;"),
            Some("{\n#if A\n  if (x) {\n#else\n  {\n#endif\n  }\n}")
        );
        assert_eq!(
            end("{ /* } */ \"}\" '}' // }\n int n = 1'000; }; tail"),
            Some("{ /* } */ \"}\" '}' // }\n int n = 1'000; }")
        );
        assert_eq!(
            end("{ auto s = R\"x( \"} )x\"; } tail"),
            Some("{ auto s = R\"x( \"} )x\"; }")
        );
        assert_eq!(
            end("#define OPEN(n) int n(void) \\\n  { \\\n  int v;\nint after(void);\n"),
            Some("#define OPEN(n) int n(void) \\\n  { \\\n  int v;")
        );
        assert_eq!(end("{ int never_closed;\n"), None);
    }

    #[test]
    fn a_recovered_header_declares_only_what_the_grammar_read_whole() {
        let extractor = SymbolExtractor::new();
        let declared = |content: &str| -> Vec<String> {
            extractor
                .extract(Path::new("e.h"), content, Language::Cpp)
                .iter()
                .map(|symbol| format!("{}:{}", symbol.kind, symbol.name))
                .collect()
        };
        let recovered = declared(
            "#ifdef __cplusplus\nextern \"C\" {\n#endif\n\n#if A\nstruct S {\n  int x;\n  BITS\n};\n\
             #endif\n\ntypedef struct { int y; } T;\nint after(void);\n\n\
             #ifdef __cplusplus\n}\n#endif\n",
        );
        for expected in ["class:T", "function:after"] {
            assert!(
                recovered.iter().any(|d| d == expected),
                "{expected} in {recovered:?}"
            );
        }
        let cut_short = declared("typedef Array(StackSlice) StackSliceArray;\n");
        assert!(
            !cut_short.iter().any(|d| d.ends_with(":StackSlice")),
            "a cut-short typedef names nothing: {cut_short:?}"
        );
        let lost_body = declared(
            "int f(int x)\n{\n    int local = x;\n#if A\n    if (x) {\n#else\n    {\n#endif\n\
             \x20       return local;\n    }\n}\n\nint proto(void);\nstruct S { int a; };\n\
             int global_after = 3;\n",
        );
        assert!(
            !lost_body.iter().any(|d| d.ends_with(":local")),
            "a lost body's local is no declaration: {lost_body:?}"
        );
        for expected in [
            "function:proto",
            "struct:S",
            "field:a",
            "variable:global_after",
        ] {
            assert!(
                lost_body.iter().any(|d| d == expected),
                "what follows a lost body is read: {expected} in {lost_body:?}"
            );
        }
        assert_eq!(
            declared(
                "int f(int x) G_GNUC_PURE;\nint g(int a,\n      int b) G_GNUC_PURE;\n\
                 static int k(int a) G_GNUC_CONST G_GNUC_WARN;\ntypedef Array(T) Ts;\n\
                 int counter ATTR;\nclass M { const char& name() const BOUND { return n_; } };\n"
            ),
            [
                "function:f",
                "function:g",
                "function:k",
                "class:Ts",
                "variable:counter",
                "class:M",
                "method:name",
            ],
            "a macro written after a declarator names nothing"
        );
        assert_eq!(
            declared(
                "typedef struct {\n  Array(Entry) list;\n  Entry empty;\n  uint32_t count;\n  \
                 Array(uint32_t) free_ids;\n} Pool;\n\
                 typedef __attribute__((aligned(1))) U16 unalign16;\n\
                 struct Q {\n  int a;\n  Array(uint32_t)\n  int after_macro;\n  PADDING;\n};\n"
            ),
            [
                "class:Pool",
                "field:list",
                "field:empty",
                "field:count",
                "field:free_ids",
                "class:unalign16",
                "struct:Q",
                "field:a",
                "field:after_macro",
            ],
            "a macro's arguments name nothing, and what follows them is its declaration's name"
        );
        assert_eq!(
            declared(
                "PyAPI_FUNC(int) Py_IsInitialized(void);\nLUA_API int (lua_gettop) (lua_State *L);\n\
                 typedef Array(uint32_t) IdList;\nstruct S {\n  Array(uint32_t) ids[4];\n  \
                 int GIT_CALLBACK(read)(void *);\n};\n"
            ),
            [
                "function:lua_gettop",
                "class:IdList",
                "struct:S",
                "field:ids",
                "method:read",
            ],
            "a keyword names nothing, and a macro's argument list stands for the name it wraps"
        );
        let begins = |content: &str, name: &str| {
            extractor
                .extract(Path::new("e.h"), content, Language::Cpp)
                .into_iter()
                .find(|symbol| symbol.name == name)
                .map(|symbol| symbol.location.effective_start())
        };
        assert_eq!(
            begins(
                "typedef struct {\n  Entry empty;\n  Array(uint32_t) free_ids;\n} Pool;\n",
                "free_ids"
            ),
            Some((3, 3)),
            "the declaration a macro's rest names begins with the macro"
        );
        assert_eq!(
            begins(
                "typedef __attribute__((aligned(1))) U16 unalign16;\n",
                "unalign16"
            ),
            Some((1, 1))
        );
        assert_eq!(
            declared(
                "class Widget {\n  class Impl;\n  struct Node;\n  Impl* impl_;\n};\n\
                 void uses(struct Used);\nstruct Glob *gp;\n"
            ),
            [
                "class:Widget",
                "class:Impl",
                "struct:Node",
                "field:impl_",
                "function:uses",
                "variable:gp",
            ],
            "a bodiless type standing alone in a class body is a forward declaration"
        );
        assert_eq!(
            declared(
                "EXPORT int (*verify_callback(const SSL *ssl))(\n    int ok, X509_STORE_CTX *ctx);\n\
                 int kept(void);\n"
            ),
            ["function:kept"]
        );
        assert_eq!(
            declared(
                "QT_BEGIN_NAMESPACE\n\nclass QWidget;\nEXPORT struct stat st;\nAPI struct item *items;\n"
            ),
            ["class:QWidget", "variable:items"]
        );
    }

    /// A class definition a macro made the grammar read as a function is read
    /// again past the macro, so it is the class it defines, with its members
    /// read as a class body holds them.
    #[test]
    fn a_class_a_macro_misread_as_a_function_is_read_past_it() {
        let declared: Vec<String> = SymbolExtractor::new()
            .extract(
                Path::new("m.h"),
                "class EXPORT Widget {\npublic:\n  Widget();\n  friend class ::ns::Access;\n  \
                 virtual void clear() const = 0;\n  typedef int Size;\nprivate:\n  int count_;\n};\n\
                 OPENSSL_EXPORT struct ssl_st { int version; };\n\
                 NS_BEGIN\n\nclass Clock : public Base { int now_; };\n\
                 class EXPORT W final { int z; };\n\
                 class LOCKABLE WARN_UNUSED Mutex { int mu_; };\n\
                 class EXPORT ns::Qualified { int q; };\n\
                 template <> class EXPORT Spec<int> { int s; };\n\
                 class __declspec(dllexport) DECL Declared { int d; };\n\
                 struct S s{1};\nint plain(int a) { int inner = a; return inner; }\n",
                Language::Cpp,
            )
            .iter()
            .map(|symbol| {
                format!(
                    "{}:{}",
                    symbol.kind,
                    symbol.name_path.as_deref().unwrap_or(&symbol.name)
                )
            })
            .collect();
        assert_eq!(
            declared,
            [
                "class:Widget",
                "constructor:Widget/Widget",
                "method:Widget/clear",
                "class:Widget/Size",
                "field:Widget/count_",
                "struct:ssl_st",
                "field:ssl_st/version",
                "class:Clock",
                "field:Clock/now_",
                "class:W",
                "field:W/z",
                "class:Mutex",
                "field:Mutex/mu_",
                "class:ns::Qualified",
                "field:ns::Qualified/q",
                "class:Spec<int>",
                "field:Spec<int>/s",
                "class:Declared",
                "field:Declared/d",
                "variable:s",
                "function:plain",
            ]
        );
    }

    /// An export macro before a declaration's type reads as its type, and
    /// the type after it as the scope of a qualified name whose `::` the
    /// grammar supplies: the name is what that qualifier qualifies, and the
    /// declarator still makes it a function.
    #[test]
    fn a_macro_read_as_a_scope_leaves_the_name_it_qualifies() {
        let content = "ZSTDLIB_API ZSTD_ErrorCode ZSTD_getErrorCode(size_t r);\n\
                       MEM_STATIC ZSTD_ErrorCode def_err(int a) { return 0; }\n\
                       EXPORT std::string name_s();\n";
        let symbols = SymbolExtractor::new().extract(Path::new("m.h"), content, Language::Cpp);
        let declared: Vec<String> = symbols
            .iter()
            .map(|symbol| format!("{}:{}", symbol.kind, symbol.name))
            .collect();
        assert_eq!(
            declared,
            [
                "function:ZSTD_getErrorCode",
                "function:def_err",
                "function:name_s"
            ]
        );
    }

    #[test]
    fn a_python_type_alias_spans_its_statement() {
        let symbols = SymbolExtractor::new().extract(
            Path::new("a.py"),
            "type Pair = tuple[int, int]\n",
            Language::Python,
        );
        let location = &symbols[0].location;
        assert_eq!(
            (
                location.effective_start(),
                location.end_line,
                location.end_column
            ),
            ((1, 1), Some(1), Some(28))
        );
    }

    #[test]
    fn a_servers_range_takes_the_declaration_the_grammar_reads_at_its_name() {
        let content = "template <typename T>\nT twice(T x) {\n    return x;\n}\n\n// Above.\n\
                       int top() {\n    return 1;\n}\n\nclass Box {\n    int v;\n};\n\n\
                       template <typename U>\nvoid Widget::set(U u) {\n}\n";
        let path = Path::new("m.cpp");
        let served =
            |name: &str, (line, column): (u32, u32), start: (u32, u32), end: (u32, u32)| {
                Symbol::new(
                    name.to_string(),
                    SymbolKind::Function,
                    Location::full(
                        path.to_path_buf(),
                        line,
                        column,
                        start.0,
                        start.1,
                        end.0,
                        end.1,
                    ),
                )
            };
        let mut symbols = vec![
            served("twice", (2, 3), (2, 1), (4, 2)),
            served("top", (7, 5), (6, 1), (9, 2)),
            served("Box", (11, 7), (11, 1), (13, 2)).with_children(vec![served(
                "v",
                (12, 9),
                (12, 5),
                (12, 10),
            )]),
            served("return", (8, 5), (8, 5), (8, 13)),
            Symbol::new(
                "Widget::set".to_string(),
                SymbolKind::Function,
                Location::full(path.to_path_buf(), 16, 14, 16, 1, 17, 2).with_name_end(16, 17),
            ),
        ];
        SymbolExtractor::new().complete_from_declarations(
            &mut symbols,
            path,
            content,
            Language::Cpp,
        );

        let extent = |symbol: &Symbol| {
            let location = &symbol.location;
            (
                location.effective_start(),
                (location.end_line.unwrap(), location.end_column.unwrap()),
            )
        };
        assert_eq!(extent(&symbols[0]), ((1, 1), (4, 2)));
        assert_eq!(extent(&symbols[1]), ((6, 1), (9, 2)));
        assert_eq!(extent(&symbols[2]), ((11, 1), (13, 3)));
        assert_eq!(extent(&symbols[2].children[0]), ((12, 5), (12, 11)));
        assert_eq!(extent(&symbols[3]), ((8, 5), (8, 13)));
        assert_eq!(extent(&symbols[4]), ((15, 1), (17, 2)));
    }

    /// A signature is read where its declaration is written: a method read
    /// out of its class is no declaration in TypeScript, C# or Java, and PHP
    /// without its opening tag is no code at all. It ends where a body or a
    /// value opens, so an initializer of any size stays out, while the type
    /// an alias names is what it declares.
    #[test]
    fn a_signature_is_read_where_its_declaration_is_written() {
        let extractor = SymbolExtractor::new();
        let signature = |file: &str, language, content: &str, name: &str| {
            extractor
                .extract_with_signatures(Path::new(file), content, language)
                .into_iter()
                .find(|symbol| symbol.name == name)
                .unwrap_or_else(|| panic!("{file} declares {name}"))
                .signature
        };
        let table = format!(
            "class T {{\n  static readonly TABLE = {{\n{}  }};\n}}\n",
            "    a: 1,\n".repeat(200)
        );
        let cases = [
            (
                "a.ts",
                Language::TypeScript,
                "class C {\n  run(a: number): number {\n    return a;\n  }\n}\n",
                "run",
                Some("run(a: number): number"),
            ),
            (
                "a.ts",
                Language::TypeScript,
                table.as_str(),
                "TABLE",
                Some("static readonly TABLE"),
            ),
            (
                "a.ts",
                Language::TypeScript,
                "export const f = (a: number): number => a;\n",
                "f",
                Some("export const f = (a: number): number =>"),
            ),
            (
                "a.ts",
                Language::TypeScript,
                "export class Svc {\n  constructor(public n = 1) {}\n}\n",
                "constructor",
                Some("constructor(public n = 1)"),
            ),
            (
                "a.cs",
                Language::CSharp,
                "class C {\n    public int Run(int a) {\n        return a;\n    }\n}\n",
                "Run",
                Some("public int Run(int a)"),
            ),
            (
                "a.cpp",
                Language::Cpp,
                "namespace n {}\nnamespace al = n;\n",
                "al",
                Some("namespace al = n"),
            ),
            (
                "a.swift",
                Language::Swift,
                "struct V {\n    var o: Int = 42 {\n        didSet { print(o) }\n    }\n}\n",
                "o",
                Some("var o: Int"),
            ),
            (
                "a.swift",
                Language::Swift,
                "let a: Int = 1, b: Int = 2\n",
                "b",
                Some("let a: Int, b: Int"),
            ),
            (
                "a.swift",
                Language::Swift,
                "func f(a: Int = 1, b: Int) {}\n",
                "f",
                Some("func f(a: Int = 1, b: Int)"),
            ),
            (
                "a.kt",
                Language::Kotlin,
                "class K {\n    val x: Int = 5 get() = field + 1\n}\n",
                "x",
                Some("val x: Int"),
            ),
            (
                "v.go",
                Language::Go,
                "package p\n\nvar x, y int = 1, 2\n",
                "x",
                Some("var x, y int"),
            ),
            (
                "a.cpp",
                Language::Cpp,
                "struct F {\n  F(int);\n  int a_, b_;\n};\nF::F(int p) : a_(compute(p)), b_{p} {}\n",
                "F::F",
                Some("F::F(int p)"),
            ),
            (
                "a.rs",
                Language::Rust,
                "pub const C: u32 = match FOO { _ => 1 };\n",
                "C",
                Some("pub const C: u32"),
            ),
            (
                "a.rs",
                Language::Rust,
                "const CL: fn(i32) -> i32 = |x| x;\n",
                "CL",
                Some("const CL: fn(i32) -> i32 = |x|"),
            ),
            (
                "A.java",
                Language::Java,
                "class J { String s = switch (x) { default -> \"a\"; }; }\n",
                "s",
                Some("String s"),
            ),
            (
                "a.ts",
                Language::TypeScript,
                "export const A = 1, B = 2;\n",
                "A",
                Some("export const A"),
            ),
            (
                "a.ts",
                Language::TypeScript,
                "export const A = 1, B = 2;\n",
                "B",
                Some("export const A, B"),
            ),
            (
                "a.cpp",
                Language::Cpp,
                "int c = 1, *p;\n",
                "p",
                Some("int c, *p"),
            ),
            (
                "a.h",
                Language::Cpp,
                "typedef __attribute__((aligned(1))) U16 unalign16;\n",
                "unalign16",
                None,
            ),
            (
                "a.cpp",
                Language::Cpp,
                "template <class T> concept Small = sizeof(T) < 4;\n",
                "Small",
                Some("template <class T> concept Small = sizeof(T) < 4"),
            ),
            (
                "a.cpp",
                Language::Cpp,
                "int a, b = 2;\n",
                "a",
                Some("int a, b"),
            ),
            (
                "a.cpp",
                Language::Cpp,
                "struct W { int x{5}, y{6}; };\n",
                "y",
                Some("int x, y"),
            ),
            (
                "a.h",
                Language::Cpp,
                "void sk_zero(STACK_OF(SAMPLE) *sk);\n",
                "sk_zero",
                Some("void sk_zero(STACK_OF(SAMPLE) *sk)"),
            ),
            (
                "a.lua",
                Language::Lua,
                "local M = {}\nM.stop = function(reason)\n  return reason\nend\n",
                "M.stop",
                Some("M.stop = function(reason)"),
            ),
            (
                "a.lua",
                Language::Lua,
                "local handler = function() end\n",
                "handler",
                Some("local handler = function()"),
            ),
            (
                "a.rb",
                Language::Ruby,
                "class C\n  LIMIT ||= compute(10)\nend\n",
                "LIMIT",
                Some("LIMIT"),
            ),
            (
                "a.dart",
                Language::Dart,
                "class C {\n  C.named();\n  factory C.alias() = C.named;\n}\n",
                "C.alias",
                Some("factory C.alias()"),
            ),
            (
                "a.dart",
                Language::Dart,
                "void outer() {\n  int helper() => 1;\n}\n",
                "helper",
                Some("int helper()"),
            ),
            (
                "a.dart",
                Language::Dart,
                "int get answer {\n  return 42;\n}\n",
                "answer",
                Some("int get answer"),
            ),
            (
                "a.dart",
                Language::Dart,
                "external void bind(int port);\n",
                "bind",
                Some("external void bind(int port)"),
            ),
            (
                "a.dart",
                Language::Dart,
                "external int get port;\n",
                "port",
                Some("external int get port"),
            ),
            (
                "a.dart",
                Language::Dart,
                "class C {\n  external static int count();\n}\n",
                "count",
                Some("external static int count()"),
            ),
            (
                "a.dart",
                Language::Dart,
                "class C {\n  external C.make();\n}\n",
                "C.make",
                Some("external C.make()"),
            ),
            (
                "a.dart",
                Language::Dart,
                "abstract class A {\n  void run();\n}\n",
                "run",
                Some("void run()"),
            ),
            (
                "a.scala",
                Language::Scala,
                "object O {\n  given shown: Show[Int] with {\n    def show(n: Int) = \"\"\n  }\n}\n",
                "shown",
                Some("given shown: Show[Int]"),
            ),
            (
                "a.scala",
                Language::Scala,
                "object O {\n  given ord: Ordering[Int] = Ordering.Int\n}\n",
                "ord",
                Some("given ord: Ordering[Int]"),
            ),
            (
                "a.kt",
                Language::Kotlin,
                "class K(val a: Int) {\n    constructor(s: String) : this(s.length) {\n        println(s)\n    }\n}\n",
                "constructor",
                Some("constructor(s: String) : this(s.length)"),
            ),
            (
                "a.cs",
                Language::CSharp,
                "class C {\n    public int P { get; private set; } = 5;\n}\n",
                "P",
                Some("public int P { get; private set; }"),
            ),
            (
                "a.cs",
                Language::CSharp,
                "class C {\n    public int Q {\n        get { return 1; }\n    }\n}\n",
                "Q",
                Some("public int Q"),
            ),
            (
                "a.cs",
                Language::CSharp,
                "class C {\n    int E => 1;\n}\n",
                "E",
                Some("int E"),
            ),
            (
                "a.cs",
                Language::CSharp,
                "class C {\n    event System.Action A { add {} remove {} }\n}\n",
                "A",
                Some("event System.Action A"),
            ),
            (
                "A.java",
                Language::Java,
                "class C {\n    @Override\n    public String toString() {\n        return \"s\";\n    }\n}\n",
                "toString",
                Some("@Override\n    public String toString()"),
            ),
            (
                "A.java",
                Language::Java,
                "class C {\n    private static final int MAX = 3;\n}\n",
                "MAX",
                Some("private static final int MAX"),
            ),
            (
                "a.php",
                Language::PHP,
                "<?php\nclass C {\n    public function run($a) {\n        return $a;\n    }\n}\n",
                "run",
                Some("public function run($a)"),
            ),
            (
                "a.php",
                Language::PHP,
                "<?php\nclass C {\n}\n",
                "C",
                Some("class C"),
            ),
            (
                "a.php",
                Language::PHP,
                "<?php\nclass C {\n    public function run(int $a = ) {\n        return $a;\n    }\n}\n",
                "run",
                None,
            ),
            (
                "a.php",
                Language::PHP,
                "<?php\nclass Big extends Base implements {\n    public function a() {}\n}\n",
                "Big",
                None,
            ),
            (
                "a.scala",
                Language::Scala,
                "class C {\n  def run(a: Int): Int = {\n    a\n  }\n}\n",
                "run",
                Some("def run(a: Int): Int"),
            ),
            (
                "a.py",
                Language::Python,
                "class C:\n    def run(self, a):\n        return a\n",
                "run",
                Some("def run(self, a)"),
            ),
            (
                "a.py",
                Language::Python,
                "def outer():\n    def inner():\n        return 1\n    return inner\n",
                "inner",
                Some("def inner()"),
            ),
            (
                "a.py",
                Language::Python,
                "X: int = {\n    \"a\": 1,\n}\n",
                "X",
                Some("X: int"),
            ),
            (
                "a.rs",
                Language::Rust,
                "/// Size.\n#[inline]\npub fn len() -> usize {\n    0\n}\n",
                "len",
                Some("pub fn len() -> usize"),
            ),
            (
                "a.rs",
                Language::Rust,
                "pub const MAX: usize = 3;\n",
                "MAX",
                Some("pub const MAX: usize"),
            ),
            (
                "a.tf",
                Language::Terraform,
                "variable \"region\" {\n  default = \"x\"\n}\n",
                "region",
                Some("variable \"region\""),
            ),
            (
                "a.ts",
                Language::TypeScript,
                "export type Handler = (a: number) => void;\n",
                "Handler",
                Some("export type Handler = (a: number) => void"),
            ),
            (
                "a.rs",
                Language::Rust,
                "pub type Res<T> = Result<T, String>;\n",
                "Res",
                Some("pub type Res<T> = Result<T, String>"),
            ),
            (
                "a.go",
                Language::Go,
                "package p\n\ntype Alias = map[string]int\n",
                "Alias",
                Some("type Alias = map[string]int"),
            ),
            (
                "a.py",
                Language::Python,
                "type Pair = tuple[int, int]\n",
                "Pair",
                Some("type Pair = tuple[int, int]"),
            ),
            (
                "a.swift",
                Language::Swift,
                "typealias Pt = Point\n",
                "Pt",
                Some("typealias Pt = Point"),
            ),
            (
                "A.java",
                Language::Java,
                "class C {\n    static final String A = \"BODY\", B = \"b\";\n}\n",
                "A",
                Some("static final String A"),
            ),
            (
                "A.java",
                Language::Java,
                "class C {\n    static final String A = \"BODY\", B = \"b\";\n}\n",
                "B",
                Some("static final String A, B"),
            ),
            (
                "A.java",
                Language::Java,
                "class C {\n    int x, y = 2, z;\n}\n",
                "x",
                Some("int x, y, z"),
            ),
            (
                "a.cs",
                Language::CSharp,
                "class C { private readonly int f = 1, g = 2; }\n",
                "g",
                Some("private readonly int f, g"),
            ),
            (
                "a.php",
                Language::PHP,
                "<?php\nconst X = 1, Y = 2;\n",
                "Y",
                Some("const X, Y"),
            ),
            (
                "a.swift",
                Language::Swift,
                "let s1 = 1, s2 = 2\n",
                "s2",
                Some("let s1, s2"),
            ),
            (
                "a.swift",
                Language::Swift,
                "enum E { case a = 1, b = 2 }\n",
                "b",
                Some("case a, b"),
            ),
            (
                "a.go",
                Language::Go,
                "package p\n\nvar a, b = 1, 2\n",
                "a",
                Some("var a, b"),
            ),
            (
                "a.dart",
                Language::Dart,
                "class D {\n  int a = 1, b = 2;\n}\n",
                "b",
                Some("int a, b"),
            ),
            (
                "a.cpp",
                Language::Cpp,
                "struct W { int f = 1, g = 2; };\n",
                "g",
                Some("int f, g"),
            ),
            (
                "a.cpp",
                Language::Cpp,
                "struct W { int f, g = 2, h; };\n",
                "f",
                Some("int f, g, h"),
            ),
            (
                "a.c",
                Language::Cpp,
                "typedef struct {\n  int a;\n} Pair, *PairPtr;\n",
                "PairPtr",
                Some("typedef struct Pair, *PairPtr"),
            ),
            (
                "a.c",
                Language::Cpp,
                "static struct Cfg { int v; } config = { 1 };\n",
                "config",
                Some("static struct Cfg config"),
            ),
            (
                "a.swift",
                Language::Swift,
                "func f(a: Int = 1, b: Int = 2) {}\n",
                "f",
                Some("func f(a: Int = 1, b: Int = 2)"),
            ),
            (
                "a.swift",
                Language::Swift,
                "enum E {\n    case c(x: Int = 1, Int = 2)\n}\n",
                "c",
                Some("case c(x: Int = 1, Int = 2)"),
            ),
            (
                "a.kt",
                Language::Kotlin,
                "fun f(a: Int = 1, b: Int = 2) {}\n",
                "f",
                Some("fun f(a: Int = 1, b: Int = 2)"),
            ),
            (
                "a.ts",
                Language::TypeScript,
                "export const { a, c = 1 } = obj;\n",
                "c",
                Some("export const { a, c = 1 }"),
            ),
            (
                "a.js",
                Language::JavaScript,
                "const [first = 0, second] = arr;\n",
                "first",
                Some("const [first = 0, second]"),
            ),
            (
                "a.scala",
                Language::Scala,
                "object O {\n  def === = true\n}\n",
                "===",
                Some("def ==="),
            ),
            (
                "a.rb",
                Language::Ruby,
                "class C\n  def self.make = new\nend\n",
                "make",
                Some("def self.make"),
            ),
            (
                "a.rb",
                Language::Ruby,
                "class C\n  def ==(other) = true\nend\n",
                "==",
                Some("def ==(other)"),
            ),
            (
                "a.rb",
                Language::Ruby,
                "class C\n  def h; end\nend\n",
                "h",
                Some("def h"),
            ),
            (
                "a.lua",
                Language::Lua,
                "function M.noop() end\n",
                "M.noop",
                Some("function M.noop()"),
            ),
            (
                "a.tf",
                Language::Terraform,
                "variable \"x\" {}\n",
                "x",
                Some("variable \"x\""),
            ),
            (
                "a.py",
                Language::Python,
                "def f(a):  # noqa: E501\n    pass\n",
                "f",
                Some("def f(a)"),
            ),
            (
                "a.cpp",
                Language::Cpp,
                "int f(int a) // does x\n{\n  return a;\n}\n",
                "f",
                Some("int f(int a)"),
            ),
            (
                "a.swift",
                Language::Swift,
                "struct V {\n  var body: String {\n    \"BODY\"\n  }\n}\n",
                "body",
                Some("var body: String"),
            ),
            (
                "a.swift",
                Language::Swift,
                "struct V {\n  var o: Int {\n    didSet {}\n  }\n}\n",
                "o",
                Some("var o: Int"),
            ),
            (
                "a.kt",
                Language::Kotlin,
                "class K {\n  val q: Int get() = 6\n}\n",
                "q",
                Some("val q: Int"),
            ),
            (
                "a.kt",
                Language::Kotlin,
                "class K {\n  val x: Int by lazy { 1 }\n}\n",
                "x",
                Some("val x: Int"),
            ),
            (
                "a.rs",
                Language::Rust,
                "macro_rules! m {\n    () => {};\n}\n",
                "m",
                Some("macro_rules! m"),
            ),
        ];
        for (file, language, content, name, expected) in cases {
            assert_eq!(
                signature(file, language, content, name).as_deref(),
                expected,
                "{file} {name}"
            );
        }
    }

    /// A server's symbol takes the signature written at its own place, however
    /// the server marks its name and whether or not a query reads a
    /// declaration there: tsserver marks an overload by its whole range, clangd
    /// a destructor by its `~`, and pyright lists an assignment under an `if`.
    /// clangd leaves a member declaration's `;` out of its range, and an `=`
    /// that ends an operator's name opens no value. A declaration the whole
    /// file reads only by recovering from an unknown macro is read alone,
    /// within the server's range, and one that does not read as written alone
    /// either — a namespace a macro opens — has no signature.
    #[test]
    fn a_server_symbol_takes_the_signature_written_at_its_place() {
        let extractor = SymbolExtractor::new();
        let cases = [
            (
                "o.ts",
                Language::TypeScript,
                "export function over(a: string): string;\nexport function over(a: any): any {\n  return a;\n}\n",
                "over",
                (2, 1, 2, 1, 4, 2),
                Some("export function over(a: any): any"),
            ),
            (
                "c.ts",
                Language::TypeScript,
                "export default {\n  methods: {\n    save() { return 1; },\n  },\n};\n",
                "methods",
                (2, 3, 2, 3, 4, 4),
                Some("methods"),
            ),
            (
                "c.ts",
                Language::TypeScript,
                "export default {\n  methods: {\n    save() { return 1; },\n  },\n};\n",
                "default",
                (1, 1, 1, 1, 5, 3),
                Some("export default"),
            ),
            (
                "w.cpp",
                Language::Cpp,
                "struct Widget { ~Widget(); };\nWidget::~Widget() {}\n",
                "~Widget",
                (2, 9, 2, 1, 2, 21),
                Some("Widget::~Widget()"),
            ),
            (
                "m.cpp",
                Language::Cpp,
                "QT_BEGIN_NAMESPACE\nvoid first() {}\n",
                "first",
                (2, 6, 2, 1, 2, 16),
                Some("void first()"),
            ),
            (
                "n.cpp",
                Language::Cpp,
                "QT_BEGIN_NAMESPACE\nvoid first() {}\nQT_END_NAMESPACE\n",
                "qt",
                (1, 1, 1, 1, 3, 17),
                None,
            ),
            (
                "w.cpp",
                Language::Cpp,
                "struct Widget {\n  Widget& operator=(const Widget&);\n};\n",
                "operator=",
                (2, 11, 2, 3, 2, 35),
                Some("Widget& operator=(const Widget&)"),
            ),
            (
                "f.py",
                Language::Python,
                "if True:\n    FLAG = True\n",
                "FLAG",
                (2, 5, 2, 5, 2, 16),
                Some("FLAG"),
            ),
            (
                "g.ts",
                Language::TypeScript,
                "declare global {\n  interface Window {\n    x: number;\n  }\n}\n",
                "global",
                (1, 9, 1, 1, 5, 2),
                Some("declare global"),
            ),
            (
                "l.py",
                Language::Python,
                "twice = lambda x: x * 2\n",
                "twice",
                (1, 1, 1, 1, 1, 24),
                Some("twice = lambda x"),
            ),
            (
                "u.cpp",
                Language::Cpp,
                "using Alias = Widget<int>;\n",
                "Alias",
                (1, 7, 1, 1, 1, 26),
                Some("using Alias = Widget<int>"),
            ),
            (
                "g.cpp",
                Language::Cpp,
                "int global_a = 1, global_b = 2;\n",
                "global_a",
                (1, 5, 1, 1, 1, 17),
                Some("int global_a"),
            ),
            (
                "g.cpp",
                Language::Cpp,
                "int global_a = 1, global_b = 2;\n",
                "global_b",
                (1, 19, 1, 1, 1, 31),
                Some("int global_a, global_b"),
            ),
            (
                "s.swift",
                Language::Swift,
                "struct V {\n  subscript(i: Int) -> Int {\n    return i\n  }\n}\n",
                "subscript(_:)",
                (2, 3, 2, 3, 4, 4),
                Some("subscript(i: Int) -> Int"),
            ),
        ];
        for (
            file,
            language,
            content,
            name,
            (line, column, from_line, from_column, end_line, end_column),
            expected,
        ) in cases
        {
            let path = Path::new(file);
            let mut symbols = vec![Symbol::new(
                name.to_string(),
                SymbolKind::Function,
                Location::full(
                    path.to_path_buf(),
                    line,
                    column,
                    from_line,
                    from_column,
                    end_line,
                    end_column,
                ),
            )];
            extractor.complete_from_declarations(&mut symbols, path, content, language);
            assert_eq!(symbols[0].signature.as_deref(), expected, "{file}");
        }
    }

    #[test]
    fn a_declaration_read_by_recovering_from_an_error_widens_only_what_it_read() {
        let path = Path::new("m.cpp");
        for (content, name, at, served, widened) in [
            (
                "#define BEGIN_NS namespace app {\n#define END_NS }\n\nBEGIN_NS\n\nint f() {\n    \
                 return 1;\n}\n\nEND_NS\n",
                "f",
                (6, 5),
                ((6, 1), (8, 2)),
                ((6, 1), (8, 2)),
            ),
            (
                "class Box {\n    int v;\n}\n\nint top() {\n    return 1;\n}\n",
                "top",
                (5, 5),
                ((5, 1), (7, 2)),
                ((5, 1), (7, 2)),
            ),
            (
                "#define GUARDED(m) std::lock_guard<std::mutex> guard_(m);\n\
                 template <typename T>\nint locked(T v) {\n    GUARDED(mu)\n    return 0;\n}\n",
                "locked",
                (3, 5),
                ((3, 1), (6, 2)),
                ((2, 1), (6, 2)),
            ),
            (
                "auto [ka, kb] = make();\n",
                "",
                (1, 6),
                ((1, 6), (1, 7)),
                ((1, 1), (1, 24)),
            ),
        ] {
            let mut symbols = vec![Symbol::new(
                name.to_string(),
                SymbolKind::Function,
                Location::full(
                    path.to_path_buf(),
                    at.0,
                    at.1,
                    served.0.0,
                    served.0.1,
                    served.1.0,
                    served.1.1,
                ),
            )];
            SymbolExtractor::new().complete_from_declarations(
                &mut symbols,
                path,
                content,
                Language::Cpp,
            );
            let location = &symbols[0].location;
            assert_eq!(
                (
                    location.effective_start(),
                    (location.end_line.unwrap(), location.end_column.unwrap()),
                ),
                widened,
                "{name}"
            );
        }
    }

    #[test]
    fn a_types_header_is_its_declaration_outside_the_body() {
        let extractor = SymbolExtractor::new();
        let headers = |path: &str, content: &str, language: Language| {
            extractor
                .type_headers(Path::new(path), content, language)
                .lines
                .into_iter()
                .collect::<Vec<_>>()
        };

        let java = "@Entity\npublic class Service\n        implements Runnable {\n    // First.\n    \
                    public void run() {\n    }\n}\n";
        assert_eq!(headers("A.java", java, Language::Java), [1, 2, 3]);

        let python =
            "class Service(\n    Base,\n):\n    # First.\n    def run(self):\n        pass\n";
        assert_eq!(headers("a.py", python, Language::Python), [1, 2, 3]);

        let kotlin = "class Service(\n    val a: Int,\n) : Base(),\n    Runnable {\n    // First.\n    \
                      fun run() {}\n}\n";
        assert_eq!(headers("a.kt", kotlin, Language::Kotlin), [1, 2, 3, 4]);

        let rust = "impl<T> Run for Service<T>\nwhere\n    T: Clone,\n{\n    /// Runs.\n    \
                    fn run(&self) {}\n}\n";
        assert_eq!(headers("a.rs", rust, Language::Rust), [1, 2, 3]);

        for (path, content, language) in [
            (
                "a.ts",
                "export enum Mode {\n  Fast,\n\n  Slow,\n}\n",
                Language::TypeScript,
            ),
            (
                "a.cpp",
                "enum class Mode {\n    Fast,\n    Slow,\n};\n",
                Language::Cpp,
            ),
            (
                "a.go",
                "package p\n\ntype Empty interface {\n\tany\n}\n",
                Language::Go,
            ),
        ] {
            let first = content.lines().position(|line| line.contains('{')).unwrap() as u32 + 1;
            assert_eq!(headers(path, content, language), [first], "{path}");
        }
        let alias = "pub type Alias =\n    Vec<u32>;\n";
        assert_eq!(headers("a.rs", alias, Language::Rust), [1, 2]);
        // An associated type is a type of its own, all header as an alias is,
        // and a requirement beside it is a member of the body.
        let associated = "pub trait Tr {\n    type Out;\n    fn run(&self);\n}\n";
        assert_eq!(headers("a.rs", associated, Language::Rust), [1, 2]);
        let keyed =
            "protocol Keyed {\n    associatedtype Key: Hashable\n    var key: Key { get }\n}\n";
        assert_eq!(headers("a.swift", keyed, Language::Swift), [1, 2]);
    }

    #[test]
    fn a_node_ending_at_a_line_start_spans_the_lines_before_it() {
        let point = |row, column| tree_sitter::Point { row, column };
        assert_eq!(line_span(point(0, 4), point(2, 0)), 1..=2);
        assert_eq!(line_span(point(0, 4), point(2, 3)), 1..=3);
        assert_eq!(line_span(point(1, 0), point(1, 0)), 2..=2);
    }

    #[test]
    fn ruby_symbols_carry_the_name_the_file_spells() {
        let extractor = SymbolExtractor::new();
        let content = r#"
module Billing
  class Invoice
    def valid?
      true
    end
    def self.build(x) = new(x)
  end
end
"#;
        let symbols = extractor.extract(Path::new("a"), content, Language::Ruby);
        let found = |path: &str| {
            symbols
                .iter()
                .find(|s| s.name_path.as_deref() == Some(path))
                .map(|s| s.kind)
        };
        assert_eq!(found("Billing"), Some(SymbolKind::Module));
        assert_eq!(found("Invoice"), Some(SymbolKind::Class));
        assert_eq!(found("Invoice/valid?"), Some(SymbolKind::Method));
        assert_eq!(found("Invoice/build"), Some(SymbolKind::Method));
    }

    #[test]
    fn shell_and_lua_extract_their_function_forms() {
        let extractor = SymbolExtractor::new();
        let shell = extractor.extract(
            Path::new("a"),
            "deploy() { :; }\nfunction rollback { :; }\n",
            Language::Bash,
        );
        assert_eq!(shell.len(), 2);
        assert!(shell.iter().all(|s| s.kind == SymbolKind::Function));

        let lua = extractor.extract(
            Path::new("a"),
            "local function helper() end\nfunction M.render() end\n",
            Language::Lua,
        );
        let names: Vec<&str> = lua.iter().map(|s| s.name.as_str()).collect();
        assert_eq!(names, ["helper", "M.render"]);
    }

    #[test]
    fn dart_names_a_function_on_its_signature() {
        let extractor = SymbolExtractor::new();
        let content = r#"
class Order {
  Order(this.id);
  void pay() {}
}
enum Status { open }
mixin Loggable { void log() {} }
void topLevel() {}
"#;
        let symbols = extractor.extract(Path::new("a"), content, Language::Dart);
        let found = |path: &str| {
            symbols
                .iter()
                .find(|s| s.name_path.as_deref() == Some(path))
                .map(|s| s.kind)
        };
        assert_eq!(found("Order"), Some(SymbolKind::Class));
        assert_eq!(found("Order/Order"), Some(SymbolKind::Constructor));
        assert_eq!(found("Order/pay"), Some(SymbolKind::Method));
        assert_eq!(found("Status"), Some(SymbolKind::Enum));
        assert_eq!(found("Loggable"), Some(SymbolKind::Interface));
        assert_eq!(found("topLevel"), Some(SymbolKind::Function));
    }

    /// A Swift function's return type carries the field its name does, and is
    /// no declaration; an initializer is one.
    #[test]
    fn a_swift_function_declares_its_name_and_not_its_return_type() {
        let declared: Vec<(String, SymbolKind)> = SymbolExtractor::new()
            .extract(
                Path::new("a.swift"),
                "class D {\n    func a() {}\n    func b(x: String) -> [Int] { return [] }\n    \
                 var c: Double = 0\n    init(v: Int) {}\n}\nfunc top() -> Bool { true }\n",
                Language::Swift,
            )
            .into_iter()
            .map(|symbol| (symbol.name_path.unwrap_or(symbol.name), symbol.kind))
            .collect();
        assert_eq!(
            declared,
            [
                ("D".to_string(), SymbolKind::Class),
                ("D/a".to_string(), SymbolKind::Method),
                ("D/b".to_string(), SymbolKind::Method),
                ("D/c".to_string(), SymbolKind::Property),
                ("D/init".to_string(), SymbolKind::Constructor),
                ("top".to_string(), SymbolKind::Function),
            ]
        );
    }

    /// Swift and Scala give a function-local binding the same node kind as a
    /// stored member, so the queries match members where they are declared.
    /// Indexing a local would put a name in the index that names nothing a
    /// caller can reach.
    #[test]
    fn a_function_local_binding_is_not_a_member() {
        let extractor = SymbolExtractor::new();
        let swift = extractor.extract(
            Path::new("a"),
            r#"
class Cart {
    var items: [Int] = []
    func total() -> Int {
        let base = 10
        return base
    }
}
"#,
            Language::Swift,
        );
        let swift_names: Vec<&str> = swift.iter().map(|s| s.name.as_str()).collect();
        assert!(swift_names.contains(&"items"));
        assert!(swift_names.contains(&"total"));
        assert!(!swift_names.contains(&"base"));

        let scala = extractor.extract(
            Path::new("a"),
            r#"
class Order {
  val id = 1
  def total(): Int = {
    val base = 10
    base
  }
}
"#,
            Language::Scala,
        );
        let scala_names: Vec<&str> = scala.iter().map(|s| s.name.as_str()).collect();
        assert!(scala_names.contains(&"id"));
        assert!(scala_names.contains(&"total"));
        assert!(!scala_names.contains(&"base"));
    }

    #[test]
    fn rust_symbols_carry_correct_kinds() {
        let extractor = SymbolExtractor::new();
        let content = r#"
fn main() {}
struct Foo {}
enum Bar { A, B }
trait Baz {}
type Alias = Foo;
"#;
        let symbols = extractor.extract(Path::new("a"), content, Language::Rust);
        let kind = |name: &str| symbols.iter().find(|s| s.name == name).map(|s| s.kind);
        assert_eq!(kind("main"), Some(SymbolKind::Function));
        assert_eq!(kind("Foo"), Some(SymbolKind::Struct));
        assert_eq!(kind("Bar"), Some(SymbolKind::Enum));
        assert_eq!(kind("Baz"), Some(SymbolKind::Interface));
        // A type alias is a named type, never a generic parameter.
        assert_eq!(kind("Alias"), Some(SymbolKind::Class));
    }

    /// The cross-surface invariant: the tree-sitter index extractor, the LSP
    /// self-type normalizer (`Symbol::normalize_symbol_name`), and the
    /// workspace-symbol path all key an impl method under the SAME container —
    /// the single `Symbol::self_type_segment` rule the index now calls too — or
    /// a `name_path` copied from one surface fails against another
    /// (`symbols`/`edit`). Pins every self-type shape: nominal, generic,
    /// scoped/unscoped trait, structural (tuple/array/pointer/qualified),
    /// primitive-element structural (no separate AST descent to diverge on
    /// anymore), a nominal path that merely starts with `fn`, and a truly
    /// nameless self type (`fn()`/`()`) that reduces to a transparent container
    /// so the method is keyed bare. `ra_label` is the matching rust-analyzer
    /// impl label; `expected` is the shared container segment, `None` for the
    /// transparent case.
    #[test]
    fn impl_method_container_agrees_with_lsp_normalizer() {
        let extractor = SymbolExtractor::new();
        let cases: [(&str, &str, Option<&str>); 13] = [
            ("impl Foo { fn m(&self) {} }", "impl Foo", Some("Foo")),
            (
                "impl<T> Wrap<T> { fn m(&self) {} }",
                "impl<T> Wrap<T>",
                Some("Wrap"),
            ),
            (
                "impl std::fmt::Display for Foo { fn m(&self) {} }",
                "impl std::fmt::Display for Foo",
                Some("Foo"),
            ),
            (
                "impl FromStr for Foo { fn m(&self) {} }",
                "impl FromStr for Foo",
                Some("Foo"),
            ),
            (
                "impl Tr for (A, B) { fn m(&self) {} }",
                "impl Tr for (A, B)",
                Some("A"),
            ),
            (
                "impl Tr for [Elem; 4] { fn m(&self) {} }",
                "impl Tr for [Elem; 4]",
                Some("Elem"),
            ),
            (
                "impl Tr for *const Ptr { fn m(&self) {} }",
                "impl Tr for *const Ptr",
                Some("Ptr"),
            ),
            (
                "impl Tr for <Qual as Baz>::Out { fn m(&self) {} }",
                "impl Tr for <Qual as Baz>::Out",
                Some("Qual"),
            ),
            // a nominal path whose head merely starts with "fn" is not a fn-pointer
            (
                "impl Tr for fn_mod::Named { fn m(&self) {} }",
                "impl Tr for fn_mod::Named",
                Some("Named"),
            ),
            // primitive-element structural types: the one rule keeps the first
            // nominal word — the AST `type_identifier`-only descent that used to
            // skip primitives (and diverge here) is gone.
            (
                "impl Tr for [u8; 4] { fn m(&self) {} }",
                "impl Tr for [u8; 4]",
                Some("u8"),
            ),
            (
                "impl Tr for fn(u8) -> u8 { fn m(&self) {} }",
                "impl Tr for fn(u8) -> u8",
                Some("u8"),
            ),
            // truly nameless self types — transparent container, method keyed bare
            (
                "impl Tr for fn() { fn m(&self) {} }",
                "impl Tr for fn()",
                None,
            ),
            ("impl Tr for () { fn m(&self) {} }", "impl Tr for ()", None),
        ];
        for (src, ra_label, expected) in cases {
            let symbols = extractor.extract(Path::new("a"), src, Language::Rust);
            let method = symbols
                .iter()
                .find(|s| s.name == "m")
                .unwrap_or_else(|| panic!("method not extracted from {src:?}"));
            assert_eq!(
                method.container.as_deref(),
                expected,
                "index container for {src:?}"
            );
            // The LSP normalizer reduces the same self type; an empty segment is
            // the transparent (no-container) case the index represents as `None`.
            let norm = Symbol::normalize_symbol_name(ra_label);
            let lsp_container = (!norm.is_empty()).then_some(norm.as_str());
            assert_eq!(lsp_container, expected, "LSP normalizer for {ra_label:?}");
            // The stored name_path is the round-trip key the workspace producer
            // rebuilds from the same container segment.
            let expected_path = match expected {
                Some(c) => format!("{c}/m"),
                None => "m".to_string(),
            };
            assert_eq!(
                method.name_path.as_deref(),
                Some(expected_path.as_str()),
                "index name_path for {src:?}"
            );
        }
    }

    /// Modules organize but never qualify the addressing path: a method of a
    /// type nested in modules is keyed `Type/method` and a module-level free
    /// function bare — matching rust-analyzer's workspace-symbol container
    /// (which omits the enclosing module) so the index path round-trips against
    /// `symbols`/`edit`. The module prefix the AST carries is dropped here.
    #[test]
    fn index_drops_enclosing_module_from_name_path() {
        let extractor = SymbolExtractor::new();
        let src = "mod a { mod b { struct Deep; impl Deep { fn m(&self) {} } fn free() {} } }";
        let symbols = extractor.extract(Path::new("a"), src, Language::Rust);
        let path = |name: &str| {
            symbols
                .iter()
                .find(|s| s.name == name)
                .unwrap_or_else(|| panic!("{name} not extracted from {src:?}"))
                .name_path
                .clone()
        };
        assert_eq!(path("m"), Some("Deep/m".to_string()));
        assert_eq!(path("free"), Some("free".to_string()));
        // No producer keeps the enclosing module in the addressing path.
        for s in &symbols {
            let np = s.name_path.as_deref().unwrap_or_default();
            assert!(
                !np.starts_with("a/") && !np.contains("/a/") && !np.contains("/b/"),
                "module leaked into name_path: {np:?}"
            );
        }
    }

    /// A statement that holds declarations without declaring anything keys
    /// none of them, even where an identifier heads it: a definition under
    /// `if TYPE_CHECKING:`, in a loop, a `with`, an `except` or a `case` body
    /// is keyed by the class or function around it, as pyright keys it.
    #[test]
    fn a_python_statement_that_holds_a_declaration_does_not_key_it() {
        let extractor = SymbolExtractor::new();
        let src = "if TYPE_CHECKING:\n    def helper(): ...\n\nfor item in []:\n    def looped(): ...\n\n\
                   class C:\n    if TYPE_CHECKING:\n        def m(self): ...\n\n\
                   def outer():\n    with open('f') as handle:\n        def inner(): ...\n    \
                   try:\n        pass\n    except ValueError as error:\n        def caught(): ...\n\n\
                   match command:\n    case point:\n        def matched(): ...\n";
        let symbols = extractor.extract(Path::new("m.py"), src, Language::Python);
        let path = |name: &str| {
            symbols
                .iter()
                .find(|s| s.name == name)
                .unwrap_or_else(|| panic!("{name} not extracted from {src:?}"))
                .name_path
                .clone()
        };
        assert_eq!(path("helper").as_deref(), Some("helper"));
        assert_eq!(path("looped").as_deref(), Some("looped"));
        assert_eq!(path("m").as_deref(), Some("C/m"));
        assert_eq!(path("inner").as_deref(), Some("outer/inner"));
        assert_eq!(path("caught").as_deref(), Some("outer/caught"));
        assert_eq!(path("matched").as_deref(), Some("matched"));
    }

    /// A member of a type nested inside another type (and a namespace) is keyed
    /// by its IMMEDIATE container only — `Inner/method`, never
    /// `ns/Outer/Inner/method` — matching what clangd's workspace surface
    /// reports (`ns::Outer::Inner` → reduced to the nearest type `Inner`) so the
    /// index path round-trips. The namespace drops out and the outer type does
    /// not widen the path; the enclosing type still qualifies the inner type
    /// itself (`Outer/Inner`).
    #[test]
    fn index_keys_nested_type_member_by_immediate_container() {
        let extractor = SymbolExtractor::new();
        let src = "namespace ns { class Outer { class Inner { void method(); }; void om(); }; }";
        let symbols = extractor.extract(Path::new("a"), src, Language::Cpp);
        let path = |name: &str| {
            symbols
                .iter()
                .find(|s| s.name == name)
                .unwrap_or_else(|| panic!("{name} not extracted from {src:?}"))
                .name_path
                .clone()
        };
        assert_eq!(path("method"), Some("Inner/method".to_string()));
        assert_eq!(path("om"), Some("Outer/om".to_string()));
        assert_eq!(path("Inner"), Some("Outer/Inner".to_string()));
        // The namespace never appears in any addressing path.
        for s in &symbols {
            let np = s.name_path.as_deref().unwrap_or_default();
            assert!(
                !np.contains("ns/"),
                "namespace leaked into name_path: {np:?}"
            );
        }
    }

    /// A `namespace`/`module` in every grammar must be path-transparent, like a
    /// Rust `mod` — a class directly inside it stays bare on the index just as it
    /// does on the workspace/documentSymbol surfaces (which never report the
    /// namespace), so a copied path round-trips. Covers the node kinds beyond
    /// Rust/C++/Java: TS/JS `internal_module` (`namespace`/`module`) and C#
    /// `namespace_declaration`.
    #[test]
    fn index_keeps_namespace_path_transparent_across_languages() {
        let extractor = SymbolExtractor::new();
        let path = |symbols: &[Symbol], name: &str| {
            symbols
                .iter()
                .find(|s| s.name == name)
                .and_then(|s| s.name_path.clone())
        };

        // TypeScript: `namespace` and `module` both parse as `internal_module`.
        let ts = "namespace NS { export class Outer { method(): void {} } \
                  export function freeFn(): void {} } \
                  module Ambient { export class Thing { go(): void {} } }";
        let ts_syms = extractor.extract(Path::new("a"), ts, Language::TypeScript);
        assert_eq!(path(&ts_syms, "Outer"), Some("Outer".to_string()));
        assert_eq!(path(&ts_syms, "method"), Some("Outer/method".to_string()));
        assert_eq!(path(&ts_syms, "freeFn"), Some("freeFn".to_string()));
        assert_eq!(path(&ts_syms, "Thing"), Some("Thing".to_string()));

        // C#: block-scoped `namespace`. An enclosing type still qualifies the
        // inner type, but the namespace never does.
        let cs = "namespace MyApp { public class Outer { public void Method() {} \
                  public class Inner { public void InnerMethod() {} } } }";
        let cs_syms = extractor.extract(Path::new("a"), cs, Language::CSharp);
        assert_eq!(path(&cs_syms, "Outer"), Some("Outer".to_string()));
        assert_eq!(path(&cs_syms, "Method"), Some("Outer/Method".to_string()));
        assert_eq!(path(&cs_syms, "Inner"), Some("Outer/Inner".to_string()));
        assert_eq!(
            path(&cs_syms, "InnerMethod"),
            Some("Inner/InnerMethod".to_string())
        );

        for s in ts_syms.iter().chain(cs_syms.iter()) {
            let np = s.name_path.as_deref().unwrap_or_default();
            assert!(
                !np.starts_with("NS/") && !np.starts_with("Ambient/") && !np.starts_with("MyApp/"),
                "namespace leaked into name_path: {np:?}"
            );
        }
    }

    /// A symbol's recorded position must land on its NAME identifier, not the
    /// item's leading keyword — otherwise `refs`/`def` on the indexed position
    /// resolve to the wrong symbol (or nothing), and the index/LSP workspace
    /// passes can't dedup to one row.
    #[test]
    fn index_anchors_symbols_at_their_name() {
        let extractor = SymbolExtractor::new();
        let src = "pub fn alpha() {}\nstruct Bravo;\nimpl Bravo { pub fn charlie(&self) {} }\n";
        let syms = extractor.extract(Path::new("a"), src, Language::Rust);
        let on_name = |name: &str| {
            let s = syms
                .iter()
                .find(|s| s.name == name)
                .unwrap_or_else(|| panic!("{name} not extracted"));
            let line = src.lines().nth((s.location.line - 1) as usize).unwrap();
            let col0 = (s.location.column - 1) as usize;
            line[col0..].starts_with(name)
        };
        assert!(on_name("alpha"), "function anchored off its name");
        assert!(on_name("Bravo"), "struct anchored off its name");
        assert!(on_name("charlie"), "method anchored off its name");
    }

    #[test]
    fn go_type_declaration_classifies_by_underlying_type() {
        let extractor = SymbolExtractor::new();
        let content = r#"
func main() {}
type Config struct {}
type Reader interface {}
type Celsius float64
"#;
        let symbols = extractor.extract(Path::new("a"), content, Language::Go);
        let kind = |name: &str| symbols.iter().find(|s| s.name == name).map(|s| s.kind);
        assert_eq!(kind("main"), Some(SymbolKind::Function));
        assert_eq!(kind("Config"), Some(SymbolKind::Struct));
        assert_eq!(kind("Reader"), Some(SymbolKind::Interface));
        assert_eq!(kind("Celsius"), Some(SymbolKind::Class));
    }

    #[test]
    fn kotlin_object_is_a_class_not_a_variable() {
        let extractor = SymbolExtractor::new();
        let content = r#"
class Widget {}
object Singleton {}
fun build() {}
"#;
        let symbols = extractor.extract(Path::new("a"), content, Language::Kotlin);
        let kind = |name: &str| symbols.iter().find(|s| s.name == name).map(|s| s.kind);
        assert_eq!(kind("Widget"), Some(SymbolKind::Class));
        assert_eq!(kind("Singleton"), Some(SymbolKind::Class));
        assert_eq!(kind("build"), Some(SymbolKind::Function));
    }

    #[test]
    fn python_symbol_extraction() {
        let extractor = SymbolExtractor::new();
        let content = r#"
def hello():
    pass

class MyClass:
    pass
"#;
        let symbols = extractor.extract(Path::new("a"), content, Language::Python);
        let kind = |name: &str| symbols.iter().find(|s| s.name == name).map(|s| s.kind);
        assert_eq!(kind("hello"), Some(SymbolKind::Function));
        assert_eq!(kind("MyClass"), Some(SymbolKind::Class));
    }

    #[test]
    fn typescript_symbol_extraction() {
        let extractor = SymbolExtractor::new();
        let content = r#"
function greet() {}
class Service {}
interface Shape {}
enum Color { Red, Blue }
"#;
        let symbols = extractor.extract(Path::new("a"), content, Language::TypeScript);
        let kind = |name: &str| symbols.iter().find(|s| s.name == name).map(|s| s.kind);
        assert_eq!(kind("greet"), Some(SymbolKind::Function));
        assert_eq!(kind("Service"), Some(SymbolKind::Class));
        assert_eq!(kind("Shape"), Some(SymbolKind::Interface));
        assert_eq!(kind("Color"), Some(SymbolKind::Enum));
    }

    #[test]
    fn a_module_declares_each_variable_no_body_encloses() {
        let extractor = SymbolExtractor::new();
        let content = r#"
const greet = (x) => x;
export const handler = async () => {};
const fexpr = function named() {};
var legacy = () => {};
const gen = function* () {};
function* topgen() {}
const config = makeConfig();
export const VERSION = "1.0";
let counter = 0, total = 1;
const { a, b: renamed, c = 1, ...rest } = obj;
const [d, [e]] = arr;
function outer() {
    const inner = () => {};
    for (let i = 0; i < 10; i++) {}
}
"#;
        let namespaced = "namespace N {\n  const inN = 1;\n  let mutN = 2;\n}\n\
                          declare module \"m\" {\n  const inM: number;\n}\n";
        let symbols = extractor.extract(Path::new("a.ts"), namespaced, Language::TypeScript);
        let paths: Vec<String> = symbols
            .iter()
            .map(|s| format!("{}:{}", s.kind, s.name_path.as_deref().unwrap_or(&s.name)))
            .collect();
        for declared in ["constant:inN", "variable:mutN", "constant:inM"] {
            assert!(
                paths.iter().any(|path| path == declared),
                "{declared} in {paths:?}"
            );
        }
        for language in [Language::TypeScript, Language::JavaScript] {
            let symbols = extractor.extract(Path::new("a"), content, language);
            let kind = |name: &str| {
                let found: Vec<_> = symbols.iter().filter(|s| s.name == name).collect();
                assert!(found.len() <= 1, "{language:?} reads {name} twice");
                found.first().map(|s| s.kind)
            };
            for function in ["greet", "handler", "fexpr", "legacy", "gen", "topgen"] {
                assert_eq!(kind(function), Some(SymbolKind::Function), "{function}");
            }
            for constant in ["config", "VERSION", "a", "renamed", "c", "rest", "d", "e"] {
                assert_eq!(kind(constant), Some(SymbolKind::Constant), "{constant}");
            }
            for variable in ["counter", "total"] {
                assert_eq!(kind(variable), Some(SymbolKind::Variable), "{variable}");
            }
            for unbound in ["b", "obj", "arr", "inner", "i"] {
                assert_eq!(kind(unbound), None, "{language:?} declares {unbound}");
            }
        }
    }

    #[test]
    fn javascript_symbol_extraction() {
        let extractor = SymbolExtractor::new();
        let content = r#"
function greet() {}
class Service {}
"#;
        let symbols = extractor.extract(Path::new("a"), content, Language::JavaScript);
        let kind = |name: &str| symbols.iter().find(|s| s.name == name).map(|s| s.kind);
        assert_eq!(kind("greet"), Some(SymbolKind::Function));
        assert_eq!(kind("Service"), Some(SymbolKind::Class));
    }

    #[test]
    fn java_symbol_extraction() {
        let extractor = SymbolExtractor::new();
        let content = r#"
class Service {}
interface Shape {}
enum Color { RED, BLUE }
"#;
        let symbols = extractor.extract(Path::new("a"), content, Language::Java);
        let kind = |name: &str| symbols.iter().find(|s| s.name == name).map(|s| s.kind);
        assert_eq!(kind("Service"), Some(SymbolKind::Class));
        assert_eq!(kind("Shape"), Some(SymbolKind::Interface));
        assert_eq!(kind("Color"), Some(SymbolKind::Enum));
    }

    #[test]
    fn cpp_symbol_extraction() {
        let extractor = SymbolExtractor::new();
        let content = r#"
class Widget {};
struct Point {};
void run() {}
"#;
        let symbols = extractor.extract(Path::new("a"), content, Language::Cpp);
        let kind = |name: &str| symbols.iter().find(|s| s.name == name).map(|s| s.kind);
        assert_eq!(kind("Widget"), Some(SymbolKind::Class));
        assert_eq!(kind("Point"), Some(SymbolKind::Struct));
        assert_eq!(kind("run"), Some(SymbolKind::Function));
    }

    #[test]
    fn csharp_symbol_extraction() {
        let extractor = SymbolExtractor::new();
        let content = r#"
class Service {}
interface IShape {}
struct Point {}
enum Color { Red }
"#;
        let symbols = extractor.extract(Path::new("a"), content, Language::CSharp);
        let kind = |name: &str| symbols.iter().find(|s| s.name == name).map(|s| s.kind);
        assert_eq!(kind("Service"), Some(SymbolKind::Class));
        assert_eq!(kind("IShape"), Some(SymbolKind::Interface));
        assert_eq!(kind("Point"), Some(SymbolKind::Struct));
        assert_eq!(kind("Color"), Some(SymbolKind::Enum));
    }

    #[test]
    fn php_symbol_extraction() {
        let extractor = SymbolExtractor::new();
        let content = r#"<?php
function greet() {}
class Service {}
interface Shape {}
"#;
        let symbols = extractor.extract(Path::new("a"), content, Language::PHP);
        let kind = |name: &str| symbols.iter().find(|s| s.name == name).map(|s| s.kind);
        assert_eq!(kind("greet"), Some(SymbolKind::Function));
        assert_eq!(kind("Service"), Some(SymbolKind::Class));
        assert_eq!(kind("Shape"), Some(SymbolKind::Interface));
    }

    /// A declaration inside a function or a method, or inside a body a
    /// declaration encloses (a const's closure, a class's initializer block),
    /// is that body's; one in a body nothing declared encloses (a top-level
    /// callback, a function called where it is written) stands for itself.
    /// The index keeps reading every one; the member reading leaves the
    /// body's own out.
    #[test]
    fn members_leave_out_what_is_declared_inside_a_body() {
        let extractor = SymbolExtractor::new();
        let cases: &[(Language, &str, &str, &[&str], &[&str])] = &[
            (
                Language::Kotlin,
                "a.kt",
                "class Svc(val id: Int) {\n    val size = 1\n    init {\n        fun doubled() = id * 2\n    }\n    constructor(n: String) : this(n.length) {\n        fun parsed() = n\n    }\n    val label: String\n        get() {\n            fun prefix() = \"x\"\n            return prefix()\n        }\n    fun run(): Int {\n        fun total() = 1\n        val f = { fun inner() = 2; inner() }\n        return total()\n    }\n}\n\nfun top(): Int {\n    fun local() = 2\n    return local()\n}\n",
                &["Svc", "id", "size", "constructor", "label", "run", "top"],
                &["doubled", "parsed", "prefix", "total", "inner", "local"],
            ),
            (
                Language::TypeScript,
                "a.ts",
                "export class C {\n  static {\n    function boot() {}\n  }\n  method() {\n    function helper() {}\n  }\n}\n\ndescribe(\"x\", () => {\n  function makeInput() {}\n});\n\n(function () {\n  function once() {}\n})();\n\nexport const handler = () => {\n  function inner() {}\n};\n\nconst f = function () {\n  function nested() {}\n};\n",
                &["C", "method", "makeInput", "once", "handler", "f"],
                &["boot", "helper", "inner", "nested"],
            ),
            (
                Language::Go,
                "a.go",
                "package main\n\nvar Top = 1\n\nfunc f() int {\n\ttype total struct{}\n\treturn 1\n}\n\nvar handler = func() int {\n\ttype x int\n\treturn 1\n}\n",
                &["Top", "f", "handler"],
                &["total", "x"],
            ),
            (
                Language::Python,
                "a.py",
                "class A:\n    def m(self):\n        def helper():\n            pass\n        return helper\n\n\ndef outer():\n    class Local:\n        pass\n    return Local\n",
                &["A", "m", "outer"],
                &["helper", "Local"],
            ),
            (
                Language::Java,
                "A.java",
                "class A {\n    void m() {\n        Runnable r = () -> {\n            class InLambda {}\n        };\n    }\n    static {\n        class InStatic {}\n    }\n    A() {\n        class InCtor {}\n    }\n}\n",
                &["A", "m"],
                &["InLambda", "InStatic", "InCtor"],
            ),
            (
                Language::Dart,
                "a.dart",
                "int add(int a, int b) {\n  int twice(int x) => x * 2;\n  return twice(a) + b;\n}\n\nclass Cart {\n  int total = 0;\n\n  static Cart empty() => Cart();\n\n  void put(int n) {\n    total += n;\n  }\n}\n",
                &["add", "Cart", "total", "empty", "put"],
                &["twice"],
            ),
            (
                Language::Ruby,
                "a.rb",
                "module Concern\n  class_methods do\n    def build\n    end\n  end\n\n  def run\n    [1].map do |x|\n      x\n    end\n  end\nend\n",
                &["Concern", "build", "run"],
                &[],
            ),
            (
                Language::Rust,
                "a.rs",
                "fn outer() {\n    fn inner() {}\n    let c = || {\n        fn in_closure() {}\n    };\n}\n\nstruct S;\n\nimpl S {\n    fn m(&self) {}\n}\n",
                &["outer", "S", "m"],
                &["inner", "in_closure"],
            ),
        ];
        for (language, path, source, members, locals) in cases {
            let names = |symbols: Vec<Symbol>| -> std::collections::BTreeSet<String> {
                symbols.into_iter().map(|symbol| symbol.name).collect()
            };
            let every = names(extractor.extract(Path::new(path), source, *language));
            let kept = names(extractor.extract_members(Path::new(path), source, *language));
            for local in *locals {
                assert!(
                    every.contains(*local),
                    "{language:?} reads {local}: {every:?}"
                );
            }
            let expected: std::collections::BTreeSet<String> =
                members.iter().map(|m| m.to_string()).collect();
            assert_eq!(kept, expected, "{language:?}");
        }
    }
}
