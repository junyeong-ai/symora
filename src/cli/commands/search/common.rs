use std::collections::HashMap;

use crate::app::App;
use crate::infra::file_filter::FileFilter;
use crate::models::symbol::Language;

/// A query is a path-like pattern when it carries `/`, `*`, or `[` rather
/// than a plain identifier. The callers route these forms specially — a `*`
/// glob resolves against the index, other path-like forms fall through to the
/// LSP workspace-symbol lookup (which the index then supplements).
pub fn looks_like_symbol_path(query: &str) -> bool {
    query.contains('/') || query.contains('*') || query.contains('[')
}

/// Resolve the active language(s) for a search call.
///
/// - explicit `--lang` wins, mapping to a single language (Unknown → empty).
/// - otherwise we rank project languages by file count.
pub fn resolve_search_languages(app: &App, language: Option<&str>) -> Vec<Language> {
    match language.map(Language::parse_or_default) {
        Some(Language::Unknown) => vec![],
        Some(lang) => vec![lang],
        None => detect_languages_by_file_count(app),
    }
}

fn detect_languages_by_file_count(app: &App) -> Vec<Language> {
    let extensions: Vec<&str> = Language::all()
        .into_iter()
        .flat_map(|lang| lang.extensions().iter().copied())
        .collect();
    let filter = FileFilter::new(app.root());
    let files = filter.discover_files(&extensions);
    let mut counts: HashMap<Language, usize> = HashMap::new();

    for file in files {
        let language = Language::from_path(&file);
        if language != Language::Unknown {
            *counts.entry(language).or_insert(0) += 1;
        }
    }

    let mut ranked: Vec<_> = counts.into_iter().collect();
    ranked.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.lsp_id().cmp(b.0.lsp_id())));
    ranked.into_iter().map(|(language, _)| language).collect()
}

/// Used by content ranking to demote markup/config matches when the user
/// hasn't pinned a language explicitly.
pub fn is_code_language(language: Language) -> bool {
    !matches!(
        language,
        Language::Markdown | Language::Toml | Language::Yaml
    )
}
