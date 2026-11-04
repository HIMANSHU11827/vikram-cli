//! VIKRAM MCP Ecosystem — one client, one server, visibility.
//!
//! This crate provides:
//! - The MCP client ([`client::McpRegistry`]): spawns and speaks JSON-RPC to third-party servers.
//!   The runtime owns configuration; this crate owns the wire.
//! - MCP Server (JSON-RPC 2.0 over STDIO) exposing approved VIKRAM tools
//! - Discovery (catalog of available MCP servers)
//! - Visibility management (Private/Public per-server toggle)

pub mod client;
pub mod discovery;
pub mod server;
pub mod visibility;

pub use client::{
    McpRegistry, PromptInfo, ResourceInfo, ServerSpec, ServerState, ToolInfo, parse_tools_response,
};
pub use discovery::{DiscoveryCatalog, ServerEntry};
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
