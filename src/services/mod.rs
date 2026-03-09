//! Service layer for Symora

pub mod ast_query;
pub mod config;
#[cfg(unix)]
pub mod daemon_lsp;
pub mod lsp;
pub mod project;
pub mod store;

pub use ast_query::{AstQueryService, DefaultAstQueryService};
pub use config::{ConfigService, DefaultConfigService};
#[cfg(unix)]
pub use daemon_lsp::DaemonLspService;
pub use lsp::{DefaultLspService, LspService};
pub use project::{DefaultProjectService, ProjectService};
pub use store::Store;
