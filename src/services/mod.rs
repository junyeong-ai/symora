//! Service layer for Symora

pub mod ast_query;
pub mod config;
#[cfg(unix)]
pub mod daemon_lsp;
#[cfg(unix)]
pub mod daemon_store;
pub mod dist;
pub mod embedding_cache;
pub mod embeddings;
pub mod imports;
pub mod lsp;
pub mod pack;
mod pack_cache;
pub mod project;
pub mod store;
pub mod test_scope;

pub use ast_query::{AstQueryService, DefaultAstQueryService};
pub use config::{ConfigService, DefaultConfigService};
#[cfg(unix)]
pub use daemon_lsp::DaemonLspService;
#[cfg(unix)]
pub use daemon_store::DaemonStoreService;
pub use lsp::{DefaultLspService, LspService};
pub use project::{DefaultProjectService, ProjectService};
pub use store::{DefaultStoreService, Store, StoreService};
pub use test_scope::{TestClassifier, TestScope};
