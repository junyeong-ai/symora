//! `symora setup deps` — install ripgrep + LSP servers via the host's package
//! managers. The matrix here mirrors the install instructions surfaced by
//! `symora doctor` (`infra/lsp/servers.rs`); both should stay aligned.

use anyhow::Result;
use clap::{Args, ValueEnum};
use serde::Serialize;

use crate::cli::utils::ui::{Step, prompt, section, step};
use crate::infra::lsp::servers::Platform;
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

pub fn run_deps(args: DepsArgs, assume_yes: bool) -> Result<DepsOutcome> {
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
                items.push(install_rust_analyzer());
                items.push(install_typescript_lsp());
                items.push(install_pyright());
                items.push(install_gopls());
            }
            Slot::Jvm => {
                items.push(install_jdtls());
                items.push(install_kotlin_lsp());
            }
            Slot::Web => {
                items.push(install_vue_lsp());
                items.push(install_intelephense());
                items.push(install_yaml_lsp());
            }
            Slot::Systems => {
                items.push(install_clangd());
                items.push(install_zls());
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
    eprintln!("  [3] core+web        — adds Vue, PHP, YAML");
    eprintln!("  [4] core+systems    — adds C/C++, Zig");
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

fn already_installed(name: &'static str, command: &str) -> Option<DepResult> {
    if have(command) {
        step(Step::Skip, format!("{name}: already installed"));
        Some(DepResult {
            name,
            status: DepStatus::AlreadyInstalled,
            note: None,
        })
    } else {
        None
    }
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
    if let Some(r) = already_installed("ripgrep", "rg") {
        return r;
    }
    match Platform::current() {
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
    }
}

fn install_rust_analyzer() -> DepResult {
    if let Some(r) = already_installed("rust-analyzer", "rust-analyzer") {
        return r;
    }
    if !have("rustup") {
        return manual(
            "rust-analyzer",
            "install rustup first — see https://rustup.rs",
        );
    }
    run_install(
        "rust-analyzer",
        "rustup",
        &["component", "add", "rust-analyzer"],
    )
}

fn install_typescript_lsp() -> DepResult {
    if let Some(r) = already_installed("typescript-language-server", "typescript-language-server") {
        return r;
    }
    if !have("npm") {
        return manual("typescript-language-server", "install Node.js + npm");
    }
    run_install(
        "typescript-language-server",
        "npm",
        &["install", "-g", "typescript", "typescript-language-server"],
    )
}

fn install_pyright() -> DepResult {
    if let Some(r) = already_installed("pyright", "pyright") {
        return r;
    }
    if !have("npm") {
        return manual("pyright", "install Node.js + npm");
    }
    run_install("pyright", "npm", &["install", "-g", "pyright"])
}

fn install_gopls() -> DepResult {
    if let Some(r) = already_installed("gopls", "gopls") {
        return r;
    }
    if !have("go") {
        return manual("gopls", "install the Go toolchain");
    }
    run_install(
        "gopls",
        "go",
        &["install", "golang.org/x/tools/gopls@latest"],
    )
}

fn install_jdtls() -> DepResult {
    if let Some(r) = already_installed("jdtls", "jdtls") {
        return r;
    }
    match Platform::current() {
        Platform::MacOS if have("brew") => run_install("jdtls", "brew", &["install", "jdtls"]),
        _ => manual("jdtls", "see https://download.eclipse.org/jdtls/snapshots/"),
    }
}

fn install_kotlin_lsp() -> DepResult {
    if let Some(r) = already_installed("kotlin-lsp", "kotlin-lsp") {
        return r;
    }
    match Platform::current() {
        Platform::MacOS if have("brew") => run_install(
            "kotlin-lsp",
            "brew",
            &["install", "JetBrains/utils/kotlin-lsp"],
        ),
        _ => manual(
            "kotlin-lsp",
            "see https://github.com/JetBrains/kotlin-lsp/releases",
        ),
    }
}

fn install_vue_lsp() -> DepResult {
    if let Some(r) = already_installed("vue-language-server", "vue-language-server") {
        return r;
    }
    if !have("npm") {
        return manual("vue-language-server", "install Node.js + npm");
    }
    run_install(
        "vue-language-server",
        "npm",
        &["install", "-g", "@vue/language-server"],
    )
}

fn install_intelephense() -> DepResult {
    if let Some(r) = already_installed("intelephense", "intelephense") {
        return r;
    }
    if !have("npm") {
        return manual("intelephense", "install Node.js + npm");
    }
    run_install("intelephense", "npm", &["install", "-g", "intelephense"])
}

fn install_yaml_lsp() -> DepResult {
    if let Some(r) = already_installed("yaml-language-server", "yaml-language-server") {
        return r;
    }
    if !have("npm") {
        return manual("yaml-language-server", "install Node.js + npm");
    }
    run_install(
        "yaml-language-server",
        "npm",
        &["install", "-g", "yaml-language-server"],
    )
}

fn install_clangd() -> DepResult {
    if let Some(r) = already_installed("clangd", "clangd") {
        return r;
    }
    match Platform::current() {
        Platform::MacOS if have("brew") => run_install("clangd", "brew", &["install", "llvm"]),
        Platform::Linux if have("apt-get") => {
            run_install("clangd", "sudo", &["apt-get", "install", "-y", "clangd"])
        }
        Platform::Linux if have("dnf") => run_install(
            "clangd",
            "sudo",
            &["dnf", "install", "-y", "clang-tools-extra"],
        ),
        _ => manual("clangd", "install via your distro's package manager"),
    }
}

fn install_zls() -> DepResult {
    if let Some(r) = already_installed("zls", "zls") {
        return r;
    }
    match Platform::current() {
        Platform::MacOS if have("brew") => run_install("zls", "brew", &["install", "zls"]),
        _ => manual("zls", "see https://github.com/zigtools/zls/releases"),
    }
}
