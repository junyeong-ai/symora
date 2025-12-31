//! Symora - Symbol-centric Code Intelligence Library
//!
//! "Open the Gate to Code Structure"
//!
//! Provides LSP-based semantic code analysis with symbol-level precision
//! for AI coding agents.
//!
//! # Platform Support
//!
//! Symora uses Unix domain sockets for daemon IPC, which are only available
//! on Unix-like systems (Linux, macOS, BSD). The daemon module is not
//! available on Windows.

pub mod app;
pub mod cli;
pub mod config;
#[cfg(unix)]
pub mod daemon;
pub mod error;
pub mod infra;
pub mod models;
pub mod services;

pub use error::{SymoraError, SymoraResult};
