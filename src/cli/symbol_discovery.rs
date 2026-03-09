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

pub fn is_probably_test_path(path: &str) -> bool {
    let lower = path.to_ascii_lowercase();
    lower.contains("/test/")
        || lower.contains("/tests/")
        || lower.ends_with("test.rs")
        || lower.ends_with("tests.rs")
        || lower.ends_with("test.kt")
        || lower.ends_with("tests.kt")
        || lower.ends_with("test.py")
        || lower.ends_with("tests.py")
        || lower.ends_with("test.js")
        || lower.ends_with("spec.js")
        || lower.ends_with("test.ts")
        || lower.ends_with("spec.ts")
        || lower.ends_with("test.tsx")
        || lower.ends_with("spec.tsx")
}

pub fn noisy_suffix_penalty(name: &str, query: &str) -> i32 {
    if name == query || !name.starts_with(query) {
        return 0;
    }

    let suffixes = ["test", "tests", "spec", "exception", "error", "errors"];
    if suffixes.iter().any(|suffix| name.ends_with(suffix)) {
        6
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
    if q.len() < 4 || !is_simple_lower_query(&q) {
        return 0;
    }

    let lower_name = name.to_ascii_lowercase();
    if low_signal_kind && lower_name == q {
        return 24;
    }
    if kind == "enum_member" && lower_name == q {
        return 18;
    }
    0
}

pub fn broad_symbol_kind_bonus(query: &str, name: &str, kind: &str, low_signal_kind: bool) -> i32 {
    let q = query.trim().trim_start_matches('/').to_ascii_lowercase();
    if q.len() < 4 || !is_simple_lower_query(&q) || low_signal_kind {
        return 0;
    }

    let lower_name = name.to_ascii_lowercase();
    let is_high_signal_kind = matches!(
        kind,
        "class" | "struct" | "interface" | "enum" | "function" | "method" | "constructor"
    );

    if is_high_signal_kind && lower_name.contains(&q) && lower_name != q {
        8
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
    let filter = crate::infra::file_filter::FileFilter::with_gitignore(root);
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

fn is_generic_broad_query(query: &str) -> bool {
    let q = query.trim().trim_start_matches('/');
    !q.is_empty() && q.len() <= 8 && is_simple_lower_query(q)
}
