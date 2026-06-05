//! # Symora
//!
//! Symbol-centric code intelligence for AI coding agents.
//!
//! Symora is a CLI-first tool that combines LSP-driven semantic navigation,
//! tree-sitter AST queries, and a SQLite-backed symbol/content index into
//! a single, machine-consumable JSON surface. The primary consumer is an
//! AI agent invoking commands through a shell or the MCP protocol.
//!
//! ## Capabilities
//!
//! - **Navigation** — `def`, `refs`, `callers`, `callees`, `implementations`,
//!   `supertypes`, `subtypes`, `typedef`, `hover`, `signature`
//! - **Analysis** — `context`, `impact` (with N-hop blast radius),
//!   `diff-impact`, `usage`, `diagnostics`
//! - **Discovery** — `search symbols`, `search content`, `search ast`,
//!   `search semantic` (opt-in embeddings), `map`, `pack`
//! - **Editing** — `write replace-body`, `write insert-before`,
//!   `write insert-after`, `rename`, `actions`
//! - **Operations** — `bench`, `daemon`, `mcp serve` (stdio + HTTP),
//!   workspace fan-out across multiple repos
//!
//! ## Crate layout
//!
//! - [`app`] — runtime composition (services, output, config)
//! - [`cli`] — command surface, output contract, analysis helpers
//! - [`services`] — LSP, AST, project, store, pack, embeddings
//! - [`infra`] — file filtering, LSP transport, AST parser pool
//! - [`mcp`] — Model Context Protocol server (stdio + HTTP)
//! - [`models`] — domain types shared across layers
//! - [`error`] — strongly-typed error enums per service
//!
//! Layer rule: `cli` and `mcp` depend on `services`, which depend on
//! `infra` and `models`. `services` and below never import from `cli`.
//!
//! ## Output contract
//!
//! Every command emits a single JSON document on stdout. List-shaped
//! responses use [`cli::response::Section`]. Errors go through
//! [`cli::OutputError`] with a stable [`cli::ErrorCode`] taxonomy so
//! agents can branch on classification rather than parsing prose.

pub mod app;
pub mod cli;
pub mod config;
pub mod constants;
#[cfg(unix)]
pub mod daemon;
pub mod error;
pub mod infra;
pub mod mcp;
pub mod models;
pub mod services;
pub mod utils;
