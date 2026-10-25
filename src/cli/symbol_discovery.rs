//! Discovery and steering heuristics: how a symbol query is classified, how
//! matches are ranked, and which languages a search covers.
//!
//! What an answer says about its own shortfalls lives in
//! [`crate::cli::response::disclosure`] instead — this module decides what to
//! look for and in what order, that one decides how to admit what was missed.

use std::cmp::Reverse;
use std::path::{Path, PathBuf};

use crate::app::App;
use crate::cli::errors::ErrorCode;
use crate::cli::response::disclosure::{LowerBound, as_paths, name_some, relative_paths};
use crate::cli::{OutputContext, OutputError};
use crate::models::symbol::{Language, PathPattern, Symbol};
use crate::services::test_scope::TestScope;

/// Candidate window for ranking before emission. Exhausting this window
/// is disclosed as a lower bound rather than a complete enumeration.
pub const SYMBOL_CANDIDATE_LIMIT: usize = 1000;

/// How many rows to ask the index for so the ranking, not the index's own
/// order, decides which ones the caller sees.
pub fn candidate_budget(limit: usize) -> usize {
    limit.max(SYMBOL_CANDIDATE_LIMIT)
}

/// What ranking reads from a match, over the row types the surfaces carry.
#[derive(Clone, Copy)]
pub struct RankedSymbol<'a> {
    pub name: &'a str,
    pub name_path: Option<&'a str>,
    pub kind: &'a str,
    pub file: &'a Path,
    pub line: u32,
    pub column: u32,
}

/// A row a symbol surface ranks.
pub trait Rankable {
    fn ranked(&self) -> RankedSymbol<'_>;
}

impl Rankable for Symbol {
    fn ranked(&self) -> RankedSymbol<'_> {
        RankedSymbol {
            name: &self.name,
            name_path: self.name_path.as_deref(),
            kind: self.kind.as_str(),
            file: &self.location.file,
            line: self.location.line,
            column: self.location.column,
        }
    }
}

/// A kind that names a value rather than a declaration to navigate to.
///
/// Distinct from [`crate::models::symbol::SymbolKind::is_low_level`], which
/// decides what a caller may EXCLUDE: a struct field is a declaration worth
/// keeping and worth ranking below the type that holds it.
pub fn low_signal_kind(kind: &str) -> bool {
    matches!(
        kind,
        "variable" | "field" | "property" | "enum_member" | "constant"
    )
}

/// A rank as a published relevance: its share of an exact match, which is the
/// most any match scores. Monotone in the rank, so the number a row carries
/// and the position it holds cannot disagree.
pub fn relevance_of(rank: i32) -> f64 {
    let share = f64::from(rank) / f64::from(EXACT_MATCH_PRIORITY);
    (share.clamp(0.0, 1.0) * 100.0).round() / 100.0
}

/// Where a match ranks against the others for this query.
///
/// One function over every symbol surface: a second copy answering the same
/// question is free to drift, and the two that preceded this one had — they
/// demoted different kinds, so the same symbol led one answer and trailed the
/// other.
/// Textual match tiers dominate test and declaration-kind tie breaks. A kind
/// qualifier selects rather than names, so the query is read as the path it
/// names (`Svc[class]` as `Svc`), and as written too where that can also name
/// a computed member.
pub fn symbol_rank(query: &str, symbol: RankedSymbol<'_>, test_scope: &TestScope) -> i32 {
    let pattern = PathPattern::parse(query);
    let path = symbol.name_path.unwrap_or(symbol.name);
    let priority = std::iter::once(pattern.path)
        .chain(pattern.literal)
        .map(|reading| symbol_match_priority(reading, symbol.name, path))
        .max()
        .unwrap_or(0);
    if priority == 0 {
        return 0;
    }
    let test = i32::from(test_scope.is_test_file(symbol.file));
    let value = i32::from(low_signal_kind(symbol.kind));
    priority - (test * 2 + value)
}

/// Orders matches best first: by [`symbol_rank`], then the shorter name, then
/// position. Every symbol surface sorts through this, so a candidate cut taken
/// over the rows the final ranking reads keeps exactly the matches that
/// ranking would lead with.
pub fn sort_by_rank<T: Rankable>(rows: &mut [T], query: &str, test_scope: &TestScope) {
    rows.sort_by_cached_key(|row| {
        let symbol = row.ranked();
        (
            Reverse(symbol_rank(query, symbol, test_scope)),
            symbol.name.len(),
            symbol.file.to_path_buf(),
            symbol.line,
            symbol.column,
        )
    });
}

/// True when a result set worth steering on sits entirely in one file:
/// more than one match, all sharing a single file. Excludes empty and
/// single-match sets (nothing to concentrate) and multi-file spreads.
pub fn is_single_file_concentration(unique_files: usize, total: usize) -> bool {
    total > 1 && unique_files == 1
}

/// The top of the ladder: a name spelled exactly as the query asked. Nothing
/// scores above it — the broad-query bonus needs a name the term only appears
/// inside — so it is what a published relevance reads as whole.
pub const EXACT_MATCH_PRIORITY: i32 = 40;

/// Relevance tier of a name/path against a query. The ladder is intentionally
/// coarse — exact (40) > anchored path suffix (34) > prefix (24) > substring
/// (16) > no match (0) — so penalties and bonuses (expressed relative to these
/// steps) can reorder within a tier without crossing it. A `*` pattern names a
/// path whole or not at all, so a path it names is exact.
pub fn symbol_match_priority(query: &str, name: &str, path: &str) -> i32 {
    let query = query.trim();
    if query.contains('*') {
        return if PathPattern::names(query, path) {
            EXACT_MATCH_PRIORITY
        } else {
            0
        };
    }
    let q = query.trim_start_matches('/').to_ascii_lowercase();
    let name = name.to_ascii_lowercase();
    let path = path.to_ascii_lowercase();
    let leaf = path.rsplit('/').next().unwrap_or(&path);

    if leaf == q || name == q || path == q {
        EXACT_MATCH_PRIORITY
    } else if path.ends_with(&format!("/{q}")) {
        34
    } else if name.starts_with(&q) {
        24
    } else if name.contains(&q) || path.contains(&q) {
        16
    } else {
        0
    }
}

/// The languages a search covers: the one named, or the code languages the
/// project contains, ranked by file count. An unnamed search is about code
/// — a symbol query has nothing to ask a configuration format, and listing
/// one as a language it could not cover would be noise rather than a gap —
/// while naming a format explicitly is a request, and requests are honored.
/// An unrecognized name covers nothing, which its caller reports.
///
/// Detection walks the tree — only the project-relative paths in `within`
/// when it names any — so it can miss a language whose only files sit under a
/// path it could not read. That language never enters the requested set, so
/// no coverage gap can name it — the walk's shortfall is reported instead, and
/// it is what makes the answer a lower bound.
pub fn resolve_search_languages(
    app: &App,
    language: Option<&str>,
    within: &[PathBuf],
) -> DetectedLanguages {
    match language.map(Language::parse_or_default) {
        Some(Language::Unknown) => DetectedLanguages::default(),
        Some(lang) => DetectedLanguages {
            languages: vec![lang],
            unread_paths: Vec::new(),
        },
        None => {
            let detected = detect_languages_by_file_count(app.root(), &Language::all(), within);
            DetectedLanguages {
                languages: detected
                    .languages
                    .into_iter()
                    .filter(|lang| lang.is_code())
                    .collect(),
                unread_paths: detected.unread_paths,
            }
        }
    }
}

/// The error an empty language set is.
///
/// Empty for three different facts, each with its own remedy, and the caller
/// cannot tell them apart from the set alone. A walk that was turned away
/// never learned what the project holds — the I/O failure is the answer, not
/// the conclusion. A `--lang` naming no language this build knows is an input
/// error. Anything else is a project with no code this command can search,
/// which is a finding. Said once here so no surface re-derives it and gets one
/// of the three wrong.
pub fn no_languages_error(
    ctx: &OutputContext,
    detected: &DetectedLanguages,
    requested: Option<&str>,
) -> OutputError {
    let unread = relative_paths(ctx, &detected.unread_paths);
    if !unread.is_empty() {
        return OutputError::new(
            ErrorCode::Io,
            format!(
                "{} path(s) could not be read ({}), so no language could be detected here",
                unread.len(),
                name_some(&unread)
            ),
        )
        .with_hint("Check the permissions on those paths, or name a language with --lang.");
    }
    match requested {
        Some(language) => OutputError::invalid(format!("Unknown language: {language}"))
            .with_hint("Run 'symora doctor' to see supported languages."),
        None => OutputError::not_found("No source files found to search"),
    }
}

/// The languages a command will ask about, and how much of the tree the walk
/// that chose them could not read.
///
/// The two travel together because a language whose only files sit under an
/// unreadable path never enters the set at all — so nothing downstream can
/// name it as a gap, and only these paths say the answer may be short.
#[derive(Default)]
pub struct DetectedLanguages {
    pub languages: Vec<Language>,
    /// Absolute, as the walk produced them. Every consumer says them through
    /// [`relative_paths`], so there is one form on the way in and one on the
    /// way out; a caller that relativized first would double-transform the
    /// moment the output layer's own convention changes.
    pub unread_paths: Vec<String>,
}

impl DetectedLanguages {
    /// What the walk that chose these languages could not read, as the bound
    /// it makes an answer. Derived here rather than at each call site: a
    /// language hidden behind such a path never enters `languages`, so no
    /// coverage gap can name it and this is the only thing that says the
    /// answer may be short on its account.
    pub fn shortfall(&self, ctx: &OutputContext) -> Option<LowerBound> {
        let unread = relative_paths(ctx, &self.unread_paths);
        (!unread.is_empty()).then_some(LowerBound::ScanCouldNotReadPaths(unread))
    }
}

/// The project's languages, most files first. One detector for every surface
/// that asks the question — a second copy is how a shortfall added here stops
/// reaching the commands that need it.
pub fn detect_languages_by_file_count(
    root: &Path,
    all_languages: &[Language],
    within: &[PathBuf],
) -> DetectedLanguages {
    use std::collections::HashMap;

    let extensions: Vec<&str> = all_languages
        .iter()
        .flat_map(|lang| lang.extensions().iter().copied())
        .collect();
    let filter = if within.is_empty() {
        crate::infra::file_filter::FileFilter::new(root)
    } else {
        crate::infra::file_filter::FileFilter::for_paths(
            root,
            within.iter().map(|path| root.join(path)).collect(),
        )
    };
    let discovery = filter.discover_files(&extensions);
    let mut counts: HashMap<Language, usize> = HashMap::new();

    for file in discovery.files {
        let language = Language::from_path(&file);
        if language != Language::Unknown {
            *counts.entry(language).or_insert(0) += 1;
        }
    }

    let mut ranked: Vec<_> = counts.into_iter().collect();
    ranked.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.lsp_id().cmp(b.0.lsp_id())));
    DetectedLanguages {
        languages: ranked.into_iter().map(|(language, _)| language).collect(),
        unread_paths: as_paths(&discovery.unreadable),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn single_file_concentration_requires_multiple_matches_in_one_file() {
        assert!(!is_single_file_concentration(0, 0));
        assert!(!is_single_file_concentration(1, 1));
        assert!(is_single_file_concentration(1, 5));
        assert!(!is_single_file_concentration(2, 5));
    }

    fn ranked<'a>(name: &'a str, kind: &'a str, file: &'a str) -> RankedSymbol<'a> {
        RankedSymbol {
            name,
            name_path: None,
            kind,
            file: Path::new(file),
            line: 1,
            column: 1,
        }
    }

    #[test]
    fn exact_matches_precede_prefixes_and_substrings_across_symbol_kinds() {
        let scope = TestScope::new();
        let rank =
            |name: &str, kind: &str| symbol_rank("user", ranked(name, kind, "src/lib.rs"), &scope);

        let exact = rank("user", "variable");
        let prefix = rank("userservice", "class");
        let substring = rank("currentuser", "class");

        assert!(
            exact > prefix,
            "exact identifiers remain addressable ahead of prefixes: {prefix} vs {exact}"
        );
        assert!(
            exact > substring,
            "the declaration spelled exactly as asked outranks a name that merely contains it: \
             {exact} vs {substring}"
        );
    }

    #[test]
    fn a_kind_qualifier_ranks_by_the_path_it_qualifies() {
        let scope = TestScope::new();
        let class = ranked("Svc", "class", "src/svc.py");
        assert_eq!(
            symbol_rank("Svc[class]", class, &scope),
            symbol_rank("Svc", class, &scope)
        );
        assert_eq!(
            symbol_rank("Svc[class]", class, &scope),
            EXACT_MATCH_PRIORITY
        );
        let computed = RankedSymbol {
            name_path: Some("Cache/[method]"),
            ..ranked("[method]", "method", "src/cache.ts")
        };
        assert_eq!(
            symbol_rank("Cache/[method]", computed, &scope),
            EXACT_MATCH_PRIORITY
        );
    }

    #[test]
    fn a_wildcard_ranks_a_path_it_names_as_exact() {
        let scope = TestScope::new();
        let member = RankedSymbol {
            name_path: Some("Cart/new"),
            ..ranked("new", "method", "src/cart.rs")
        };
        for query in ["*/new", "Cart/*", "/Cart/*", "C*t/n*", "*[method]"] {
            assert_eq!(
                symbol_rank(query, member, &scope),
                EXACT_MATCH_PRIORITY,
                "{query}"
            );
        }
        for query in ["Order/*", "/*", "*/total"] {
            assert_eq!(symbol_rank(query, member, &scope), 0, "{query}");
        }
    }

    #[test]
    fn sorting_demotes_tests_within_a_tier_and_breaks_ties_by_position() {
        struct Row(&'static str, &'static str, &'static str, u32);
        impl Rankable for Row {
            fn ranked(&self) -> RankedSymbol<'_> {
                RankedSymbol {
                    line: self.3,
                    ..ranked(self.0, self.1, self.2)
                }
            }
        }
        let mut rows = [
            Row("UserStore", "class", "a/user.rs", 1),
            Row("User", "class", "a/tests/user.rs", 1),
            Row("User", "class", "b/user.rs", 9),
            Row("User", "class", "b/user.rs", 2),
            Row("User", "variable", "a/user.rs", 1),
        ];
        sort_by_rank(&mut rows, "user", &TestScope::new());
        let order: Vec<_> = rows
            .iter()
            .map(|row| (row.0, row.1, row.2, row.3))
            .collect();
        assert_eq!(
            order,
            [
                ("User", "class", "b/user.rs", 2),
                ("User", "class", "b/user.rs", 9),
                ("User", "variable", "a/user.rs", 1),
                ("User", "class", "a/tests/user.rs", 1),
                ("UserStore", "class", "a/user.rs", 1),
            ],
            "an exact name leads every prefix; within it a value kind and then a test file \
             give way, and position settles what rank and length cannot"
        );
    }
}
