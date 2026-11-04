use serde_json::{Value, json};
use vikram_contracts::{ActionKind, PermissionRequest, SideEffect, Trust, VikramError};

use crate::SkillRegistry;

use crate::tools::*;
pub(crate) struct SkillList;

impl Tool for SkillList {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "skill.list".into(),
            description: "List available skills (name and description).".into(),
            schema: json!({
                "type": "object",
                "properties": {}
            }),
        }
    }

    fn permission(&self, _args: &Value) -> Result<PermissionRequest, VikramError> {
        Ok(PermissionRequest::new(
            ActionKind::Read,
            "skill.list",
            "skills",
            SideEffect::None,
            Trust::Model,
        ))
    }

    fn run(&self, _args: &Value, ctx: &ToolContext) -> Result<ToolOutput, VikramError> {
        let reg = SkillRegistry::for_workspace(&ctx.workspace_root);
        let skills = reg.list();
        if skills.is_empty() {
            return Ok(ToolOutput {
                content: "no skills available".into(),
                ok: true,
            });
        }
        let body = skills
            .iter()
            .map(|s| format!("{}: {}", s.name, s.description))
            .collect::<Vec<_>>()
            .join("\n");
        Ok(ToolOutput {
            content: truncated(body),
            ok: true,
        })
    }
}

pub(crate) struct SkillLoad;

impl Tool for SkillLoad {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "skill.load".into(),
            description: "Load a skill's instructions by name.".into(),
            schema: json!({
                "type": "object",
                "properties": {
                    "name": {"type": "string"}
                },
                "required": ["name"]
            }),
        }
    }

    fn permission(&self, args: &Value) -> Result<PermissionRequest, VikramError> {
        let name = arg_str(args, "name")?;
        Ok(PermissionRequest::new(
            ActionKind::Read,
            "skill.load",
            name,
            SideEffect::None,
            Trust::Model,
        ))
    }

    fn run(&self, args: &Value, ctx: &ToolContext) -> Result<ToolOutput, VikramError> {
        let name = arg_str(args, "name")?;
        let reg = SkillRegistry::for_workspace(&ctx.workspace_root);
        match reg.load(&name) {
            Some(skill) => Ok(ToolOutput {
                content: skill.instructions,
                ok: true,
            }),
            None => Ok(ToolOutput {
                content: format!("skill not found: {name}"),
                ok: false,
            }),
        }
    }
}

pub(crate) struct SkillSearch;

impl Tool for SkillSearch {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "skill.search".into(),
            description: "Search available skills by query (name and description).".into(),
            schema: json!({
                "type": "object",
                "properties": {
                    "query": {"type": "string"}
                },
                "required": ["query"]
            }),
        }
    }

    fn permission(&self, _args: &Value) -> Result<PermissionRequest, VikramError> {
        Ok(PermissionRequest::new(
            ActionKind::Read,
            "skill.search",
            "skills",
            SideEffect::None,
            Trust::Model,
        ))
    }

    fn run(&self, args: &Value, ctx: &ToolContext) -> Result<ToolOutput, VikramError> {
        let query = arg_str(args, "query")?;
        let reg = SkillRegistry::for_workspace(&ctx.workspace_root);
        let skills = reg.search(&query);
        if skills.is_empty() {
            return Ok(ToolOutput {
                content: "no matching skills".into(),
                ok: true,
            });
        }
        let body = skills
            .iter()
            .map(|s| format!("{}: {}", s.name, s.description))
            .collect::<Vec<_>>()
            .join("\n");
        Ok(ToolOutput {
            content: truncated(body),
            ok: true,
        })
    }
}

pub(crate) struct SkillRead;

impl Tool for SkillRead {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "skill.read".into(),
            description: "Read a resource file bundled with a skill by name and relative path."
                .into(),
            schema: json!({
                "type": "object",
                "properties": {
                    "name": {"type": "string"},
                    "path": {"type": "string"}
                },
                "required": ["name", "path"]
            }),
        }
    }

    fn permission(&self, args: &Value) -> Result<PermissionRequest, VikramError> {
        let name = arg_str(args, "name")?;
        Ok(PermissionRequest::new(
            ActionKind::Read,
            "skill.read",
            name,
            SideEffect::None,
            Trust::Model,
        ))
    }

    fn run(&self, args: &Value, ctx: &ToolContext) -> Result<ToolOutput, VikramError> {
        let name = arg_str(args, "name")?;
        let rel = arg_str(args, "path")?;
        let reg = SkillRegistry::for_workspace(&ctx.workspace_root);
        match reg.read_resource(&name, &rel) {
            Ok(content) => Ok(ToolOutput {
                content: truncated(content),
                ok: true,
            }),
            Err(e) => Ok(ToolOutput {
                content: e,
                ok: false,
            }),
        }
    }
}

/// Format the registered name for an MCP tool.
pub fn mcp_tool_name(server: &str, tool: &str) -> String {
    format!("mcp.{server}.{tool}")
}
