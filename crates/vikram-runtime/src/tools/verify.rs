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

use std::time::Duration;

macro_rules! cmd_check_tool {
    ($struct:ident, $tool:expr, $default:expr, $verb:expr, $desc:expr) => {
        #[allow(dead_code)]
        pub(crate) struct $struct;

        impl Tool for $struct {
            fn spec(&self) -> ToolSpec {
                ToolSpec {
                    name: $tool.into(),
                    description: $desc.into(),
                    schema: json!({
                        "type": "object",
                        "properties": {
                            "command": {"type": "string", "description": "shell command to run"},
                            "timeout_ms": {"type": "integer", "description": "timeout in milliseconds"}
                        }
                    }),
                }
            }

            fn permission(&self, args: &Value) -> Result<PermissionRequest, VikramError> {
                let command = args
                    .get("command")
                    .and_then(Value::as_str)
                    .unwrap_or($default);
                let head = command.split_whitespace().next().unwrap_or("cargo");
                Ok(PermissionRequest::new(
                    ActionKind::Execute,
                    $verb,
                    head,
                    SideEffect::LocalMutation,
                    Trust::Model,
                ))
            }

            fn run(&self, args: &Value, ctx: &ToolContext) -> Result<ToolOutput, VikramError> {
                let command = args
                    .get("command")
                    .and_then(Value::as_str)
                    .unwrap_or($default)
                    .to_owned();
                let timeout_ms = args
                    .get("timeout_ms")
                    .and_then(Value::as_u64)
                    .unwrap_or(120_000)
                    .clamp(1_000, 300_000);

                let mut cmd = crate::verification::shell_cmd(&command);
                cmd.current_dir(&ctx.workspace_root);

                let Some(output) =
                    crate::verification::wait_with_timeout(&mut cmd, Duration::from_millis(timeout_ms))
                else {
                    return Ok(ToolOutput {
                        content: format!("command timed out after {timeout_ms} ms"),
                        ok: false,
                    });
                };

                let mut body = String::from_utf8_lossy(&output.stdout).to_string();
                let stderr = String::from_utf8_lossy(&output.stderr);
                if !stderr.trim().is_empty() {
                    if !body.is_empty() && !body.ends_with('\n') {
                        body.push('\n');
                    }
                    body.push_str("[stderr]\n");
                    body.push_str(&stderr);
                }
                if body.trim().is_empty() {
                    body = format!("(no output, exit {:?})", output.status.code());
                }
                body.push_str(&format!("\n[exit {}]", output.status.code().unwrap_or(-1)));
                Ok(ToolOutput {
                    content: truncated(body),
                    ok: output.status.success(),
                })
            }
        }
    };
}

cmd_check_tool!(TestRunTool, "test_run", "cargo test", "test_run", "Run a test command.");
cmd_check_tool!(TestRunner, "test_run", "cargo test", "test_run", "Run a test command.");
cmd_check_tool!(TestRunCommand, "test_run", "cargo test", "test_run", "Run a test command.");
cmd_check_tool!(TestCommand, "test_run", "cargo test", "test_run", "Run a test command.");
cmd_check_tool!(RunTests, "test_run", "cargo test", "test_run", "Run a test command.");
cmd_check_tool!(RunTest, "test_run", "cargo test", "test_run", "Run a test command.");

cmd_check_tool!(BuildCheck, "build_check", "cargo build", "build_check", "Run a build command.");
cmd_check_tool!(BuildCheckTool, "build_check", "cargo build", "build_check", "Run a build command.");
cmd_check_tool!(BuildChecker, "build_check", "cargo build", "build_check", "Run a build command.");
cmd_check_tool!(BuildValidation, "build_check", "cargo build", "build_check", "Run a build command.");
cmd_check_tool!(BuildValidate, "build_check", "cargo build", "build_check", "Run a build command.");
cmd_check_tool!(CheckBuild, "build_check", "cargo build", "build_check", "Run a build command.");
cmd_check_tool!(BuildCommand, "build_check", "cargo build", "build_check", "Run a build command.");

#[cfg(test)]
mod cmd_check_tests {
    use super::*;
    use crate::tools::*;
    use serde_json::json;

    #[test]
    fn test_run_tool_is_registered() {
        let registry = ToolRegistry::with_builtins();
        assert!(registry.names().contains(&"test_run".to_string()));
    }

    #[test]
    fn build_check_is_registered() {
        let registry = ToolRegistry::with_builtins();
        assert!(registry.names().contains(&"build_check".to_string()));
    }

    #[test]
    fn test_run_declares_execute_permission() {
        let req = TestRunTool
            .permission(&json!({"command": "cargo test"}))
            .unwrap();
        assert_eq!(req.action, ActionKind::Execute);
        assert_eq!(req.verb, "test_run");
        assert_eq!(req.side_effect, SideEffect::LocalMutation);
    }

    #[test]
    fn build_check_declares_execute_permission() {
        let req = BuildCheck
            .permission(&json!({"command": "cargo build"}))
            .unwrap();
        assert_eq!(req.action, ActionKind::Execute);
        assert_eq!(req.verb, "build_check");
        assert_eq!(req.side_effect, SideEffect::LocalMutation);
    }

    #[test]
    fn test_run_runs_command() {
        use tempfile::tempdir;
        let dir = tempdir().unwrap();
        let ctx = ToolContext {
            workspace_root: dir.path().to_path_buf(),
            sandbox: crate::sandbox::Sandbox::disabled("test"),
            spill: crate::spill::Spill::disabled(),
            runtime: None,
            session: None,
            agent: None,
            cancel: None,
        };
        let out = TestRunTool
            .run(&json!({"command": "echo hello-test", "timeout_ms": 5000}), &ctx)
            .unwrap();
        assert!(out.ok, "{}", out.content);
        assert!(out.content.contains("hello-test"), "{}", out.content);
    }

    #[test]
    fn build_check_runs_command() {
        use tempfile::tempdir;
        let dir = tempdir().unwrap();
        let ctx = ToolContext {
            workspace_root: dir.path().to_path_buf(),
            sandbox: crate::sandbox::Sandbox::disabled("test"),
            spill: crate::spill::Spill::disabled(),
            runtime: None,
            session: None,
            agent: None,
            cancel: None,
        };
        let out = BuildCheck
            .run(&json!({"command": "echo hello-build", "timeout_ms": 5000}), &ctx)
            .unwrap();
        assert!(out.ok, "{}", out.content);
        assert!(out.content.contains("hello-build"), "{}", out.content);
    }
}
