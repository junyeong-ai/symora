pub mod config;
pub mod diagnostic;
pub mod lsp;
pub mod symbol;

// Re-export commonly used types
pub use config::SymoraConfig;
pub use diagnostic::Diagnostic;
pub use lsp::{
    ApplyActionResult, CallHierarchyItem, CodeAction, CodeActionKind, FileChangeWithEdits,
    HoverInfo, Position, Range, RenameResult, TextEdit, WorkspaceEdit,
};
pub use symbol::{Language, Location, Symbol, SymbolKind};
