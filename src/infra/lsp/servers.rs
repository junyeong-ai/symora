use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Duration;

use crate::error::LspError;
use crate::models::symbol::Language;

// Server Performance Tiers

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ServerTier {
    /// Fast servers (< 15s init): rust-analyzer, clangd, gopls
    Fast,
    /// Standard servers (15-45s init): intelephense, kotlin-ls, ruby-lsp
    Standard,
    /// Slow servers (45-120s init): pyright, typescript-language-server, jdtls
    Slow,
}

impl ServerTier {
    pub fn init_timeout(&self) -> Duration {
        match self {
            Self::Fast => Duration::from_secs(15),
            Self::Standard => Duration::from_secs(45),
            Self::Slow => Duration::from_secs(120),
        }
    }

    pub fn request_timeout(&self) -> Duration {
        match self {
            Self::Fast => Duration::from_secs(15),
            Self::Standard => Duration::from_secs(30),
            Self::Slow => Duration::from_secs(60),
        }
    }

    pub fn cross_file_timeout(&self) -> Duration {
        match self {
            Self::Fast => Duration::from_secs(20),
            Self::Standard => Duration::from_secs(45),
            Self::Slow => Duration::from_secs(90),
        }
    }

    pub fn shutdown_timeout(&self) -> Duration {
        Duration::from_secs(5)
    }
}

// Platform Detection

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Platform {
    MacOS,
    Linux,
    Windows,
}

impl Platform {
    pub fn current() -> Self {
        if cfg!(target_os = "macos") {
            Self::MacOS
        } else if cfg!(target_os = "windows") {
            Self::Windows
        } else {
            Self::Linux
        }
    }
}

// Server Configuration

#[derive(Debug, Clone, Copy)]
pub struct InstallInstructions {
    pub macos: &'static str,
    pub linux: &'static str,
    pub windows: &'static str,
}

impl InstallInstructions {
    pub fn current(&self) -> &'static str {
        match Platform::current() {
            Platform::MacOS => self.macos,
            Platform::Linux => self.linux,
            Platform::Windows => self.windows,
        }
    }
}

#[derive(Debug, Clone)]
pub struct ServerConfig {
    pub display_name: &'static str,
    pub command: &'static str,
    pub args: &'static [&'static str],
    pub install: InstallInstructions,
    pub version_arg: &'static str,
    /// Binary to run for the `doctor` version report when the stdio
    /// server entrypoint itself has no usable version flag (pyright's
    /// `pyright-langserver` rejects `--version`; the `pyright` CLI
    /// reports it). `None` = probe `command`.
    pub version_command: Option<&'static str>,
    pub tier: ServerTier,
}

impl ServerConfig {
    pub fn init_timeout(&self) -> Duration {
        self.tier.init_timeout()
    }

    pub fn request_timeout(&self) -> Duration {
        self.tier.request_timeout()
    }

    pub fn cross_file_timeout(&self) -> Duration {
        self.tier.cross_file_timeout()
    }

    /// Resolve `command` to an absolute, spawnable executable path.
    ///
    /// One deterministic search shared by spawn, `server_status`, and
    /// `doctor`, so "reported installed" and "actually spawnable" can
    /// never disagree. Search order:
    ///
    /// 1. `command` as an absolute path (taken as-is when it exists).
    /// 2. Every directory on the inherited `PATH`.
    /// 3. Fixed, version-free well-known install directories — this is
    ///    what keeps npm/cargo-installed servers reachable when the
    ///    process inherited a thin GUI-session `PATH`. Deliberately no
    ///    globbing (`~/.nvm/versions/node/*/bin` would pick an arbitrary
    ///    toolchain); version-managed setups are named in the error hint
    ///    instead.
    ///
    /// Failure is loud: the error hint lists every directory searched
    /// plus the install instruction.
    pub fn resolve(&self) -> Result<PathBuf, LspError> {
        resolve_command(self.command).map_err(|searched| LspError::ServerNotInstalled {
            name: self.display_name.to_string(),
            install_hint: not_found_hint(self.install.current(), &searched),
        })
    }

    pub fn is_installed(&self) -> bool {
        self.resolve().is_ok()
    }

    /// Version string for `doctor` reports.
    ///
    /// Runs the version probe (`version_command` when the stdio server
    /// binary has no usable `--version`, e.g. pyright-langserver probes
    /// the `pyright` CLI instead) with a hard timeout — an LSP stdio
    /// entrypoint handed an unknown flag can block on stdin, and a
    /// diagnostic must never hang the report.
    pub fn probe_version(&self) -> Option<String> {
        let probe = self.version_command.unwrap_or(self.command);
        let path = resolve_command(probe).ok()?;
        let output = run_with_timeout(&path, self.version_arg, Duration::from_secs(2))?;
        if !output.status.success() {
            // A failed probe blanks the version column; it must never
            // surface an error line as if it were a version string.
            return None;
        }

        let stdout = String::from_utf8_lossy(&output.stdout);
        let stderr = String::from_utf8_lossy(&output.stderr);

        let text = if stdout.trim().is_empty() {
            stderr.to_string()
        } else {
            stdout.to_string()
        };

        text.lines()
            .find(|line| !line.trim().is_empty())
            .map(|s| s.trim().to_string())
    }
}

/// Resolve a command name to an absolute executable path, or return the
/// full list of directories searched (for the loud failure hint).
fn resolve_command(command: &str) -> Result<PathBuf, Vec<PathBuf>> {
    let as_path = Path::new(command);
    if as_path.is_absolute() {
        return if is_executable_file(as_path) {
            Ok(as_path.to_path_buf())
        } else {
            Err(as_path
                .parent()
                .map(Path::to_path_buf)
                .into_iter()
                .collect())
        };
    }

    let mut dirs: Vec<PathBuf> = std::env::var_os("PATH")
        .map(|path| std::env::split_paths(&path).collect())
        .unwrap_or_default();
    for well_known in well_known_dirs() {
        if !dirs.contains(&well_known) {
            dirs.push(well_known);
        }
    }

    search_dirs(command, &dirs)
}

fn search_dirs(command: &str, dirs: &[PathBuf]) -> Result<PathBuf, Vec<PathBuf>> {
    for dir in dirs {
        for name in candidate_names(command) {
            let candidate = dir.join(name);
            if is_executable_file(&candidate) {
                return Ok(candidate);
            }
        }
    }
    Err(dirs.to_vec())
}

/// Fixed, version-free directories where language servers commonly land
/// outside the inherited `PATH`. Deliberately no globbing: a `*` over a
/// version manager's tree (nvm/pyenv/asdf) picks an arbitrary toolchain.
fn well_known_dirs() -> Vec<PathBuf> {
    let mut dirs: Vec<PathBuf> = match Platform::current() {
        Platform::MacOS => vec!["/opt/homebrew/bin".into(), "/usr/local/bin".into()],
        Platform::Linux => vec!["/usr/local/bin".into(), "/usr/bin".into()],
        Platform::Windows => vec![],
    };
    if let Some(home) = std::env::var_os("HOME").map(PathBuf::from) {
        dirs.push(home.join(".local/bin"));
        dirs.push(home.join(".cargo/bin"));
    }
    dirs
}

/// Spawnable filenames for a bare command. On Windows npm ships `.cmd`
/// shims and compiled tools ship `.exe` — a fixed list, no PATHEXT walk.
fn candidate_names(command: &str) -> Vec<String> {
    if Platform::current() == Platform::Windows {
        vec![
            format!("{command}.exe"),
            format!("{command}.cmd"),
            format!("{command}.bat"),
            command.to_string(),
        ]
    } else {
        vec![command.to_string()]
    }
}

fn is_executable_file(path: &Path) -> bool {
    if !path.is_file() {
        return false;
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        path.metadata()
            .is_ok_and(|m| m.permissions().mode() & 0o111 != 0)
    }
    #[cfg(not(unix))]
    {
        true
    }
}

fn not_found_hint(install: &str, searched: &[PathBuf]) -> String {
    let mut hint = format!(
        "{install}. Searched: {}",
        searched
            .iter()
            .map(|d| d.display().to_string())
            .collect::<Vec<_>>()
            .join(", ")
    );
    if let Some(home) = std::env::var_os("HOME").map(PathBuf::from)
        && home.join(".nvm").is_dir()
    {
        hint.push_str(
            ". nvm detected: nvm-managed binaries are not on the daemon's PATH — \
             run `nvm use` in your shell or symlink the binary into ~/.local/bin",
        );
    }
    hint
}

/// `Command::output()` with a hard deadline; kills the child on timeout.
fn run_with_timeout(path: &Path, arg: &str, timeout: Duration) -> Option<std::process::Output> {
    let mut child = Command::new(path)
        .arg(arg)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .ok()?;

    let deadline = std::time::Instant::now() + timeout;
    loop {
        match child.try_wait() {
            Ok(Some(_)) => return child.wait_with_output().ok(),
            Ok(None) if std::time::Instant::now() >= deadline => {
                let _ = child.kill();
                let _ = child.wait();
                return None;
            }
            Ok(None) => std::thread::sleep(Duration::from_millis(25)),
            Err(_) => {
                let _ = child.kill();
                return None;
            }
        }
    }
}

/// Default server configurations for all supported languages
pub fn defaults() -> HashMap<Language, ServerConfig> {
    let mut configs = HashMap::new();

    configs.insert(
        Language::Rust,
        ServerConfig {
            display_name: "rust-analyzer",
            command: "rust-analyzer",
            args: &[],
            version_arg: "--version",
            version_command: None,
            install: InstallInstructions {
                macos: "rustup component add rust-analyzer",
                linux: "rustup component add rust-analyzer",
                windows: "rustup component add rust-analyzer",
            },
            tier: ServerTier::Fast,
        },
    );

    configs.insert(
        Language::Cpp,
        ServerConfig {
            display_name: "clangd",
            command: "clangd",
            args: &[
                "--background-index",
                "--header-insertion=iwyu",
                "--clang-tidy",
                "--completion-style=detailed",
                "--function-arg-placeholders",
                "--pch-storage=memory",
            ],
            version_arg: "--version",
            version_command: None,
            install: InstallInstructions {
                macos: "brew install llvm",
                linux: "apt install clangd",
                windows: "Download from https://clangd.llvm.org/installation",
            },
            tier: ServerTier::Fast,
        },
    );

    configs.insert(
        Language::Zig,
        ServerConfig {
            display_name: "zls",
            command: "zls",
            args: &[],
            version_arg: "--version",
            version_command: None,
            install: InstallInstructions {
                macos: "brew install zls",
                linux: "Download from https://github.com/zigtools/zls/releases",
                windows: "Download from https://github.com/zigtools/zls/releases",
            },
            tier: ServerTier::Fast,
        },
    );

    configs.insert(
        Language::Java,
        ServerConfig {
            display_name: "jdtls",
            command: "jdtls",
            args: &[],
            version_arg: "--version",
            version_command: None,
            install: InstallInstructions {
                macos: "brew install jdtls",
                linux: "Download from https://download.eclipse.org/jdtls/snapshots/",
                windows: "Download from https://download.eclipse.org/jdtls/snapshots/",
            },
            tier: ServerTier::Slow,
        },
    );

    configs.insert(
        Language::Kotlin,
        ServerConfig {
            display_name: "kotlin-lsp",
            command: "kotlin-lsp",
            args: &["--stdio"],
            version_arg: "--help",
            version_command: None,
            install: InstallInstructions {
                macos: "brew install JetBrains/utils/kotlin-lsp",
                linux: "Download from https://github.com/JetBrains/kotlin-lsp/releases",
                windows: "Download from https://github.com/JetBrains/kotlin-lsp/releases",
            },
            tier: ServerTier::Slow,
        },
    );

    configs.insert(
        Language::Scala,
        ServerConfig {
            display_name: "metals",
            command: "metals",
            args: &[],
            version_arg: "--version",
            version_command: None,
            install: InstallInstructions {
                macos: "brew install metals",
                linux: "cs install metals",
                windows: "cs install metals",
            },
            tier: ServerTier::Slow,
        },
    );

    configs.insert(
        Language::Clojure,
        ServerConfig {
            display_name: "clojure-lsp",
            command: "clojure-lsp",
            args: &[],
            version_arg: "--version",
            version_command: None,
            install: InstallInstructions {
                macos: "brew install clojure-lsp/brew/clojure-lsp-native",
                linux: "Download from https://github.com/clojure-lsp/clojure-lsp/releases",
                windows: "Download from https://github.com/clojure-lsp/clojure-lsp/releases",
            },
            tier: ServerTier::Standard,
        },
    );

    configs.insert(
        Language::CSharp,
        ServerConfig {
            display_name: "csharp-ls",
            command: "csharp-ls",
            args: &[],
            version_arg: "--version",
            version_command: None,
            install: InstallInstructions {
                macos: "dotnet tool install -g csharp-ls",
                linux: "dotnet tool install -g csharp-ls",
                windows: "dotnet tool install -g csharp-ls",
            },
            tier: ServerTier::Standard,
        },
    );

    configs.insert(
        Language::FSharp,
        ServerConfig {
            display_name: "fsautocomplete",
            command: "fsautocomplete",
            args: &["--adaptive-lsp-server-enabled", "--project-graph-enabled"],
            version_arg: "--version",
            version_command: None,
            install: InstallInstructions {
                macos: "dotnet tool install -g fsautocomplete",
                linux: "dotnet tool install -g fsautocomplete",
                windows: "dotnet tool install -g fsautocomplete",
            },
            tier: ServerTier::Standard,
        },
    );

    configs.insert(
        Language::TypeScript,
        ServerConfig {
            display_name: "typescript-language-server",
            command: "typescript-language-server",
            args: &["--stdio"],
            version_arg: "--version",
            version_command: None,
            install: InstallInstructions {
                macos: "npm install -g typescript typescript-language-server",
                linux: "npm install -g typescript typescript-language-server",
                windows: "npm install -g typescript typescript-language-server",
            },
            tier: ServerTier::Slow,
        },
    );

    configs.insert(
        Language::JavaScript,
        ServerConfig {
            display_name: "typescript-language-server",
            command: "typescript-language-server",
            args: &["--stdio"],
            version_arg: "--version",
            version_command: None,
            install: InstallInstructions {
                macos: "npm install -g typescript typescript-language-server",
                linux: "npm install -g typescript typescript-language-server",
                windows: "npm install -g typescript typescript-language-server",
            },
            tier: ServerTier::Slow,
        },
    );

    configs.insert(
        Language::Vue,
        ServerConfig {
            display_name: "vue-language-server",
            command: "vue-language-server",
            args: &["--stdio"],
            version_arg: "--version",
            version_command: None,
            install: InstallInstructions {
                macos: "npm install -g @vue/language-server",
                linux: "npm install -g @vue/language-server",
                windows: "npm install -g @vue/language-server",
            },
            tier: ServerTier::Standard,
        },
    );

    configs.insert(
        Language::Python,
        ServerConfig {
            display_name: "pyright",
            command: "pyright-langserver",
            args: &["--stdio"],
            version_arg: "--version",
            version_command: Some("pyright"),
            install: InstallInstructions {
                macos: "npm install -g pyright",
                linux: "npm install -g pyright",
                windows: "npm install -g pyright",
            },
            tier: ServerTier::Slow,
        },
    );

    configs.insert(
        Language::Ruby,
        ServerConfig {
            display_name: "ruby-lsp",
            command: "ruby-lsp",
            args: &[],
            version_arg: "--version",
            version_command: None,
            install: InstallInstructions {
                macos: "gem install ruby-lsp",
                linux: "gem install ruby-lsp",
                windows: "gem install ruby-lsp",
            },
            tier: ServerTier::Standard,
        },
    );

    configs.insert(
        Language::PHP,
        ServerConfig {
            display_name: "intelephense",
            command: "intelephense",
            args: &["--stdio"],
            version_arg: "--version",
            version_command: None,
            install: InstallInstructions {
                macos: "npm install -g intelephense",
                linux: "npm install -g intelephense",
                windows: "npm install -g intelephense",
            },
            tier: ServerTier::Standard,
        },
    );

    configs.insert(
        Language::Perl,
        ServerConfig {
            display_name: "PerlNavigator",
            command: "perlnavigator",
            args: &["--stdio"],
            version_arg: "--version",
            version_command: None,
            install: InstallInstructions {
                macos: "npm install -g perlnavigator-server",
                linux: "npm install -g perlnavigator-server",
                windows: "npm install -g perlnavigator-server",
            },
            tier: ServerTier::Standard,
        },
    );

    configs.insert(
        Language::Lua,
        ServerConfig {
            display_name: "lua-language-server",
            command: "lua-language-server",
            args: &[],
            version_arg: "--version",
            version_command: None,
            install: InstallInstructions {
                macos: "brew install lua-language-server",
                linux: "Download from https://github.com/LuaLS/lua-language-server/releases",
                windows: "Download from https://github.com/LuaLS/lua-language-server/releases",
            },
            tier: ServerTier::Standard,
        },
    );

    configs.insert(
        Language::Bash,
        ServerConfig {
            display_name: "bash-language-server",
            command: "bash-language-server",
            args: &["start"],
            version_arg: "--version",
            version_command: None,
            install: InstallInstructions {
                macos: "npm install -g bash-language-server",
                linux: "npm install -g bash-language-server",
                windows: "npm install -g bash-language-server",
            },
            tier: ServerTier::Standard,
        },
    );

    configs.insert(
        Language::PowerShell,
        ServerConfig {
            display_name: "PowerShell EditorServices",
            command: "pwsh",
            args: &["-NoLogo", "-NoProfile", "-Command", "Import-Module PowerShellEditorServices; Start-EditorServices -HostName symora -HostProfileId symora -HostVersion 1.0.0 -BundledModulesPath $env:PSES_BUNDLE_PATH -Stdio"],
            version_arg: "--version",
            version_command: None,
            install: InstallInstructions {
                macos: "Install-Module -Name PowerShellEditorServices -Scope CurrentUser",
                linux: "Install-Module -Name PowerShellEditorServices -Scope CurrentUser",
                windows: "Install-Module -Name PowerShellEditorServices -Scope CurrentUser",
            },
            tier: ServerTier::Standard,
        },
    );

    configs.insert(
        Language::Haskell,
        ServerConfig {
            display_name: "haskell-language-server",
            command: "haskell-language-server-wrapper",
            args: &["--lsp"],
            version_arg: "--version",
            version_command: None,
            install: InstallInstructions {
                macos: "ghcup install hls",
                linux: "ghcup install hls",
                windows: "ghcup install hls",
            },
            tier: ServerTier::Slow,
        },
    );

    configs.insert(
        Language::Elixir,
        ServerConfig {
            display_name: "elixir-ls",
            command: "elixir-ls",
            args: &[],
            version_arg: "--version",
            version_command: None,
            install: InstallInstructions {
                macos: "brew install elixir-ls",
                linux: "Download from https://github.com/elixir-lsp/elixir-ls/releases",
                windows: "Download from https://github.com/elixir-lsp/elixir-ls/releases",
            },
            tier: ServerTier::Standard,
        },
    );

    configs.insert(
        Language::Erlang,
        ServerConfig {
            display_name: "erlang_ls",
            command: "erlang_ls",
            args: &[],
            version_arg: "--version",
            version_command: None,
            install: InstallInstructions {
                macos: "brew install erlang_ls",
                linux: "Download from https://github.com/erlang-ls/erlang_ls/releases",
                windows: "Download from https://github.com/erlang-ls/erlang_ls/releases",
            },
            tier: ServerTier::Standard,
        },
    );

    configs.insert(
        Language::Elm,
        ServerConfig {
            display_name: "elm-language-server",
            command: "elm-language-server",
            args: &[],
            version_arg: "--version",
            version_command: None,
            install: InstallInstructions {
                macos: "npm install -g @elm-tooling/elm-language-server",
                linux: "npm install -g @elm-tooling/elm-language-server",
                windows: "npm install -g @elm-tooling/elm-language-server",
            },
            tier: ServerTier::Standard,
        },
    );

    configs.insert(
        Language::OCaml,
        ServerConfig {
            display_name: "ocamllsp",
            command: "ocamllsp",
            args: &[],
            version_arg: "--version",
            version_command: None,
            install: InstallInstructions {
                macos: "opam install ocaml-lsp-server",
                linux: "opam install ocaml-lsp-server",
                windows: "opam install ocaml-lsp-server",
            },
            tier: ServerTier::Standard,
        },
    );

    configs.insert(
        Language::Go,
        ServerConfig {
            display_name: "gopls",
            command: "gopls",
            args: &["serve"],
            version_arg: "version",
            version_command: None,
            install: InstallInstructions {
                macos: "go install golang.org/x/tools/gopls@latest",
                linux: "go install golang.org/x/tools/gopls@latest",
                windows: "go install golang.org/x/tools/gopls@latest",
            },
            tier: ServerTier::Fast,
        },
    );

    configs.insert(
        Language::Swift,
        ServerConfig {
            display_name: "sourcekit-lsp",
            command: "sourcekit-lsp",
            args: &[],
            version_arg: "--version",
            version_command: None,
            install: InstallInstructions {
                macos: "Included with Xcode",
                linux: "Download from https://swift.org/download/",
                windows: "Download from https://swift.org/download/",
            },
            tier: ServerTier::Standard,
        },
    );

    configs.insert(
        Language::Dart,
        ServerConfig {
            display_name: "dart-language-server",
            command: "dart",
            args: &["language-server", "--protocol=lsp"],
            version_arg: "--version",
            version_command: None,
            install: InstallInstructions {
                macos: "brew install dart",
                linux: "apt install dart",
                windows: "choco install dart-sdk",
            },
            tier: ServerTier::Standard,
        },
    );

    configs.insert(
        Language::Terraform,
        ServerConfig {
            display_name: "terraform-ls",
            command: "terraform-ls",
            args: &["serve"],
            version_arg: "--version",
            version_command: None,
            install: InstallInstructions {
                macos: "brew install hashicorp/tap/terraform-ls",
                linux: "Download from https://releases.hashicorp.com/terraform-ls/",
                windows: "choco install terraform-ls",
            },
            tier: ServerTier::Standard,
        },
    );

    configs.insert(
        Language::Yaml,
        ServerConfig {
            display_name: "yaml-language-server",
            command: "yaml-language-server",
            args: &["--stdio"],
            version_arg: "--version",
            version_command: None,
            install: InstallInstructions {
                macos: "npm install -g yaml-language-server",
                linux: "npm install -g yaml-language-server",
                windows: "npm install -g yaml-language-server",
            },
            tier: ServerTier::Standard,
        },
    );

    configs.insert(
        Language::Toml,
        ServerConfig {
            display_name: "taplo",
            command: "taplo",
            args: &["lsp", "stdio"],
            version_arg: "--version",
            version_command: None,
            install: InstallInstructions {
                macos: "brew install taplo",
                linux: "cargo install taplo-cli --locked",
                windows: "cargo install taplo-cli --locked",
            },
            tier: ServerTier::Standard,
        },
    );

    configs.insert(
        Language::Nix,
        ServerConfig {
            display_name: "nil",
            command: "nil",
            args: &[],
            version_arg: "--version",
            version_command: None,
            install: InstallInstructions {
                macos: "nix profile install nixpkgs#nil",
                linux: "nix profile install nixpkgs#nil",
                windows: "nix profile install nixpkgs#nil",
            },
            tier: ServerTier::Standard,
        },
    );

    configs.insert(
        Language::Rego,
        ServerConfig {
            display_name: "regal",
            command: "regal",
            args: &["language-server"],
            version_arg: "version",
            version_command: None,
            install: InstallInstructions {
                macos: "brew install styrainc/packages/regal",
                linux: "Download from https://github.com/StyraInc/regal/releases",
                windows: "Download from https://github.com/StyraInc/regal/releases",
            },
            tier: ServerTier::Standard,
        },
    );

    configs.insert(
        Language::R,
        ServerConfig {
            display_name: "R languageserver",
            command: "R",
            args: &["--slave", "-e", "languageserver::run()"],
            version_arg: "--version",
            version_command: None,
            install: InstallInstructions {
                macos: "R -e 'install.packages(\"languageserver\")'",
                linux: "R -e 'install.packages(\"languageserver\")'",
                windows: "R -e 'install.packages(\"languageserver\")'",
            },
            tier: ServerTier::Standard,
        },
    );

    configs.insert(
        Language::Julia,
        ServerConfig {
            display_name: "LanguageServer.jl",
            command: "julia",
            args: &[
                "--startup-file=no",
                "--history-file=no",
                "-e",
                "using LanguageServer; runserver()",
            ],
            version_arg: "--version",
            version_command: None,
            install: InstallInstructions {
                macos: "julia -e 'using Pkg; Pkg.add(\"LanguageServer\")'",
                linux: "julia -e 'using Pkg; Pkg.add(\"LanguageServer\")'",
                windows: "julia -e 'using Pkg; Pkg.add(\"LanguageServer\")'",
            },
            tier: ServerTier::Standard,
        },
    );

    configs.insert(
        Language::Fortran,
        ServerConfig {
            display_name: "fortls",
            command: "fortls",
            args: &[],
            version_arg: "--version",
            version_command: None,
            install: InstallInstructions {
                macos: "pip install fortls",
                linux: "pip install fortls",
                windows: "pip install fortls",
            },
            tier: ServerTier::Standard,
        },
    );

    configs.insert(
        Language::Markdown,
        ServerConfig {
            display_name: "marksman",
            command: "marksman",
            args: &["server"],
            version_arg: "--version",
            version_command: None,
            install: InstallInstructions {
                macos: "brew install marksman",
                linux: "Download from https://github.com/artempyanykh/marksman/releases",
                windows: "Download from https://github.com/artempyanykh/marksman/releases",
            },
            tier: ServerTier::Standard,
        },
    );

    configs
}

/// Server health check result
#[derive(Debug, Clone)]
pub struct ServerHealth {
    pub language: Language,
    pub name: &'static str,
    pub installed: bool,
    pub version: Option<String>,
    pub install_instruction: &'static str,
    pub tier: ServerTier,
}

/// Check health of all configured servers
pub fn check_all_servers() -> Vec<ServerHealth> {
    let configs = defaults();
    let mut results = Vec::new();

    for (language, config) in configs {
        let installed = config.is_installed();
        let version = if installed {
            config.probe_version()
        } else {
            None
        };

        results.push(ServerHealth {
            language,
            name: config.display_name,
            installed,
            version,
            install_instruction: config.install.current(),
            tier: config.tier,
        });
    }

    results.sort_by_key(|a| a.language.to_string());

    results
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fake_executable(dir: &std::path::Path, name: &str) -> PathBuf {
        let path = dir.join(name);
        std::fs::write(&path, "#!/bin/sh\nexit 1\n").unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
        }
        path
    }

    #[test]
    fn search_finds_executable_in_listed_dir() {
        let dir = tempfile::tempdir().unwrap();
        let bin = fake_executable(dir.path(), "fake-ls");
        let found = search_dirs("fake-ls", &[dir.path().to_path_buf()]).unwrap();
        assert_eq!(found, bin);
    }

    #[test]
    fn search_skips_non_executable_files() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("fake-ls"), "not a binary").unwrap();
        let err = search_dirs("fake-ls", &[dir.path().to_path_buf()]).unwrap_err();
        assert_eq!(err, vec![dir.path().to_path_buf()]);
    }

    #[test]
    fn search_miss_returns_every_searched_dir() {
        let a = tempfile::tempdir().unwrap();
        let b = tempfile::tempdir().unwrap();
        let dirs = vec![a.path().to_path_buf(), b.path().to_path_buf()];
        let err = search_dirs("missing-ls", &dirs).unwrap_err();
        assert_eq!(err, dirs);
    }

    #[test]
    fn resolve_command_accepts_existing_absolute_path() {
        let dir = tempfile::tempdir().unwrap();
        let bin = fake_executable(dir.path(), "abs-ls");
        let found = resolve_command(bin.to_str().unwrap()).unwrap();
        assert_eq!(found, bin);
    }

    /// The pyright regression, generalized: a server whose binary exists
    /// but whose `--version` exits non-zero must still count as
    /// installed — installation is resolvability, never an exit code.
    #[test]
    fn installed_does_not_depend_on_version_exit_code() {
        let dir = tempfile::tempdir().unwrap();
        let bin = fake_executable(dir.path(), "grumpy-ls"); // exits 1 on any arg
        let config = ServerConfig {
            display_name: "grumpy-ls",
            command: Box::leak(bin.to_string_lossy().into_owned().into_boxed_str()),
            args: &[],
            version_arg: "--version",
            version_command: None,
            install: InstallInstructions {
                macos: "none",
                linux: "none",
                windows: "none",
            },
            tier: ServerTier::Fast,
        };
        assert!(config.is_installed());
        assert!(config.resolve().is_ok());
        // The version probe legitimately fails — that only blanks the
        // doctor version column, it does not mark the server missing.
        assert_eq!(config.probe_version(), None);
    }

    #[test]
    fn not_found_hint_lists_searched_dirs_and_install_instruction() {
        let dirs = vec![PathBuf::from("/nowhere/a"), PathBuf::from("/nowhere/b")];
        let hint = not_found_hint("npm install -g fake-ls", &dirs);
        assert!(hint.contains("npm install -g fake-ls"));
        assert!(hint.contains("/nowhere/a"));
        assert!(hint.contains("/nowhere/b"));
    }

    #[test]
    fn test_defaults() {
        let configs = defaults();
        assert!(configs.contains_key(&Language::Rust));
        assert!(configs.contains_key(&Language::TypeScript));
        assert!(configs.contains_key(&Language::Python));
        assert!(configs.contains_key(&Language::Go));
    }

    #[test]
    fn test_platform_detection() {
        let platform = Platform::current();
        // Just verify it returns a valid value
        assert!(matches!(
            platform,
            Platform::MacOS | Platform::Linux | Platform::Windows
        ));
    }

    #[test]
    fn test_install_instructions() {
        let configs = defaults();
        let rust_config = configs.get(&Language::Rust).unwrap();

        // Verify all platforms have instructions
        assert!(!rust_config.install.macos.is_empty());
        assert!(!rust_config.install.linux.is_empty());
        assert!(!rust_config.install.windows.is_empty());
    }

    #[test]
    fn test_check_all_servers() {
        let health = check_all_servers();
        // Should have health info for all supported languages
        assert!(health.len() >= 6);
    }
}
