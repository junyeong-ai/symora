use std::sync::Arc;

use anyhow::Result;
use serde::Deserialize;
use serde_json::Value;

use crate::app::App;
use crate::cli::LocationArg;
use crate::cli::commands::{
    actions::{ActionsArgs, ActionsCommand, ApplyArgs, ListArgs},
    callees::CalleesArgs,
    callers::CallersArgs,
    context::ContextArgs,
    def::DefArgs,
    hover::HoverArgs,
    impact::ImpactArgs,
    implementations::ImplArgs,
    map::{MapArgs, MapCommand},
    pack::{PackArgs, PackShape},
    refs::RefsArgs,
    rename::RenameArgs,
    search::{SearchArgs, SearchCommand},
    symbols::SymbolsArgs,
    write::{InsertArgs, ReplaceBodyArgs, WriteArgs, WriteCommand},
};
use crate::cli::output::{BufferedSink, OutputFormat, OutputOptions};
use crate::constants::defaults;

use super::schema::LocationInput;

pub async fn dispatch(name: &str, arguments: Value, app: &App) -> Result<String> {
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
        "build_context_pack" => run_context_pack(arguments, app).await,
        "rename_symbol" => run_rename_symbol(arguments, app).await,
        "list_code_actions" => run_list_code_actions(arguments, app).await,
        "apply_code_action" => run_apply_code_action(arguments, app).await,
        "replace_symbol_body" => run_replace_body(arguments, app).await,
        "insert_before_symbol" => run_insert_before(arguments, app).await,
        "insert_after_symbol" => run_insert_after(arguments, app).await,
        other => anyhow::bail!("Unknown tool: {other}"),
    }
}

async fn capture<F, Fut>(app: &App, run: F) -> Result<String>
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
    Ok(buf.take().join("\n"))
}

// --- discovery ------------------------------------------------------------

async fn run_project_overview(_args: Value, app: &App) -> Result<String> {
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

async fn run_file_overview(args: Value, app: &App) -> Result<String> {
    let input: FileOverviewInput = serde_json::from_value(args)?;
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

async fn run_search_symbols(args: Value, app: &App) -> Result<String> {
    let input: SearchSymbolsInput = serde_json::from_value(args)?;
    capture(app, move |a| async move {
        crate::cli::commands::search::execute(
            SearchArgs {
                command: SearchCommand::Symbols {
                    query: input.query,
                    language: input.language,
                    kind: input.kind,
                    semantic: false,
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

async fn run_search_content(args: Value, app: &App) -> Result<String> {
    let input: SearchContentInput = serde_json::from_value(args)?;
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

async fn run_list_file_symbols(args: Value, app: &App) -> Result<String> {
    let input: ListFileSymbolsInput = serde_json::from_value(args)?;
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

async fn run_inspect_symbol(args: Value, app: &App) -> Result<String> {
    let input: InspectSymbolInput = serde_json::from_value(args)?;
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

async fn run_find_definition(args: Value, app: &App) -> Result<String> {
    let loc: LocationInput = serde_json::from_value(args)?;
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

async fn run_find_references(args: Value, app: &App) -> Result<String> {
    let input: FindReferencesInput = serde_json::from_value(args)?;
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

async fn run_find_callers(args: Value, app: &App) -> Result<String> {
    let input: CallHierarchyInput = serde_json::from_value(args)?;
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

async fn run_find_callees(args: Value, app: &App) -> Result<String> {
    let input: CallHierarchyInput = serde_json::from_value(args)?;
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

async fn run_find_implementations(args: Value, app: &App) -> Result<String> {
    let input: CallHierarchyInput = serde_json::from_value(args)?;
    capture(app, move |a| async move {
        crate::cli::commands::implementations::execute(
            ImplArgs {
                loc: input.loc.into_arg(),
                limit: input.limit,
            },
            &a,
        )
        .await
    })
    .await
}

async fn run_hover(args: Value, app: &App) -> Result<String> {
    let loc: LocationInput = serde_json::from_value(args)?;
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
}

fn default_context_all() -> bool {
    true
}

async fn run_get_context(args: Value, app: &App) -> Result<String> {
    let input: GetContextInput = serde_json::from_value(args)?;
    capture(app, move |a| async move {
        crate::cli::commands::context::execute(
            ContextArgs {
                loc: input.loc.into_arg(),
                all: input.all,
                callers: input.callers,
                callees: input.callees,
                types: input.types,
                tests: input.tests,
                body: false,
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

async fn run_get_impact(args: Value, app: &App) -> Result<String> {
    let input: GetImpactInput = serde_json::from_value(args)?;
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

async fn run_context_pack(args: Value, app: &App) -> Result<String> {
    let input: ContextPackInput = serde_json::from_value(args)?;
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

async fn run_rename_symbol(args: Value, app: &App) -> Result<String> {
    let input: RenameSymbolInput = serde_json::from_value(args)?;
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

async fn run_list_code_actions(args: Value, app: &App) -> Result<String> {
    let input: ListCodeActionsInput = serde_json::from_value(args)?;
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

async fn run_apply_code_action(args: Value, app: &App) -> Result<String> {
    let input: ApplyCodeActionInput = serde_json::from_value(args)?;
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
    loc: LocationInput,
    body: String,
    #[serde(default)]
    dry_run: bool,
}

async fn run_replace_body(args: Value, app: &App) -> Result<String> {
    let input: ReplaceBodyInput = serde_json::from_value(args)?;
    capture(app, move |a| async move {
        crate::cli::commands::write::execute(
            WriteArgs {
                command: WriteCommand::ReplaceBody(ReplaceBodyArgs {
                    loc: LocationArg {
                        location: input.loc.to_string(),
                    },
                    body: input.body,
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
struct InsertInput {
    #[serde(flatten)]
    loc: LocationInput,
    code: String,
    #[serde(default)]
    dry_run: bool,
}

async fn run_insert_before(args: Value, app: &App) -> Result<String> {
    run_insert(args, app, WriteCommand::InsertBefore).await
}

async fn run_insert_after(args: Value, app: &App) -> Result<String> {
    run_insert(args, app, WriteCommand::InsertAfter).await
}

async fn run_insert(
    args: Value,
    app: &App,
    wrap: fn(InsertArgs) -> WriteCommand,
) -> Result<String> {
    let input: InsertInput = serde_json::from_value(args)?;
    capture(app, move |a| async move {
        let insert_args = InsertArgs {
            loc: LocationArg {
                location: input.loc.to_string(),
            },
            code: input.code,
            dry_run: input.dry_run,
        };
        crate::cli::commands::write::execute(
            WriteArgs {
                command: wrap(insert_args),
            },
            &a,
        )
        .await
    })
    .await
}
