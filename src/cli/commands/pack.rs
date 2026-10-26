use anyhow::Result;
use clap::{Args, ValueEnum};
use serde::{Deserialize, Serialize};

use crate::app::App;
use crate::cli::response::Section;
use crate::cli::response::disclosure::{as_paths, name_some, relative_paths};
use crate::infra::file_filter::FileFilter;
use crate::services::pack::{PackConfig, PackResult, PackedFile, PackedSymbol, build_pack};

#[derive(Args, Debug)]
#[command(
    after_long_help = "Summarize files within an approximate token budget.\n\
                       Files are ranked from syntax-level import paths, then top-level\n\
                       signatures are emitted until the budget is reached. This is an\n\
                       orientation aid, not a complete dependency graph.\n\
                       Unsupported escape or string syntax is omitted from import edges.\n\
                       Rust paths resolve through each crate's module tree; a path whose\n\
                       file that tree leaves open is omitted with a hint.\n\
                       \n\
                       Examples:\n  \
                       symora pack --tokens 4000\n  \
                       symora pack --tokens 8000 --focus src/services/pack/mod.rs\n  \
                       symora pack --tokens 2000 --shape markdown"
)]
pub struct PackArgs {
    /// Approximate token budget. Files are considered in rank order and
    /// included when they fit in the remaining estimate.
    #[arg(long, default_value_t = crate::constants::defaults::PACK_TOKENS)]
    pub tokens: usize,

    /// Optional file path or substring to bias the PageRank towards. The
    /// matching files get extra teleport mass so their neighbours rank
    /// higher too.
    #[arg(long)]
    pub focus: Option<String>,

    /// Cap on top-level symbols per file in the pack.
    #[arg(long, default_value_t = crate::constants::defaults::PACK_SYMBOLS_PER_FILE)]
    pub per_file: usize,

    /// Output shape. JSON is the machine-readable contract;
    /// markdown is a plain-text view ready to paste straight into an LLM
    /// context window.
    #[arg(long, value_enum, default_value_t = PackShape::Json)]
    pub shape: PackShape,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, ValueEnum, Deserialize)]
#[clap(rename_all = "lowercase")]
#[serde(rename_all = "lowercase")]
pub enum PackShape {
    #[default]
    Json,
    Markdown,
}

#[derive(Debug, Serialize)]
struct PackOutput {
    budget_tokens: usize,
    estimated_tokens: usize,
    graph_size: usize,
    /// Present (true) only when the walk could not read part of the tree, so
    /// `graph_size` — the file set this pack selected from — is a lower bound.
    /// The budget's own exclusions are already visible in `estimated_tokens`
    /// against `budget_tokens`; this is the exclusion nothing else would show.
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    incomplete: bool,
    /// The paths that flag stands for. A reader cannot act on "somewhere in
    /// the tree", and this pack has no rebuild to prescribe — naming them is
    /// the whole remedy. The markdown shape says the same sentence.
    #[serde(skip_serializing_if = "<[String]>::is_empty")]
    unread_paths: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    focus: Option<String>,
    files: Section<PackFileOutput>,
}

#[derive(Debug, Serialize)]
struct PackFileOutput {
    path: String,
    language: String,
    rank: f64,
    symbols: Vec<PackSymbolOutput>,
    /// Total declarations when the per-file cap or token budget showed fewer. Absent when
    /// the list above is all of them, so a shape and a whole one read apart.
    #[serde(skip_serializing_if = "Option::is_none")]
    declared_symbols: Option<usize>,
}

#[derive(Debug, Serialize)]
struct PackSymbolOutput {
    name: String,
    kind: String,
    line: u32,
    signature: String,
}

pub async fn execute(args: PackArgs, app: &App) -> Result<()> {
    let ctx = &app.output;
    let root = ctx.root().to_path_buf();

    let filter = FileFilter::new(&root);
    let cfg = PackConfig {
        max_symbols_per_file: args.per_file,
        max_file_size_bytes: app.config().search.max_file_size_bytes(),
        ..PackConfig::default()
    };

    let tokens = args.tokens;
    let focus = args.focus.clone();
    let result = tokio::task::spawn_blocking(move || {
        build_pack(&root, tokens, focus.as_deref(), &filter, &cfg)
    })
    .await?;
    match result {
        Ok(result) => match args.shape {
            PackShape::Json => {
                let output = build_json_output(ctx, &args, result);
                ctx.print_success(output);
            }
            PackShape::Markdown => {
                let text = render_markdown(ctx, &args, &result);
                ctx.print_text(&text);
            }
        },
        Err(e) => ctx.print_error(crate::cli::OutputError::invalid(e.to_string())),
    }

    Ok(())
}

fn build_json_output(
    ctx: &crate::cli::OutputContext,
    args: &PackArgs,
    result: PackResult,
) -> PackOutput {
    let hints = declaration_coverage_hints(&result);
    let estimated = result.estimated_tokens;
    let graph = result.graph_size;
    let unread_paths = relative_paths(ctx, &as_paths(&result.unreadable_paths));
    let files: Vec<PackFileOutput> = result.files.into_iter().map(into_file_output).collect();
    PackOutput {
        budget_tokens: args.tokens,
        estimated_tokens: estimated,
        graph_size: graph,
        incomplete: !unread_paths.is_empty(),
        unread_paths,
        focus: args.focus.clone(),
        files: Section::new(files).with_hints(hints),
    }
}

fn declaration_coverage_hints(result: &PackResult) -> Vec<String> {
    let mut hints = Vec::new();
    if result.rust_relative_imports_omitted {
        hints.push("Rust paths whose file the crate's module tree leaves open (from a file no crate's mod declarations reach, or through a module whose file its declarations do not determine) were omitted; use semantic references or inspect source.".to_string());
    }
    if !result.undetermined_imports.is_empty() {
        let specifiers: Vec<String> = result
            .undetermined_imports
            .iter()
            .map(|specifier| format!("`{specifier}`"))
            .collect();
        hints.push(format!(
            "{} import specifier(s) matching a tsconfig `paths` alias or a workspace package name named no file the configuration determines ({}); their dependencies do not contribute to ranking. Inspect the configuration or source.",
            specifiers.len(),
            name_some(&specifiers)
        ));
    }
    if !result.declaration_undetermined_imports.is_empty() {
        let specifiers: Vec<String> = result
            .declaration_undetermined_imports
            .iter()
            .map(|specifier| format!("`{specifier}`"))
            .collect();
        hints.push(format!(
            "{} import specifier(s) that `classic` module resolution reads name a JavaScript file in the project, but a declaration in `node_modules/@types` or a `typeRoots` directory, which TypeScript takes first and pack does not look up, may stand for them ({}); their dependencies do not contribute to ranking. Inspect the declaration or source.",
            specifiers.len(),
            name_some(&specifiers)
        ));
    }
    if !result.version_undetermined_imports.is_empty() {
        let specifiers: Vec<String> = result
            .version_undetermined_imports
            .iter()
            .map(|specifier| format!("`{specifier}`"))
            .collect();
        let ranges = if result.typescript_ranges.is_empty() {
            String::new()
        } else {
            let ranges: Vec<String> = result
                .typescript_ranges
                .iter()
                .map(|range| format!("`{range}`"))
                .collect();
            format!(" (declared: {})", name_some(&ranges))
        };
        // A declared range settles nothing where a `typescript` is
        // installed, since pack reads the installed one first.
        const DECLARE: &str = "Install `typescript`, or declare it as a caret, tilde, exact or x-range version of 5, 6 or 7 (`^6.0.3`) in the nearest `package.json`, to settle them.";
        const REPLACE: &str = "pack reads the installed one before any declared range.";
        let remedy = match (
            result.typescript_not_installed,
            result.typescript_installed_unread,
        ) {
            (_, false) => DECLARE.to_string(),
            (false, true) => format!(
                "Install a `typescript` of 5, 6 or 7 (`6.0.3`) in place of the one installed above 7 or with no version number to settle them: {REPLACE}"
            ),
            (true, true) => format!(
                "{DECLARE} Where the one installed is above 7 or has no version number, install a 5, 6 or 7 in its place: {REPLACE}"
            ),
        };
        hints.push(format!(
            "{} import specifier(s) name different files under the TypeScript majors that may compile their importers ({}): pack reads no major from 5 to 7 from the `typescript` installed or declared{ranges}; their dependencies do not contribute to ranking. {remedy}",
            specifiers.len(),
            name_some(&specifiers)
        ));
    }
    if !result.undetermined_ts_projects.is_empty() {
        hints.push(format!(
            "{} file(s) sit below a tsconfig whose `references` name no one project compiling them ({}); their non-relative specifiers were read without a tsconfig's `paths`, `baseUrl` or conditions. Inspect the project references.",
            result.undetermined_ts_projects.len(),
            name_some(&result.undetermined_ts_projects)
        ));
    }
    if !result.unsupported_import_languages.is_empty() {
        let languages = result
            .unsupported_import_languages
            .iter()
            .map(|language| language.lsp_id())
            .collect::<Vec<_>>()
            .join(", ");
        hints.push(format!("No import extractor for {languages}; their outgoing dependencies do not contribute to ranking. Use references or inspect source for semantic links."));
    }
    if result.unsupported_languages.is_empty() {
        return hints;
    }
    let languages = result
        .unsupported_languages
        .iter()
        .map(|language| language.lsp_id())
        .collect::<Vec<_>>()
        .join(", ");
    hints.push(format!(
        "No declaration extractor for {languages}; files in these languages participate in the import graph but have no signatures in this pack. Inspect their source or query their language server."
    ));
    hints
}

fn into_file_output(file: PackedFile) -> PackFileOutput {
    PackFileOutput {
        path: file.path.display().to_string(),
        language: file.language.lsp_id().to_string(),
        rank: file.rank,
        declared_symbols: (file.declared_symbols > file.symbols.len())
            .then_some(file.declared_symbols),
        symbols: file.symbols.into_iter().map(into_symbol_output).collect(),
    }
}

fn into_symbol_output(symbol: PackedSymbol) -> PackSymbolOutput {
    PackSymbolOutput {
        name: symbol.name,
        kind: symbol.kind,
        line: symbol.line,
        signature: symbol.signature,
    }
}

fn render_markdown(
    ctx: &crate::cli::OutputContext,
    args: &PackArgs,
    result: &PackResult,
) -> String {
    let mut out = String::new();
    out.push_str("# Symora context pack\n\n");
    out.push_str(&format!(
        "- **Budget**: {} tokens (used ~{})\n",
        args.tokens, result.estimated_tokens
    ));
    out.push_str(&format!(
        "- **Graph**: {} files in the import graph\n",
        result.graph_size
    ));
    // The markdown shape publishes `graph_size` as a header fact just as the
    // JSON does, so it carries the same claim and needs the same qualifier.
    let named = relative_paths(ctx, &as_paths(&result.unreadable_paths));
    if !named.is_empty() {
        out.push_str(&format!(
            "- **Incomplete**: {} path(s) could not be read ({}), so the graph above is a lower \
             bound\n",
            named.len(),
            name_some(&named)
        ));
    }
    for hint in declaration_coverage_hints(result) {
        out.push_str(&format!("- **Coverage**: {hint}\n"));
    }
    out.push_str(&format!("- **Files included**: {}\n", result.files.len()));
    if let Some(focus) = args.focus.as_deref() {
        out.push_str(&format!("- **Focus**: `{focus}`\n"));
    }
    out.push('\n');

    for file in &result.files {
        out.push_str(&format!(
            "## {} _(rank {:.3}, {})_\n",
            file.path.display(),
            file.rank,
            file.language.lsp_id(),
        ));
        if file.declared_symbols > file.symbols.len() {
            out.push_str(&format!(
                "_Showing {} of {} declarations._\n",
                file.symbols.len(),
                file.declared_symbols,
            ));
        }
        if file.symbols.is_empty() {
            if crate::services::symbols::SymbolExtractor::is_supported(file.language) {
                out.push_str("_(no extractable symbols)_\n\n");
            } else {
                out.push_str(
                    "_(no declaration extractor for this language; inspect the source)_\n\n",
                );
            }
            continue;
        }
        for sym in &file.symbols {
            out.push_str(&format!(
                "- L{} `{}` — {}\n",
                sym.line, sym.kind, sym.signature
            ));
        }
        out.push('\n');
    }

    out
}

#[cfg(test)]
mod tests {
    fn ctx() -> crate::cli::OutputContext {
        crate::cli::OutputContext::new(PathBuf::from("/proj"), crate::cli::OutputOptions::default())
    }

    use super::*;
    use crate::models::symbol::Language;
    use std::path::PathBuf;

    fn sample_result() -> PackResult {
        PackResult {
            files: vec![PackedFile {
                path: PathBuf::from("src/main.rs"),
                language: Language::Rust,
                rank: 0.123,
                declared_symbols: 1,
                symbols: vec![PackedSymbol {
                    name: "main".to_string(),
                    kind: "function".to_string(),
                    line: 10,
                    signature: "pub fn main()".to_string(),
                }],
            }],
            estimated_tokens: 50,
            graph_size: 25,
            unsupported_languages: Vec::new(),
            unsupported_import_languages: Vec::new(),
            rust_relative_imports_omitted: false,
            undetermined_imports: Vec::new(),
            declaration_undetermined_imports: Vec::new(),
            version_undetermined_imports: Vec::new(),
            typescript_ranges: Vec::new(),
            typescript_not_installed: false,
            typescript_installed_unread: false,
            undetermined_ts_projects: Vec::new(),
            unreadable_paths: Vec::new(),
        }
    }

    #[test]
    fn unsupported_declarations_are_disclosed_even_when_the_pack_is_empty() {
        let root = tempfile::tempdir().unwrap();
        std::fs::write(
            root.path().join("demo.ex"),
            "defmodule Demo do\n  def greet(), do: :ok\nend\n",
        )
        .unwrap();
        let result = build_pack(
            root.path(),
            4000,
            None,
            &FileFilter::new(root.path()),
            &PackConfig {
                use_cache: false,
                ..Default::default()
            },
        )
        .unwrap();
        assert!(result.files.is_empty());
        assert_eq!(result.unsupported_languages, vec![Language::Elixir]);
        let args = PackArgs {
            tokens: 4000,
            focus: None,
            per_file: 12,
            shape: PackShape::Json,
        };
        assert!(
            render_markdown(&ctx(), &args, &result).contains("No declaration extractor for elixir")
        );
        let output = serde_json::to_value(build_json_output(&ctx(), &args, result)).unwrap();
        assert_eq!(output["files"]["count"], 0);
        let hints = output["files"]["hints"].as_array().unwrap();
        for disclosed in [
            "No import extractor for elixir",
            "No declaration extractor for elixir",
        ] {
            assert!(
                hints
                    .iter()
                    .any(|hint| hint.as_str().unwrap().contains(disclosed)),
                "{disclosed}: {hints:?}"
            );
        }
    }

    #[test]
    fn markdown_includes_budget_and_files() {
        let args = PackArgs {
            tokens: 4000,
            focus: Some("auth".to_string()),
            per_file: 12,
            shape: PackShape::Markdown,
        };
        let mut result = sample_result();
        let md = render_markdown(&ctx(), &args, &result);
        assert!(md.starts_with("# Symora context pack"));
        assert!(md.contains("4000"));
        assert!(md.contains("auth"));
        assert!(md.contains("src/main.rs"));
        assert!(md.contains("pub fn main()"));
        assert!(!md.contains("Showing"));

        result.files[0].declared_symbols = 3;
        let partial = render_markdown(&ctx(), &args, &result);
        assert!(partial.contains("Showing 1 of 3 declarations"));
        assert!(partial.contains("pub fn main()"));
        assert!(!partial.contains("Rust paths whose file the crate's module tree leaves open"));
        result.rust_relative_imports_omitted = true;
        result.undetermined_imports = vec!["@/styles/app".to_string()];
        result.declaration_undetermined_imports = vec!["jq".to_string()];
        result.version_undetermined_imports = vec!["p6".to_string()];
        result.typescript_ranges = vec![">=5".to_string()];
        result.typescript_not_installed = true;
        result.undetermined_ts_projects = vec!["scripts/gen.ts".to_string()];
        assert!(
            render_markdown(&ctx(), &args, &result)
                .contains("Rust paths whose file the crate's module tree leaves open")
        );
        let json = serde_json::to_value(build_json_output(&ctx(), &args, result)).unwrap();
        assert!(
            json["files"]["hints"][0]
                .as_str()
                .unwrap()
                .contains("Rust paths whose file the crate's module tree leaves open")
        );
        assert!(
            json["files"]["hints"][1]
                .as_str()
                .unwrap()
                .contains("1 import specifier(s) matching a tsconfig `paths` alias or a workspace package name named no file the configuration determines (`@/styles/app`)")
        );
        assert!(
            json["files"]["hints"][2]
                .as_str()
                .unwrap()
                .contains("1 import specifier(s) that `classic` module resolution reads name a JavaScript file in the project, but a declaration in `node_modules/@types` or a `typeRoots` directory, which TypeScript takes first and pack does not look up, may stand for them (`jq`); their dependencies do not contribute to ranking.")
        );
        assert!(
            json["files"]["hints"][3]
                .as_str()
                .unwrap()
                .contains("1 import specifier(s) name different files under the TypeScript majors that may compile their importers (`p6`): pack reads no major from 5 to 7 from the `typescript` installed or declared (declared: `>=5`); their dependencies do not contribute to ranking. Install `typescript`, or declare it as a caret, tilde, exact or x-range version of 5, 6 or 7 (`^6.0.3`) in the nearest `package.json`, to settle them.")
        );
        assert!(
            json["files"]["hints"][4]
                .as_str()
                .unwrap()
                .contains("1 file(s) sit below a tsconfig whose `references` name no one project compiling them (scripts/gen.ts)")
        );
    }

    /// A `typescript` installed above 7, or with no version number, is read
    /// before any declared range, so the version hint advises installing a
    /// 5, 6 or 7 in its place, alone where every importer has one installed.
    #[test]
    fn the_version_hint_advises_replacing_an_installed_typescript_it_reads_no_major_from() {
        let hint = |not_installed, installed_unread| {
            let mut result = sample_result();
            result.version_undetermined_imports = vec!["p6".to_string()];
            result.typescript_not_installed = not_installed;
            result.typescript_installed_unread = installed_unread;
            declaration_coverage_hints(&result).remove(0)
        };
        let declare = "Install `typescript`, or declare it as a caret, tilde, exact or x-range version of 5, 6 or 7 (`^6.0.3`) in the nearest `package.json`, to settle them.";
        assert!(hint(true, false).ends_with(declare));
        assert!(hint(false, true).ends_with(
            "their dependencies do not contribute to ranking. Install a `typescript` of 5, 6 or 7 (`6.0.3`) in place of the one installed above 7 or with no version number to settle them: pack reads the installed one before any declared range."
        ));
        assert!(hint(true, true).ends_with(&format!(
            "{declare} Where the one installed is above 7 or has no version number, install a 5, 6 or 7 in its place: pack reads the installed one before any declared range."
        )));
    }

    #[test]
    fn markdown_omits_focus_line_when_absent() {
        let args = PackArgs {
            tokens: 1000,
            focus: None,
            per_file: 12,
            shape: PackShape::Markdown,
        };
        let md = render_markdown(&ctx(), &args, &sample_result());
        assert!(!md.contains("Focus"));
    }

    #[test]
    fn markdown_marks_files_without_symbols() {
        let mut result = sample_result();
        result.files[0].symbols.clear();
        let args = PackArgs {
            tokens: 1000,
            focus: None,
            per_file: 12,
            shape: PackShape::Markdown,
        };
        let md = render_markdown(&ctx(), &args, &result);
        assert!(md.contains("no extractable symbols"));
    }
}
