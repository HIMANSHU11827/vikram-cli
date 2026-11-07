use serde_json::{Value, json};
use vikram_contracts::{ActionKind, PermissionRequest, SideEffect, Trust, VikramError};

use crate::MemoryStore;
use crate::config::database_path;

use crate::tools::*;
pub(crate) struct MemorySave;

impl Tool for MemorySave {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "memory.save".into(),
            description: "Persist a key/value pair to memory.".into(),
            schema: json!({
                "type": "object",
                "properties": {
                    "key": {"type": "string"},
                    "value": {"type": "string"},
                    "scope": {"type": "string"}
                },
                "required": ["key", "value"]
            }),
        }
    }

    fn permission(&self, args: &Value) -> Result<PermissionRequest, VikramError> {
        let key = arg_str(args, "key")?;
        Ok(PermissionRequest::new(
            ActionKind::Write,
            "memory.save",
            format!("memory:{key}"),
            SideEffect::LocalMutation,
            Trust::Model,
        ))
    }

    fn run(&self, args: &Value, _ctx: &ToolContext) -> Result<ToolOutput, VikramError> {
        let key = arg_str(args, "key")?;
        let value = arg_str(args, "value")?;
        let scope = args
            .get("scope")
            .and_then(Value::as_str)
            .unwrap_or("global");
        let store =
            MemoryStore::open(database_path()).map_err(|e| VikramError::Storage(e.to_string()))?;
        store
            .save(&key, &value, scope)
            .map_err(|e| VikramError::Storage(e.to_string()))?;
        Ok(ToolOutput {
            content: format!("saved memory `{key}` in scope `{scope}`"),
            ok: true,
        })
    }
}

pub(crate) struct MemorySearch;

impl Tool for MemorySearch {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "memory.search".into(),
            description: "Search memory entries by substring.".into(),
            schema: json!({
                "type": "object",
                "properties": {
                    "query": {"type": "string"},
                    "scope": {"type": "string"}
                },
                "required": ["query"]
            }),
        }
    }

    fn permission(&self, args: &Value) -> Result<PermissionRequest, VikramError> {
        let query = arg_str(args, "query")?;
        Ok(PermissionRequest::new(
            ActionKind::Read,
            "memory.search",
            query,
            SideEffect::None,
            Trust::Model,
        ))
    }

    fn run(&self, args: &Value, _ctx: &ToolContext) -> Result<ToolOutput, VikramError> {
        let query = arg_str(args, "query")?;
        let scope = args
            .get("scope")
            .and_then(Value::as_str)
            .unwrap_or("global");
        let store =
            MemoryStore::open(database_path()).map_err(|e| VikramError::Storage(e.to_string()))?;
        let hits = store
            .search(&query, scope)
            .map_err(|e| VikramError::Storage(e.to_string()))?;
        if hits.is_empty() {
            return Ok(ToolOutput {
                content: format!("no memory matches for `{query}` in scope `{scope}`"),
                ok: true,
            });
        }
        let body = hits
            .iter()
            .map(|(k, v)| format!("{k}: {v}"))
            .collect::<Vec<_>>()
            .join("\n");
        Ok(ToolOutput {
            content: truncated(body),
            ok: true,
        })
    }
}

pub(crate) struct MemoryList;

impl Tool for MemoryList {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "memory.list".into(),
            description: "List all memory entries in a scope.".into(),
            schema: json!({
                "type": "object",
                "properties": {
                    "scope": {"type": "string"}
                }
            }),
        }
    }

    fn permission(&self, args: &Value) -> Result<PermissionRequest, VikramError> {
        let scope = args
            .get("scope")
            .and_then(Value::as_str)
            .unwrap_or("global");
        Ok(PermissionRequest::new(
            ActionKind::Read,
            "memory.list",
            scope,
            SideEffect::None,
            Trust::Model,
        ))
    }

    fn run(&self, args: &Value, _ctx: &ToolContext) -> Result<ToolOutput, VikramError> {
        let scope = args
            .get("scope")
            .and_then(Value::as_str)
            .unwrap_or("global");
        let store =
            MemoryStore::open(database_path()).map_err(|e| VikramError::Storage(e.to_string()))?;
        let entries = store
            .list(scope)
            .map_err(|e| VikramError::Storage(e.to_string()))?;
        if entries.is_empty() {
            return Ok(ToolOutput {
                content: format!("no memory entries in scope `{scope}`"),
                ok: true,
            });
        }
        let body = entries
            .iter()
            .map(|(k, v)| format!("{k}: {v}"))
            .collect::<Vec<_>>()
            .join("\n");
        Ok(ToolOutput {
            content: truncated(body),
            ok: true,
        })
    }
}

pub(crate) struct MemoryDelete;

impl Tool for MemoryDelete {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "memory.delete".into(),
            description: "Delete a memory entry.".into(),
            schema: json!({
                "type": "object",
                "properties": {
                    "key": {"type": "string"},
                    "scope": {"type": "string"}
                },
                "required": ["key"]
            }),
        }
    }

    fn permission(&self, args: &Value) -> Result<PermissionRequest, VikramError> {
        let key = arg_str(args, "key")?;
        Ok(PermissionRequest::new(
            ActionKind::Write,
            "memory.delete",
            format!("memory:{key}"),
            SideEffect::Destructive,
            Trust::Model,
        ))
    }

    fn run(&self, args: &Value, _ctx: &ToolContext) -> Result<ToolOutput, VikramError> {
        let key = arg_str(args, "key")?;
        let scope = args
            .get("scope")
            .and_then(Value::as_str)
            .unwrap_or("global");
        let store =
            MemoryStore::open(database_path()).map_err(|e| VikramError::Storage(e.to_string()))?;
        let deleted = store
            .delete(&key, scope)
            .map_err(|e| VikramError::Storage(e.to_string()))?;
        Ok(ToolOutput {
            content: if deleted {
                format!("deleted memory `{key}` in scope `{scope}`")
            } else {
                format!("no memory entry `{key}` in scope `{scope}`")
            },
            ok: deleted,
        })
    }
}

pub(crate) struct MemoryConsolidate;

impl Tool for MemoryConsolidate {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "memory.consolidate".into(),
            description: "Run the memory consolidation lifecycle: capture staged observations, deduplicate, detect contradictions (same key, differing value), supersede losers with provenance lineage, and expire stale low-score entries. Observations are key/value pairs with source, confidence, importance and timestamps.".into(),
            schema: json!({
                "type": "object",
                "properties": {
                    "action": {"type": "string", "enum": ["capture", "run", "list", "score"], "description": "capture stages one observation; run consolidates; list shows observations for a scope; score previews ranking. Default: run"},
                    "key": {"type": "string", "description": "observation key (capture only)"},
                    "value": {"type": "string", "description": "observation value (capture only)"},
                    "scope": {"type": "string", "description": "memory scope, default global"},
                    "source": {"type": "string", "description": "provenance source (capture only), default tool"},
                    "confidence": {"type": "number", "description": "0.0..1.0 provenance confidence (capture only), default 0.5"},
                    "importance": {"type": "number", "description": "0.0..1.0 importance weight (capture only), default 0.5"},
                    "status": {"type": "string", "enum": ["staged", "active", "superseded", "expired"], "description": "filter for list/score"}
                }
            }),
        }
    }

    fn permission(&self, args: &Value) -> Result<PermissionRequest, VikramError> {
        let action = args.get("action").and_then(Value::as_str).unwrap_or("run");
        let side_effect = if action == "capture" || action == "run" {
            SideEffect::LocalMutation
        } else {
            SideEffect::None
        };
        Ok(PermissionRequest::new(
            ActionKind::Write,
            "memory.consolidate",
            format!("memory:consolidate:{action}"),
            side_effect,
            Trust::Model,
        ))
    }

    fn run(&self, args: &Value, _ctx: &ToolContext) -> Result<ToolOutput, VikramError> {
        use crate::consolidation::{ConsolidationPolicy, ConsolidationStore, score};
        let action = args.get("action").and_then(Value::as_str).unwrap_or("run");
        let scope = args
            .get("scope")
            .and_then(Value::as_str)
            .unwrap_or("global");
        let store = ConsolidationStore::open(crate::config::database_path())
            .map_err(|e| VikramError::Storage(e.to_string()))?;
        let policy = ConsolidationPolicy::default();

        match action {
            "capture" => {
                let key = arg_str(args, "key")?;
                let value = arg_str(args, "value")?;
                let source = args.get("source").and_then(Value::as_str).unwrap_or("tool");
                let confidence = args
                    .get("confidence")
                    .and_then(Value::as_f64)
                    .unwrap_or(0.5);
                let importance = args
                    .get("importance")
                    .and_then(Value::as_f64)
                    .unwrap_or(0.5);
                let obs = store
                    .capture(&key, &value, scope, source, confidence, importance)
                    .map_err(|e| VikramError::Storage(e.to_string()))?;
                Ok(ToolOutput {
                    content: format!(
                        "staged observation `{}` scope=`{}` source=`{}` confidence={:.2} importance={:.2} id=`{}`",
                        key, scope, source, obs.confidence, obs.importance, obs.id
                    ),
                    ok: true,
                })
            }
            "list" => {
                let observations = store
                    .load_scope(scope)
                    .map_err(|e| VikramError::Storage(e.to_string()))?;
                let status_filter = args.get("status").and_then(Value::as_str);
                let filtered: Vec<_> = observations
                    .iter()
                    .filter(|o| status_filter.is_none_or(|s| o.status.as_str() == s))
                    .collect();
                if filtered.is_empty() {
                    return Ok(ToolOutput {
                        content: format!("no observations in scope `{scope}`"),
                        ok: true,
                    });
                }
                let body = filtered
                    .iter()
                    .map(|o| {
                        format!(
                            "[{}] {}: {} (source={}, confidence={:.2}, importance={:.2}, status={}, created={}, updated={}, accesses={})",
                            o.scope, o.key, o.value, o.source, o.confidence, o.importance,
                            o.status.as_str(), o.created_ms, o.updated_ms, o.access_count
                        )
                    })
                    .collect::<Vec<_>>()
                    .join("\n");
                Ok(ToolOutput {
                    content: truncated(body),
                    ok: true,
                })
            }
            "score" => {
                let observations = store
                    .load_scope(scope)
                    .map_err(|e| VikramError::Storage(e.to_string()))?;
                let now = crate::types::now_ms();
                let mut ranked: Vec<_> = observations.iter().collect();
                ranked.sort_by(|a, b| {
                    score(b, now, &policy)
                        .partial_cmp(&score(a, now, &policy))
                        .unwrap_or(std::cmp::Ordering::Equal)
                });
                if ranked.is_empty() {
                    return Ok(ToolOutput {
                        content: format!("no observations in scope `{scope}`"),
                        ok: true,
                    });
                }
                let body = ranked
                    .iter()
                    .map(|o| {
                        format!(
                            "{:.4} {}: {} [{}]",
                            score(o, now, &policy),
                            o.key,
                            o.value,
                            o.status.as_str()
                        )
                    })
                    .collect::<Vec<_>>()
                    .join("\n");
                Ok(ToolOutput {
                    content: truncated(body),
                    ok: true,
                })
            }
            // "run" and any unknown action fall through to consolidation.
            _ => {
                let (report, observations) = store
                    .run_consolidation(Some(scope), &policy)
                    .map_err(|e| VikramError::Storage(e.to_string()))?;
                let mut body = format!(
                    "captured={} deduplicated={} contradictions={} superseded={} expired={} active_after={} staged_after={}",
                    report.captured,
                    report.deduplicated,
                    report.contradictions_detected,
                    report.superseded,
                    report.expired,
                    report.active_after,
                    report.staged_after
                );
                if !observations.is_empty() {
                    body.push_str("\n--- observations (score desc) ---");
                    for o in &observations {
                        body.push_str(&format!(
                            "\n{:.4} [{}] {}: {} (source={}, confidence={:.2}, status={})",
                            score(o, crate::types::now_ms(), &policy),
                            o.scope,
                            o.key,
                            o.value,
                            o.source,
                            o.confidence,
                            o.status.as_str()
                        ));
                        if let Some(by) = &o.superseded_by {
                            body.push_str(&format!(" superseded_by={}", by));
                        }
                    }
                }
                Ok(ToolOutput {
                    content: truncated(body),
                    ok: true,
                })
            }
        }
    }
}

pub(crate) struct MemoryNote;

impl Tool for MemoryNote {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "memory.note".into(),
            description: "Append a free-form note to MEMORY.md or USER.md (Hermes-style).".into(),
            schema: json!({
                "type": "object",
                "properties": {
                    "target": {"type": "string", "enum": ["memory", "user"]},
                    "note": {"type": "string"}
                },
                "required": ["note"]
            }),
        }
    }

    fn permission(&self, args: &Value) -> Result<PermissionRequest, VikramError> {
        let note = arg_str(args, "note")?;
        Ok(PermissionRequest::new(
            ActionKind::Write,
            "memory.note",
            note,
            SideEffect::LocalMutation,
            Trust::Model,
        ))
    }

    fn run(&self, args: &Value, _ctx: &ToolContext) -> Result<ToolOutput, VikramError> {
        let note = arg_str(args, "note")?;
        let target = args
            .get("target")
            .and_then(Value::as_str)
            .unwrap_or("memory");
        let home = crate::config::vikram_home();
        let mut f = if target == "user" {
            crate::memory_files::user_file(&home)
        } else {
            crate::memory_files::memory_file(&home)
        };
        f.add(&note).map_err(VikramError::Storage)?;
        Ok(ToolOutput {
            content: format!("noted in {target}"),
            ok: true,
        })
    }
}
