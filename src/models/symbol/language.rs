use std::fmt;
use std::path::Path;
use std::str::FromStr;

use serde::{Deserialize, Serialize};

/// Programming languages Symora can route work to. The set is intentionally
/// broader than the LSP/tree-sitter coverage matrix — other layers gate on
/// what they actually support, so the model can pass through arbitrary
/// extensions without losing fidelity.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum Language {
    Rust,
    Cpp,
    Zig,

    Java,
    Kotlin,
    Scala,
    Clojure,

    CSharp,
    FSharp,

    TypeScript,
    JavaScript,
    Vue,

    Python,
    Ruby,
    PHP,
    Perl,
    Lua,
    Bash,
    PowerShell,

    Haskell,
    Elixir,
    Erlang,
    Elm,
    OCaml,

    Go,
    Swift,
    Dart,

    Terraform,
    Yaml,
    Toml,
    Json,
    Nix,
    Rego,

    Html,
    Css,
    Scss,
    Sql,

    R,
    Julia,
    Fortran,

    Markdown,
    Mdx,

    #[default]
    Unknown,
}

impl Language {
    /// Detect a language from a file extension (case-insensitive, no leading dot).
    pub fn from_extension(ext: &str) -> Self {
        match ext.to_lowercase().as_str() {
            "rs" => Self::Rust,
            "c" | "cpp" | "cc" | "cxx" | "h" | "hpp" | "hxx" => Self::Cpp,
            "zig" => Self::Zig,

            "java" => Self::Java,
            "kt" | "kts" => Self::Kotlin,
            "scala" | "sc" => Self::Scala,
            "clj" | "cljs" | "cljc" | "edn" => Self::Clojure,

            "cs" => Self::CSharp,
            "fs" | "fsx" | "fsi" => Self::FSharp,

            "ts" | "tsx" | "mts" | "cts" => Self::TypeScript,
            "js" | "jsx" | "mjs" | "cjs" => Self::JavaScript,
            "vue" => Self::Vue,

            "py" | "pyi" => Self::Python,
            "rb" | "rake" | "gemspec" => Self::Ruby,
            "php" => Self::PHP,
            "pl" | "pm" | "t" => Self::Perl,
            "lua" => Self::Lua,
            "sh" | "bash" | "zsh" => Self::Bash,
            "ps1" | "psm1" | "psd1" => Self::PowerShell,

            "hs" | "lhs" => Self::Haskell,
            "ex" | "exs" => Self::Elixir,
            "erl" | "hrl" => Self::Erlang,
            "elm" => Self::Elm,
            "ml" | "mli" => Self::OCaml,

            "go" => Self::Go,
            "swift" => Self::Swift,
            "dart" => Self::Dart,

            "tf" | "tfvars" | "hcl" => Self::Terraform,
            "yaml" | "yml" => Self::Yaml,
            "toml" => Self::Toml,
            "json" | "jsonc" => Self::Json,
            "nix" => Self::Nix,
            "rego" => Self::Rego,

            "html" | "htm" => Self::Html,
            "css" => Self::Css,
            "scss" => Self::Scss,
            "sql" => Self::Sql,

            "r" | "rmd" => Self::R,
            "jl" => Self::Julia,
            "f" | "f90" | "f95" | "f03" | "f08" | "for" => Self::Fortran,

            "md" | "markdown" => Self::Markdown,
            "mdx" => Self::Mdx,

            _ => Self::Unknown,
        }
    }

    /// Detect a language from a file path's extension.
    pub fn from_path(path: &Path) -> Self {
        path.extension()
            .and_then(|e| e.to_str())
            .map(Self::from_extension)
            .unwrap_or(Self::Unknown)
    }

    /// Parse a language from a CLI string, returning `Unknown` for unrecognised values.
    pub fn parse_or_default(s: &str) -> Self {
        s.parse().unwrap_or(Self::Unknown)
    }

    /// File extensions for this language (no leading dot, lowercase).
    pub fn extensions(&self) -> &'static [&'static str] {
        match self {
            Self::Rust => &["rs"],
            Self::Cpp => &["c", "cpp", "cc", "cxx", "h", "hpp", "hxx"],
            Self::Zig => &["zig"],

            Self::Java => &["java"],
            Self::Kotlin => &["kt", "kts"],
            Self::Scala => &["scala", "sc"],
            Self::Clojure => &["clj", "cljs", "cljc", "edn"],

            Self::CSharp => &["cs"],
            Self::FSharp => &["fs", "fsx", "fsi"],

            Self::TypeScript => &["ts", "tsx", "mts", "cts"],
            Self::JavaScript => &["js", "jsx", "mjs", "cjs"],
            Self::Vue => &["vue"],

            Self::Python => &["py", "pyi"],
            Self::Ruby => &["rb", "rake", "gemspec"],
            Self::PHP => &["php"],
            Self::Perl => &["pl", "pm", "t"],
            Self::Lua => &["lua"],
            Self::Bash => &["sh", "bash", "zsh"],
            Self::PowerShell => &["ps1", "psm1", "psd1"],

            Self::Haskell => &["hs", "lhs"],
            Self::Elixir => &["ex", "exs"],
            Self::Erlang => &["erl", "hrl"],
            Self::Elm => &["elm"],
            Self::OCaml => &["ml", "mli"],

            Self::Go => &["go"],
            Self::Swift => &["swift"],
            Self::Dart => &["dart"],

            Self::Terraform => &["tf", "tfvars", "hcl"],
            Self::Yaml => &["yaml", "yml"],
            Self::Toml => &["toml"],
            Self::Json => &["json", "jsonc"],
            Self::Nix => &["nix"],
            Self::Rego => &["rego"],

            Self::Html => &["html", "htm"],
            Self::Css => &["css"],
            Self::Scss => &["scss"],
            Self::Sql => &["sql"],

            Self::R => &["r", "rmd"],
            Self::Julia => &["jl"],
            Self::Fortran => &["f", "f90", "f95", "f03", "f08", "for"],

            Self::Markdown => &["md", "markdown"],
            Self::Mdx => &["mdx"],

            Self::Unknown => &[],
        }
    }

    /// LSP language identifier (matches the `textDocument.languageId` wire value).
    pub fn lsp_id(&self) -> &'static str {
        match self {
            Self::Rust => "rust",
            Self::Cpp => "cpp",
            Self::Zig => "zig",

            Self::Java => "java",
            Self::Kotlin => "kotlin",
            Self::Scala => "scala",
            Self::Clojure => "clojure",

            Self::CSharp => "csharp",
            Self::FSharp => "fsharp",

            Self::TypeScript => "typescript",
            Self::JavaScript => "javascript",
            Self::Vue => "vue",

            Self::Python => "python",
            Self::Ruby => "ruby",
            Self::PHP => "php",
            Self::Perl => "perl",
            Self::Lua => "lua",
            Self::Bash => "shellscript",
            Self::PowerShell => "powershell",

            Self::Haskell => "haskell",
            Self::Elixir => "elixir",
            Self::Erlang => "erlang",
            Self::Elm => "elm",
            Self::OCaml => "ocaml",

            Self::Go => "go",
            Self::Swift => "swift",
            Self::Dart => "dart",

            Self::Terraform => "terraform",
            Self::Yaml => "yaml",
            Self::Toml => "toml",
            Self::Json => "json",
            Self::Nix => "nix",
            Self::Rego => "rego",

            Self::Html => "html",
            Self::Css => "css",
            Self::Scss => "scss",
            Self::Sql => "sql",

            Self::R => "r",
            Self::Julia => "julia",
            Self::Fortran => "fortran",

            Self::Markdown => "markdown",
            Self::Mdx => "mdx",

            Self::Unknown => "plaintext",
        }
    }

    /// Every language Symora knows about, excluding `Unknown`.
    pub fn all() -> Vec<Self> {
        vec![
            Self::Rust,
            Self::Cpp,
            Self::Zig,
            Self::Java,
            Self::Kotlin,
            Self::Scala,
            Self::Clojure,
            Self::CSharp,
            Self::FSharp,
            Self::TypeScript,
            Self::JavaScript,
            Self::Vue,
            Self::Python,
            Self::Ruby,
            Self::PHP,
            Self::Perl,
            Self::Lua,
            Self::Bash,
            Self::PowerShell,
            Self::Haskell,
            Self::Elixir,
            Self::Erlang,
            Self::Elm,
            Self::OCaml,
            Self::Go,
            Self::Swift,
            Self::Dart,
            Self::Terraform,
            Self::Yaml,
            Self::Toml,
            Self::Json,
            Self::Nix,
            Self::Rego,
            Self::Html,
            Self::Css,
            Self::Scss,
            Self::Sql,
            Self::R,
            Self::Julia,
            Self::Fortran,
            Self::Markdown,
            Self::Mdx,
        ]
    }

    /// Whether an unscoped search covers this language — the fan-out it pays
    /// for, the corpus it embeds, the files it ranks and scans. Documentation
    /// and data formats are covered only when named: a symbol query has
    /// nothing to ask one, and listing it as a language the answer could not
    /// cover is noise rather than a gap. Narrowing this narrows every one of
    /// those at once.
    ///
    /// The match is exhaustive so a new language cannot join the default
    /// fan-out by omission.
    pub fn is_code(self) -> bool {
        match self {
            Self::Markdown | Self::Mdx | Self::Yaml | Self::Toml | Self::Json => false,
            Self::Unknown => false,
            Self::Rust
            | Self::Cpp
            | Self::Zig
            | Self::Java
            | Self::Kotlin
            | Self::Scala
            | Self::Clojure
            | Self::CSharp
            | Self::FSharp
            | Self::TypeScript
            | Self::JavaScript
            | Self::Vue
            | Self::Python
            | Self::Ruby
            | Self::PHP
            | Self::Perl
            | Self::Lua
            | Self::Bash
            | Self::PowerShell
            | Self::Haskell
            | Self::Elixir
            | Self::Erlang
            | Self::Elm
            | Self::OCaml
            | Self::Go
            | Self::Swift
            | Self::Dart
            | Self::Terraform
            | Self::Nix
            | Self::Rego
            | Self::Html
            | Self::Css
            | Self::Scss
            | Self::Sql
            | Self::R
            | Self::Julia
            | Self::Fortran => true,
        }
    }
}

impl fmt::Display for Language {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.lsp_id())
    }
}

impl FromStr for Language {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s.to_lowercase().as_str() {
            "rust" | "rs" => Ok(Self::Rust),
            "cpp" | "c++" | "c" => Ok(Self::Cpp),
            "zig" => Ok(Self::Zig),

            "java" => Ok(Self::Java),
            "kotlin" | "kt" => Ok(Self::Kotlin),
            "scala" => Ok(Self::Scala),
            "clojure" | "clj" => Ok(Self::Clojure),

            "csharp" | "c#" | "cs" => Ok(Self::CSharp),
            "fsharp" | "f#" | "fs" => Ok(Self::FSharp),

            "typescript" | "ts" => Ok(Self::TypeScript),
            "javascript" | "js" => Ok(Self::JavaScript),
            "vue" => Ok(Self::Vue),

            "python" | "py" => Ok(Self::Python),
            "ruby" | "rb" => Ok(Self::Ruby),
            "php" => Ok(Self::PHP),
            "perl" | "pl" => Ok(Self::Perl),
            "lua" => Ok(Self::Lua),
            "bash" | "sh" | "shell" | "shellscript" => Ok(Self::Bash),
            "powershell" | "pwsh" | "ps1" => Ok(Self::PowerShell),

            "haskell" | "hs" => Ok(Self::Haskell),
            "elixir" | "ex" => Ok(Self::Elixir),
            "erlang" | "erl" => Ok(Self::Erlang),
            "elm" => Ok(Self::Elm),
            "ocaml" | "ml" => Ok(Self::OCaml),

            "go" | "golang" => Ok(Self::Go),
            "swift" => Ok(Self::Swift),
            "dart" => Ok(Self::Dart),

            "terraform" | "tf" | "hcl" => Ok(Self::Terraform),
            "yaml" | "yml" => Ok(Self::Yaml),
            "toml" => Ok(Self::Toml),
            "json" | "jsonc" => Ok(Self::Json),
            "nix" => Ok(Self::Nix),
            "rego" => Ok(Self::Rego),

            "html" | "htm" => Ok(Self::Html),
            "css" => Ok(Self::Css),
            "scss" => Ok(Self::Scss),
            "sql" => Ok(Self::Sql),

            "r" => Ok(Self::R),
            "julia" | "jl" => Ok(Self::Julia),
            "fortran" | "f90" => Ok(Self::Fortran),

            "markdown" | "md" => Ok(Self::Markdown),
            "mdx" => Ok(Self::Mdx),

            _ => Err(format!("Unknown language: {}", s)),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn from_extension_recognises_common_languages() {
        assert_eq!(Language::from_extension("kt"), Language::Kotlin);
        assert_eq!(Language::from_extension("rs"), Language::Rust);
        assert_eq!(Language::from_extension("ts"), Language::TypeScript);
        assert_eq!(Language::from_extension("py"), Language::Python);
        assert_eq!(Language::from_extension("go"), Language::Go);
        assert_eq!(Language::from_extension("java"), Language::Java);
    }

    #[test]
    fn from_extension_unknown_falls_back() {
        assert_eq!(Language::from_extension("txt"), Language::Unknown);
    }

    #[test]
    fn lsp_id_round_trips_through_parse() {
        // The store persists languages as `lsp_id()` and the daemon wire
        // carries that same value, so every language must parse back to
        // itself — otherwise a daemon-mode `--lang` filter silently breaks.
        for language in Language::all() {
            assert_eq!(
                Language::parse_or_default(language.lsp_id()),
                language,
                "lsp_id `{}` did not round-trip",
                language.lsp_id()
            );
        }
    }

    #[test]
    fn from_path_extracts_extension() {
        use std::path::PathBuf;
        assert_eq!(
            Language::from_path(&PathBuf::from("src/main.rs")),
            Language::Rust
        );
        assert_eq!(
            Language::from_path(&PathBuf::from("README.md")),
            Language::Markdown
        );
        assert_eq!(
            Language::from_path(&PathBuf::from("Makefile")),
            Language::Unknown
        );
    }

    #[test]
    fn extensions_round_trip_through_from_extension() {
        for lang in Language::all() {
            for ext in lang.extensions() {
                assert_eq!(Language::from_extension(ext), lang, "ext {ext}");
            }
        }
    }
}
