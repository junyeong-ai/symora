//! `symora setup deps` — install ripgrep + LSP servers via the host's package
//! managers. The matrix here mirrors the install instructions surfaced by
//! `symora doctor` (`infra/lsp/servers.rs`); both should stay aligned.

use std::collections::HashMap;

use anyhow::Result;
use clap::{Args, ValueEnum};
use serde::Serialize;

use crate::cli::utils::ui::{Step, prompt, section, step};
use crate::infra::lsp::servers::{Platform, ServerConfig, ServerSource};
use crate::models::symbol::Language;
use crate::services::dist::process::{have, run_streaming};

#[derive(Args, Debug)]
pub struct DepsArgs {
    /// Pre-select a dependency group; skips the prompt.
    #[arg(long, value_enum, default_value_t = DepsGroup::None)]
    pub group: DepsGroup,
}

#[derive(ValueEnum, Clone, Copy, Debug, Serialize, PartialEq, Eq)]
#[serde(rename_all = "kebab-case")]
#[clap(rename_all = "kebab-case")]
pub enum DepsGroup {
    None,
    Core,
    CoreJvm,
    CoreWeb,
    CoreSystems,
    All,
}

impl DepsGroup {
    fn includes(self) -> &'static [Slot] {
        match self {
            DepsGroup::None => &[],
            DepsGroup::Core => &[Slot::Ripgrep, Slot::Core],
            DepsGroup::CoreJvm => &[Slot::Ripgrep, Slot::Core, Slot::Jvm],
            DepsGroup::CoreWeb => &[Slot::Ripgrep, Slot::Core, Slot::Web],
            DepsGroup::CoreSystems => &[Slot::Ripgrep, Slot::Core, Slot::Systems],
            DepsGroup::All => &[
                Slot::Ripgrep,
                Slot::Core,
                Slot::Jvm,
                Slot::Web,
                Slot::Systems,
            ],
        }
    }
}

#[derive(Copy, Clone)]
enum Slot {
    Ripgrep,
    Core,
    Jvm,
    Web,
    Systems,
}

#[derive(Serialize, Debug)]
pub struct DepsOutcome {
    pub group: DepsGroup,
    pub count: usize,
    pub showing: usize,
    pub items: Vec<DepResult>,
    pub truncated: bool,
}

#[derive(Serialize, Debug)]
pub struct DepResult {
    pub name: &'static str,
    pub status: DepStatus,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub note: Option<String>,
}

#[derive(Serialize, Debug, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum DepStatus {
    /// Tool is already on `$PATH` — nothing to do.
    AlreadyInstalled,
    /// Tool was installed by this run.
    Installed,
    /// Install command failed; `note` carries the underlying error.
    Failed,
    /// No automated install path on this platform; `note` points at docs.
    Manual,
}

pub fn run_deps(
    args: DepsArgs,
    assume_yes: bool,
    server_configs: &HashMap<Language, ServerConfig>,
) -> Result<DepsOutcome> {
    section("setup deps");

    let group = if args.group == DepsGroup::None {
        choose_group_interactive(assume_yes)
    } else {
        args.group
    };

    if matches!(group, DepsGroup::None) {
        step(
            Step::Skip,
            "no dependencies selected — run 'symora doctor' for hints",
        );
        return Ok(DepsOutcome {
            group,
            count: 0,
            showing: 0,
            items: Vec::new(),
            truncated: false,
        });
    }

    let mut items: Vec<DepResult> = Vec::new();
    for slot in group.includes() {
        match slot {
            Slot::Ripgrep => items.push(install_ripgrep()),
            Slot::Core => {
                items.push(install_rust_analyzer(&server_configs[&Language::Rust]));
                items.push(install_typescript_lsp(
                    &server_configs[&Language::TypeScript],
                ));
                items.push(install_pyright(&server_configs[&Language::Python]));
                items.push(install_gopls(&server_configs[&Language::Go]));
            }
            Slot::Jvm => {
                items.push(install_jdtls(&server_configs[&Language::Java]));
                items.push(install_kotlin_lsp(&server_configs[&Language::Kotlin]));
            }
            Slot::Web => {
                items.push(install_vue_lsp(&server_configs[&Language::Vue]));
                items.push(install_intelephense(&server_configs[&Language::PHP]));
                items.push(install_yaml_lsp(&server_configs[&Language::Yaml]));
                items.push(install_ruby_lsp(&server_configs[&Language::Ruby]));
            }
            Slot::Systems => {
                items.push(install_clangd(&server_configs[&Language::Cpp]));
                items.push(install_zls(&server_configs[&Language::Zig]));
                items.push(install_csharp_lsp(&server_configs[&Language::CSharp]));
            }
        }
    }

    let count = items.len();
    Ok(DepsOutcome {
        group,
        count,
        showing: count,
        items,
        truncated: false,
    })
}

fn choose_group_interactive(assume_yes: bool) -> DepsGroup {
    if assume_yes || !crate::cli::utils::ui::stdin_is_tty() {
        return DepsGroup::None;
    }
    eprintln!("  [1] core            — ripgrep + Rust/TS/Python/Go LSPs");
    eprintln!("  [2] core+jvm        — adds Java, Kotlin");
    eprintln!("  [3] core+web        — adds Vue, PHP, YAML, Ruby");
    eprintln!("  [4] core+systems    — adds C/C++, Zig, C#");
    eprintln!("  [5] all             — every supported LSP");
    eprintln!("  [6] skip");

    let raw = prompt("  choose [1-6] (default 6): ", "6", false);
    match raw.trim() {
        "1" => DepsGroup::Core,
        "2" => DepsGroup::CoreJvm,
        "3" => DepsGroup::CoreWeb,
        "4" => DepsGroup::CoreSystems,
        "5" => DepsGroup::All,
        _ => DepsGroup::None,
    }
}

/// Pre-install gate evaluated against the same merged table the spawn path
/// uses, so AlreadyInstalled always means "symora can spawn it".
/// None = proceed with the platform install commands.
fn check_server(config: &ServerConfig) -> Option<DepResult> {
    if config.is_installed() {
        step(
            Step::Skip,
            format!("{}: already installed", config.display_name),
        );
        return Some(DepResult {
            name: config.display_name,
            status: DepStatus::AlreadyInstalled,
            note: None,
        });
    }
    if config.source == ServerSource::Config {
        return Some(manual(
            config.display_name,
            "command is overridden by [lsp.servers] in symora config — installing the \
             stock server would not change what symora spawns; fix the override path or \
             remove the override first",
        ));
    }
    None
}

fn run_install(name: &'static str, program: &str, args: &[&str]) -> DepResult {
    step(
        Step::Run,
        format!("installing {name} via `{program} {}`", args.join(" ")),
    );
    match run_streaming(program, args) {
        Ok(()) => {
            step(Step::Ok, format!("{name} installed"));
            DepResult {
                name,
                status: DepStatus::Installed,
                note: None,
            }
        }
        Err(err) => {
            step(Step::Warn, format!("{name}: {err}"));
            DepResult {
                name,
                status: DepStatus::Failed,
                note: Some(err.to_string()),
            }
        }
    }
}

/// Install a language server, then VERIFY symora can actually spawn it. An
/// installer exiting 0 is not proof: a binary placed on a PATH this process
/// never had, or a partial install, would otherwise be reported `Installed`
/// though symora still cannot use it — a plausible-but-wrong status (invariant
/// 4). Re-probing the same resolution the spawn path uses turns a
/// not-yet-resolvable server into an honest `Failed` with a recovery hint.
fn run_install_server(config: &ServerConfig, program: &str, args: &[&str]) -> DepResult {
    let result = run_install(config.display_name, program, args);
    if result.status != DepStatus::Installed || config.is_installed() {
        return result;
    }
    step(
        Step::Warn,
        format!(
            "{}: install reported success but the server is not resolvable",
            config.display_name
        ),
    );
    DepResult {
        name: config.display_name,
        status: DepStatus::Failed,
        note: Some(
            "install command succeeded but symora still cannot resolve the server on PATH — \
             open a new shell (and run `symora daemon restart`), then re-check with `symora doctor`"
                .to_string(),
        ),
    }
}

/// Emit a "no automated path here, do it yourself" record. By convention
/// `hint` reads as a complete sentence beginning with an imperative verb
/// (`install …`, `run …`, `see …`) so the JSON `note` field renders cleanly
/// when surfaced to an agent or end user.
fn manual(name: &'static str, hint: &str) -> DepResult {
    step(Step::Warn, format!("{name}: manual install — {hint}"));
    DepResult {
        name,
        status: DepStatus::Manual,
        note: Some(hint.to_string()),
    }
}

// ─── individual installers ──────────────────────────────────────────────

fn install_ripgrep() -> DepResult {
    if have("rg") {
        step(Step::Skip, "ripgrep: already installed");
        return DepResult {
            name: "ripgrep",
            status: DepStatus::AlreadyInstalled,
            note: None,
        };
    }
    let result = match Platform::current() {
        Platform::MacOS if have("brew") => run_install("ripgrep", "brew", &["install", "ripgrep"]),
        Platform::MacOS => manual("ripgrep", "install Homebrew or run 'cargo install ripgrep'"),
        Platform::Linux if have("apt-get") => {
            run_install("ripgrep", "sudo", &["apt-get", "install", "-y", "ripgrep"])
        }
        Platform::Linux if have("dnf") => {
            run_install("ripgrep", "sudo", &["dnf", "install", "-y", "ripgrep"])
        }
        Platform::Linux if have("pacman") => run_install(
            "ripgrep",
            "sudo",
            &["pacman", "-S", "--noconfirm", "ripgrep"],
        ),
        Platform::Linux if have("cargo") => {
            run_install("ripgrep", "cargo", &["install", "ripgrep"])
        }
        Platform::Linux => manual("ripgrep", "install via your distro's package manager"),
        Platform::Windows => manual(
            "ripgrep",
            "see https://github.com/BurntSushi/ripgrep/releases",
        ),
    };
    verify_on_path(result, "rg")
}

/// Post-install verification for a tool resolved by bare command name (ripgrep)
/// rather than a ServerConfig: an installer exiting 0 is not proof `rg` is on
/// this process's PATH, so re-probe and report an honest Failed otherwise —
/// the same discipline `run_install_server` applies to language servers.
fn verify_on_path(result: DepResult, program: &str) -> DepResult {
    if result.status != DepStatus::Installed || have(program) {
        return result;
    }
    step(
        Step::Warn,
        format!(
            "{}: install reported success but `{program}` is not resolvable",
            result.name
        ),
    );
    DepResult {
        name: result.name,
        status: DepStatus::Failed,
        note: Some(format!(
            "install command succeeded but `{program}` is still not on PATH — open a new shell, \
             then re-check"
        )),
    }
}

fn install_rust_analyzer(config: &ServerConfig) -> DepResult {
    if let Some(r) = check_server(config) {
        return r;
    }
    if !have("rustup") {
        return manual(
            "rust-analyzer",
            "install rustup first — see https://rustup.rs",
        );
    }
    run_install_server(config, "rustup", &["component", "add", "rust-analyzer"])
}

fn install_typescript_lsp(config: &ServerConfig) -> DepResult {
    if let Some(r) = check_server(config) {
        return r;
    }
    if !have("npm") {
        return manual("typescript-language-server", "install Node.js + npm");
    }
    run_install_server(
        config,
        "npm",
        &["install", "-g", "typescript", "typescript-language-server"],
    )
}

fn install_pyright(config: &ServerConfig) -> DepResult {
    if let Some(r) = check_server(config) {
        return r;
    }
    if !have("npm") {
        return manual("pyright", "install Node.js + npm");
    }
    run_install_server(config, "npm", &["install", "-g", "pyright"])
}

fn install_gopls(config: &ServerConfig) -> DepResult {
    if let Some(r) = check_server(config) {
        return r;
    }
    if !have("go") {
        return manual("gopls", "install the Go toolchain");
    }
    run_install_server(
        config,
        "go",
        &["install", "golang.org/x/tools/gopls@latest"],
    )
}

fn install_jdtls(config: &ServerConfig) -> DepResult {
    if let Some(r) = check_server(config) {
        return r;
    }
    match Platform::current() {
        Platform::MacOS if have("brew") => {
            run_install_server(config, "brew", &["install", "jdtls"])
        }
        _ => manual("jdtls", "see https://download.eclipse.org/jdtls/snapshots/"),
    }
}

fn install_kotlin_lsp(config: &ServerConfig) -> DepResult {
    if let Some(r) = check_server(config) {
        return r;
    }
    match Platform::current() {
        Platform::MacOS if have("brew") => {
            run_install_server(config, "brew", &["install", "JetBrains/utils/kotlin-lsp"])
        }
        _ => manual(
            "kotlin-lsp",
            "see https://github.com/JetBrains/kotlin-lsp/releases",
        ),
    }
}

fn install_vue_lsp(config: &ServerConfig) -> DepResult {
    if let Some(r) = check_server(config) {
        return r;
    }
    if !have("npm") {
        return manual("vue-language-server", "install Node.js + npm");
    }
    run_install_server(config, "npm", &["install", "-g", "@vue/language-server"])
}

fn install_intelephense(config: &ServerConfig) -> DepResult {
    if let Some(r) = check_server(config) {
        return r;
    }
    if !have("npm") {
        return manual("intelephense", "install Node.js + npm");
    }
    run_install_server(config, "npm", &["install", "-g", "intelephense"])
}

fn install_yaml_lsp(config: &ServerConfig) -> DepResult {
    if let Some(r) = check_server(config) {
        return r;
    }
    if !have("npm") {
        return manual("yaml-language-server", "install Node.js + npm");
    }
    run_install_server(config, "npm", &["install", "-g", "yaml-language-server"])
}

fn install_clangd(config: &ServerConfig) -> DepResult {
    if let Some(r) = check_server(config) {
        return r;
    }
    match Platform::current() {
        Platform::MacOS if have("brew") => run_install_server(config, "brew", &["install", "llvm"]),
        Platform::Linux if have("apt-get") => {
            run_install_server(config, "sudo", &["apt-get", "install", "-y", "clangd"])
        }
        Platform::Linux if have("dnf") => run_install_server(
            config,
            "sudo",
            &["dnf", "install", "-y", "clang-tools-extra"],
        ),
        _ => manual("clangd", "install via your distro's package manager"),
    }
}

fn install_zls(config: &ServerConfig) -> DepResult {
    if let Some(r) = check_server(config) {
        return r;
    }
    match Platform::current() {
        Platform::MacOS if have("brew") => run_install_server(config, "brew", &["install", "zls"]),
        _ => manual("zls", "see https://github.com/zigtools/zls/releases"),
    }
}

fn install_ruby_lsp(config: &ServerConfig) -> DepResult {
    if let Some(r) = check_server(config) {
        return r;
    }
    if !have("gem") {
        return manual("ruby-lsp", "install Ruby first (it provides `gem`)");
    }
    run_install_server(config, "gem", &["install", "ruby-lsp"])
}

fn install_csharp_lsp(config: &ServerConfig) -> DepResult {
    if let Some(r) = check_server(config) {
        return r;
    }
    if !have("dotnet") {
        return manual(
            "csharp-ls",
            "install the .NET SDK first (it provides `dotnet`)",
        );
    }
    run_install_server(config, "dotnet", &["tool", "install", "-g", "csharp-ls"])
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::infra::lsp::servers::InstallInstructions;
    use crate::models::config::ServerTier;

    fn server(command: String, source: ServerSource) -> ServerConfig {
        ServerConfig {
            display_name: "fake-ls",
            command,
            args: vec![],
            version_arg: "--version",
            version_command: None,
            install: InstallInstructions {
                macos: "none",
                linux: "none",
                windows: "none",
            },
            tier: ServerTier::Fast,
            source,
        }
    }

    #[test]
    fn check_server_none_for_missing_builtin() {
        let config = server("/nonexistent/fake-ls".to_string(), ServerSource::Builtin);
        assert!(check_server(&config).is_none());
    }

    #[test]
    fn check_server_already_installed_for_resolvable_command() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("fake-ls");
        std::fs::write(&path, "#!/bin/sh\nexit 0\n").unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
        }
        let config = server(path.to_string_lossy().into_owned(), ServerSource::Builtin);
        let result = check_server(&config).unwrap();
        assert_eq!(result.status, DepStatus::AlreadyInstalled);
    }

    #[test]
    fn check_server_blocks_install_when_override_missing() {
        let config = server("/nonexistent/fake-ls".to_string(), ServerSource::Config);
        let result = check_server(&config).unwrap();
        assert_eq!(result.status, DepStatus::Manual);
        assert!(result.note.unwrap().contains("lsp.servers"));
    }

    #[test]
    fn install_downgrades_to_failed_when_server_still_unresolvable() {
        // The install command "succeeds" (`true` exits 0), but symora still
        // cannot resolve the server — an honest Failed, never a plausible
        // Installed that the spawn path would then contradict.
        let config = server("/nonexistent/fake-ls".to_string(), ServerSource::Builtin);
        let result = run_install_server(&config, "true", &[]);
        assert_eq!(result.status, DepStatus::Failed);
        assert!(result.note.unwrap().contains("cannot resolve"));
    }
}
