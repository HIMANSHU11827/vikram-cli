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
        f.add(&note).map_err(|e| VikramError::Storage(e))?;
        Ok(ToolOutput {
            content: format!("noted in {target}"),
            ok: true,
        })
    }
}
