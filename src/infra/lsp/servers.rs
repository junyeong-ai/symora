//! Language Server Configurations
//!
//! Platform-aware configurations with tiered timeout profiles.

use std::collections::HashMap;
use std::process::Command;
use std::time::Duration;

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
    pub name: &'static str,
    pub command: &'static str,
    pub args: &'static [&'static str],
    pub install: InstallInstructions,
    pub version_arg: &'static str,
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

    pub fn is_installed(&self) -> bool {
        // Try which/where command first (most reliable)
        #[cfg(unix)]
        if let Ok(output) = Command::new("which").arg(self.command).output()
            && output.status.success()
        {
            return true;
        }

        #[cfg(windows)]
        if let Ok(output) = Command::new("where").arg(self.command).output()
            && output.status.success()
        {
            return true;
        }

        // Fallback: try version command
        Command::new(self.command)
            .arg(self.version_arg)
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
            .is_ok()
    }

    /// Get installed version (if available)
    pub fn version(&self) -> Option<String> {
        let output = Command::new(self.command)
            .arg(self.version_arg)
            .output()
            .ok()?;

        if !output.status.success() {
            return None;
        }

        let stdout = String::from_utf8_lossy(&output.stdout);
        let stderr = String::from_utf8_lossy(&output.stderr);

        // Try to extract version from output
        let text = if stdout.trim().is_empty() {
            stderr.to_string()
        } else {
            stdout.to_string()
        };

        // Return first non-empty line as version
        text.lines()
            .find(|line| !line.trim().is_empty())
            .map(|s| s.trim().to_string())
    }
}

/// Default server configurations for all supported languages
pub fn defaults() -> HashMap<Language, ServerConfig> {
    let mut configs = HashMap::new();

    configs.insert(
        Language::Rust,
        ServerConfig {
            name: "rust-analyzer",
            command: "rust-analyzer",
            args: &[],
            version_arg: "--version",
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
            name: "clangd",
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
            name: "zls",
            command: "zls",
            args: &[],
            version_arg: "--version",
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
            name: "jdtls",
            command: "jdtls",
            args: &[],
            version_arg: "--version",
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
            name: "kotlin-lsp",
            command: "kotlin-lsp",
            args: &["--stdio"],
            version_arg: "--help",
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
            name: "metals",
            command: "metals",
            args: &[],
            version_arg: "--version",
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
            name: "clojure-lsp",
            command: "clojure-lsp",
            args: &[],
            version_arg: "--version",
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
            name: "csharp-ls",
            command: "csharp-ls",
            args: &[],
            version_arg: "--version",
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
            name: "fsautocomplete",
            command: "fsautocomplete",
            args: &["--adaptive-lsp-server-enabled", "--project-graph-enabled"],
            version_arg: "--version",
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
            name: "typescript-language-server",
            command: "typescript-language-server",
            args: &["--stdio"],
            version_arg: "--version",
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
            name: "typescript-language-server",
            command: "typescript-language-server",
            args: &["--stdio"],
            version_arg: "--version",
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
            name: "vue-language-server",
            command: "vue-language-server",
            args: &["--stdio"],
            version_arg: "--version",
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
            name: "pyright",
            command: "pyright-langserver",
            args: &["--stdio"],
            version_arg: "--version",
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
            name: "ruby-lsp",
            command: "ruby-lsp",
            args: &[],
            version_arg: "--version",
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
            name: "intelephense",
            command: "intelephense",
            args: &["--stdio"],
            version_arg: "--version",
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
            name: "PerlNavigator",
            command: "perlnavigator",
            args: &["--stdio"],
            version_arg: "--version",
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
            name: "lua-language-server",
            command: "lua-language-server",
            args: &[],
            version_arg: "--version",
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
            name: "bash-language-server",
            command: "bash-language-server",
            args: &["start"],
            version_arg: "--version",
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
            name: "PowerShell EditorServices",
            command: "pwsh",
            args: &["-NoLogo", "-NoProfile", "-Command", "Import-Module PowerShellEditorServices; Start-EditorServices -HostName symora -HostProfileId symora -HostVersion 1.0.0 -BundledModulesPath $env:PSES_BUNDLE_PATH -Stdio"],
            version_arg: "--version",
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
            name: "haskell-language-server",
            command: "haskell-language-server-wrapper",
            args: &["--lsp"],
            version_arg: "--version",
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
            name: "elixir-ls",
            command: "elixir-ls",
            args: &[],
            version_arg: "--version",
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
            name: "erlang_ls",
            command: "erlang_ls",
            args: &[],
            version_arg: "--version",
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
            name: "elm-language-server",
            command: "elm-language-server",
            args: &[],
            version_arg: "--version",
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
            name: "ocamllsp",
            command: "ocamllsp",
            args: &[],
            version_arg: "--version",
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
            name: "gopls",
            command: "gopls",
            args: &["serve"],
            version_arg: "version",
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
            name: "sourcekit-lsp",
            command: "sourcekit-lsp",
            args: &[],
            version_arg: "--version",
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
            name: "dart-language-server",
            command: "dart",
            args: &["language-server", "--protocol=lsp"],
            version_arg: "--version",
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
            name: "terraform-ls",
            command: "terraform-ls",
            args: &["serve"],
            version_arg: "--version",
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
            name: "yaml-language-server",
            command: "yaml-language-server",
            args: &["--stdio"],
            version_arg: "--version",
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
            name: "taplo",
            command: "taplo",
            args: &["lsp", "stdio"],
            version_arg: "--version",
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
            name: "nil",
            command: "nil",
            args: &[],
            version_arg: "--version",
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
            name: "regal",
            command: "regal",
            args: &["language-server"],
            version_arg: "version",
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
            name: "R languageserver",
            command: "R",
            args: &["--slave", "-e", "languageserver::run()"],
            version_arg: "--version",
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
            name: "LanguageServer.jl",
            command: "julia",
            args: &[
                "--startup-file=no",
                "--history-file=no",
                "-e",
                "using LanguageServer; runserver()",
            ],
            version_arg: "--version",
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
            name: "fortls",
            command: "fortls",
            args: &[],
            version_arg: "--version",
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
            name: "marksman",
            command: "marksman",
            args: &["server"],
            version_arg: "--version",
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
}

/// Check health of all configured servers
pub fn check_all_servers() -> Vec<ServerHealth> {
    let configs = defaults();
    let mut results = Vec::new();

    for (language, config) in configs {
        let installed = config.is_installed();
        let version = if installed { config.version() } else { None };

        results.push(ServerHealth {
            language,
            name: config.name,
            installed,
            version,
            install_instruction: config.install.current(),
        });
    }

    // Sort by language name for consistent output
    results.sort_by(|a, b| format!("{:?}", a.language).cmp(&format!("{:?}", b.language)));

    results
}

#[cfg(test)]
mod tests {
    use super::*;

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
