use serde_json::{Value, json};
use vikram_contracts::{ActionKind, PermissionRequest, SideEffect, Trust, VikramError};

use crate::tools::*;
pub struct AskUser;

impl Tool for AskUser {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "ask_user".into(),
            description: "Ask the user a question and get a response.".into(),
            schema: json!({
                "type": "object",
                "properties": {
                    "question": {"type": "string"},
                    "choices": {"type": "array", "items": {"type": "string"}}
                },
                "required": ["question"]
            }),
        }
    }

    fn permission(&self, args: &Value) -> Result<PermissionRequest, VikramError> {
        let question = args
            .get("question")
            .and_then(Value::as_str)
            .unwrap_or("user");
        Ok(PermissionRequest::new(
            ActionKind::Read,
            "ask_user",
            question,
            SideEffect::None,
            Trust::Model,
        ))
    }

    fn run(&self, args: &Value, _ctx: &ToolContext) -> Result<ToolOutput, VikramError> {
        let question = arg_str(args, "question")?;
        let choices = args
            .get("choices")
            .and_then(Value::as_array)
            .map(|arr| arr.iter().filter_map(Value::as_str).collect::<Vec<_>>())
            .unwrap_or_default();

        static COUNTER: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let seq = COUNTER.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let request_id = format!("ask-{}-{}", crate::types::now_ms(), seq);

        let choices_json = serde_json::to_string(&choices).unwrap_or_else(|_| "[]".to_string());
        let content = format!(
            "QUESTION:{request_id}\n{question}\nChoices: {choices_json}\nStatus: awaiting_user_response"
        );
        Ok(ToolOutput { content, ok: true })
    }
}

// Alias for compatibility with alternate naming.
pub type Ask = AskUser;

pub struct PermissionCheck;

impl Tool for PermissionCheck {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "permission.check".into(),
            description: "Check if an action is permitted.".into(),
            schema: json!({
                "type": "object",
                "properties": {
                    "verb": {"type": "string"},
                    "resource": {"type": "string"},
                    "action_kind": {"type": "string", "enum": ["read", "write", "execute", "network", "delegate"]}
                },
                "required": ["verb", "resource"]
            }),
        }
    }

    fn permission(&self, args: &Value) -> Result<PermissionRequest, VikramError> {
        let resource = args
            .get("resource")
            .and_then(Value::as_str)
            .unwrap_or("permission");
        Ok(PermissionRequest::new(
            ActionKind::Read,
            "permission.check",
            resource,
            SideEffect::None,
            Trust::Model,
        ))
    }

    fn run(&self, args: &Value, ctx: &ToolContext) -> Result<ToolOutput, VikramError> {
        let verb = arg_str(args, "verb")?;
        let resource = arg_str(args, "resource")?;
        let action = match args
            .get("action_kind")
            .and_then(Value::as_str)
            .unwrap_or("read")
        {
            "write" => ActionKind::Write,
            "execute" => ActionKind::Execute,
            "network" => ActionKind::Network,
            "delegate" => ActionKind::Delegate,
            "spend" => ActionKind::Spend,
            "admin" => ActionKind::Admin,
            _ => ActionKind::Read,
        };
        let Some(runtime) = ctx.runtime.as_ref() else {
            return Ok(ToolOutput {
                content: "Permission check: no runtime policy in this context — the answer would be invented".to_owned(),
                ok: false,
            });
        };
        // Ask the same judge a real call goes through, so the answer is the decision the runtime
        // would actually make — not a fabricated "allowed".
        let request = PermissionRequest::new(
            action,
            verb.clone(),
            resource.clone(),
            SideEffect::LocalMutation,
            Trust::Model,
        );
        let decision = runtime.judge(&request);
        let verdict = match &decision {
            vikram_contracts::Decision::Allow => "allowed".to_owned(),
            vikram_contracts::Decision::Deny { reason } => format!("denied: {reason}"),
            vikram_contracts::Decision::RequireApproval { reason, .. } => {
                format!("requires approval: {reason}")
            }
        };
        Ok(ToolOutput {
            content: format!("Permission check: {verb} on {resource} → {verdict}"),
            ok: decision.is_allowed(),
        })
    }
}

// Aliases for compatibility with alternate naming.
pub type PermissionManager = PermissionCheck;
pub type Permission = PermissionCheck;
