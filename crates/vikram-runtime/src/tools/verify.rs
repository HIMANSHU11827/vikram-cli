use serde_json::{Value, json};
use vikram_contracts::{ActionKind, PermissionRequest, SideEffect, Trust, VikramError};

use crate::tools::*;
pub(crate) struct VerifyRun;

impl Tool for VerifyRun {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "verify".into(),
            description: "Run the build+test verification loop (single attempt, no auto-repair) and return structured pass/fail plus git change evidence."
                .into(),
            schema: json!({
                "type": "object",
                "properties": {
                    "max_attempts": {"type": "integer", "description": "accepted for compatibility; only one attempt runs because no repair step is wired"},
                    "expected_changes": {"type": "array", "items": {"type": "string"}, "description": "workspace-relative paths you expect to have modified; compared against git status as warnings only, never affects pass/fail"},
                    "expect": {"type": "array", "items": {"type": "string"}, "description": "literal case-sensitive substrings you expect to find in the changed files' new content (e.g. \"fn handle_poll\", \"POLL_MARKER\"); reported per-pattern as found/missing, warnings only, never affects pass/fail"}
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
        // The repair closure below always declines, so a second attempt can never
        // change the outcome. Cap the knob at 1 rather than advertise retries
        // that silently never happen.
        let max_attempts = args
            .get("max_attempts")
            .and_then(Value::as_u64)
            .unwrap_or(1)
            .clamp(1, 1) as u32;
        let budget = crate::verification::Budget::attempts(max_attempts);
        let expected_changes: Vec<String> = args
            .get("expected_changes")
            .and_then(Value::as_array)
            .map(|items| {
                items
                    .iter()
                    .filter_map(Value::as_str)
                    .map(str::to_owned)
                    .collect()
            })
            .unwrap_or_default();
        let expect: Vec<String> = args
            .get("expect")
            .and_then(Value::as_array)
            .map(|items| {
                items
                    .iter()
                    .filter_map(Value::as_str)
                    .map(str::to_owned)
                    .collect()
            })
            .unwrap_or_default();
        let outcome = crate::verification::verify(
            &ctx.workspace_root,
            &crate::verification::VerificationPlan::cargo(),
            &budget,
            &ctx.sandbox,
            &expected_changes,
            &expect,
            |_, _| None,
        )?;
        let steps = outcome
            .steps
            .iter()
            .map(|s| {
                let mut entry = json!({"name": s.name, "ok": s.ok, "duration_ms": s.duration_ms});
                if !s.ok {
                    entry["detail"] = json!(output_tail(&s.output, MAX_STEP_DETAIL));
                }
                entry
            })
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
            "changes": {
                "files": outcome.changes.files,
                "note": outcome.changes.note,
                "warnings": outcome.changes.warnings,
                "expectations": outcome
                    .changes
                    .expectations
                    .iter()
                    .map(|e| {
                        json!({
                            "pattern": e.pattern,
                            "found_in": e.found_in,
                            "found": e.found,
                            "checked": e.checked,
                        })
                    })
                    .collect::<Vec<_>>(),
            },
        });
        // The verdict leads the output on its own line: the growth pipeline reads it even when
        // the JSON report below is truncated or spooled.
        let verdict = if outcome.ok {
            "verify: PASS".to_owned()
        } else {
            match &outcome.failure {
                Some((step, kind)) => {
                    format!("verify: FAIL step={step} kind={}", kind.as_str())
                }
                None => "verify: FAIL".to_owned(),
            }
        };
        Ok(ToolOutput {
            content: truncated(format!("{verdict}\n{}", payload)),
            ok: outcome.ok,
        })
    }
}

use std::time::Duration;

/// How much of a failing step's output the JSON report keeps per step. The tail carries the
/// error; `failure_output` already holds the fuller (head-truncated) capture.
const MAX_STEP_DETAIL: usize = 500;

/// The last `max` chars of `text`, trimmed.
fn output_tail(text: &str, max: usize) -> String {
    let trimmed = text.trim();
    let chars: Vec<char> = trimmed.chars().collect();
    if chars.len() <= max {
        trimmed.to_owned()
    } else {
        chars[chars.len() - max..].iter().collect()
    }
}

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
                // The resource is what the approval prompt and the journal show. Recording only
                // the first word (`test_run cargo`) hid the rest of an arbitrary `sh -c` string —
                // the operator approved a description narrower than the command that ran. Like
                // `shell.run`, an arbitrary command is never a routine local mutation: `Smart`
                // must not auto-allow it.
                let side_effect = if looks_destructive(command) {
                    SideEffect::Destructive
                } else {
                    SideEffect::ExternalChange
                };
                Ok(PermissionRequest::new(
                    ActionKind::Execute,
                    $verb,
                    command,
                    side_effect,
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
                ctx.sandbox.wrap_command(&mut cmd, &ctx.workspace_root);
                cmd.current_dir(&ctx.workspace_root);
                let sandbox_note = ctx.sandbox.enforcement_notice();

                let output = match crate::verification::wait_with_timeout(
                    &mut cmd,
                    Duration::from_millis(timeout_ms),
                    ctx.cancel.as_ref(),
                ) {
                    Ok(Some(output)) => output,
                    Ok(None) => {
                        return Ok(ToolOutput {
                            content: format!("command timed out after {timeout_ms} ms"),
                            ok: false,
                        });
                    }
                    Err(e) => {
                        return Ok(ToolOutput {
                            content: e,
                            ok: false,
                        });
                    }
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
                append_sandbox_note(&mut body, &sandbox_note);
                Ok(ToolOutput {
                    content: truncated(body),
                    ok: output.status.success(),
                })
            }
        }
    };
}

cmd_check_tool!(
    TestRunTool,
    "test_run",
    "cargo test",
    "test_run",
    "Run a test command."
);
cmd_check_tool!(
    TestRunner,
    "test_run",
    "cargo test",
    "test_run",
    "Run a test command."
);
cmd_check_tool!(
    TestRunCommand,
    "test_run",
    "cargo test",
    "test_run",
    "Run a test command."
);
cmd_check_tool!(
    TestCommand,
    "test_run",
    "cargo test",
    "test_run",
    "Run a test command."
);
cmd_check_tool!(
    RunTests,
    "test_run",
    "cargo test",
    "test_run",
    "Run a test command."
);
cmd_check_tool!(
    RunTest,
    "test_run",
    "cargo test",
    "test_run",
    "Run a test command."
);

cmd_check_tool!(
    BuildCheck,
    "build_check",
    "cargo build",
    "build_check",
    "Run a build command."
);
cmd_check_tool!(
    BuildCheckTool,
    "build_check",
    "cargo build",
    "build_check",
    "Run a build command."
);
cmd_check_tool!(
    BuildChecker,
    "build_check",
    "cargo build",
    "build_check",
    "Run a build command."
);
cmd_check_tool!(
    BuildValidation,
    "build_check",
    "cargo build",
    "build_check",
    "Run a build command."
);
cmd_check_tool!(
    BuildValidate,
    "build_check",
    "cargo build",
    "build_check",
    "Run a build command."
);
cmd_check_tool!(
    CheckBuild,
    "build_check",
    "cargo build",
    "build_check",
    "Run a build command."
);
cmd_check_tool!(
    BuildCommand,
    "build_check",
    "cargo build",
    "build_check",
    "Run a build command."
);

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
        assert_eq!(req.side_effect, SideEffect::ExternalChange);
    }

    #[test]
    fn build_check_declares_execute_permission() {
        let req = BuildCheck
            .permission(&json!({"command": "cargo build"}))
            .unwrap();
        assert_eq!(req.action, ActionKind::Execute);
        assert_eq!(req.verb, "build_check");
        assert_eq!(req.side_effect, SideEffect::ExternalChange);
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
            bot: None,
            cancel: None,
        };
        let out = TestRunTool
            .run(
                &json!({"command": "echo hello-test", "timeout_ms": 5000}),
                &ctx,
            )
            .unwrap();
        assert!(out.ok, "{}", out.content);
        assert!(out.content.contains("hello-test"), "{}", out.content);
    }

    #[test]
    fn output_tail_keeps_the_end_within_the_bound() {
        let long = format!("{}\nerror: the failure", "y".repeat(2_000));
        let tail = output_tail(&long, MAX_STEP_DETAIL);
        assert!(tail.chars().count() <= MAX_STEP_DETAIL);
        assert!(tail.contains("error: the failure"));
        assert_eq!(output_tail("short", MAX_STEP_DETAIL), "short");
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
            bot: None,
            cancel: None,
        };
        let out = BuildCheck
            .run(
                &json!({"command": "echo hello-build", "timeout_ms": 5000}),
                &ctx,
            )
            .unwrap();
        assert!(out.ok, "{}", out.content);
        assert!(out.content.contains("hello-build"), "{}", out.content);
    }
}
