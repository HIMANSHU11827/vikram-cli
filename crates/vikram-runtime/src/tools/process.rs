use serde_json::{Value, json};
use std::path::PathBuf;
use vikram_contracts::{ActionKind, PermissionRequest, SideEffect, Trust, VikramError};

use crate::tools::*;
pub(crate) struct Process;

impl Tool for Process {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "process".into(),
            description: "List or kill system processes.".into(),
            schema: json!({
                "type": "object",
                "properties": {
                    "action": {"type": "string", "enum": ["list", "kill"]},
                    "pid": {"type": "integer"}
                },
                "required": ["action"]
            }),
        }
    }

    fn permission(&self, _args: &Value) -> Result<PermissionRequest, VikramError> {
        Ok(PermissionRequest::new(
            ActionKind::Execute,
            "process.run",
            "process",
            SideEffect::LocalMutation,
            Trust::Model,
        ))
    }

    fn run(&self, args: &Value, _ctx: &ToolContext) -> Result<ToolOutput, VikramError> {
        let action = args.get("action").and_then(Value::as_str).unwrap_or("list");
        match action {
            "list" => {
                #[cfg(windows)]
                let mut cmd = {
                    let mut c = std::process::Command::new("tasklist");
                    c
                };
                #[cfg(not(windows))]
                let mut cmd = {
                    let mut c = std::process::Command::new("ps");
                    c.arg("aux");
                    c
                };
                let output = cmd
                    .output()
                    .map_err(|e| VikramError::EffectFailed(e.to_string()))?;
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
                Ok(ToolOutput {
                    content: truncated(body),
                    ok: output.status.success(),
                })
            }
            "kill" => {
                let pid = args.get("pid").and_then(Value::as_i64).ok_or_else(|| {
                    VikramError::ProviderRejected("missing integer argument `pid`".into())
                })?;
                #[cfg(windows)]
                let mut cmd = {
                    let mut c = std::process::Command::new("taskkill");
                    c.args(["/PID", &pid.to_string()]);
                    c
                };
                #[cfg(not(windows))]
                let mut cmd = {
                    let mut c = std::process::Command::new("kill");
                    c.arg(pid.to_string());
                    c
                };
                let output = cmd
                    .output()
                    .map_err(|e| VikramError::EffectFailed(e.to_string()))?;
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
                Ok(ToolOutput {
                    content: truncated(body),
                    ok: output.status.success(),
                })
            }
            other => Ok(ToolOutput {
                content: format!("unknown action `{other}`; expected list or kill"),
                ok: false,
            }),
        }
    }
}

// Task storage helpers
fn tasks_path() -> PathBuf {
    crate::config::vikram_home().join("tasks.json")
}

fn load_tasks() -> Vec<Value> {
    let path = tasks_path();
    std::fs::read_to_string(&path)
        .ok()
        .and_then(|raw| serde_json::from_str::<Value>(&raw).ok())
        .and_then(|v| v.as_array().cloned())
        .unwrap_or_default()
}

fn save_tasks(tasks: &[Value]) -> Result<(), VikramError> {
    let path = tasks_path();
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|e| VikramError::Storage(e.to_string()))?;
    }
    let body =
        serde_json::to_string_pretty(tasks).map_err(|e| VikramError::Storage(e.to_string()))?;
    std::fs::write(&path, body).map_err(|e| VikramError::Storage(e.to_string()))?;
    Ok(())
}

pub(crate) struct Task;

impl Tool for Task {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "task".into(),
            description: "Create, list, or update tasks persisted in $VIKRAM_HOME/tasks.json."
                .into(),
            schema: json!({
                "type": "object",
                "properties": {
                    "action": {"type": "string", "enum": ["create", "list", "update"]},
                    "title": {"type": "string"},
                    "description": {"type": "string"},
                    "id": {"type": "integer"},
                    "status": {"type": "string", "enum": ["pending", "active", "completed", "failed"]}
                },
                "required": ["action"]
            }),
        }
    }

    fn permission(&self, _args: &Value) -> Result<PermissionRequest, VikramError> {
        Ok(PermissionRequest::new(
            ActionKind::Write,
            "task.manage",
            "tasks",
            SideEffect::LocalMutation,
            Trust::Model,
        ))
    }

    fn run(&self, args: &Value, _ctx: &ToolContext) -> Result<ToolOutput, VikramError> {
        let action = args.get("action").and_then(Value::as_str).unwrap_or("list");
        match action {
            "create" => {
                let title = arg_str(args, "title")?;
                let description = args
                    .get("description")
                    .and_then(Value::as_str)
                    .unwrap_or("")
                    .to_string();
                let mut tasks = load_tasks();
                let next_id = tasks
                    .iter()
                    .filter_map(|t| t.get("id").and_then(Value::as_u64))
                    .max()
                    .unwrap_or(0)
                    + 1;
                let task = json!({
                    "id": next_id,
                    "title": title,
                    "description": description,
                    "status": "pending"
                });
                tasks.push(task);
                save_tasks(&tasks)?;
                Ok(ToolOutput {
                    content: format!("created task {next_id}"),
                    ok: true,
                })
            }
            "list" => {
                let tasks = load_tasks();
                if tasks.is_empty() {
                    return Ok(ToolOutput {
                        content: "no tasks".into(),
                        ok: true,
                    });
                }
                let body = tasks
                    .iter()
                    .map(|t| {
                        let id = t.get("id").and_then(Value::as_u64).unwrap_or(0);
                        let status = t.get("status").and_then(Value::as_str).unwrap_or("pending");
                        let title = t.get("title").and_then(Value::as_str).unwrap_or("");
                        let desc = t.get("description").and_then(Value::as_str).unwrap_or("");
                        if desc.is_empty() {
                            format!("{id} [{status}] {title}")
                        } else {
                            format!("{id} [{status}] {title} - {desc}")
                        }
                    })
                    .collect::<Vec<_>>()
                    .join("\n");
                Ok(ToolOutput {
                    content: truncated(body),
                    ok: true,
                })
            }
            "update" => {
                let id = args.get("id").and_then(Value::as_u64).ok_or_else(|| {
                    VikramError::ProviderRejected("missing integer argument `id`".into())
                })?;
                let status = arg_str(args, "status")?;
                let mut tasks = load_tasks();
                let mut found = false;
                for t in tasks.iter_mut() {
                    if t.get("id").and_then(Value::as_u64) == Some(id) {
                        t["status"] = json!(status);
                        found = true;
                    }
                }
                if !found {
                    return Ok(ToolOutput {
                        content: format!("task not found: {id}"),
                        ok: false,
                    });
                }
                save_tasks(&tasks)?;
                Ok(ToolOutput {
                    content: format!("updated task {id} -> {status}"),
                    ok: true,
                })
            }
            other => Ok(ToolOutput {
                content: format!("unknown action `{other}`; expected create, list, or update"),
                ok: false,
            }),
        }
    }
}
