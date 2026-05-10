use anyhow::Result;
use clap::{Args, ValueEnum};
use serde::{Deserialize, Serialize};

use crate::app::App;
use crate::cli::response::Section;
use crate::infra::file_filter::FileFilter;
use crate::services::pack::{PackConfig, PackResult, PackedFile, PackedSymbol, build_pack};

#[derive(Args, Debug)]
#[command(
    after_long_help = "Pack the most relevant files into a token-budgeted context bundle.\n\
                       Files are ranked with PageRank over the import graph, then top-level\n\
                       signatures are emitted until the budget is reached.\n\
                       \n\
                       Examples:\n  \
                       symora pack --tokens 4000\n  \
                       symora pack --tokens 8000 --focus src/services/pack.rs\n  \
                       symora pack --tokens 2000 --shape markdown"
)]
pub struct PackArgs {
    /// Approximate token budget. Files are added in rank order until adding
    /// the next one would push the pack over this number.
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

    let filter = FileFilter::with_gitignore(&root);
    let cfg = PackConfig {
        max_symbols_per_file: args.per_file,
        ..PackConfig::default()
    };

    match build_pack(&root, args.tokens, args.focus.as_deref(), &filter, &cfg) {
        Ok(result) => match args.shape {
            PackShape::Json => {
                let output = build_json_output(&args, result);
                ctx.print_success(output);
            }
            PackShape::Markdown => {
                let text = render_markdown(&args, &result);
                ctx.print_text(&text);
            }
        },
        Err(e) => ctx.print_error(e),
    }

    Ok(())
}

fn build_json_output(args: &PackArgs, result: PackResult) -> PackOutput {
    let estimated = result.estimated_tokens;
    let graph = result.graph_size;
    let files: Vec<PackFileOutput> = result.files.into_iter().map(into_file_output).collect();
    PackOutput {
        budget_tokens: args.tokens,
        estimated_tokens: estimated,
        graph_size: graph,
        focus: args.focus.clone(),
        files: Section::new(files),
    }
}

fn into_file_output(file: PackedFile) -> PackFileOutput {
    PackFileOutput {
        path: file.path.display().to_string(),
        language: file.language.lsp_id().to_string(),
        rank: file.rank,
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

fn render_markdown(args: &PackArgs, result: &PackResult) -> String {
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
        if file.symbols.is_empty() {
            out.push_str("_(no extractable symbols)_\n\n");
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
    use super::*;
    use crate::models::symbol::Language;
    use std::path::PathBuf;

    fn sample_result() -> PackResult {
        PackResult {
            files: vec![PackedFile {
                path: PathBuf::from("src/main.rs"),
                language: Language::Rust,
                rank: 0.123,
                symbols: vec![PackedSymbol {
                    name: "main".to_string(),
                    kind: "function".to_string(),
                    line: 10,
                    signature: "pub fn main()".to_string(),
                }],
            }],
            estimated_tokens: 50,
            graph_size: 25,
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
        let md = render_markdown(&args, &sample_result());
        assert!(md.starts_with("# Symora context pack"));
        assert!(md.contains("4000"));
        assert!(md.contains("auth"));
        assert!(md.contains("src/main.rs"));
        assert!(md.contains("pub fn main()"));
    }

    #[test]
    fn markdown_omits_focus_line_when_absent() {
        let args = PackArgs {
            tokens: 1000,
            focus: None,
            per_file: 12,
            shape: PackShape::Markdown,
        };
        let md = render_markdown(&args, &sample_result());
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
        let md = render_markdown(&args, &result);
        assert!(md.contains("no extractable symbols"));
    }
}
