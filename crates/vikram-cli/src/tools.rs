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
    }
}

fn list_tools(args: ListArgs) -> Result<(), String> {
    let registry = ToolRegistry::with_builtins();
    let all_specs = registry.specs();
    let filtered: Vec<&ToolSpec> = if let Some(ref q) = args.query {
        all_specs.iter().filter(|s| s.name.contains(q.as_str())).collect()
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
        println!("{}", serde_json::to_string_pretty(&rows).map_err(|e| e.to_string())?);
        return Ok(());
    }

    if filtered.is_empty() {
        println!("no tools matched");
        return Ok(());
    }

    // Column widths
    let name_width = filtered.iter().map(|s| s.name.len()).max().unwrap_or(24).max(24);
    let desc_width = 60;

    println!(
        "{:<name_width$}  {}",
        "NAME",
        "DESCRIPTION",
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
        let desc = if desc.len() > desc_width {
            format!("{}...", &desc[..desc_width - 3])
        } else {
            desc.to_string()
        };
        println!("{:<name_width$}  {}", spec.name, desc, name_width = name_width);
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
    println!("schema:      {}", serde_json::to_string_pretty(&spec.schema).map_err(|e| e.to_string())?);
    Ok(())
}
