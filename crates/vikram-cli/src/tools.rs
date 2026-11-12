//! `vikram tools` — inspect the built-in tool surface.
//!
//! This is the operator's view into what Vikram can call, mirroring how
//! Hermes exposes `~/.hermes/bin/` as files.

use clap::{Args, Subcommand};
use vikram_runtime::config::Config;
use vikram_runtime::tools::{ToolRegistry, ToolSpec};

#[derive(Debug, Subcommand)]
pub enum ToolsAction {
    /// List every built-in tool with its description.
    List(ListArgs),
    /// Show full details for one tool (schema, description).
    Show { name: String },
    /// List per-tool permission overrides (allow, deny, ask).
    Permissions,
    /// Set a per-tool permission override: allow, deny, or ask.
    SetPermission { tool: String, permission: String },
    /// Remove a per-tool permission override.
    RemovePermission { tool: String },
}

#[derive(Debug, Args, Default)]
pub struct ListArgs {
    /// Emit JSON instead of a table.
    #[arg(long)]
    json: bool,
    /// Filter by a substring in the tool name.
    #[arg(long)]
    query: Option<String>,
}

pub fn run(action: ToolsAction, _config: &Config) -> Result<(), String> {
    match action {
        ToolsAction::List(args) => list_tools(args),
        ToolsAction::Show { name } => show_tool(&name),
        ToolsAction::Permissions => list_permissions(),
        ToolsAction::SetPermission { tool, permission } => set_permission(&tool, &permission),
        ToolsAction::RemovePermission { tool } => remove_permission(&tool),
    }
}

fn list_permissions() -> Result<(), String> {
    let config = Config::load().map_err(|err| err.to_string())?;
    let mut rows: Vec<(&String, &String)> = config
        .security
        .tool_permissions
        .iter()
        .chain(config.tool_permissions.iter())
        .collect();
    rows.sort_by(|a, b| a.0.cmp(b.0));
    rows.dedup_by(|a, b| a.0 == b.0);
    if rows.is_empty() {
        println!("no tool permission overrides");
        return Ok(());
    }
    for (tool, permission) in rows {
        println!("{tool}  {permission}");
    }
    Ok(())
}

/// Load the durable config, apply the change, save. Loading fresh (rather than mutating the
/// process config) keeps global flag overrides like `--model` out of the written file.
fn edit_permissions(
    edit: impl FnOnce(&mut Config) -> Result<String, String>,
) -> Result<(), String> {
    let mut config = Config::load().map_err(|err| err.to_string())?;
    let message = edit(&mut config)?;
    config.save().map_err(|err| err.to_string())?;
    println!(
        "{message} ({})",
        vikram_runtime::config::config_path().display()
    );
    Ok(())
}

pub fn set_permission(tool: &str, permission: &str) -> Result<(), String> {
    let tool = tool.trim();
    if tool.is_empty() {
        return Err("tool name must not be empty".to_owned());
    }
    let perm = permission.trim().to_lowercase();
    if !matches!(perm.as_str(), "allow" | "deny" | "ask") {
        return Err(format!(
            "unknown permission `{permission}`; choose from allow, deny, ask"
        ));
    }
    let tool = tool.to_owned();
    edit_permissions(move |config| {
        config
            .security
            .tool_permissions
            .insert(tool.clone(), perm.clone());
        config.tool_permissions.insert(tool.clone(), perm.clone());
        Ok(format!("set tool permission: {tool} -> {perm}"))
    })
}

pub fn remove_permission(tool: &str) -> Result<(), String> {
    let tool = tool.trim();
    if tool.is_empty() {
        return Err("tool name must not be empty".to_owned());
    }
    let tool = tool.to_owned();
    edit_permissions(move |config| {
        let security = config.security.tool_permissions.remove(&tool);
        let flat = config.tool_permissions.remove(&tool);
        match (security, flat) {
            (None, None) => Err(format!("no permission override for `{tool}`")),
            _ => Ok(format!("removed tool permission override: {tool}")),
        }
    })
}

fn list_tools(args: ListArgs) -> Result<(), String> {
    let registry = ToolRegistry::with_builtins();
    let all_specs = registry.specs();
    let filtered: Vec<&ToolSpec> = if let Some(ref q) = args.query {
        all_specs
            .iter()
            .filter(|s| s.name.contains(q.as_str()))
            .collect()
    } else {
        all_specs.iter().collect()
    };

    if args.json {
        let rows: Vec<serde_json::Value> = filtered
            .iter()
            .map(|s| {
                serde_json::json!({
                    "name": s.name,
                    "description": s.description,
                })
            })
            .collect();
        println!(
            "{}",
            serde_json::to_string_pretty(&rows).map_err(|e| e.to_string())?
        );
        return Ok(());
    }

    if filtered.is_empty() {
        println!("no tools matched");
        return Ok(());
    }

    // Column widths
    let name_width = filtered
        .iter()
        .map(|s| s.name.len())
        .max()
        .unwrap_or(24)
        .max(24);
    let desc_width = 60;

    println!(
        "{:<name_width$}  DESCRIPTION",
        "NAME",
        name_width = name_width
    );
    println!(
        "{:-<name_width$}  {:-<desc_width$}",
        "",
        "",
        name_width = name_width,
        desc_width = desc_width
    );
    for spec in &filtered {
        let desc = spec.description.lines().next().unwrap_or("").trim();
        let desc = if desc.chars().count() > desc_width {
            let truncated: String = desc.chars().take(desc_width.saturating_sub(3)).collect();
            format!("{}...", truncated)
        } else {
            desc.to_string()
        };
        println!(
            "{:<name_width$}  {}",
            spec.name,
            desc,
            name_width = name_width
        );
    }

    println!("\n{} tool(s)", filtered.len());
    Ok(())
}

fn show_tool(name: &str) -> Result<(), String> {
    let registry = ToolRegistry::with_builtins();
    let all_specs = registry.specs();
    let spec = all_specs
        .iter()
        .find(|s| s.name == name)
        .ok_or_else(|| format!("no tool named `{name}` (use `vikram tools list` to browse)"))?;

    println!("name:        {}", spec.name);
    println!("description: {}", spec.description);
    println!(
        "schema:      {}",
        serde_json::to_string_pretty(&spec.schema).map_err(|e| e.to_string())?
    );
    Ok(())
}
