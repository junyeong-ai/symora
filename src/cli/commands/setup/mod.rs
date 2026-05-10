//! `symora setup` — orchestrates Claude Code skill install and language-server
//! dependency install. Runs the full flow when invoked without a subcommand.

mod deps;
mod skill;

use anyhow::Result;
use clap::{Args, Subcommand};
use serde::Serialize;

use crate::app::App;

pub use deps::{DepsArgs, DepsGroup, DepsOutcome, run_deps};
pub use skill::{SkillArgs, SkillOutcome, run_skill};

#[derive(Args, Debug)]
pub struct SetupArgs {
    #[command(subcommand)]
    pub command: Option<SetupCommand>,

    /// Skip all interactive prompts and accept defaults.
    #[arg(long, short = 'y', global = true)]
    pub yes: bool,

    /// Skip the skill phase when running the full setup flow.
    #[arg(long)]
    pub skip_skill: bool,

    /// Skip the dependency phase when running the full setup flow.
    #[arg(long)]
    pub skip_deps: bool,

    /// Pre-select a dependency group when running the full flow without prompts.
    #[arg(long, value_enum, default_value_t = DepsGroup::None)]
    pub deps: DepsGroup,

    /// Git ref (tag/branch/sha) to fetch the skill from when running outside
    /// a checkout (e.g. `v0.7.0` or `main`). Default: the running binary's
    /// release tag.
    #[arg(long, value_name = "REF")]
    pub git_ref: Option<String>,
}

#[derive(Subcommand, Debug)]
pub enum SetupCommand {
    /// Install or update the Claude Code skill (.claude/skills/symora).
    Skill(SkillArgs),
    /// Install language servers and ripgrep.
    Deps(DepsArgs),
}

#[derive(Serialize, Debug)]
struct SetupOutput {
    status: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    skill: Option<SkillOutcome>,
    #[serde(skip_serializing_if = "Option::is_none")]
    deps: Option<DepsOutcome>,
}

pub async fn execute(args: SetupArgs, app: &App) -> Result<()> {
    match args.command {
        Some(SetupCommand::Skill(skill_args)) => {
            let outcome = run_skill(skill_args, args.yes)?;
            let body = SetupOutput {
                status: "ok".to_string(),
                skill: Some(outcome),
                deps: None,
            };
            app.output.print_success(body);
        }
        Some(SetupCommand::Deps(deps_args)) => {
            let outcome = run_deps(deps_args, args.yes)?;
            let body = SetupOutput {
                status: "ok".to_string(),
                skill: None,
                deps: Some(outcome),
            };
            app.output.print_success(body);
        }
        None => {
            let mut skill_outcome = None;
            let mut deps_outcome = None;

            if !args.skip_skill {
                let skill_args = SkillArgs {
                    git_ref: args.git_ref.clone(),
                    no_backup: false,
                    force: false,
                };
                skill_outcome = Some(run_skill(skill_args, args.yes)?);
            }

            if !args.skip_deps {
                let deps_args = DepsArgs { group: args.deps };
                deps_outcome = Some(run_deps(deps_args, args.yes)?);
            }

            let body = SetupOutput {
                status: "ok".to_string(),
                skill: skill_outcome,
                deps: deps_outcome,
            };
            app.output.print_success(body);
        }
    }
    Ok(())
}
