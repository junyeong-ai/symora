//! LSP Infrastructure for Symora
//!
//! Provides race-safe Language Server Protocol communication:
//! - JSON-RPC 2.0 protocol types
//! - Async message transport with proper framing
//! - Thread-safe client with atomic request IDs
//! - Server manager for multiple language servers
//! - Language capabilities matrix

pub mod capabilities;
pub mod client;
pub mod health;
pub mod init_options;
pub mod manager;
pub mod protocol;
pub mod servers;
pub mod transport;

pub use capabilities::{
    LspFeature, SupportLevel, get_alternative_suggestion, get_support_level,
    get_unsupported_message, is_feature_supported, language_display_name, language_server_name,
};
pub use client::{DocumentSyncGuard, HealthStatus, IndexingState, LspClient};
pub use health::HealthMonitor;
pub use manager::{LspManager, ServerStatus};
pub use servers::ServerConfig;
