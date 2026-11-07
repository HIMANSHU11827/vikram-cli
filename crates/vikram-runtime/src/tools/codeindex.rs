use serde_json::{Value, json};
use std::path::Path;
use vikram_codeindex::{CodeIndex, SymbolKind};
use vikram_contracts::{ActionKind, PermissionRequest, SideEffect, Trust, VikramError};

use crate::tools::*;

/// Open the index if it already exists; returns None (never creates) when absent so
/// callers can degrade to grep instead of an empty answer.
fn open_existing(root: &Path) -> Option<CodeIndex> {
    let db_path = root.join(".vikram").join("codeindex.db");
    if !db_path.is_file() {
        return None;
    }
    CodeIndex::open(root, &db_path).ok()
}

/// Plain-text fallback when no index exists: bounded line search over source files,
/// same cap as `search.text` so it cannot hang a turn.
fn grep_fallback(root: &Path, query: &str) -> String {
    if query.trim().is_empty() {
        return "[codeindex] no index at `.vikram/codeindex.db`; run codeindex index first (grep fallback has no query)".to_string();
    }
    let mut hits: Vec<String> = Vec::new();
    grep_walk(root, query, &mut hits, 0);
    if hits.is_empty() {
        format!(
            "[codeindex] no index at `.vikram/codeindex.db`; grep fallback found no matches for `{query}`"
        )
    } else {
        format!(
            "[codeindex] no index at `.vikram/codeindex.db`; grep fallback:\n{}",
            hits.join("\n")
        )
    }
}

fn grep_walk(dir: &Path, query: &str, hits: &mut Vec<String>, depth: usize) {
    if depth > 12 || hits.len() >= 100 {
        return;
    }
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        let name = entry.file_name().to_string_lossy().into_owned();
        if name == ".git" || name == "target" || name == "node_modules" || name == ".vikram" {
            continue;
        }
        if path.is_dir() {
            grep_walk(&path, query, hits, depth + 1);
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
            if hits.len() >= 100 {
                return;
            }
            if line.contains(query) {
                hits.push(format!("{}:{}: {}", path.display(), idx + 1, line.trim()));
            }
        }
    }
}

/// Ranked context used by turn-loop injection: index results first, grep fallback
/// when the index is absent. Public so `turn.rs` can call it directly.
pub fn codeindex_context(root: &Path, query: &str) -> String {
    match open_existing(root) {
        Some(idx) => match idx.ranked_context(query, 20) {
            Ok(text) if !text.trim().is_empty() => text,
            _ => grep_fallback(root, query),
        },
        None => grep_fallback(root, query),
    }
}

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
                    "action": {"type": "string", "enum": ["index", "search", "repo_map", "symbols", "definitions", "references", "imports"]},
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
                if q.trim().is_empty() {
                    // No query: a ranked context dump is more useful than an empty list.
                    let ctx_text = codeindex_context(&ctx.workspace_root, "");
                    return Ok(ToolOutput {
                        content: truncated(ctx_text),
                        ok: true,
                    });
                }
                match open_existing(&ctx.workspace_root) {
                    Some(idx) => {
                        let syms = idx
                            .search_symbols(q, 50)
                            .map_err(|e| VikramError::ProviderRejected(e.to_string()))?;
                        if syms.is_empty() {
                            let fb = grep_fallback(&ctx.workspace_root, q);
                            return Ok(ToolOutput {
                                content: truncated(fb),
                                ok: true,
                            });
                        }
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
                            content: truncated(out.join("\n")),
                            ok: true,
                        })
                    }
                    None => Ok(ToolOutput {
                        content: truncated(grep_fallback(&ctx.workspace_root, q)),
                        ok: true,
                    }),
                }
            }
            "definitions" | "references" | "imports" => {
                let q = args.get("query").and_then(Value::as_str).unwrap_or("");
                if q.trim().is_empty() {
                    return Ok(ToolOutput {
                        content: "codeindex definitions/references/imports need a `query`".into(),
                        ok: false,
                    });
                }
                match open_existing(&ctx.workspace_root) {
                    Some(idx) => {
                        let content = match action.as_str() {
                            "definitions" => {
                                let defs = idx
                                    .definitions_of(q)
                                    .map_err(|e| VikramError::ProviderRejected(e.to_string()))?;
                                let mut lines: Vec<String> = defs
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
                                // Call-graph approximation: files that reference the name.
                                for f in idx.references_of(q).unwrap_or_default() {
                                    lines.push(format!("called/referenced from {f}"));
                                }
                                if lines.is_empty() {
                                    format!("no indexed definition for `{q}`")
                                } else {
                                    lines.join("\n")
                                }
                            }
                            "references" => {
                                let refs = idx
                                    .references_of(q)
                                    .map_err(|e| VikramError::ProviderRejected(e.to_string()))?;
                                if refs.is_empty() {
                                    format!("no indexed references to `{q}`")
                                } else {
                                    refs.join("\n")
                                }
                            }
                            _ => {
                                // imports: symbol definitions in other files + refs.
                                let defs = idx.definitions_of(q).unwrap_or_default();
                                let mut lines: Vec<String> = defs
                                    .iter()
                                    .map(|s| {
                                        format!(
                                            "imported from {} ({} {})",
                                            s.file.display(),
                                            kind_str(&s.kind),
                                            s.name
                                        )
                                    })
                                    .collect();
                                for f in idx.references_of(q).unwrap_or_default() {
                                    lines.push(format!("used in {f}"));
                                }
                                if lines.is_empty() {
                                    format!("no indexed imports for `{q}`")
                                } else {
                                    lines.join("\n")
                                }
                            }
                        };
                        Ok(ToolOutput {
                            content: truncated(content),
                            ok: true,
                        })
                    }
                    None => Ok(ToolOutput {
                        content: truncated(grep_fallback(&ctx.workspace_root, q)),
                        ok: true,
                    }),
                }
            }
            "repo_map" => {
                let budget = args
                    .get("budget_tokens")
                    .and_then(Value::as_u64)
                    .unwrap_or(2000) as usize;
                match open_existing(&ctx.workspace_root) {
                    Some(idx) => {
                        let m = idx
                            .repo_map(budget)
                            .map_err(|e| VikramError::ProviderRejected(e.to_string()))?;
                        Ok(ToolOutput {
                            content: if m.trim().is_empty() {
                                "[codeindex] index is empty; run codeindex index first".to_owned()
                            } else {
                                m
                            },
                            ok: true,
                        })
                    }
                    None => Ok(ToolOutput {
                        content: "[codeindex] no index at `.vikram/codeindex.db`; run codeindex index first (repo_map unavailable)"
                            .to_owned(),
                        ok: true,
                    }),
                }
            }
            "symbols" => {
                let path = args.get("path").and_then(Value::as_str).unwrap_or("");
                let full = if path.is_empty() {
                    ctx.workspace_root.clone()
                } else {
                    ctx.workspace_root.join(path)
                };
                match open_existing(&ctx.workspace_root) {
                    Some(idx) => {
                        let syms = idx
                            .search_symbols("", 1000)
                            .map_err(|e| VikramError::ProviderRejected(e.to_string()))?;
                        // Symbol.file is stored relative to the index root; compare the
                        // tail so a relative `path` arg still filters.
                        let rel = full
                            .strip_prefix(&ctx.workspace_root)
                            .unwrap_or(&full)
                            .to_string_lossy()
                            .into_owned();
                        let filtered: Vec<String> = syms
                            .iter()
                            .filter(|s| {
                                rel.is_empty()
                                    || s.file.starts_with(&full)
                                    || s.file.to_string_lossy().starts_with(&rel)
                            })
                            .map(|s| format!("{} {} L{}", kind_str(&s.kind), s.name, s.start_line))
                            .collect();
                        Ok(ToolOutput {
                            content: truncated(filtered.join("\n")),
                            ok: true,
                        })
                    }
                    None => Ok(ToolOutput {
                        content: "[codeindex] no index at `.vikram/codeindex.db`; run codeindex index first (symbols unavailable)"
                            .to_owned(),
                        ok: true,
                    }),
                }
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
