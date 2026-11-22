use serde_json::{Value, json};
use vikram_contracts::{ActionKind, PermissionRequest, SideEffect, Trust, VikramError};

use crate::tools::*;
pub(crate) struct VerifyRun;

impl Tool for VerifyRun {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "verify".into(),
            description: "Run the build+test verification loop and return structured pass/fail plus \
                          git change evidence. Strict by default: the run only passes when the change \
                          it was asked to prove is actually there, so declare `expect` and/or \
                          `expected_changes` with every call."
                .into(),
            schema: json!({
                "type": "object",
                "properties": {
                    "strict": {"type": "boolean", "description": "default true: a missing, unsatisfied, or uncheckable expectation fails the run with kind \"expectation_missing\" instead of being a warning. Set false for the old advisory behaviour."},
                    "max_attempts": {"type": "integer", "description": "attempts the loop may run (default 1, capped at 10). A retry only runs after a repair step changes the workspace, and this tool edits nothing, so it stops at the first failing attempt unless a caller wires a repair hook."},
                    "expected_changes": {"type": "array", "items": {"type": "string"}, "description": "workspace-relative paths the change must have modified; under strict, a path git does not report fails the run"},
                    "expect": {"type": "array", "items": {"type": "string"}, "description": "literal case-sensitive substrings the changed files' new content must contain (e.g. \"fn handle_poll\", \"POLL_MARKER\"); under strict, an absent pattern fails the run"}
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
        let parsed = VerifyArgs::parse(args);
        let outcome = crate::verification::verify(
            &ctx.workspace_root,
            &crate::verification::VerificationPlan::cargo(),
            &parsed.budget(),
            &ctx.sandbox,
            &parsed.expectations(),
            |_, _| None,
        )?;
        Ok(render_outcome(&outcome, parsed.strict))
    }
}

/// One `verify` call's arguments, parsed once so the defaults are testable without running a
/// build.
struct VerifyArgs {
    max_attempts: u32,
    strict: bool,
    expected_changes: Vec<String>,
    expect: Vec<String>,
}

/// One attempt unless the caller asks for more: this tool edits nothing between attempts, so a
/// second one would repeat the same command against the same tree and reach the same verdict.
/// The loop itself honours any budget it is given (`Budget::attempts`), and a caller that wires
/// a repair hook gets the retries.
const DEFAULT_MAX_ATTEMPTS: u32 = 1;
/// A ceiling, not a policy: an unbounded value from the model would spend the whole turn on one
/// failing command.
const MAX_ATTEMPTS_CEILING: u32 = 10;

impl VerifyArgs {
    fn parse(args: &Value) -> Self {
        let max_attempts = args
            .get("max_attempts")
            .and_then(Value::as_u64)
            .unwrap_or(u64::from(DEFAULT_MAX_ATTEMPTS))
            .clamp(1, u64::from(MAX_ATTEMPTS_CEILING)) as u32;
        Self {
            max_attempts,
            // Strict for the model-facing tool: the whole point of verifying is to prove the
            // change, and the lenient path passes a run that proved nothing.
            strict: args.get("strict").and_then(Value::as_bool).unwrap_or(true),
            expected_changes: string_list(args, "expected_changes"),
            expect: string_list(args, "expect"),
        }
    }

    fn budget(&self) -> crate::verification::Budget {
        crate::verification::Budget::attempts(self.max_attempts)
    }

    fn expectations(&self) -> crate::verification::ExpectationPolicy {
        crate::verification::ExpectationPolicy {
            expected_changes: self.expected_changes.clone(),
            expect: self.expect.clone(),
            strict: self.strict,
        }
    }
}

fn string_list(args: &Value, key: &str) -> Vec<String> {
    args.get(key)
        .and_then(Value::as_array)
        .map(|items| {
            items
                .iter()
                .filter_map(Value::as_str)
                .map(str::to_owned)
                .collect()
        })
        .unwrap_or_default()
}

/// The model-visible report. The verdict leads on its own line: the growth pipeline reads it even
/// when the JSON below is truncated or spooled, and under strict mode the line carries the reason
/// the change could not be proven.
fn render_outcome(outcome: &crate::verification::VerificationOutcome, strict: bool) -> ToolOutput {
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
        "strict": strict,
        "expectation_failure": outcome.expectation_failure,
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
    let verdict = if outcome.ok {
        "verify: PASS".to_owned()
    } else {
        match &outcome.failure {
            Some((step, kind)) => format!("verify: FAIL step={step} kind={}", kind.as_str()),
            None => "verify: FAIL".to_owned(),
        }
    };
    let verdict = match &outcome.expectation_failure {
        Some(reason) => format!("{verdict} — {reason}"),
        None => verdict,
    };
    ToolOutput {
        content: truncated(format!("{verdict}\n{payload}")),
        ok: outcome.ok,
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

                let (output, timed_out) = match crate::verification::wait_with_timeout(
                    &mut cmd,
                    Duration::from_millis(timeout_ms),
                    ctx.cancel.as_ref(),
                ) {
                    Ok(crate::verification::WaitOutcome::Finished(output)) => (output, false),
                    Ok(crate::verification::WaitOutcome::TimedOut(partial)) => (partial, true),
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
                if timed_out {
                    if !body.is_empty() && !body.ends_with('\n') {
                        body.push('\n');
                    }
                    body.push_str(&format!(
                        "[command timed out after {timeout_ms} ms — output above is partial]"
                    ));
                }
                if body.trim().is_empty() {
                    body = format!("(no output, exit {:?})", output.status.code());
                }
                body.push_str(&format!("\n[exit {}]", output.status.code().unwrap_or(-1)));
                append_sandbox_note(&mut body, &sandbox_note);
                Ok(ToolOutput {
                    content: truncated(body),
                    ok: output.status.success() && !timed_out,
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
    fn verify_is_strict_unless_told_otherwise() {
        assert!(VerifyArgs::parse(&json!({})).strict);
        assert!(VerifyArgs::parse(&json!({"expect": ["x"]})).strict);
        assert!(!VerifyArgs::parse(&json!({"strict": false})).strict);
    }

    #[test]
    fn max_attempts_is_honoured_above_the_old_clamp() {
        let parsed = VerifyArgs::parse(&json!({"max_attempts": 7}));
        assert_eq!(
            parsed.max_attempts, 7,
            "the configured value must be what runs"
        );
        assert_eq!(parsed.budget().max_attempts, 7);
        // The default is documented, and a wild value is bounded rather than obeyed.
        assert_eq!(
            VerifyArgs::parse(&json!({})).budget().max_attempts,
            DEFAULT_MAX_ATTEMPTS
        );
        assert_eq!(
            VerifyArgs::parse(&json!({"max_attempts": 9_999})).max_attempts,
            MAX_ATTEMPTS_CEILING
        );
        assert_eq!(
            VerifyArgs::parse(&json!({"max_attempts": 0})).max_attempts,
            1
        );
    }

    #[test]
    fn the_schema_advertises_strict_and_its_default() {
        let spec = VerifyRun.spec();
        let schema = &spec.schema;
        assert_eq!(schema["properties"]["strict"]["type"], "boolean");
        assert!(
            schema["properties"]["strict"]["description"]
                .as_str()
                .is_some_and(|d| d.contains("default true")),
            "{}",
            spec.schema
        );
        assert!(
            schema["properties"]["max_attempts"]["description"]
                .as_str()
                .is_some_and(|d| d.contains("default 1")),
            "{}",
            spec.schema
        );
        assert!(spec.description.contains("Strict by default"));
    }

    #[test]
    fn an_expectation_missing_verdict_reaches_the_model() {
        let outcome = crate::verification::VerificationOutcome {
            ok: false,
            attempts: 1,
            failure: Some((
                "expectations".to_owned(),
                crate::verification::FailureKind::ExpectationMissing,
            )),
            steps: Vec::new(),
            changes: crate::verification::ChangeReport::default(),
            expectation_failure: Some(
                "strict verification requires an expectation and none was declared".to_owned(),
            ),
        };
        let out = render_outcome(&outcome, true);
        assert!(!out.ok);
        assert!(
            out.content
                .starts_with("verify: FAIL step=expectations kind=expectation_missing"),
            "{}",
            out.content
        );
        assert!(
            out.content.contains("requires an expectation"),
            "the reason must be visible even when the JSON is truncated: {}",
            out.content
        );
        assert!(out.content.contains("\"strict\":true"), "{}", out.content);
    }

    #[test]
    fn a_passing_verdict_stays_a_plain_pass() {
        let outcome = crate::verification::VerificationOutcome {
            ok: true,
            attempts: 1,
            failure: None,
            steps: Vec::new(),
            changes: crate::verification::ChangeReport::default(),
            expectation_failure: None,
        };
        let out = render_outcome(&outcome, true);
        assert!(out.ok);
        assert!(out.content.starts_with("verify: PASS"), "{}", out.content);
        assert!(!out.content.contains("expectation_missing"));
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
