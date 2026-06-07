//! `symora setup mcp` — wire the `symora mcp serve` MCP server into every
//! installed agent host from one command, idempotently and reversibly.
//!
//! CLI-only by design (like the rest of `setup`): it mutates the user's
//! machine outside the project boundary, so it is never exposed as an MCP
//! tool. It is a deliberate, explicit step — bare `symora setup` does not
//! touch host configs.

mod host;
mod registry;
mod writers;

use anyhow::Result;
use clap::Args;
use serde::Serialize;

use crate::app::App;
use crate::cli::response::Section;
use crate::services::dist::{display as display_path, home};
use host::{Env, FileAction, HostOutcome, HostTarget, ServerSpec};

#[derive(Args, Debug)]
pub struct McpSetupArgs {
    /// Limit to specific hosts (repeatable, e.g. `--host claude_code`).
    /// Default: every host detected as installed.
    #[arg(long = "host", value_name = "ID")]
    pub hosts: Vec<String>,

    /// Write to every supported host, even ones not detected as installed.
    /// Mutually exclusive with `--host`.
    #[arg(long, conflicts_with = "hosts")]
    pub all: bool,

    /// Remove the symora entry instead of writing it (reverses install).
    #[arg(long)]
    pub uninstall: bool,

    /// Report what would change without writing anything.
    #[arg(long)]
    pub dry_run: bool,
}

#[derive(Serialize, Debug)]
pub struct McpSetupOutcome {
    pub uninstall: bool,
    pub dry_run: bool,
    #[serde(flatten)]
    pub hosts: Section<HostOutcome>,
}

pub fn run_mcp(args: McpSetupArgs, app: &App) -> Result<McpSetupOutcome> {
    let env = Env {
        home: home()?,
        project_root: app.root().to_path_buf(),
    };

    let selected = select_hosts(&args.hosts)?;
    // Explicit selection (or --all) acts regardless of detection; a bare
    // auto-run only touches hosts that are actually installed, so it never
    // fabricates a config for an agent the user doesn't have.
    let explicit = !args.hosts.is_empty() || args.all;

    let spec = ServerSpec {
        command: symora_binary()?,
        args: vec!["mcp".to_string(), "serve".to_string()],
    };

    let mut items = Vec::new();
    for host in selected {
        let detected = host.detect(&env);
        if !explicit && !detected {
            continue;
        }
        items.push(act_on_host(host, &env, &spec, &args, detected));
    }

    Ok(McpSetupOutcome {
        uninstall: args.uninstall,
        dry_run: args.dry_run,
        hosts: Section::new(items),
    })
}

fn select_hosts(requested: &[String]) -> Result<Vec<&'static dyn HostTarget>> {
    if requested.is_empty() {
        return Ok(registry::all().to_vec());
    }
    requested
        .iter()
        .map(|id| {
            registry::find(id).ok_or_else(|| {
                // A bad `--host` value is invalid input, not an internal
                // failure — surface it with the branchable code and the
                // valid set so the agent can correct and retry.
                anyhow::Error::new(crate::cli::OutputError::invalid(format!(
                    "unknown host `{id}`; valid hosts: {}",
                    registry::ids().join(", ")
                )))
            })
        })
        .collect()
}

fn act_on_host(
    host: &'static dyn HostTarget,
    env: &Env,
    spec: &ServerSpec,
    args: &McpSetupArgs,
    detected: bool,
) -> HostOutcome {
    let path = host.config_path(env);
    let result = if args.uninstall {
        host.uninstall(env, args.dry_run)
    } else {
        host.install(env, spec, args.dry_run)
    };
    match result {
        Ok(action) => HostOutcome {
            host: host.id(),
            action,
            config_path: Some(display_path(&path)),
            detected,
            error: None,
        },
        // A parse/write failure leaves the host's config untouched — skip it
        // and report why, never clobber a config we couldn't understand.
        Err(e) => HostOutcome {
            host: host.id(),
            action: FileAction::Skipped,
            config_path: Some(display_path(&path)),
            detected,
            error: Some(e.into()),
        },
    }
}

/// Absolute path to the running binary — reliable regardless of the host's
/// spawn `$PATH`, and stable across re-runs for a given install. This is the
/// same source `daemon` and `self update` use to refer to the binary.
fn symora_binary() -> Result<String> {
    let exe = std::env::current_exe()?;
    Ok(exe.to_string_lossy().into_owned())
}

#[cfg(test)]
mod tests {
    use super::host::{Env, FileAction};
    use super::*;

    fn spec() -> ServerSpec {
        ServerSpec {
            command: "/abs/symora".to_string(),
            args: vec!["mcp".to_string(), "serve".to_string()],
        }
    }

    /// Build an `Env` whose HOME and project both live under a temp dir, so
    /// every host write lands in the sandbox and never the real machine.
    fn sandbox() -> (tempfile::TempDir, Env) {
        let dir = tempfile::tempdir().unwrap();
        let home = dir.path().join("home");
        let project = dir.path().join("project");
        std::fs::create_dir_all(&home).unwrap();
        std::fs::create_dir_all(&project).unwrap();
        let env = Env {
            home,
            project_root: project,
        };
        (dir, env)
    }

    #[test]
    fn install_then_uninstall_round_trips_every_host() {
        let (_dir, env) = sandbox();
        let spec = spec();
        for host in registry::all() {
            // Install on a clean host creates the config and is detectable.
            let action = host.install(&env, &spec, false).unwrap();
            assert_eq!(action, FileAction::Created, "{}", host.id());
            assert!(host.config_path(&env).exists(), "{}", host.id());

            // Re-install is a no-op with byte-identical config.
            let before = std::fs::read(host.config_path(&env)).unwrap();
            assert_eq!(
                host.install(&env, &spec, false).unwrap(),
                FileAction::Unchanged
            );
            let after = std::fs::read(host.config_path(&env)).unwrap();
            assert_eq!(before, after, "{} re-install changed bytes", host.id());

            // Uninstall removes our entry.
            assert_eq!(host.uninstall(&env, false).unwrap(), FileAction::Removed);

            // Uninstall again finds nothing.
            assert_eq!(host.uninstall(&env, false).unwrap(), FileAction::NotFound);
        }
    }

    #[test]
    fn uninstall_preserves_unrelated_config() {
        let (_dir, env) = sandbox();
        // Seed each host's config with a foreign entry, then install+uninstall
        // symora and confirm the foreign entry survives.
        let claude_path = registry::find("claude_code").unwrap().config_path(&env);
        std::fs::write(
            &claude_path,
            r#"{"mcpServers":{"other":{"command":"x","args":[]}}}"#,
        )
        .unwrap();
        let codex_path = registry::find("codex").unwrap().config_path(&env);
        std::fs::create_dir_all(codex_path.parent().unwrap()).unwrap();
        std::fs::write(&codex_path, "model = \"gpt-5\"\n").unwrap();

        for host in registry::all() {
            host.install(&env, &spec(), false).unwrap();
            host.uninstall(&env, false).unwrap();
        }

        let claude = std::fs::read_to_string(&claude_path).unwrap();
        assert!(
            claude.contains("\"other\""),
            "foreign mcp server must survive"
        );
        assert!(!claude.contains("symora"));
        let codex = std::fs::read_to_string(&codex_path).unwrap();
        assert!(
            codex.contains("model = \"gpt-5\""),
            "foreign toml must survive"
        );
        assert!(!codex.contains("symora"));
    }

    #[test]
    fn dry_run_writes_nothing() {
        let (_dir, env) = sandbox();
        for host in registry::all() {
            let action = host.install(&env, &spec(), true).unwrap();
            assert_eq!(
                action,
                FileAction::Created,
                "{} reports the intended action",
                host.id()
            );
            assert!(
                !host.config_path(&env).exists(),
                "{} must not write on dry-run",
                host.id()
            );
        }
    }

    #[test]
    fn the_project_config_is_never_a_detection_signal() {
        let (_dir, env) = sandbox();
        let claude = registry::find("claude_code").unwrap();
        // Claude's project `.mcp.json` lives outside its detection inputs
        // (`~/.claude` + the `claude` binary on PATH), so writing it must not
        // change whether the host is considered installed — regardless of
        // whether `claude` happens to be on the test machine's PATH.
        let before = claude.detect(&env);
        claude.install(&env, &spec(), false).unwrap();
        assert_eq!(
            claude.detect(&env),
            before,
            "writing .mcp.json must not flip detection"
        );

        // The host's own directory is a valid positive signal.
        std::fs::create_dir_all(env.home.join(".claude")).unwrap();
        assert!(claude.detect(&env));
    }

    #[test]
    fn unknown_host_is_rejected_with_the_valid_set() {
        // `Vec<&dyn HostTarget>` isn't `Debug`, so match rather than unwrap.
        let err = match select_hosts(&["nope".to_string()]) {
            Ok(_) => panic!("an unknown host id must be rejected"),
            Err(e) => e.to_string(),
        };
        assert!(err.contains("unknown host `nope`"));
        assert!(err.contains("claude_code"));
    }

    #[test]
    fn claude_uninstall_deletes_an_emptied_file_but_codex_keeps_it() {
        let (_dir, env) = sandbox();
        // Claude's project file is ours: emptying it deletes it.
        let claude = registry::find("claude_code").unwrap();
        claude.install(&env, &spec(), false).unwrap();
        claude.uninstall(&env, false).unwrap();
        assert!(
            !claude.config_path(&env).exists(),
            "an emptied project .mcp.json should be removed"
        );

        // Codex's user config is shared: it stays even if only blank remains.
        let codex = registry::find("codex").unwrap();
        codex.install(&env, &spec(), false).unwrap();
        codex.uninstall(&env, false).unwrap();
        assert!(
            codex.config_path(&env).exists(),
            "the shared codex config must never be deleted"
        );
    }
}
