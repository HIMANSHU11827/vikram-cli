use serde_json::Value;
use std::sync::Arc;
use vikram_contracts::{ActionKind, PermissionRequest, SideEffect, Trust, VikramError};
use vikram_mcp::client::ToolAnnotations;

use crate::mcp::McpRegistry;

use crate::tools::*;
pub struct McpTool {
    pub server: String,
    pub tool: String,
    pub description: String,
    pub schema: Value,
    /// The hints the server published in `tools/list`. A missing hint means unknown, and unknown is
    /// classified cautiously — never as a local mutation.
    pub annotations: ToolAnnotations,
    pub registry: Arc<std::sync::Mutex<McpRegistry>>,
    pub visibility: Arc<std::sync::Mutex<vikram_mcp::visibility::VisibilityManager>>,
}

/// How a remote tool is judged, from the server's own hints where it gave them and the cautious
/// default where it did not.
///
/// A remote tool is never [`SideEffect::LocalMutation`]: `Smart` and `AcceptEdits` auto-approve an
/// `Execute` + `LocalMutation` call, so classifying a server's `send_email` that way is how a remote
/// server could act without a prompt. Read-only is the only tier that needs no human; everything
/// else asks.
pub(crate) fn classify(annotations: &ToolAnnotations) -> (ActionKind, SideEffect) {
    // `destructiveHint` only qualifies a non-read-only tool, so read-only wins when both are set.
    if annotations.read_only_hint == Some(true) {
        (ActionKind::Read, SideEffect::None)
    } else if annotations.destructive_hint == Some(true) {
        (ActionKind::Execute, SideEffect::Destructive)
    } else {
        (ActionKind::Execute, SideEffect::ExternalChange)
    }
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
        let (action, side_effect) = classify(&self.annotations);
        Ok(PermissionRequest::new(
            action,
            "mcp.call",
            // The resource stays `server.tool` so the prompt names what is being asked for.
            format!("{}.{}", self.server, self.tool),
            side_effect,
            Trust::Model,
        ))
    }

    #[allow(clippy::await_holding_lock)]
    fn run(&self, args: &Value, _ctx: &ToolContext) -> Result<ToolOutput, VikramError> {
        // "Private" is a real gate, not a label: a private server's tools do not run.
        let is_public = self
            .visibility
            .lock()
            .map(|m| m.is_public(&self.server))
            .unwrap_or(false);
        if !is_public {
            return Err(VikramError::EffectFailed(format!(
                "MCP server `{}` is private — its tools are not exposed",
                self.server
            )));
        }
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

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use vikram_contracts::{Decision, PermissionMode, Posture};

    fn tool(name: &str, annotations: ToolAnnotations) -> McpTool {
        McpTool {
            server: "remote".into(),
            tool: name.into(),
            description: String::new(),
            schema: json!({"type": "object"}),
            annotations,
            registry: Arc::new(std::sync::Mutex::new(McpRegistry::load(Vec::new()))),
            visibility: Arc::new(std::sync::Mutex::new(
                vikram_mcp::visibility::VisibilityManager::new(),
            )),
        }
    }

    fn read_only() -> ToolAnnotations {
        ToolAnnotations {
            read_only_hint: Some(true),
            ..ToolAnnotations::default()
        }
    }

    fn destructive() -> ToolAnnotations {
        ToolAnnotations {
            destructive_hint: Some(true),
            ..ToolAnnotations::default()
        }
    }

    /// What a policy rule that asks would have produced before the posture is applied.
    fn asks() -> Decision {
        Decision::RequireApproval {
            reason: "policy asks".into(),
            amendment: None,
        }
    }

    fn smart(decision: Decision, request: &PermissionRequest) -> Decision {
        PermissionMode::Smart.resolve(
            decision,
            request,
            Posture {
                workspace_root: Some("/tmp/ws"),
                listed: false,
            },
        )
    }

    #[test]
    fn a_read_only_mcp_tool_is_a_read_with_no_side_effect() {
        let request = tool("search", read_only()).permission(&json!({})).unwrap();
        assert_eq!(request.action, ActionKind::Read);
        assert_eq!(request.side_effect, SideEffect::None);
        assert_eq!(request.resource, "remote.search");
    }

    #[test]
    fn a_destructive_mcp_tool_is_destructive() {
        let request = tool("drop", destructive()).permission(&json!({})).unwrap();
        assert_eq!(request.side_effect, SideEffect::Destructive);
    }

    #[test]
    fn read_only_wins_over_a_destructive_hint() {
        let both = ToolAnnotations {
            read_only_hint: Some(true),
            destructive_hint: Some(true),
            ..ToolAnnotations::default()
        };
        let request = tool("both", both).permission(&json!({})).unwrap();
        assert_eq!(request.action, ActionKind::Read);
        assert_eq!(request.side_effect, SideEffect::None);
    }

    #[test]
    fn an_unannotated_mcp_tool_is_an_external_change_never_a_local_mutation() {
        let request = tool("send_email", ToolAnnotations::default())
            .permission(&json!({}))
            .unwrap();
        assert_eq!(request.action, ActionKind::Execute);
        assert_eq!(
            request.side_effect,
            SideEffect::ExternalChange,
            "LocalMutation is what Smart auto-approves, so it must never be the default here"
        );
    }

    #[test]
    fn an_unannotated_mcp_tool_is_not_auto_approved_under_smart_mode() {
        let request = tool("send_email", ToolAnnotations::default())
            .permission(&json!({}))
            .unwrap();
        assert!(
            matches!(smart(asks(), &request), Decision::RequireApproval { .. }),
            "a remote tool with no hints must still reach the operator under Smart"
        );
    }

    #[test]
    fn a_destructive_mcp_tool_is_not_auto_approved_under_smart_mode() {
        let request = tool("drop", destructive()).permission(&json!({})).unwrap();
        assert!(matches!(
            smart(asks(), &request),
            Decision::RequireApproval { .. }
        ));
    }

    #[test]
    fn a_read_only_mcp_tool_does_not_prompt_under_smart_mode() {
        let request = tool("search", read_only()).permission(&json!({})).unwrap();
        assert!(
            matches!(smart(asks(), &request), Decision::Allow),
            "a tool the server promises cannot change anything needs no human"
        );
    }
}
