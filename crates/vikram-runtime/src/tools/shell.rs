use serde_json::{Value, json};
use std::time::{Duration, Instant};
use vikram_contracts::{ActionKind, PermissionRequest, SideEffect, Trust, VikramError};

use crate::tools::*;
/// Shared implementation of the RunShell execution pattern: spawn a shell command in
/// `ctx.workspace_root`, capture stdout+stderr, enforce a timeout, truncate output.
///
/// Enforcement: the command is wrapped by `ctx.sandbox` (bubblewrap on Linux,
/// Seatbelt on macOS, Job-Objects stub / pass-through elsewhere with a degradation
/// notice). Policy still gates the call before this runs; the sandbox shrinks what
/// the running process can do. When the sandbox is not enforced, a one-line notice
/// is appended so the model never mistakes host execution for isolation.
fn run_shell_command(command: &str, ctx: &ToolContext, timeout_ms: u64) -> ToolOutput {
    #[cfg(windows)]
    let mut cmd = {
        let comspec = std::env::var("COMSPEC")
            .ok()
            .map(|s| s.trim().to_owned())
            .filter(|s| !s.is_empty())
            .unwrap_or_else(|| "cmd".to_owned());
        let mut c = std::process::Command::new(&comspec);
        c.args(["/C", command]);
        c
    };
    #[cfg(not(windows))]
    let mut cmd = {
        let shell = std::env::var("SHELL")
            .ok()
            .map(|s| s.trim().to_owned())
            .filter(|s| !s.is_empty())
            .unwrap_or_else(|| "sh".to_owned());
        let mut c = std::process::Command::new(&shell);
        c.args(["-c", command]);
        c
    };
    ctx.sandbox.wrap_command(&mut cmd, &ctx.workspace_root);
    let sandbox_note = ctx.sandbox.enforcement_notice();

    let mut child = match cmd
        .current_dir(&ctx.workspace_root)
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
    {
        Ok(child) => child,
        Err(e) => {
            return ToolOutput {
                content: format!("failed to start command: {e}"),
                ok: false,
            };
        }
    };

    let deadline = Instant::now() + Duration::from_millis(timeout_ms);
    loop {
        match child.try_wait() {
            Ok(Some(_)) => break,
            Ok(None) => {
                if Instant::now() >= deadline {
                    let _ = child.kill();
                    let _ = child.wait();
                    return ToolOutput {
                        content: format!("command timed out after {timeout_ms} ms"),
                        ok: false,
                    };
                }
                std::thread::sleep(Duration::from_millis(25));
            }
            Err(e) => {
                return ToolOutput {
                    content: format!("failed while waiting: {e}"),
                    ok: false,
                };
            }
        }
    }

    let output = match child.wait_with_output() {
        Ok(o) => o,
        Err(e) => {
            return ToolOutput {
                content: format!("failed while waiting: {e}"),
                ok: false,
            };
        }
    };
    let mut body = String::new();
    body.push_str(&String::from_utf8_lossy(&output.stdout));
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
    // The exit code is the one fact a command's row must not guess at, and the platform only hands
    // it over here, so it travels with the output instead of being inferred later.
    body.push_str(&format!("\n[exit {}]", output.status.code().unwrap_or(-1)));
    append_sandbox_note(&mut body, &sandbox_note);
    ToolOutput {
        content: truncated(body),
        ok: output.status.success(),
    }
}

/// Append the degradation notice when the OS sandbox is not enforcing. Kept tiny so
/// it never drowns the command output it qualifies.
fn append_sandbox_note(body: &mut String, note: &Option<String>) {
    if let Some(note) = note {
        body.push_str(&format!("\n[sandbox] {note}"));
    }
}

pub(crate) struct RunShell;

impl Tool for RunShell {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "shell.run".into(),
            description: "Run a shell command in the workspace.".into(),
            schema: json!({
                "type": "object",
                "properties": {
                    "command": {"type": "string"},
                    "timeout_ms": {"type": "integer"}
                },
                "required": ["command"]
            }),
        }
    }

    fn permission(&self, args: &Value) -> Result<PermissionRequest, VikramError> {
        let command = arg_str(args, "command")?;
        let head = command.split_whitespace().next().unwrap_or("shell");
        Ok(PermissionRequest::new(
            ActionKind::Execute,
            "shell.run",
            head,
            SideEffect::LocalMutation,
            Trust::Model,
        ))
    }

    fn run(&self, args: &Value, ctx: &ToolContext) -> Result<ToolOutput, VikramError> {
        let command = arg_str(args, "command")?;
        let timeout_ms = args
            .get("timeout_ms")
            .and_then(Value::as_u64)
            .unwrap_or(30_000)
            .clamp(1_000, 120_000);

        #[cfg(windows)]
        let mut cmd = {
            let comspec = std::env::var("COMSPEC")
                .ok()
                .map(|s| s.trim().to_owned())
                .filter(|s| !s.is_empty())
                .unwrap_or_else(|| "cmd".to_owned());
            let mut c = std::process::Command::new(&comspec);
            c.args(["/C", &command]);
            c
        };
        #[cfg(not(windows))]
        let mut cmd = {
            let shell = std::env::var("SHELL")
                .ok()
                .map(|s| s.trim().to_owned())
                .filter(|s| !s.is_empty())
                .unwrap_or_else(|| "sh".to_owned());
            let mut c = std::process::Command::new(&shell);
            c.args(["-c", &command]);
            c
        };
        ctx.sandbox.wrap_command(&mut cmd, &ctx.workspace_root);
        let sandbox_note = ctx.sandbox.enforcement_notice();

        let mut child = match cmd
            .current_dir(&ctx.workspace_root)
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .spawn()
        {
            Ok(child) => child,
            Err(e) => {
                return Ok(ToolOutput {
                    content: format!("failed to start command: {e}"),
                    ok: false,
                });
            }
        };

        let deadline = Instant::now() + Duration::from_millis(timeout_ms);
        loop {
            match child.try_wait() {
                Ok(Some(_)) => break,
                Ok(None) => {
                    if Instant::now() >= deadline {
                        let _ = child.kill();
                        let _ = child.wait();
                        return Ok(ToolOutput {
                            content: format!("command timed out after {timeout_ms} ms"),
                            ok: false,
                        });
                    }
                    std::thread::sleep(Duration::from_millis(25));
                }
                Err(e) => {
                    return Ok(ToolOutput {
                        content: format!("failed while waiting: {e}"),
                        ok: false,
                    });
                }
            }
        }

        let output = child
            .wait_with_output()
            .map_err(|e| VikramError::EffectFailed(e.to_string()))?;
        let mut body = String::new();
        body.push_str(&String::from_utf8_lossy(&output.stdout));
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

/// Persistent interactive terminal backed by a real PTY.
pub(crate) struct TerminalTool;

impl Tool for TerminalTool {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "terminal".into(),
            description:
                "Drive a persistent interactive PTY session (open/write/read/resize/close/list)."
                    .into(),
            schema: json!({
                "type": "object",
                "properties": {
                    "action": {"type": "string", "enum": ["open", "write", "read", "resize", "close", "list"]},
                    "session": {"type": "string"},
                    "input": {"type": "string"},
                    "shell": {"type": "string"},
                    "cwd": {"type": "string"},
                    "rows": {"type": "integer"},
                    "cols": {"type": "integer"},
                    "drain": {"type": "boolean"}
                },
                "required": ["action"]
            }),
        }
    }

    fn permission(&self, args: &Value) -> Result<PermissionRequest, VikramError> {
        let action = args.get("action").and_then(Value::as_str).unwrap_or("open");
        Ok(PermissionRequest::new(
            ActionKind::Execute,
            "terminal",
            format!("terminal.{action}"),
            SideEffect::LocalMutation,
            Trust::Model,
        ))
    }

    fn run(&self, args: &Value, ctx: &ToolContext) -> Result<ToolOutput, VikramError> {
        let action = arg_str(args, "action")?;
        let result = match action.as_str() {
            "open" => {
                let shell = args.get("shell").and_then(Value::as_str);
                let cwd = args
                    .get("cwd")
                    .and_then(Value::as_str)
                    .map(|c| c.to_owned())
                    .unwrap_or_else(|| ctx.workspace_root.to_string_lossy().into_owned());
                let rows = args.get("rows").and_then(Value::as_u64).unwrap_or(24) as u16;
                let cols = args.get("cols").and_then(Value::as_u64).unwrap_or(80) as u16;
                let name = args.get("session").and_then(Value::as_str);
                // Reopening a live name is the caller asking for the session it already has. It is
                // answered as a success with the session to use, because an error here reads to a
                // model as "that name is taken" and sends it off inventing new ones.
                if let Some(name) = name.map(str::trim).filter(|n| !n.is_empty())
                    && crate::pty::is_open(name)
                {
                    Ok(format!(
                        "terminal session `{name}` is already open — reuse it with action=write/read (no second shell started)"
                    ))
                } else {
                    match crate::pty::open_sandboxed(
                        &ctx.sandbox,
                        name,
                        shell,
                        Some(&cwd),
                        rows,
                        cols,
                    ) {
                        Ok(id) => {
                            let mut msg = format!("opened terminal session `{id}` in {cwd}");
                            if let Some(note) = ctx.sandbox.enforcement_notice() {
                                msg.push_str(&format!(" [sandbox] {note}"));
                            }
                            Ok(msg)
                        }
                        Err(e) => Err(e),
                    }
                }
            }
            "write" => {
                let session = arg_str(args, "session")?;
                let input = arg_str(args, "input")?;
                crate::pty::write(&session, &input).map(|_| format!("wrote {} bytes", input.len()))
            }
            "read" => {
                let session = arg_str(args, "session")?;
                let drain = args.get("drain").and_then(Value::as_bool).unwrap_or(false);
                crate::pty::read(&session, drain)
            }
            "resize" => {
                let session = arg_str(args, "session")?;
                let rows = args.get("rows").and_then(Value::as_u64).unwrap_or(24) as u16;
                let cols = args.get("cols").and_then(Value::as_u64).unwrap_or(80) as u16;
                crate::pty::resize(&session, rows, cols)
                    .map(|_| format!("resized to {cols}x{rows}"))
            }
            "close" => {
                let session = arg_str(args, "session")?;
                crate::pty::close(&session).map(|_| format!("closed session `{session}`"))
            }
            "list" => {
                let sessions = crate::pty::list();
                if sessions.is_empty() {
                    Ok("no live terminal sessions".to_owned())
                } else {
                    let mut out = String::new();
                    for (id, shell, cwd) in sessions {
                        out.push_str(&format!("{id}\t{shell}\t{cwd}\n"));
                    }
                    Ok(out)
                }
            }
            other => Err(format!("unknown terminal action `{other}`")),
        };

        Ok(match result {
            Ok(content) => ToolOutput {
                content: truncated(content),
                ok: true,
            },
            Err(err) => ToolOutput {
                content: err,
                ok: false,
            },
        })
    }
}

pub(crate) struct Git;

impl Tool for Git {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "git".into(),
            description: "Run a read-only git subcommand in the workspace.".into(),
            schema: json!({
                "type": "object",
                "properties": {
                    "subcommand": {"type": "string", "enum": ["status", "diff", "log", "branch", "show"]},
                    "staged": {"type": "boolean"},
                    "commit": {"type": "string"}
                },
                "required": ["subcommand"]
            }),
        }
    }

    fn permission(&self, args: &Value) -> Result<PermissionRequest, VikramError> {
        let sub = arg_str(args, "subcommand")?;
        let (action, side_effect) = match sub.as_str() {
            "status" | "diff" | "log" | "branch" | "show" => (ActionKind::Read, SideEffect::None),
            "commit" => (ActionKind::Write, SideEffect::LocalMutation),
            _ => (ActionKind::Read, SideEffect::None),
        };
        Ok(PermissionRequest::new(
            action,
            "git",
            sub,
            side_effect,
            Trust::Model,
        ))
    }

    fn run(&self, args: &Value, ctx: &ToolContext) -> Result<ToolOutput, VikramError> {
        let sub = arg_str(args, "subcommand")?;
        let command = match sub.as_str() {
            "status" => "git status --short".to_string(),
            "diff" => {
                let staged = args.get("staged").and_then(Value::as_bool).unwrap_or(false);
                if staged {
                    "git diff --cached".to_string()
                } else {
                    "git diff".to_string()
                }
            }
            "log" => "git log --oneline -20".to_string(),
            "branch" => "git branch -a".to_string(),
            "show" => {
                let commit = args.get("commit").and_then(Value::as_str).unwrap_or("HEAD");
                format!("git show {}", commit)
            }
            other => {
                return Ok(ToolOutput {
                    content: format!("unsupported git subcommand: {other}"),
                    ok: false,
                });
            }
        };
        Ok(run_shell_command(&command, ctx, 30_000))
    }
}

pub(crate) struct BuildRun;

impl Tool for BuildRun {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "build.run".into(),
            description: "Detect the build system and run a build.".into(),
            schema: json!({
                "type": "object",
                "properties": {}
            }),
        }
    }

    fn permission(&self, _args: &Value) -> Result<PermissionRequest, VikramError> {
        Ok(PermissionRequest::new(
            ActionKind::Execute,
            "build.run",
            "build",
            SideEffect::LocalMutation,
            Trust::Model,
        ))
    }

    fn run(&self, _args: &Value, ctx: &ToolContext) -> Result<ToolOutput, VikramError> {
        let root = &ctx.workspace_root;
        let command = if root.join("Cargo.toml").is_file() {
            "cargo build".to_string()
        } else if root.join("package.json").is_file() {
            if root.join("pnpm-lock.yaml").is_file() {
                "pnpm build".to_string()
            } else {
                "npm run build".to_string()
            }
        } else if root.join("Makefile").is_file() {
            "make".to_string()
        } else if root.join("pyproject.toml").is_file() {
            "python -m build".to_string()
        } else {
            return Ok(ToolOutput {
                content: "no build system detected".into(),
                ok: false,
            });
        };
        Ok(run_shell_command(&command, ctx, 120_000))
    }
}

pub(crate) struct TestRun;

impl Tool for TestRun {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "test.run".into(),
            description: "Detect the test framework and run tests.".into(),
            schema: json!({
                "type": "object",
                "properties": {}
            }),
        }
    }

    fn permission(&self, _args: &Value) -> Result<PermissionRequest, VikramError> {
        Ok(PermissionRequest::new(
            ActionKind::Execute,
            "test.run",
            "test",
            SideEffect::LocalMutation,
            Trust::Model,
        ))
    }

    fn run(&self, _args: &Value, ctx: &ToolContext) -> Result<ToolOutput, VikramError> {
        let root = &ctx.workspace_root;
        let command = if root.join("Cargo.toml").is_file() {
            "cargo test".to_string()
        } else if root.join("package.json").is_file() {
            "npm test".to_string()
        } else if root.join("pytest.ini").is_file() || root.join("pyproject.toml").is_file() {
            "pytest".to_string()
        } else {
            return Ok(ToolOutput {
                content: "no test framework detected".into(),
                ok: false,
            });
        };
        Ok(run_shell_command(&command, ctx, 120_000))
    }
}
