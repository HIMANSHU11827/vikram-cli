use serde_json::{Value, json};
use std::collections::HashMap;
use std::sync::{LazyLock, Mutex};
use std::time::{Duration, Instant};
use vikram_contracts::{ActionKind, PermissionRequest, SideEffect, Trust, VikramError};

use crate::config::Config;

use crate::tools::*;

/// The same query twice in one turn must not hit the network twice. Results are cached for a short
/// window: long enough to collapse a model that asks the same thing repeatedly, short enough that a
/// later turn in a long session still sees fresh news.
const SEARCH_CACHE_TTL: Duration = Duration::from_secs(600);
/// Bounded so a session that searches all day cannot grow the process indefinitely.
const SEARCH_CACHE_CAP: usize = 128;

struct CachedSearch {
    at: Instant,
    content: String,
}

static SEARCH_CACHE: LazyLock<Mutex<HashMap<String, CachedSearch>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

fn cache_key(backend: &str, query: &str, limit: usize) -> String {
    format!(
        "{backend}\u{1f}{limit}\u{1f}{}",
        query.trim().to_lowercase()
    )
}

fn cache_get(key: &str) -> Option<String> {
    let mut cache = SEARCH_CACHE.lock().unwrap_or_else(|e| e.into_inner());
    match cache.get(key) {
        Some(entry) if entry.at.elapsed() < SEARCH_CACHE_TTL => Some(entry.content.clone()),
        Some(_) => {
            cache.remove(key);
            None
        }
        None => None,
    }
}

fn cache_put(key: String, content: String) {
    let mut cache = SEARCH_CACHE.lock().unwrap_or_else(|e| e.into_inner());
    if cache.len() >= SEARCH_CACHE_CAP {
        let now = Instant::now();
        cache.retain(|_, entry| now.duration_since(entry.at) < SEARCH_CACHE_TTL);
        if cache.len() >= SEARCH_CACHE_CAP {
            // Still full of live entries: drop the oldest one.
            if let Some(oldest) = cache
                .iter()
                .min_by_key(|(_, entry)| entry.at)
                .map(|(key, _)| key.clone())
            {
                cache.remove(&oldest);
            }
        }
    }
    cache.insert(
        key,
        CachedSearch {
            at: Instant::now(),
            content,
        },
    );
}

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
        let backend = format!("{:?}", config.search.backend).to_lowercase();
        let key = cache_key(&backend, &query, limit);
        if let Some(content) = cache_get(&key) {
            return Ok(ToolOutput { content, ok: true });
        }
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
        let output = search_tool_output(outcome);
        if output.ok {
            cache_put(key, output.content.clone());
        }
        Ok(output)
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
            content: "web search failed: the backend answered with zero results. Do not repeat this query: try different words, or fetch a real page instead (browser.fetch or browser.open on a news site's URL, then browser.text, browser.links), or call http against a site's own endpoint.".into(),
            ok: false,
        },
        Err(err) => ToolOutput {
            content: format!("web search failed: {err}"),
            ok: false,
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_same_query_is_served_from_the_cache_once_warmed() {
        let key = cache_key("bing", "  Rust 2024 Edition ", 5);
        assert_eq!(
            key,
            cache_key("bing", "rust 2024 edition", 5),
            "case and surrounding space must not create a second entry"
        );
        assert_ne!(key, cache_key("brave", "rust 2024 edition", 5));
        assert_ne!(key, cache_key("bing", "rust 2024 edition", 3));

        assert!(cache_get(&key).is_none());
        cache_put(key.clone(), "cached body".into());
        assert_eq!(cache_get(&key).as_deref(), Some("cached body"));
    }
}
