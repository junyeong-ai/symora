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
        (Language::TypeScript | Language::JavaScript, _) => Partial,

        // Python (pyright) - slow on large projects
        (Language::Python, FindImplementations) => None,
        (Language::Python, IncomingCalls | OutgoingCalls) => None,
        (Language::Python, _) => Partial,

        // Java (jdtls) - excellent support
        (Language::Java, _) => Full,

        // Kotlin (kotlin-lsp) - class-level only
        (Language::Kotlin, FindSymbol) => Partial,
        (Language::Kotlin, FindReferences) => Partial,
        (Language::Kotlin, FindImplementations) => None,
        (Language::Kotlin, GotoTypeDefinition) => Partial,
        (Language::Kotlin, IncomingCalls | OutgoingCalls) => None,
        (Language::Kotlin, Rename) => Partial,
        (Language::Kotlin, _) => Full,

        // PHP (intelephense) - rename requires premium
        (Language::PHP, Rename) => None,
        (Language::PHP, FindImplementations) => None,
        (Language::PHP, IncomingCalls | OutgoingCalls) => None,
        (Language::PHP, _) => Full,

        // C# (csharp-ls) - requires installation
        (Language::CSharp, _) => Partial,

        // Default for other languages
        _ => Partial,
    }
}

/// What to run instead when a server cannot answer a feature. It is followed
/// as written, so every command it names must be one the CLI accepts.
pub fn get_alternative_suggestion(language: Language, feature: LspFeature) -> String {
    use LspFeature::*;

    match (language, feature) {
        // PHP alternatives
        (Language::PHP, Rename) => {
            "Rename requires Intelephense Premium. Try: symora search content \"<symbol>\"".into()
        }
        (Language::PHP, FindImplementations) => "Try: symora refs <location>".into(),

        // Python alternatives
        (Language::Python, FindImplementations | IncomingCalls | OutgoingCalls) => {
            "Try: symora refs <location> or symora search content \"<symbol>\"".into()
        }

        // Kotlin alternatives
        (Language::Kotlin, FindSymbol) => {
            "Only class-level symbols. Try: symora search ast \"(class_declaration)\" -l kotlin"
                .into()
        }
        (Language::Kotlin, FindReferences) => {
            "References may be incomplete. Try: symora search content \"<symbol>\"".into()
        }
        (Language::Kotlin, FindImplementations | IncomingCalls | OutgoingCalls) => {
            "Try: symora refs <location>".into()
        }

        // TypeScript/JavaScript alternatives
        (Language::TypeScript | Language::JavaScript, FindReferences) => {
            "May return incomplete results. Try: symora search content \"<symbol>\"".into()
        }

        // C/C++ alternatives
        (Language::Cpp, OutgoingCalls) => {
            "Outgoing calls not supported by clangd. Try: symora refs <location>".into()
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
    fn every_suggested_command_parses() {
        use LspFeature::*;

        let features = [
            FindSymbol,
            FindReferences,
            GotoDefinition,
            GotoTypeDefinition,
            FindImplementations,
            Hover,
            Diagnostics,
            Rename,
            IncomingCalls,
            OutgoingCalls,
            CodeActions,
        ];
        for language in Language::all() {
            for feature in features {
                crate::cli::assert_named_commands_parse(&get_alternative_suggestion(
                    language, feature,
                ));
            }
        }
    }
}
