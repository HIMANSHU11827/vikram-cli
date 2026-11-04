//! MCP lives in the `vikram-mcp` crate: one client ([`McpRegistry`]) that owns the JSON-RPC wire,
//! one server for `vikram mcp serve`, and visibility. This module re-exports the client so runtime
//! code names it through the host, and config stays here because the runtime owns configuration.

pub use vikram_mcp::client::{
    McpRegistry, PromptInfo, ResourceInfo, ServerSpec, ServerState, ToolInfo, parse_tools_response,
};
