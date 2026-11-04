use serde_json::Value;
use std::sync::Arc;
use vikram_contracts::{ActionKind, PermissionRequest, SideEffect, Trust, VikramError};

use crate::mcp::McpRegistry;

use crate::tools::*;
pub struct McpTool {
    pub server: String,
    pub tool: String,
    pub description: String,
    pub schema: Value,
    pub registry: Arc<std::sync::Mutex<McpRegistry>>,
}

impl Tool for McpTool {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: mcp_tool_name(&self.server, &self.tool),
            description: if self.description.is_empty() {
                format!("MCP tool {} on {}", self.tool, self.server)
            } else {
                self.description.clone()
            },
            schema: self.schema.clone(),
        }
    }

    fn permission(&self, _args: &Value) -> Result<PermissionRequest, VikramError> {
        Ok(PermissionRequest::new(
            ActionKind::Execute,
            "mcp.call",
            format!("{}.{}", self.server, self.tool),
            SideEffect::LocalMutation,
            Trust::Model,
        ))
    }

    #[allow(clippy::await_holding_lock)]
    fn run(&self, args: &Value, _ctx: &ToolContext) -> Result<ToolOutput, VikramError> {
        let registry = Arc::clone(&self.registry);
        let server = self.server.clone();
        let tool = self.tool.clone();
        let arguments = args.clone();
        let result = match tokio::runtime::Handle::try_current() {
            Ok(handle) => handle.block_on(async {
                let mut guard = registry.lock().expect("mcp lock");
                guard.call_tool(&server, &tool, arguments).await
            }),
            Err(_) => {
                let rt = tokio::runtime::Runtime::new()
                    .map_err(|e| VikramError::EffectFailed(e.to_string()))?;
                rt.block_on(async {
                    let mut guard = registry.lock().expect("mcp lock");
                    guard.call_tool(&server, &tool, arguments).await
                })
            }
        };
        match result {
            Ok(text) => Ok(ToolOutput {
                content: truncated(text),
                ok: true,
            }),
            Err(err) => Ok(ToolOutput {
                content: err,
                ok: false,
            }),
        }
    }
}
