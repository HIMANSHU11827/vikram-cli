use serde_json::{Value, json};
use vikram_contracts::{ActionKind, PermissionRequest, SideEffect, Trust, VikramError};

use crate::PluginRegistry;

use crate::tools::*;
pub(crate) struct PluginExecute;

impl Tool for PluginExecute {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "plugin.execute".into(),
            description: "Execute a discovered plugin's entry point.".into(),
            schema: json!({
                "type": "object",
                "properties": {
                    "plugin": {"type": "string"},
                    "args": {"type": "string"}
                },
                "required": ["plugin"]
            }),
        }
    }

    fn permission(&self, args: &Value) -> Result<PermissionRequest, VikramError> {
        let plugin = arg_str(args, "plugin")?;
        Ok(PermissionRequest::new(
            ActionKind::Execute,
            "plugin.execute",
            plugin,
            SideEffect::LocalMutation,
            Trust::Model,
        ))
    }

    fn run(&self, args: &Value, ctx: &ToolContext) -> Result<ToolOutput, VikramError> {
        let plugin = arg_str(args, "plugin")?;
        let args_str = args.get("args").and_then(Value::as_str).unwrap_or("");
        let reg = PluginRegistry::for_workspace(&ctx.workspace_root);
        match reg.execute(&plugin, args_str) {
            Ok(out) => Ok(ToolOutput {
                content: truncated(out),
                ok: true,
            }),
            Err(e) => Ok(ToolOutput {
                content: e,
                ok: false,
            }),
        }
    }
}

pub(crate) struct PluginList;

impl Tool for PluginList {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "plugin.list".into(),
            description: "List discovered plugins and their loaded status.".into(),
            schema: json!({"type": "object", "properties": {}}),
        }
    }

    fn permission(&self, _args: &Value) -> Result<PermissionRequest, VikramError> {
        Ok(PermissionRequest::new(
            ActionKind::Read,
            "plugin.list",
            "plugins",
            SideEffect::None,
            Trust::Model,
        ))
    }

    fn run(&self, _args: &Value, ctx: &ToolContext) -> Result<ToolOutput, VikramError> {
        let reg = PluginRegistry::for_workspace(&ctx.workspace_root);
        let plugins = reg.plugins();
        if plugins.is_empty() {
            return Ok(ToolOutput {
                content: "no plugins".into(),
                ok: true,
            });
        }
        let body = plugins
            .iter()
            .map(|p| format!("{} (loaded={})", p.name, p.loaded))
            .collect::<Vec<_>>()
            .join("\n");
        Ok(ToolOutput {
            content: truncated(body),
            ok: true,
        })
    }
}

/// Shared entry point for the plugin lifecycle tools: the workspace view of the plugin set.
fn plugin_registry(ctx: &ToolContext) -> PluginRegistry {
    PluginRegistry::for_workspace(&ctx.workspace_root)
}

pub(crate) struct PluginInstall;

impl Tool for PluginInstall {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "plugin.install".into(),
            description: "Install a plugin directory into the plugins directory.".into(),
            schema: json!({
                "type": "object",
                "properties": {
                    "path": {"type": "string"}
                },
                "required": ["path"]
            }),
        }
    }

    fn permission(&self, args: &Value) -> Result<PermissionRequest, VikramError> {
        let path = arg_str(args, "path")?;
        Ok(PermissionRequest::new(
            ActionKind::Write,
            "plugin.install",
            path,
            SideEffect::Destructive,
            Trust::Model,
        ))
    }

    fn run(&self, args: &Value, ctx: &ToolContext) -> Result<ToolOutput, VikramError> {
        let raw = arg_str(args, "path")?;
        let (source, _) = canonical_resource(&ctx.workspace_root, &raw)?;
        let registry = plugin_registry(ctx);
        match registry.install(&source) {
            Ok(id) => Ok(ToolOutput {
                content: format!(
                    "installed plugin `{id}` into {}",
                    registry.root().join(&id).display()
                ),
                ok: true,
            }),
            Err(e) => Ok(ToolOutput {
                content: e,
                ok: false,
            }),
        }
    }
}

pub(crate) struct PluginUninstall;

impl Tool for PluginUninstall {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "plugin.uninstall".into(),
            description: "Remove an installed plugin's directory.".into(),
            schema: json!({
                "type": "object",
                "properties": {
                    "plugin": {"type": "string"}
                },
                "required": ["plugin"]
            }),
        }
    }

    fn permission(&self, args: &Value) -> Result<PermissionRequest, VikramError> {
        let plugin = arg_str(args, "plugin")?;
        Ok(PermissionRequest::new(
            ActionKind::Write,
            "plugin.uninstall",
            plugin,
            SideEffect::Destructive,
            Trust::Model,
        ))
    }

    fn run(&self, args: &Value, ctx: &ToolContext) -> Result<ToolOutput, VikramError> {
        let plugin = arg_str(args, "plugin")?;
        match plugin_registry(ctx).uninstall(&plugin) {
            Ok(()) => Ok(ToolOutput {
                content: format!("uninstalled plugin `{plugin}`"),
                ok: true,
            }),
            Err(e) => Ok(ToolOutput {
                content: e,
                ok: false,
            }),
        }
    }
}

pub(crate) struct PluginEnable;

impl Tool for PluginEnable {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "plugin.enable".into(),
            description: "Enable an installed plugin by rewriting its manifest.".into(),
            schema: json!({
                "type": "object",
                "properties": {
                    "plugin": {"type": "string"}
                },
                "required": ["plugin"]
            }),
        }
    }

    fn permission(&self, args: &Value) -> Result<PermissionRequest, VikramError> {
        let plugin = arg_str(args, "plugin")?;
        Ok(PermissionRequest::new(
            ActionKind::Write,
            "plugin.enable",
            plugin,
            SideEffect::LocalMutation,
            Trust::Model,
        ))
    }

    fn run(&self, args: &Value, ctx: &ToolContext) -> Result<ToolOutput, VikramError> {
        let plugin = arg_str(args, "plugin")?;
        match plugin_registry(ctx).enable(&plugin) {
            Ok(()) => Ok(ToolOutput {
                content: format!("enabled plugin `{plugin}`"),
                ok: true,
            }),
            Err(e) => Ok(ToolOutput {
                content: e,
                ok: false,
            }),
        }
    }
}

pub(crate) struct PluginDisable;

impl Tool for PluginDisable {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "plugin.disable".into(),
            description: "Disable an installed plugin by rewriting its manifest.".into(),
            schema: json!({
                "type": "object",
                "properties": {
                    "plugin": {"type": "string"}
                },
                "required": ["plugin"]
            }),
        }
    }

    fn permission(&self, args: &Value) -> Result<PermissionRequest, VikramError> {
        let plugin = arg_str(args, "plugin")?;
        Ok(PermissionRequest::new(
            ActionKind::Write,
            "plugin.disable",
            plugin,
            SideEffect::LocalMutation,
            Trust::Model,
        ))
    }

    fn run(&self, args: &Value, ctx: &ToolContext) -> Result<ToolOutput, VikramError> {
        let plugin = arg_str(args, "plugin")?;
        match plugin_registry(ctx).disable(&plugin) {
            Ok(()) => Ok(ToolOutput {
                content: format!("disabled plugin `{plugin}`"),
                ok: true,
            }),
            Err(e) => Ok(ToolOutput {
                content: e,
                ok: false,
            }),
        }
    }
}

pub(crate) struct PluginValidate;

impl Tool for PluginValidate {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "plugin.validate".into(),
            description: "Check a plugin directory's manifest without installing it.".into(),
            schema: json!({
                "type": "object",
                "properties": {
                    "path": {"type": "string"}
                },
                "required": ["path"]
            }),
        }
    }

    fn permission(&self, args: &Value) -> Result<PermissionRequest, VikramError> {
        let path = arg_str(args, "path")?;
        Ok(PermissionRequest::new(
            ActionKind::Read,
            "plugin.validate",
            path,
            SideEffect::None,
            Trust::Model,
        ))
    }

    fn run(&self, args: &Value, ctx: &ToolContext) -> Result<ToolOutput, VikramError> {
        let raw = arg_str(args, "path")?;
        let (source, _) = canonical_resource(&ctx.workspace_root, &raw)?;
        match plugin_registry(ctx).validate(&source) {
            Ok(issues) if issues.is_empty() => Ok(ToolOutput {
                content: format!("{}: no problems found", source.display()),
                ok: true,
            }),
            Ok(issues) => Ok(ToolOutput {
                ok: !issues.iter().any(|i| i.starts_with("error:")),
                content: truncated(issues.join("\n")),
            }),
            Err(e) => Ok(ToolOutput {
                content: e,
                ok: false,
            }),
        }
    }
}

pub(crate) struct PluginInspect;

impl Tool for PluginInspect {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "plugin.inspect".into(),
            description: "Show an installed plugin's manifest summary.".into(),
            schema: json!({
                "type": "object",
                "properties": {
                    "plugin": {"type": "string"}
                },
                "required": ["plugin"]
            }),
        }
    }

    fn permission(&self, args: &Value) -> Result<PermissionRequest, VikramError> {
        let plugin = arg_str(args, "plugin")?;
        Ok(PermissionRequest::new(
            ActionKind::Read,
            "plugin.inspect",
            plugin,
            SideEffect::None,
            Trust::Model,
        ))
    }

    fn run(&self, args: &Value, ctx: &ToolContext) -> Result<ToolOutput, VikramError> {
        let plugin = arg_str(args, "plugin")?;
        match plugin_registry(ctx).inspect(&plugin) {
            Ok(summary) => Ok(ToolOutput {
                content: truncated(summary),
                ok: true,
            }),
            Err(e) => Ok(ToolOutput {
                content: e,
                ok: false,
            }),
        }
    }
}

pub(crate) struct PluginDeps;

impl Tool for PluginDeps {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "plugin.deps".into(),
            description: "Report installed plugins whose dependencies are missing or unsatisfied."
                .into(),
            schema: json!({"type": "object", "properties": {}}),
        }
    }

    fn permission(&self, _args: &Value) -> Result<PermissionRequest, VikramError> {
        Ok(PermissionRequest::new(
            ActionKind::Read,
            "plugin.deps",
            "plugins",
            SideEffect::None,
            Trust::Model,
        ))
    }

    fn run(&self, _args: &Value, ctx: &ToolContext) -> Result<ToolOutput, VikramError> {
        let problems = plugin_registry(ctx).resolve_dependencies();
        if problems.is_empty() {
            return Ok(ToolOutput {
                content: "all plugin dependencies are satisfied".into(),
                ok: true,
            });
        }
        let mut body = String::new();
        for (id, issues) in &problems {
            body.push_str(&format!("{id}:\n"));
            for issue in issues {
                body.push_str(&format!("  - {issue}\n"));
            }
        }
        Ok(ToolOutput {
            content: truncated(body),
            ok: true,
        })
    }
}
