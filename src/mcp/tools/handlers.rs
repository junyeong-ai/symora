use std::sync::Arc;

use anyhow::Result;
use serde::Deserialize;
use serde_json::Value;

use crate::app::App;
use crate::cli::commands::{
    actions::{ActionsArgs, ActionsCommand, ApplyArgs, ListArgs},
    callees::CalleesArgs,
    callers::CallersArgs,
    context::ContextArgs,
    def::DefArgs,
    diagnostics::DiagnosticsArgs,
    edit::{EditArgs, EditCommand},
    hover::HoverArgs,
    impact::ImpactArgs,
    implementations::ImplementationsArgs,
    map::{MapArgs, MapCommand},
    pack::{PackArgs, PackShape},
    refs::RefsArgs,
    rename::RenameArgs,
    search::{SearchArgs, SearchCommand},
    symbols::SymbolsArgs,
};
use crate::cli::output::{BufferedSink, OutputFormat, OutputOptions};
use crate::constants::defaults;

use super::schema::{EditTargetInput, LocationInput};

/// Captured command output plus whether the command reported a handled
/// failure (`print_error`), so the MCP layer can set `isError` truthfully.
pub struct CapturedOutput {
    pub body: String,
    pub errored: bool,
}

/// Deserialize tool arguments, surfacing shape errors as
/// `invalid_argument` — the agent can fix its arguments; this is not an
/// internal parse failure.
fn parse_args<T: serde::de::DeserializeOwned>(args: Value) -> Result<T> {
    serde_json::from_value(args).map_err(|e| {
        anyhow::Error::new(crate::cli::OutputError::invalid(format!(
            "Invalid tool arguments: {e}"
        )))
    })
}

pub async fn dispatch(name: &str, arguments: Value, app: &App) -> Result<CapturedOutput> {
    match name {
        "get_project_overview" => run_project_overview(arguments, app).await,
        "get_file_overview" => run_file_overview(arguments, app).await,
        "search_symbols" => run_search_symbols(arguments, app).await,
        "search_content" => run_search_content(arguments, app).await,
        "list_file_symbols" => run_list_file_symbols(arguments, app).await,
        "inspect_symbol" => run_inspect_symbol(arguments, app).await,
        "find_definition" => run_find_definition(arguments, app).await,
        "find_references" => run_find_references(arguments, app).await,
        "find_callers" => run_find_callers(arguments, app).await,
        "find_callees" => run_find_callees(arguments, app).await,
        "find_implementations" => run_find_implementations(arguments, app).await,
        "get_hover" => run_hover(arguments, app).await,
        "get_context" => run_get_context(arguments, app).await,
        "get_impact" => run_get_impact(arguments, app).await,
        "get_diagnostics" => run_get_diagnostics(arguments, app).await,
        "build_context_pack" => run_context_pack(arguments, app).await,
        "rename_symbol" => run_rename_symbol(arguments, app).await,
        "list_code_actions" => run_list_code_actions(arguments, app).await,
        "apply_code_action" => run_apply_code_action(arguments, app).await,
        "replace_symbol_body" => run_replace_body(arguments, app).await,
        "insert_before_symbol" => run_insert_before(arguments, app).await,
        "insert_after_symbol" => run_insert_after(arguments, app).await,
        "delete_symbol" => run_delete_symbol(arguments, app).await,
        other => Err(anyhow::Error::new(crate::cli::OutputError::not_found(
            format!("Unknown tool: {other}"),
        ))),
    }
}

async fn capture<F, Fut>(app: &App, run: F) -> Result<CapturedOutput>
where
    F: FnOnce(App) -> Fut,
    Fut: std::future::Future<Output = Result<()>>,
{
    let buf = BufferedSink::new();
    let scoped = app.with_output_sink(
        Arc::new(buf.clone()),
        OutputOptions {
            format: OutputFormat::Compact,
            quiet: false,
            token_estimate: false,
        },
    );
    run(scoped).await?;
    Ok(CapturedOutput {
        body: buf.take().join("\n"),
        errored: buf.errored(),
    })
}

// --- discovery ------------------------------------------------------------

async fn run_project_overview(_args: Value, app: &App) -> Result<CapturedOutput> {
    capture(app, |a| async move {
        crate::cli::commands::map::execute(
            MapArgs {
                command: MapCommand::Summary { limit: 10 },
            },
            &a,
        )
        .await
    })
    .await
}

#[derive(Deserialize)]
struct FileOverviewInput {
    path: String,
    #[serde(default = "default_file_overview_depth")]
    depth: u32,
    #[serde(default = "default_related_limit")]
    related_limit: usize,
}

fn default_file_overview_depth() -> u32 {
    1
}

fn default_related_limit() -> usize {
    8
}

async fn run_file_overview(args: Value, app: &App) -> Result<CapturedOutput> {
    let input: FileOverviewInput = parse_args(args)?;
    capture(app, move |a| async move {
        crate::cli::commands::map::execute(
            MapArgs {
                command: MapCommand::File {
                    path: input.path,
                    depth: input.depth,
                    related_limit: input.related_limit,
                },
            },
            &a,
        )
        .await
    })
    .await
}

#[derive(Deserialize)]
struct SearchSymbolsInput {
    query: String,
    language: Option<String>,
    kind: Option<String>,
    limit: Option<usize>,
}

async fn run_search_symbols(args: Value, app: &App) -> Result<CapturedOutput> {
    let input: SearchSymbolsInput = parse_args(args)?;
    capture(app, move |a| async move {
        crate::cli::commands::search::execute(
            SearchArgs {
                command: SearchCommand::Symbols {
                    query: input.query,
                    language: input.language,
                    kind: input.kind,
                    workspace_symbols: false,
                    limit: input.limit,
                },
            },
            &a,
        )
        .await
    })
    .await
}

#[derive(Deserialize)]
struct SearchContentInput {
    query: String,
    language: Option<String>,
    limit: Option<usize>,
}

async fn run_search_content(args: Value, app: &App) -> Result<CapturedOutput> {
    let input: SearchContentInput = parse_args(args)?;
    capture(app, move |a| async move {
        crate::cli::commands::search::execute(
            SearchArgs {
                command: SearchCommand::Content {
                    query: input.query,
                    language: input.language,
                    limit: input.limit,
                },
            },
            &a,
        )
        .await
    })
    .await
}

#[derive(Deserialize)]
struct ListFileSymbolsInput {
    file: String,
    #[serde(default)]
    depth: u32,
    #[serde(default)]
    body: bool,
    #[serde(default)]
    signature: bool,
}

async fn run_list_file_symbols(args: Value, app: &App) -> Result<CapturedOutput> {
    let input: ListFileSymbolsInput = parse_args(args)?;
    capture(app, move |a| async move {
        crate::cli::commands::symbols::execute(
            SymbolsArgs {
                file: Some(input.file),
                name: None,
                symbol: None,
                lang: None,
                body: input.body,
                signature: input.signature,
                depth: input.depth,
                kind: None,
                exclude: None,
                substring: false,
                structural: false,
                limit: None,
            },
            &a,
        )
        .await
    })
    .await
}

#[derive(Deserialize)]
struct InspectSymbolInput {
    symbol_path: String,
    language: Option<String>,
    #[serde(default)]
    body: bool,
}

async fn run_inspect_symbol(args: Value, app: &App) -> Result<CapturedOutput> {
    let input: InspectSymbolInput = parse_args(args)?;
    capture(app, move |a| async move {
        crate::cli::commands::symbols::execute(
            SymbolsArgs {
                file: None,
                name: None,
                symbol: Some(input.symbol_path),
                lang: input.language,
                body: input.body,
                signature: false,
                depth: 0,
                kind: None,
                exclude: None,
                substring: false,
                structural: false,
                limit: None,
            },
            &a,
        )
        .await
    })
    .await
}

// --- navigation -----------------------------------------------------------

async fn run_find_definition(args: Value, app: &App) -> Result<CapturedOutput> {
    let loc: LocationInput = parse_args(args)?;
    capture(app, move |a| async move {
        crate::cli::commands::def::execute(
            DefArgs {
                loc: loc.into_arg(),
            },
            &a,
        )
        .await
    })
    .await
}

#[derive(Deserialize)]
struct FindReferencesInput {
    #[serde(flatten)]
    loc: LocationInput,
    #[serde(default)]
    snippet: bool,
    limit: Option<usize>,
}

async fn run_find_references(args: Value, app: &App) -> Result<CapturedOutput> {
    let input: FindReferencesInput = parse_args(args)?;
    capture(app, move |a| async move {
        crate::cli::commands::refs::execute(
            RefsArgs {
                loc: input.loc.into_arg(),
                snippet: input.snippet,
                context: None,
                limit: input.limit,
            },
            &a,
        )
        .await
    })
    .await
}

#[derive(Deserialize)]
struct CallHierarchyInput {
    #[serde(flatten)]
    loc: LocationInput,
    limit: Option<usize>,
}

async fn run_find_callers(args: Value, app: &App) -> Result<CapturedOutput> {
    let input: CallHierarchyInput = parse_args(args)?;
    capture(app, move |a| async move {
        crate::cli::commands::callers::execute(
            CallersArgs {
                loc: input.loc.into_arg(),
                limit: input.limit,
                no_fallback: false,
            },
            &a,
        )
        .await
    })
    .await
}

async fn run_find_callees(args: Value, app: &App) -> Result<CapturedOutput> {
    let input: CallHierarchyInput = parse_args(args)?;
    capture(app, move |a| async move {
        crate::cli::commands::callees::execute(
            CalleesArgs {
                loc: input.loc.into_arg(),
                limit: input.limit,
            },
            &a,
        )
        .await
    })
    .await
}

async fn run_find_implementations(args: Value, app: &App) -> Result<CapturedOutput> {
    let input: CallHierarchyInput = parse_args(args)?;
    capture(app, move |a| async move {
        crate::cli::commands::implementations::execute(
            ImplementationsArgs {
                loc: input.loc.into_arg(),
                limit: input.limit,
            },
            &a,
        )
        .await
    })
    .await
}

async fn run_hover(args: Value, app: &App) -> Result<CapturedOutput> {
    let loc: LocationInput = parse_args(args)?;
    capture(app, move |a| async move {
        crate::cli::commands::hover::execute(
            HoverArgs {
                loc: loc.into_arg(),
            },
            &a,
        )
        .await
    })
    .await
}

// --- analysis -------------------------------------------------------------

#[derive(Deserialize)]
struct GetContextInput {
    #[serde(flatten)]
    loc: LocationInput,
    #[serde(default)]
    callers: bool,
    #[serde(default)]
    callees: bool,
    #[serde(default)]
    types: bool,
    #[serde(default)]
    tests: bool,
    #[serde(default = "default_context_all")]
    all: bool,
    #[serde(default)]
    with_bodies: bool,
    #[serde(default = "default_body_tokens")]
    body_tokens: usize,
}

fn default_context_all() -> bool {
    true
}

fn default_body_tokens() -> usize {
    defaults::CONTEXT_BODY_TOKENS
}

async fn run_get_context(args: Value, app: &App) -> Result<CapturedOutput> {
    let input: GetContextInput = parse_args(args)?;
    capture(app, move |a| async move {
        crate::cli::commands::context::execute(
            ContextArgs {
                loc: input.loc.into_arg(),
                all: input.all,
                callers: input.callers,
                callees: input.callees,
                types: input.types,
                tests: input.tests,
                // Pinned off: the unbudgeted target-body flag bloats every
                // call; with_bodies is the token-budgeted route to bodies
                // (it includes the target's).
                body: false,
                with_bodies: input.with_bodies,
                body_tokens: input.body_tokens,
            },
            &a,
        )
        .await
    })
    .await
}

#[derive(Deserialize)]
struct GetImpactInput {
    #[serde(flatten)]
    loc: LocationInput,
    #[serde(default = "default_impact_limit")]
    limit: usize,
    #[serde(default = "default_impact_depth")]
    depth: u32,
}

fn default_impact_limit() -> usize {
    defaults::IMPACT_FILES_LIMIT
}

fn default_impact_depth() -> u32 {
    defaults::IMPACT_DEFAULT_DEPTH
}

async fn run_get_impact(args: Value, app: &App) -> Result<CapturedOutput> {
    let input: GetImpactInput = parse_args(args)?;
    capture(app, move |a| async move {
        crate::cli::commands::impact::execute(
            ImpactArgs {
                loc: input.loc.into_arg(),
                limit: input.limit,
                depth: input.depth,
            },
            &a,
        )
        .await
    })
    .await
}

#[derive(Deserialize)]
struct GetDiagnosticsInput {
    file: String,
    severity: Option<String>,
    source: Option<String>,
}

async fn run_get_diagnostics(args: Value, app: &App) -> Result<CapturedOutput> {
    let input: GetDiagnosticsInput = parse_args(args)?;
    capture(app, move |a| async move {
        crate::cli::commands::diagnostics::execute(
            DiagnosticsArgs {
                file: input.file.into(),
                severity: input
                    .severity
                    .map(|s| s.split(',').map(str::to_string).collect()),
                source: input.source,
                // Pinned off: per-diagnostic definition/type-definition and
                // quickfix probes multiply LSP round-trips; the catalog's
                // navigation and code-action tools cover the follow-up.
                with_context: false,
                with_suggestions: false,
            },
            &a,
        )
        .await
    })
    .await
}

#[derive(Deserialize)]
struct ContextPackInput {
    #[serde(default = "default_pack_tokens")]
    tokens: usize,
    focus: Option<String>,
    #[serde(default = "default_pack_per_file")]
    per_file: usize,
    #[serde(default)]
    shape: PackShape,
}

fn default_pack_tokens() -> usize {
    defaults::PACK_TOKENS
}

fn default_pack_per_file() -> usize {
    defaults::PACK_SYMBOLS_PER_FILE
}

async fn run_context_pack(args: Value, app: &App) -> Result<CapturedOutput> {
    let input: ContextPackInput = parse_args(args)?;
    capture(app, move |a| async move {
        crate::cli::commands::pack::execute(
            PackArgs {
                tokens: input.tokens,
                focus: input.focus,
                per_file: input.per_file,
                shape: input.shape,
            },
            &a,
        )
        .await
    })
    .await
}

// --- mutation -------------------------------------------------------------

#[derive(Deserialize)]
struct RenameSymbolInput {
    #[serde(flatten)]
    loc: LocationInput,
    new_name: String,
    #[serde(default)]
    dry_run: bool,
}

async fn run_rename_symbol(args: Value, app: &App) -> Result<CapturedOutput> {
    let input: RenameSymbolInput = parse_args(args)?;
    capture(app, move |a| async move {
        crate::cli::commands::rename::execute(
            RenameArgs {
                location: input.loc.to_string(),
                new_name: input.new_name,
                dry_run: input.dry_run,
            },
            &a,
        )
        .await
    })
    .await
}

#[derive(Deserialize)]
struct ListCodeActionsInput {
    #[serde(flatten)]
    loc: LocationInput,
    kind: Option<String>,
    #[serde(default)]
    preferred: bool,
}

async fn run_list_code_actions(args: Value, app: &App) -> Result<CapturedOutput> {
    let input: ListCodeActionsInput = parse_args(args)?;
    capture(app, move |a| async move {
        crate::cli::commands::actions::execute(
            ActionsArgs {
                command: ActionsCommand::List(ListArgs {
                    location: input.loc.to_string(),
                    kind: input.kind,
                    preferred: input.preferred,
                }),
            },
            &a,
        )
        .await
    })
    .await
}

#[derive(Deserialize)]
struct ApplyCodeActionInput {
    #[serde(flatten)]
    loc: LocationInput,
    title: String,
    #[serde(default)]
    dry_run: bool,
}

async fn run_apply_code_action(args: Value, app: &App) -> Result<CapturedOutput> {
    let input: ApplyCodeActionInput = parse_args(args)?;
    capture(app, move |a| async move {
        crate::cli::commands::actions::execute(
            ActionsArgs {
                command: ActionsCommand::Apply(ApplyArgs {
                    location: input.loc.to_string(),
                    title: input.title,
                    dry_run: input.dry_run,
                }),
            },
            &a,
        )
        .await
    })
    .await
}

#[derive(Deserialize)]
struct ReplaceBodyInput {
    #[serde(flatten)]
    target: EditTargetInput,
    body: String,
    #[serde(default)]
    dry_run: bool,
    #[serde(default)]
    with_diagnostics: bool,
}

async fn run_replace_body(args: Value, app: &App) -> Result<CapturedOutput> {
    let input: ReplaceBodyInput = parse_args(args)?;
    let (target, symbol) = input.target.into_target()?;
    run_edit(app, move || EditCommand::ReplaceBody {
        target,
        symbol,
        body: input.body,
        dry_run: input.dry_run,
        with_diagnostics: input.with_diagnostics,
    })
    .await
}

#[derive(Deserialize)]
struct InsertInput {
    #[serde(flatten)]
    target: EditTargetInput,
    code: String,
    #[serde(default)]
    dry_run: bool,
    #[serde(default)]
    with_diagnostics: bool,
}

async fn run_insert_before(args: Value, app: &App) -> Result<CapturedOutput> {
    let input: InsertInput = parse_args(args)?;
    let (target, symbol) = input.target.into_target()?;
    run_edit(app, move || EditCommand::InsertBefore {
        target,
        symbol,
        code: input.code,
        dry_run: input.dry_run,
        with_diagnostics: input.with_diagnostics,
    })
    .await
}

async fn run_insert_after(args: Value, app: &App) -> Result<CapturedOutput> {
    let input: InsertInput = parse_args(args)?;
    let (target, symbol) = input.target.into_target()?;
    run_edit(app, move || EditCommand::InsertAfter {
        target,
        symbol,
        code: input.code,
        dry_run: input.dry_run,
        with_diagnostics: input.with_diagnostics,
    })
    .await
}

#[derive(Deserialize)]
struct DeleteSymbolInput {
    #[serde(flatten)]
    target: EditTargetInput,
    #[serde(default)]
    expect_no_references: bool,
    #[serde(default)]
    dry_run: bool,
    #[serde(default)]
    with_diagnostics: bool,
}

async fn run_delete_symbol(args: Value, app: &App) -> Result<CapturedOutput> {
    let input: DeleteSymbolInput = parse_args(args)?;
    let (target, symbol) = input.target.into_target()?;
    run_edit(app, move || EditCommand::Delete {
        target,
        symbol,
        expect_no_references: input.expect_no_references,
        dry_run: input.dry_run,
        with_diagnostics: input.with_diagnostics,
    })
    .await
}

async fn run_edit(
    app: &App,
    command: impl FnOnce() -> EditCommand + Send + 'static,
) -> Result<CapturedOutput> {
    capture(app, move |a| async move {
        crate::cli::commands::edit::execute(EditArgs { command: command() }, &a).await
    })
    .await
}

/// Field names each tool's input struct deserializes, keyed by tool name.
/// Kept beside the structs so a new field and its row land together; the
/// catalog lockstep test asserts every row is a subset of the tool's
/// advertised schema properties — `dispatch` rejects undeclared keys, so
/// an unadvertised field would be unreachable at runtime.
#[cfg(test)]
pub(super) fn input_fields(tool: &str) -> Option<&'static [&'static str]> {
    Some(match tool {
        "get_project_overview" => &[],
        "get_file_overview" => &["path", "depth", "related_limit"],
        "search_symbols" => &["query", "language", "kind", "limit"],
        "search_content" => &["query", "language", "limit"],
        "list_file_symbols" => &["file", "depth", "body", "signature"],
        "inspect_symbol" => &["symbol_path", "language", "body"],
        "find_definition" | "get_hover" => &["file", "line", "column"],
        "find_references" => &["file", "line", "column", "snippet", "limit"],
        "find_callers" | "find_callees" | "find_implementations" => {
            &["file", "line", "column", "limit"]
        }
        "get_context" => &[
            "file",
            "line",
            "column",
            "callers",
            "callees",
            "types",
            "tests",
            "all",
            "with_bodies",
            "body_tokens",
        ],
        "get_impact" => &["file", "line", "column", "limit", "depth"],
        "get_diagnostics" => &["file", "severity", "source"],
        "build_context_pack" => &["tokens", "focus", "per_file", "shape"],
        "rename_symbol" => &["file", "line", "column", "new_name", "dry_run"],
        "list_code_actions" => &["file", "line", "column", "kind", "preferred"],
        "apply_code_action" => &["file", "line", "column", "title", "dry_run"],
        "replace_symbol_body" => &[
            "file",
            "line",
            "column",
            "symbol",
            "body",
            "dry_run",
            "with_diagnostics",
        ],
        "insert_before_symbol" | "insert_after_symbol" => &[
            "file",
            "line",
            "column",
            "symbol",
            "code",
            "dry_run",
            "with_diagnostics",
        ],
        "delete_symbol" => &[
            "file",
            "line",
            "column",
            "symbol",
            "expect_no_references",
            "dry_run",
            "with_diagnostics",
        ],
        _ => return None,
    })
}

/// The complementary lockstep direction: deserialize `args` into the
/// tool's input struct and discard the value. The catalog lockstep test
/// feeds this an object covering every advertised property, so a
/// required struct field the catalog (and the `input_fields` row above)
/// doesn't advertise fails here as a missing-field error instead of
/// surfacing as a runtime invalid_argument.
#[cfg(test)]
pub(super) fn deserialize_input(tool: &str, args: Value) -> Option<Result<(), String>> {
    fn check<T: serde::de::DeserializeOwned>(args: Value) -> Result<(), String> {
        serde_json::from_value::<T>(args)
            .map(drop)
            .map_err(|e| e.to_string())
    }
    Some(match tool {
        "get_project_overview" => Ok(()),
        "get_file_overview" => check::<FileOverviewInput>(args),
        "search_symbols" => check::<SearchSymbolsInput>(args),
        "search_content" => check::<SearchContentInput>(args),
        "list_file_symbols" => check::<ListFileSymbolsInput>(args),
        "inspect_symbol" => check::<InspectSymbolInput>(args),
        "find_definition" | "get_hover" => check::<LocationInput>(args),
        "find_references" => check::<FindReferencesInput>(args),
        "find_callers" | "find_callees" | "find_implementations" => {
            check::<CallHierarchyInput>(args)
        }
        "get_context" => check::<GetContextInput>(args),
        "get_impact" => check::<GetImpactInput>(args),
        "get_diagnostics" => check::<GetDiagnosticsInput>(args),
        "build_context_pack" => check::<ContextPackInput>(args),
        "rename_symbol" => check::<RenameSymbolInput>(args),
        "list_code_actions" => check::<ListCodeActionsInput>(args),
        "apply_code_action" => check::<ApplyCodeActionInput>(args),
        "replace_symbol_body" => check::<ReplaceBodyInput>(args),
        "insert_before_symbol" | "insert_after_symbol" => check::<InsertInput>(args),
        "delete_symbol" => check::<DeleteSymbolInput>(args),
        _ => return None,
    })
}
