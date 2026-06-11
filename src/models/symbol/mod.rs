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
        let base_path = match parent_path {
            Some(parent) => format!("{}/{}", parent, self.name),
            None => self.name.clone(),
        };

        self.name_path = Some(match self.overload_idx {
            Some(idx) => format!("{}[{}]", base_path, idx),
            None => base_path.clone(),
        });

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
        let path = self.path();

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

    fn matches_glob_part(value: &str, pattern: &str) -> bool {
        if pattern == "*" {
            return true;
        }

        if let Some(prefix) = pattern.strip_suffix('*') {
            return value.starts_with(prefix);
        }

        if let Some(suffix) = pattern.strip_prefix('*') {
            return value.ends_with(suffix);
        }

        if let Some((prefix, suffix)) = pattern.split_once('*') {
            return value.starts_with(prefix) && value.ends_with(suffix);
        }

        value == pattern
    }

    pub fn filter_by_path(symbols: &[Symbol], pattern: &str) -> Vec<Symbol> {
        let mut results = Vec::new();
        Self::collect_matching(symbols, pattern, &mut results);
        results
    }

    fn collect_matching(symbols: &[Symbol], pattern: &str, results: &mut Vec<Symbol>) {
        for symbol in symbols {
            if symbol.matches_path(pattern) {
                results.push(symbol.clone());
            }
            Self::collect_matching(&symbol.children, pattern, results);
        }
    }

    /// All symbols whose exact path equals `path`, in tree order.
    /// Children of distinct same-named parents share paths — overload
    /// indices apply only at the colliding level, and children are keyed
    /// off the index-free base path — so an exact path can legitimately
    /// match more than one symbol; callers decide whether that is an
    /// error.
    pub fn find_all_by_path<'a>(symbols: &'a [Symbol], path: &str) -> Vec<&'a Symbol> {
        let mut matches = Vec::new();
        Self::collect_exact_path(symbols, path, &mut matches);
        matches
    }

    fn collect_exact_path<'a>(symbols: &'a [Symbol], path: &str, matches: &mut Vec<&'a Symbol>) {
        for symbol in symbols {
            if symbol.path() == path {
                matches.push(symbol);
            }
            Self::collect_exact_path(&symbol.children, path, matches);
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
        let mut results = Vec::new();
        Self::collect_advanced(
            symbols,
            pattern,
            substring,
            include_kinds,
            exclude_kinds,
            exclude_low_level,
            &mut results,
        );
        results
    }

    fn collect_advanced(
        symbols: &[Symbol],
        pattern: Option<&str>,
        substring: bool,
        include_kinds: Option<&[SymbolKind]>,
        exclude_kinds: Option<&[SymbolKind]>,
        exclude_low_level: bool,
        results: &mut Vec<Symbol>,
    ) {
        for symbol in symbols {
            let excluded = exclude_kinds.is_some_and(|k| k.contains(&symbol.kind))
                || include_kinds.is_some_and(|k| !k.contains(&symbol.kind))
                || (exclude_low_level && symbol.kind.is_low_level());

            if excluded {
                Self::collect_advanced(
                    &symbol.children,
                    pattern,
                    substring,
                    include_kinds,
                    exclude_kinds,
                    exclude_low_level,
                    results,
                );
                continue;
            }

            let matches = match pattern {
                None => true,
                Some(p) if substring => symbol.matches_substring(p),
                Some(p) => symbol.matches_path(p),
            };

            if matches {
                results.push(symbol.clone());
            }

            Self::collect_advanced(
                &symbol.children,
                pattern,
                substring,
                include_kinds,
                exclude_kinds,
                exclude_low_level,
                results,
            );
        }
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

    pub fn strip_type_parameters(name: &str) -> String {
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

    #[test]
    fn filter_by_path_recurses_into_children() {
        let mut class = build_symbol("MyClass", SymbolKind::Class);
        class.children = vec![
            build_symbol("update", SymbolKind::Method),
            build_symbol("reset", SymbolKind::Method),
        ];
        class.compute_paths(None);

        let results = Symbol::filter_by_path(&[class.clone()], "MyClass/update");
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].name, "update");

        let results = Symbol::filter_by_path(&[class.clone()], "*/reset");
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].name, "reset");

        let results = Symbol::filter_by_path(&[class], "MyClass/*");
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
    /// off the index-free base path — so one exact child path can match
    /// several symbols, while an indexed parent path stays unique.
    #[test]
    fn find_all_by_path_returns_every_exact_match() {
        let mut first = build_symbol("Foo", SymbolKind::Class);
        first.children = vec![build_symbol("bar", SymbolKind::Method)];
        let mut second = build_symbol("Foo", SymbolKind::Class);
        second.children = vec![build_symbol("bar", SymbolKind::Method)];

        let mut symbols = vec![first, second];
        Symbol::compute_paths_for_all(&mut symbols);

        assert_eq!(symbols[0].path(), "Foo[0]");
        assert_eq!(symbols[1].path(), "Foo[1]");
        assert_eq!(Symbol::find_all_by_path(&symbols, "Foo/bar").len(), 2);
        assert_eq!(Symbol::find_all_by_path(&symbols, "Foo[0]").len(), 1);
        assert!(Symbol::find_all_by_path(&symbols, "Foo/missing").is_empty());
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
