//! Domain types for symbols, kinds, languages, and source locations.
//!
//! Each top-level type lives in its own submodule but is re-exported from
//! `crate::models::symbol::*` so callers don't see the split.

mod kind;
mod language;
mod location;

pub use kind::SymbolKind;
pub use language::Language;
pub use location::Location;

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Symbol {
    pub name: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub name_path: Option<String>,
    pub kind: SymbolKind,
    pub location: Location,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub container: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub body: Option<String>,
    /// What the declaration is written with ahead of its body, read where it
    /// is written rather than out of its body alone.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub signature: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub children: Vec<Symbol>,
    /// The kind that tells this symbol apart from its same-named siblings,
    /// set only when it does. See [`Symbol::compute_paths_for_all`].
    #[serde(skip_serializing_if = "Option::is_none")]
    pub discriminator: Option<SymbolKind>,
}

impl Symbol {
    /// Fill each symbol's `body` from the file it was read out of, spanning
    /// its declaration: the lines an edit of the whole declaration replaces
    /// or deletes, decorators, attributes and doc comments included, so a
    /// body read, changed and passed back keeps them.
    ///
    /// A body is text, not a language-server answer: every producer of a
    /// `Symbol` fills it the same way, from the same two ends of the same
    /// location.
    pub fn attach_bodies(symbols: &mut [Symbol], content: &str) {
        Self::attach_bodies_from(symbols, &content.lines().collect::<Vec<_>>());
    }

    fn attach_bodies_from(symbols: &mut [Symbol], lines: &[&str]) {
        for symbol in symbols {
            let start = symbol.location.effective_start().0.saturating_sub(1) as usize;
            let end = symbol
                .location
                .end_line
                .unwrap_or(symbol.location.line)
                .saturating_sub(1) as usize;
            let from_start = lines.get(start..).unwrap_or_default();
            let body = &from_start[..from_start.len().min(end.saturating_sub(start) + 1)];
            if !body.is_empty() {
                symbol.body = Some(body.join("\n"));
            }
            Self::attach_bodies_from(&mut symbol.children, lines);
        }
    }

    /// The body from the line that names the symbol, where its signature and
    /// visibility are written, past the lines its declaration opens with.
    pub fn body_from_name(&self) -> Option<&str> {
        let body = self.body.as_deref()?;
        match self
            .location
            .line
            .checked_sub(self.location.effective_start().0)?
        {
            0 => Some(body),
            above => body
                .match_indices('\n')
                .nth(above as usize - 1)
                .map(|(newline, _)| &body[newline + 1..]),
        }
    }

    pub fn new(name: String, kind: SymbolKind, location: Location) -> Self {
        Self {
            name,
            name_path: None,
            kind,
            location,
            container: None,
            body: None,
            signature: None,
            children: Vec::new(),
            discriminator: None,
        }
    }

    pub fn with_container(mut self, container: impl Into<String>) -> Self {
        self.container = Some(container.into());
        self
    }

    pub fn with_body(mut self, body: impl Into<String>) -> Self {
        self.body = Some(body.into());
        self
    }

    pub fn with_children(mut self, children: Vec<Symbol>) -> Self {
        self.children = children;
        self
    }

    pub fn compute_paths(&mut self, parent_path: Option<&str>) {
        // A nameless container (e.g. an impl whose self type has no nominal
        // name) is transparent: it contributes no path segment, so its children
        // attach to the enclosing path and never inherit a stray leading `/`.
        if self.name.is_empty() {
            self.name_path = parent_path.map(str::to_string);
            for child in &mut self.children {
                child.compute_paths(parent_path);
            }
            return;
        }

        let base_path = match parent_path {
            Some(parent) => format!("{}/{}", parent, self.name),
            None => self.name.clone(),
        };

        self.name_path = Some(match self.discriminator {
            Some(kind) => format!("{base_path}[{kind}]"),
            None => base_path.clone(),
        });

        // A member is keyed by its IMMEDIATE container only — `Type/method`,
        // not `Outer/Inner/method` — because that is all the LSP workspace
        // surface can report (rust-analyzer/clangd give a method's container as
        // its nearest enclosing type; the surrounding namespaces and outer
        // types are flattened away and cannot be recovered from the container
        // string). Passing this node's own NAME (not its accumulated path) down
        // makes every producer agree, so a copied path round-trips. A
        // namespace/module/package qualifies nothing: it passes its parent path
        // straight through (a free item under it stays bare). The name carries
        // no qualifier, so same-named sibling parents (`Foo[struct]`,
        // `Foo[object]`) still share a child path (`Foo/bar`).
        let child_parent = if self.kind.is_namespace_like() {
            parent_path
        } else {
            Some(self.name.as_str())
        };
        for child in &mut self.children {
            child.compute_paths(child_parent);
        }
    }

    pub fn compute_paths_for_all(symbols: &mut [Symbol]) {
        Self::assign_discriminators(symbols);
        for symbol in symbols {
            symbol.compute_paths(None);
        }
    }

    /// Give each `workspace/symbol` result the path it is addressed by,
    /// [`Self::workspace_name_path`], so ranking, deduplication, and path
    /// patterns read the same key from a live answer as from a document tree.
    pub fn assign_workspace_paths(symbols: &mut [Symbol]) {
        for symbol in symbols {
            if let Some(path) = symbol.workspace_name_path() {
                symbol.name_path = Some(path);
            }
        }
    }

    /// Give each symbol the qualifier that distinguishes it from its
    /// same-named siblings — its kind, and only where its kind is unique
    /// among them.
    ///
    /// A path is the addressing key an edit re-resolves against a live file,
    /// so it has to survive edits elsewhere in that file. A kind does: a
    /// `struct Cart` stays `Cart[struct]` however many `impl Cart` blocks
    /// appear beside it. A position among siblings does not — inserting one
    /// overload above another silently moves every path below it, which on a
    /// mutating surface rewrites the wrong symbol.
    ///
    /// Where kind cannot tell siblings apart — true overloads — nothing is
    /// invented. Those symbols share a bare path, and every surface that must
    /// act on exactly one of them already refuses an ambiguous path and names
    /// the candidates with their positions.
    fn assign_discriminators(symbols: &mut [Symbol]) {
        use std::collections::HashMap;

        let mut kinds_by_name: HashMap<String, Vec<SymbolKind>> = HashMap::new();
        for symbol in symbols.iter() {
            kinds_by_name
                .entry(symbol.name.clone())
                .or_default()
                .push(symbol.kind);
        }

        for symbol in symbols.iter_mut() {
            let kinds = kinds_by_name
                .get(&symbol.name)
                .map(Vec::as_slice)
                .unwrap_or_default();
            let identifies = !symbol.name.is_empty()
                && kinds.len() > 1
                && kinds.iter().filter(|k| **k == symbol.kind).count() == 1;
            symbol.discriminator = identifies.then_some(symbol.kind);

            if !symbol.children.is_empty() {
                Self::assign_discriminators(&mut symbol.children);
            }
        }
    }

    pub fn path(&self) -> &str {
        self.name_path.as_deref().unwrap_or(&self.name)
    }

    /// The path without the kind that tells this symbol from same-named
    /// siblings: the key an index or workspace row carries for it.
    pub fn unqualified_path(&self) -> &str {
        Self::unqualified(self.path(), &self.name, self.kind.as_str())
    }

    /// A declaration's path without the qualifier its last segment may carry.
    ///
    /// A qualifier is the declaration's own kind written after its name, so
    /// the name and kind say whether a path ends in one. The text alone
    /// cannot: a bracket also opens a TypeScript computed name (`C/[key]`),
    /// and a name can end in `/` (C++'s `Derived/operator/`).
    pub fn unqualified<'a>(path: &'a str, name: &str, kind: &str) -> &'a str {
        path.strip_suffix(']')
            .and_then(|rest| rest.strip_suffix(kind))
            .and_then(|rest| rest.strip_suffix('['))
            .filter(|base| base.ends_with(name))
            .unwrap_or(path)
    }

    /// Whether a `--symbol` pattern names this declaration. One without a
    /// name carries the path of what holds it, so a path cannot tell it from
    /// its holder: it is addressed by its position instead.
    pub fn matches_path(&self, pattern: &str) -> bool {
        !self.name.is_empty()
            && PathPattern::parse(pattern).matches(self.unqualified_path(), self.kind)
    }

    fn matches_wildcard(path: &str, pattern: &str, exact: bool) -> bool {
        let parts: Vec<&str> = pattern.split('/').collect();
        let path_parts: Vec<&str> = path.split('/').collect();

        if exact && parts.len() != path_parts.len() {
            return false;
        }

        if parts.len() > path_parts.len() {
            return false;
        }

        let offset = path_parts.len() - parts.len();
        for (i, part) in parts.iter().enumerate() {
            let path_part = match path_parts.get(offset + i) {
                Some(p) => *p,
                None => return false,
            };

            if !Self::matches_glob_part(path_part, part) {
                return false;
            }
        }
        true
    }

    /// Glob-match one path segment. `*` matches zero or more characters and
    /// may appear any number of times: the segments between stars must occur
    /// in order, with the first and last anchored to the ends.
    fn matches_glob_part(value: &str, pattern: &str) -> bool {
        if !pattern.contains('*') {
            return value == pattern;
        }

        let parts: Vec<&str> = pattern.split('*').collect();
        let first = parts[0];
        let last = parts[parts.len() - 1];

        // The two end anchors must fit without overlapping.
        if !value.starts_with(first)
            || !value.ends_with(last)
            || value.len() < first.len() + last.len()
        {
            return false;
        }

        // Middle literals must appear in order inside the gap between anchors.
        let mut cursor = first.len();
        let end = value.len() - last.len();
        for mid in &parts[1..parts.len() - 1] {
            if mid.is_empty() {
                continue;
            }
            match value[cursor..end].find(mid) {
                Some(pos) => cursor += pos + mid.len(),
                None => return false,
            }
        }
        true
    }

    /// References to every symbol whose path matches `pattern`, in tree order.
    /// Borrows rather than clones so a selecting caller (e.g. a destructive
    /// edit) clones only the one it keeps.
    pub fn filter_by_path<'a>(symbols: &'a [Symbol], pattern: &str) -> Vec<&'a Symbol> {
        let mut out = Vec::new();
        Self::collect_by(symbols, &|s| s.matches_path(pattern), &mut out);
        out
    }

    /// The single depth-first traversal behind every symbol filter: visit
    /// every node (always recursing into children) and collect the ones the
    /// predicate selects.
    fn collect_by<'a>(
        symbols: &'a [Symbol],
        predicate: &impl Fn(&Symbol) -> bool,
        out: &mut Vec<&'a Symbol>,
    ) {
        for symbol in symbols {
            if predicate(symbol) {
                out.push(symbol);
            }
            Self::collect_by(&symbol.children, predicate, out);
        }
    }

    pub fn matches_substring(&self, substring: &str) -> bool {
        self.name.to_lowercase().contains(&substring.to_lowercase())
    }

    /// Normalize a raw LSP documentSymbol name into the segment used for
    /// symbol paths, the name the index extractor reads for the same
    /// declaration (`declared_name`); an `impl` block
    /// collapses to its bare implementing type, so a method reads
    /// `Type/method` on every surface — `impl Foo`, `impl Trait for Foo`, and
    /// `impl<T> Foo<T>` all yield `Foo`, matching the index extractor (which
    /// keys on the impl's self type, not the trait).
    pub fn normalize_symbol_name(name: &str) -> String {
        let name = name.trim();
        if let Some(rest) = name.strip_prefix("impl")
            && rest.starts_with(|c: char| c.is_whitespace() || c == '<')
        {
            return Self::self_type_segment(rest);
        }
        Self::declared_name(name)
    }

    /// Reduce a self-type expression to its path segment — the one rule every
    /// name_path producer (the index extractor, the documentSymbol converter,
    /// and the workspace-symbol path) applies to an impl's self type so they
    /// agree. From an impl header it is the type after ` for ` (trait impl)
    /// else the head type; a structural type (tuple/array/pointer/fn-ptr/
    /// qualified) collapses to its first nominal identifier, and a nominal one
    /// drops the where-clause, the impl and type generics, a leading
    /// `&`/`&mut`/`dyn`/lifetime, and any module path.
    pub(crate) fn self_type_segment(self_type: &str) -> String {
        let head = self_type.rsplit(" for ").next().unwrap_or(self_type);
        let head = head.split(" where ").next().unwrap_or(head).trim();

        // Structural self types — tuple `(A, B)`, array/slice `[T; N]`,
        // pointer `*const T`, fn-pointer `fn(..)`, qualified `<T as Tr>::X` —
        // have no outer nominal name. Take the first nominal type identifier
        // within, or an empty string for a truly nameless one like `fn()` (the
        // path builder then treats the impl as a transparent container).
        // Checked before stripping leading generics so a qualified type's
        // `<…>` isn't mistaken for impl params.
        if Self::is_structural_self_type(head) {
            return Self::first_nominal_ident(head);
        }

        // Plain nominal or module path. A leading `<…>` here is the generic
        // params of an inherent `impl<T> Foo<T>` (the trait side, if any, was
        // already split off at ` for `); drop them, then peel a leading
        // reference/mut/dyn/lifetime.
        let head = Self::strip_leading_generics(head);
        let mut ty = head;
        loop {
            let start = ty;
            ty = ty.trim_start_matches('&').trim_start();
            ty = ty.strip_prefix("mut ").unwrap_or(ty).trim_start();
            ty = ty.strip_prefix("dyn ").unwrap_or(ty).trim_start();
            if let Some(rest) = ty.strip_prefix('\'') {
                ty = rest
                    .split_once(char::is_whitespace)
                    .map(|(_, t)| t.trim_start())
                    .unwrap_or("");
            }
            if ty == start {
                break;
            }
        }
        // Take the last path segment (the leaf type), THEN drop its own
        // generics — in that order, so generics on an OUTER segment
        // (`Outer<T>::Inner`) don't truncate the path before the leaf is taken.
        let ty = ty.rsplit("::").next().unwrap_or(ty).trim();
        ty.split('<').next().unwrap_or(ty).trim().to_string()
    }

    /// A structural self type — tuple, array/slice, raw pointer, fn-pointer, or
    /// qualified `<T as Tr>::X` — carries no outer nominal name.
    fn is_structural_self_type(head: &str) -> bool {
        head.starts_with(['(', '[', '*'])
            || head.starts_with("fn(")
            || (head.starts_with('<') && head.contains(" as "))
    }

    /// The path an unresolved workspace symbol is addressed by: its container
    /// reduced via [`Self::self_type_segment`] and then to its IMMEDIATE
    /// segment — so an impl method matches the index/documentSymbol name_path
    /// (`Type/method`, never `Outer/Inner/method`) and a copied path
    /// round-trips to `symbols`/`edit`. Language container separators (`::`,
    /// `.`, `#`, `\`) normalize to `/`; only the last segment is kept, because
    /// that is the nearest enclosing type — the outer types/namespaces the LSP
    /// folds into the container string are not part of the addressing path on
    /// any surface (a Rust self type is already a single segment, so this is a
    /// no-op there).
    pub(crate) fn workspace_name_path(&self) -> Option<String> {
        let name = self.name.trim();
        if name.is_empty() {
            return None;
        }
        let container = Self::self_type_segment(self.container.as_deref().unwrap_or_default())
            .replace("::", "/")
            .replace(['.', '#', '\\'], "/");
        let container = container.rsplit('/').next().unwrap_or(&container).trim();
        Some(if container.is_empty() {
            name.to_string()
        } else {
            format!("{container}/{name}")
        })
    }

    /// The first nominal type identifier in a structural type string, skipping
    /// punctuation and type-position keywords. Empty when there is none (e.g.
    /// `fn()`), which the path builder treats as a transparent container.
    fn first_nominal_ident(s: &str) -> String {
        let mut rest = s;
        while let Some(start) = rest.find(|c: char| c.is_alphabetic() || c == '_') {
            let after = &rest[start..];
            let end = after
                .find(|c: char| !(c.is_alphanumeric() || c == '_'))
                .unwrap_or(after.len());
            let word = &after[..end];
            if !matches!(
                word,
                "const" | "mut" | "dyn" | "fn" | "as" | "where" | "impl" | "for"
            ) {
                return word.to_string();
            }
            rest = &after[end..];
        }
        String::new()
    }

    /// Drop a single balanced leading `<...>` (the generic params of an
    /// `impl<...>` header), returning the remainder trimmed.
    fn strip_leading_generics(s: &str) -> &str {
        if !s.starts_with('<') {
            return s;
        }
        let mut depth = 0usize;
        for (i, c) in s.char_indices() {
            match c {
                '<' => depth += 1,
                '>' => {
                    depth -= 1;
                    if depth == 0 {
                        return s[i + 1..].trim_start();
                    }
                }
                _ => {}
            }
        }
        s
    }

    /// The name a server's label gives a declaration. A parameter list or
    /// type parameters after it are dropped (`foo(int)`, `Foo<T>`), a
    /// qualifier's type arguments kept (`Box<T>::get`), and so is
    /// a Go method's receiver before it (gopls: `(*Server).Start`). An
    /// operator keeps its symbols: a label that begins with them names the
    /// operator (sourcekit-lsp: `<(lhs:rhs:)` is `<`), and so does the
    /// `operator` keyword followed by one, qualified or not (clangd:
    /// `operator<`, `Money::operator()`) — as would a method of another
    /// language named `operator` whose server writes an empty parameter list
    /// after it. A label that is an
    /// anonymous marker as a whole (tsserver: `<function>`) names nothing.
    fn declared_name(label: &str) -> String {
        let label = label.trim();
        let is_marker = label
            .strip_prefix('<')
            .and_then(|rest| rest.strip_suffix('>'))
            .is_some_and(|word| !word.is_empty() && word.chars().all(|c| c.is_ascii_alphabetic()));
        if is_marker {
            return String::new();
        }
        let label = label
            .strip_prefix('(')
            .and_then(|rest| rest.split_once(")."))
            .map_or(label, |(_, name)| name);
        let is_identifier = |c: char| c.is_alphanumeric() || c == '_' || c == '$';
        let (qualifier, unqualified) = match label.rfind("::operator") {
            Some(at) => label.split_at(at + "::".len()),
            None => ("", label),
        };
        if let Some(symbol) = unqualified.strip_prefix("operator")
            && (symbol.starts_with("()")
                || symbol.starts_with(|c: char| !is_identifier(c) && c != '('))
        {
            let end = match symbol.strip_prefix("()") {
                Some(_) => 2,
                None => symbol.find('(').unwrap_or(symbol.len()),
            };
            return format!("{qualifier}operator{}", symbol[..end].trim_end());
        }
        let (operator, rest) = label.split_at(
            label
                .find(|c: char| c == '(' || is_identifier(c))
                .unwrap_or(label.len()),
        );
        let rest = &rest[..Self::name_end(rest)];
        format!("{operator}{rest}").trim().to_string()
    }

    /// Where the name a label begins with ends: at a parameter list, or at
    /// type parameters after it. Type arguments a qualifier carries are part
    /// of the name (clangd: `Box<T>::get`).
    fn name_end(label: &str) -> usize {
        let mut from = 0;
        while let Some(found) = label[from..].find(['(', '<']) {
            let open = from + found;
            if label[open..].starts_with('(') {
                return open;
            }
            let mut depth = 0usize;
            let close = label[open..].char_indices().find_map(|(at, c)| {
                match c {
                    '<' => depth += 1,
                    '>' => {
                        depth -= 1;
                        if depth == 0 {
                            return Some(open + at);
                        }
                    }
                    _ => {}
                }
                None
            });
            match close {
                Some(close) if label[close + 1..].starts_with("::") => from = close + 1,
                _ => return open,
            }
        }
        label.len()
    }
}

/// A `--symbol` pattern, read for the declarations it names.
///
/// A trailing `[kind]` qualifies the name before it. Only a real kind counts:
/// brackets are ordinary name syntax in several languages — a Scala or Kotlin
/// `Foo[A]` is one name — and reading those as qualifiers would make a pattern
/// match nothing while a bare `Foo` started matching them. A qualifier follows
/// a name, so a pattern that opens with a bracket is one, whatever kind its
/// text spells.
///
/// A qualifier selects the declarations of the kind it names. A listing prints
/// one where it tells same-named siblings apart (`Cart[struct]` beside
/// `Cart[object]`), and it keeps naming that declaration whatever is added or
/// removed beside it; a declaration printed without one, such as an index row,
/// answers it by its kind.
///
/// Where the name before the bracket ends in `/`, the text cannot say whether
/// the bracket qualifies a name that ends in `/` (C++'s `operator/`) or opens a
/// computed member named by its brackets (TypeScript's `[key]`), so the
/// pattern names what either reading names.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PathPattern<'a> {
    /// The path the pattern names, without its qualifier. Every reading of the
    /// pattern begins with it, so text that finds it finds them all.
    pub path: &'a str,
    /// The kind the qualifier requires.
    pub kind: Option<SymbolKind>,
    /// The pattern as written, where it can also name a computed member.
    pub literal: Option<&'a str>,
}

impl<'a> PathPattern<'a> {
    pub fn parse(pattern: &'a str) -> Self {
        if let Some(bracket) = pattern.rfind('[')
            && let Some(kind) = pattern[bracket..]
                .strip_prefix('[')
                .and_then(|qualifier| qualifier.strip_suffix(']'))
                .and_then(|qualifier| qualifier.parse::<SymbolKind>().ok())
        {
            let path = &pattern[..bracket];
            if !path.strip_prefix('/').unwrap_or(path).is_empty() {
                return Self {
                    path,
                    kind: Some(kind),
                    literal: path.ends_with('/').then_some(pattern),
                };
            }
        }
        Self {
            path: pattern,
            kind: None,
            literal: None,
        }
    }

    /// Whether the pattern names a declaration, given its unqualified path and
    /// its kind. An unqualified pattern names every kind, so an agent that
    /// knows only the name still reaches every candidate.
    pub fn matches(&self, path: &str, kind: SymbolKind) -> bool {
        (self.kind.is_none_or(|qualifier| qualifier == kind) && Self::names(self.path, path))
            || self
                .literal
                .is_some_and(|literal| Self::names(literal, path))
    }

    /// Whether a pattern's path names an unqualified path: a leading `/`
    /// forces a full-path exact match, otherwise bare last-component,
    /// `/`-anchored suffix, or `*` wildcard matching. Every surface reads a
    /// pattern through this, so each resolves it the same way.
    pub fn names(pattern: &str, path: &str) -> bool {
        let (pattern, exact) = match pattern.strip_prefix('/') {
            Some(absolute) => (absolute, true),
            None => (pattern, false),
        };
        if pattern.contains('*') {
            Symbol::matches_wildcard(path, pattern, exact)
        } else if exact {
            path == pattern
        } else if pattern.contains('/') {
            path == pattern || path.ends_with(&format!("/{pattern}"))
        } else {
            path.rsplit('/').next().unwrap_or(path) == pattern
        }
    }

    /// The last segment of a path — what its declaration is called — after
    /// the path of what encloses it, if anything does. A segment is never
    /// empty, so a trailing `/` ends a name (`operator/`) rather than opening
    /// another segment. A `/` inside a name (`operator/=`) reads as a
    /// separator: the path alone cannot say where such a name begins.
    pub fn split_name(path: &str) -> (Option<&str>, &str) {
        let path = path.strip_prefix('/').unwrap_or(path);
        let named = path.strip_suffix('/').unwrap_or(path);
        match named.rfind('/') {
            Some(slash) => (Some(&path[..slash]), &path[slash + 1..]),
            None => (None, path),
        }
    }

    /// Whether a name is a computed member's: an expression in brackets, such
    /// as TypeScript's `[key]`. A name that only begins with a bracket, as
    /// Ruby's `[]` and `[]=` do, is spelled like any other.
    pub fn is_computed(name: &str) -> bool {
        name.len() > 2 && name.starts_with('[') && name.ends_with(']')
    }

    /// A name without the type parameters a bracket opens after it, as in
    /// Scala's `Foo[A]`: the name a server lists the declaration under.
    pub fn bare_name(name: &str) -> &str {
        match name.find('[') {
            Some(bracket) if bracket > 0 => &name[..bracket],
            _ => name,
        }
    }
}

/// Which declarations a symbol listing keeps.
#[derive(Debug, Default, Clone, Copy)]
pub struct SymbolFilter<'a> {
    /// A path pattern, or with `substring` a fragment of the name.
    pub pattern: Option<&'a str>,
    pub substring: bool,
    pub include_kinds: Option<&'a [SymbolKind]>,
    pub exclude_kinds: Option<&'a [SymbolKind]>,
    /// Leave out variables, constants and other value-level declarations.
    pub structural: bool,
}

impl SymbolFilter<'_> {
    /// Every declaration the filter keeps, at any depth, in tree order.
    pub fn matches(&self, symbols: &[Symbol]) -> Vec<Symbol> {
        let mut out = Vec::new();
        Symbol::collect_by(
            symbols,
            &|symbol| !self.excludes(symbol) && self.selects(symbol),
            &mut out,
        );
        out.into_iter().cloned().collect()
    }

    /// The declarations a nested listing answers with: each kept one at its
    /// outermost position, holding what is beneath it, so no declaration is
    /// listed twice. An excluded declaration is left out and what it holds
    /// takes its place. Without a selecting filter that is the whole tree.
    pub fn outermost(&self, symbols: Vec<Symbol>) -> Vec<Symbol> {
        let mut out = Vec::new();
        for mut symbol in symbols {
            let children = std::mem::take(&mut symbol.children);
            if !self.excludes(&symbol) && self.selects(&symbol) {
                symbol.children = self.pruned(children);
                out.push(symbol);
            } else {
                out.extend(self.outermost(children));
            }
        }
        out
    }

    fn pruned(&self, symbols: Vec<Symbol>) -> Vec<Symbol> {
        let mut out = Vec::new();
        for mut symbol in symbols {
            let children = self.pruned(std::mem::take(&mut symbol.children));
            if self.excludes(&symbol) {
                out.extend(children);
            } else {
                symbol.children = children;
                out.push(symbol);
            }
        }
        out
    }

    fn excludes(&self, symbol: &Symbol) -> bool {
        self.exclude_kinds
            .is_some_and(|kinds| kinds.contains(&symbol.kind))
            || (self.structural && symbol.kind.is_low_level())
    }

    fn selects(&self, symbol: &Symbol) -> bool {
        self.include_kinds
            .is_none_or(|kinds| kinds.contains(&symbol.kind))
            && match self.pattern {
                None => true,
                Some(fragment) if self.substring => symbol.matches_substring(fragment),
                Some(pattern) => symbol.matches_path(pattern),
            }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn build_symbol(name: &str, kind: SymbolKind) -> Symbol {
        Symbol::new(
            name.to_string(),
            kind,
            Location::point(PathBuf::from("test.rs"), 1, 1),
        )
    }

    #[test]
    fn a_body_spans_the_declaration_and_reads_from_its_name() {
        let content = "import functools\n\n\n@functools.cache\n@other\ndef top():\n    return 2\n\n\n\
                       def plain():\n    return 1\n";
        let at = |name: &str, line, start, end| {
            Symbol::new(
                name.to_string(),
                SymbolKind::Function,
                Location::full(PathBuf::from("m.py"), line, 5, start, 1, end, 13),
            )
        };
        let mut symbols = vec![at("top", 6, 4, 7), at("plain", 10, 10, 11)];
        Symbol::attach_bodies(&mut symbols, content);

        assert_eq!(
            symbols[0].body.as_deref(),
            Some("@functools.cache\n@other\ndef top():\n    return 2")
        );
        assert_eq!(
            symbols[0].body_from_name(),
            Some("def top():\n    return 2")
        );
        assert_eq!(
            symbols[1].body_from_name(),
            Some("def plain():\n    return 1")
        );
    }

    #[test]
    fn a_member_takes_its_body_from_the_same_lines_as_its_type() {
        let content = "class Shop:\r\n    def open(self):\r\n        return 1\r\n";
        let at = |name: &str, kind, line, end| {
            Symbol::new(
                name.to_string(),
                kind,
                Location::full(PathBuf::from("m.py"), line, 5, line, 1, end, 17),
            )
        };
        let mut class = at("Shop", SymbolKind::Class, 1, 3);
        class.children = vec![at("open", SymbolKind::Method, 2, 3)];
        let mut symbols = vec![class];
        Symbol::attach_bodies(&mut symbols, content);

        assert_eq!(
            symbols[0].children[0].body.as_deref(),
            Some("    def open(self):\n        return 1")
        );
    }

    #[test]
    fn compute_paths_chains_parent_to_children() {
        let mut class = build_symbol("MyClass", SymbolKind::Class);
        class.children = vec![
            build_symbol("update", SymbolKind::Method),
            build_symbol("reset", SymbolKind::Method),
        ];

        class.compute_paths(None);

        assert_eq!(class.name_path, Some("MyClass".to_string()));
        assert_eq!(
            class.children[0].name_path,
            Some("MyClass/update".to_string())
        );
        assert_eq!(
            class.children[1].name_path,
            Some("MyClass/reset".to_string())
        );
    }

    #[test]
    fn compute_paths_treats_modules_as_path_transparent() {
        // mod a { mod b { struct Deep { fn m }  fn free } }
        let mut a = build_symbol("a", SymbolKind::Module);
        let mut b = build_symbol("b", SymbolKind::Module);
        let mut deep = build_symbol("Deep", SymbolKind::Struct);
        deep.children = vec![build_symbol("m", SymbolKind::Method)];
        b.children = vec![deep, build_symbol("free", SymbolKind::Function)];
        a.children = vec![b];

        a.compute_paths(None);

        // Modules are self-named but never qualify descendants — a method is
        // `Type/method`, a module-level item bare — matching the workspace
        // producer so a copied path round-trips.
        assert_eq!(a.name_path, Some("a".to_string()));
        let b = &a.children[0];
        assert_eq!(b.name_path, Some("b".to_string()));
        let deep = &b.children[0];
        assert_eq!(deep.name_path, Some("Deep".to_string()));
        assert_eq!(deep.children[0].name_path, Some("Deep/m".to_string()));
        assert_eq!(b.children[1].name_path, Some("free".to_string()));
    }

    #[test]
    fn compute_paths_keys_by_immediate_container_for_nested_types() {
        // namespace ns { class Outer { class Inner { void method } void om } }
        let mut ns = build_symbol("ns", SymbolKind::Namespace);
        let mut outer = build_symbol("Outer", SymbolKind::Class);
        let mut inner = build_symbol("Inner", SymbolKind::Class);
        inner.children = vec![build_symbol("method", SymbolKind::Method)];
        outer.children = vec![inner, build_symbol("om", SymbolKind::Method)];
        ns.children = vec![outer];

        ns.compute_paths(None);

        // The namespace is self-named but transparent; an enclosing type does
        // NOT widen a member's path — every member is keyed by its IMMEDIATE
        // container, matching what the LSP workspace surface can report.
        let outer = &ns.children[0];
        assert_eq!(outer.name_path, Some("Outer".to_string()));
        let inner = &outer.children[0];
        assert_eq!(inner.name_path, Some("Outer/Inner".to_string()));
        // method is `Inner/method`, NOT `Outer/Inner/method`.
        assert_eq!(
            inner.children[0].name_path,
            Some("Inner/method".to_string())
        );
        assert_eq!(outer.children[1].name_path, Some("Outer/om".to_string()));
    }

    #[test]
    fn matches_path_exact_relative_and_suffix() {
        let mut sym = build_symbol("update", SymbolKind::Method);
        sym.name_path = Some("MyClass/update".to_string());

        assert!(sym.matches_path("update"));
        assert!(sym.matches_path("MyClass/update"));
        assert!(!sym.matches_path("OtherClass/update"));
    }

    #[test]
    fn matches_path_supports_wildcards() {
        let mut sym = build_symbol("update", SymbolKind::Method);
        sym.name_path = Some("MyClass/update".to_string());

        assert!(sym.matches_path("*/update"));
        assert!(sym.matches_path("MyClass/*"));
        assert!(!sym.matches_path("*/reset"));
    }

    /// A segment may carry any number of `*`: the between-star literals must
    /// occur in order, anchored at the ends. A single `split_once` matcher
    /// silently fails these, so they are explicit.
    #[test]
    fn matches_path_handles_multiple_stars_in_a_segment() {
        let mut sym = build_symbol("getUserName", SymbolKind::Method);
        sym.name_path = Some("Service/getUserName".to_string());

        // contains-style and interior multi-star
        assert!(sym.matches_path("*User*"));
        assert!(sym.matches_path("get*User*Name"));
        assert!(sym.matches_path("*/get*Name"));
        // explicit a*b*c shape: two stars, three ordered anchored literals
        assert!(sym.matches_path("g*User*e"));
        assert!(!sym.matches_path("g*Zzz*e"));
        // order matters and end anchors hold
        assert!(!sym.matches_path("*Name*User*"));
        assert!(!sym.matches_path("get*Xyz*Name"));
        // a star matches the empty string
        assert!(sym.matches_path("getUserName*"));

        // leading-`/` exact mode still enforces full-path segment count
        assert!(sym.matches_path("/Service/get*Name"));
        assert!(!sym.matches_path("/get*Name"));
    }

    /// A server's label names the declaration the index reads under the same
    /// name: never an empty one for a Go method or an operator.
    #[test]
    fn normalize_symbol_name_reads_the_name_a_label_gives() {
        for (label, name) in [
            ("process(int, String)", "process"),
            ("Foo<T>", "Foo"),
            ("(*Server).Start", "Start"),
            ("(Server).Name", "Name"),
            ("(*List[T]).Push", "Push"),
            ("<(lhs:rhs:)", "<"),
            ("<<(lhs:rhs:)", "<<"),
            ("==(lhs:rhs:)", "=="),
            ("init(cents:)", "init"),
            ("operator<", "operator<"),
            ("operator()", "operator()"),
            ("operator new", "operator new"),
            ("operator +(Money, Money)", "operator +"),
            ("operator()(int)", "operator()"),
            ("Money::operator<", "Money::operator<"),
            ("ns::Money::operator()", "ns::Money::operator()"),
            ("Money::value", "Money::value"),
            ("Box<T>::get", "Box<T>::get"),
            ("Stack<T>::push", "Stack<T>::push"),
            (
                "ns::Map<K, std::pair<K, V>>::insert(K)",
                "ns::Map<K, std::pair<K, V>>::insert",
            ),
            ("operator(int)", "operator"),
            ("<function>", ""),
            ("<unknown>", ""),
        ] {
            assert_eq!(Symbol::normalize_symbol_name(label), name, "{label}");
        }
    }

    /// An LSP `impl` block collapses to its bare implementing type so a
    /// method's path reads `Type/method` on every surface (matching the index
    /// extractor) — inherent, trait, generic, and module-qualified alike.
    #[test]
    fn normalize_symbol_name_collapses_impl_to_the_self_type() {
        assert_eq!(Symbol::normalize_symbol_name("impl Symbol"), "Symbol");
        assert_eq!(
            Symbol::normalize_symbol_name("impl fmt::Display for Language"),
            "Language"
        );
        assert_eq!(Symbol::normalize_symbol_name("impl<T> Foo<T>"), "Foo");
        assert_eq!(
            Symbol::normalize_symbol_name("impl<T> Bar<T> for crate::Foo<T>"),
            "Foo"
        );
        // an unscoped trait still resolves to the self type, never the trait
        assert_eq!(
            Symbol::normalize_symbol_name("impl FromStr for Language"),
            "Language"
        );
        // where-clause, reference, dyn, and lifetime targets reduce to the type
        assert_eq!(
            Symbol::normalize_symbol_name("impl Foo for Bar where Bar: Send"),
            "Bar"
        );
        assert_eq!(Symbol::normalize_symbol_name("impl Trait for &Foo"), "Foo");
        assert_eq!(
            Symbol::normalize_symbol_name("impl Trait for &mut Foo"),
            "Foo"
        );
        assert_eq!(
            Symbol::normalize_symbol_name("impl Display for dyn Error"),
            "Error"
        );
        assert_eq!(
            Symbol::normalize_symbol_name("impl<'a> Trait for &'a Foo"),
            "Foo"
        );
        // structural self types reduce to their first nominal type identifier,
        // matching the index extractor (so search↔symbols↔edit agree)
        assert_eq!(
            Symbol::normalize_symbol_name("impl Trait for (Foo, Bar)"),
            "Foo"
        );
        assert_eq!(
            Symbol::normalize_symbol_name("impl Trait for [Foo; 4]"),
            "Foo"
        );
        assert_eq!(
            Symbol::normalize_symbol_name("impl Trait for *const Foo"),
            "Foo"
        );
        assert_eq!(
            Symbol::normalize_symbol_name("impl Bar for <Foo as Baz>::Out"),
            "Foo"
        );
        // generics on an OUTER path segment must not truncate before the leaf:
        // the nearest type is taken first, then its own generics are dropped
        assert_eq!(
            Symbol::normalize_symbol_name("impl Tr for ns::Outer<T>::Inner"),
            "Inner"
        );
        assert_eq!(
            Symbol::normalize_symbol_name("impl Tr for Outer<T>::Inner<U>"),
            "Inner"
        );
        // a self type with no nominal name at all yields the empty (transparent) segment
        assert_eq!(Symbol::normalize_symbol_name("impl Trait for fn()"), "");
        // a nominal type whose name merely starts with "fn" is NOT a fn-pointer
        assert_eq!(
            Symbol::normalize_symbol_name("impl Trait for fn_mod::Named"),
            "Named"
        );
        assert_eq!(
            Symbol::normalize_symbol_name("impl Trait for Fnable"),
            "Fnable"
        );
        // non-impl names keep the plain parameter/generic stripping
        assert_eq!(Symbol::normalize_symbol_name("execute(args)"), "execute");
        assert_eq!(Symbol::normalize_symbol_name("Vec<T>"), "Vec");
        // a type that merely starts with the letters "impl" is not an impl
        assert_eq!(Symbol::normalize_symbol_name("implicit"), "implicit");
    }

    /// A qualifier that matches no real symbol degrades to a literal name
    /// (no match), never a parse panic or a wrong match.
    #[test]
    fn matches_path_treats_an_unmatched_qualifier_as_literal() {
        let mut sym = build_symbol("bar", SymbolKind::Method);
        sym.name_path = Some("Foo/bar".to_string());

        assert!(!sym.matches_path("bar[class]"));
        assert!(!sym.matches_path("bar["));
        assert!(!sym.matches_path("bar[]"));
    }

    #[test]
    fn filter_by_path_recurses_into_children() {
        let mut class = build_symbol("MyClass", SymbolKind::Class);
        class.children = vec![
            build_symbol("update", SymbolKind::Method),
            build_symbol("reset", SymbolKind::Method),
        ];
        class.compute_paths(None);
        let symbols = [class];

        let results = Symbol::filter_by_path(&symbols, "MyClass/update");
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].name, "update");

        let results = Symbol::filter_by_path(&symbols, "*/reset");
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].name, "reset");

        let results = Symbol::filter_by_path(&symbols, "MyClass/*");
        assert_eq!(results.len(), 2);
    }

    #[test]
    fn matches_substring_is_case_insensitive() {
        let sym = build_symbol("getValue", SymbolKind::Function);
        assert!(sym.matches_substring("get"));
        assert!(sym.matches_substring("Value"));
        assert!(sym.matches_substring("getValue"));
        assert!(sym.matches_substring("GET"));
        assert!(!sym.matches_substring("set"));
    }

    #[test]
    fn filter_advanced_with_include_or_exclude_kinds() {
        let mut class = build_symbol("MyClass", SymbolKind::Class);
        class.children = vec![
            build_symbol("update", SymbolKind::Method),
            build_symbol("count", SymbolKind::Variable),
        ];
        class.compute_paths(None);

        let results = SymbolFilter {
            include_kinds: Some(&[SymbolKind::Method]),
            ..SymbolFilter::default()
        }
        .matches(std::slice::from_ref(&class));
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].name, "update");

        let results = SymbolFilter {
            exclude_kinds: Some(&[SymbolKind::Variable]),
            ..SymbolFilter::default()
        }
        .matches(std::slice::from_ref(&class));
        assert_eq!(results.len(), 2);
        assert!(results.iter().all(|s| s.kind != SymbolKind::Variable));
    }

    #[test]
    fn filter_advanced_excludes_low_level_kinds() {
        let mut class = build_symbol("MyClass", SymbolKind::Class);
        class.children = vec![
            build_symbol("update", SymbolKind::Method),
            build_symbol("count", SymbolKind::Variable),
        ];
        class.compute_paths(None);

        let results = SymbolFilter {
            structural: true,
            ..SymbolFilter::default()
        }
        .matches(&[class]);
        assert_eq!(results.len(), 2);
        assert!(results.iter().all(|s| !s.kind.is_low_level()));
    }

    #[test]
    fn filter_advanced_substring_walks_tree() {
        let mut class = build_symbol("UserService", SymbolKind::Class);
        class.children = vec![
            build_symbol("getUser", SymbolKind::Method),
            build_symbol("setUser", SymbolKind::Method),
            build_symbol("deleteAll", SymbolKind::Method),
        ];
        class.compute_paths(None);

        let results = SymbolFilter {
            pattern: Some("User"),
            substring: true,
            ..SymbolFilter::default()
        }
        .matches(&[class]);
        assert_eq!(results.len(), 3);
    }

    /// A nested listing names each declaration once: a kept declaration holds
    /// what is beneath it, a filter that keeps none of the outer ones reaches
    /// the inner ones, and an excluded one gives its place to what it holds.
    #[test]
    fn a_nested_listing_keeps_each_declaration_once_at_its_outermost_place() {
        let mut class = build_symbol("Service", SymbolKind::Class);
        class.children = vec![
            build_symbol("start", SymbolKind::Method),
            build_symbol("count", SymbolKind::Variable),
        ];
        class.compute_paths(None);
        let tree = vec![class, build_symbol("main", SymbolKind::Function)];
        let shape = |symbols: Vec<Symbol>| -> Vec<(String, Vec<String>)> {
            symbols
                .into_iter()
                .map(|s| (s.name, s.children.into_iter().map(|c| c.name).collect()))
                .collect()
        };
        let named = |pairs: &[(&str, &[&str])]| -> Vec<(String, Vec<String>)> {
            pairs
                .iter()
                .map(|(name, children)| {
                    (
                        name.to_string(),
                        children.iter().map(|c| c.to_string()).collect(),
                    )
                })
                .collect()
        };

        assert_eq!(
            shape(SymbolFilter::default().outermost(tree.clone())),
            named(&[("Service", &["start", "count"]), ("main", &[])])
        );
        let methods = [SymbolKind::Method];
        assert_eq!(
            shape(
                SymbolFilter {
                    include_kinds: Some(&methods),
                    ..SymbolFilter::default()
                }
                .outermost(tree.clone())
            ),
            named(&[("start", &[])])
        );
        assert_eq!(
            shape(
                SymbolFilter {
                    structural: true,
                    ..SymbolFilter::default()
                }
                .outermost(tree.clone())
            ),
            named(&[("Service", &["start"]), ("main", &[])])
        );
        assert_eq!(
            shape(
                SymbolFilter {
                    pattern: Some("s"),
                    substring: true,
                    ..SymbolFilter::default()
                }
                .outermost(tree.clone())
            ),
            named(&[("Service", &["start", "count"])])
        );
        let classes = [SymbolKind::Class];
        assert_eq!(
            shape(
                SymbolFilter {
                    exclude_kinds: Some(&classes),
                    ..SymbolFilter::default()
                }
                .outermost(tree)
            ),
            named(&[("start", &[]), ("count", &[]), ("main", &[])])
        );
    }

    /// Kind qualifies a name only where it identifies. Overloads share a
    /// kind, so nothing is invented for them; the sibling that a kind does
    /// single out gets it.
    #[test]
    fn a_qualifier_is_assigned_only_where_kind_identifies() {
        let mut class = build_symbol("MyClass", SymbolKind::Class);
        class.children = vec![
            build_symbol("doSomething", SymbolKind::Method),
            build_symbol("doSomething", SymbolKind::Method),
            build_symbol("doSomething", SymbolKind::Field),
            build_symbol("unique", SymbolKind::Method),
        ];

        let mut symbols = vec![class];
        Symbol::compute_paths_for_all(&mut symbols);

        let class = &symbols[0];
        assert_eq!(class.children[0].discriminator, None);
        assert_eq!(class.children[1].discriminator, None);
        assert_eq!(class.children[2].discriminator, Some(SymbolKind::Field));
        assert_eq!(class.children[3].discriminator, None);

        assert_eq!(
            class.children[0].name_path,
            Some("MyClass/doSomething".to_string())
        );
        assert_eq!(
            class.children[2].name_path,
            Some("MyClass/doSomething[field]".to_string())
        );
        assert_eq!(
            class.children[3].name_path,
            Some("MyClass/unique".to_string())
        );
    }

    /// The shape a type and its implementation block make in Rust: two
    /// same-named siblings of different kinds, each addressable, and each
    /// path unmoved by anything added beside it.
    #[test]
    fn a_type_and_its_implementation_block_stay_separately_addressable() {
        let mut symbols = vec![
            build_symbol("Cart", SymbolKind::Struct),
            build_symbol("Cart", SymbolKind::Object),
        ];
        Symbol::compute_paths_for_all(&mut symbols);

        assert_eq!(symbols[0].path(), "Cart[struct]");
        assert_eq!(symbols[1].path(), "Cart[object]");
        assert_eq!(Symbol::filter_by_path(&symbols, "Cart[struct]").len(), 1);
        assert_eq!(Symbol::filter_by_path(&symbols, "Cart").len(), 2);

        let mut reordered = vec![
            build_symbol("Cart", SymbolKind::Object),
            build_symbol("Cart", SymbolKind::Struct),
        ];
        Symbol::compute_paths_for_all(&mut reordered);
        assert_eq!(reordered[1].path(), "Cart[struct]");
    }

    /// Qualified parents keep distinct paths, but their children hang off
    /// the unqualified base — so one child path can match several symbols
    /// (an ambiguity edit refuses) while the parent path stays unique.
    #[test]
    fn filter_by_path_matches_same_named_parents_children() {
        let mut first = build_symbol("Foo", SymbolKind::Class);
        first.children = vec![build_symbol("bar", SymbolKind::Method)];
        let mut second = build_symbol("Foo", SymbolKind::Interface);
        second.children = vec![build_symbol("bar", SymbolKind::Method)];

        let mut symbols = vec![first, second];
        Symbol::compute_paths_for_all(&mut symbols);

        assert_eq!(symbols[0].path(), "Foo[class]");
        assert_eq!(symbols[1].path(), "Foo[interface]");
        assert_eq!(Symbol::filter_by_path(&symbols, "Foo/bar").len(), 2);
        assert_eq!(Symbol::filter_by_path(&symbols, "Foo[class]").len(), 1);
        assert!(Symbol::filter_by_path(&symbols, "Foo/missing").is_empty());
    }

    #[test]
    fn matches_path_filters_by_qualifier() {
        let mut sym = build_symbol("doSomething", SymbolKind::Field);
        sym.discriminator = Some(SymbolKind::Field);
        sym.name_path = Some("MyClass/doSomething[field]".to_string());

        assert!(sym.matches_path("doSomething"));
        assert!(sym.matches_path("doSomething[field]"));
        assert!(!sym.matches_path("doSomething[method]"));
        assert!(sym.matches_path("MyClass/doSomething[field]"));
        assert!(!sym.matches_path("MyClass/doSomething[method]"));
    }

    /// A leading `/` pins the whole path, so a suffix that would otherwise
    /// match is refused.
    #[test]
    fn matches_path_supports_absolute_form() {
        let mut sym = build_symbol("bar", SymbolKind::Method);
        sym.name_path = Some("Foo/bar".to_string());

        assert!(sym.matches_path("/Foo/bar"));
        assert!(!sym.matches_path("/bar"));
        assert!(sym.matches_path("bar"));
    }

    #[test]
    fn a_pattern_reads_a_trailing_kind_as_a_qualifier() {
        let read = |pattern| {
            let read = PathPattern::parse(pattern);
            (read.path, read.kind, read.literal)
        };
        assert_eq!(read("method"), ("method", None, None));
        assert_eq!(
            read("method[field]"),
            ("method", Some(SymbolKind::Field), None)
        );
        assert_eq!(
            read("Class/method[method]"),
            ("Class/method", Some(SymbolKind::Method), None)
        );
        assert_eq!(read("method[]"), ("method[]", None, None));
        assert_eq!(read("method["), ("method[", None, None));
        assert_eq!(read("[key]"), ("[key]", None, None));
        assert_eq!(read("/[key]"), ("/[key]", None, None));
        assert_eq!(
            read("C/[key][method]"),
            ("C/[key]", Some(SymbolKind::Method), None)
        );
        assert_eq!(
            read("C/[key]"),
            ("C/", Some(SymbolKind::Key), Some("C/[key]"))
        );
    }

    /// A bracket after a `/` qualifies a name that ends in `/` or opens a
    /// computed name, and the text cannot say which, so a pattern names both.
    /// The path a symbol is matched by comes from its own name and kind,
    /// never from reparsing its text.
    #[test]
    fn a_bracket_after_a_slash_names_an_operator_or_a_computed_member() {
        let mut derived = build_symbol("Derived", SymbolKind::Struct);
        derived.children = vec![
            build_symbol("operator/", SymbolKind::Namespace),
            build_symbol("operator/", SymbolKind::Method),
        ];
        let mut symbols = vec![derived];
        Symbol::compute_paths_for_all(&mut symbols);
        let method = &symbols[0].children[1];
        assert_eq!(method.path(), "Derived/operator/[method]");
        assert_eq!(method.unqualified_path(), "Derived/operator/");

        let found = |pattern| {
            Symbol::filter_by_path(&symbols, pattern)
                .iter()
                .map(|symbol| symbol.path().to_string())
                .collect::<Vec<_>>()
        };
        assert_eq!(
            found("Derived/operator/[method]"),
            ["Derived/operator/[method]"]
        );
        assert_eq!(
            found("operator/[namespace]"),
            ["Derived/operator/[namespace]"]
        );
        assert_eq!(found("Derived/operator/").len(), 2);

        let mut class = build_symbol("C", SymbolKind::Class);
        class.children = vec![
            build_symbol("[key]", SymbolKind::Method),
            build_symbol("[key]", SymbolKind::Property),
            build_symbol("[method]", SymbolKind::Method),
        ];
        let mut symbols = vec![class];
        Symbol::compute_paths_for_all(&mut symbols);
        assert_eq!(symbols[0].children[0].path(), "C/[key][method]");
        let found = |pattern| Symbol::filter_by_path(&symbols, pattern).len();
        assert_eq!(found("C/[key]"), 2);
        assert_eq!(found("C/[key][method]"), 1);
        assert_eq!(found("C/[method]"), 1);
        assert_eq!(found("C/[key][field]"), 0);
    }

    /// A qualifier is the declaration's own kind written after its name, so
    /// only that suffix comes off; brackets that are the name stay.
    #[test]
    fn a_path_sheds_only_the_qualifier_its_name_and_kind_spell() {
        assert_eq!(
            Symbol::unqualified("C/[method][method]", "[method]", "method"),
            "C/[method]"
        );
        assert_eq!(
            Symbol::unqualified("C/[method]", "[method]", "method"),
            "C/[method]"
        );
        assert_eq!(
            Symbol::unqualified("Derived/operator/[method]", "operator/", "method"),
            "Derived/operator/"
        );
        assert_eq!(Symbol::unqualified("Foo[A]", "Foo[A]", "class"), "Foo[A]");
        assert_eq!(
            Symbol::unqualified("C/x[field]", "x", "method"),
            "C/x[field]"
        );
    }

    /// A qualifier selects the declarations of its kind, wherever the path is
    /// declared: a namespace adds no segment, so a function in one shares its
    /// path with a same-named one beside it, and both answer `[function]`.
    /// A path printed without a qualifier answers one by its kind.
    #[test]
    fn a_qualifier_selects_the_declarations_of_its_kind() {
        let mut ns = build_symbol("ns", SymbolKind::Namespace);
        ns.children = vec![build_symbol("operator/", SymbolKind::Function)];
        let mut symbols = vec![
            ns,
            build_symbol("operator/", SymbolKind::Namespace),
            build_symbol("operator/", SymbolKind::Function),
        ];
        Symbol::compute_paths_for_all(&mut symbols);
        assert_eq!(symbols[0].children[0].path(), "operator/");
        assert_eq!(symbols[2].path(), "operator/[function]");
        assert_eq!(
            Symbol::filter_by_path(&symbols, "/operator/[function]").len(),
            2
        );
        assert_eq!(
            Symbol::filter_by_path(&symbols, "/operator/[namespace]").len(),
            1
        );

        let mut area = build_symbol("area", SymbolKind::Method);
        area.name_path = Some("Shape/area".to_string());
        assert!(area.matches_path("Shape/area[method]"));
        assert!(!area.matches_path("Shape/area[field]"));
    }

    /// A declaration without a name carries the path of what holds it, so it
    /// answers no path and takes no qualifier: `S[enum]` names no anonymous
    /// enum inside `S`, and `S` names only `S`.
    #[test]
    fn a_declaration_without_a_name_answers_no_path() {
        let mut s = build_symbol("S", SymbolKind::Struct);
        s.children = vec![
            build_symbol("", SymbolKind::Class),
            build_symbol("", SymbolKind::Enum),
            build_symbol("f", SymbolKind::Method),
        ];
        let mut symbols = vec![s];
        Symbol::compute_paths_for_all(&mut symbols);
        assert_eq!(symbols[0].children[1].discriminator, None);
        assert!(Symbol::filter_by_path(&symbols, "S[enum]").is_empty());
        assert_eq!(Symbol::filter_by_path(&symbols, "S").len(), 1);
        assert_eq!(Symbol::filter_by_path(&symbols, "S/f").len(), 1);
    }

    /// The last segment of a path is what its declaration is called, and a
    /// segment is never empty, so a trailing `/` belongs to the name.
    #[test]
    fn a_path_ends_in_the_name_of_its_declaration() {
        assert_eq!(PathPattern::split_name("A/B/c"), (Some("A/B"), "c"));
        assert_eq!(PathPattern::split_name("/c"), (None, "c"));
        assert_eq!(
            PathPattern::split_name("Derived/operator/"),
            (Some("Derived"), "operator/")
        );
        assert_eq!(PathPattern::split_name("operator/"), (None, "operator/"));
        assert_eq!(PathPattern::split_name("C/[key]"), (Some("C"), "[key]"));
    }

    /// Brackets carry type parameters in several languages. Reading one as
    /// a qualifier would leave `Foo[A]` matching nothing and make a bare
    /// `Foo` match it, neither of which the caller asked for.
    #[test]
    fn a_bracketed_type_parameter_is_part_of_the_name() {
        assert_eq!(PathPattern::parse("Foo[A]").path, "Foo[A]");
        assert_eq!(PathPattern::parse("Map[K, V]").path, "Map[K, V]");

        let mut generic = build_symbol("Foo[A]", SymbolKind::Class);
        generic.name_path = Some("Foo[A]".to_string());
        assert!(generic.matches_path("Foo[A]"));
        assert!(!generic.matches_path("Foo"));
    }
}
