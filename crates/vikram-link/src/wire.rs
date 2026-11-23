//! The event stream on the wire.
//!
//! `RuntimeEvent` is an in-process enum with no serialization of its own — the TUI matches on it
//! directly. A linked device cannot, so each event is projected into JSON here, in one place: the
//! payload types that already serialize are carried whole, and the three snapshots that do not
//! (session, bot, approval) are summarised to the fields a remote view needs. The projection is
//! additive: a new field never breaks a client, and a client that needs the full row asks
//! `GET /sessions`.

use serde::Serialize;
use serde_json::{Value, json};
use vikram_contracts::SessionId;
use vikram_runtime::types::{ApprovalPrompt, BotSnapshot, RuntimeEvent, SessionSnapshot, now_ms};

fn value<T: Serialize>(input: &T) -> Value {
    serde_json::to_value(input).unwrap_or(Value::Null)
}

/// The session an event belongs to, when it belongs to one. This is what the event filter keys on:
/// a token that does not reach the session never sees the event.
pub fn event_session(event: &RuntimeEvent) -> Option<&SessionId> {
    match event {
        RuntimeEvent::SessionCreated(snapshot) | RuntimeEvent::SessionUpdated(snapshot) => {
            Some(&snapshot.id)
        }
        RuntimeEvent::SessionClosed(session)
        | RuntimeEvent::UserMessage { session, .. }
        | RuntimeEvent::StreamStarted { session, .. }
        | RuntimeEvent::StreamDelta { session, .. }
        | RuntimeEvent::StreamThinkingDelta { session, .. }
        | RuntimeEvent::MessageCommitted { session, .. }
        | RuntimeEvent::ToolStarted { session, .. }
        | RuntimeEvent::ToolFinished { session, .. }
        | RuntimeEvent::TurnFinished { session, .. }
        | RuntimeEvent::ModeChanged { session, .. }
        | RuntimeEvent::UserQuestion { session, .. }
        | RuntimeEvent::ContextCompacted { session, .. }
        | RuntimeEvent::ContextPruned { session, .. }
        | RuntimeEvent::ContextMetered { session, .. }
        | RuntimeEvent::PlanUpdated { session, .. } => Some(session),
        RuntimeEvent::ApprovalRequested(prompt) => Some(&prompt.session),
        RuntimeEvent::Activity(activity) => Some(&activity.session),
        _ => None,
    }
}

fn session_row(snapshot: &SessionSnapshot) -> Value {
    json!({
        "session": snapshot.id.to_string(),
        "title": snapshot.title,
        "cwd": snapshot.cwd.display().to_string(),
        "model": snapshot.model,
        "bot": snapshot.bot.as_ref().map(|bot| bot.to_string()),
        "turn": value(&snapshot.turn),
        "status": snapshot.status,
        "messages": snapshot.messages.len(),
        "queued": snapshot.queued,
        "created_ms": snapshot.created_ms,
        "updated_ms": snapshot.updated_ms,
    })
}

fn bot_row(bot: &BotSnapshot) -> Value {
    json!({
        "bot": bot.profile.id.to_string(),
        "name": bot.profile.name,
        "role": bot.profile.role,
        "state": value(&bot.state),
        "session": bot.session.as_ref().map(|session| session.to_string()),
        "task": bot.current_task,
        "progress": bot.progress,
        "unread": bot.unread,
    })
}

/// One pending approval, as a client sees it.
pub fn approval(prompt: &ApprovalPrompt) -> Value {
    json!({
        "approval": prompt.approval.to_string(),
        "session": prompt.session.to_string(),
        "verb": prompt.verb,
        "resource": prompt.resource,
        "reason": prompt.reason,
        "amendment": prompt.amendment,
    })
}

fn notice_level(level: &vikram_runtime::types::NoticeLevel) -> &'static str {
    match level {
        vikram_runtime::types::NoticeLevel::Info => "info",
        vikram_runtime::types::NoticeLevel::Warn => "warn",
        vikram_runtime::types::NoticeLevel::Error => "error",
    }
}

/// One event, as a client receives it. `ts_ms` is stamped here so a client can order two events
/// that arrived in the same read without trusting its own clock.
pub fn event(event: &RuntimeEvent) -> Value {
    let body = match event {
        RuntimeEvent::Status(status) => json!({"type": "status", "status": value(status)}),
        RuntimeEvent::Metrics(metrics) => json!({"type": "metrics", "metrics": value(metrics)}),
        RuntimeEvent::SessionCreated(snapshot) => {
            let mut row = session_row(snapshot);
            row["type"] = json!("session_created");
            row
        }
        RuntimeEvent::SessionUpdated(snapshot) => {
            let mut row = session_row(snapshot);
            row["type"] = json!("session_updated");
            row
        }
        RuntimeEvent::SessionClosed(session) => {
            json!({"type": "session_closed", "session": session.to_string()})
        }
        RuntimeEvent::UserMessage {
            session,
            turn,
            text,
        } => json!({
            "type": "user_message",
            "session": session.to_string(),
            "turn": turn.to_string(),
            "text": text,
        }),
        RuntimeEvent::StreamStarted { session, turn } => json!({
            "type": "stream_started",
            "session": session.to_string(),
            "turn": turn.to_string(),
        }),
        RuntimeEvent::StreamDelta {
            session,
            turn,
            text,
        } => json!({
            "type": "stream_delta",
            "session": session.to_string(),
            "turn": turn.to_string(),
            "text": text,
        }),
        RuntimeEvent::StreamThinkingDelta {
            session,
            turn,
            text,
        } => json!({
            "type": "stream_thinking_delta",
            "session": session.to_string(),
            "turn": turn.to_string(),
            "text": text,
        }),
        RuntimeEvent::MessageCommitted { session, message } => json!({
            "type": "message_committed",
            "session": session.to_string(),
            "message": value(message),
        }),
        RuntimeEvent::ToolStarted {
            session,
            call,
            name,
            summary,
            resource,
            arguments,
            cwd,
        } => json!({
            "type": "tool_started",
            "session": session.to_string(),
            "call": call.to_string(),
            "name": name,
            "summary": summary,
            "resource": resource,
            "arguments": arguments,
            "cwd": cwd,
        }),
        RuntimeEvent::ToolFinished {
            session,
            call,
            name,
            ok,
            summary,
            content,
            resource,
            arguments,
            cwd,
            duration_ms,
        } => json!({
            "type": "tool_finished",
            "session": session.to_string(),
            "call": call.to_string(),
            "name": name,
            "ok": ok,
            "summary": summary,
            "content": content,
            "resource": resource,
            "arguments": arguments,
            "cwd": cwd,
            "duration_ms": duration_ms,
        }),
        RuntimeEvent::TurnFinished {
            session,
            turn,
            state,
        } => json!({
            "type": "turn_finished",
            "session": session.to_string(),
            "turn": turn.to_string(),
            "state": value(state),
        }),
        RuntimeEvent::BotChanged(bot) => {
            let mut row = bot_row(bot);
            row["type"] = json!("bot_changed");
            row
        }
        RuntimeEvent::AgentRemoved(bot) => json!({"type": "agent_removed", "bot": bot.to_string()}),
        RuntimeEvent::HiveRosterChanged(roster) => {
            json!({"type": "team_changed", "team": value(roster)})
        }
        RuntimeEvent::TeamRemoved(team) => {
            json!({"type": "team_removed", "team": team.to_string()})
        }
        RuntimeEvent::TaskChanged(task) => json!({"type": "task_changed", "task": value(task)}),
        RuntimeEvent::ApprovalRequested(prompt) => {
            let mut row = approval(prompt);
            row["type"] = json!("approval_requested");
            row
        }
        RuntimeEvent::ModeChanged {
            session,
            previous,
            mode,
        } => json!({
            "type": "mode_changed",
            "session": session.to_string(),
            "previous": value(previous),
            "mode": value(mode),
        }),
        RuntimeEvent::ApprovalResolved { approval, allowed } => json!({
            "type": "approval_resolved",
            "approval": approval.to_string(),
            "allowed": allowed,
        }),
        RuntimeEvent::UserQuestion {
            session,
            question,
            choices,
            request_id,
        } => json!({
            "type": "user_question",
            "session": session.to_string(),
            "question": question,
            "choices": choices,
            "request_id": request_id,
        }),
        RuntimeEvent::UserAnswerResolved { request_id, answer } => json!({
            "type": "user_answer_resolved",
            "request_id": request_id,
            "answer": answer,
        }),
        RuntimeEvent::McpChanged {
            configured,
            connected,
            tools,
        } => json!({
            "type": "mcp_changed",
            "configured": configured,
            "connected": connected,
            "tools": tools,
        }),
        RuntimeEvent::PluginsChanged { installed, loaded } => json!({
            "type": "plugins_changed",
            "installed": installed,
            "loaded": loaded,
        }),
        RuntimeEvent::SkillsChanged { discovered, loaded } => json!({
            "type": "skills_changed",
            "discovered": discovered,
            "loaded": loaded,
        }),
        RuntimeEvent::ContextCompacted {
            session,
            folded,
            kept,
            before_tokens,
            after_tokens,
            through_seq,
        } => json!({
            "type": "context_compacted",
            "session": session.to_string(),
            "folded": folded,
            "kept": kept,
            "before_tokens": before_tokens,
            "after_tokens": after_tokens,
            "through_seq": through_seq,
        }),
        RuntimeEvent::ContextPruned {
            session,
            replaced,
            saved_tokens,
        } => json!({
            "type": "context_pruned",
            "session": session.to_string(),
            "replaced": replaced,
            "saved_tokens": saved_tokens,
        }),
        RuntimeEvent::ContextMetered { session, meter } => json!({
            "type": "context_metered",
            "session": session.to_string(),
            "meter": value(meter),
        }),
        RuntimeEvent::ModelsListed { models, error } => json!({
            "type": "models_listed",
            "models": models,
            "error": error,
        }),
        RuntimeEvent::Notice { level, text } => json!({
            "type": "notice",
            "level": notice_level(level),
            "text": text,
        }),
        RuntimeEvent::PlanUpdated { session, plan } => json!({
            "type": "plan_updated",
            "session": session.to_string(),
            "plan": value(plan),
        }),
        RuntimeEvent::Error { text } => json!({"type": "error", "text": text}),
        RuntimeEvent::Shutdown => json!({"type": "shutdown"}),
        RuntimeEvent::Activity(activity) => {
            json!({"type": "activity", "activity": value(activity)})
        }
    };
    let mut body = body;
    body["ts_ms"] = json!(now_ms());
    body
}

#[cfg(test)]
mod tests {
    use super::*;
    use vikram_contracts::{SessionId, TurnId};

    #[test]
    fn a_delta_carries_its_session_turn_and_text() {
        let session = SessionId::new("ses_1");
        let turn = TurnId::new("turn_1");
        let delta = RuntimeEvent::StreamDelta {
            session: session.clone(),
            turn,
            text: "hello".into(),
        };
        assert_eq!(event_session(&delta), Some(&session));
        let row = event(&delta);
        assert_eq!(row["type"], "stream_delta");
        assert_eq!(row["session"], "ses_1");
        assert_eq!(row["text"], "hello");
        assert!(row["ts_ms"].as_u64().unwrap() > 0);
    }

    #[test]
    fn a_global_event_has_no_session_and_a_shutdown_is_serializable() {
        let shutdown = RuntimeEvent::Shutdown;
        assert!(event_session(&shutdown).is_none());
        assert_eq!(event(&shutdown)["type"], "shutdown");
    }
}
