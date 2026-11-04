use serde_json::{Value, json};
use vikram_codeindex::{CodeIndex, SymbolKind};
use vikram_contracts::{ActionKind, PermissionRequest, SideEffect, Trust, VikramError};

use crate::tools::*;

pub(crate) struct CodeIndexTool;

impl Tool for CodeIndexTool {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "codeindex".into(),
            description: "Structural repository intelligence: index, search symbols, repo map."
                .into(),
            schema: json!({
                "type": "object",
                "properties": {
                    "action": {"type": "string", "enum": ["index", "search", "repo_map", "symbols"]},
                    "query": {"type": "string"},
                    "path": {"type": "string"},
                    "budget_tokens": {"type": "integer"}
                },
                "required": ["action"]
            }),
        }
    }

    fn permission(&self, args: &Value) -> Result<PermissionRequest, VikramError> {
        let action = args
            .get("action")
            .and_then(Value::as_str)
            .unwrap_or("search");
        let (ak, se) = match action {
            "index" => (ActionKind::Write, SideEffect::LocalMutation),
            _ => (ActionKind::Read, SideEffect::None),
        };
        Ok(PermissionRequest::new(
            ak,
            "codeindex",
            action,
            se,
            Trust::Model,
        ))
    }

    fn run(&self, args: &Value, ctx: &ToolContext) -> Result<ToolOutput, VikramError> {
        let action = arg_str(args, "action")?;
        let db_path = ctx.workspace_root.join(".vikram").join("codeindex.db");
        match action.as_str() {
            "index" => {
                let mut idx = CodeIndex::open(&ctx.workspace_root, &db_path)
                    .map_err(|e| VikramError::ProviderRejected(e.to_string()))?;
                let stats = idx
                    .index(&[])
                    .map_err(|e| VikramError::ProviderRejected(e.to_string()))?;
                Ok(ToolOutput {
                    content: format!(
                        "indexed: {} files, {} symbols, {} unchanged, {} removed",
                        stats.indexed, stats.symbols, stats.unchanged, stats.removed
                    ),
                    ok: true,
                })
            }
            "search" => {
                let q = args.get("query").and_then(Value::as_str).unwrap_or("");
                let idx = CodeIndex::open(&ctx.workspace_root, &db_path)
                    .map_err(|e| VikramError::ProviderRejected(e.to_string()))?;
                let syms = idx
                    .search_symbols(q, 50)
                    .map_err(|e| VikramError::ProviderRejected(e.to_string()))?;
                let out: Vec<String> = syms
                    .iter()
                    .map(|s| {
                        format!(
                            "{} {} {}:{}",
                            kind_str(&s.kind),
                            s.name,
                            s.file.display(),
                            s.start_line
                        )
                    })
                    .collect();
                Ok(ToolOutput {
                    content: out.join("\n"),
                    ok: true,
                })
            }
            "repo_map" => {
                let budget = args
                    .get("budget_tokens")
                    .and_then(Value::as_u64)
                    .unwrap_or(2000) as usize;
                let idx = CodeIndex::open(&ctx.workspace_root, &db_path)
                    .map_err(|e| VikramError::ProviderRejected(e.to_string()))?;
                let m = idx
                    .repo_map(budget)
                    .map_err(|e| VikramError::ProviderRejected(e.to_string()))?;
                Ok(ToolOutput {
                    content: m,
                    ok: true,
                })
            }
            "symbols" => {
                let path = args.get("path").and_then(Value::as_str).unwrap_or("");
                let full = if path.is_empty() {
                    ctx.workspace_root.clone()
                } else {
                    ctx.workspace_root.join(path)
                };
                let idx = CodeIndex::open(&ctx.workspace_root, &db_path)
                    .map_err(|e| VikramError::ProviderRejected(e.to_string()))?;
                let syms = idx
                    .search_symbols("", 1000)
                    .map_err(|e| VikramError::ProviderRejected(e.to_string()))?;
                let filtered: Vec<String> = syms
                    .iter()
                    .filter(|s| s.file.starts_with(&full) || full == ctx.workspace_root)
                    .map(|s| format!("{} {} L{}", kind_str(&s.kind), s.name, s.start_line))
                    .collect();
                Ok(ToolOutput {
                    content: filtered.join("\n"),
                    ok: true,
                })
            }
            other => Ok(ToolOutput {
                content: format!("unknown action: {other}"),
                ok: false,
            }),
        }
    }
}

fn kind_str(k: &SymbolKind) -> &'static str {
    match k {
        SymbolKind::Function => "fn",
        SymbolKind::Method => "method",
        SymbolKind::Struct => "struct",
        SymbolKind::Class => "class",
        SymbolKind::Enum => "enum",
        SymbolKind::Trait => "trait",
        SymbolKind::Interface => "interface",
        SymbolKind::Module => "mod",
        SymbolKind::Const => "const",
        SymbolKind::Type => "type",
        SymbolKind::Impl => "impl",
        SymbolKind::Other(_) => "other",
    }
}
