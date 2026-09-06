use anyhow::Result;
use serde::Serialize;

use crate::app::App;
use crate::cli::OutputError;
use crate::cli::response::disclosure::relative_unread_paths;
use crate::models::symbol::Language;
use crate::services::store::IndexOptions;
use crate::services::store::UnreadPath;

use super::IndexCommand;

#[derive(Serialize)]
struct IndexStatusOutput {
    file_count: usize,
    symbol_count: usize,
    content_line_count: usize,
    /// The bytes the index occupies on disk. A property of the store rather
    /// than of any one build, so it is read here and not reported by the
    /// build that happened to write it — a size named beside a build reads
    /// as that build's cost, when what it measures keeps moving afterwards.
    index_size_bytes: u64,
    /// When the build behind `languages` published, in Unix seconds, and 0
    /// when no completed build stands. Read from that build's own record, so
    /// a date here always dates the coverage listed beside it — row counts
    /// move with any build, finished or not, and cannot date one.
    last_indexed: u64,
    is_indexing: bool,
    /// Paths the last completed build could not read — files it could not
    /// open, directories it could not enter. Present only when there were
    /// any: the index does not cover them even though `languages` names
    /// their language, so a zero from one of those languages is a lower
    /// bound. Named rather than counted, because the repair is per path —
    /// fix their permissions and rebuild, or refresh one of them on its own.
    #[serde(skip_serializing_if = "<[String]>::is_empty")]
    unread_paths: Vec<String>,
    /// The languages this index answers authoritatively for — empty until a
    /// build completes. Row counts alone cannot tell a whole index from one
    /// a narrowed build or a per-file refresh left partial, and a symbol
    /// search reads as complete only for the languages listed here.
    languages: Vec<String>,
    /// The build to run when no completed one stands behind these counts.
    /// A never-built index is the one state every search reads as absence,
    /// and it is the only state the remedy is unconditional in.
    #[serde(skip_serializing_if = "<[String]>::is_empty")]
    next_commands: Vec<String>,
}

#[derive(Serialize)]
struct IndexBuildOutput {
    status: String,
    file_count: usize,
    symbol_count: usize,
    content_line_count: usize,
    /// Paths the build could not read — files it could not open, and
    /// directories it could not enter — so the index does not cover them
    /// even though its scope names their language. A build that hit any of
    /// these also leaves deleted files' rows in place, because it can no
    /// longer tell "gone" from "not seen". Omitted when there were none.
    #[serde(skip_serializing_if = "<[String]>::is_empty")]
    unread_paths: Vec<String>,
}

/// The unread paths as a response says them: relative to the project root, and
/// as names alone — whether the walk knew one to be a file decides which
/// languages it can be keeping rows from, which is the answer's business and
/// not the reader's.
fn named_unread_paths(ctx: &crate::cli::OutputContext, paths: &[UnreadPath]) -> Vec<String> {
    relative_unread_paths(ctx, paths)
        .into_iter()
        .map(|unread| unread.path)
        .collect()
}

/// The languages a build is scoped to, or `None` for every indexed language.
///
/// A name that resolves to no language is refused rather than dropped: what a
/// build covers is what every later search treats as authoritative, so a typo
/// silently narrowing the scope to nothing would leave an index that answers
/// confident zeroes for the code it holds. The same refusal every other
/// `--lang` gives.
fn parse_build_languages(languages: Option<&str>) -> Result<Option<Vec<Language>>, OutputError> {
    let Some(spec) = languages else {
        return Ok(None);
    };
    let mut parsed = Vec::new();
    for name in spec.split(',').map(str::trim) {
        match Language::parse_or_default(name) {
            Language::Unknown => {
                return Err(OutputError::invalid(format!("Unknown language: {name}"))
                    .with_hint("Run 'symora doctor' to see supported languages."));
            }
            language => parsed.push(language),
        }
    }
    if parsed.is_empty() {
        return Err(OutputError::invalid("--lang names no language")
            .with_hint("Pass a comma-separated list, or omit --lang to index every language."));
    }
    Ok(Some(parsed))
}

/// The build to prescribe beside a status.
///
/// A never-built index answers every search as absence, and that is the one
/// state whose remedy holds without qualification. A completed build needs no
/// second one, and a build in flight is already the remedy running.
fn status_next_commands(languages: &[Language], is_indexing: bool) -> Vec<String> {
    match languages.is_empty() && !is_indexing {
        true => vec!["symora search index build".to_string()],
        false => Vec::new(),
    }
}

pub async fn execute_index_command(app: &App, command: IndexCommand) -> Result<()> {
    let ctx = &app.output;

    match command {
        IndexCommand::Build { force, languages } => {
            let languages = match parse_build_languages(languages.as_deref()) {
                Ok(languages) => languages,
                Err(e) => {
                    ctx.print_error(e);
                    return Ok(());
                }
            };
            match app.store.index(IndexOptions { force, languages }).await {
                Ok(stats) => ctx.print_success(IndexBuildOutput {
                    status: "completed".to_string(),
                    file_count: stats.file_count,
                    symbol_count: stats.symbol_count,
                    content_line_count: stats.content_line_count,
                    unread_paths: named_unread_paths(ctx, &stats.unread_paths),
                }),
                Err(e) => ctx.print_error(OutputError::from(e)),
            }
        }
        IndexCommand::Status => match app.store.index_status().await {
            Ok(stats) => ctx.print_success(IndexStatusOutput {
                file_count: stats.file_count,
                symbol_count: stats.symbol_count,
                content_line_count: stats.content_line_count,
                index_size_bytes: stats.index_size_bytes,
                last_indexed: stats.last_indexed,
                is_indexing: stats.is_indexing,
                unread_paths: named_unread_paths(ctx, &stats.unread_paths),
                languages: stats
                    .languages
                    .iter()
                    .map(|l| l.lsp_id().to_string())
                    .collect(),
                next_commands: status_next_commands(&stats.languages, stats.is_indexing),
            }),
            Err(e) => ctx.print_error(OutputError::from(e)),
        },
        IndexCommand::Clear => match app.store.index_clear().await {
            Ok(()) => ctx.print_success(serde_json::json!({ "cleared": true })),
            Err(e) => ctx.print_error(OutputError::from(e)),
        },
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_a_never_built_index_prescribes_a_build() {
        assert_eq!(
            status_next_commands(&[], false),
            ["symora search index build"]
        );
        assert!(status_next_commands(&[Language::Rust], false).is_empty());
        assert!(status_next_commands(&[], true).is_empty());
    }

    /// What a build covers is what every later search treats as authoritative,
    /// so a `--lang` that names no language must not quietly become a scope.
    /// Dropping the name would leave a build that indexes files it then
    /// answers for none of — a confident zero over code it holds.
    #[test]
    fn a_lang_that_names_no_language_is_refused_rather_than_dropped() {
        assert_eq!(parse_build_languages(None).unwrap(), None);
        assert_eq!(
            parse_build_languages(Some("rust,python")).unwrap(),
            Some(vec![Language::Rust, Language::Python])
        );

        for spec in ["bogus", "", "rust,bogus", ",", " "] {
            assert!(
                parse_build_languages(Some(spec)).is_err(),
                "'{spec}' names no language and must be refused"
            );
        }
    }
}
