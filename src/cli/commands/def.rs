use anyhow::Result;
use clap::Args;

use crate::app::App;
use crate::cli::LocationArg;
use crate::cli::commands::common::execute_optional;
use crate::cli::python_members::{Qualifier, qualifier, tree_copy_of};
use crate::cli::response::{DefinitionOutput, LocationOutput};

#[derive(Args, Debug)]
pub struct DefArgs {
    #[command(flatten)]
    pub loc: LocationArg,
}

pub async fn execute(args: DefArgs, app: &App) -> Result<()> {
    execute_optional(
        app,
        args.loc,
        |file, line, col| async move {
            let answer = app.lsp.goto_definition(&file, line, col).await?;
            Ok(crate::models::lsp::Indexed::new(
                answer.data.map(|definition| definition.location),
                answer.indexing,
            ))
        },
        |def, ctx| {
            // A definition outside the project that is another checkout's
            // copy of this tree's file is the server reading that copy.
            let copy = tree_copy_of(ctx.root(), &def.file);
            let member = copy.as_ref().and_then(|_| qualifier(app, &def.file));
            DefinitionOutput {
                definition: Some(LocationOutput::from_location(&def, ctx.root())),
                message: None,
                indexing: None,
                coverage_gaps: member.iter().map(Qualifier::coverage_gap).collect(),
                hints: copy
                    .map(|own| {
                        format!(
                            "The definition is another checkout's copy of this tree's {}",
                            own.display()
                        )
                    })
                    .into_iter()
                    .chain(member.map(|q| q.hint))
                    .collect(),
            }
        },
        DefinitionOutput::not_found,
    )
    .await
}
