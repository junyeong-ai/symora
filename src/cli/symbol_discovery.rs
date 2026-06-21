use crate::error::LspError;

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
/// equals a short generic query (a local `user` for query `user`) is almost
/// never the navigation target. Demoted past a high-signal prefix match
/// (`symbol_match_priority` 24 plus `BROAD_QUERY_HIGH_SIGNAL_BONUS`) so the
/// function or type carrying the term wins.
pub const GENERIC_LOW_SIGNAL_EXACT_PENALTY: i32 = 24;

/// An enum member exactly matching a short generic query is low-signal, but is
/// at least a named, addressable declaration — so it is demoted less than a
/// bare variable (`GENERIC_LOW_SIGNAL_EXACT_PENALTY`).
pub const GENERIC_ENUM_MEMBER_EXACT_PENALTY: i32 = 18;

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
    if truncated {
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

/// Why a language is missing from the result, as a stable marker an agent
/// can branch on (install a server, retry a timeout, or narrow with --lang).
/// A capability gap — `is_unsupported` covers both the static table and a
/// runtime JSON-RPC method-not-found — classifies as `unsupported`, matching
/// the central error classifier.
pub fn coverage_reason(err: &LspError) -> &'static str {
    match err {
        LspError::ServerNotInstalled { .. } => "server_not_installed",
        LspError::Timeout(_) => "timed_out",
        LspError::UnsupportedLanguage(_) => "unsupported",
        e if e.is_unsupported() => "unsupported",
        _ => "unavailable",
    }
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

pub fn detect_languages_by_file_count(
    root: &std::path::Path,
    all_languages: &[crate::models::symbol::Language],
) -> Vec<crate::models::symbol::Language> {
    use std::collections::HashMap;

    let extensions: Vec<&str> = all_languages
        .iter()
        .flat_map(|lang| lang.extensions().iter().copied())
        .collect();
    let filter = crate::infra::file_filter::FileFilter::new(root);
    let files = filter.discover_files(&extensions);
    let mut counts: HashMap<crate::models::symbol::Language, usize> = HashMap::new();

    for file in files {
        let language = crate::models::symbol::Language::from_path(&file);
        if language != crate::models::symbol::Language::Unknown {
            *counts.entry(language).or_insert(0) += 1;
        }
    }

    let mut ranked: Vec<_> = counts.into_iter().collect();
    ranked.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.lsp_id().cmp(b.0.lsp_id())));
    ranked.into_iter().map(|(language, _)| language).collect()
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

    #[test]
    fn ranking_weights_keep_high_signal_above_low_signal_exact_for_broad_query() {
        // A broad query: a high-signal kind containing the term must outrank a
        // low-signal exact match of the same query. This pins the relative
        // magnitudes of the bonus and the penalty, not their absolute values.
        let high = symbol_match_priority("user", "userservice", "userservice")
            + broad_symbol_kind_bonus("user", "userservice", "class", false);
        let low = symbol_match_priority("user", "user", "user")
            - generic_exact_identifier_penalty("user", "user", "variable", true);
        assert!(
            high > low,
            "high-signal {high} should beat low-signal {low}"
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
