//! LSP Capabilities Matrix
//!
//! Defines which LSP features are supported by each language server.
//! Used for providing accurate error messages when a feature is not supported.

use crate::models::symbol::Language;

/// LSP feature categories
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LspFeature {
    /// textDocument/documentSymbol
    FindSymbol,
    /// textDocument/references
    FindReferences,
    /// textDocument/definition
    GotoDefinition,
    /// textDocument/typeDefinition
    GotoTypeDefinition,
    /// textDocument/implementation
    FindImplementations,
    /// textDocument/hover
    Hover,
    /// textDocument/publishDiagnostics
    Diagnostics,
    /// textDocument/rename
    Rename,
    /// callHierarchy/incomingCalls
    IncomingCalls,
    /// callHierarchy/outgoingCalls
    OutgoingCalls,
    /// textDocument/codeAction
    CodeActions,
    /// typeHierarchy/supertypes + typeHierarchy/subtypes
    TypeHierarchy,
    /// textDocument/inlayHint
    InlayHints,
}

impl LspFeature {
    /// Get human-readable name for the feature
    pub fn display_name(&self) -> &'static str {
        match self {
            Self::FindSymbol => "find symbol",
            Self::FindReferences => "find references",
            Self::GotoDefinition => "go to definition",
            Self::GotoTypeDefinition => "go to type definition",
            Self::FindImplementations => "find implementations",
            Self::Hover => "hover",
            Self::Diagnostics => "diagnostics",
            Self::Rename => "rename",
            Self::IncomingCalls => "incoming calls",
            Self::OutgoingCalls => "outgoing calls",
            Self::CodeActions => "code actions",
            Self::TypeHierarchy => "type hierarchy",
            Self::InlayHints => "inlay hints",
        }
    }

    /// Get the CLI command name for this feature
    pub fn command_name(&self) -> &'static str {
        match self {
            Self::FindSymbol => "find symbol",
            Self::FindReferences => "find refs",
            Self::GotoDefinition => "find def",
            Self::GotoTypeDefinition => "find typedef",
            Self::FindImplementations => "find impl",
            Self::Hover => "hover",
            Self::Diagnostics => "diagnostics",
            Self::Rename => "rename",
            Self::IncomingCalls => "calls incoming",
            Self::OutgoingCalls => "calls outgoing",
            Self::CodeActions => "actions list",
            Self::TypeHierarchy => "types",
            Self::InlayHints => "hints",
        }
    }
}

/// Support level for a feature
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SupportLevel {
    /// Fully supported and tested
    Full,
    /// Partially supported or unreliable
    Partial,
    /// Not supported by the language server
    None,
}

/// Get the support level for a feature in a specific language
pub fn get_support_level(language: Language, feature: LspFeature) -> SupportLevel {
    use LspFeature::*;
    use SupportLevel::*;

    match (language, feature) {
        // Rust (rust-analyzer) - excellent support
        (Language::Rust, _) => Full,

        // C/C++ (clangd) - excellent support
        (Language::Cpp, IncomingCalls) => Partial,
        (Language::Cpp, OutgoingCalls) => None,
        (Language::Cpp, _) => Full,

        // Go (gopls) - excellent support
        (Language::Go, _) => Full,

        // TypeScript/JavaScript (tsserver) - limited by slow initialization
        (Language::TypeScript | Language::JavaScript, FindSymbol | GotoDefinition) => Full,
        (Language::TypeScript | Language::JavaScript, FindReferences) => Partial,
        (Language::TypeScript | Language::JavaScript, Hover | Rename) => Partial,
        (Language::TypeScript | Language::JavaScript, FindImplementations) => Partial,
        (Language::TypeScript | Language::JavaScript, GotoTypeDefinition) => Partial,
        (Language::TypeScript | Language::JavaScript, TypeHierarchy) => None,
        (Language::TypeScript | Language::JavaScript, _) => Partial,

        // Python (pyright) - slow on large projects
        (Language::Python, FindImplementations) => None,
        (Language::Python, IncomingCalls | OutgoingCalls) => None,
        (Language::Python, TypeHierarchy) => None,
        (Language::Python, _) => Partial,

        // Java (jdtls) - excellent support
        (Language::Java, _) => Full,

        // Kotlin (kotlin-lsp) - class-level only
        (Language::Kotlin, FindSymbol) => Partial,
        (Language::Kotlin, FindReferences) => Partial,
        (Language::Kotlin, FindImplementations) => None,
        (Language::Kotlin, GotoTypeDefinition) => Partial,
        (Language::Kotlin, IncomingCalls | OutgoingCalls) => None,
        (Language::Kotlin, TypeHierarchy) => None,
        (Language::Kotlin, InlayHints) => None,
        (Language::Kotlin, Rename) => Partial,
        (Language::Kotlin, _) => Full,

        // PHP (intelephense) - rename requires premium
        (Language::PHP, Rename) => None,
        (Language::PHP, FindImplementations) => None,
        (Language::PHP, IncomingCalls | OutgoingCalls) => None,
        (Language::PHP, TypeHierarchy) => None,
        (Language::PHP, _) => Full,

        // C# (csharp-ls) - requires installation
        (Language::CSharp, _) => Partial,

        // Default for other languages
        _ => Partial,
    }
}

/// Check if a feature is supported (Full or Partial)
pub fn is_feature_supported(language: Language, feature: LspFeature) -> bool {
    get_support_level(language, feature) != SupportLevel::None
}

/// Get a helpful message when a feature is not supported
pub fn get_unsupported_message(language: Language, feature: LspFeature) -> String {
    let lang_name = language_display_name(language);
    let server_name = language_server_name(language);
    let feature_name = feature.display_name();

    let suggestion = get_alternative_suggestion(language, feature);

    format!(
        "{} ({}) does not support '{}'. {}",
        lang_name, server_name, feature_name, suggestion
    )
}

/// Get language display name
pub fn language_display_name(language: Language) -> &'static str {
    match language {
        // Systems
        Language::Rust => "Rust",
        Language::Cpp => "C++",
        Language::Zig => "Zig",
        // JVM
        Language::Java => "Java",
        Language::Kotlin => "Kotlin",
        Language::Scala => "Scala",
        Language::Clojure => "Clojure",
        // .NET
        Language::CSharp => "C#",
        Language::FSharp => "F#",
        // Web
        Language::TypeScript => "TypeScript",
        Language::JavaScript => "JavaScript",
        Language::Vue => "Vue",
        // Scripting
        Language::Python => "Python",
        Language::Ruby => "Ruby",
        Language::PHP => "PHP",
        Language::Perl => "Perl",
        Language::Lua => "Lua",
        Language::Bash => "Bash",
        Language::PowerShell => "PowerShell",
        // Functional
        Language::Haskell => "Haskell",
        Language::Elixir => "Elixir",
        Language::Erlang => "Erlang",
        Language::Elm => "Elm",
        Language::OCaml => "OCaml",
        // Mobile/Application
        Language::Go => "Go",
        Language::Swift => "Swift",
        Language::Dart => "Dart",
        // Config/DevOps
        Language::Terraform => "Terraform",
        Language::Yaml => "YAML",
        Language::Toml => "TOML",
        Language::Nix => "Nix",
        Language::Rego => "Rego",
        // Scientific
        Language::R => "R",
        Language::Julia => "Julia",
        Language::Fortran => "Fortran",
        // Documentation
        Language::Markdown => "Markdown",
        Language::Unknown => "Unknown",
    }
}

/// Get language server name
pub fn language_server_name(language: Language) -> &'static str {
    match language {
        // Systems
        Language::Rust => "rust-analyzer",
        Language::Cpp => "clangd",
        Language::Zig => "zls",
        // JVM
        Language::Java => "jdtls",
        Language::Kotlin => "kotlin-language-server",
        Language::Scala => "metals",
        Language::Clojure => "clojure-lsp",
        // .NET
        Language::CSharp => "csharp-ls",
        Language::FSharp => "fsautocomplete",
        // Web
        Language::TypeScript | Language::JavaScript => "tsserver",
        Language::Vue => "volar",
        // Scripting
        Language::Python => "pyright",
        Language::Ruby => "ruby-lsp",
        Language::PHP => "intelephense",
        Language::Perl => "perlnavigator",
        Language::Lua => "lua-language-server",
        Language::Bash => "bash-language-server",
        Language::PowerShell => "powershell-editor-services",
        // Functional
        Language::Haskell => "haskell-language-server",
        Language::Elixir => "elixir-ls",
        Language::Erlang => "erlang_ls",
        Language::Elm => "elm-language-server",
        Language::OCaml => "ocamllsp",
        // Mobile/Application
        Language::Go => "gopls",
        Language::Swift => "sourcekit-lsp",
        Language::Dart => "dart-language-server",
        // Config/DevOps
        Language::Terraform => "terraform-ls",
        Language::Yaml => "yaml-language-server",
        Language::Toml => "taplo",
        Language::Nix => "nil",
        Language::Rego => "regal",
        // Scientific
        Language::R => "languageserver",
        Language::Julia => "LanguageServer.jl",
        Language::Fortran => "fortls",
        // Documentation
        Language::Markdown => "marksman",
        Language::Unknown => "unknown",
    }
}

/// Get alternative suggestion for unsupported features
pub fn get_alternative_suggestion(language: Language, feature: LspFeature) -> String {
    use LspFeature::*;

    match (language, feature) {
        // PHP alternatives
        (Language::PHP, Rename) => {
            "Rename requires Intelephense Premium. Try: symora search text \"<symbol>\"".into()
        }
        (Language::PHP, FindImplementations) => "Try: symora find refs <location>".into(),

        // Python alternatives
        (Language::Python, FindImplementations | IncomingCalls | OutgoingCalls) => {
            "Try: symora find refs <location> or symora search text \"<symbol>\"".into()
        }

        // Kotlin alternatives
        (Language::Kotlin, FindSymbol) => {
            "Only class-level symbols. Try: symora search ast \"class_declaration\" -l kotlin"
                .into()
        }
        (Language::Kotlin, FindReferences) => {
            "References may be incomplete. Try: symora search text \"<symbol>\"".into()
        }
        (Language::Kotlin, FindImplementations | IncomingCalls | OutgoingCalls) => {
            "Try: symora find refs <location>".into()
        }

        // TypeScript/JavaScript alternatives
        (Language::TypeScript | Language::JavaScript, Hover | Rename) => {
            "May timeout on large projects. Try: symora daemon restart".into()
        }
        (Language::TypeScript | Language::JavaScript, FindReferences) => {
            "May return incomplete results. Try: symora search text \"<symbol>\"".into()
        }

        // C/C++ alternatives
        (Language::Cpp, OutgoingCalls) => {
            "Outgoing calls not supported by clangd. Try: symora find refs <location>".into()
        }

        // Default
        _ => "This feature may not be available for this language.".into(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_rust_full_support() {
        assert_eq!(
            get_support_level(Language::Rust, LspFeature::FindImplementations),
            SupportLevel::Full
        );
    }

    #[test]
    fn test_python_no_impl_support() {
        assert_eq!(
            get_support_level(Language::Python, LspFeature::FindImplementations),
            SupportLevel::None
        );
    }

    #[test]
    fn test_unsupported_message() {
        let msg = get_unsupported_message(Language::Python, LspFeature::FindImplementations);
        assert!(msg.contains("Python"));
        assert!(msg.contains("pyright"));
        assert!(msg.contains("find implementations"));
    }
}
