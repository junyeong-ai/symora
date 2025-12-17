//! Symora - Symbol-centric Code Intelligence Library
//!
//! "Open the Gate to Code Structure"
//!
//! Provides LSP-based semantic code analysis with symbol-level precision
//! for AI coding agents.

pub mod app;
pub mod cli;
pub mod config;
pub mod daemon;
pub mod error;
pub mod infra;
pub mod models;
pub mod services;

pub use error::{SymoraError, SymoraResult};
