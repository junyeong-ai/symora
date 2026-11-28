use std::collections::HashMap;
use std::path::Path;

use anyhow::Result;
use clap::Args;
use futures::StreamExt;

use crate::app::App;
use crate::cli::commands::common::parse_symbol_kind;
use crate::cli::response::disclosure::{Cap, CoverageReason, LowerBound, with_cap};
use crate::cli::response::{ReferenceCount, Section, SymbolOutput, SymbolSource};
use crate::cli::utils::shell_arg;
use crate::cli::{FileSymbols, OutputError, resolve_project_file};
use crate::constants::defaults::REFERENCE_COUNT_CONCURRENCY;
use crate::models::lsp::FindSymbolsOptions;
use crate::models::symbol::{Symbol, SymbolFilter, SymbolKind};

#[derive(Args, Debug)]
#[command(
    after_long_help = "Use `symbols <file>` to inspect one file semantically.\nAdd --symbol <path> to narrow it to a name, Class/method, or */method pattern.\nUse `search symbols` to find a declaration whose file you do not know; its --body and --signature read the source the same way.\n"
)]
pub struct SymbolsArgs {
    /// File path
    pub file: String,

    /// Filter by symbol path (e.g., "Class/method", "*/update")
    #[arg(short, long)]
    pub symbol: Option<String>,

    /// Include symbol body
    #[arg(short, long, conflicts_with = "signature")]
    pub body: bool,

    /// Include only signature
    #[arg(long, conflicts_with = "body")]
    pub signature: bool,

    /// Include nested symbols up to depth
    #[arg(short, long, default_value = "0")]
    pub depth: u32,

    /// Filter by symbol kind(s), comma-separated
    #[arg(long, short = 'k')]
    pub kind: Option<String>,

    /// Exclude symbol kind(s), comma-separated
    #[arg(long)]
    pub exclude: Option<String>,

    /// Use substring matching
    #[arg(long)]
    pub substring: bool,

    /// Exclude low-level symbols (variables, constants)
    #[arg(long)]
    pub structural: bool,

    /// Count each listed declaration's references, other than a function's
    /// parameters and locals; --depth 1 lists a type's members
    #[arg(long)]
    pub refs: bool,

    /// Maximum results
    #[arg(long)]
    pub limit: Option<usize>,
}

pub async fn execute(args: SymbolsArgs, app: &App) -> Result<()> {
    let ctx = &app.output;
    let limit = args.limit.unwrap_or(app.config().lsp.symbol_limit);

    let include_kinds = parse_kind_list(&args.kind)?;
    let exclude_kinds = parse_kind_list(&args.exclude)?;

    let abs_path = match resolve_project_file(std::path::Path::new(&args.file), app.root()) {
        Ok(p) => p,
        Err(e) => {
            ctx.print_error(e);
            return Ok(());
        }
    };

    let effective_depth = if args.symbol.is_some() && args.depth == 0 {
        10
    } else {
        args.depth
    };

    let source = SymbolSource::new(args.body, args.signature);
    // This listing again, as the size fitter names it for a row whose nested
    // declarations it cuts: the same depth, filters and source.
    let rerun = [
        Some(format!(" --depth={effective_depth}")),
        args.kind
            .as_deref()
            .map(|kind| format!(" --kind={}", shell_arg(kind))),
        args.exclude
            .as_deref()
            .map(|kind| format!(" --exclude={}", shell_arg(kind))),
        args.structural.then(|| " --structural".to_string()),
        args.refs.then(|| " --refs".to_string()),
        args.body.then(|| " --body".to_string()),
        args.signature.then(|| " --signature".to_string()),
    ]
    .into_iter()
    .flatten()
    .collect::<String>();
    let file = shell_arg(&ctx.relative_path(&abs_path));
    let options = FindSymbolsOptions::default().with_depth(effective_depth);
    let options = match source {
        SymbolSource::Location => options,
        SymbolSource::Signature | SymbolSource::Body => options.with_body(),
    };

    let filter = SymbolFilter {
        pattern: args.symbol.as_deref(),
        substring: args.substring,
        include_kinds: include_kinds.as_deref(),
        exclude_kinds: exclude_kinds.as_deref(),
        structural: args.structural,
    };
    // A path names declarations whichever reading holds them; a listing
    // without one lists the tree the server gives, in its own kinds.
    let answer = match args.symbol {
        Some(_) => {
            crate::cli::declared_addressing(app, &abs_path, options, |symbols| {
                !filter.matches(symbols).is_empty()
            })
            .await
        }
        None => crate::cli::declared_in(app, &abs_path, options).await,
    };
    match answer {
        Ok(answer) => {
            let declarations = filter.outermost(answer.symbols.clone());

            let total = declarations.len();
            let listed = &declarations[..total.min(limit)];
            let counts = match args.refs {
                true => Some(count_references(app, &abs_path, &answer, listed).await),
                false => None,
            };
            // Every row is of this one file, so of this one member.
            let member = counts
                .as_ref()
                .and_then(|_| crate::cli::python_members::qualifier(app, &abs_path));
            let within = member.as_ref().map(|q| q.gap.reason);
            let items: Vec<SymbolOutput> = listed
                .iter()
                .map(|s| match source {
                    SymbolSource::Location => SymbolOutput::from_symbol(s, ctx.root()),
                    SymbolSource::Signature => SymbolOutput::from_symbol_signature(s, ctx.root()),
                    SymbolSource::Body => {
                        SymbolOutput::from_symbol(s, ctx.root()).with_signature(s.signature.clone())
                    }
                })
                .map(|item| match &counts {
                    Some(counts) => with_reference_counts(item, counts, within),
                    None => item,
                })
                .collect();

            let mut hints: Vec<String> = member.into_iter().map(|q| q.hint).collect();
            hints.extend(unreferenced_hint(&items));
            if counts
                .iter()
                .flat_map(HashMap::values)
                .any(|count| matches!(count, ReferenceCount::Missing(CoverageReason::TimedOut)))
            {
                hints.push(
                    LowerBound::AnalysisTimedOut {
                        secs: app.config().lsp.timeout_secs,
                        narrowing: None,
                    }
                    .hint(),
                );
            }
            let body_paths = SymbolOutput::body_paths(&items);
            let relist = children_relist(&items, &rerun, &file);
            ctx.print_source(
                FileSymbolsOutput {
                    backend: answer.backend,
                    symbols: with_cap(
                        Section::with_total(items, total).with_hints(hints),
                        Cap::Option("--limit"),
                    ),
                },
                &body_paths,
                relist.as_deref(),
            );
        }
        Err(e) => ctx.print_error(e),
    }

    Ok(())
}

/// Each counted declaration, keyed by where its name is: two declarations of
/// one file never share that position.
type ReferenceCounts = HashMap<(u32, u32), ReferenceCount>;

/// The declarations a listing counts references for: every listed one, and
/// beneath one that holds members, those members. What anything else holds —
/// a function's local function or type — is internal to it and is not counted.
fn countable<'a>(symbols: &'a [Symbol], out: &mut Vec<&'a Symbol>) {
    for symbol in symbols {
        out.push(symbol);
        if symbol.kind.holds_members() {
            countable(&symbol.children, out);
        }
    }
}

/// Each listed declaration's references, asked where the language server
/// reads that declaration ([`FileSymbols::served_as`]): a count taken at a
/// position it reads as another declaration would be that declaration's.
async fn count_references(
    app: &App,
    file: &Path,
    answer: &FileSymbols,
    listed: &[Symbol],
) -> ReferenceCounts {
    let mut targets = Vec::new();
    countable(listed, &mut targets);
    futures::stream::iter(targets.into_iter().cloned())
        .map(|symbol: Symbol| async move {
            let key = (symbol.location.line, symbol.location.column);
            let outcome = answer
                .references_of(app.lsp.as_ref(), file, &symbol, app.root())
                .await;
            (key, ReferenceCount::of(&outcome, app.test_scope()))
        })
        .buffered(REFERENCE_COUNT_CONCURRENCY)
        .collect()
        .await
}

/// `within` is the reason the file's workspace member qualifies a count
/// taken there ([`ReferenceCount::status_within`]).
fn with_reference_counts(
    mut item: SymbolOutput,
    counts: &ReferenceCounts,
    within: Option<CoverageReason>,
) -> SymbolOutput {
    if let Some(count) = counts.get(&(item.location.line, item.location.column)) {
        if let ReferenceCount::Measured {
            total, test, prod, ..
        } = count
        {
            item.refs = Some(*total);
            item.test_refs = Some(*test);
            item.prod_refs = Some(*prod);
        }
        item.refs_status = count.status_within(within);
    }
    item.children = item
        .children
        .map(|children| children.map_items(|child| with_reference_counts(child, counts, within)));
    item
}

/// What a listed zero leaves unsaid. It is a list an agent deletes from, and a
/// declaration can be reached without a reference naming it.
fn unreferenced_hint(items: &[SymbolOutput]) -> Option<String> {
    fn zeros(items: &[SymbolOutput]) -> usize {
        items
            .iter()
            .map(|item| {
                usize::from(item.refs == Some(0))
                    + item
                        .children
                        .as_ref()
                        .map_or(0, |children| zeros(&children.items))
            })
            .sum()
    }
    let unreferenced = zeros(items);
    (unreferenced > 0).then(|| {
        format!(
            "{unreferenced} declaration(s) have refs 0. A call through a type, interface or \
             trait, a registration, reflection and serialization reach a declaration without \
             naming it, so a zero is not evidence that nothing does"
        )
    })
}

/// How a reader lists a row's nested declarations whole where the size
/// target cut them: this listing again, confined to the row's `name_path`,
/// with `rerun`'s depth, filters and source. Absent where no row nests any.
fn children_relist(items: &[SymbolOutput], rerun: &str, file: &str) -> Option<String> {
    items.iter().any(|item| item.children.is_some()).then(|| {
        format!(
            "list a row's cut children whole, for its name_path, with symora symbols \
             --symbol=\"<symbol>\"{rerun} -- {file}"
        )
    })
}

/// A file's symbol list beside what produced it: an AST answer is flat and
/// carries no server-assigned kinds, so a caller reads `backend` before it
/// reads absence as structure.
#[derive(serde::Serialize)]
struct FileSymbolsOutput {
    backend: crate::cli::SymbolBackend,
    #[serde(flatten)]
    symbols: Section<SymbolOutput>,
}

fn parse_kind_list(kinds: &Option<String>) -> Result<Option<Vec<SymbolKind>>, OutputError> {
    let Some(kinds) = kinds else {
        return Ok(None);
    };
    let kinds = kinds
        .split(',')
        .map(str::trim)
        .filter(|kind| !kind.is_empty())
        .map(parse_symbol_kind)
        .collect::<Result<Vec<_>, _>>()?;
    Ok((!kinds.is_empty()).then_some(kinds))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cli::response::LocationOutput;

    fn row(name_path: Option<&str>, children: Option<Vec<SymbolOutput>>) -> SymbolOutput {
        SymbolOutput {
            name: name_path.unwrap_or("anonymous").to_string(),
            name_path: name_path.map(str::to_string),
            kind: "class".to_string(),
            location: LocationOutput {
                file: "src/shapes.py".to_string(),
                line: 1,
                column: 7,
                end_line: None,
                end_column: None,
                snippet: None,
                degraded_column: None,
            },
            container: None,
            refs: None,
            test_refs: None,
            prod_refs: None,
            refs_status: None,
            signature: None,
            documentation: None,
            body: None,
            body_omitted: None,
            children: children.map(Section::new),
        }
    }

    /// A row's cut children are listed whole by this listing confined to the
    /// row's path; a listing whose rows nest nothing has no such list.
    #[test]
    fn a_rows_cut_children_are_listed_whole_by_the_listing_at_its_path() {
        let nesting = [row(
            Some("Shape"),
            Some(vec![row(Some("Shape/area"), None)]),
        )];
        let flat = [row(Some("free"), None)];

        let relist =
            children_relist(&nesting, " --depth=2", "'src/shapes.py'").expect("a row nests");

        assert!(
            relist.ends_with(
                "with symora symbols --symbol=\"<symbol>\" --depth=2 -- 'src/shapes.py'"
            ),
            "{relist}"
        );
        crate::cli::assert_named_commands_parse(&relist);
        assert_eq!(
            children_relist(&flat, " --depth=2", "'src/shapes.py'"),
            None
        );
    }
}
