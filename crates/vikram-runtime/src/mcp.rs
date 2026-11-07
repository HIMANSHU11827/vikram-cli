//! MCP lives in the `vikram-mcp` crate: one client ([`McpRegistry`]) that owns the JSON-RPC wire,
//! one server for `vikram mcp serve`, and visibility. This module re-exports the client so runtime
//! code names it through the host, and config stays here because the runtime owns configuration.
//!
//! Connection failures are never swallowed: [`McpRegistry::connect_all`] returns
//! `Err` naming every server that failed, each [`ServerState`] carries its own
//! `error`, and the runtime surfaces the aggregate as a warning notice.

pub use vikram_mcp::client::{
    McpRegistry, PromptInfo, ResourceInfo, ServerSpec, ServerState, ToolInfo, parse_tools_response,
};

/// Attempt to connect every configured server, in order. One failing server
/// never stops the others, but this is `Err` when anything failed — the
/// returned registry still holds the servers that did connect, and each
/// [`ServerState`] records its own failure for per-server reporting.
///
/// Callers must handle the `Err` (log, emit a notice, collect per-server
/// errors) rather than treating "registry returned" as success.
pub async fn connect_all(registry: McpRegistry) -> (McpRegistry, Result<(), String>) {
    registry.connect_all().await
}
