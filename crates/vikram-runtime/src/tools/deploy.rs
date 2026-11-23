use serde_json::{Value, json};
use std::path::Path;
use std::time::Duration;
use vikram_contracts::{ActionKind, PermissionRequest, SideEffect, Trust, VikramError};

use crate::tools::*;

/// A deploy step may compile a release binary or restart a service, so the deadline is generous in
/// the way a cleanup deadline is not.
const STEP_TIMEOUT: Duration = Duration::from_secs(600);
/// A declaration is a handful of command lines, so anything larger is something else.
const MAX_DECLARATION_BYTES: u64 = 256 * 1024;
/// The refusal every deploy without an undo path gets. It is the tool's whole point, so it is
/// spelled once and pinned by exact wording in the tests.
const NO_ROLLBACK: &str =
    "a deploy without a rollback command is refused: a failed deploy you cannot undo is an outage";

struct Declaration {
    steps: Vec<String>,
    rollback: Vec<String>,
    cwd: String,
}

pub(crate) struct DeployRun;

impl Tool for DeployRun {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "deploy.run".into(),
            description: "Run the steps of a declared deploy and, if a step fails, the rollback \
                          steps declared beside them. The declaration is a workspace JSON file \
                          with `steps`, `rollback` and an optional `cwd`; a declaration without a \
                          rollback is refused."
                .into(),
            schema: json!({
                "type": "object",
                "properties": {
                    "deploy": {
                        "type": "string",
                        "description": "Workspace-relative path to the deploy declaration JSON."
                    }
                },
                "required": ["deploy"]
            }),
        }
    }

    fn permission(&self, args: &Value) -> Result<PermissionRequest, VikramError> {
        let path = args
            .get("deploy")
            .and_then(Value::as_str)
            .unwrap_or("deploy")
            .to_owned();
        Ok(PermissionRequest::new(
            ActionKind::Execute,
            "deploy.run",
            path,
            SideEffect::ExternalChange,
            Trust::Model,
        ))
    }

    fn run(&self, args: &Value, ctx: &ToolContext) -> Result<ToolOutput, VikramError> {
        let (path, display) = canonical_resource(&ctx.workspace_root, &arg_str(args, "deploy")?)?;
        let length = match std::fs::metadata(&path) {
            Ok(meta) if meta.is_file() => meta.len(),
            Ok(_) => return Ok(refused(format!("`{display}` is not a file"))),
            Err(error) => return Ok(refused(format!("cannot read `{display}`: {error}"))),
        };
        if length > MAX_DECLARATION_BYTES {
            return Ok(refused(format!(
                "`{display}` is {length} bytes, above the {} KiB ceiling for a deploy declaration",
                MAX_DECLARATION_BYTES / 1024
            )));
        }
        let text = match std::fs::read_to_string(&path) {
            Ok(text) => text,
            Err(error) => return Ok(refused(format!("cannot read `{display}`: {error}"))),
        };
        let declaration = match declaration(&text) {
            Ok(declaration) => declaration,
            Err(message) => return Ok(refused(format!("`{display}`: {message}"))),
        };

        let cwd = match canonical_resource(&ctx.workspace_root, &declaration.cwd) {
            Ok((cwd, _)) => cwd,
            Err(_) => {
                return Ok(refused(format!(
                    "`{display}` names cwd `{}`, which is outside the workspace root `{}`; \
                     deploy.run will not run steps there",
                    declaration.cwd,
                    ctx.workspace_root.display()
                )));
            }
        };
        if !cwd.is_dir() {
            return Ok(refused(format!(
                "`{display}` names cwd `{}`, which is not a directory; nothing ran",
                declaration.cwd
            )));
        }
        if let Some(step) = declaration
            .steps
            .iter()
            .find(|step| looks_destructive(step))
        {
            return Ok(refused(format!(
                "a deploy step is destructive and deploy.run will not run it: `{step}` — run it \
                 as shell.run, which asks for approval of exactly that command"
            )));
        }
        if let Some(step) = declaration
            .rollback
            .iter()
            .find(|step| looks_destructive(step))
        {
            return Ok(refused(format!(
                "a rollback step is destructive and deploy.run will not run it: `{step}` — run it \
                 as shell.run, which asks for approval of exactly that command"
            )));
        }

        let total = declaration.steps.len();
        let cwd_display = cwd.display();
        let mut report = format!("deploy `{display}` in `{cwd_display}` ({total} steps)\n");
        let mut failure = None;
        for (index, step) in declaration.steps.iter().enumerate() {
            let number = index + 1;
            let outcome = run_step(step, &cwd, ctx);
            report.push_str(&format!(
                "\nstep {number}/{total} `{step}`\n{}\n",
                outcome.body
            ));
            if !outcome.ok {
                failure = Some((number, step.clone(), outcome.exit_code, outcome.timed_out));
                break;
            }
        }

        let Some((number, step, code, timed_out)) = failure else {
            report.push_str(&format!(
                "\ndeploy succeeded: all {total} steps ran; no rollback was needed"
            ));
            return Ok(ToolOutput {
                content: truncated(report),
                ok: true,
            });
        };

        if timed_out {
            report.push_str(&format!(
                "\ndeploy failed at step {number}/{total}: `{step}` timed out after {}s\n",
                STEP_TIMEOUT.as_secs()
            ));
        } else {
            report.push_str(&format!(
                "\ndeploy failed at step {number}/{total}: `{step}` exited {code}\n"
            ));
        }

        let rollback_total = declaration.rollback.len();
        let mut rollback_failure = None;
        let mut succeeded = 0usize;
        for (index, step) in declaration.rollback.iter().enumerate() {
            let outcome = run_step(step, &cwd, ctx);
            report.push_str(&format!(
                "\nrollback {}/{} `{step}`\n{}\n",
                index + 1,
                rollback_total,
                outcome.body
            ));
            if outcome.ok {
                succeeded += 1;
            } else if rollback_failure.is_none() {
                rollback_failure = Some((index + 1, step.clone(), outcome.exit_code));
            }
        }

        report.push_str(&format!(
            "\nrollback ran: {succeeded} of {rollback_total} steps succeeded"
        ));
        match rollback_failure {
            Some((number, step, code)) => report.push_str(&format!(
                "; rollback step {number} `{step}` exited {code} — the workspace may be \
                 half-rolled-back"
            )),
            None => report.push_str("; the failed deploy was undone by the declared rollback"),
        }

        Ok(ToolOutput {
            content: truncated(report),
            ok: false,
        })
    }
}

fn refused(content: String) -> ToolOutput {
    ToolOutput { content, ok: false }
}

/// Parse a declaration and enforce everything that can be checked before a command runs: an undo
/// path, at least one step, and a `cwd` that is a plain string. Destructive steps and the cwd's
/// place inside the workspace need the workspace root, so they are checked in `run`.
fn declaration(text: &str) -> Result<Declaration, String> {
    let value: Value = serde_json::from_str(text)
        .map_err(|error| format!("the declaration is not JSON: {error}"))?;
    if !value.is_object() {
        return Err(
            "the declaration must be a JSON object with `steps` and `rollback` arrays".to_owned(),
        );
    }
    let rollback = match value.get("rollback") {
        None | Some(Value::Null) => return Err(NO_ROLLBACK.to_owned()),
        Some(_) => commands(&value, "rollback")?,
    };
    if rollback.is_empty() {
        return Err(NO_ROLLBACK.to_owned());
    }
    let steps = commands(&value, "steps")?;
    if steps.is_empty() {
        return Err(
            "the declaration has no steps: a deploy that runs nothing is not a deploy".to_owned(),
        );
    }
    let cwd = match value.get("cwd") {
        None | Some(Value::Null) => ".".to_owned(),
        Some(Value::String(cwd)) => cwd.clone(),
        Some(_) => return Err("`cwd` must be a string path".to_owned()),
    };
    Ok(Declaration {
        steps,
        rollback,
        cwd,
    })
}

fn commands(value: &Value, key: &str) -> Result<Vec<String>, String> {
    let Some(entries) = value.get(key) else {
        return Err(format!("the declaration has no `{key}` array"));
    };
    let Some(entries) = entries.as_array() else {
        return Err(format!("`{key}` must be an array of shell commands"));
    };
    entries
        .iter()
        .map(|entry| match entry.as_str() {
            Some(command) if !command.trim().is_empty() => Ok(command.to_owned()),
            Some(_) => Err(format!("`{key}` holds an empty command")),
            None => Err(format!("`{key}` holds a non-string entry")),
        })
        .collect()
}

struct StepOutcome {
    body: String,
    ok: bool,
    exit_code: i32,
    timed_out: bool,
}

/// One step, run the way `shell.run` runs a command: through the host shell, wrapped by the OS
/// sandbox, on a deadline that kills the process group, with its output bounded before it joins
/// the report.
fn run_step(command: &str, cwd: &Path, ctx: &ToolContext) -> StepOutcome {
    let (program, args) = shell_invocation(command);
    let mut cmd = std::process::Command::new(&program);
    cmd.args(&args);
    cmd.current_dir(cwd);
    ctx.sandbox.wrap_command(&mut cmd, &ctx.workspace_root);
    match crate::verification::wait_with_timeout(&mut cmd, STEP_TIMEOUT, ctx.cancel.as_ref()) {
        Ok(crate::verification::WaitOutcome::Finished(output)) => StepOutcome {
            body: step_body(&output, false, ctx),
            ok: output.status.success(),
            exit_code: output.status.code().unwrap_or(-1),
            timed_out: false,
        },
        Ok(crate::verification::WaitOutcome::TimedOut(partial)) => StepOutcome {
            body: step_body(&partial, true, ctx),
            ok: false,
            exit_code: partial.status.code().unwrap_or(-1),
            timed_out: true,
        },
        Err(error) => StepOutcome {
            body: truncated(format!("the step could not run: {error}")),
            ok: false,
            exit_code: -1,
            timed_out: false,
        },
    }
}

fn step_body(output: &std::process::Output, timed_out: bool, ctx: &ToolContext) -> String {
    let mut body = String::from_utf8_lossy(&output.stdout).into_owned();
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
            "[step timed out after {}s — output above is partial]",
            STEP_TIMEOUT.as_secs()
        ));
    }
    if body.trim().is_empty() {
        body = format!("(no output, exit {:?})", output.status.code());
    }
    body.push_str(&format!("\n[exit {}]", output.status.code().unwrap_or(-1)));
    append_sandbox_note(&mut body, &ctx.sandbox.enforcement_notice());
    if !output.status.success() && !timed_out {
        append_denial(
            &mut body,
            ctx.sandbox.classify_denial(output.status.code(), &stderr),
        );
    }
    truncated(body)
}

/// The host shell, matching `shell.run`: `cmd /C` on Windows, `$SHELL -c` elsewhere.
fn shell_invocation(command: &str) -> (String, Vec<String>) {
    #[cfg(windows)]
    {
        let comspec = std::env::var("COMSPEC")
            .ok()
            .map(|s| s.trim().to_owned())
            .filter(|s| !s.is_empty())
            .unwrap_or_else(|| "cmd".to_owned());
        (comspec, vec!["/C".to_owned(), command.to_owned()])
    }
    #[cfg(not(windows))]
    {
        let shell = std::env::var("SHELL")
            .ok()
            .map(|s| s.trim().to_owned())
            .filter(|s| !s.is_empty())
            .unwrap_or_else(|| "sh".to_owned());
        (shell, vec!["-c".to_owned(), command.to_owned()])
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn workspace(root: &Path) -> ToolContext {
        ToolContext {
            workspace_root: root.to_path_buf(),
            ..ToolContext::default()
        }
    }

    fn declare(directory: &Path, declaration: Value) {
        std::fs::write(directory.join("deploy.json"), declaration.to_string()).unwrap();
    }

    fn run(directory: &Path) -> ToolOutput {
        DeployRun
            .run(&json!({"deploy": "deploy.json"}), &workspace(directory))
            .unwrap()
    }

    #[test]
    fn a_deploy_without_a_rollback_command_is_refused() {
        let directory = tempfile::tempdir().unwrap();
        for declaration in [
            json!({"steps": ["cargo build --release"]}),
            json!({"steps": ["cargo build --release"], "rollback": []}),
        ] {
            declare(directory.path(), declaration);
            let output = run(directory.path());
            assert!(!output.ok, "{}", output.content);
            assert!(
                output
                    .content
                    .contains("a deploy without a rollback command is refused"),
                "{}",
                output.content
            );
            assert!(
                output
                    .content
                    .contains("a failed deploy you cannot undo is an outage"),
                "{}",
                output.content
            );
        }
    }

    #[test]
    fn a_failed_deploy_runs_the_rollback() {
        let directory = tempfile::tempdir().unwrap();
        declare(
            directory.path(),
            json!({
                "steps": ["echo first", "exit 7", "echo never"],
                "rollback": ["echo undone > undone.txt"]
            }),
        );

        let output = run(directory.path());

        assert!(!output.ok, "{}", output.content);
        assert!(
            output
                .content
                .contains("deploy failed at step 2/3: `exit 7` exited 7"),
            "{}",
            output.content
        );
        assert!(
            output
                .content
                .contains("rollback ran: 1 of 1 steps succeeded"),
            "{}",
            output.content
        );
        assert!(
            !output.content.contains("step 3/3"),
            "the step after the failure must not run: {}",
            output.content
        );
        assert!(
            directory.path().join("undone.txt").exists(),
            "the rollback's own effect must be on disk: {}",
            output.content
        );
    }

    #[test]
    fn a_step_outside_the_workspace_cwd_is_refused() {
        let directory = tempfile::tempdir().unwrap();
        declare(
            directory.path(),
            json!({
                "steps": ["echo should-not-run > ran.txt"],
                "rollback": ["echo undone > undone.txt"],
                "cwd": "../.."
            }),
        );

        let output = run(directory.path());

        assert!(!output.ok, "{}", output.content);
        assert!(
            output.content.contains("outside the workspace root"),
            "{}",
            output.content
        );
        assert!(
            !directory.path().join("ran.txt").exists(),
            "a refused deploy runs nothing"
        );
        assert!(
            !directory.path().join("undone.txt").exists(),
            "a refused deploy rolls nothing back"
        );
    }

    #[test]
    fn a_destructive_step_is_refused() {
        let directory = tempfile::tempdir().unwrap();
        declare(
            directory.path(),
            json!({"steps": ["rm -rf build"], "rollback": ["echo undone"]}),
        );

        let output = run(directory.path());

        assert!(!output.ok, "{}", output.content);
        assert!(output.content.contains("destructive"), "{}", output.content);
        assert!(
            output.content.contains("rm -rf build"),
            "{}",
            output.content
        );
    }

    #[test]
    fn an_empty_step_list_is_refused() {
        let directory = tempfile::tempdir().unwrap();
        declare(
            directory.path(),
            json!({"steps": [], "rollback": ["echo undone"]}),
        );

        let output = run(directory.path());

        assert!(!output.ok, "{}", output.content);
        assert!(
            output.content.contains("has no steps"),
            "{}",
            output.content
        );
    }

    #[test]
    fn a_declaration_outside_the_workspace_is_a_policy_denial() {
        let directory = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        std::fs::write(
            outside.path().join("deploy.json"),
            json!({"steps": ["echo hi"], "rollback": ["echo undone"]}).to_string(),
        )
        .unwrap();
        let error = DeployRun
            .run(
                &json!({"deploy": outside.path().join("deploy.json").to_string_lossy()}),
                &workspace(directory.path()),
            )
            .unwrap_err();
        assert!(
            matches!(error, VikramError::PolicyDenied { .. }),
            "a declaration outside the root must be a policy denial: {error:?}"
        );
    }

    #[test]
    fn the_tool_is_registered_and_advertises_its_schema() {
        let registry = ToolRegistry::with_builtins();
        let tool = registry.get("deploy.run").expect("deploy.run registered");
        assert_eq!(tool.spec().name, "deploy.run");
        assert_eq!(tool.spec().schema["type"], "object");
        assert_eq!(tool.spec().schema["properties"]["deploy"]["type"], "string");
        assert_eq!(tool.spec().schema["required"][0], "deploy");
        assert!(registry.names().contains(&"deploy.run".to_owned()));
    }

    #[test]
    fn the_permission_is_an_execute_with_an_external_change() {
        let request = DeployRun
            .permission(&json!({"deploy": "ops/deploy.json"}))
            .unwrap();
        assert_eq!(request.action, ActionKind::Execute);
        assert_eq!(request.side_effect, SideEffect::ExternalChange);
        assert_eq!(request.verb, "deploy.run");
        assert_eq!(request.resource, "ops/deploy.json");
    }
}
