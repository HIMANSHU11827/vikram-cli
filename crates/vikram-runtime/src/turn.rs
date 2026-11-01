//! The turn loop.
//!
//! One turn is: provider stream -> assistant text -> zero or more tool calls, each gated by the
//! policy crate -> tool results -> repeat, bounded by a step budget. The loop owns no durable
//! state of its own: every observation is published, every message is journaled, and cancellation
//! is checked between every step.

use futures_util::StreamExt;
use std::sync::Arc;
use std::time::Instant;
use tokio::sync::oneshot;
use vikram_contracts::policy::Decision;
use vikram_contracts::{
    ApprovalId, Block, Message, PermissionMode, RecordKind, Role, ToolCall, ToolCallId, Trust,
    TurnId,
};

use crate::agents::ManagedAgent;
use crate::provider::{CompletionRequest, ProviderEvent};
use crate::runtime::{PendingApproval, RuntimeInner};
use crate::session::SessionRuntime;
use crate::skills::SkillRegistry;
use crate::types::{
    AgentState, ApprovalPrompt, NoticeLevel, RuntimeEvent, TaskState, TurnState, now_ms,
};

/// An agent turn carries the task record it must close when it finishes.
pub struct AgentRun {
    pub agent: Arc<ManagedAgent>,
    pub task_id: String,
}

pub(crate) fn spawn_turn(
    inner: Arc<RuntimeInner>,
    session: Arc<SessionRuntime>,
    text: String,
    agent: Option<AgentRun>,
) {
    tokio::spawn(async move {
        run_turn(inner, session, text, agent).await;
    });
}

pub(crate) async fn run_turn(
    inner: Arc<RuntimeInner>,
    session: Arc<SessionRuntime>,
    text: String,
    agent: Option<AgentRun>,
) {
    let turn = TurnId::generate();
    let started = Instant::now();
    let cancel = session.cancel_handle();

    // The user's message is durable before any model work begins.
    let user_message = Message::user(text.clone());
    session.push_message(user_message.clone(), now_ms());
    inner.journal(
        &session.id,
        RecordKind::Message,
        serde_json::json!({"message": user_message}),
    );
    inner.emit(RuntimeEvent::UserMessage {
        session: session.id.clone(),
        turn: turn.clone(),
        text,
    });
    inner.touch(&session);

    session.begin_turn(now_ms());
    inner.emit(RuntimeEvent::StreamStarted {
        session: session.id.clone(),
        turn: turn.clone(),
    });
    if let Some(run) = &agent {
        run.agent.set_state(AgentState::Thinking);
        inner.publish_agent(&run.agent);
    }
    inner.emit(RuntimeEvent::Status(inner.status()));
    inner.publish_metrics();

    let mut outcome = TurnState::Idle;
    let mut output_tokens = 0u64;
    let mut reported_output: Option<u64> = None;
    let mut reported_input: Option<u64> = None;
    let max_steps = u64::from(inner.config.max_steps);
    let mut steps_used = 0u64;

    let Some(provider) = inner.provider.clone() else {
        inner.emit(RuntimeEvent::Error {
            text: "no model configured: set `model` and the API key named by `api_key_env`".into(),
        });
        finish(
            &inner,
            &session,
            &agent,
            turn,
            TurnState::Failed,
            started,
            output_tokens,
        );
        return;
    };

    loop {
        if cancel.is_cancelled() {
            outcome = TurnState::Cancelled;
            break;
        }
        wait_unpaused(&agent, &cancel).await;
        if cancel.is_cancelled() {
            outcome = TurnState::Cancelled;
            break;
        }

        // Inject the budgeted skill catalogue (metadata only) as an External-trust
        // system message. Skill bodies are never loaded here; they are read on demand
        // through skill.load / skill.read.
        let reg = SkillRegistry::for_workspace(&inner.workspace_root);
        let discovered = reg.skills().len();
        inner.emit(RuntimeEvent::SkillsChanged {
            discovered,
            loaded: 0,
        });
        let (catalog_text, _) = reg.catalog_listing();
        let mut request_messages = Vec::new();
        if !catalog_text.trim().is_empty() {
            request_messages.push(Message::text(Role::System, catalog_text, Trust::External));
        }
        request_messages.extend(session.messages());

        let request = CompletionRequest {
            model: provider.model().to_owned(),
            messages: request_messages,
            tools: inner.tools.specs(),
            max_output_tokens: inner.config.max_output_tokens,
        };

        let mut stream = provider.stream(request);
        let mut text_buffer = String::new();
        let mut reasoning_buffer = String::new();
        let mut calls: Vec<ToolCall> = Vec::new();
        let mut stream_failed = false;

        while let Some(item) = stream.next().await {
            if cancel.is_cancelled() {
                break;
            }
            match item {
                Ok(ProviderEvent::TextDelta(delta)) => {
                    if !delta.is_empty() {
                        output_tokens += vikram_kernel::TokenMeter::estimate(&delta);
                        text_buffer.push_str(&delta);
                        inner.emit(RuntimeEvent::StreamDelta {
                            session: session.id.clone(),
                            turn: turn.clone(),
                            text: delta,
                        });
                        if let Some(run) = &agent {
                            if run.agent.state() != AgentState::Running {
                                run.agent.set_state(AgentState::Running);
                                inner.publish_agent(&run.agent);
                            }
                        }
                    }
                }
                Ok(ProviderEvent::ThinkingDelta(delta)) => {
                    if !delta.is_empty() {
                        reasoning_buffer.push_str(&delta);
                        inner.emit(RuntimeEvent::StreamThinkingDelta {
                            session: session.id.clone(),
                            turn: turn.clone(),
                            text: delta,
                        });
                    }
                }
                Ok(ProviderEvent::ToolCall(call)) => calls.push(call),
                Ok(ProviderEvent::Finished {
                    input_tokens,
                    output_tokens: out,
                }) => {
                    reported_input = input_tokens;
                    reported_output = out;
                    break;
                }
                Err(err) => {
                    inner.emit(RuntimeEvent::Error {
                        text: format!("provider: {err}"),
                    });
                    stream_failed = true;
                    break;
                }
            }
        }

        if cancel.is_cancelled() {
            if !text_buffer.is_empty() {
                commit_assistant_text(&inner, &session, &text_buffer, &reasoning_buffer);
                reasoning_buffer.clear();
            }
            outcome = TurnState::Cancelled;
            break;
        }
        if stream_failed {
            if !text_buffer.is_empty() {
                commit_assistant_text(&inner, &session, &text_buffer, &reasoning_buffer);
                reasoning_buffer.clear();
            }
            outcome = TurnState::Failed;
            break;
        }

        if !text_buffer.is_empty() {
            commit_assistant_text(&inner, &session, &text_buffer, &reasoning_buffer);
            reasoning_buffer.clear();
        }

        if calls.is_empty() {
            break;
        }

        let mut results: Vec<(ToolCallId, String, bool)> = Vec::new();
        let mut cancelled_mid_tools = false;

        for call in calls {
            if cancel.is_cancelled() {
                cancelled_mid_tools = true;
                break;
            }
            if steps_used >= max_steps {
                inner.emit(RuntimeEvent::Error {
                    text: format!("step budget exhausted after {} steps", steps_used),
                });
                outcome = TurnState::Failed;
                cancelled_mid_tools = true;
                break;
            }
            steps_used += 1;
            let (content, ok) = run_tool(&inner, &session, &call, &cancel, &agent).await;
            results.push((call.id.clone(), content, ok));
            if cancel.is_cancelled() {
                cancelled_mid_tools = true;
                break;
            }
        }

        for (call_id, content, ok) in results {
            let result = Message::tool_result(call_id, content, ok);
            session.push_message(result.clone(), now_ms());
            inner.journal(
                &session.id,
                RecordKind::ToolResult,
                serde_json::json!({"message": result}),
            );
            inner.emit(RuntimeEvent::MessageCommitted {
                session: session.id.clone(),
                message: result,
            });
        }

        if cancelled_mid_tools {
            if cancel.is_cancelled() {
                outcome = TurnState::Cancelled;
            }
            break;
        }
    }

    session.record_usage(reported_input, reported_output.or(Some(output_tokens)));
    finish(
        &inner,
        &session,
        &agent,
        turn,
        outcome,
        started,
        output_tokens,
    );
}

fn commit_assistant_text(
    inner: &Arc<RuntimeInner>,
    session: &Arc<SessionRuntime>,
    text: &str,
    reasoning: &str,
) {
    let mut blocks = Vec::new();
    if !reasoning.is_empty() {
        blocks.push(Block::Reasoning {
            text: reasoning.to_owned(),
            trust: Trust::Model,
        });
    }
    if !text.is_empty() {
        blocks.push(Block::Text {
            text: text.to_owned(),
            trust: Trust::Model,
        });
    }
    if blocks.is_empty() {
        return;
    }
    let message = Message {
        role: Role::Assistant,
        blocks,
    };
    session.push_message(message.clone(), now_ms());
    inner.journal(
        &session.id,
        RecordKind::Message,
        serde_json::json!({"message": message}),
    );
    inner.emit(RuntimeEvent::MessageCommitted {
        session: session.id.clone(),
        message,
    });
}

fn finish(
    inner: &Arc<RuntimeInner>,
    session: &Arc<SessionRuntime>,
    agent: &Option<AgentRun>,
    turn: TurnId,
    outcome: TurnState,
    started: Instant,
    output_tokens: u64,
) {
    let elapsed = started.elapsed();
    let elapsed_ms = elapsed.as_millis() as u64;
    let tps = if output_tokens > 0 && !elapsed.is_zero() {
        Some(output_tokens as f64 / elapsed.as_secs_f64())
    } else {
        None
    };
    *inner.last_turn_ms.lock().expect("turn metric") = Some(elapsed_ms);
    *inner.last_tokens_per_sec.lock().expect("tps metric") = tps;

    session.finish_turn(outcome, now_ms());
    inner.emit(RuntimeEvent::TurnFinished {
        session: session.id.clone(),
        turn,
        state: outcome,
    });

    if let Some(run) = agent {
        let (state, task_state) = match outcome {
            TurnState::Cancelled => (AgentState::Stopped, TaskState::Cancelled),
            TurnState::Failed => (AgentState::Failed, TaskState::Failed),
            _ => (AgentState::Completed, TaskState::Completed),
        };
        run.agent.set_state(state);
        run.agent.set_task(None);
        inner.publish_agent(&run.agent);
        if let Some(task) = inner.tasks.finish(&run.task_id, task_state) {
            inner.task_snapshot(&task);
        }
    }

    inner.emit(RuntimeEvent::Status(inner.status()));
    inner.publish_metrics();

    if outcome != TurnState::Cancelled
        && !session.cancel_handle().is_cancelled()
        && let Some(next) = session.dequeue()
    {
        spawn_turn(
            Arc::clone(inner),
            Arc::clone(session),
            next,
            agent.as_ref().map(|run| AgentRun {
                agent: Arc::clone(&run.agent),
                task_id: String::new(),
            }),
        );
    } else if session.queue_len() > 0 {
        inner.emit(RuntimeEvent::Notice {
            level: NoticeLevel::Warn,
            text: format!("{} queued message(s) pending", session.queue_len()),
        });
    }
}

async fn wait_unpaused(agent: &Option<AgentRun>, cancel: &vikram_kernel::CancellationToken) {
    let Some(run) = agent else {
        return;
    };
    while run.agent.is_paused() {
        if cancel.is_cancelled() {
            return;
        }
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    }
}

async fn run_tool(
    inner: &Arc<RuntimeInner>,
    session: &Arc<SessionRuntime>,
    call: &ToolCall,
    cancel: &vikram_kernel::CancellationToken,
    agent: &Option<AgentRun>,
) -> (String, bool) {
    let Some(tool) = inner.tools.get(&call.name) else {
        inner.emit(RuntimeEvent::ToolFinished {
            session: session.id.clone(),
            call: call.id.clone(),
            name: call.name.clone(),
            ok: false,
            summary: "not registered".into(),
        });
        return (format!("tool `{}` is not registered", call.name), false);
    };

    let request = match tool.permission(&call.arguments) {
        Ok(request) => request,
        Err(err) => {
            inner.emit(RuntimeEvent::ToolFinished {
                session: session.id.clone(),
                call: call.id.clone(),
                name: call.name.clone(),
                ok: false,
                summary: err.to_string(),
            });
            return (err.to_string(), false);
        }
    };

    let raw = inner.policy().evaluate(&request);
    let decision = inner.judge(&request);
    let auto = PermissionMode::auto_resolved(&raw, &decision);
    inner.journal(
        &session.id,
        RecordKind::Policy,
        serde_json::json!({
            "verb": request.verb,
            "resource": request.resource,
            "mode": inner.mode().as_str(),
            "evaluated": decision_name(&raw),
            "decision": decision_name(&decision),
            "auto": auto,
            "reason": decision_reason(&decision),
        }),
    );

    match decision {
        Decision::Deny { reason } => {
            inner.emit(RuntimeEvent::ToolFinished {
                session: session.id.clone(),
                call: call.id.clone(),
                name: call.name.clone(),
                ok: false,
                summary: format!("denied: {reason}"),
            });
            return (format!("permission denied: {reason}"), false);
        }
        Decision::RequireApproval { reason, amendment } => {
            let approval = ApprovalId::generate();
            let prompt = ApprovalPrompt {
                approval: approval.clone(),
                session: session.id.clone(),
                verb: request.verb.clone(),
                resource: request.resource.clone(),
                reason: reason.clone(),
                amendment,
            };
            let (tx, rx) = oneshot::channel();
            inner.approvals.lock().expect("approvals lock").insert(
                approval.clone(),
                PendingApproval {
                    prompt: prompt.clone(),
                    respond: tx,
                },
            );
            session.set_turn(TurnState::WaitingApproval);
            if let Some(run) = agent {
                run.agent.set_state(AgentState::Waiting);
                inner.publish_agent(&run.agent);
            }
            inner.emit(RuntimeEvent::ApprovalRequested(prompt));
            inner.emit(RuntimeEvent::ToolStarted {
                session: session.id.clone(),
                call: call.id.clone(),
                name: call.name.clone(),
                summary: format!("{} {} · awaiting approval", request.verb, request.resource),
            });

            let allowed = wait_for_approval(rx, cancel).await;
            if cancel.is_cancelled() {
                return ("cancelled while awaiting approval".into(), false);
            }
            inner.journal(
                &session.id,
                RecordKind::Approval,
                serde_json::json!({
                    "approval": approval.as_str(),
                    "verb": request.verb,
                    "resource": request.resource,
                    "answered": if allowed { "allow" } else { "deny" },
                }),
            );
            if !allowed {
                inner.emit(RuntimeEvent::ToolFinished {
                    session: session.id.clone(),
                    call: call.id.clone(),
                    name: call.name.clone(),
                    ok: false,
                    summary: "denied by operator".into(),
                });
                return ("permission denied by operator".into(), false);
            }
            session.set_turn(TurnState::Tooling);
            if let Some(run) = agent {
                run.agent.set_state(AgentState::Running);
                inner.publish_agent(&run.agent);
            }
        }
        Decision::Allow => {}
    }

    inner.journal(
        &session.id,
        RecordKind::ToolCall,
        serde_json::json!({"name": call.name, "arguments": call.arguments}),
    );

    let summary = summarize_arguments(&call.arguments);
    inner.emit(RuntimeEvent::ToolStarted {
        session: session.id.clone(),
        call: call.id.clone(),
        name: call.name.clone(),
        summary: summary.clone(),
    });
    session.set_turn(TurnState::Tooling);
    if let Some(run) = agent {
        run.agent.set_state(AgentState::Running);
        inner.publish_agent(&run.agent);
    }

    let context = inner.tool_context();
    let arguments = call.arguments.clone();
    let handle = tool.clone();
    let joined = tokio::task::spawn_blocking(move || handle.run(&arguments, &context)).await;

    let (mut content, mut ok) = match joined {
        Ok(Ok(output)) => (output.content, output.ok),
        Ok(Err(err)) => (err.to_string(), false),
        Err(err) => (format!("tool task failed: {err}"), false),
    };

    // `ask_user` does not answer a question, it asks one. The reply is registered as a pending
    // question, published, and waited on with the same oneshot discipline the approval gate uses;
    // the operator's answer is appended to the tool result so the model sees it as the outcome of
    // its own call. Cancellation resolves the wait and fails the call instead of hanging.
    if ok && let Some((request_id, question, choices)) = parse_question(&content) {
        let receiver = inner.open_question(&request_id);
        inner.emit(RuntimeEvent::UserQuestion {
            session: session.id.clone(),
            question,
            choices,
            request_id: request_id.clone(),
        });
        if let Some(run) = agent {
            run.agent.set_state(AgentState::Waiting);
            inner.publish_agent(&run.agent);
        }
        match wait_for_answer(receiver, cancel).await {
            Some(answer) => content = format!("{content}\nUser answered: {answer}"),
            None => {
                inner.close_question(&request_id);
                ok = false;
                content = format!("{content}\nStatus: cancelled");
            }
        }
    }

    inner.journal(
        &session.id,
        RecordKind::ToolResult,
        serde_json::json!({
            "call": call.id.as_str(),
            "name": call.name,
            "ok": ok,
            "content": truncate(&content, 64_000),
        }),
    );
    inner.emit(RuntimeEvent::ToolFinished {
        session: session.id.clone(),
        call: call.id.clone(),
        name: call.name.clone(),
        ok,
        summary: if ok {
            summary
        } else {
            format!("failed · {summary}")
        },
    });

    (content, ok)
}

async fn wait_for_approval(
    receiver: oneshot::Receiver<bool>,
    cancel: &vikram_kernel::CancellationToken,
) -> bool {
    tokio::select! {
        result = receiver => result.unwrap_or(false),
        _ = async {
            while !cancel.is_cancelled() {
                tokio::time::sleep(std::time::Duration::from_millis(100)).await;
            }
        } => false,
    }
}

/// The question gate's twin of [`wait_for_approval`]: the answer, or `None` once the turn is
/// cancelled. It never invents an answer, so a cancelled question fails the tool call.
async fn wait_for_answer(
    receiver: oneshot::Receiver<String>,
    cancel: &vikram_kernel::CancellationToken,
) -> Option<String> {
    tokio::select! {
        result = receiver => result.ok(),
        _ = async {
            while !cancel.is_cancelled() {
                tokio::time::sleep(std::time::Duration::from_millis(100)).await;
            }
        } => None,
    }
}

/// Parse the `ask_user` tool's structured reply:
/// `QUESTION:{request_id}\n{question}\nChoices: {json}\nStatus: awaiting_user_response`.
///
/// Returns `None` for any ordinary tool output, so the gate only opens for a real question.
fn parse_question(content: &str) -> Option<(String, String, Vec<String>)> {
    let mut lines = content.lines();
    let request_id = lines.next()?.strip_prefix("QUESTION:")?.trim().to_owned();
    if request_id.is_empty() {
        return None;
    }
    let mut question_lines: Vec<&str> = Vec::new();
    let mut choices: Vec<String> = Vec::new();
    for line in lines {
        if let Some(json) = line.strip_prefix("Choices:") {
            choices = serde_json::from_str(json.trim()).unwrap_or_default();
        } else if !line.starts_with("Status:") {
            question_lines.push(line);
        }
    }
    let question = question_lines.join("\n").trim().to_owned();
    Some((request_id, question, choices))
}

/// What a decision is, in the one word the journal uses for it.
fn decision_name(decision: &Decision) -> &'static str {
    match decision {
        Decision::Allow => "allow",
        Decision::Deny { .. } => "deny",
        Decision::RequireApproval { .. } => "ask",
    }
}

/// The reason a decision carries; an allow carries none.
fn decision_reason(decision: &Decision) -> String {
    match decision {
        Decision::Allow => String::new(),
        Decision::Deny { reason } | Decision::RequireApproval { reason, .. } => reason.clone(),
    }
}

fn summarize_arguments(arguments: &serde_json::Value) -> String {
    let text = match arguments {
        serde_json::Value::Object(map) => {
            let mut parts = Vec::new();
            for (key, value) in map.iter().take(3) {
                let rendered = match value {
                    serde_json::Value::String(s) => s.clone(),
                    other => other.to_string(),
                };
                parts.push(format!("{key}={rendered}"));
            }
            parts.join(" ")
        }
        other => other.to_string(),
    };
    truncate(&text, 80)
}

fn truncate(text: &str, limit: usize) -> String {
    if text.len() <= limit {
        return text.to_owned();
    }
    let cut = text
        .char_indices()
        .take_while(|(i, _)| *i < limit)
        .last()
        .map(|(i, c)| i + c.len_utf8())
        .unwrap_or(0);
    format!("{}…", &text[..cut])
}

/// Blocks that a provider-facing message may carry; exposed for tests of the loop's message shape.
#[allow(dead_code)]
fn assistant_with_calls(text: &str, calls: Vec<ToolCall>) -> Message {
    let mut blocks = Vec::new();
    if !text.is_empty() {
        blocks.push(Block::Text {
            text: text.to_owned(),
            trust: Trust::Model,
        });
    }
    for call in calls {
        blocks.push(Block::ToolCall { call });
    }
    Message {
        role: Role::Assistant,
        blocks,
    }
}

#[cfg(test)]
mod tests {
    use super::parse_question;

    #[test]
    fn an_ask_user_reply_yields_the_request_id_the_question_and_the_choices() {
        let content =
            "QUESTION:ask-1-0\nWhich file?\nChoices: [\"a\",\"b\"]\nStatus: awaiting_user_response";
        let (request_id, question, choices) =
            parse_question(content).expect("the ask_user shape must parse");
        assert_eq!(request_id, "ask-1-0");
        assert_eq!(question, "Which file?");
        assert_eq!(choices, vec!["a".to_owned(), "b".to_owned()]);
    }

    #[test]
    fn a_question_without_choices_still_parses() {
        let content = "QUESTION:ask-2-1\nExplain?\nChoices: []\nStatus: awaiting_user_response";
        let (request_id, question, choices) = parse_question(content).unwrap();
        assert_eq!(request_id, "ask-2-1");
        assert_eq!(question, "Explain?");
        assert!(choices.is_empty());
    }

    #[test]
    fn ordinary_tool_output_is_not_treated_as_a_question() {
        assert!(parse_question("hello").is_none());
        assert!(parse_question("").is_none());
        assert!(parse_question("QUESTION:\nq").is_none());
    }
}
