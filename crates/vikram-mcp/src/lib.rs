//! VIKRAM MCP Ecosystem — Server, Private Runtime, Discovery, Visibility.
//!
//! This crate provides:
//! - MCP Server (JSON-RPC 2.0 over STDIO) exposing approved VIKRAM tools
//! - Private MCP Runtime (process lifecycle management for third-party servers)
//! - Discovery (catalog of available MCP servers)
//! - Visibility management (Private/Public per-server toggle)

pub mod discovery;
pub mod runtime;
pub mod server;
pub mod visibility;

pub use discovery::{DiscoveryCatalog, ServerEntry};
pub use runtime::{PrivateRuntime, ServerInstance, ServerStatus};
pub use server::{McpServer, McpServerConfig as ServerConfig};
pub use visibility::{Visibility, VisibilityManager};

use thiserror::Error;

#[derive(Debug, Error)]
pub enum McpError {
    #[error("server not found: {name}")]
    NotFound { name: String },
    #[error("server already running: {name}")]
    AlreadyRunning { name: String },
    #[error("server not running: {name}")]
    NotRunning { name: String },
    #[error("spawn failed: {0}")]
    Spawn(String),
    #[error("io error: {0}")]
    Io(String),
    #[error("json error: {0}")]
    Json(String),
    #[error("permission denied: {0}")]
    PermissionDenied(String),
    #[error("invalid config: {0}")]
    InvalidConfig(String),
}

pub type Result<T> = std::result::Result<T, McpError>;
