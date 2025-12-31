//! Daemon Module
//!
//! This module provides daemon functionality for persistent LSP server management.
//! It uses Unix domain sockets for IPC and is only available on Unix-like systems.
//!
//! # Platform Support
//!
//! This module requires Unix domain sockets and is not available on Windows.
//! On non-Unix platforms, LSP operations run in direct (non-daemon) mode.

pub mod client;
pub mod dto;
mod handlers;
pub mod protocol;
pub mod server;

pub use client::DaemonClient;
pub use protocol::{Request, RequestId, Response, RpcError};
pub use server::{DaemonConfig, DaemonServer};
