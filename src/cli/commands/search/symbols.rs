use std::collections::HashSet;
use std::path::PathBuf;

use anyhow::Result;
use serde::{Deserialize, Serialize};

use crate::app::App;
use crate::cli::OutputError;
use crate::cli::symbol_discovery::{
    broad_symbol_kind_bonus, generic_exact_identifier_penalty, is_probably_test_path,
    noisy_suffix_penalty, symbol_lookup_hints, symbol_match_priority,
};
#[cfg(unix)]
use crate::daemon::DaemonClient;
use crate::models::lsp::FindSymbolsOptions;
use crate::models::symbol::Language;

use super::common::{looks_like_symbol_path, resolve_search_languages};

#[derive(Serialize, Deserialize)]
pub(super) struct SymbolSearchOutput {
    pub count: usize,
    #[serde(default)]
    pub showing: usize,
    #[serde(alias = "results")]
    pub items: Vec<SymbolResultOutput>,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub truncated: bool,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub hints: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub next_commands: Vec<String>,
}

#[derive(Clone, Serialize, Deserialize)]
pub(super) struct SymbolResultOutput {
    pub name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name_path: Option<String>,
    pub kind: String,
    pub file: String,
    pub line: u32,
    pub column: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub container: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub backend: Option<String>,
    pub score: f64,
}

pub async fn execute_symbol_search(
    app: &App,
    query: &str,
    language: Option<&str>,
    kind: Option<&str>,
    semantic: bool,
    limit: usize,
) -> Result<()> {
    let ctx = &app.output;

    let query = query.trim();
    if query.is_empty() {
        ctx.print_error(OutputError::invalid("Search query cannot be empty"));
        return Ok(());
    }

    let search_languages = resolve_search_languages(app, language);
    let use_semantic = semantic || looks_like_symbol_path(query);

    if use_semantic {
        return execute_semantic_symbol_search(app, query, kind, limit, &search_languages).await;
    }

    #[cfg(unix)]
    {
        let client = DaemonClient::new(app.root());
        match client.search_symbols(query, Some(limit), kind).await {
            Ok(response) => {
                let mut parsed: SymbolSearchOutput = serde_json::from_value(response)
                    .map_err(|e| anyhow::anyhow!("Invalid daemon response: {}", e))?;

                for r in &mut parsed.items {
                    r.file = ctx.relative_path(&PathBuf::from(&r.file));
                    r.backend = Some("index".to_string());
                }

                if !search_languages.is_empty() && parsed.items.len() < limit {
                    let semantic_results =
                        collect_semantic_symbol_results(app, query, kind, limit, &search_languages)
                            .await;
                    parsed.items =
                        merge_symbol_results(parsed.items, semantic_results, limit, query);
                }

                parsed.showing = parsed.items.len();
                if parsed.count < parsed.showing {
                    parsed.count = parsed.showing;
                }

                parsed.truncated = parsed.items.len() > 1 && parsed.items.len() >= limit;
                parsed.hints = symbol_search_hints(
                    query,
                    language,
                    kind,
                    parsed.truncated,
                    parsed.items.len(),
                );
                parsed.next_commands = symbol_search_next_commands(&parsed.items, query, language);

                ctx.print_success(parsed);
            }
            Err(e) => {
                if should_fallback_to_semantic(&e.to_string(), &search_languages) {
                    return execute_semantic_symbol_search(
                        app,
                        query,
                        kind,
                        limit,
                        &search_languages,
                    )
                    .await;
                }
                ctx.print_error(e);
            }
        }
    }

    #[cfg(not(unix))]
    {
        let _ = (kind, limit, language);
        ctx.print_error(OutputError::unsupported(
            "Search requires daemon mode (Unix only)",
        ));
    }

    Ok(())
}

async fn execute_semantic_symbol_search(
    app: &App,
    query: &str,
    kind: Option<&str>,
    limit: usize,
    languages: &[Language],
) -> Result<()> {
    let ctx = &app.output;
    if languages.is_empty() {
        ctx.print_error(OutputError::not_found(
            "No project languages detected for semantic symbol search",
        ));
        return Ok(());
    }

    let mut results = collect_semantic_symbol_results(app, query, kind, limit, languages).await;

    if looks_like_symbol_path(query) && results.len() < limit {
        let expanded =
            collect_document_path_results(app, query, kind, limit, languages, &results).await;
        results = merge_symbol_results(results, expanded, limit, query);
    }

    #[cfg(unix)]
    if looks_like_symbol_path(query) {
        let client = DaemonClient::new(app.root());
        if let Ok(response) = client.search_symbols(query, Some(limit), kind).await
            && let Ok(mut parsed) = serde_json::from_value::<SymbolSearchOutput>(response)
        {
            for result in &mut parsed.items {
                result.file = ctx.relative_path(&PathBuf::from(&result.file));
                result.backend = Some("index".to_string());
            }
            results = merge_symbol_results(results, parsed.items, limit, query);
        }
    }

    let response = SymbolSearchOutput {
        count: results.len(),
        showing: results.len(),
        items: results.clone(),
        truncated: results.len() > 1 && results.len() >= limit,
        hints: symbol_search_hints(
            query,
            None,
            kind,
            results.len() > 1 && results.len() >= limit,
            results.len(),
        ),
        next_commands: symbol_search_next_commands(&results, query, None),
    };
    ctx.print_success(response);
    Ok(())
}

async fn collect_semantic_symbol_results(
    app: &App,
    query: &str,
    kind: Option<&str>,
    limit: usize,
    languages: &[Language],
) -> Vec<SymbolResultOutput> {
    let ctx = &app.output;
    let parsed_kind = kind.map(crate::models::symbol::SymbolKind::parse_or_default);
    if languages.is_empty() {
        return Vec::new();
    }

    let semantic_query = workspace_query_from_pattern(query);
    let overfetch_limit = if looks_like_symbol_path(query) {
        limit
    } else {
        limit.saturating_mul(4)
    };
    let mut seen = HashSet::new();
    let mut results = Vec::new();

    for language in languages {
        let Ok(mut symbols) = app.lsp.workspace_symbols(&semantic_query, *language).await else {
            continue;
        };

        for symbol in &mut symbols {
            if let Some(path) = synthesized_symbol_path(symbol) {
                symbol.name_path = Some(path);
            }
        }

        let filtered = if looks_like_symbol_path(query) {
            crate::models::symbol::Symbol::filter_advanced(
                &symbols,
                Some(query),
                false,
                parsed_kind.as_ref().map(std::slice::from_ref),
                None,
                false,
            )
        } else {
            symbols
                .into_iter()
                .filter(|symbol| {
                    kind_matches(symbol, parsed_kind.as_ref())
                        && symbol
                            .name
                            .to_ascii_lowercase()
                            .contains(&query.to_ascii_lowercase())
                })
                .collect()
        };

        for symbol in filtered {
            let key = format!(
                "{}:{}:{}:{}",
                symbol.location.file.display(),
                symbol.location.line,
                symbol.location.column,
                symbol.name
            );
            if seen.insert(key) {
                results.push(symbol);
            }
        }

        if results.len() >= overfetch_limit.saturating_mul(2) {
            break;
        }
    }

    results.sort_by(|a, b| {
        score_semantic_symbol(query, b)
            .partial_cmp(&score_semantic_symbol(query, a))
            .unwrap_or(std::cmp::Ordering::Equal)
            .then_with(|| a.name.cmp(&b.name))
    });

    let mut outputs: Vec<_> = results
        .into_iter()
        .take(overfetch_limit)
        .map(|symbol| {
            let score = score_semantic_symbol(query, &symbol);
            SymbolResultOutput {
                name: symbol.name,
                name_path: symbol.name_path,
                kind: symbol.kind.to_string(),
                file: ctx.relative_path(&symbol.location.file),
                line: symbol.location.line,
                column: symbol.location.column,
                container: symbol.container,
                backend: Some("semantic".to_string()),
                score,
            }
        })
        .collect();
    sort_symbol_results(&mut outputs, query);
    prune_low_value_symbol_results(&mut outputs, query, limit);
    outputs
}

fn merge_symbol_results(
    primary: Vec<SymbolResultOutput>,
    secondary: Vec<SymbolResultOutput>,
    limit: usize,
    query: &str,
) -> Vec<SymbolResultOutput> {
    let mut seen = HashSet::new();
    let mut merged = Vec::new();

    for result in primary.into_iter().chain(secondary) {
        let symbol_key = result.name_path.as_deref().unwrap_or(&result.name);
        let key = format!("{}:{}:{}", result.file, result.line, symbol_key,);
        if seen.insert(key) {
            merged.push(result);
        }
    }

    sort_symbol_results(&mut merged, query);
    prune_low_value_symbol_results(&mut merged, query, limit);
    merged.truncate(limit);
    merged
}

async fn collect_document_path_results(
    app: &App,
    query: &str,
    kind: Option<&str>,
    limit: usize,
    languages: &[Language],
    seed_results: &[SymbolResultOutput],
) -> Vec<SymbolResultOutput> {
    let ctx = &app.output;
    let parsed_kind = kind.map(crate::models::symbol::SymbolKind::parse_or_default);
    let leaf = workspace_query_from_pattern(query);
    if leaf.is_empty() {
        return Vec::new();
    }

    let mut candidate_files = Vec::new();
    let mut seen_files = HashSet::new();

    for result in seed_results {
        let file = app.root().join(&result.file);
        if seen_files.insert(file.clone()) {
            candidate_files.push(file);
        }
    }

    if candidate_files.len() < limit {
        let semantic_seeds =
            collect_semantic_symbol_results(app, &leaf, kind, limit * 2, languages).await;
        for result in semantic_seeds {
            let file = app.root().join(&result.file);
            if seen_files.insert(file.clone()) {
                candidate_files.push(file);
                if candidate_files.len() >= limit * 2 {
                    break;
                }
            }
        }
    }

    let mut expanded = Vec::new();
    let mut seen_symbols = HashSet::new();
    for file in candidate_files.into_iter().take(limit * 2) {
        let Ok(mut symbols) = app
            .lsp
            .find_symbols(&file, FindSymbolsOptions::default().with_depth(10))
            .await
        else {
            continue;
        };

        crate::models::symbol::Symbol::compute_paths_for_all(&mut symbols);
        let matched = crate::models::symbol::Symbol::filter_advanced(
            &symbols,
            Some(query),
            false,
            parsed_kind.as_ref().map(std::slice::from_ref),
            None,
            false,
        );

        for symbol in matched {
            let file_rel = ctx.relative_path(&symbol.location.file);
            let key = format!(
                "{}:{}:{}:{}",
                file_rel,
                symbol.location.line,
                symbol.location.column,
                symbol.path()
            );
            if seen_symbols.insert(key) {
                expanded.push(SymbolResultOutput {
                    score: score_semantic_symbol(query, &symbol),
                    name: symbol.name,
                    name_path: symbol.name_path,
                    kind: symbol.kind.to_string(),
                    file: file_rel,
                    line: symbol.location.line,
                    column: symbol.location.column,
                    container: symbol.container,
                    backend: Some("document".to_string()),
                });
            }
        }
    }

    sort_symbol_results(&mut expanded, query);
    prune_low_value_symbol_results(&mut expanded, query, limit);
    expanded.truncate(limit);
    expanded
}

fn sort_symbol_results(results: &mut [SymbolResultOutput], query: &str) {
    results.sort_by(|a, b| {
        symbol_result_priority(query, b)
            .cmp(&symbol_result_priority(query, a))
            .then_with(|| {
                b.score
                    .partial_cmp(&a.score)
                    .unwrap_or(std::cmp::Ordering::Equal)
            })
            .then_with(|| a.name.len().cmp(&b.name.len()))
            .then_with(|| a.file.cmp(&b.file))
            .then_with(|| a.line.cmp(&b.line))
    });
}

fn prune_low_value_symbol_results(
    results: &mut Vec<SymbolResultOutput>,
    query: &str,
    limit: usize,
) {
    if looks_like_symbol_path(query) || results.is_empty() {
        return;
    }

    let q = query.trim().trim_start_matches('/').to_ascii_lowercase();
    let high_value_count = results
        .iter()
        .filter(|result| is_high_value_symbol_result(result, &q))
        .count();

    if high_value_count >= usize::min(limit, 3) {
        results.retain(|result| is_high_value_symbol_result(result, &q));
    }
}

fn symbol_result_priority(query: &str, result: &SymbolResultOutput) -> i32 {
    let q = query.trim().trim_start_matches('/').to_ascii_lowercase();
    let name = result.name.to_ascii_lowercase();
    let path = result
        .name_path
        .as_deref()
        .unwrap_or(&result.name)
        .to_ascii_lowercase();
    let match_priority = symbol_match_priority(query, &name, &path);

    let test_penalty = if is_probably_test_path(&result.file) {
        8
    } else {
        0
    };
    let container_penalty = result
        .container
        .as_deref()
        .map(|container| {
            if container.to_ascii_lowercase().contains("test") {
                3
            } else {
                0
            }
        })
        .unwrap_or(0);
    let kind_penalty = if is_low_signal_kind(&result.kind) {
        6
    } else {
        0
    };
    let suffix_penalty = noisy_suffix_penalty(&name, &q);
    let generic_exact_penalty = generic_exact_identifier_penalty(
        query,
        &name,
        &result.kind,
        is_low_signal_kind(&result.kind),
    );
    let kind_bonus =
        broad_symbol_kind_bonus(query, &name, &result.kind, is_low_signal_kind(&result.kind));

    match_priority + kind_bonus
        - test_penalty
        - container_penalty
        - kind_penalty
        - suffix_penalty
        - generic_exact_penalty
}

fn is_high_value_symbol_result(result: &SymbolResultOutput, query: &str) -> bool {
    let name = result.name.to_ascii_lowercase();
    !is_probably_test_path(&result.file)
        && !is_low_signal_kind(&result.kind)
        && noisy_suffix_penalty(&name, query) == 0
}

fn is_low_signal_kind(kind: &str) -> bool {
    matches!(
        kind,
        "variable" | "field" | "property" | "enum_member" | "constant"
    )
}

fn should_fallback_to_semantic(error: &str, languages: &[Language]) -> bool {
    !languages.is_empty() && error.contains("Store not initialized")
}

fn workspace_query_from_pattern(pattern: &str) -> String {
    let trimmed = pattern.trim().trim_start_matches('/');
    let last = trimmed.rsplit('/').next().unwrap_or(trimmed);
    let base = last.split('[').next().unwrap_or(last);
    base.trim_matches('*').to_string()
}

fn synthesized_symbol_path(symbol: &crate::models::symbol::Symbol) -> Option<String> {
    let name = symbol.name.trim();
    if name.is_empty() {
        return None;
    }

    let container = symbol
        .container
        .as_deref()
        .unwrap_or_default()
        .replace("::", "/")
        .replace(['.', '#', '\\'], "/");
    let container = container.trim_matches('/');
    if container.is_empty() {
        Some(name.to_string())
    } else {
        Some(format!("{}/{}", container, name))
    }
}

fn kind_matches(
    symbol: &crate::models::symbol::Symbol,
    kind: Option<&crate::models::symbol::SymbolKind>,
) -> bool {
    kind.is_none_or(|expected| &symbol.kind == expected)
}

fn score_semantic_symbol(query: &str, symbol: &crate::models::symbol::Symbol) -> f64 {
    let q = query.trim().trim_start_matches('/').to_ascii_lowercase();
    let name = symbol.name.to_ascii_lowercase();
    let path = symbol
        .name_path
        .as_deref()
        .unwrap_or(&symbol.name)
        .to_ascii_lowercase();
    let last = q.rsplit('/').next().unwrap_or(&q);

    if looks_like_symbol_path(query) {
        if path == q {
            1.0
        } else if path.ends_with(&format!("/{q}")) {
            0.96
        } else if name == last {
            0.78
        } else if path.contains(last) {
            0.68
        } else {
            0.5
        }
    } else if name == q {
        1.0
    } else if path.ends_with(&format!("/{q}")) {
        0.95
    } else if name.starts_with(&q) {
        0.9
    } else if name.contains(&q) {
        0.75
    } else {
        0.5
    }
}

fn symbol_search_hints(
    query: &str,
    language: Option<&str>,
    kind: Option<&str>,
    truncated: bool,
    result_count: usize,
) -> Vec<String> {
    symbol_lookup_hints(
        query,
        looks_like_symbol_path(query),
        language.is_none(),
        kind.is_none(),
        truncated,
        result_count,
    )
}

fn symbol_search_next_commands(
    results: &[SymbolResultOutput],
    query: &str,
    language: Option<&str>,
) -> Vec<String> {
    if !needs_symbol_follow_up(results) {
        return Vec::new();
    }

    let mut commands = Vec::new();
    if let Some(first) = results.first() {
        let lang_flag = language
            .map(|lang| format!(" --lang {lang}"))
            .unwrap_or_default();
        let symbol_key = first.name_path.as_deref().unwrap_or(&first.name);
        commands.push(format!(
            "symora symbols {} --symbol '{}' --depth 2",
            first.file, symbol_key
        ));
        commands.push(format!("symora map file {} --related-limit 5", first.file));
        if !looks_like_symbol_path(query) {
            commands.push(format!(
                "symora search symbols '{}'{} --kind {}",
                query, lang_flag, first.kind
            ));
        }
    }
    commands.truncate(3);
    commands
}

fn needs_symbol_follow_up(results: &[SymbolResultOutput]) -> bool {
    if results.len() <= 1 {
        return false;
    }

    let first = &results[0];
    let second = &results[1];
    let score_close = (first.score - second.score).abs() < 0.15;
    let different_file = first.file != second.file;
    let different_symbol = first.name_path != second.name_path || first.kind != second.kind;

    (score_close && different_symbol) || different_file
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn symbol_hints_are_empty_for_exact_single_result() {
        let hints = symbol_search_hints("SearchCommand/Content", None, None, false, 1);
        assert!(hints.is_empty());
    }

    #[test]
    fn symbol_search_output_uses_items_field() {
        let output = SymbolSearchOutput {
            count: 3,
            showing: 1,
            items: vec![SymbolResultOutput {
                name: "SearchCommand".to_string(),
                name_path: Some("SearchCommand".to_string()),
                kind: "enum".to_string(),
                file: "src/cli/commands/search/mod.rs".to_string(),
                line: 30,
                column: 1,
                container: None,
                backend: Some("index".to_string()),
                score: 1.0,
            }],
            truncated: true,
            hints: vec!["narrow it".to_string()],
            next_commands: vec![
                "symora symbols src/cli/commands/search/mod.rs --depth 1".to_string(),
            ],
        };

        let value = serde_json::to_value(output).unwrap();
        assert_eq!(value["count"], 3);
        assert_eq!(value["showing"], 1);
        assert!(value.get("items").is_some());
        assert!(value.get("results").is_none());
        assert_eq!(value["truncated"], true);
    }
}
