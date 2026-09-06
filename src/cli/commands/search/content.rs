use anyhow::Result;
use serde::{Deserialize, Serialize};
use tokio::io::AsyncBufReadExt;

use crate::app::App;
use crate::cli::OutputError;
use crate::cli::response::Section;
use crate::infra::file_filter::FileFilter;
use crate::models::symbol::Language;

use crate::cli::response::disclosure::{
    LowerBound, as_paths, index_holes_bound, index_unavailable_disclosure, ordered_bounds,
    relative_paths, relative_stale_files, with_lower_bounds,
};

#[derive(Clone, Serialize, Deserialize)]
pub(super) struct ContentResultOutput {
    pub file: String,
    pub line: u32,
    pub content: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub backend: Option<String>,
    pub score: f64,
}

pub async fn execute_content_search(
    app: &App,
    query: &str,
    language: Option<&str>,
    limit: usize,
) -> Result<()> {
    let ctx = &app.output;

    let query = query.trim();
    if query.is_empty() {
        ctx.print_error(OutputError::invalid("Search query cannot be empty"));
        return Ok(());
    }

    // A zero cap cannot be answered honestly: the index reports the total
    // it saw through the window, so an empty window would publish a zero
    // for a repository full of matches. Ask for one result to learn the
    // count.
    if limit == 0 {
        ctx.print_error(
            OutputError::invalid("--limit must be at least 1")
                .with_hint("Use --limit 1 to learn the count from a single result."),
        );
        return Ok(());
    }

    let language_filter = language.map(Language::parse_or_default);
    if language_filter == Some(Language::Unknown) {
        ctx.print_error(
            OutputError::invalid(format!(
                "Unknown language: {}",
                language.unwrap_or_default()
            ))
            .with_hint("Run 'symora doctor' to see supported languages."),
        );
        return Ok(());
    }
    // The index answers for the languages it holds content for; the rest —
    // and, on a miss, all of them, since text written since the last build is
    // in the working tree and not in the index — are read from the tree
    // itself. A zero is thus always confirmed against disk. A covered-language
    // HIT answers from the index alone: edits made outside symora since the
    // last build surface through `stale` on the files the rows came from, and
    // through the next `index build` — that snapshot semantic is the price of
    // not re-walking the tree on every query.
    let scope = content_scope(language_filter);

    match app.store.search_content(query, limit, &scope).await {
        Ok(page) => {
            // An index MISS is confirmed against the whole scope — text
            // written since the last build lives in the tree, not the
            // index. A hit narrows the live read to the languages the
            // page's own snapshot says the index did not cover.
            //
            // The same fork settles whose holes bound the answer. A miss
            // rescans everything from the tree and reports that scan's own
            // shortfall; inheriting the build's on top would recommend a
            // rebuild for paths this run just read successfully.
            let (live, answered_from_index): (Vec<Language>, Vec<Language>) = if page.total == 0 {
                (scope, Vec::new())
            } else {
                (
                    scope
                        .into_iter()
                        .filter(|lang| !page.covered.contains(lang))
                        .collect(),
                    page.covered.clone(),
                )
            };
            let stale_files = relative_stale_files(ctx, &page.stale_files);
            let mut items: Vec<ContentResultOutput> = page
                .rows
                .into_iter()
                .map(|r| ContentResultOutput {
                    file: ctx.relative_path(&r.file),
                    line: r.line,
                    content: r.content,
                    backend: Some("index".to_string()),
                    score: r.score,
                })
                .collect();
            let mut count = page.total;
            let index_bounds = index_holes_bound(ctx, &page.unread_paths, &answered_from_index);
            let mut scan_bound = None;
            if !live.is_empty() {
                let scanned = scan_content(app, query, &live, limit).await;
                count += scanned.total;
                items.extend(scanned.rows);
                let unread = relative_paths(ctx, &as_paths(&scanned.unreadable_paths));
                scan_bound =
                    (!unread.is_empty()).then_some(LowerBound::ScanCouldNotReadPaths(unread));
            }
            // The same order every other surface says them in: what cannot be
            // read now leads what a past build could not.
            let lower_bounds = ordered_bounds(scan_bound, index_bounds);
            let section =
                finish_content_search(items, count, query, language, limit, &lower_bounds, &[]);
            // Narrowed to the rows this answer actually emitted: the index page
            // it came from is a superset of them.
            let stale = section.items.iter().any(|item| {
                item.backend.as_deref() == Some("index") && stale_files.contains(&item.file)
            });
            ctx.print_success(section.with_stale(stale));
        }
        // A daemon that was never reached says nothing about the store, so
        // there is nothing to answer around: it is reported as itself, or a
        // lost daemon goes unnoticed while every search quietly pays for a
        // full walk — and one lost daemon reads differently depending on which
        // command was in flight (INV3).
        Err(e) if !e.describes_the_store() => ctx.print_error(OutputError::from(e)),
        // Nothing to read the index for — never built, a build owns it, or it
        // could not be opened at all. The filesystem scan answers from the
        // tree itself for all three, so the answer is authoritative rather
        // than a lower bound: unlike the symbol surfaces, whose fallback is a
        // language server that speaks for some languages and not others, this
        // one reads the whole scope the query named. What the answer costs
        // nothing, the INDEX still does — an unbuilt one is the ordinary state
        // and says nothing, while one that could not be opened is a fact about
        // the store that only the surfaces reading it can report.
        Err(e) => {
            let scanned = scan_content(app, query, &scope, limit).await;
            ctx.print_success(finish_after_index_failure(
                ctx, scanned, &e, query, language, limit,
            ));
        }
    }

    Ok(())
}

/// Shaping for an answer the tree gave whole while the index was unreadable.
/// The store's failure is derived from the error here rather than handed in
/// beside the rows, so scanning around a broken index and reporting one are
/// the same act and cannot come apart.
fn finish_after_index_failure(
    ctx: &crate::cli::OutputContext,
    scanned: ScannedContent,
    error: &crate::error::StoreError,
    query: &str,
    language: Option<&str>,
    limit: usize,
) -> Section<ContentResultOutput> {
    let unread = relative_paths(ctx, &as_paths(&scanned.unreadable_paths));
    let lower_bounds: Vec<LowerBound> = (!unread.is_empty())
        .then_some(LowerBound::ScanCouldNotReadPaths(unread))
        .into_iter()
        .collect();
    finish_content_search(
        scanned.rows,
        scanned.total,
        query,
        language,
        limit,
        &lower_bounds,
        &Vec::from_iter(index_unavailable_disclosure(error)),
    )
}

/// Final shaping shared by the index and scan paths: rank the merged rows,
/// cap emission at `limit`, and derive `truncated`/hints from the
/// match count — never from limit saturation. `lower_bounds` is a parameter
/// because no route may publish a count without stating whether it is the
/// whole one; each reason it is not leads the hints, ahead of the advice for
/// narrowing a result set that may not be complete. `route_facts` are what the
/// route itself has to admit — an index it could not read — each with the
/// command that acts on it; they sit between, since the answer is whole and
/// they qualify the index rather than the count.
fn finish_content_search(
    candidates: Vec<ContentResultOutput>,
    count: usize,
    query: &str,
    language: Option<&str>,
    limit: usize,
    lower_bounds: &[LowerBound],
    route_facts: &[(String, String)],
) -> Section<ContentResultOutput> {
    let count = count.max(candidates.len());
    let items = rank_content_results(candidates, limit);

    let truncated = items.len() < count;
    let mut hints: Vec<String> = route_facts.iter().map(|(fact, _)| fact.clone()).collect();
    let mut next_commands: Vec<String> = route_facts
        .iter()
        .map(|(_, remedy)| remedy.clone())
        .collect();
    hints.extend(content_search_hints(
        query,
        language,
        count,
        truncated,
        items.len(),
        limit,
    ));
    next_commands.extend(content_search_next_commands(&items, language));
    with_lower_bounds(
        Section::with_total(items, count)
            .with_hints(hints)
            .with_next_commands(next_commands),
        lower_bounds,
    )
}

/// Content search over the working tree itself, for the given languages —
/// the ground truth the index is a cache of. Every discovered file that
/// reads as text is searched and every match is counted; only the rows that
/// can still survive the cap are kept, so a one-character query over a large
/// tree costs a bounded amount of memory rather than one line per match.
///
/// What the walk could not enter, could not open, or could not finish reading
/// is counted rather than skipped, because those are the paths that decide
/// whether the count is the whole truth: a scan is the authoritative answer
/// when no index exists, and an authority that quietly drops what it could
/// not read publishes a confident zero over a file it never saw.
async fn scan_content(
    app: &App,
    query: &str,
    languages: &[Language],
    limit: usize,
) -> ScannedContent {
    let extensions: Vec<&str> = languages
        .iter()
        .flat_map(|lang| lang.extensions().iter().copied())
        .collect();
    if extensions.is_empty() {
        return ScannedContent::default();
    }
    let filter = FileFilter::new(app.root());
    let discovery = filter.discover_files(&extensions);
    let mut files = discovery.files;
    files.sort();

    let q = query.to_ascii_lowercase();
    let mut scanned = ScannedContent {
        unreadable_paths: discovery.unreadable.clone(),
        ..ScannedContent::default()
    };
    for file in files {
        let handle = match tokio::fs::File::open(&file).await {
            Ok(handle) => handle,
            Err(e) => {
                if crate::infra::hides_content(&e) {
                    scanned.unreadable_paths.push(file.clone());
                }
                continue;
            }
        };
        // A file is searched whole or not at all — the same rule the indexer
        // applies when it reads one — so a read that fails midway never
        // leaves a half-searched file reported as searched.
        let mut lines = tokio::io::BufReader::new(handle).lines();
        let mut matches = Vec::new();
        let mut file_total = 0usize;
        let mut line_number = 0u32;
        let read_whole = loop {
            match lines.next_line().await {
                Ok(Some(line)) => {
                    // A NUL byte marks the file as binary — outside the
                    // search domain, exactly as the indexer treats it, and
                    // so not a path the scan failed to read. Invalid UTF-8
                    // reaches the same verdict through the read error below.
                    if line.contains('\0') {
                        break Read::NotText;
                    }
                    line_number += 1;
                    let score = score_content_line(&q, &line);
                    if score > 0.0 {
                        file_total += 1;
                        matches.push(ContentResultOutput {
                            file: app.output.relative_path(&file),
                            line: line_number,
                            content: line,
                            backend: Some("scan".to_string()),
                            score,
                        });
                        // One file can hold more matches than memory: keep
                        // the rows that can still make the cap, count the
                        // rest. The comparator is the one the merge and the
                        // SQL ORDER BY share, so dropping the tail here
                        // cannot drop a row the merge would have kept.
                        if matches.len() > limit.saturating_mul(2) {
                            matches.sort_by(rank);
                            matches.truncate(limit);
                        }
                    }
                }
                Ok(None) => break Read::Whole,
                Err(e) => {
                    break if crate::infra::hides_text(&e) {
                        Read::Failed
                    } else {
                        Read::NotText
                    };
                }
            }
        };
        match read_whole {
            Read::Whole => {
                scanned.total += file_total;
                scanned.rows.extend(matches);
                scanned.rows.sort_by(rank);
                scanned.rows.truncate(limit);
            }
            Read::Failed => scanned.unreadable_paths.push(file.clone()),
            Read::NotText => {}
        }
    }
    scanned
}

/// How far a file was read. A file is searched whole or not at all — the same
/// rule the indexer applies — so the three outcomes are distinct: a file that
/// holds no text is outside the search domain and the answer is complete
/// without it, while one that failed partway is a hole in the answer.
enum Read {
    Whole,
    NotText,
    Failed,
}

/// A scan's answer: every match counted, the rows that can still make the
/// cap, and what the scan could not read — which is what decides whether
/// `total` is the whole count or a lower bound.
#[derive(Default)]
struct ScannedContent {
    rows: Vec<ContentResultOutput>,
    total: usize,
    unreadable_paths: Vec<std::path::PathBuf>,
}

/// Steering for a result set: how to narrow when there is something to
/// narrow, and — for a zero without `--lang` — what was not searched: an
/// unscoped content search covers code files, so documentation and
/// configuration formats reach it only by name.
fn content_search_hints(
    query: &str,
    language: Option<&str>,
    count: usize,
    truncated: bool,
    count_shown: usize,
    limit: usize,
) -> Vec<String> {
    let mut hints = Vec::new();
    if count == 0 {
        if language.is_none() {
            let unsearched: Vec<&str> = Language::all()
                .into_iter()
                .filter(|lang| !lang.is_code())
                .map(|lang| lang.lsp_id())
                .collect();
            hints.push(format!(
                "Only code files were searched; documentation and configuration files ({}) \
                 are searched by naming one with --lang",
                unsearched.join(", ")
            ));
        }
        return hints;
    }
    // Only when the emission cap is what bound the list — see the same rule
    // on the symbol side. A count above the rows can come from a source total
    // larger than the rows in hand, which no `--limit` reaches.
    if truncated && count_shown >= limit {
        hints.push("Narrow results with a longer query phrase or increase --limit".to_string());
    }
    if language.is_none() {
        hints.push("Add --lang to limit content search to one language".to_string());
    }
    if !query.contains(' ') {
        hints.push(
            "Use a more specific multi-token phrase when broad keyword matches are noisy"
                .to_string(),
        );
    }
    hints.truncate(3);
    hints
}

fn content_search_next_commands(
    results: &[ContentResultOutput],
    language: Option<&str>,
) -> Vec<String> {
    if results.len() <= 1 {
        return Vec::new();
    }

    let mut commands = Vec::new();
    if let Some(first) = results.first() {
        commands.push(format!("symora map file {} --related-limit 5", first.file));
        commands.push(format!("symora symbols {} --depth 1", first.file));
        if language.is_none() {
            let lang = Language::from_path(std::path::Path::new(&first.file));
            if lang != Language::Unknown {
                commands.push(format!(
                    "symora search content '{}' --lang {}",
                    first.content.trim(),
                    lang.lsp_id()
                ));
            }
        }
    }
    commands.truncate(3);
    commands
}

fn rank_content_results(
    mut results: Vec<ContentResultOutput>,
    limit: usize,
) -> Vec<ContentResultOutput> {
    results.sort_by(rank);
    results.truncate(limit);
    results
}

/// The languages a content search reads: the one named, or every code
/// language. Documentation and configuration formats are outside an
/// unscoped search's domain rather than ranked below it — which is why the
/// comparator below needs no class key, and why an empty answer names them
/// as reachable by `--lang`.
fn content_scope(language_filter: Option<Language>) -> Vec<Language> {
    match language_filter {
        Some(lang) => vec![lang],
        None => Language::all()
            .into_iter()
            .filter(|lang| lang.is_code())
            .collect(),
    }
}

/// The comparator the SQL pre-limit ORDER BY mirrors (score, line length
/// in characters, path, line) — identical on both sides so the index and
/// scan paths keep the same rows through a cap, and prefix-closed, so
/// bounding either side to `limit` before merging cannot drop a row the
/// merge would have kept. Backend is not a rank signal, since a language is
/// served by exactly one of the two.
fn rank(a: &ContentResultOutput, b: &ContentResultOutput) -> std::cmp::Ordering {
    b.score
        .partial_cmp(&a.score)
        .unwrap_or(std::cmp::Ordering::Equal)
        .then_with(|| a.content.chars().count().cmp(&b.content.chars().count()))
        .then_with(|| a.file.cmp(&b.file))
        .then_with(|| a.line.cmp(&b.line))
}

/// Mirror of the SQL scoring in `build_content_search_query`, term for
/// term: membership is the raw line containing the query (the `LIKE`),
/// and the score is the match's character position within the line
/// trimmed of tabs and spaces — exactly SQL's `TRIM(…, char(9,32))`, not
/// Rust's Unicode `trim` — with a match living only in that trimmed
/// margin scoring as SQL's `INSTR = 0` does: the ELSE rung. `query` is
/// already lowercased by the caller; 0.0 means no match.
fn score_content_line(query: &str, line: &str) -> f64 {
    let lower = line.to_ascii_lowercase();
    if !lower.contains(query) {
        return 0.0;
    }
    let trimmed = lower.trim_matches([' ', '\t']);
    match trimmed.find(query) {
        Some(byte_pos) => match trimmed[..byte_pos].chars().count() {
            0 => 1.0,
            pos if pos < 8 => 0.8,
            pos if pos < 32 => 0.6,
            _ => 0.4,
        },
        None => 0.4,
    }
}

#[cfg(test)]
mod tests {

    /// A scan is the authoritative answer when no index exists, so what it
    /// could not read decides whether its count is the whole truth. The
    /// reason leads the hints — the advice for narrowing a result set is
    /// worth taking only once the reader knows the set may be short.
    #[test]
    fn a_scan_that_could_not_read_a_path_says_so_before_it_says_anything_else() {
        let section = finish_content_search(
            vec![result("src/a.rs", 1)],
            1,
            "zeta",
            None,
            10,
            &[LowerBound::ScanCouldNotReadPaths(vec![
                "src/p0".to_string(),
                "src/p1".to_string(),
                "src/p2".to_string(),
            ])],
            &[],
        );

        assert!(section.incomplete);
        assert!(section.hints[0].contains("3 path(s) could not be read"));
        assert!(section.hints[0].contains("lower bound"));
        assert!(
            section.hints.len() > 1,
            "the search's own advice survives behind the disclosure"
        );

        let exact =
            finish_content_search(vec![result("src/a.rs", 1)], 1, "zeta", None, 10, &[], &[]);
        assert!(!exact.incomplete);
    }

    /// An index that could not be READ is a fact about the store, and only the
    /// surfaces that read it can report one. The answer itself is whole — a
    /// scan reads the scope the query named — so the fact qualifies the index
    /// rather than the count, and sits behind what makes a count short.
    #[test]
    fn an_index_that_could_not_be_read_is_said_even_though_the_scan_answered_whole() {
        let ctx =
            crate::cli::OutputContext::new(std::path::PathBuf::from("/repo"), Default::default());
        let scanned = || ScannedContent {
            rows: vec![result("src/a.rs", 1)],
            total: 1,
            unreadable_paths: Vec::new(),
        };

        let section = finish_after_index_failure(
            &ctx,
            scanned(),
            &crate::error::StoreError::Corrupt("boom".to_string()),
            "zeta",
            None,
            10,
        );
        assert!(
            !section.incomplete,
            "the scan read the whole scope, so the count is exact"
        );
        assert!(section.hints[0].contains("search index could not be read"));
        assert!(section.hints[0].contains("boom"));
        assert_eq!(section.next_commands[0], "symora search index status");

        // An index that was never built is the ordinary state and says nothing.
        let unbuilt = finish_after_index_failure(
            &ctx,
            scanned(),
            &crate::error::StoreError::NotInitialized,
            "zeta",
            None,
            10,
        );
        assert!(
            !unbuilt
                .hints
                .iter()
                .any(|hint| hint.contains("search index could not be read"))
        );
        assert!(
            !unbuilt
                .next_commands
                .iter()
                .any(|command| command == "symora search index status")
        );
    }

    /// The remedy differs by cause, so it travels with the cause: an index
    /// built over unread paths is repaired by rebuilding once they are
    /// readable, while a scan's own holes are not a symora command away.
    #[test]
    fn the_remedy_travels_with_the_reason_the_count_is_short() {
        let indexed = finish_content_search(
            Vec::new(),
            0,
            "zeta",
            None,
            10,
            &[LowerBound::IndexBuiltOverUnreadPaths {
                paths: vec!["src/one".to_string()],
                repairable: true,
            }],
            &[],
        );
        assert_eq!(
            indexed.next_commands[0],
            "symora search index build --force"
        );

        let scanned = finish_content_search(
            Vec::new(),
            0,
            "zeta",
            None,
            10,
            &[LowerBound::ScanCouldNotReadPaths(vec![
                "src/p0".to_string(),
            ])],
            &[],
        );
        assert!(
            !scanned
                .next_commands
                .iter()
                .any(|c| c.contains("index build")),
            "rebuilding does not make an unreadable path readable"
        );
    }
    use super::*;

    fn result(file: &str, line: u32) -> ContentResultOutput {
        ContentResultOutput {
            file: file.to_string(),
            line,
            content: "async fn run() {}".to_string(),
            backend: Some("scan".to_string()),
            score: 1.0,
        }
    }

    /// The scan scorer is the SQL ladder, term for term: membership from
    /// the raw line, position from a line trimmed of exactly tab and space
    /// (`char(9,32)`) — a tab indent scores 1.0, an NBSP is content and
    /// pushes the position, and a match only in the trimmed margin takes
    /// the ELSE rung, as SQL's `INSTR = 0` does.
    #[test]
    fn scan_scoring_mirrors_the_sql_ladder() {
        assert_eq!(score_content_line("probe", "\tprobe()"), 1.0);
        assert_eq!(score_content_line("probe", "  probe()"), 1.0);
        assert_eq!(score_content_line("probe", "\u{a0}probe()"), 0.8);
        assert_eq!(score_content_line("probe", "no match here"), 0.0);
        // The rung boundaries, as SQL's INSTR ladder draws them: character
        // position 0 / 1..8 / 8..32 / 32.. (INSTR 1 / 2..=8 / 9..=32 / else).
        assert_eq!(score_content_line("q", &format!("{}q", "x".repeat(7))), 0.8);
        assert_eq!(score_content_line("q", &format!("{}q", "x".repeat(8))), 0.6);
        assert_eq!(
            score_content_line("q", &format!("{}q", "x".repeat(31))),
            0.6
        );
        assert_eq!(
            score_content_line("q", &format!("{}q", "x".repeat(32))),
            0.4
        );
        // Position counts characters, not bytes — multibyte prefixes align
        // with SQL's character-based INSTR.
        assert_eq!(score_content_line("probe", "한글 probe"), 0.8);
        // A match that only exists in the trimmed margin is still a member
        // (the raw line contains it) and takes the ELSE rung, as SQL's
        // INSTR = 0 does.
        assert_eq!(score_content_line("\tab", "x\tab\t"), 0.8);
        assert_eq!(score_content_line("a\t", "a\t"), 0.4);
    }

    /// Every row the comparator orders comes from this scope, which is what
    /// lets it rank on relevance alone. A non-code language admitted here
    /// would reach the same list with nothing separating it from source.
    #[test]
    fn an_unscoped_content_search_reads_code_files_only() {
        let scope = content_scope(None);
        assert!(!scope.is_empty());
        assert!(
            scope.iter().all(|lang| lang.is_code()),
            "non-code languages in the unscoped domain: {:?}",
            scope.iter().filter(|l| !l.is_code()).collect::<Vec<_>>()
        );
        assert_eq!(
            content_scope(Some(Language::Markdown)),
            vec![Language::Markdown]
        );
    }

    /// The merged comparator is the SQL pre-limit ORDER BY, key for key:
    /// score, then line length in characters, then path, then line — so
    /// the row a cap keeps is the same row whichever backend served it.
    #[test]
    fn merged_ranking_breaks_score_ties_by_length_before_path() {
        let row = |file: &str, content: &str| ContentResultOutput {
            file: file.to_string(),
            line: 1,
            content: content.to_string(),
            backend: Some("scan".to_string()),
            score: 1.0,
        };
        let ranked = rank_content_results(
            vec![
                row("a.rs", "needle plus a long tail"),
                row("z.rs", "needle"),
            ],
            1,
        );
        assert_eq!(ranked.len(), 1);
        assert_eq!(ranked[0].file, "z.rs");
    }

    #[test]
    fn finish_content_search_derives_truncation_from_exact_count() {
        let candidates = vec![result("src/a.rs", 1), result("src/b.rs", 2)];
        let section = finish_content_search(candidates, 10, "run", None, 2, &[], &[]);

        assert_eq!(section.count, 10);
        assert_eq!(section.showing, 2);
        assert!(section.truncated);
    }

    #[test]
    fn finish_content_search_complete_results_are_not_truncated() {
        let candidates = vec![result("src/a.rs", 1)];
        let section = finish_content_search(candidates, 1, "run", None, 10, &[], &[]);

        assert_eq!(section.count, 1);
        assert_eq!(section.showing, 1);
        assert!(!section.truncated);
    }

    /// A zero is not steered toward a narrower query — that would present the
    /// miss as noise rather than as the answer. What it says instead is what
    /// an unscoped search left out: the non-code formats, reachable by name.
    #[test]
    fn an_empty_result_names_what_was_not_searched_instead_of_narrowing() {
        let hints = content_search_hints("run", None, 0, false, 0, 10);
        assert_eq!(hints.len(), 1);
        assert!(hints[0].contains("Only code files were searched"));
        assert!(hints[0].contains("markdown"));
        assert!(content_search_hints("run", Some("markdown"), 0, false, 0, 10).is_empty());
        assert!(!content_search_hints("run", None, 5, false, 5, 10).is_empty());
    }
}
