use serde_json::{Value, json};
use std::path::Path;
use vikram_contracts::{ActionKind, PermissionRequest, SideEffect, Trust, VikramError};

use crate::tools::*;
pub(crate) struct SearchText;

impl Tool for SearchText {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "search.text".into(),
            description: "Search workspace text files for a substring.".into(),
            schema: json!({
                "type": "object",
                "properties": {
                    "query": {"type": "string"},
                    "path": {"type": "string"}
                },
                "required": ["query"]
            }),
        }
    }

    fn permission(&self, args: &Value) -> Result<PermissionRequest, VikramError> {
        let query = arg_str(args, "query")?;
        Ok(PermissionRequest::new(
            ActionKind::Read,
            "search.text",
            query,
            SideEffect::None,
            Trust::Model,
        ))
    }

    fn run(&self, args: &Value, ctx: &ToolContext) -> Result<ToolOutput, VikramError> {
        let query = arg_str(args, "query")?;
        let raw = args.get("path").and_then(Value::as_str).unwrap_or(".");
        let (root, _) = canonical_resource(&ctx.workspace_root, raw)?;
        let mut hits: Vec<String> = Vec::new();
        walk(&root, &query, &mut hits, 0);
        if hits.is_empty() {
            return Ok(ToolOutput {
                content: format!("no matches for `{query}`"),
                ok: true,
            });
        }
        Ok(ToolOutput {
            content: truncated(hits.join("\n")),
            ok: true,
        })
    }
}

fn walk(dir: &Path, query: &str, hits: &mut Vec<String>, depth: usize) {
    if depth > 12 || hits.len() >= 200 {
        return;
    }
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        let name = entry.file_name().to_string_lossy().into_owned();
        if name == ".git" || name == "target" || name == "node_modules" {
            continue;
        }
        if path.is_dir() {
            walk(&path, query, hits, depth + 1);
            continue;
        }
        let Ok(meta) = entry.metadata() else { continue };
        if meta.len() > 1_000_000 {
            continue;
        }
        let Ok(text) = std::fs::read_to_string(&path) else {
            continue;
        };
        for (idx, line) in text.lines().enumerate() {
            if hits.len() >= 200 {
                return;
            }
            if line.contains(query) {
                hits.push(format!("{}:{}: {}", path.display(), idx + 1, line.trim()));
            }
        }
    }
}
