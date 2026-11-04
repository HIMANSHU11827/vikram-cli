use serde_json::{Value, json};
use vikram_contracts::{ActionKind, PermissionRequest, SideEffect, Trust, VikramError};

use crate::tools::*;
pub(crate) struct VerifyRun;

impl Tool for VerifyRun {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "verify".into(),
            description: "Run the build+test verification loop and return structured pass/fail."
                .into(),
            schema: json!({
                "type": "object",
                "properties": {
                    "max_attempts": {"type": "integer"}
                }
            }),
        }
    }

    fn permission(&self, _args: &Value) -> Result<PermissionRequest, VikramError> {
        Ok(PermissionRequest::new(
            ActionKind::Execute,
            "verify",
            "cargo",
            SideEffect::LocalMutation,
            Trust::Model,
        ))
    }

    fn run(&self, args: &Value, ctx: &ToolContext) -> Result<ToolOutput, VikramError> {
        let max_attempts = args
            .get("max_attempts")
            .and_then(Value::as_u64)
            .unwrap_or(1)
            .clamp(1, 10) as u32;
        let budget = crate::verification::Budget::attempts(max_attempts);
        let outcome = crate::verification::verify(
            &ctx.workspace_root,
            &crate::verification::VerificationPlan::cargo(),
            &budget,
            |_, _| None,
        )?;
        let steps = outcome
            .steps
            .iter()
            .map(|s| json!({"name": s.name, "ok": s.ok, "duration_ms": s.duration_ms}))
            .collect::<Vec<_>>();
        let payload = json!({
            "ok": outcome.ok,
            "attempts": outcome.attempts,
            "failure": outcome
                .failure
                .as_ref()
                .map(|(n, k)| json!({"step": n, "kind": k.as_str()})),
            "failure_output": outcome
                .steps
                .iter()
                .rev()
                .find(|s| !s.ok)
                .map(|s| truncated(s.output.clone())),
            "steps": steps,
        });
        Ok(ToolOutput {
            content: truncated(payload.to_string()),
            ok: outcome.ok,
        })
    }
}
