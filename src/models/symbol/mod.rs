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
    #[serde(skip_serializing_if = "Option::is_none")]
    pub overload_idx: Option<u32>,
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
            overload_idx: None,
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

        self.name_path = Some(match self.overload_idx {
            Some(idx) => format!("{}[{}]", base_path, idx),
            None => base_path.clone(),
        });

        // Children hang off the index-free base so same-named sibling parents
        // (`Foo[0]`, `Foo[1]`) still share a child path (`Foo/bar`).
        for child in &mut self.children {
            child.compute_paths(Some(&base_path));
        }
    }

    pub fn compute_paths_for_all(symbols: &mut [Symbol]) {
        Self::assign_overload_indices(symbols);
        for symbol in symbols {
            symbol.compute_paths(None);
        }
    }

    fn assign_overload_indices(symbols: &mut [Symbol]) {
        use std::collections::HashMap;

        let mut name_counts: HashMap<String, u32> = HashMap::new();
        for symbol in symbols.iter() {
            *name_counts.entry(symbol.name.clone()).or_insert(0) += 1;
        }

        let mut name_indices: HashMap<String, u32> = HashMap::new();
        for symbol in symbols.iter_mut() {
            let count = name_counts.get(&symbol.name).copied().unwrap_or(1);
            if count > 1 {
                let idx = name_indices.entry(symbol.name.clone()).or_insert(0);
                symbol.overload_idx = Some(*idx);
                *idx += 1;
            }

            if !symbol.children.is_empty() {
                Self::assign_overload_indices(&mut symbol.children);
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
        let (pattern_base, pattern_idx) = Self::parse_overload_index(pattern);
        let (path_base, path_idx) = Self::parse_overload_index(path);

        if let Some(pidx) = pattern_idx
            && path_idx != Some(pidx)
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

    fn parse_overload_index(s: &str) -> (&str, Option<u32>) {
        if let Some(bracket_pos) = s.rfind('[')
            && s.ends_with(']')
            && let Ok(idx) = s[bracket_pos + 1..s.len() - 1].parse::<u32>()
        {
            return (&s[..bracket_pos], Some(idx));
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

        if !name.is_empty()
            && name != "<unknown>"
            && name != "<anonymous>"
            && !name.starts_with('<')
        {
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
            return Self::impl_self_type(rest);
        }
        Self::strip_type_parameters(name)
    }

    /// The implementing type from an impl header (the text after `impl`): the
    /// type after ` for ` for a trait impl, otherwise the head type, reduced to
    /// the bare type name — the where-clause, the impl and type generics, a
    /// leading `&`/`&mut`/`dyn`/lifetime, and any module path all stripped.
    fn impl_self_type(after_impl: &str) -> String {
        let head = after_impl.rsplit(" for ").next().unwrap_or(after_impl);
        let head = head.split(" where ").next().unwrap_or(head).trim();

        // Structural self types — tuple `(A, B)`, array/slice `[T; N]`,
        // pointer `*const T`, fn-pointer `fn(..)`, qualified `<T as Tr>::X` —
        // have no outer nominal name. Take the first nominal type identifier
        // within (an empty string for a truly nameless one like `fn()`),
        // matching the index extractor's first-`type_identifier` rule so the
        // two surfaces agree on the path. Checked before stripping leading
        // generics so a qualified type's `<…>` isn't mistaken for impl params.
        if head.starts_with(['(', '[', '*'])
            || head.starts_with("fn(")
            || (head.starts_with('<') && head.contains(" as "))
        {
            return Self::first_nominal_ident(head);
        }

        // Plain nominal or module path: a leading `<…>` here is the generic
        // params of an inherent `impl<T> Foo<T>` (the trait side, if any, was
        // already split off at ` for `). Drop them, then drop the type's own
        // generics, peel a leading reference/mut/dyn/lifetime, and take the
        // bare type name (last path segment).
        let head = Self::strip_leading_generics(head);
        let mut ty = head.split('<').next().unwrap_or(head).trim();
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
        ty.rsplit("::").next().unwrap_or(ty).trim().to_string()
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

    /// Malformed overload indices degrade to literal names (no match against
    /// real symbols), never a parse panic or a wrong match.
    #[test]
    fn matches_path_treats_malformed_overload_index_as_literal() {
        let mut sym = build_symbol("bar", SymbolKind::Method);
        sym.name_path = Some("Foo/bar".to_string());

        assert!(!sym.matches_path("bar[abc]"));
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

    #[test]
    fn overload_indices_assign_only_on_collisions() {
        let mut class = build_symbol("MyClass", SymbolKind::Class);
        class.children = vec![
            build_symbol("doSomething", SymbolKind::Method),
            build_symbol("doSomething", SymbolKind::Method),
            build_symbol("doSomething", SymbolKind::Method),
            build_symbol("unique", SymbolKind::Method),
        ];

        let mut symbols = vec![class];
        Symbol::compute_paths_for_all(&mut symbols);

        let class = &symbols[0];
        assert_eq!(class.children[0].overload_idx, Some(0));
        assert_eq!(class.children[1].overload_idx, Some(1));
        assert_eq!(class.children[2].overload_idx, Some(2));
        assert_eq!(class.children[3].overload_idx, None);

        assert_eq!(
            class.children[0].name_path,
            Some("MyClass/doSomething[0]".to_string())
        );
        assert_eq!(
            class.children[3].name_path,
            Some("MyClass/unique".to_string())
        );
    }

    /// Colliding parents get overload indices, but their children hang
    /// off the index-free base path — so one child path can match several
    /// symbols (an ambiguity edit refuses), while an indexed parent path
    /// stays unique.
    #[test]
    fn filter_by_path_matches_same_named_parents_children() {
        let mut first = build_symbol("Foo", SymbolKind::Class);
        first.children = vec![build_symbol("bar", SymbolKind::Method)];
        let mut second = build_symbol("Foo", SymbolKind::Class);
        second.children = vec![build_symbol("bar", SymbolKind::Method)];

        let mut symbols = vec![first, second];
        Symbol::compute_paths_for_all(&mut symbols);

        assert_eq!(symbols[0].path(), "Foo[0]");
        assert_eq!(symbols[1].path(), "Foo[1]");
        assert_eq!(Symbol::filter_by_path(&symbols, "Foo/bar").len(), 2);
        assert_eq!(Symbol::filter_by_path(&symbols, "Foo[0]").len(), 1);
        assert!(Symbol::filter_by_path(&symbols, "Foo/missing").is_empty());
    }

    #[test]
    fn matches_path_filters_by_overload_index() {
        let mut sym = build_symbol("doSomething", SymbolKind::Method);
        sym.overload_idx = Some(1);
        sym.name_path = Some("MyClass/doSomething[1]".to_string());

        assert!(sym.matches_path("doSomething"));
        assert!(sym.matches_path("doSomething[1]"));
        assert!(!sym.matches_path("doSomething[0]"));
        assert!(sym.matches_path("MyClass/doSomething[1]"));
        assert!(!sym.matches_path("MyClass/doSomething[0]"));
    }

    #[test]
    fn matches_path_supports_absolute_form() {
        let mut sym = build_symbol("update", SymbolKind::Method);
        sym.name_path = Some("MyClass/update".to_string());

        assert!(sym.matches_path("update"));
        assert!(sym.matches_path("MyClass/update"));

        assert!(sym.matches_path("/MyClass/update"));
        assert!(!sym.matches_path("/update"));
        assert!(!sym.matches_path("/Other/MyClass/update"));
    }

    #[test]
    fn parse_overload_index_extracts_bracketed_int() {
        assert_eq!(Symbol::parse_overload_index("method"), ("method", None));
        assert_eq!(
            Symbol::parse_overload_index("method[0]"),
            ("method", Some(0))
        );
        assert_eq!(
            Symbol::parse_overload_index("method[123]"),
            ("method", Some(123))
        );
        assert_eq!(
            Symbol::parse_overload_index("Class/method[2]"),
            ("Class/method", Some(2))
        );
        assert_eq!(
            Symbol::parse_overload_index("method[abc]"),
            ("method[abc]", None)
        );
        assert_eq!(Symbol::parse_overload_index("method["), ("method[", None));
    }
}
