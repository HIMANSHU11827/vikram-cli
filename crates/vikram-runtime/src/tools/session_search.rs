use serde_json::{Value, json};
use vikram_contracts::{
    ActionKind, BotId, PermissionRequest, ProjectId, SideEffect, Trust, VikramError,
};
use vikram_store::Store;

use crate::config::database_path;
use crate::tools::*;

pub(crate) struct SessionSearch;

impl Tool for SessionSearch {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "session.search".into(),
            description: "Search past session messages via FTS5.".into(),
            schema: json!({
                "type": "object",
                "properties": {
                    "query": {"type": "string"},
                    "limit": {"type": "integer"},
                    "agent": {"type": "string"},
                    "project": {"type": "string"},
                    "since": {"type": "integer", "description": "Only sessions updated at or after this epoch-ms timestamp."}
                },
                "required": ["query"]
            }),
        }
    }

    fn permission(&self, args: &Value) -> Result<PermissionRequest, VikramError> {
        let q = arg_str(args, "query")?;
        Ok(PermissionRequest::new(
            ActionKind::Read,
            "session.search",
            q,
            SideEffect::None,
            Trust::Model,
        ))
    }

    fn run(&self, args: &Value, _ctx: &ToolContext) -> Result<ToolOutput, VikramError> {
        let q = arg_str(args, "query")?;
        let limit = args.get("limit").and_then(Value::as_u64).unwrap_or(10) as usize;
        let agent = args.get("agent").and_then(Value::as_str).map(BotId::new);
        let project = args
            .get("project")
            .and_then(Value::as_str)
            .map(ProjectId::new);
        let since = args.get("since").and_then(Value::as_u64).unwrap_or(0);
        let store =
            Store::open(&database_path()).map_err(|e| VikramError::Storage(e.to_string()))?;
        let hits = store
            .search_session_text_filtered(&q, agent.as_ref(), project.as_ref(), since, limit)
            .map_err(|e| VikramError::Storage(e.to_string()))?;
        if hits.is_empty() {
            // Fallback to LIKE-based search_sessions
            let rows = store
                .search_sessions(&q, true)
                .map_err(|e| VikramError::Storage(e.to_string()))?;
            let out: Vec<String> = rows
                .iter()
                .map(|r| format!("{}: {}", r.id, r.title))
                .collect();
            return Ok(ToolOutput {
                content: out.join("\n"),
                ok: true,
            });
        }
        let out: Vec<String> = hits
            .iter()
            .map(|(s, snip)| format!("{}: {}", s, snip))
            .collect();
        Ok(ToolOutput {
            content: out.join("\n"),
            ok: true,
        })
    }
}
