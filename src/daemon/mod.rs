//! Daemon Module

pub mod client;
pub mod dto;
mod handlers;
pub mod protocol;
pub mod server;

pub use client::DaemonClient;
pub use protocol::{Request, RequestId, Response, RpcError};
pub use server::{DaemonConfig, DaemonServer};
