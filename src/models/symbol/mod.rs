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
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub children: Vec<Symbol>,
    /// The kind that tells this symbol apart from its same-named siblings,
    /// set only when it does. See [`Symbol::compute_paths_for_all`].
    #[serde(skip_serializing_if = "Option::is_none")]
    pub discriminator: Option<SymbolKind>,
}

impl Symbol {
    pub fn new(name: String, kind: SymbolKind, location: Location) -> Self {
        Self {
            name,
            name_path: None,
            kind,
            location,
            container: None,
            body: None,
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
            let identifies =
                kinds.len() > 1 && kinds.iter().filter(|k| **k == symbol.kind).count() == 1;
            symbol.discriminator = identifies.then_some(symbol.kind);

            if !symbol.children.is_empty() {
                Self::assign_discriminators(&mut symbol.children);
            }
        }
    }

    pub fn path(&self) -> &str {
        self.name_path.as_deref().unwrap_or(&self.name)
    }

    pub fn matches_path(&self, pattern: &str) -> bool {
        Self::path_matches(self.path(), pattern)
    }

    /// Match a precomputed path string against a `--symbol`-style pattern: a
    /// leading `/` forces a full-path exact match, otherwise bare
    /// last-component / `/`-anchored suffix / `*` wildcard matching. Shared by
    /// the symbol method and the index-row glob filter so every surface
    /// resolves a pattern the same way.
    pub fn path_matches(path: &str, pattern: &str) -> bool {
        if let Some(abs_pattern) = pattern.strip_prefix('/') {
            return Self::matches_pattern(path, abs_pattern, true);
        }
        Self::matches_pattern(path, pattern, false)
    }

    fn matches_pattern(path: &str, pattern: &str, exact: bool) -> bool {
        let (pattern_base, pattern_qualifier) = Self::split_qualifier(pattern);
        let (path_base, path_qualifier) = Self::split_qualifier(path);

        if let Some(qualifier) = pattern_qualifier
            && path_qualifier != Some(qualifier)
        {
            return false;
        }

        let pattern = pattern_base;
        let path = path_base;

        if pattern.contains('*') {
            Self::matches_wildcard(path, pattern, exact)
        } else if exact {
            path == pattern
        } else if pattern.contains('/') {
            path == pattern || path.ends_with(&format!("/{}", pattern))
        } else {
            let name = path.rsplit('/').next().unwrap_or(path);
            name == pattern
        }
    }

    /// Split a path into its base and its trailing `[kind]`, if any.
    ///
    /// Only a real kind counts. Brackets are ordinary name syntax in
    /// several languages — a Scala or Kotlin `Foo[A]` is one name, not a
    /// qualified one — and reading those as qualifiers would make a pattern
    /// match nothing while a bare `Foo` started matching them.
    ///
    /// An unqualified pattern matches a qualified path, so an agent that
    /// knows only the name still reaches every candidate.
    fn split_qualifier(s: &str) -> (&str, Option<&str>) {
        if let Some(bracket) = s.rfind('[')
            && let Some(qualifier) = s[bracket..]
                .strip_prefix('[')
                .and_then(|q| q.strip_suffix(']'))
            && qualifier.parse::<SymbolKind>().is_ok()
        {
            return (&s[..bracket], Some(qualifier));
        }
        (s, None)
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

    pub fn filter_advanced(
        symbols: &[Symbol],
        pattern: Option<&str>,
        substring: bool,
        include_kinds: Option<&[SymbolKind]>,
        exclude_kinds: Option<&[SymbolKind]>,
        exclude_low_level: bool,
    ) -> Vec<Symbol> {
        let mut out = Vec::new();
        Self::collect_by(
            symbols,
            &|symbol| {
                let excluded = exclude_kinds.is_some_and(|k| k.contains(&symbol.kind))
                    || include_kinds.is_some_and(|k| !k.contains(&symbol.kind))
                    || (exclude_low_level && symbol.kind.is_low_level());
                if excluded {
                    return false;
                }
                match pattern {
                    None => true,
                    Some(p) if substring => symbol.matches_substring(p),
                    Some(p) => symbol.matches_path(p),
                }
            },
            &mut out,
        );
        out.into_iter().cloned().collect()
    }

    pub fn normalize_name(name: &str, file: &std::path::Path, kind: SymbolKind) -> String {
        let name = name.trim();

        // An empty name is an intentionally transparent container — e.g. a
        // nameless-self-type impl (`impl Tr for fn()`) already reduced to an
        // empty segment by `normalize_symbol_name`. Keep it empty so
        // `compute_paths` lets its members attach to the enclosing path; minting
        // a `<stem>_<suffix>` segment here would re-qualify them (`lib_config/m`)
        // and break the cross-surface name_path. Only the LSP's anonymous
        // markers below earn a synthesized, addressable name.
        if name.is_empty() {
            return String::new();
        }

        if name != "<unknown>" && name != "<anonymous>" && !name.starts_with('<') {
            return name.to_string();
        }

        let stem = file
            .file_stem()
            .and_then(|s| s.to_str())
            .unwrap_or("anonymous");

        let suffix = match kind {
            SymbolKind::Module => "module",
            SymbolKind::Function => "fn",
            SymbolKind::Variable | SymbolKind::Constant => "export",
            SymbolKind::Object => "config",
            _ => "symbol",
        };

        format!("{}_{}", stem, suffix)
    }

    /// Normalize a raw LSP documentSymbol name into the segment used for
    /// symbol paths. Functions/methods drop their parameter list and
    /// generics; an `impl` block collapses to its bare implementing type, so
    /// a method reads `Type/method` on every surface — `impl Foo`,
    /// `impl Trait for Foo`, and `impl<T> Foo<T>` all yield `Foo`, matching
    /// the index extractor (which keys on the impl's self type, not the trait).
    pub fn normalize_symbol_name(name: &str) -> String {
        let name = name.trim();
        if let Some(rest) = name.strip_prefix("impl")
            && rest.starts_with(|c: char| c.is_whitespace() || c == '<')
        {
            return Self::self_type_segment(rest);
        }
        Self::strip_type_parameters(name)
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

    fn strip_type_parameters(name: &str) -> String {
        let name = name.trim();

        let name = if let Some(paren_pos) = name.find('(') {
            &name[..paren_pos]
        } else {
            name
        };

        let name = if let Some(angle_pos) = name.find('<') {
            &name[..angle_pos]
        } else {
            name
        };

        name.trim().to_string()
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
    fn normalize_name_keeps_empty_transparent_but_names_anonymous_markers() {
        let f = std::path::Path::new("src/lib.rs");
        // An intentionally-empty container stays transparent (no stray segment).
        assert_eq!(Symbol::normalize_name("", f, SymbolKind::Object), "");
        assert_eq!(Symbol::normalize_name("   ", f, SymbolKind::Class), "");
        // The LSP's anonymous markers still earn an addressable file-stem name.
        assert_eq!(
            Symbol::normalize_name("<anonymous>", f, SymbolKind::Object),
            "lib_config"
        );
        assert_eq!(
            Symbol::normalize_name("<unknown>", f, SymbolKind::Function),
            "lib_fn"
        );
        // A real identifier passes through untouched.
        assert_eq!(
            Symbol::normalize_name("foo", f, SymbolKind::Function),
            "foo"
        );
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

        let include_kinds = vec![SymbolKind::Method];
        let results = Symbol::filter_advanced(
            &[class.clone()],
            None,
            false,
            Some(&include_kinds),
            None,
            false,
        );
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].name, "update");

        let exclude_kinds = vec![SymbolKind::Variable];
        let results = Symbol::filter_advanced(
            &[class.clone()],
            None,
            false,
            None,
            Some(&exclude_kinds),
            false,
        );
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

        let results = Symbol::filter_advanced(&[class], None, false, None, None, true);
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

        let results = Symbol::filter_advanced(&[class], Some("User"), true, None, None, false);
        assert_eq!(results.len(), 3);
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

    #[test]
    fn split_qualifier_separates_a_trailing_bracketed_token() {
        assert_eq!(Symbol::split_qualifier("method"), ("method", None));
        assert_eq!(
            Symbol::split_qualifier("method[field]"),
            ("method", Some("field"))
        );
        assert_eq!(
            Symbol::split_qualifier("Class/method[method]"),
            ("Class/method", Some("method"))
        );
        assert_eq!(Symbol::split_qualifier("method[]"), ("method[]", None));
        assert_eq!(Symbol::split_qualifier("method["), ("method[", None));
    }

    /// Brackets carry type parameters in several languages. Reading one as
    /// a qualifier would leave `Foo[A]` matching nothing and make a bare
    /// `Foo` match it, neither of which the caller asked for.
    #[test]
    fn a_bracketed_type_parameter_is_part_of_the_name() {
        assert_eq!(Symbol::split_qualifier("Foo[A]"), ("Foo[A]", None));
        assert_eq!(Symbol::split_qualifier("Map[K, V]"), ("Map[K, V]", None));

        let mut generic = build_symbol("Foo[A]", SymbolKind::Class);
        generic.name_path = Some("Foo[A]".to_string());
        assert!(generic.matches_path("Foo[A]"));
        assert!(!generic.matches_path("Foo"));
    }
}
