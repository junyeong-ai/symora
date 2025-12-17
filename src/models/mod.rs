//! Data models for Symora
//!
//! Contains core type definitions used throughout the application.

pub mod config;
pub mod diagnostic;
pub mod lsp;
pub mod symbol;

// Re-export commonly used types
pub use config::SymoraConfig;
pub use diagnostic::Diagnostic;
pub use lsp::{
    ApplyActionResult, CallHierarchyItem, CodeAction, CodeActionKind, FileChange,
    FileChangeWithEdits, HoverInfo, Position, Range, RenameResult, ServerStatus, TextEdit,
    WorkspaceEdit,
};
pub use symbol::{Language, Location, Symbol, SymbolKind};
