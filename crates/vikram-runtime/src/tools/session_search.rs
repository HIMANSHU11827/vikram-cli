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

    fn run(&self, args: &Value, ctx: &ToolContext) -> Result<ToolOutput, VikramError> {
        let q = arg_str(args, "query")?;
        // An unchecked `as i64` would wrap a huge limit negative, and SQLite reads LIMIT -1 as
        // unlimited — clamp before it reaches the store.
        let limit = args
            .get("limit")
            .and_then(Value::as_u64)
            .unwrap_or(10)
            .min(100) as usize;
        let project = args
            .get("project")
            .and_then(Value::as_str)
            .map(ProjectId::new);
        let since = args.get("since").and_then(Value::as_u64).unwrap_or(0);
        let store =
            Store::open(&database_path()).map_err(|e| VikramError::Storage(e.to_string()))?;
        // A bot or delegated agent reads only its own transcripts: the caller's identity
        // overrides the model-supplied `agent` filter instead of being widened by it. Only the
        // main agent — no bot, no agent — may name any agent it likes.
        let agent = match caller_bot(&store, ctx) {
            Some(id) => Some(id),
            None => args.get("agent").and_then(Value::as_str).map(BotId::new),
        };
        let hits = store
            .search_session_text_filtered(&q, agent.as_ref(), project.as_ref(), since, limit)
            .map_err(|e| VikramError::Storage(e.to_string()))?;
        if hits.is_empty() {
            // Fallback to LIKE-based search_sessions_page. It searches titles and journal
            // payloads, not message bodies, and it knows nothing about the filters — so they
            // are applied here. Pages are walked until `limit` rows survive the filters or the
            // matches run out, honouring the same bound the FTS path above does; an empty
            // answer says so instead of returning "" with ok:true.
            let page_size = limit.max(1);
            let mut out: Vec<String> = Vec::new();
            let mut offset = 0usize;
            loop {
                let page = store
                    .search_sessions_page(&q, true, Some(page_size), offset)
                    .map_err(|e| VikramError::Storage(e.to_string()))?;
                let exhausted = page.len() < page_size;
                offset += page.len();
                out.extend(
                    page.iter()
                        .filter(|r| agent.as_ref().is_none_or(|a| r.bot.as_ref() == Some(a)))
                        .filter(|r| {
                            project
                                .as_ref()
                                .is_none_or(|p| r.project_id.as_ref() == Some(p))
                        })
                        .filter(|r| r.updated_ms >= since)
                        .map(|r| format!("{}: {}", r.id, r.title)),
                );
                out.truncate(limit);
                if exhausted || out.len() >= limit {
                    break;
                }
            }
            if out.is_empty() {
                return Ok(ToolOutput {
                    content: format!("no sessions match `{q}`"),
                    ok: false,
                });
            }
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

/// The bot this call runs as, when it runs as one. `ctx.bot` already carries the session's
/// BotId; `ctx.agent` carries the profile *name*, resolved through the roster so the filter
/// lands on `sessions.bot_id`. An unresolvable name still yields a filter — one that matches
/// nothing — rather than letting the call read every session.
fn caller_bot(store: &Store, ctx: &ToolContext) -> Option<BotId> {
    if let Some(bot) = ctx.bot.as_deref().filter(|bot| !bot.is_empty()) {
        return Some(BotId::new(bot));
    }
    let agent = ctx.agent.as_deref().filter(|agent| !agent.is_empty())?;
    let id = store
        .list_bots()
        .unwrap_or_default()
        .into_iter()
        .find(|row| row.name == agent)
        .map(|row| row.id)
        .unwrap_or_else(|| BotId::new(agent));
    Some(id)
}

#[cfg(test)]
mod tests {
    use super::*;
    use vikram_contracts::SessionId;
    use vikram_store::BotRow;

    fn isolated_ctx(dir: &std::path::Path) -> (std::sync::MutexGuard<'static, ()>, ToolContext) {
        let guard = crate::config::lock_security_env();
        unsafe { std::env::set_var("VIKRAM_HOME", dir) };
        (guard, ToolContext::default())
    }

    fn seed_session(store: &Store, bot: Option<&BotId>, text: &str) -> SessionId {
        let row = store
            .create_session("seeded", std::path::Path::new("/tmp"), None, 1)
            .unwrap();
        store.set_session_bot(&row.id, bot, 1).unwrap();
        store.index_session_text(&row.id, text).unwrap();
        row.id
    }

    #[test]
    fn a_bot_only_sees_its_own_sessions() {
        let home = tempfile::tempdir().unwrap();
        let (_guard, mut ctx) = isolated_ctx(home.path());
        let db = crate::config::database_path();
        let store = Store::open(&db).unwrap();
        let bot_a = BotId::new("bot-a");
        let bot_b = BotId::new("bot-b");
        let mine = seed_session(&store, Some(&bot_a), "sharedneedle from a");
        let theirs = seed_session(&store, Some(&bot_b), "sharedneedle from b");
        seed_session(&store, None, "sharedneedle from main");

        ctx.bot = Some("bot-a".to_owned());
        let out = SessionSearch
            .run(&json!({"query": "sharedneedle"}), &ctx)
            .unwrap();
        assert!(out.ok, "{}", out.content);
        assert!(out.content.contains(mine.as_str()), "{}", out.content);
        assert!(!out.content.contains(theirs.as_str()), "{}", out.content);

        // A model-supplied `agent` filter cannot widen a bot past its own transcripts.
        let out = SessionSearch
            .run(&json!({"query": "sharedneedle", "agent": "bot-b"}), &ctx)
            .unwrap();
        assert!(out.content.contains(mine.as_str()), "{}", out.content);
        assert!(!out.content.contains(theirs.as_str()), "{}", out.content);
    }

    #[test]
    fn an_agent_name_resolves_to_its_bot_id() {
        let home = tempfile::tempdir().unwrap();
        let (_guard, mut ctx) = isolated_ctx(home.path());
        let db = crate::config::database_path();
        let store = Store::open(&db).unwrap();
        store
            .save_bot(&BotRow {
                id: BotId::new("bot-123"),
                name: "Rudra".to_owned(),
                role: "tag".to_owned(),
                description: String::new(),
                icon: None,
                created_ms: 1,
                soul: String::new(),
            })
            .unwrap();
        let mine = seed_session(&store, Some(&BotId::new("bot-123")), "agentneedle mine");
        let other = seed_session(&store, Some(&BotId::new("bot-999")), "agentneedle other");

        ctx.agent = Some("Rudra".to_owned());
        let out = SessionSearch
            .run(&json!({"query": "agentneedle"}), &ctx)
            .unwrap();
        assert!(out.ok, "{}", out.content);
        assert!(out.content.contains(mine.as_str()), "{}", out.content);
        assert!(!out.content.contains(other.as_str()), "{}", out.content);
    }

    #[test]
    fn the_main_agent_may_still_filter_by_agent_and_limit_is_clamped() {
        let home = tempfile::tempdir().unwrap();
        let (_guard, ctx) = isolated_ctx(home.path());
        let db = crate::config::database_path();
        let store = Store::open(&db).unwrap();
        let bot = BotId::new("bot-a");
        let mine = seed_session(&store, Some(&bot), "limitneedle");
        let other = seed_session(&store, None, "limitneedle");

        // No bot, no agent: the caller is the main agent and its `agent` argument stands.
        let out = SessionSearch
            .run(&json!({"query": "limitneedle", "agent": "bot-a"}), &ctx)
            .unwrap();
        assert!(out.content.contains(mine.as_str()), "{}", out.content);
        assert!(!out.content.contains(other.as_str()), "{}", out.content);

        // A limit far past the cap is clamped, not wrapped to SQLite's unlimited -1.
        for _ in 0..150 {
            store
                .index_session_text(&mine, "limitneedle again")
                .unwrap();
        }
        let out = SessionSearch
            .run(
                &json!({"query": "limitneedle", "agent": "bot-a", "limit": u64::MAX}),
                &ctx,
            )
            .unwrap();
        let lines = out.content.lines().count();
        assert_eq!(lines, 100, "{}", out.content);
    }
}
