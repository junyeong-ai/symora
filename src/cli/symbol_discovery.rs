//! Discovery and steering heuristics: how a symbol query is classified, how
//! matches are ranked, and which languages a search covers.
//!
//! What an answer says about its own shortfalls lives in
//! [`crate::cli::response::disclosure`] instead — this module decides what to
//! look for and in what order, that one decides how to admit what was missed.

use std::path::Path;

use crate::app::App;
use crate::cli::errors::ErrorCode;
use crate::cli::response::disclosure::{LowerBound, as_paths, name_some, relative_paths};
use crate::cli::{OutputContext, OutputError};
use crate::models::symbol::Language;
use crate::services::test_scope::TestScope;

// Ranking weights for symbol discovery. Every value is expressed relative to
// the `symbol_match_priority` tier ladder (exact = 40, anchored-suffix = 34,
// prefix = 24, substring = 16) so the magnitudes are justified against each
// other rather than tuned in isolation. Changing one means re-checking the
// ordering tests below — they pin the intended relative outcomes.

/// A prefix match whose only extra over the query is a test-noise suffix
/// (`userTest` for `user`) is demoted by the same magnitude as a low-signal
/// kind: enough to sink it below a clean match of the same kind, never enough
/// to push it under an unrelated substring hit.
pub const NOISY_SUFFIX_PENALTY: i32 = 6;

/// A low-signal-kind symbol (variable/field/property/constant) whose name
/// equals a short generic query is rarely the navigation target when a
/// declaration carrying the term exists. Added to `LOW_SIGNAL_KIND_PENALTY`,
/// it must clear `BROAD_QUERY_HIGH_SIGNAL_BONUS` so a high-signal PREFIX match
/// wins, and stay under the ladder's 16-point tier gap so a high-signal
/// SUBSTRING match — a term the name merely contains — does not. A demotion
/// that spans a whole tier cancels exactness itself: `TrendPoint` led a search
/// for `endpoint` over the declaration spelled exactly that.
pub const GENERIC_LOW_SIGNAL_EXACT_PENALTY: i32 = 8;

/// An enum member exactly matching a short generic query is low-signal, but is
/// at least a named, addressable declaration — so it is demoted less than a
/// bare variable (`GENERIC_LOW_SIGNAL_EXACT_PENALTY`).
pub const GENERIC_ENUM_MEMBER_EXACT_PENALTY: i32 = 6;

/// For a broad single-word query, lifts a high-signal kind (class/struct/
/// interface/enum/function/method/constructor) whose name *contains* the term
/// above a same-named low-signal exact match — matching how an agent reads
/// "show me the User thing".
pub const BROAD_QUERY_HIGH_SIGNAL_BONUS: i32 = 8;

/// A symbol declared in a test file is demoted in discovery rankings: agents
/// almost always want the production declaration first. Same magnitude as
/// `BROAD_QUERY_HIGH_SIGNAL_BONUS` so a high-signal production match always
/// outranks a test-file match of equal textual relevance.
pub const TEST_FILE_PENALTY: i32 = 8;

/// A low-signal kind (variable/field/property/enum member/constant) is demoted
/// relative to a declaration of the same textual relevance.
pub const LOW_SIGNAL_KIND_PENALTY: i32 = 6;

/// Below this length a query is too short to confidently classify a low-signal
/// exact match as noise (a 2–3 char query exact-matching a field may genuinely
/// be the target), so the generic-exactness penalties do not apply.
const GENERIC_QUERY_MIN_LEN: usize = 4;

/// How many matches a ranking chooses from. A page selected by an order other
/// than the one that ranks it is arbitrary with respect to the answer, so the
/// index is asked for candidates rather than for the answer itself. Measured
/// over six repositories, 95% of ordinary queries match fewer rows than this
/// and are therefore ranked over every match they have.
pub const SYMBOL_CANDIDATE_LIMIT: usize = 1000;

/// How many rows to ask the index for so the ranking, not the index's own
/// order, decides which ones the caller sees.
pub fn candidate_budget(limit: usize) -> usize {
    limit.max(SYMBOL_CANDIDATE_LIMIT)
}

/// What ranking reads from a match, over the row types the surfaces carry.
pub struct RankedSymbol<'a> {
    pub name: &'a str,
    pub name_path: Option<&'a str>,
    pub kind: &'a str,
    pub file: &'a Path,
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

/// Where a match ranks against the others for this query.
///
/// One function over every symbol surface: a second copy answering the same
/// question is free to drift, and the two that preceded this one had — they
/// demoted different kinds, so the same symbol led one answer and trailed the
/// other.
pub fn symbol_rank(query: &str, symbol: RankedSymbol<'_>, test_scope: &TestScope) -> i32 {
    let q = query.trim().trim_start_matches('/').to_ascii_lowercase();
    let name = symbol.name.to_ascii_lowercase();
    let path = symbol.name_path.unwrap_or(symbol.name).to_ascii_lowercase();
    let low_signal = low_signal_kind(symbol.kind);

    let test_penalty = if test_scope.is_test_file(symbol.file) {
        TEST_FILE_PENALTY
    } else {
        0
    };
    let kind_penalty = if low_signal {
        LOW_SIGNAL_KIND_PENALTY
    } else {
        0
    };

    symbol_match_priority(query, &name, &path)
        + broad_symbol_kind_bonus(query, &name, symbol.kind, low_signal)
        - test_penalty
        - kind_penalty
        - noisy_suffix_penalty(&name, &q)
        - generic_exact_identifier_penalty(query, &name, symbol.kind, low_signal)
}

/// A simple lowercase query of at most this many characters (`user`, `parse`,
/// `handler`) is treated as a broad common term for hint and ranking gating.
/// Eight covers the bulk of single-word domain nouns without catching compound
/// identifiers like `parsefile`. This only steers hints and tie-breaking; it
/// never suppresses results.
const GENERIC_QUERY_MAX_LEN: usize = 8;

pub fn symbol_lookup_hints(
    query: &str,
    path_mode: bool,
    lang_is_none: bool,
    no_kind_filter: bool,
    truncated: bool,
    result_count: usize,
    limit: usize,
) -> Vec<String> {
    if result_count <= 1 && !truncated {
        return Vec::new();
    }

    let mut hints = Vec::new();
    if is_generic_broad_query(query) {
        hints.push(
            "This query is very broad; prefer a more specific domain term or add --kind first"
                .to_string(),
        );
    }
    // Only when the emission cap is what bound the list. A count above the
    // rows can also come from matches noise suppression removed, or from a
    // source total larger than the rows in hand — raising `--limit` surfaces
    // neither, so prescribing it would send an agent after results that
    // cannot arrive.
    if truncated && result_count >= limit {
        hints.push("Narrow results with a longer query or increase --limit".to_string());
    }
    if !path_mode {
        hints.push(
            "Use --symbol with a path-like query such as Class/method or */update for precise matches"
                .to_string(),
        );
    }
    if lang_is_none {
        hints.push("Add --lang to constrain search in mixed-language workspaces".to_string());
    }
    if no_kind_filter {
        hints.push(
            "Add --kind to focus on classes, methods, functions, or other symbol kinds".to_string(),
        );
    }
    hints.truncate(3);
    hints
}

/// True when a result set worth steering on sits entirely in one file:
/// more than one match, all sharing a single file. Excludes empty and
/// single-match sets (nothing to concentrate) and multi-file spreads.
pub fn is_single_file_concentration(unique_files: usize, total: usize) -> bool {
    total > 1 && unique_files == 1
}

/// Relevance tier of a name/path against a query. The ladder is intentionally
/// coarse — exact (40) > anchored path suffix (34) > prefix (24) > substring
/// (16) > no match (0) — so penalties and bonuses (expressed relative to these
/// steps) can reorder within a tier without crossing it.
pub fn symbol_match_priority(query: &str, name: &str, path: &str) -> i32 {
    let q = query.trim().trim_start_matches('/').to_ascii_lowercase();
    let name = name.to_ascii_lowercase();
    let path = path.to_ascii_lowercase();
    let leaf = path.rsplit('/').next().unwrap_or(&path);

    if leaf == q || name == q || path == q {
        40
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

/// Demote a prefix match whose only extra is a test-noise suffix (e.g.
/// `userTest` for query `user`). Error/exception types are deliberately
/// NOT noise — `StoreError` is exactly what a search for `Store` wants.
pub fn noisy_suffix_penalty(name: &str, query: &str) -> i32 {
    if name == query || !name.starts_with(query) {
        return 0;
    }

    let suffixes = ["test", "tests", "spec"];
    if suffixes.iter().any(|suffix| name.ends_with(suffix)) {
        NOISY_SUFFIX_PENALTY
    } else {
        0
    }
}

pub fn generic_exact_identifier_penalty(
    query: &str,
    name: &str,
    kind: &str,
    low_signal_kind: bool,
) -> i32 {
    let q = query.trim().trim_start_matches('/').to_ascii_lowercase();
    if q.len() < GENERIC_QUERY_MIN_LEN || !is_simple_lower_query(&q) {
        return 0;
    }

    let lower_name = name.to_ascii_lowercase();
    if low_signal_kind && lower_name == q {
        return GENERIC_LOW_SIGNAL_EXACT_PENALTY;
    }
    if kind == "enum_member" && lower_name == q {
        return GENERIC_ENUM_MEMBER_EXACT_PENALTY;
    }
    0
}

pub fn broad_symbol_kind_bonus(query: &str, name: &str, kind: &str, low_signal_kind: bool) -> i32 {
    let q = query.trim().trim_start_matches('/').to_ascii_lowercase();
    if q.len() < GENERIC_QUERY_MIN_LEN || !is_simple_lower_query(&q) || low_signal_kind {
        return 0;
    }

    let lower_name = name.to_ascii_lowercase();
    let is_high_signal_kind = matches!(
        kind,
        "class" | "struct" | "interface" | "enum" | "function" | "method" | "constructor"
    );

    if is_high_signal_kind && lower_name.contains(&q) && lower_name != q {
        BROAD_QUERY_HIGH_SIGNAL_BONUS
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
/// Detection walks the tree, so it can miss a language whose only files sit
/// under a path it could not read. That language never enters the requested
/// set, so no coverage gap can name it — the walk's shortfall is reported
/// instead, and it is what makes the answer a lower bound.
pub fn resolve_search_languages(app: &App, language: Option<&str>) -> DetectedLanguages {
    match language.map(Language::parse_or_default) {
        Some(Language::Unknown) => DetectedLanguages::default(),
        Some(lang) => DetectedLanguages {
            languages: vec![lang],
            unread_paths: Vec::new(),
        },
        None => {
            let detected = detect_languages_by_file_count(app.root(), &Language::all());
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
) -> DetectedLanguages {
    use std::collections::HashMap;

    let extensions: Vec<&str> = all_languages
        .iter()
        .flat_map(|lang| lang.extensions().iter().copied())
        .collect();
    let filter = crate::infra::file_filter::FileFilter::new(root);
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

fn is_simple_lower_query(query: &str) -> bool {
    query
        .chars()
        .all(|ch| ch.is_ascii_lowercase() || ch.is_ascii_digit() || ch == '_' || ch == '-')
}

/// A short, simple lowercase query likely to be a broad common term. Pure
/// function of the query text — the decision never depends on how many results
/// a search returned, so the same query always classifies the same way.
pub fn is_generic_broad_query(query: &str) -> bool {
    let q = query.trim().trim_start_matches('/');
    !q.is_empty() && q.len() <= GENERIC_QUERY_MAX_LEN && is_simple_lower_query(q)
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

    #[test]
    fn generic_broad_query_is_pure_text_classification() {
        assert!(is_generic_broad_query("user"));
        assert!(is_generic_broad_query("handler"));
        assert!(!is_generic_broad_query("parsefilesystem"));
        assert!(!is_generic_broad_query("CamelCase"));
        assert!(!is_generic_broad_query(""));
    }

    /// A broad query orders a low-signal exact match between the two
    /// high-signal tiers, and the weights are what put it there. It loses to a
    /// name the term OPENS, because that name is what the query was reaching
    /// for; it beats a name the term merely appears inside, because a demotion
    /// wide enough to span a whole tier cancels exactness itself and hands the
    /// answer to an accidental substring.
    #[test]
    fn a_low_signal_exact_match_sits_between_the_high_signal_tiers() {
        let scope = TestScope::new();
        let rank = |name: &str, kind: &str| {
            symbol_rank(
                "user",
                RankedSymbol {
                    name,
                    name_path: None,
                    kind,
                    file: Path::new("src/lib.rs"),
                },
                &scope,
            )
        };

        let exact = rank("user", "variable");
        let prefix = rank("userservice", "class");
        let substring = rank("currentuser", "class");

        assert!(
            prefix > exact,
            "a type the term opens is what the query reached for: {prefix} vs {exact}"
        );
        assert!(
            exact > substring,
            "the declaration spelled exactly as asked outranks a name that merely contains it: \
             {exact} vs {substring}"
        );
    }

    #[test]
    fn noisy_suffix_only_penalizes_test_suffixes_not_error_types() {
        assert_eq!(
            noisy_suffix_penalty("usertest", "user"),
            NOISY_SUFFIX_PENALTY
        );
        assert_eq!(noisy_suffix_penalty("storeerror", "store"), 0);
        assert_eq!(noisy_suffix_penalty("user", "user"), 0);
    }
}
