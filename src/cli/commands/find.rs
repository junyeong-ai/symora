//! Find command implementation
//!
//! LSP-powered symbol discovery and navigation.

use anyhow::Result;
use clap::{Args, Subcommand};

use crate::app::App;
use crate::cli::ParsedLocation;
use crate::cli::response::{
    DefinitionResponse, LocationOutput, ReferencesResponse, SymbolOutput, SymbolsResponse,
};
use crate::models::lsp::FindSymbolsOptions;
use crate::models::symbol::{Language, Symbol, SymbolKind};

#[derive(Args, Debug)]
pub struct FindArgs {
    #[command(subcommand)]
    pub command: FindCommand,
}

#[derive(Subcommand, Debug)]
pub enum FindCommand {
    /// Find symbols in a file
    Symbol {
        /// File path (or use --name for workspace search)
        #[arg(required_unless_present = "name")]
        file: Option<String>,

        /// Search symbols by name across workspace
        #[arg(short, long)]
        name: Option<String>,

        /// Filter by symbol path (e.g., "Class/method", "*/update", "Class/*")
        #[arg(short = 's', long)]
        symbol: Option<String>,

        /// Language for workspace search (use 'symora doctor' to see all supported)
        #[arg(short, long, required_if_eq("name", "name"))]
        lang: Option<String>,

        /// Include symbol body (source code)
        #[arg(short, long)]
        body: bool,

        /// Include nested symbols up to depth (0 = top-level only)
        #[arg(short, long, default_value = "0")]
        depth: u32,

        /// Filter by symbol kind(s), comma-separated (function,class,method)
        #[arg(long)]
        kind: Option<String>,

        /// Exclude symbol kind(s), comma-separated (variable,constant)
        #[arg(long)]
        exclude: Option<String>,

        /// Use substring matching for symbol names (case-insensitive)
        #[arg(long)]
        substring: bool,

        /// Exclude low-level symbols (variables, constants, literals)
        #[arg(long)]
        structural: bool,

        /// Maximum results (default from config: lsp.symbol_limit)
        #[arg(long)]
        limit: Option<usize>,
    },

    /// Find all references to a symbol at position
    Refs {
        /// File path with position (file:line:column)
        location: String,

        /// Maximum results (default from config: lsp.refs_limit)
        #[arg(long)]
        limit: Option<usize>,
    },

    /// Go to definition of symbol at position
    Def {
        /// File path with position (file:line:column)
        location: String,
    },

    /// Go to type definition (find the type of a variable/expression)
    Typedef {
        /// File path with position (file:line:column)
        location: String,
    },

    /// Find implementations of a trait/interface
    Impl {
        /// File path with position (file:line:column)
        location: String,

        /// Maximum results (default from config: lsp.impl_limit)
        #[arg(long)]
        limit: Option<usize>,
    },
}

pub async fn execute(args: FindArgs, app: &App) -> Result<()> {
    let ctx = &app.output;
    let cfg = app.config();

    match args.command {
        FindCommand::Symbol {
            file,
            name,
            symbol,
            lang,
            body,
            depth,
            kind,
            exclude,
            substring,
            structural,
            limit,
        } => {
            let limit = limit.unwrap_or(cfg.lsp.symbol_limit);

            // Parse include kinds (comma-separated)
            let include_kinds = parse_kind_list(&kind, ctx)?;
            let exclude_kinds = parse_kind_list(&exclude, ctx)?;

            // Workspace search by name
            if let Some(query) = name {
                let language = lang
                    .map(|l| Language::from_str_loose(&l))
                    .unwrap_or(Language::Unknown);

                if language == Language::Unknown {
                    ctx.print_error("Language is required for workspace symbol search. Use --lang <language>. Run 'symora doctor' to see all supported languages.");
                    return Ok(());
                }

                match app.lsp.workspace_symbols(&query, language).await {
                    Ok(symbols) => {
                        let filtered = Symbol::filter_advanced(
                            &symbols,
                            if substring { Some(&query) } else { None },
                            substring,
                            include_kinds.as_deref(),
                            exclude_kinds.as_deref(),
                            structural,
                        );

                        let limited: Vec<_> = filtered.into_iter().take(limit).collect();
                        let response = SymbolsResponse {
                            count: limited.len(),
                            symbols: limited
                                .iter()
                                .map(|s| SymbolOutput::from_symbol(s, ctx.root()))
                                .collect(),
                        };
                        ctx.print_success_flat(response);
                    }
                    Err(e) => ctx.print_error(&e.to_string()),
                }
                return Ok(());
            }

            // File-based symbol search
            let file = match file {
                Some(f) => f,
                None => {
                    ctx.print_error("File path is required when --name is not provided");
                    return Ok(());
                }
            };
            let path = std::path::Path::new(&file);
            let abs_path = if path.is_absolute() {
                path.to_path_buf()
            } else {
                app.root().join(path)
            };

            let effective_depth = if symbol.is_some() && depth == 0 {
                10
            } else {
                depth
            };
            let options = FindSymbolsOptions::new().with_depth(effective_depth);
            let options = if body { options.with_body() } else { options };

            match app.lsp.find_symbols(&abs_path, options).await {
                Ok(mut symbols) => {
                    Symbol::compute_paths_for_all(&mut symbols);

                    let filtered = Symbol::filter_advanced(
                        &symbols,
                        symbol.as_deref(),
                        substring,
                        include_kinds.as_deref(),
                        exclude_kinds.as_deref(),
                        structural,
                    );

                    let limited: Vec<_> = filtered.into_iter().take(limit).collect();
                    let response = SymbolsResponse {
                        count: limited.len(),
                        symbols: limited
                            .iter()
                            .map(|s| SymbolOutput::from_symbol(s, ctx.root()))
                            .collect(),
                    };
                    ctx.print_success_flat(response);
                }
                Err(e) => ctx.print_error(&e.to_string()),
            }
        }

        FindCommand::Refs { location, limit } => {
            let limit = limit.unwrap_or(cfg.lsp.refs_limit);
            let loc = ParsedLocation::parse(&location)?.to_absolute()?;

            match app
                .lsp
                .find_references(&loc.file, loc.line, loc.column)
                .await
            {
                Ok(locations) => {
                    let project_refs: Vec<_> = locations
                        .iter()
                        .filter(|l| ctx.is_project_path(&l.file))
                        .take(limit)
                        .collect();

                    let response = ReferencesResponse {
                        count: project_refs.len(),
                        references: project_refs
                            .iter()
                            .map(|l| {
                                LocationOutput::from_path(&l.file, l.line, l.column, ctx.root())
                            })
                            .collect(),
                    };
                    ctx.print_success_flat(response);
                }
                Err(e) => ctx.print_error(&e.to_string()),
            }
        }

        FindCommand::Def { location } => {
            let loc = ParsedLocation::parse(&location)?.to_absolute()?;

            match app
                .lsp
                .goto_definition(&loc.file, loc.line, loc.column)
                .await
            {
                Ok(Some(def)) => {
                    let response = DefinitionResponse {
                        definition: Some(LocationOutput::from_path(
                            &def.file,
                            def.line,
                            def.column,
                            ctx.root(),
                        )),
                        message: None,
                    };
                    ctx.print_success_flat(response);
                }
                Ok(None) => {
                    let response = DefinitionResponse {
                        definition: None,
                        message: Some("No definition found".to_string()),
                    };
                    ctx.print_success_flat(response);
                }
                Err(e) => ctx.print_error(&e.to_string()),
            }
        }

        FindCommand::Typedef { location } => {
            let loc = ParsedLocation::parse(&location)?.to_absolute()?;

            match app
                .lsp
                .goto_type_definition(&loc.file, loc.line, loc.column)
                .await
            {
                Ok(Some(def)) => {
                    let response = DefinitionResponse {
                        definition: Some(LocationOutput::from_path(
                            &def.file,
                            def.line,
                            def.column,
                            ctx.root(),
                        )),
                        message: None,
                    };
                    ctx.print_success_flat(response);
                }
                Ok(None) => {
                    let response = DefinitionResponse {
                        definition: None,
                        message: Some("No type definition found".to_string()),
                    };
                    ctx.print_success_flat(response);
                }
                Err(e) => ctx.print_error(&e.to_string()),
            }
        }

        FindCommand::Impl { location, limit } => {
            let limit = limit.unwrap_or(cfg.lsp.impl_limit);
            let loc = ParsedLocation::parse(&location)?.to_absolute()?;

            match app
                .lsp
                .find_implementations(&loc.file, loc.line, loc.column)
                .await
            {
                Ok(locations) => {
                    let limited: Vec<_> = locations.into_iter().take(limit).collect();
                    let response = ReferencesResponse {
                        count: limited.len(),
                        references: limited
                            .iter()
                            .map(|l| {
                                LocationOutput::from_path(&l.file, l.line, l.column, ctx.root())
                            })
                            .collect(),
                    };
                    ctx.print_success_flat(response);
                }
                Err(e) => ctx.print_error(&e.to_string()),
            }
        }
    }

    Ok(())
}

/// Parse comma-separated kind list into Vec<SymbolKind>
fn parse_kind_list(
    kind_str: &Option<String>,
    ctx: &crate::cli::OutputContext,
) -> Result<Option<Vec<SymbolKind>>> {
    let Some(kinds) = kind_str else {
        return Ok(None);
    };

    let mut result = Vec::new();
    for k in kinds.split(',') {
        let k = k.trim();
        if k.is_empty() {
            continue;
        }
        match k.parse::<SymbolKind>() {
            Ok(kind) => result.push(kind),
            Err(_) => {
                ctx.print_error(&format!(
                    "Unknown symbol kind: '{}'. Valid kinds: {}",
                    k,
                    SymbolKind::all_kind_names().join(", ")
                ));
                anyhow::bail!("Invalid kind");
            }
        }
    }

    if result.is_empty() {
        Ok(None)
    } else {
        Ok(Some(result))
    }
}
