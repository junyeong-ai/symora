//! Init command implementation
//!
//! Initialize a new Symora project.

use std::path::PathBuf;

use anyhow::Result;
use clap::Args;
use serde::Serialize;

use crate::app::App;

#[derive(Args, Debug)]
pub struct InitArgs {
    /// Project path (defaults to current directory)
    #[arg(short, long)]
    pub path: Option<PathBuf>,

    /// Project name
    #[arg(short, long)]
    pub name: Option<String>,

    /// Force re-initialization
    #[arg(short, long)]
    pub force: bool,
}

#[derive(Serialize)]
struct InitResponse {
    status: String,
    name: Option<String>,
    path: String,
    config_path: String,
    languages: Vec<String>,
}

pub async fn execute(args: InitArgs, app: &App) -> Result<()> {
    let ctx = &app.output;
    let _path = args.path.unwrap_or_else(|| app.root().to_path_buf());

    match app.project.init(args.name.as_deref(), args.force).await {
        Ok(info) => {
            let response = InitResponse {
                status: "initialized".to_string(),
                name: Some(info.name),
                path: ctx.relative_path(&info.root),
                config_path: ctx.relative_path(&info.config_path),
                languages: info
                    .languages
                    .iter()
                    .map(|l| format!("{:?}", l).to_lowercase())
                    .collect(),
            };
            ctx.print_success_flat(response);
        }
        Err(e) => ctx.print_error(&e.to_string()),
    }

    Ok(())
}
