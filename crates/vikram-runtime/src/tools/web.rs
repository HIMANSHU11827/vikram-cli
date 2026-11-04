use serde_json::{Value, json};
use vikram_contracts::{ActionKind, PermissionRequest, SideEffect, Trust, VikramError};

use crate::config::Config;

use crate::tools::*;
pub(crate) struct WebSearch;

impl Tool for WebSearch {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "web.search".into(),
            description: "Search the web (Bing HTML, or Brave when a key is configured) and return result snippets.".into(),
            schema: json!({
                "type": "object",
                "properties": {
                    "query": {"type": "string"},
                    "limit": {"type": "integer"}
                },
                "required": ["query"]
            }),
        }
    }

    fn permission(&self, args: &Value) -> Result<PermissionRequest, VikramError> {
        let query = arg_str(args, "query")?;
        Ok(PermissionRequest::new(
            ActionKind::Network,
            "web.search",
            query,
            SideEffect::None,
            Trust::Model,
        ))
    }

    fn run(&self, args: &Value, _ctx: &ToolContext) -> Result<ToolOutput, VikramError> {
        let query = arg_str(args, "query")?;
        let limit = args
            .get("limit")
            .and_then(Value::as_u64)
            .map(|n| n as usize)
            .filter(|&n| n > 0)
            .unwrap_or(5);

        let config = match Config::load() {
            Ok(config) => config,
            Err(err) => {
                return Ok(ToolOutput {
                    content: format!("web search is not configured: {err}"),
                    ok: false,
                });
            }
        };
        let api_key = config.search_api_key_with_source().map(|(value, _)| value);
        let client = match crate::search::client() {
            Ok(client) => client,
            Err(err) => {
                return Ok(ToolOutput {
                    content: format!("web search failed: {err}"),
                    ok: false,
                });
            }
        };

        let outcome =
            crate::search::search(&config.search, api_key.as_deref(), &client, &query, limit);
        Ok(search_tool_output(outcome))
    }
}

/// Map a backend outcome onto a tool result. A backend that answered with nothing usable is a
/// failure: an empty success would tell the model a search happened when it did not.
pub(crate) fn search_tool_output(
    outcome: Result<Vec<crate::search::SearchResult>, crate::search::SearchError>,
) -> ToolOutput {
    match outcome {
        Ok(results) if !results.is_empty() => ToolOutput {
            content: truncated(crate::search::render(&results)),
            ok: true,
        },
        Ok(_) => ToolOutput {
            content: "web search failed: the backend answered with zero results".into(),
            ok: false,
        },
        Err(err) => ToolOutput {
            content: format!("web search failed: {err}"),
            ok: false,
        },
    }
}
