use std::collections::{BTreeSet, HashMap, HashSet};
use std::path::Path;
use std::sync::Mutex;

use streaming_iterator::StreamingIterator;
use tree_sitter::{Node, Parser, Query, QueryCursor};

use crate::models::symbol::{Language, Location, Symbol, SymbolKind};

/// A parser and its pre-compiled extraction query for one language. The
/// query is compiled once at startup, not per file — query compilation is
/// the dominant per-call cost otherwise.
struct LanguageEntry {
    parser: Mutex<Parser>,
    query: Query,
}

/// Tree-sitter symbol extraction for the indexed store. One registry maps
/// each supported language to its parser and compiled query; adding a
/// language is a single `register` call.
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
    /// The index records it, because rows are only as good as the queries that
    /// produced them: a file whose bytes have not changed is skipped by a
    /// rebuild, so an index built by an earlier extractor would keep serving
    /// its rows — an answer short by exactly the declaration forms the new one
    /// was written to reach, with nothing to say so. Changing a query is
    /// therefore enough to retire a build, and no version has to be remembered.
    pub fn extraction_digest() -> u64 {
        let described = EXTRACTORS
            .iter()
            .map(|(language, _, query)| format!("{}\u{1}{query}", language.lsp_id()))
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

    /// The declarations of [`extract`](Self::extract) that stand for
    /// themselves: none that belongs to a body a declaration stands for
    /// ([`is_local`]). What is declared inside such a body is its own, and a
    /// change to it is a change to the body.
    pub fn extract_members(&self, path: &Path, content: &str, language: Language) -> Vec<Symbol> {
        self.extract_where(path, content, language, |node, declared| {
            !is_local(node, declared)
        })
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
        let entry = self.languages.get(&language)?;
        let tree = entry.parser.lock().ok()?.parse(content, None)?;
        let mut cursor = QueryCursor::new();
        let mut declarations = Vec::new();
        let mut matches = cursor.matches(&entry.query, tree.root_node(), content.as_bytes());
        while let Some(m) = matches.next() {
            if let Some(capture) = m.captures().first() {
                for symbol in extract_from_match(m, path, content, language) {
                    declarations.push((capture.node, symbol));
                }
            }
        }
        Some(read(declarations))
    }
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
/// grammar reads any of them (it reads no TypeScript enum member and no Rust
/// associated type): the grammar's `body`, or where a grammar gives it no
/// field, a Go type's literal (`struct {…}`, `interface {…}`), a Kotlin
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
/// constructors, accessors and initializer blocks. A kind is listed once for
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
    "local_function_statement",
    // Go, Rust
    "func_literal",
    "closure_expression",
    // Java, C#, Kotlin
    "constructor_declaration",
    "static_initializer",
    "accessor_declaration",
    "anonymous_initializer",
    "secondary_constructor",
    "lambda_literal",
    "getter",
    "setter",
];

/// Whether a declaration belongs to a body rather than standing for itself:
/// a function or method encloses it, or an [`ANONYMOUS_BODIES`] body does that
/// a declaration in turn encloses (a const's arrow function, a class's `init`
/// block). A body no declaration encloses (a callback passed at the top of a
/// file, a function called where it is written) absorbs nothing, since
/// nothing would stand for what it holds. Callable nodes reached from the
/// declaration through callable nodes alone are the declaration itself —
/// Dart's query reads a method's signature, which its method signature and
/// declaration wrap — while a body holds a declaration in a block of
/// statements.
fn is_local(node: Node, declared: &std::collections::HashSet<usize>) -> bool {
    let mut own = true;
    let mut in_body = false;
    let mut ancestor = node.parent();
    while let Some(outer) = ancestor {
        if node_kind(outer).is_callable() {
            if !own {
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
        ancestor = outer.parent();
    }
    false
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
    languages.insert(
        language,
        LanguageEntry {
            parser: Mutex::new(parser),
            query,
        },
    );
}

/// The symbols a query match declares: its one name, or each name a
/// statement declaring several states ([`declared_names`]), all over the
/// statement's one range — or each over its name alone where the statement
/// also assigns to something else ([`assigns_beyond_its_names`]).
fn extract_from_match(
    m: &tree_sitter::QueryMatch,
    path: &Path,
    content: &str,
    language: Language,
) -> Vec<Symbol> {
    let Some(node) = m.captures().first().map(|capture| capture.node) else {
        return Vec::new();
    };
    let names = declared_names(node, language);
    let named: Vec<(String, SymbolKind, Node)> = if names.len() > 1 {
        names
            .into_iter()
            .filter_map(|name| {
                let text = content.get(name.start_byte()..name.end_byte())?;
                (!names_nothing(text)).then(|| (text.to_string(), node_kind(node), name))
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
    let container = extract_container_path(node, content, language);

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
            let (name_end_line, name_end_column) =
                scalar_position(content, anchor.end_byte(), anchor.end_position());
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
            let mut symbol = Symbol::new(name, kind, location);
            symbol.name_path = name_path;
            symbol.container = container.clone();
            symbol
        })
        .collect()
}

/// The node that spans the whole of a declaration: the one the grammar
/// gives it, or a wrapper that holds it with more of its syntax — a TS/JS
/// `export` statement (decorators on it included), a TypeScript `declare`, a
/// `const`, `let` or `var` statement declaring it alone, a Python
/// `decorated_definition`, a C++ `template` header or `extern "C"`, a Go
/// `type`, `const` or `var` declaring it alone, and a Dart function, method
/// or class member, which holds its annotations and body beside the
/// signature the query reads.
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
            (Language::Go, "type_declaration" | "const_declaration" | "var_declaration") => alone(),
            (Language::Dart, "function_declaration" | "method_declaration") => holds("signature"),
            (Language::Dart, "method_signature" | "class_member") => true,
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
/// [`extract_from_match`] reads each as a symbol of its own over that one
/// range. Listed per language, as each grammar holds the names differently.
fn declared_names(node: Node, language: Language) -> Vec<Node> {
    let fields = |field| {
        let mut cursor = node.walk();
        node.children_by_field_name(field, &mut cursor)
            .collect::<Vec<_>>()
    };
    match language {
        Language::Go | Language::Swift => fields("name"),
        Language::Java => fields("declarator")
            .into_iter()
            .filter_map(|declarator| declarator.child_by_field_name("name"))
            .collect(),
        Language::Cpp => fields("declarator")
            .into_iter()
            .map(|declarator| {
                declarator
                    .child_by_field_name("declarator")
                    .unwrap_or(declarator)
            })
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
        .collect(),
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
}

/// Whether an assignment's target is a name it declares, rather than an
/// attribute, a subscript or a Ruby instance variable it assigns to.
fn binds_a_name(target: Node, language: Language) -> bool {
    match language {
        Language::Python => target.kind() == "identifier",
        Language::Ruby => matches!(target.kind(), "constant" | "identifier"),
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
        && assignment_targets(node).any(|target| !binds_a_name(target, language))
}

/// Where a declaration's text begins: its node ([`declaration_node`]), or
/// the lines before it among its siblings that belong to it — a TypeScript
/// class member's decorators, a Rust item's outer attributes and doc comments
/// (`///` is `#[doc]`). A comment between those lines is inside the
/// declaration; one above them is not.
fn declaration_start(node: Node, language: Language) -> Node {
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

fn extract_container_path(mut node: Node, content: &str, language: Language) -> Option<String> {
    // HCL qualifies a name with the type label written before it
    // (`google_storage_bucket.logs`), which is what tells two resources named
    // `this` apart. Nothing encloses a top-level block to walk up to.
    if language == Language::Terraform {
        return hcl_type_label(node, content);
    }

    // A symbol is keyed by its IMMEDIATE container only — the nearest enclosing
    // type/impl — so a method reads `Type/method`, an enclosing outer type or
    // module never widens it to `Outer/Inner/method`, and a module-level item
    // stays bare. This matches what the LSP workspace surface can report (a
    // method's container is its nearest type; outer types and namespaces are
    // flattened away there), so a name_path round-trips across the index,
    // documentSymbol, and workspace surfaces. Modules/namespaces/packages
    // qualify nothing and are skipped on the way up to that nearest type.
    while let Some(parent) = node.parent() {
        node = parent;
        if let Some((name, kind)) = extract_name_and_kind(node, content, language)
            && !name.is_empty()
            && !kind.is_namespace_like()
        {
            return Some(name);
        }
    }
    None
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
        .get(name_node.start_byte()..name_node.end_byte())?
        .to_string();
    if names_nothing(&name) {
        return None;
    }
    Some((name, node_kind(node)))
}

/// Whether a declared name is no name at all: empty, or the blank `_` that
/// discards what is bound to it.
fn names_nothing(name: &str) -> bool {
    name.is_empty() || name == "_"
}

/// The first child of `node` with the given grammar kind. Grammars that wrap a
/// declaration's name in unnamed intermediate nodes are read through this
/// rather than by position, which moves whenever modifiers or attributes are
/// written before it.
fn child_of_kind<'a>(node: Node<'a>, kind: &str) -> Option<Node<'a>> {
    (0..node.child_count()).find_map(|i| node.child(i).filter(|c| c.kind() == kind))
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
            .or_else(|| node.child_by_field_name("name")),
        _ => node
            .child_by_field_name("name")
            .or_else(|| declared_names(node, language).into_iter().next()),
    };

    name_field.or_else(|| {
        let count = node.child_count();
        for i in 0..count {
            if let Some(child) = node.child(i) {
                let kind = child.kind();
                if matches!(
                    kind,
                    "identifier"
                        | "name"
                        | "simple_identifier"
                        | "type_identifier"
                        | "property_identifier"
                ) {
                    return Some(child);
                }
            }
        }
        None
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
        | "protocol_function_declaration" => SymbolKind::Method,

        "constructor_signature" => SymbolKind::Constructor,

        "class_declaration"
        | "abstract_class_declaration"
        | "record_declaration"
        | "delegate_declaration"
        | "class_definition"
        | "class_specifier"
        | "object_declaration"
        | "object_definition"
        | "extension_declaration"
        | "class"
        | "impl_item" => SymbolKind::Class,

        "struct_item" | "struct_specifier" | "struct_type" | "struct_declaration"
        | "union_item" => SymbolKind::Struct,

        "enum_item" | "enum_declaration" | "enum_specifier" => SymbolKind::Enum,

        "enum_variant" => SymbolKind::EnumMember,

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
        "mod_item"
        | "namespace_definition"
        | "package_declaration"
        | "internal_module"
        | "module"
        | "namespace_declaration"
        | "file_scoped_namespace_declaration" => SymbolKind::Module,

        // Go `type X = ...`: classify by the spec's underlying type.
        "type_spec" => go_type_kind(node),

        // A named type alias introduces a type, not a generic `<T>` param.
        "type_item" | "type_alias_declaration" | "type_alias" => SymbolKind::Class,

        "const_item" | "const_spec" | "val_definition" | "const_declaration" => {
            SymbolKind::Constant
        }

        "static_item" | "var_spec" | "var_definition" => SymbolKind::Variable,

        // JS/TS `const f = () => {}` is a callable; classify by the
        // initializer rather than always Variable (which is_low_level would
        // drop under exclude_low_level).
        "variable_declarator" => declarator_kind(node),

        // Python states a named value with a bare binding: no keyword separates
        // a constant from a variable, so nothing structural does either.
        "assignment" => SymbolKind::Variable,

        "property_declaration"
        | "public_field_definition"
        | "field_definition"
        | "property_signature" => SymbolKind::Property,
        "field_declaration" | "declaration" => field_kind(node),

        _ => SymbolKind::Variable,
    }
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

/// Classify a member declaration by its declarator: C++ states a method
/// declaration with the same `field_declaration` node it states a data member
/// with, and only the declarator separates them. Java, whose fields use the
/// same node kind, declares methods elsewhere and is unaffected.
fn field_kind(node: Node) -> SymbolKind {
    match node.child_by_field_name("declarator").map(|d| d.kind()) {
        Some("function_declarator") => SymbolKind::Method,
        _ => SymbolKind::Field,
    }
}

/// Classify a JS/TS `variable_declarator` by its initializer: a function value
/// (arrow function, function expression, or generator function) is a callable
/// Function; anything else is a plain Variable. Mirrors `go_type_kind`'s
/// value-field dispatch and keeps the decision structural — no name heuristics.
fn declarator_kind(node: Node) -> SymbolKind {
    match node.child_by_field_name("value").map(|v| v.kind()) {
        Some("arrow_function") | Some("function_expression") | Some("generator_function") => {
            SymbolKind::Function
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
(enum_item) @symbol
(trait_item) @symbol
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
(const_declaration (const_spec) @symbol)
(var_declaration (var_spec) @symbol)
"#;

// A binding's node is the same at module scope, in a class body, and inside a
// function, so members are matched where they are declared. The names are
// the identifiers the assignment's `left` binds, through a chain as well
// (`a = b = None`, see `declared_names`); a destructuring bind states its names
// in one pattern (`a, b = …`), which is not read.
const PYTHON_QUERY: &str = r#"
(function_definition) @symbol
(class_definition) @symbol
(module (expression_statement (assignment) @symbol))
(class_definition (block (expression_statement (assignment) @symbol)))
"#;

// Module-scope `const f = () => {}` / `export const f = function () {}` is the
// dominant TS/JS function form, but it parses as a variable_declarator, not a
// function_declaration. Capture it — anchored to module scope and filtered to a
// function-valued initializer — so it is indexed without dragging in nested
// locals, loop counters, or destructuring patterns. Both lexical_declaration
// (const/let) and variable_declaration (var), bare and export-wrapped.
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
(class_body (public_field_definition) @symbol)
(program (lexical_declaration (variable_declarator value: [(arrow_function) (function_expression) (generator_function)]) @symbol))
(program (variable_declaration (variable_declarator value: [(arrow_function) (function_expression) (generator_function)]) @symbol))
(program (export_statement (lexical_declaration (variable_declarator value: [(arrow_function) (function_expression) (generator_function)]) @symbol)))
(program (export_statement (variable_declaration (variable_declarator value: [(arrow_function) (function_expression) (generator_function)]) @symbol)))
"#;

const JAVASCRIPT_QUERY: &str = r#"
(function_declaration) @symbol
(generator_function_declaration) @symbol
(class_declaration) @symbol
(method_definition) @symbol
(class_body (field_definition) @symbol)
(program (lexical_declaration (variable_declarator value: [(arrow_function) (function_expression) (generator_function)]) @symbol))
(program (variable_declaration (variable_declarator value: [(arrow_function) (function_expression) (generator_function)]) @symbol))
(program (export_statement (lexical_declaration (variable_declarator value: [(arrow_function) (function_expression) (generator_function)]) @symbol)))
(program (export_statement (variable_declaration (variable_declarator value: [(arrow_function) (function_expression) (generator_function)]) @symbol)))
"#;

const JAVA_QUERY: &str = r#"
(package_declaration) @symbol
(class_declaration) @symbol
(record_declaration) @symbol
(interface_declaration) @symbol
(annotation_type_declaration) @symbol
(enum_declaration) @symbol
(method_declaration) @symbol
(field_declaration) @symbol
"#;

const KOTLIN_QUERY: &str = r#"
(class_declaration) @symbol
(object_declaration) @symbol
(function_declaration) @symbol
(property_declaration) @symbol
"#;

// A member declared in a class body is a `field_declaration`, and one a
// `template` header heads is a plain `declaration` inside the template.
const CPP_QUERY: &str = r#"
(function_definition) @symbol
(class_specifier) @symbol
(struct_specifier) @symbol
(enum_specifier) @symbol
(namespace_definition) @symbol
(field_declaration) @symbol
(field_declaration_list (template_declaration (declaration) @symbol))
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
(method_declaration) @symbol
(property_declaration) @symbol
(field_declaration) @symbol
"#;

const RUBY_QUERY: &str = r#"
(module) @symbol
(class) @symbol
(method) @symbol
(singleton_method) @symbol
(program (assignment) @symbol)
(module (body_statement (assignment) @symbol))
(class (body_statement (assignment) @symbol))
"#;

const BASH_QUERY: &str = r#"
(function_definition) @symbol
"#;

const LUA_QUERY: &str = r#"
(function_declaration) @symbol
"#;

// A Swift `let`/`var` inside a function body parses as the same
// `property_declaration` a stored property does, so members are matched
// where they are declared — directly in a type body or at file scope.
const SWIFT_QUERY: &str = r#"
(class_declaration) @symbol
(protocol_declaration) @symbol
(function_declaration) @symbol
(protocol_function_declaration) @symbol
(class_body (property_declaration) @symbol)
(source_file (property_declaration) @symbol)
"#;

// `val`/`var` share one node kind with their function-local counterparts;
// the template body is what separates a member from a local.
const SCALA_QUERY: &str = r#"
(class_definition) @symbol
(object_definition) @symbol
(trait_definition) @symbol
(function_definition) @symbol
(function_declaration) @symbol
(template_body (val_definition) @symbol)
(template_body (var_definition) @symbol)
"#;

// Dart names a function on its signature, not on the declaration that
// wraps it, and a method's signature nests the same node.
const DART_QUERY: &str = r#"
(class_declaration) @symbol
(enum_declaration) @symbol
(mixin_declaration) @symbol
(extension_declaration) @symbol
(function_signature) @symbol
(constructor_signature) @symbol
(class_body (_ (declaration) @symbol))
"#;

// HCL states a declaration as a block carrying quoted labels — `variable "x"`,
// `resource "type" "name"` — at any depth. A block with no label configures its
// parent rather than declaring anything. Which attributes declare is not a
// shape (see `hcl_attribute_declares`), so they are all offered and read there.
const TERRAFORM_QUERY: &str = r#"
(block (string_lit)) @symbol
(body (attribute) @symbol)
"#;

const PHP_QUERY: &str = r#"
(namespace_definition) @symbol
(function_definition) @symbol
(class_declaration) @symbol
(class_declaration (declaration_list (const_declaration) @symbol))
(enum_declaration) @symbol
(interface_declaration) @symbol
(trait_declaration) @symbol
(method_declaration) @symbol
(property_declaration) @symbol
"#;

#[cfg(test)]
mod tests {
    use super::*;

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
            language: Language::Rust,
            path: "a.rs",
            source: r#"
pub trait T { fn tm(&self); }
pub union U { a: u32 }
macro_rules! mac { () => {} }
pub type Al = u32;
pub const CX: u32 = 1;
pub static ST: u32 = 2;
pub enum E { A }
pub mod inner { pub fn nested() {} }
pub struct S { pub inner_field: u32 }
impl S { pub fn m(&self) { let local = 1; } }
"#,
            declares: &[
                "interface:T",
                "function:T/tm",
                "struct:U",
                "function:mac",
                "class:Al",
                "constant:CX",
                "variable:ST",
                "enum:E",
                "enum_member:E/A",
                "field:U/a",
                "module:inner",
                "function:nested",
                "struct:S",
                "field:S/inner_field",
                "class:S",
                "function:S/m",
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

const CX = 1
var VY = 2

func F() {}
func (s *S) M() {}
"#,
            declares: &[
                "class:Al",
                "struct:S",
                "field:S/F",
                "interface:I",
                "constant:CX",
                "variable:VY",
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

class A:
    attr: int = 1

    def m(self):
        local = 5
        return local

def f(): pass

async def af(): pass
"#,
            declares: &[
                "variable:MODULE_CONST",
                "variable:annotated",
                "variable:bare_ann",
                "class:A",
                "variable:A/attr",
                "function:A/m",
                "function:f",
                "function:af",
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
export enum Color { Red }
export class C {
    static SC = 1;
    private priv: number = 2;
    m(): void {}
}
export function f(): void {}
export const arrow = () => 1;
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
                "class:C",
                "property:C/SC",
                "property:C/priv",
                "method:C/m",
                "function:f",
                "function:arrow",
            ],
        },
        DeclarationFixture {
            language: Language::JavaScript,
            path: "a.js",
            source: r#"
export class C {
    static SC = 1;
    inst = 2;
    m() {}
}
export function f() {}
export const arrow = () => 1;
function* gen() {}
"#,
            declares: &[
                "class:C",
                "property:C/SC",
                "property:C/inst",
                "method:C/m",
                "function:f",
                "function:arrow",
                "function:gen",
            ],
        },
        DeclarationFixture {
            language: Language::Java,
            path: "A.java",
            source: r#"
package p;

record R(int a) {}

@interface Ann {}

interface I { void im(); }

enum E { X }

class C {
    int field;
    void m() {}
}
"#,
            declares: &[
                "module:p",
                "class:R",
                "interface:Ann",
                "interface:I",
                "method:I/im",
                "enum:E",
                "class:C",
                "field:C/field",
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
        void M() {}
    }
}
"#,
            declares: &[
                "module:N",
                "class:Rec",
                "class:D",
                "interface:I",
                "method:I/M",
                "struct:St",
                "field:St/F",
                "enum:E",
                "class:C",
                "property:C/P",
                "method:C/M",
            ],
        },
        DeclarationFixture {
            language: Language::PHP,
            path: "a.php",
            source: r#"
<?php
namespace N;

enum E { case X; }

trait T { public function tm() {} }

interface I { public function im(); }

abstract class A {
    const AX = 1;
    public $prop;
    abstract public function am();
}

function f() {}
"#,
            declares: &[
                "module:N",
                "enum:E",
                "interface:T",
                "method:T/tm",
                "interface:I",
                "method:I/im",
                "class:A",
                "constant:A/AX",
                "property:A/prop",
                "method:A/am",
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

class Widget {
    val name = "w"
    fun render() {}
}

fun top() {}
"#,
            declares: &[
                "class:I",
                "function:I/im",
                "class:Registry",
                "function:Registry/register",
                "class:Widget",
                "property:Widget/name",
                "function:Widget/render",
                "function:top",
            ],
        },
        DeclarationFixture {
            language: Language::Cpp,
            path: "a.cpp",
            source: r#"
namespace N {

struct S { int f; };

class C {
public:
    void m();
    template <typename U>
    U make();
};

enum E { X };

void f() {}

}
"#,
            declares: &[
                "module:N",
                "struct:S",
                "field:S/f",
                "class:C",
                "method:C/m",
                "method:C/make",
                "enum:E",
                "function:f",
            ],
        },
        DeclarationFixture {
            language: Language::Ruby,
            path: "a.rb",
            source: r#"
TOP_CONST = 1

module M
  IN_MODULE = 2

  class C
    IN_CLASS = 3

    def m; end
    def self.cm; end
  end
end
"#,
            declares: &[
                "variable:TOP_CONST",
                "module:M",
                "variable:IN_MODULE",
                "class:C",
                "variable:C/IN_CLASS",
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
function top() end

local function helper() end
"#,
            declares: &["function:top", "function:helper"],
        },
        DeclarationFixture {
            language: Language::Swift,
            path: "a.swift",
            source: r#"
protocol P {
    func pm()
}

class C {
    var p = 1
    func cm() {}
}

func top() {}
"#,
            declares: &[
                "interface:P",
                "method:P/pm",
                "class:C",
                "property:C/p",
                "function:C/cm",
                "function:top",
            ],
        },
        DeclarationFixture {
            language: Language::Scala,
            path: "a.scala",
            source: r#"
trait T {
  def tm(): Unit
}

object O {
  val v = 1
}

class C {
  def cm(): Unit = {}
}
"#,
            declares: &[
                "interface:T",
                "function:T/tm",
                "class:O",
                "constant:O/v",
                "class:C",
                "function:C/cm",
            ],
        },
        DeclarationFixture {
            language: Language::Dart,
            path: "a.dart",
            source: r#"
mixin M {}

enum E { x }

abstract class A {
  void am();
}

class C {
  int field = 1;
  static const sc = 2;
  C();
  void cm() {}
}

void top() {}
"#,
            declares: &[
                "interface:M",
                "enum:E",
                "class:A",
                "function:A/am",
                "class:C",
                "field:C/field",
                "field:C/sc",
                "constructor:C/C",
                "function:C/cm",
                "function:top",
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
}

module "network" {
  source = "./modules/network"
}

output "bucket_url" {
  value = google_storage_bucket.logs.url
}
"#,
            declares: &[
                "struct:gcs",
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
                    int top() {\n  return 2;\n}\n";
        let spans = |name: &str| {
            extractor
                .extract(Path::new("a.dart"), dart, Language::Dart)
                .into_iter()
                .find(|symbol| symbol.name == name)
                .map(|symbol| (symbol.location.effective_start(), symbol.location.end_line))
                .unwrap()
        };
        assert_eq!(spans("toString"), ((2, 3), Some(5)));
        assert_eq!(spans("top"), ((8, 1), Some(10)));
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
             const X = 1\nvar y = 2\n",
            Language::Go,
        );
        assert_eq!(span(&go, "Foo"), ((3, 1), (5, 2)));
        assert_eq!(span(&go, "B"), ((8, 2), (8, 7)));
        assert_eq!(span(&go, "X"), ((11, 1), (11, 12)));
        assert_eq!(span(&go, "y"), ((12, 1), (12, 10)));
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
                "a.rb",
                Language::Ruby,
                "UNUSED = SHARED = 1\nONE = 1\n",
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
            ("a.rs", "pub trait Tr {\n    type Out;\n}\n", Language::Rust),
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
                "a.swift",
                "protocol Keyed {\n    associatedtype Key: Hashable\n    var key: Key { get }\n}\n",
                Language::Swift,
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
        assert_eq!(found("Order/pay"), Some(SymbolKind::Function));
        assert_eq!(found("Status"), Some(SymbolKind::Enum));
        assert_eq!(found("Loggable"), Some(SymbolKind::Interface));
        assert_eq!(found("topLevel"), Some(SymbolKind::Function));
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
    fn typescript_module_scope_function_declarators_are_functions() {
        let extractor = SymbolExtractor::new();
        let content = r#"
const greet = (x: number) => x;
export const handler = async () => {};
const fexpr = function named() {};
var legacy = () => {};
const gen = function* () {};
function* topgen() {}
const config = makeConfig();
const VERSION = "1.0";
const klass = class {};
const { a, b } = obj;
const [c, d] = arr;
function outer() {
    const inner = () => {};
    for (let i = 0; i < 10; i++) {}
}
"#;
        let symbols = extractor.extract(Path::new("a"), content, Language::TypeScript);
        let kind = |name: &str| symbols.iter().find(|s| s.name == name).map(|s| s.kind);
        // Module-scope function-valued declarators are Functions (callable),
        // not low-level Variables.
        assert_eq!(kind("greet"), Some(SymbolKind::Function));
        assert_eq!(kind("handler"), Some(SymbolKind::Function));
        assert_eq!(kind("fexpr"), Some(SymbolKind::Function));
        assert_eq!(kind("legacy"), Some(SymbolKind::Function));
        // A generator expression is a callable function value, indexed like the
        // arrow and function-expression forms.
        assert_eq!(kind("gen"), Some(SymbolKind::Function));
        // A top-level generator declaration (`function* g(){}`) is a distinct
        // node kind from `function_declaration` — indexed as a Function too.
        assert_eq!(kind("topgen"), Some(SymbolKind::Function));
        // Non-function initializers are never captured by the value-filtered
        // query (they would only ever be Variables, which are not indexed here).
        assert_eq!(kind("config"), None);
        assert_eq!(kind("VERSION"), None);
        assert_eq!(kind("klass"), None);
        // Destructuring patterns never emit a brace-named symbol.
        assert_eq!(kind("a"), None);
        assert_eq!(kind("b"), None);
        // Nested locals and loop counters are excluded by the module-scope
        // anchor, so JS/TS indexing stays free of per-statement noise.
        assert_eq!(kind("inner"), None);
        assert_eq!(kind("i"), None);
        // A function-valued declarator is classified Function, which is callable
        // (not is_low_level), so it survives exclude_low_level — a plain Variable
        // would not.
        assert!(!SymbolKind::Function.is_low_level());
        assert!(kind("greet").is_some_and(|k| !k.is_low_level()));
    }

    #[test]
    fn javascript_module_scope_const_arrows_match_typescript() {
        let extractor = SymbolExtractor::new();
        let content = r#"
const greet = () => {};
export const handler = function () {};
function outer() {
    const inner = () => {};
}
"#;
        let symbols = extractor.extract(Path::new("a"), content, Language::JavaScript);
        let kind = |name: &str| symbols.iter().find(|s| s.name == name).map(|s| s.kind);
        assert_eq!(kind("greet"), Some(SymbolKind::Function));
        assert_eq!(kind("handler"), Some(SymbolKind::Function));
        // The bare (variable_declarator) capture is gone: no nested-local noise.
        assert_eq!(kind("inner"), None);
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
                "class Svc(val id: Int) {\n    val size = 1\n    init {\n        val doubled = id * 2\n    }\n    constructor(n: String) : this(n.length) {\n        val parsed = n\n    }\n    val label: String\n        get() {\n            val prefix = \"x\"\n            return prefix\n        }\n    fun run(): Int {\n        val total = 1\n        val f = { val inner = 2; inner }\n        return total\n    }\n}\n\nfun top(): Int {\n    val local = 2\n    return local\n}\n",
                &["Svc", "size", "label", "run", "top"],
                &["doubled", "parsed", "prefix", "total", "local"],
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
                "package main\n\nvar Top = 1\n\nfunc f() int {\n\tvar total = 1\n\treturn total\n}\n\nvar handler = func() int {\n\tvar x = 1\n\treturn x\n}\n",
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
