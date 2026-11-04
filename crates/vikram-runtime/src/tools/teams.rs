use serde_json::{Value, json};
use std::time::{Duration, Instant};
use vikram_contracts::{ActionKind, AgentId, PermissionRequest, SideEffect, Trust, VikramError};

use crate::config::database_path;
use crate::teams::TeamManager;
use vikram_store::{AgentRow, Store};

use crate::tools::*;
/// A tool wrapper that invokes an MCP server tool.
pub(crate) struct TeamCreate;

impl Tool for TeamCreate {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "team.create".into(),
            description: "Create a new team.".into(),
            schema: json!({
                "type": "object",
                "properties": {"name": {"type": "string"}},
                "required": ["name"]
            }),
        }
    }

    fn permission(&self, args: &Value) -> Result<PermissionRequest, VikramError> {
        let name = arg_str(args, "name")?;
        Ok(PermissionRequest::new(
            ActionKind::Write,
            "team.create",
            name,
            SideEffect::LocalMutation,
            Trust::Model,
        ))
    }

    fn run(&self, args: &Value, _ctx: &ToolContext) -> Result<ToolOutput, VikramError> {
        let name = arg_str(args, "name")?;
        let mgr = TeamManager::load();
        match mgr.create(&name) {
            Ok(()) => Ok(ToolOutput {
                content: format!("created team `{name}`"),
                ok: true,
            }),
            Err(e) => Ok(ToolOutput {
                content: e,
                ok: false,
            }),
        }
    }
}

pub(crate) struct TeamList;

impl Tool for TeamList {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "team.list".into(),
            description: "List teams and their members.".into(),
            schema: json!({
                "type": "object",
                "properties": {}
            }),
        }
    }

    fn permission(&self, _args: &Value) -> Result<PermissionRequest, VikramError> {
        Ok(PermissionRequest::new(
            ActionKind::Read,
            "team.list",
            "teams",
            SideEffect::None,
            Trust::Model,
        ))
    }

    fn run(&self, _args: &Value, _ctx: &ToolContext) -> Result<ToolOutput, VikramError> {
        let mgr = TeamManager::load();
        let teams = mgr.list();
        if teams.is_empty() {
            return Ok(ToolOutput {
                content: "no teams".into(),
                ok: true,
            });
        }
        let body = teams
            .iter()
            .map(|t| {
                let members = t
                    .members
                    .iter()
                    .map(|m| m.as_str())
                    .collect::<Vec<_>>()
                    .join(", ");
                format!("{}: [{}]", t.name, members)
            })
            .collect::<Vec<_>>()
            .join("\n");
        Ok(ToolOutput {
            content: truncated(body),
            ok: true,
        })
    }
}

pub(crate) struct TeamDelegate;

impl Tool for TeamDelegate {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "team.delegate".into(),
            description: "Delegate a task to a team or agent.".into(),
            schema: json!({
                "type": "object",
                "properties": {
                    "team": {"type": "string"},
                    "task": {"type": "string"},
                    "agent": {"type": "string"}
                },
                "required": ["team", "task"]
            }),
        }
    }

    fn permission(&self, args: &Value) -> Result<PermissionRequest, VikramError> {
        let team = arg_str(args, "team")?;
        Ok(PermissionRequest::new(
            ActionKind::Delegate,
            "team.delegate",
            team,
            SideEffect::LocalMutation,
            Trust::Model,
        ))
    }

    fn run(&self, args: &Value, _ctx: &ToolContext) -> Result<ToolOutput, VikramError> {
        let team = arg_str(args, "team")?;
        let task = arg_str(args, "task")?;
        let agent = args.get("agent").and_then(Value::as_str).map(AgentId::new);
        let mgr = TeamManager::load();
        match mgr.delegate_task(&team, &task, agent) {
            Ok(()) => Ok(ToolOutput {
                content: format!("delegated task to team `{team}`"),
                ok: true,
            }),
            Err(e) => Ok(ToolOutput {
                content: e,
                ok: false,
            }),
        }
    }
}

pub(crate) struct HiveCreate;

impl Tool for HiveCreate {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "hive.create".into(),
            description: "Create a new HIVE team.".into(),
            schema: json!({
                "type": "object",
                "properties": {"name": {"type": "string"}},
                "required": ["name"]
            }),
        }
    }

    fn permission(&self, args: &Value) -> Result<PermissionRequest, VikramError> {
        let name = arg_str(args, "name")?;
        Ok(PermissionRequest::new(
            ActionKind::Write,
            "hive.create",
            name,
            SideEffect::LocalMutation,
            Trust::Model,
        ))
    }

    fn run(&self, args: &Value, _ctx: &ToolContext) -> Result<ToolOutput, VikramError> {
        let name = arg_str(args, "name")?;
        let mgr = TeamManager::load();
        match mgr.create(&name) {
            Ok(()) => Ok(ToolOutput {
                content: format!("created team `{name}`"),
                ok: true,
            }),
            Err(e) => Ok(ToolOutput {
                content: e,
                ok: false,
            }),
        }
    }
}

pub(crate) struct HiveList;

impl Tool for HiveList {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "hive.list".into(),
            description: "List HIVE teams and their members.".into(),
            schema: json!({
                "type": "object",
                "properties": {}
            }),
        }
    }

    fn permission(&self, _args: &Value) -> Result<PermissionRequest, VikramError> {
        Ok(PermissionRequest::new(
            ActionKind::Read,
            "hive.list",
            "teams",
            SideEffect::None,
            Trust::Model,
        ))
    }

    fn run(&self, _args: &Value, _ctx: &ToolContext) -> Result<ToolOutput, VikramError> {
        let mgr = TeamManager::load();
        let teams = mgr.list();
        if teams.is_empty() {
            return Ok(ToolOutput {
                content: "no teams".into(),
                ok: true,
            });
        }
        let body = teams
            .iter()
            .map(|t| {
                let members = t
                    .members
                    .iter()
                    .map(|m| m.as_str())
                    .collect::<Vec<_>>()
                    .join(", ");
                format!("{}: [{}]", t.name, members)
            })
            .collect::<Vec<_>>()
            .join("\n");
        Ok(ToolOutput {
            content: truncated(body),
            ok: true,
        })
    }
}

pub(crate) struct HiveShow;

impl Tool for HiveShow {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "hive.show".into(),
            description: "Show details of a HIVE team.".into(),
            schema: json!({
                "type": "object",
                "properties": {"name": {"type": "string"}},
                "required": ["name"]
            }),
        }
    }

    fn permission(&self, args: &Value) -> Result<PermissionRequest, VikramError> {
        let name = arg_str(args, "name")?;
        Ok(PermissionRequest::new(
            ActionKind::Read,
            "hive.show",
            name,
            SideEffect::None,
            Trust::Model,
        ))
    }

    fn run(&self, args: &Value, _ctx: &ToolContext) -> Result<ToolOutput, VikramError> {
        let name = arg_str(args, "name")?;
        let mgr = TeamManager::load();
        match mgr.get(&name) {
            Some(team) => {
                let members = team
                    .members
                    .iter()
                    .map(|m| m.as_str())
                    .collect::<Vec<_>>()
                    .join(", ");
                let task = team.task.clone().unwrap_or_else(|| "(none)".to_string());
                let content = format!(
                    "name: {}\nmembers: [{}]\ntask: {}\ncreated_ms: {}",
                    team.name, members, task, team.created_ms
                );
                Ok(ToolOutput {
                    content: truncated(content),
                    ok: true,
                })
            }
            None => Ok(ToolOutput {
                content: format!("team not found: {name}"),
                ok: false,
            }),
        }
    }
}

pub(crate) struct HiveDelegate;

impl Tool for HiveDelegate {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "hive.delegate".into(),
            description: "Delegate a task to a HIVE team.".into(),
            schema: json!({
                "type": "object",
                "properties": {
                    "team": {"type": "string"},
                    "task": {"type": "string"},
                    "agent": {"type": "string"}
                },
                "required": ["team", "task"]
            }),
        }
    }

    fn permission(&self, args: &Value) -> Result<PermissionRequest, VikramError> {
        let team = arg_str(args, "team")?;
        Ok(PermissionRequest::new(
            ActionKind::Delegate,
            "hive.delegate",
            team,
            SideEffect::LocalMutation,
            Trust::Model,
        ))
    }

    fn run(&self, args: &Value, _ctx: &ToolContext) -> Result<ToolOutput, VikramError> {
        let team = arg_str(args, "team")?;
        let task = arg_str(args, "task")?;
        let agent = args.get("agent").and_then(Value::as_str).map(AgentId::new);
        let mgr = TeamManager::load();
        match mgr.delegate_task(&team, &task, agent) {
            Ok(()) => Ok(ToolOutput {
                content: format!("delegated task to team `{team}`"),
                ok: true,
            }),
            Err(e) => Ok(ToolOutput {
                content: e,
                ok: false,
            }),
        }
    }
}

pub(crate) struct HiveDelete;

impl Tool for HiveDelete {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "hive.delete".into(),
            description: "Delete a HIVE team.".into(),
            schema: json!({
                "type": "object",
                "properties": {"name": {"type": "string"}},
                "required": ["name"]
            }),
        }
    }

    fn permission(&self, args: &Value) -> Result<PermissionRequest, VikramError> {
        let name = arg_str(args, "name")?;
        Ok(PermissionRequest::new(
            ActionKind::Write,
            "hive.delete",
            name,
            SideEffect::Destructive,
            Trust::Model,
        ))
    }

    fn run(&self, args: &Value, _ctx: &ToolContext) -> Result<ToolOutput, VikramError> {
        let name = arg_str(args, "name")?;
        let mgr = TeamManager::load();
        match mgr.delete(&name) {
            Ok(()) => Ok(ToolOutput {
                content: format!("deleted team `{name}`"),
                ok: true,
            }),
            Err(e) => Ok(ToolOutput {
                content: e,
                ok: false,
            }),
        }
    }
}

// ---- Bot tools (saved agent profiles in the store) ----

pub(crate) struct BotList;

impl Tool for BotList {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "bot.list".into(),
            description: "List saved bot profiles from the store.".into(),
            schema: json!({"type": "object", "properties": {}}),
        }
    }

    fn permission(&self, _args: &Value) -> Result<PermissionRequest, VikramError> {
        Ok(PermissionRequest::new(
            ActionKind::Read,
            "bot.list",
            "bots",
            SideEffect::None,
            Trust::Model,
        ))
    }

    fn run(&self, _args: &Value, _ctx: &ToolContext) -> Result<ToolOutput, VikramError> {
        let store =
            Store::open(&database_path()).map_err(|e| VikramError::Storage(e.to_string()))?;
        let rows = store
            .list_agents()
            .map_err(|e| VikramError::Storage(e.to_string()))?;
        if rows.is_empty() {
            return Ok(ToolOutput {
                content: "no bots".into(),
                ok: true,
            });
        }
        let body = rows
            .iter()
            .map(|r| format!("{} ({}) - {}", r.name, r.role, r.description))
            .collect::<Vec<_>>()
            .join("\n");
        Ok(ToolOutput {
            content: truncated(body),
            ok: true,
        })
    }
}

pub(crate) struct BotShow;

impl Tool for BotShow {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "bot.show".into(),
            description: "Show details of a saved bot profile.".into(),
            schema: json!({
                "type": "object",
                "properties": {"name": {"type": "string"}},
                "required": ["name"]
            }),
        }
    }

    fn permission(&self, args: &Value) -> Result<PermissionRequest, VikramError> {
        let name = arg_str(args, "name")?;
        Ok(PermissionRequest::new(
            ActionKind::Read,
            "bot.show",
            name,
            SideEffect::None,
            Trust::Model,
        ))
    }

    fn run(&self, args: &Value, _ctx: &ToolContext) -> Result<ToolOutput, VikramError> {
        let name = arg_str(args, "name")?;
        let store =
            Store::open(&database_path()).map_err(|e| VikramError::Storage(e.to_string()))?;
        let rows = store
            .list_agents()
            .map_err(|e| VikramError::Storage(e.to_string()))?;
        match rows.into_iter().find(|r| r.name == name) {
            Some(row) => {
                let content = format!(
                    "id: {}\nname: {}\nrole: {}\ndescription: {}\nicon: {}\ncreated_ms: {}",
                    row.id.as_str(),
                    row.name,
                    row.role,
                    row.description,
                    row.icon.as_deref().unwrap_or("(none)"),
                    row.created_ms
                );
                Ok(ToolOutput {
                    content: truncated(content),
                    ok: true,
                })
            }
            None => Ok(ToolOutput {
                content: format!("bot not found: {name}"),
                ok: false,
            }),
        }
    }
}

pub(crate) struct BotCreate;

impl Tool for BotCreate {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "bot.create".into(),
            description: "Create a new bot profile in the store.".into(),
            schema: json!({
                "type": "object",
                "properties": {
                    "name": {"type": "string"},
                    "role": {"type": "string"},
                    "description": {"type": "string"}
                },
                "required": ["name", "role", "description"]
            }),
        }
    }

    fn permission(&self, args: &Value) -> Result<PermissionRequest, VikramError> {
        let name = arg_str(args, "name")?;
        Ok(PermissionRequest::new(
            ActionKind::Write,
            "bot.create",
            name,
            SideEffect::LocalMutation,
            Trust::Model,
        ))
    }

    fn run(&self, args: &Value, _ctx: &ToolContext) -> Result<ToolOutput, VikramError> {
        let name = arg_str(args, "name")?;
        let role = arg_str(args, "role")?;
        let description = arg_str(args, "description")?;
        let store =
            Store::open(&database_path()).map_err(|e| VikramError::Storage(e.to_string()))?;
        let row = AgentRow {
            id: AgentId::generate(),
            name: name.clone(),
            role,
            description,
            icon: None,
            created_ms: crate::types::now_ms(),
        };
        store
            .save_agent(&row)
            .map_err(|e| VikramError::Storage(e.to_string()))?;
        Ok(ToolOutput {
            content: format!("created bot `{}` (id {})", name, row.id.as_str()),
            ok: true,
        })
    }
}

// ---- Agent tools (saved agent profiles in the store) ----

pub(crate) struct AgentList;

impl Tool for AgentList {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "agent.list".into(),
            description: "List agents from the store.".into(),
            schema: json!({"type": "object", "properties": {}}),
        }
    }

    fn permission(&self, _args: &Value) -> Result<PermissionRequest, VikramError> {
        Ok(PermissionRequest::new(
            ActionKind::Read,
            "agent.list",
            "agents",
            SideEffect::None,
            Trust::Model,
        ))
    }

    fn run(&self, _args: &Value, _ctx: &ToolContext) -> Result<ToolOutput, VikramError> {
        let store =
            Store::open(&database_path()).map_err(|e| VikramError::Storage(e.to_string()))?;
        let rows = store
            .list_agents()
            .map_err(|e| VikramError::Storage(e.to_string()))?;
        if rows.is_empty() {
            return Ok(ToolOutput {
                content: "no agents".into(),
                ok: true,
            });
        }
        let body = rows
            .iter()
            .map(|r| format!("{} ({}) - {}", r.name, r.role, r.description))
            .collect::<Vec<_>>()
            .join("\n");
        Ok(ToolOutput {
            content: truncated(body),
            ok: true,
        })
    }
}

pub(crate) struct AgentShow;

impl Tool for AgentShow {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "agent.show".into(),
            description: "Show details of an agent from the store.".into(),
            schema: json!({
                "type": "object",
                "properties": {"name": {"type": "string"}},
                "required": ["name"]
            }),
        }
    }

    fn permission(&self, args: &Value) -> Result<PermissionRequest, VikramError> {
        let name = arg_str(args, "name")?;
        Ok(PermissionRequest::new(
            ActionKind::Read,
            "agent.show",
            name,
            SideEffect::None,
            Trust::Model,
        ))
    }

    fn run(&self, args: &Value, _ctx: &ToolContext) -> Result<ToolOutput, VikramError> {
        let name = arg_str(args, "name")?;
        let store =
            Store::open(&database_path()).map_err(|e| VikramError::Storage(e.to_string()))?;
        let rows = store
            .list_agents()
            .map_err(|e| VikramError::Storage(e.to_string()))?;
        match rows.into_iter().find(|r| r.name == name) {
            Some(row) => {
                let content = format!(
                    "id: {}\nname: {}\nrole: {}\ndescription: {}\nicon: {}\ncreated_ms: {}",
                    row.id.as_str(),
                    row.name,
                    row.role,
                    row.description,
                    row.icon.as_deref().unwrap_or("(none)"),
                    row.created_ms
                );
                Ok(ToolOutput {
                    content: truncated(content),
                    ok: true,
                })
            }
            None => Ok(ToolOutput {
                content: format!("agent not found: {name}"),
                ok: false,
            }),
        }
    }
}

pub(crate) struct Diagnostics;

impl Tool for Diagnostics {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "diagnostics.collect".into(),
            description:
                "Collect compiler/lint diagnostics by running cargo check or an available linter."
                    .into(),
            schema: json!({
                "type": "object",
                "properties": {}
            }),
        }
    }

    fn permission(&self, _args: &Value) -> Result<PermissionRequest, VikramError> {
        Ok(PermissionRequest::new(
            ActionKind::Read,
            "diagnostics.collect",
            "diagnostics",
            SideEffect::None,
            Trust::Model,
        ))
    }

    fn run(&self, _args: &Value, ctx: &ToolContext) -> Result<ToolOutput, VikramError> {
        // Decide command.
        let cargo_toml = ctx.workspace_root.join("Cargo.toml");
        let (program, args) = if cargo_toml.exists() {
            ("cargo", vec!["check".to_string()])
        } else {
            // Fall back: try cargo check anyway? Or python compileall.
            // Attempt a generic lint: if package.json exists, run npm run lint? Keep simple: cargo check.
            ("cargo", vec!["check".to_string()])
        };

        let mut cmd = std::process::Command::new(program);
        cmd.args(&args)
            .current_dir(&ctx.workspace_root)
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped());

        let mut child = match cmd.spawn() {
            Ok(c) => c,
            Err(e) => {
                return Ok(ToolOutput {
                    content: format!("failed to start diagnostics: {e}"),
                    ok: false,
                });
            }
        };

        let timeout_ms: u64 = 120_000;
        let deadline = Instant::now() + Duration::from_millis(timeout_ms);
        loop {
            match child.try_wait() {
                Ok(Some(_)) => break,
                Ok(None) => {
                    if Instant::now() >= deadline {
                        let _ = child.kill();
                        let _ = child.wait();
                        return Ok(ToolOutput {
                            content: format!("diagnostics timed out after {timeout_ms} ms"),
                            ok: false,
                        });
                    }
                    std::thread::sleep(Duration::from_millis(50));
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

        let stdout = String::from_utf8_lossy(&output.stdout).to_string();
        let stderr = String::from_utf8_lossy(&output.stderr).to_string();
        let mut body = stdout.clone();
        if !stderr.trim().is_empty() {
            if !body.is_empty() && !body.ends_with('\n') {
                body.push('\n');
            }
            body.push_str(&stderr);
        }
        if body.trim().is_empty() {
            body = format!("(no output, exit {:?})", output.status.code());
        }

        // Count errors/warnings.
        let mut errors: usize = 0;
        let mut warnings: usize = 0;
        for line in body.lines() {
            let t = line.trim_start();
            if t.starts_with("error") {
                errors += 1;
            } else if t.starts_with("warning") {
                warnings += 1;
            }
        }

        let content = format!("errors: {}, warnings: {}\n{}", errors, warnings, body);
        Ok(ToolOutput {
            content: truncated(content),
            ok: output.status.success(),
        })
    }
}
