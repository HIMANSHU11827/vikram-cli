use std::cell::RefCell;
use std::path::PathBuf;
use std::rc::Rc;

use serde_json::{Value, json};
use vikram_contracts::{ActionKind, PermissionRequest, SideEffect, Trust, VikramError};

use crate::tools::*;
use crate::verification::{FailureKind, VerificationPlan};
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
                    "max_attempts": {"type": "integer", "description": "attempts the loop may run (default 1, capped at 10). A failed attempt returns a repair instruction (step, command, classified kind, bounded output, files to open) and another attempt runs only when that failure names at least one file. This tool does not edit, so a deterministic failure usually fails again; retries help flaky steps. Timeouts are never retried."},
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
        let plan = VerificationPlan::cargo();
        let repairs = Rc::new(RefCell::new(Vec::new()));
        let outcome = crate::verification::verify(
            &ctx.workspace_root,
            &plan,
            &parsed.budget(),
            &ctx.sandbox,
            &parsed.expectations(),
            repair_hook(&plan, Rc::clone(&repairs)),
        )?;
        Ok(render_outcome(&outcome, parsed.strict, &repairs.borrow()))
    }
}

/// A failed attempt turned into what the next one needs to know: which step and command failed,
/// how the output was classified, the output itself (bounded), and the files the output points
/// at. The hook's return is the loop's continue/stop signal, so it is the parsed path list; the
/// instruction is what the model reads, since this tool edits nothing.
#[derive(Clone, Debug)]
struct RepairInstruction {
    step: String,
    kind: FailureKind,
    command: String,
    files: Vec<PathBuf>,
    output: String,
}

impl RepairInstruction {
    fn new(step: &str, command: &str, output: &str, kind: FailureKind) -> Self {
        Self {
            step: step.to_owned(),
            kind,
            command: command.to_owned(),
            files: files_named_in(output),
            output: bounded_excerpt(output, MAX_REPAIR_OUTPUT),
        }
    }

    fn render(&self) -> String {
        let files = if self.files.is_empty() {
            "none named".to_owned()
        } else {
            self.files
                .iter()
                .map(|path| path.display().to_string())
                .collect::<Vec<_>>()
                .join(", ")
        };
        format!(
            "repair `{}` failed as {}: run `{}`; open {}\n{}",
            self.step,
            self.kind.as_str(),
            self.command,
            files,
            self.output
        )
    }
}

fn repair_hook<'a>(
    plan: &'a VerificationPlan,
    log: Rc<RefCell<Vec<RepairInstruction>>>,
) -> impl FnMut(&str, &str) -> Option<Vec<PathBuf>> + 'a {
    move |step, output| {
        let command = plan
            .steps
            .iter()
            .find(|s| s.name == step)
            .map(|s| s.command.clone())
            .unwrap_or_default();
        let instruction =
            RepairInstruction::new(step, &command, output, FailureKind::classify(output, false));
        let files = instruction.files.clone();
        log.borrow_mut().push(instruction);
        (!files.is_empty()).then_some(files)
    }
}

/// A per-attempt excerpt ceiling. The full failing step is already capped at
/// `MAX_TOOL_OUTPUT` in the report; ten attempts at that size would bury it, so each
/// instruction keeps enough of both ends to find the error and its summary.
const MAX_REPAIR_OUTPUT: usize = 1_000;
const MAX_REPAIR_FILES: usize = 20;

const SOURCE_EXTENSIONS: &[&str] = &[
    "rs", "toml", "py", "ts", "tsx", "js", "jsx", "go", "java", "kt", "c", "h", "cc", "cpp", "hpp",
    "cs", "rb", "swift", "sh", "md", "json", "yaml", "yml",
];

/// The paths a compiler or test runner points at. Only the shapes with a stable meaning are
/// read: rustc's `--> file:line:col`, a panic's `panicked at file:line:col:`, and a line that
/// opens with `file:line`. A bare path in prose is not treated as a file.
fn files_named_in(output: &str) -> Vec<PathBuf> {
    let mut files: Vec<PathBuf> = Vec::new();
    for line in output.lines() {
        let candidate = line
            .split_once("-->")
            .map(|(_, rest)| rest)
            .or_else(|| line.split_once("panicked at ").map(|(_, rest)| rest))
            .unwrap_or(line);
        let Some(token) = first_token(candidate) else {
            continue;
        };
        let Some(path) = source_path(token) else {
            continue;
        };
        if files.len() >= MAX_REPAIR_FILES {
            break;
        }
        if !files.iter().any(|seen| seen == &path) {
            files.push(path);
        }
    }
    files
}

fn first_token(text: &str) -> Option<&str> {
    let trimmed = text.trim_start();
    let end = trimmed
        .find(|c: char| c.is_whitespace())
        .unwrap_or(trimmed.len());
    let token =
        trimmed[..end].trim_matches(|c| matches!(c, '`' | '"' | '\'' | '(' | ')' | ',' | ':'));
    (!token.is_empty()).then_some(token)
}

/// `src/main.rs:4:5` → `src/main.rs`; anything without a known source extension is not a file
/// the model should open.
fn source_path(token: &str) -> Option<PathBuf> {
    let mut candidate = token;
    for _ in 0..2 {
        let Some((head, tail)) = candidate.rsplit_once(':') else {
            break;
        };
        if tail.chars().all(|c| c.is_ascii_digit()) {
            candidate = head;
        } else {
            break;
        }
    }
    let extension = candidate.rsplit_once('.')?.1;
    SOURCE_EXTENSIONS
        .contains(&extension)
        .then(|| PathBuf::from(candidate))
}

fn bounded_excerpt(text: &str, max: usize) -> String {
    let trimmed = text.trim();
    let chars: Vec<char> = trimmed.chars().collect();
    if chars.len() <= max {
        return trimmed.to_owned();
    }
    let head = max / 2;
    let tail = max - head;
    format!(
        "{}\n… [{} chars elided] …\n{}",
        chars[..head].iter().collect::<String>(),
        chars.len() - max,
        chars[chars.len() - tail..].iter().collect::<String>()
    )
}

/// One `verify` call's arguments, parsed once so the defaults are testable without running a
/// build.
struct VerifyArgs {
    max_attempts: u32,
    strict: bool,
    expected_changes: Vec<String>,
    expect: Vec<String>,
}

/// One attempt unless the caller asks for more. The retry path is real — a failure that names a
/// file buys another attempt and every attempt publishes its own repair instruction — but a
/// deterministic failure against an unedited tree only repeats itself, so retries stay opt-in.
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
fn render_outcome(
    outcome: &crate::verification::VerificationOutcome,
    strict: bool,
    repairs: &[RepairInstruction],
) -> ToolOutput {
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
        "repairs": repairs
            .iter()
            .map(|repair| {
                json!({
                    "step": repair.step,
                    "kind": repair.kind.as_str(),
                    "command": repair.command,
                    "files": repair.files.iter().map(|p| p.display().to_string()).collect::<Vec<_>>(),
                    "instruction": repair.render(),
                })
            })
            .collect::<Vec<_>>(),
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
        let out = render_outcome(&outcome, true, &[]);
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
        let out = render_outcome(&outcome, true, &[]);
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

    fn scratch_context(dir: &std::path::Path) -> ToolContext {
        ToolContext {
            workspace_root: dir.to_path_buf(),
            sandbox: crate::sandbox::Sandbox::disabled("test"),
            spill: crate::spill::Spill::disabled(),
            runtime: None,
            session: None,
            agent: None,
            bot: None,
            cancel: None,
        }
    }

    #[test]
    fn a_failed_verify_attempts_a_repair_and_reports_each_attempt() {
        let dir = tempfile::tempdir().unwrap();
        let ctx = scratch_context(dir.path());
        // A scripted build stands in for cargo: it fails once, naming a file, then passes. The
        // real cargo plan costs a full workspace build to exercise the same loop.
        let plan = VerificationPlan {
            steps: vec![crate::verification::PlanStep::new(
                "build",
                "if [ -f flag.txt ]; then exit 0; fi; touch flag.txt; \
                 echo 'error[E0425]: cannot find value `x`'; echo '  --> src/main.rs:4:5'; exit 1",
                Duration::from_secs(10),
            )],
        };
        let repairs = Rc::new(RefCell::new(Vec::new()));
        let outcome = crate::verification::verify(
            &ctx.workspace_root,
            &plan,
            &crate::verification::Budget::attempts(3),
            &ctx.sandbox,
            &crate::verification::ExpectationPolicy::default(),
            repair_hook(&plan, Rc::clone(&repairs)),
        )
        .unwrap();
        assert!(outcome.ok, "{outcome:?}");
        assert_eq!(outcome.attempts, 2, "the failing attempt was retried");
        let repairs = repairs.borrow();
        assert_eq!(repairs.len(), 1, "the failed attempt is reported once");
        assert_eq!(repairs[0].files, vec![PathBuf::from("src/main.rs")]);
        assert_eq!(repairs[0].kind, FailureKind::CompileError);
        let out = render_outcome(&outcome, false, &repairs);
        assert!(out.ok, "{}", out.content);
        assert!(out.content.contains("\"repairs\""), "{}", out.content);
        assert!(out.content.contains("src/main.rs"), "{}", out.content);
        assert!(
            out.content.contains("cannot find value"),
            "the failing output reaches the model: {}",
            out.content
        );
    }

    #[test]
    fn a_repair_instruction_names_the_file_the_error_points_at() {
        let output = "error[E0425]: cannot find value `x` in this scope\n  \
                      --> crates/vikram-runtime/src/tools/verify.rs:12:9\n  \
                      |\n12 |     let x = y;\n   |         ^ not found\n";
        let instruction = RepairInstruction::new(
            "build",
            "cargo build --workspace",
            output,
            FailureKind::classify(output, false),
        );
        assert_eq!(
            instruction.files,
            vec![PathBuf::from("crates/vikram-runtime/src/tools/verify.rs")]
        );
        let text = instruction.render();
        assert!(
            text.contains("crates/vikram-runtime/src/tools/verify.rs"),
            "the instruction must name the path to open: {text}"
        );
        assert!(text.contains("compile_error"), "{text}");
        assert!(text.contains("cargo build --workspace"), "{text}");
    }

    #[test]
    fn a_timed_out_attempt_does_not_retry() {
        let dir = tempfile::tempdir().unwrap();
        let ctx = scratch_context(dir.path());
        let plan = VerificationPlan {
            steps: vec![crate::verification::PlanStep::new(
                "hung",
                "sleep 5",
                Duration::from_millis(200),
            )],
        };
        let repairs = Rc::new(RefCell::new(Vec::new()));
        let outcome = crate::verification::verify(
            &ctx.workspace_root,
            &plan,
            &crate::verification::Budget::attempts(3),
            &ctx.sandbox,
            &crate::verification::ExpectationPolicy::default(),
            repair_hook(&plan, Rc::clone(&repairs)),
        )
        .unwrap();
        assert!(!outcome.ok, "{outcome:?}");
        assert_eq!(outcome.attempts, 1, "a timeout must not burn the budget");
        assert_eq!(
            outcome.failure.map(|(_, kind)| kind),
            Some(FailureKind::Timeout)
        );
        assert!(
            repairs.borrow().is_empty(),
            "a timeout is never handed to the repair hook"
        );
    }

    #[test]
    fn a_huge_failing_output_is_bounded_in_the_instruction() {
        let output = format!(
            "error[E0425]: cannot find value\n{}  --> src/main.rs:1:1\n",
            "x".repeat(200_000)
        );
        let instruction = RepairInstruction::new(
            "build",
            "cargo build --workspace",
            &output,
            FailureKind::CompileError,
        );
        assert!(
            instruction.output.chars().count() <= MAX_REPAIR_OUTPUT + 64,
            "stored output is bounded: {} chars",
            instruction.output.chars().count()
        );
        assert!(instruction.output.ends_with("--> src/main.rs:1:1"));
        assert_eq!(instruction.files, vec![PathBuf::from("src/main.rs")]);
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
