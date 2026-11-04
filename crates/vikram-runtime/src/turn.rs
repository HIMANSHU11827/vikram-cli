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
    ActionKind, ApprovalId, Block, Message, PermissionMode, PermissionRequest, RecordKind, Role,
    ToolCall, ToolCallId, Trust, TurnId,
};

use crate::agents::ManagedAgent;
use crate::learning::TurnGrowth;
use crate::provider::{CompletionRequest, ProviderEvent};
use crate::runtime::{PendingApproval, RuntimeInner};
use crate::session::SessionRuntime;
use crate::skills::SkillRegistry;
use crate::types::{
    AgentState, ApprovalPrompt, ContextMeter, ContextPart, NoticeLevel, RuntimeEvent, TaskState,
    TurnState, now_ms,
};

/// An agent turn carries the task record it must close when it finishes.
pub struct AgentRun {
    pub agent: Arc<ManagedAgent>,
    pub task_id: String,
}

/// Who asked for a tool call. The gate itself is identical in both cases — the same evaluator, the
/// same floor, the same posture — so making this a parameter cannot widen anything. What differs is
/// the provenance recorded on the request and whether the call is allowed to rewrite the turn state
/// of a session that may already be running one.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Origin {
    Model,
    /// Typed by the human at the keyboard: the `!` prefix.
    Operator,
}

impl Origin {
    fn trust(self) -> Trust {
        match self {
            Origin::Model => Trust::Model,
            Origin::Operator => Trust::User,
        }
    }

    fn as_str(self) -> &'static str {
        match self {
            Origin::Model => "model",
            Origin::Operator => "operator",
        }
    }
}

/// Said to the model on every turn, in its own system message. The transcript prints these lines
/// between the tool rows as the agent's voice, so they are part of the interface and not decoration:
/// one or two sentences, present tense, naming the file, command or question in hand.
const PREAMBLE: &str = "\
You are VIKRAM, a personal AI assistant with a real personality — a friend and family member to \
the user — virtually present, still in active development. You are not a robot and not just a CLI; \
you are a proper AI assistant person. You are primarily a private AI assistant for the user, while \
also supporting multiple users (family and friends) when needed. You remember and understand the \
user and help with many tasks, like a capable personal assistant. You never sugarcoat: you are \
honest and direct about what a personal AI assistant should say, even when it is uncomfortable, \
because honesty is part of being a true friend. Chat politely and clearly, in a proper, respectful \
manner, like talking with a friend or family member, and introduce yourself as an AI assistant \
when it helps. Alongside your tool calls, write one or two short sentences of progress in the \
present tense: what you are about to do and why, naming the file, command or question involved. \
Write one before you start a piece of work, and again whenever what you are doing changes, for \
example when one thing finished and another begins. These lines are shown to the operator as your \
voice between the tool rows, so each must stand alone. Do not narrate every step, do not repeat a \
line you already wrote, and never describe your private reasoning.";

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
    let request_text = text.clone();
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
    // The conversation now has something to be named after. Naming runs in the background and is
    // gated to one attempt per session, so this costs nothing on the turn the operator is waiting for.
    inner.maybe_title_session(&session);

    session.begin_turn(now_ms());
    // What this turn actually did, collected as it happens and handed to the growth pipeline.
    let mut growth = TurnGrowth::default();
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
    let mut last_estimated_input = 0u64;
    let mut over_limit_retries = 0u32;
    let max_steps = u64::from(inner.config.max_steps);
    let mut steps_used = 0u64;

    let Some(provider) = inner.provider.read().expect("provider lock").clone() else {
        inner.emit(RuntimeEvent::Error {
            text: "no model configured: set `model` and the API key named by `api_key_env`".into(),
        });
        finish(
            &inner,
            &session,
            &agent,
            turn,
            TurnReport {
                outcome: TurnState::Failed,
                started,
                output_tokens,
                request: &request_text,
                growth: &TurnGrowth::default(),
            },
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
        let skills = reg.skills();
        let discovered = skills.len();
        let loaded = skills.iter().filter(|s| s.loaded).count();
        inner.emit(RuntimeEvent::SkillsChanged { discovered, loaded });
        let (catalog_text, _) = reg.catalog_listing();
        // Every part of the request is labelled in the order it is assembled, so the inspector
        // prices what is actually being sent rather than guessing at the shape afterwards.
        let mut prefix: Vec<Message> = Vec::new();
        let mut labels: Vec<&'static str> = Vec::new();
        if !catalog_text.trim().is_empty() {
            labels.push("skill catalogue");
            prefix.push(Message::text(Role::System, catalog_text, Trust::External));
        }
        // Project and entity instructions are context, not decoration: they arrive on every turn so
        // the model does not have to re-read the same AGENTS.md to remember the rules it works under.
        if let Some(project_text) = inner.project_context() {
            labels.push("project instructions");
            prefix.push(Message::text(Role::System, project_text, Trust::External));
        }
        // A delegated agent carries its own identity: the profile's name, role and description
        // become the system identity for its session, so its behaviour follows its configuration.
        if let Some(run) = &agent {
            let profile = &run.agent.profile;
            let identity = format!(
                "You are {}. Role: {}. {}.",
                profile.name, profile.role, profile.description
            );
            labels.push("agent identity");
            prefix.push(Message::text(Role::System, identity, Trust::External));
        }
        // The operator sees the model's own words between the tool rows, so the model has to be
        // told that those words exist and what shape they take.
        labels.push("turn voice");
        prefix.push(Message::text(Role::System, PREAMBLE, Trust::External));
        // Memory and project context are assembled against the budget that is left after the
        // transcript, not on top of it.
        if let Some(memory_text) = inner.retrieved_memory(&session) {
            labels.insert(0, "retrieved memory");
            prefix.insert(0, Message::text(Role::System, memory_text, Trust::External));
        }
        let (transcript, budget) = inner.prepare_context(&session, &prefix);
        let estimator = session.estimator();
        let mut parts: Vec<ContextPart> = labels
            .iter()
            .zip(&prefix)
            .map(|(label, message)| ContextPart::new(*label, estimator.estimate_message(message)))
            .collect();
        parts.push(ContextPart::new(
            "tool schemas",
            inner.tool_schema_tokens(&estimator),
        ));
        let mut summary_tokens = 0u64;
        let mut tool_result_tokens = 0u64;
        let mut conversation_tokens = 0u64;
        for (index, message) in transcript.iter().enumerate() {
            let cost = estimator.estimate_message(message);
            if index == 0 && crate::compaction::is_compaction_summary(message) {
                summary_tokens += cost;
            } else if message.role == Role::Tool {
                tool_result_tokens += cost;
            } else {
                conversation_tokens += cost;
            }
        }
        for (label, tokens) in [
            ("compaction summary", summary_tokens),
            ("conversation", conversation_tokens),
            ("tool results", tool_result_tokens),
        ] {
            if tokens > 0 {
                parts.push(ContextPart::new(label, tokens));
            }
        }
        let used = crate::compaction::active_context_tokens(&estimator, &transcript, &budget);
        inner.emit(RuntimeEvent::ContextMetered {
            session: session.id.clone(),
            meter: ContextMeter {
                window: budget.limits.context_window,
                usable: budget.usable_input(),
                used,
                measured: budget.anchor.is_some(),
                limits_assumed: budget.limits.source
                    == crate::context_engine::LimitsSource::Assumed,
                output_reserve: budget.output_reserve,
                overhead: budget.overhead,
                parts,
            },
        });
        let mut request_messages = prefix;
        request_messages.extend(transcript);

        // Recorded against the provider's own figure when it reports one, which is how the
        // estimator learns this model's real cost per character.
        let estimated_input = estimator.estimate_messages(&request_messages);
        last_estimated_input = estimated_input;
        let request = CompletionRequest {
            model: inner.model(),
            messages: request_messages,
            tools: inner.tools.specs(),
            max_output_tokens: inner.config.max_output_tokens,
        };

        let mut stream = provider.stream(request);
        let mut dropped_for_limit = false;
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
                        output_tokens += vikram_contracts::TokenMeter::estimate(&delta);
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
                    let text = err.to_string();
                    // A provider that rejects the request for size is recoverable once, by dropping
                    // the oldest turn the transcript can afford to lose. Retrying the identical
                    // request would fail identically, and looping on it burns the step budget.
                    if inner.context_limit_error(&text) && over_limit_retries < 2 {
                        over_limit_retries += 1;
                        let dropped = session.drop_oldest_for_limit(now_ms());
                        if dropped > 0 {
                            inner.emit(RuntimeEvent::Notice {
                                level: NoticeLevel::Warn,
                                text: format!(
                                    "model rejected the request as too large: dropped {dropped} oldest messages (still durable in the journal) and retried"
                                ),
                            });
                            dropped_for_limit = true;
                            break;
                        }
                    }
                    inner.emit(RuntimeEvent::Error {
                        text: format!("provider: {text}"),
                    });
                    stream_failed = true;
                    break;
                }
            }
        }

        if dropped_for_limit {
            // Nothing was produced; rebuild the request from the shortened transcript.
            continue;
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
            if !text_buffer.is_empty() || !reasoning_buffer.is_empty() {
                commit_assistant_text(&inner, &session, &text_buffer, &reasoning_buffer);
                text_buffer.clear();
                reasoning_buffer.clear();
            }
            outcome = TurnState::Failed;
            break;
        }

        // Whatever the model said or thought before it reached for a tool belongs at this point in
        // the transcript, not at the end of the turn. Committing only when there was text left a
        // reasoning-only step buffered until some later message carried it, which is how thoughts
        // ended up piled under the answer. Both buffers are cleared because they were just spent.
        if !text_buffer.is_empty() || !reasoning_buffer.is_empty() {
            commit_assistant_text(&inner, &session, &text_buffer, &reasoning_buffer);
            text_buffer.clear();
            reasoning_buffer.clear();
        }

        if calls.is_empty() {
            break;
        }

        let mut results: Vec<(ToolCallId, String, bool)> = Vec::new();
        let mut cancelled_mid_tools = false;

        for (index, call) in calls.iter().enumerate() {
            if cancel.is_cancelled() {
                cancelled_mid_tools = true;
                results.extend(answer_unrun(&calls[index..], Abandoned::Cancelled));
                break;
            }
            if steps_used >= max_steps {
                inner.emit(RuntimeEvent::Error {
                    text: format!("step budget exhausted after {} steps", steps_used),
                });
                outcome = TurnState::Failed;
                cancelled_mid_tools = true;
                results.extend(answer_unrun(&calls[index..], Abandoned::OutOfSteps));
                break;
            }
            steps_used += 1;
            let (content, ok) =
                run_tool(&inner, &session, &call, &cancel, &agent, Origin::Model).await;
            growth.record_call(&call.name);
            if !ok {
                growth.record_failure(&call.name, &content);
            }
            results.push((call.id.clone(), content, ok));
            if cancel.is_cancelled() {
                cancelled_mid_tools = true;
                results.extend(answer_unrun(&calls[index + 1..], Abandoned::Cancelled));
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

        // Steer: incorporate a user instruction injected mid-turn at this safe point.
        if let Some(steer_text) = session.take_steer() {
            let msg = Message::user(steer_text.clone());
            session.push_message(msg.clone(), now_ms());
            inner.journal(
                &session.id,
                RecordKind::Message,
                serde_json::json!({"message": msg}),
            );
            inner.emit(RuntimeEvent::MessageCommitted {
                session: session.id.clone(),
                message: msg,
            });
            inner.emit(RuntimeEvent::Notice {
                level: NoticeLevel::Info,
                text: "steered".into(),
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
    if let (Some(input), Some(output)) = (reported_input, reported_output) {
        // Compare what we predicted for this exact prompt with what the provider charged for it.
        session.observe_reported_usage(last_estimated_input, input, output);
    }
    // Account for the turn durably. A provider-reported pair is written as `measured`; anything
    // else is the estimator's figure and is labelled so, never passed off as a billed count.
    let prompt_tokens = reported_input.unwrap_or(last_estimated_input);
    let completion_tokens = reported_output.unwrap_or(output_tokens);
    if prompt_tokens + completion_tokens > 0 {
        let _ = inner.store.record_usage(&vikram_store::UsageRow {
            id: 0,
            session: session.id.clone(),
            ts_ms: now_ms(),
            model: inner.model(),
            prompt_tokens,
            completion_tokens,
            total_tokens: prompt_tokens + completion_tokens,
            measured: reported_input.is_some() && reported_output.is_some(),
        });
    }
    finish(
        &inner,
        &session,
        &agent,
        turn,
        TurnReport {
            outcome,
            started,
            output_tokens,
            request: &request_text,
            growth: &growth,
        },
    );
}

/// Why a batch of tool calls stopped before all of them ran.
enum Abandoned {
    Cancelled,
    OutOfSteps,
}

/// Answer the calls a batch never reached. The model asked for them, so the transcript the next turn
/// reads has to say what happened to each one: silence is what makes a resumed conversation
/// unreadable, and the honest line differs depending on whether a person stopped the turn or the
/// step budget did.
fn answer_unrun(calls: &[ToolCall], why: Abandoned) -> Vec<(ToolCallId, String, bool)> {
    let text = match why {
        Abandoned::Cancelled => "cancelled before completion",
        Abandoned::OutOfSteps => "not run: the step budget was exhausted first",
    };
    calls
        .iter()
        .map(|call| (call.id.clone(), text.to_owned(), false))
        .collect()
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

/// What a finished turn is worth telling the rest of the runtime: how it ended, what it cost, what
/// the operator asked for, and what the turn actually did.
struct TurnReport<'a> {
    outcome: TurnState,
    started: Instant,
    output_tokens: u64,
    request: &'a str,
    growth: &'a TurnGrowth,
}

fn finish(
    inner: &Arc<RuntimeInner>,
    session: &Arc<SessionRuntime>,
    agent: &Option<AgentRun>,
    turn: TurnId,
    report: TurnReport<'_>,
) {
    let TurnReport {
        outcome,
        started,
        output_tokens,
        request,
        growth,
    } = report;
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
    // Growth runs before the turn is announced finished, so an interface that reads what was
    // learned on this event never sees a turn that has not yet been learned from.
    crate::learning::on_turn_end(inner, session, request, outcome, growth);
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
        // A delegated session answers the session that asked; the answer is bounded, and the child's
        // transcript is not handed over with it.
        if let Some(parent) = session.parent() {
            inner.deliver_delegation(&parent, session);
        }
    }

    inner.emit(RuntimeEvent::Status(inner.status()));
    inner.publish_metrics();

    if outcome != TurnState::Cancelled
        && !session.cancel_handle().is_cancelled()
        && let Some(next) = session.dequeue()
    {
        // A queued follow-up keeps the same agent (if any) but gets its own
        // background task id so progress and completion are tracked rather than
        // being silently dropped (the previous `String::new()` lost the task).
        let queued_agent = agent.as_ref().map(|run| {
            run.agent.set_state(AgentState::Thinking);
            run.agent.set_task(Some(next.clone()));
            inner.publish_agent(&run.agent);
            let bg = inner.tasks.start(
                format!("agent: {}", run.agent.profile.name),
                session.id.clone(),
            );
            AgentRun {
                agent: Arc::clone(&run.agent),
                task_id: bg.id.clone(),
            }
        });
        spawn_turn(Arc::clone(inner), Arc::clone(session), next, queued_agent);
    } else if session.queue_len() > 0 {
        inner.emit(RuntimeEvent::Notice {
            level: NoticeLevel::Warn,
            text: format!("{} queued message(s) pending", session.queue_len()),
        });
    }
}

async fn wait_unpaused(agent: &Option<AgentRun>, cancel: &vikram_contracts::CancellationToken) {
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

/// Run one tool call the operator asked for by name — what `!` types, and what a `/` command that
/// actually does something does. It goes through the same gate, the same approval prompt and the
/// same journal entries as a model-issued call; what differs is the provenance, and that the output
/// is not handed back to a model.
pub(crate) async fn run_operator_tool(
    inner: &Arc<RuntimeInner>,
    session: &Arc<SessionRuntime>,
    call: &ToolCall,
) -> (String, bool) {
    let cancel = session.cancel_handle();
    run_tool(inner, session, call, &cancel, &None, Origin::Operator).await
}

async fn run_tool(
    inner: &Arc<RuntimeInner>,
    session: &Arc<SessionRuntime>,
    call: &ToolCall,
    cancel: &vikram_contracts::CancellationToken,
    agent: &Option<AgentRun>,
    origin: Origin,
) -> (String, bool) {
    let started = std::time::Instant::now();
    let cwd = inner.tool_context().workspace_root.display().to_string();
    let arguments_text = truncate(&call.arguments.to_string(), 8_000);
    let elapsed = move || started.elapsed().as_millis() as u64;

    let Some(tool) = inner.tools.get(&call.name) else {
        inner.emit(RuntimeEvent::ToolFinished {
            session: session.id.clone(),
            call: call.id.clone(),
            name: call.name.clone(),
            ok: false,
            summary: "not registered".into(),
            content: format!("tool `{}` is not registered", call.name),
            resource: String::new(),
            arguments: arguments_text.clone(),
            cwd: cwd.clone(),
            duration_ms: elapsed(),
        });
        return (format!("tool `{}` is not registered", call.name), false);
    };

    if !inner.tools.is_enabled(&call.name) {
        inner.emit(RuntimeEvent::ToolFinished {
            session: session.id.clone(),
            call: call.id.clone(),
            name: call.name.clone(),
            ok: false,
            summary: "disabled".into(),
            content: format!("tool `{}` is disabled", call.name),
            resource: String::new(),
            arguments: arguments_text.clone(),
            cwd: cwd.clone(),
            duration_ms: elapsed(),
        });
        return (format!("tool `{}` is disabled", call.name), false);
    }
    let mut request = match tool.permission(&call.arguments) {
        Ok(request) => PermissionRequest {
            originating_trust: origin.trust(),
            ..request
        },
        Err(err) => {
            inner.emit(RuntimeEvent::ToolFinished {
                session: session.id.clone(),
                call: call.id.clone(),
                name: call.name.clone(),
                ok: false,
                summary: err.to_string(),
                content: err.to_string(),
                resource: String::new(),
                arguments: arguments_text.clone(),
                cwd: cwd.clone(),
                duration_ms: elapsed(),
            });
            return (err.to_string(), false);
        }
    };

    // Tools report workspace-relative (or arbitrary) paths; resolve them against the workspace root
    // so the policy floor and scope checks see a canonical absolute path, and escapes are refused.
    if matches!(request.action, ActionKind::Read | ActionKind::Write) {
        let root = &inner.workspace_root;
        if let Some(resolved) =
            crate::config::resolve_within(root, std::path::Path::new(&request.resource))
        {
            request.resource = resolved.to_string_lossy().into_owned();
        }
    }

    // A delegated session is one step further from the operator, and `max_delegation_depth` is only
    // real if the gate can see that: a child may not keep spawning children.
    let raw = inner.policy().evaluate_at_depth(&request, session.depth());
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
            "trust": origin.as_str(),
            "reason": decision_reason(&decision),
        }),
    );

    // The operator's `!` runs beside whatever the session is already doing, so it may only publish
    // turn state when nothing else owns the turn; a model call owns it by definition.
    let owns_turn = origin == Origin::Model || !session.is_busy();

    match decision {
        Decision::Deny { reason } => {
            inner.emit(RuntimeEvent::ToolFinished {
                session: session.id.clone(),
                call: call.id.clone(),
                name: call.name.clone(),
                ok: false,
                summary: format!("denied: {reason}"),
                content: format!("permission denied: {reason}"),
                resource: request.resource.clone(),
                arguments: arguments_text.clone(),
                cwd: cwd.clone(),
                duration_ms: elapsed(),
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
            if owns_turn {
                session.set_turn(TurnState::WaitingApproval);
            }
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
                resource: request.resource.clone(),
                arguments: arguments_text.clone(),
                cwd: cwd.clone(),
            });

            let allowed = wait_for_approval(rx, cancel).await;
            if cancel.is_cancelled() {
                inner.emit(RuntimeEvent::ToolFinished {
                    session: session.id.clone(),
                    call: call.id.clone(),
                    name: call.name.clone(),
                    ok: false,
                    summary: "cancelled · awaiting approval".into(),
                    content: "cancelled while awaiting approval".into(),
                    resource: request.resource.clone(),
                    arguments: arguments_text.clone(),
                    cwd: cwd.clone(),
                    duration_ms: elapsed(),
                });
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
                    content: "permission denied by operator".into(),
                    resource: request.resource.clone(),
                    arguments: arguments_text.clone(),
                    cwd: cwd.clone(),
                    duration_ms: elapsed(),
                });
                return ("permission denied by operator".into(), false);
            }
            if owns_turn {
                session.set_turn(TurnState::Tooling);
            }
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
        serde_json::json!({"call": call.id.as_str(), "name": call.name, "arguments": call.arguments}),
    );

    let summary = summarize_arguments(&call.arguments);
    inner.emit(RuntimeEvent::ToolStarted {
        session: session.id.clone(),
        call: call.id.clone(),
        name: call.name.clone(),
        summary: summary.clone(),
        resource: request.resource.clone(),
        arguments: arguments_text.clone(),
        cwd: cwd.clone(),
    });
    if owns_turn {
        session.set_turn(TurnState::Tooling);
    }
    if let Some(run) = agent {
        run.agent.set_state(AgentState::Running);
        inner.publish_agent(&run.agent);
    }

    let mut context = inner.tool_context_for(&session.id);
    // Tools whose subject is the running system (delegation) need the live runtime, not a file.
    context.runtime = Some(Arc::clone(inner));
    let arguments = call.arguments.clone();
    let handle = tool.clone();
    let mut blocking = tokio::task::spawn_blocking(move || handle.run(&arguments, &context));
    let joined = tokio::select! {
        joined = &mut blocking => joined,
        _ = async {
            while !cancel.is_cancelled() {
                tokio::time::sleep(std::time::Duration::from_millis(50)).await;
            }
        } => {
            // The turn no longer waits on the tool, and the blocking task is dropped; a
            // tool that honors its stop-hook stops, a stray one finishes unknown and its
            // result is discarded.
            blocking.abort();
            inner.journal(
                &session.id,
                RecordKind::ToolResult,
                serde_json::json!({
                    "call": call.id.as_str(),
                    "name": call.name,
                    "ok": false,
                    "content": "cancelled while running",
                }),
            );
            return ("cancelled while running".to_owned(), false);
        }
    };

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

    // What is journaled and sent next is what the model will be held to: the digest. The full text
    // stays on disk under the session's spool, named by the handle appended to it.
    let content = inner.spool_output(&session.id, call.id.as_str(), content);

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
        content: truncate(&content, 8_000),
        resource: request.resource.clone(),
        arguments: arguments_text.clone(),
        cwd: cwd.clone(),
        duration_ms: elapsed(),
    });

    (content, ok)
}

async fn wait_for_approval(
    receiver: oneshot::Receiver<bool>,
    cancel: &vikram_contracts::CancellationToken,
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
    cancel: &vikram_contracts::CancellationToken,
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

#[cfg(test)]
mod tests {
    use super::parse_question;
    use super::*;
    use crate::config::{Config, ContextConfig};
    use crate::provider::{Provider, ProviderStream, ToolSpec};
    use crate::runtime::Runtime;
    use crate::tools::{Tool, ToolContext, ToolOutput};
    use std::collections::VecDeque;
    use std::path::Path;
    use std::sync::Mutex;
    use tokio::sync::broadcast;
    use vikram_contracts::{ActionKind, PermissionRequest, SessionId, SideEffect, VikramError};

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

    /// What a connected interface is handed, in the order it is handed over: committed speech, the
    /// row for a tool that started, the row for the tool that finished, the result that re-enters
    /// the conversation.
    #[derive(Debug, Clone, PartialEq, Eq)]
    enum Seen {
        Speech(String),
        ToolStarted(String),
        ToolFinished(String, bool),
        ToolResult(ToolCallId, String),
    }

    /// A provider that replays one canned stream per request. Running out of script yields an empty
    /// stream, which ends the turn the same way a model that says nothing more would.
    ///
    /// It also keeps what each request contained, because the only context worth asserting about is
    /// the one that was actually sent.
    struct ScriptedProvider {
        script: Mutex<VecDeque<Vec<ProviderEvent>>>,
        requests: Mutex<Vec<Vec<Message>>>,
    }

    impl ScriptedProvider {
        fn new(batches: Vec<Vec<ProviderEvent>>) -> Self {
            Self {
                script: Mutex::new(batches.into()),
                requests: Mutex::new(Vec::new()),
            }
        }

        fn taken(&self) -> Vec<Vec<Message>> {
            self.requests.lock().expect("requests lock").clone()
        }
    }

    impl Provider for ScriptedProvider {
        fn name(&self) -> &str {
            "scripted"
        }

        fn model(&self) -> &str {
            "test-model"
        }

        fn context_window(&self) -> Option<u64> {
            Some(8192)
        }

        fn stream(&self, request: CompletionRequest) -> ProviderStream {
            self.requests
                .lock()
                .expect("requests lock")
                .push(request.messages);
            let batch = self
                .script
                .lock()
                .expect("script lock")
                .pop_front()
                .unwrap_or_default();
            Box::pin(futures_util::stream::iter(
                batch.into_iter().map(Ok::<_, VikramError>),
            ))
        }
    }

    /// A tool that declares a plain read, so the gate allows it in a temporary workspace, and whose
    /// only effect is to say it ran.
    struct ProbeTool;

    impl Tool for ProbeTool {
        fn spec(&self) -> ToolSpec {
            ToolSpec {
                name: "probe".into(),
                description: "records that it ran".into(),
                schema: serde_json::json!({"type": "object", "properties": {}}),
            }
        }

        fn permission(&self, _args: &serde_json::Value) -> Result<PermissionRequest, VikramError> {
            Ok(PermissionRequest::new(
                ActionKind::Read,
                "probe.run",
                "probe",
                SideEffect::None,
                Trust::Model,
            ))
        }

        fn run(
            &self,
            _args: &serde_json::Value,
            _ctx: &ToolContext,
        ) -> Result<ToolOutput, VikramError> {
            Ok(ToolOutput {
                content: "probe: done".into(),
                ok: true,
            })
        }
    }

    /// `VIKRAM_HOME` is process-global, so a test that points it at a temporary directory holds the
    /// shared guard for its whole body.
    fn isolated_home(dir: &Path) -> std::sync::MutexGuard<'static, ()> {
        let guard = crate::config::lock_security_env();
        unsafe { std::env::set_var("VIKRAM_HOME", dir.join("home")) };
        guard
    }

    fn config_for(dir: &Path) -> Config {
        Config {
            model: "test-model".into(),
            workspace_root: Some(dir.to_path_buf()),
            ..Config::default()
        }
    }

    /// The first session, named by the operator rather than left for the runtime to name.
    ///
    /// A session with no human-chosen title spends one model request on naming itself as soon as the
    /// first real message arrives. These tests script the exact number of requests a turn makes, so
    /// that request would be taken out of the script and land on the wrong turn — a naming call is
    /// not part of what a turn is being tested to do.
    fn named_session(handle: &crate::runtime::RuntimeHandle) -> vikram_contracts::SessionId {
        let id = handle.snapshot().sessions[0].id.clone();
        handle
            .rename_session(&id, "Probe session".into())
            .expect("name the session");
        id
    }

    /// Drain one turn's events into the sequence the interface would draw, stopping at the end of
    /// the turn. Five seconds bounds a hung loop instead of letting the test hang with it.
    async fn record(events: &mut broadcast::Receiver<RuntimeEvent>) -> Vec<Seen> {
        let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(5);
        let mut seen = Vec::new();
        loop {
            if tokio::time::Instant::now() >= deadline {
                panic!("the turn never finished; saw {seen:?}");
            }
            let Ok(Ok(event)) =
                tokio::time::timeout(std::time::Duration::from_millis(100), events.recv()).await
            else {
                continue;
            };
            match event {
                RuntimeEvent::MessageCommitted { message, .. } => match message.role {
                    Role::Assistant => {
                        for block in &message.blocks {
                            if let Block::Text { text, .. } = block {
                                seen.push(Seen::Speech(text.clone()));
                            }
                        }
                    }
                    Role::Tool => {
                        for block in &message.blocks {
                            if let Block::ToolResult {
                                call, content, ok, ..
                            } = block
                            {
                                assert!(*ok, "the probe tool always succeeds");
                                seen.push(Seen::ToolResult(call.clone(), content.clone()));
                            }
                        }
                    }
                    _ => {}
                },
                RuntimeEvent::ToolStarted { name, .. } => seen.push(Seen::ToolStarted(name)),
                RuntimeEvent::ToolFinished { name, ok, .. } => {
                    seen.push(Seen::ToolFinished(name, ok))
                }
                RuntimeEvent::TurnFinished { .. } => return seen,
                _ => {}
            }
        }
    }

    /// Run one turn against a scripted provider. Returns the published order and the durable
    /// conversation the next request would carry.
    #[allow(clippy::await_holding_lock)]
    async fn run_scripted(script: Vec<Vec<ProviderEvent>>) -> (Vec<Seen>, Vec<Message>) {
        let dir = tempfile::tempdir().expect("temp dir");
        let _home = isolated_home(dir.path());
        let runtime = Runtime::start_with_provider(
            config_for(dir.path()),
            Arc::new(ScriptedProvider::new(script)),
        )
        .await
        .expect("runtime starts");
        let handle = runtime.handle();
        handle.inner.tools.register(Box::new(ProbeTool));
        let mut events = handle.events();
        let session = named_session(&handle);
        handle
            .submit(&session, "audit the code".into())
            .expect("the turn is accepted");
        let seen = record(&mut events).await;
        let messages = handle
            .inner
            .session(&session)
            .expect("the session survives the turn")
            .messages();
        (seen, messages)
    }

    #[tokio::test]
    async fn a_turn_writes_a_durable_usage_ledger_entry() {
        let dir = tempfile::tempdir().expect("temp dir");
        let _home = isolated_home(dir.path());
        let runtime = Runtime::start_with_provider(
            config_for(dir.path()),
            Arc::new(ScriptedProvider::new(vec![vec![
                ProviderEvent::TextDelta("done".into()),
                ProviderEvent::Finished {
                    input_tokens: Some(10),
                    output_tokens: Some(8),
                },
            ]])),
        )
        .await
        .expect("runtime starts");
        let handle = runtime.handle();
        let mut events = handle.events();
        let session = named_session(&handle);
        handle
            .submit(&session, "hello".into())
            .expect("the turn is accepted");
        let _ = record(&mut events).await;

        let rows = handle
            .inner
            .store
            .list_usage(&session)
            .expect("the ledger is readable");
        assert_eq!(rows.len(), 1, "one turn, one accounting row");
        let row = &rows[0];
        assert!(row.measured, "the provider reported these figures");
        assert_eq!(row.prompt_tokens, 10);
        assert_eq!(row.completion_tokens, 8);
        assert_eq!(row.total_tokens, 18);
        assert_eq!(row.model, "test-model");

        let totals = handle.inner.store.usage_totals(&session).unwrap();
        assert_eq!(totals.requests, 1);
        assert_eq!(totals.measured_requests, 1);
        assert_eq!(totals.total_tokens, 18);
    }

    #[tokio::test]
    async fn an_unreported_turn_is_still_accounted_but_not_marked_measured() {
        let dir = tempfile::tempdir().expect("temp dir");
        let _home = isolated_home(dir.path());
        let runtime = Runtime::start_with_provider(
            config_for(dir.path()),
            Arc::new(ScriptedProvider::new(vec![vec![
                ProviderEvent::TextDelta("a reasonably long answer to estimate".into()),
                ProviderEvent::Finished {
                    input_tokens: None,
                    output_tokens: None,
                },
            ]])),
        )
        .await
        .expect("runtime starts");
        let handle = runtime.handle();
        let mut events = handle.events();
        let session = named_session(&handle);
        handle
            .submit(&session, "hello".into())
            .expect("the turn is accepted");
        let _ = record(&mut events).await;

        let rows = handle.inner.store.list_usage(&session).unwrap();
        assert_eq!(rows.len(), 1);
        assert!(
            !rows[0].measured,
            "no provider figure, so this is an estimate"
        );
        assert!(
            rows[0].total_tokens > 0,
            "the estimator still produces a non-zero, honestly-labelled count"
        );
        assert_eq!(
            handle
                .inner
                .store
                .usage_totals(&session)
                .unwrap()
                .measured_requests,
            0,
            "an estimated turn must not inflate the measured count"
        );
    }

    #[tokio::test]
    async fn speech_arriving_before_a_tool_call_is_committed_before_the_call_runs() {
        let call = ToolCall::new("probe", serde_json::json!({}));
        let call_id = call.id.clone();
        let (seen, messages) = run_scripted(vec![
            vec![
                ProviderEvent::TextDelta("The security review is running".into()),
                ProviderEvent::TextDelta(" in the background.".into()),
                ProviderEvent::ToolCall(call),
                ProviderEvent::Finished {
                    input_tokens: Some(10),
                    output_tokens: Some(8),
                },
            ],
            vec![
                ProviderEvent::TextDelta("It found nothing.".into()),
                ProviderEvent::Finished {
                    input_tokens: None,
                    output_tokens: None,
                },
            ],
        ])
        .await;

        assert_eq!(
            seen,
            vec![
                Seen::Speech("The security review is running in the background.".into()),
                Seen::ToolStarted("probe".into()),
                Seen::ToolFinished("probe".into(), true),
                Seen::ToolResult(call_id, "probe: done".into()),
                Seen::Speech("It found nothing.".into()),
            ],
            "the first sentence must be published before the tool row, not folded into the answer"
        );

        // Two turns of speech, one result, in order: the second sentence is not appended to the
        // first, and neither is repeated.
        let roles: Vec<Role> = messages.iter().map(|m| m.role).collect();
        assert_eq!(
            roles,
            vec![Role::User, Role::Assistant, Role::Tool, Role::Assistant]
        );
        let speech: Vec<String> = messages
            .iter()
            .filter(|m| m.role == Role::Assistant)
            .map(Message::text_body)
            .collect();
        assert_eq!(
            speech,
            vec![
                "The security review is running in the background.".to_owned(),
                "It found nothing.".to_owned(),
            ]
        );
    }

    #[tokio::test]
    async fn every_batch_that_calls_tools_commits_its_own_sentence_first() {
        let first = ToolCall::new("probe", serde_json::json!({"step": 1}));
        let first_id = first.id.clone();
        let second = ToolCall::new("probe", serde_json::json!({"step": 2}));
        let second_id = second.id.clone();
        let (seen, messages) = run_scripted(vec![
            vec![
                ProviderEvent::TextDelta("Checking the dependencies.".into()),
                ProviderEvent::ToolCall(first),
                ProviderEvent::Finished {
                    input_tokens: None,
                    output_tokens: None,
                },
            ],
            vec![
                ProviderEvent::TextDelta("Checking the tests.".into()),
                ProviderEvent::ToolCall(second),
                ProviderEvent::Finished {
                    input_tokens: None,
                    output_tokens: None,
                },
            ],
            vec![
                ProviderEvent::TextDelta("Both are clean.".into()),
                ProviderEvent::Finished {
                    input_tokens: None,
                    output_tokens: None,
                },
            ],
        ])
        .await;

        assert_eq!(
            seen,
            vec![
                Seen::Speech("Checking the dependencies.".into()),
                Seen::ToolStarted("probe".into()),
                Seen::ToolFinished("probe".into(), true),
                Seen::ToolResult(first_id, "probe: done".into()),
                Seen::Speech("Checking the tests.".into()),
                Seen::ToolStarted("probe".into()),
                Seen::ToolFinished("probe".into(), true),
                Seen::ToolResult(second_id, "probe: done".into()),
                Seen::Speech("Both are clean.".into()),
            ]
        );
        let speech: Vec<String> = messages
            .iter()
            .filter(|m| m.role == Role::Assistant)
            .map(Message::text_body)
            .collect();
        assert_eq!(
            speech,
            vec![
                "Checking the dependencies.".to_owned(),
                "Checking the tests.".to_owned(),
                "Both are clean.".to_owned(),
            ]
        );
    }

    #[tokio::test]
    async fn a_turn_that_ends_in_speech_commits_it_once() {
        let (seen, messages) = run_scripted(vec![vec![
            ProviderEvent::TextDelta("Nothing to do.".into()),
            ProviderEvent::Finished {
                input_tokens: None,
                output_tokens: None,
            },
        ]])
        .await;

        assert_eq!(seen, vec![Seen::Speech("Nothing to do.".into())]);
        let roles: Vec<Role> = messages.iter().map(|m| m.role).collect();
        assert_eq!(roles, vec![Role::User, Role::Assistant]);
    }

    #[tokio::test]
    async fn a_batch_with_no_text_publishes_no_speech() {
        let call = ToolCall::new("probe", serde_json::json!({}));
        let (seen, _) = run_scripted(vec![
            vec![
                ProviderEvent::ToolCall(call),
                ProviderEvent::Finished {
                    input_tokens: None,
                    output_tokens: None,
                },
            ],
            vec![ProviderEvent::Finished {
                input_tokens: None,
                output_tokens: None,
            }],
        ])
        .await;

        assert!(
            !seen.iter().any(|item| matches!(item, Seen::Speech(_))),
            "a silent tool call must not gain a progress sentence: {seen:?}"
        );
    }

    /// One exchange the way a live transcript holds it: an instruction, an assistant turn carrying a
    /// call, then the result that answers that call. A fold that split these is an API error, not a
    /// saving.
    fn exchange(prompt: &str, reply: &str) -> Vec<Message> {
        let call = ToolCall::new("probe", serde_json::json!({ "note": prompt }));
        let id = call.id.clone();
        vec![
            Message::user(prompt.to_owned()),
            Message {
                role: Role::Assistant,
                blocks: vec![
                    Block::Text {
                        text: format!("looking into {prompt}"),
                        trust: Trust::Model,
                    },
                    Block::ToolCall { call },
                ],
            },
            Message::tool_result(id, reply.to_owned(), true),
        ]
    }

    /// Drain a whole turn, keeping the context events rather than the speech rows.
    async fn drain(
        events: &mut broadcast::Receiver<RuntimeEvent>,
    ) -> (
        Option<(usize, usize, u64, u64)>,
        Option<crate::types::ContextMeter>,
        Vec<(usize, u64)>,
    ) {
        let mut compacted = None;
        let mut meter = None;
        let mut pruned = Vec::new();
        let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(5);
        loop {
            if tokio::time::Instant::now() >= deadline {
                panic!("the turn never finished");
            }
            let Ok(Ok(event)) =
                tokio::time::timeout(std::time::Duration::from_millis(100), events.recv()).await
            else {
                continue;
            };
            match event {
                RuntimeEvent::ContextCompacted {
                    folded,
                    kept,
                    before_tokens,
                    after_tokens,
                    ..
                } => compacted = Some((folded, kept, before_tokens, after_tokens)),
                RuntimeEvent::ContextMetered { meter: m, .. } => meter = Some(m),
                RuntimeEvent::ContextPruned {
                    replaced,
                    saved_tokens,
                    ..
                } => pruned.push((replaced, saved_tokens)),
                RuntimeEvent::TurnFinished { .. } => return (compacted, meter, pruned),
                _ => {}
            }
        }
    }

    /// The whole context path, through the loop that actually runs: a transcript that no longer fits
    /// is folded before the request leaves, the fold is journalled, the request the provider receives
    /// carries the summary and keeps every tool call paired with its result, and the interface is
    /// told what the request was made of.
    #[tokio::test]
    #[allow(clippy::await_holding_lock)]
    async fn an_over_budget_transcript_folds_before_the_request_leaves() {
        let dir = tempfile::tempdir().expect("temp dir");
        let _home = isolated_home(dir.path());
        let provider = Arc::new(ScriptedProvider::new(vec![vec![ProviderEvent::Finished {
            input_tokens: Some(1_200),
            output_tokens: Some(20),
        }]]));
        let config = Config {
            model: "test-model".into(),
            workspace_root: Some(dir.path().to_path_buf()),
            context_window: Some(12_000),
            max_output_tokens: Some(200),
            context: ContextConfig {
                trigger_percent: 10,
                keep_recent: 6,
                ..ContextConfig::default()
            },
            ..Config::default()
        };
        let runtime = Runtime::start_with_provider(config, provider.clone())
            .await
            .expect("runtime starts");
        let handle = runtime.handle();
        let session_id = named_session(&handle);
        let session = handle
            .inner
            .session(&session_id)
            .expect("the session exists");
        for index in 0..12 {
            for message in exchange(
                &format!(
                    "inspect item {index} and report what it reads, writes and returns in this workspace"
                ),
                &format!("item {index} reads a file and returns its first line"),
            ) {
                session.push_message(message, now_ms());
            }
        }
        let seeded = session.messages().len();
        assert_eq!(seeded, 36);

        let mut events = handle.events();
        handle
            .submit(&session_id, "summarise everything found".into())
            .expect("the turn is accepted");
        let (compacted, meter, _) = drain(&mut events).await;

        let (folded, kept, before_tokens, after_tokens) =
            compacted.expect("an over-budget transcript folds before the request goes out");
        assert!(folded >= 6, "the fold must clear a real span, got {folded}");
        assert_eq!(kept, 6, "the tail is kept verbatim");
        assert!(
            after_tokens < before_tokens,
            "a fold that saves nothing is not a fold: {before_tokens} -> {after_tokens}"
        );

        let requests = provider.taken();
        assert_eq!(requests.len(), 1, "one request went out");
        let sent = &requests[0];
        assert!(
            sent.iter()
                .any(|m| crate::compaction::is_compaction_summary(m)),
            "the summary travels with the request"
        );
        assert!(
            crate::compaction::pairing_is_consistent(sent),
            "a fold must not split a tool call from its result"
        );
        assert!(
            sent.last().is_some_and(|m| m.role == Role::User),
            "the live instruction is the newest thing in the request"
        );

        let stored = session.messages().len();
        assert!(
            stored < seeded,
            "the folded transcript replaces the long one: {seeded} -> {stored}"
        );
        // The instruction is durable before the fold runs, so it is already inside the kept tail:
        // summary + tail, nothing dropped on the way in.
        assert_eq!(stored, kept + 1, "summary + kept tail");
        assert!(
            session
                .messages()
                .last()
                .is_some_and(|m| m.role == Role::User),
            "the live instruction survived the fold in stored order"
        );

        let records = handle
            .inner
            .store
            .replay_messages(&session_id)
            .expect("the journal reads back");
        assert!(
            records.iter().any(|r| r.kind == RecordKind::Compaction),
            "the fold is durable, not just in memory"
        );

        let meter = meter.expect("the request is priced part by part");
        assert!(meter.window > 0 && meter.usable > 0);
        assert!(
            !meter.measured,
            "nothing reported a figure before this request"
        );
        assert!(
            meter.parts.iter().any(|p| p.label == "conversation"),
            "the inspector names the parts: {:?}",
            meter.parts
        );

        // The provider's own figure becomes the anchor, so the next estimate trusts it.
        let state = session.context.lock().expect("context lock");
        assert!(state.anchor.is_some(), "reported usage anchored the meter");
        assert!(
            state.estimator.ratio() < 2.5,
            "calibration stayed inside its clamp"
        );
    }

    /// A result too large for the transcript is digested, not discarded: both ends stay in context,
    /// and the middle is readable back from the session's spool through the named handle.
    #[tokio::test]
    #[allow(clippy::await_holding_lock)]
    async fn an_oversized_tool_result_is_spooled_and_read_back() {
        let dir = tempfile::tempdir().expect("temp dir");
        let _home = isolated_home(dir.path());
        let provider = Arc::new(ScriptedProvider::new(vec![vec![ProviderEvent::Finished {
            input_tokens: None,
            output_tokens: None,
        }]]));
        let config = Config {
            model: "test-model".into(),
            workspace_root: Some(dir.path().to_path_buf()),
            context: ContextConfig {
                tool_output_ceiling: 400,
                ..ContextConfig::default()
            },
            ..Config::default()
        };
        let runtime = Runtime::start_with_provider(config, provider)
            .await
            .expect("runtime starts");
        let handle = runtime.handle();
        let session_id = named_session(&handle);

        let body = format!(
            "HEAD\n{}MIDDLE-MARKER {}\nTAIL",
            "a".repeat(2_000),
            "b".repeat(2_000)
        );
        let digested = handle
            .inner
            .spool_output(&session_id, "call-7", body.clone());
        assert!(digested.chars().count() < body.chars().count());
        assert!(
            digested.starts_with("HEAD"),
            "the front of the result survives"
        );
        assert!(
            digested.contains("TAIL"),
            "the back survives too: {digested}"
        );
        assert!(
            digested.contains("output.read(id=\"call-7\""),
            "the digest names how to get the rest: {digested}"
        );

        let tool = handle
            .inner
            .tools
            .get("output.read")
            .expect("output.read is registered");
        let context = handle.inner.tool_context_for(&session_id);
        let read = tool
            .run(
                &serde_json::json!({ "id": "call-7", "offset": 2_005, "limit": 40 }),
                &context,
            )
            .expect("the tool runs");
        assert!(read.ok, "{}", read.content);
        assert!(read.content.contains("MIDDLE-MARKER"), "{}", read.content);
        assert!(
            read.content
                .contains(&format!("of {}", body.chars().count())),
            "the read says how much is there: {}",
            read.content
        );

        // A handle from another session's spool is not addressable here at all.
        let other =
            crate::spill::Spill::for_session(dir.path(), &vikram_contracts::SessionId::generate());
        assert!(other.read("call-7", 0, 40).is_err());
    }

    /// The file tools checkpoint what they overwrite, so an agent edit is never the only copy: the
    /// pre-edit bytes are spooled per-session and `restore_file` can bring them back.
    #[tokio::test]
    #[allow(clippy::await_holding_lock)]
    async fn a_session_file_write_is_checkpointed_and_restorable() {
        let dir = tempfile::tempdir().expect("temp dir");
        let _home = isolated_home(dir.path());
        let runtime = Runtime::start_with_provider(
            config_for(dir.path()),
            Arc::new(ScriptedProvider::new(vec![])),
        )
        .await
        .expect("runtime starts");
        let handle = runtime.handle();
        let session_id = named_session(&handle);
        let mut context = handle.inner.tool_context_for(&session_id);
        context.runtime = Some(Arc::clone(&handle.inner));

        let write = handle
            .inner
            .tools
            .get("fs.write")
            .expect("fs.write is registered");
        write
            .run(
                &serde_json::json!({ "path": "notes/a.txt", "content": "one" }),
                &context,
            )
            .expect("the create write runs");
        let file = dir.path().join("notes/a.txt");
        write
            .run(
                &serde_json::json!({ "path": "notes/a.txt", "content": "two" }),
                &context,
            )
            .expect("the overwrite runs");
        assert_eq!(std::fs::read_to_string(&file).unwrap(), "two");

        let changes = handle.inner.workspace.file_changes(&session_id).unwrap();
        assert_eq!(changes.len(), 2, "both writes are recorded");
        let create = changes
            .iter()
            .find(|c| c.operation == "create")
            .expect("the first write is a create");
        assert!(
            create.checkpoint_path.is_none(),
            "a create has no earlier content to recover"
        );
        let modify = changes
            .iter()
            .find(|c| c.operation == "modify")
            .expect("the overwrite is a modify");
        assert!(
            modify.checkpoint_path.is_some(),
            "an overwrite keeps the bytes it replaced"
        );

        handle
            .inner
            .workspace
            .restore_file(&session_id, &file)
            .expect("the checkpoint restores");
        assert_eq!(
            std::fs::read_to_string(&file).unwrap(),
            "one",
            "recovery returns the pre-overwrite content, not the discarded edit"
        );
    }

    /// Delegation transfers a bounded brief down and a bounded answer back. The child never sees
    /// the parent's transcript, and a chain that would exceed the depth limit is refused.
    #[tokio::test]
    #[allow(clippy::await_holding_lock)]
    async fn a_delegated_agent_starts_from_a_brief_and_answers_its_parent() {
        let dir = tempfile::tempdir().expect("temp dir");
        let _home = isolated_home(dir.path());
        let provider = Arc::new(ScriptedProvider::new(vec![vec![
            ProviderEvent::TextDelta("Reviewed it: the gate is wired before the request.".into()),
            ProviderEvent::Finished {
                input_tokens: None,
                output_tokens: Some(9),
            },
        ]]));
        let config = Config {
            model: "test-model".into(),
            workspace_root: Some(dir.path().to_path_buf()),
            context: ContextConfig {
                delegation_budget_tokens: 200,
                ..ContextConfig::default()
            },
            ..Config::default()
        };
        let runtime = Runtime::start_with_provider(config, provider)
            .await
            .expect("runtime starts");
        let handle = runtime.handle();
        let parent_id = handle.snapshot().sessions[0].id.clone();
        let parent = handle
            .inner
            .session(&parent_id)
            .expect("the parent session");
        for message in exchange(
            "audit the payment module and report the gates it misses",
            "we will check turn.rs before the gate lands",
        ) {
            parent.push_message(message, now_ms());
        }

        let profile = vikram_contracts::AgentId::new("delegatee");
        handle.inner.agents.add(Arc::new(ManagedAgent::from_profile(
            crate::types::AgentProfile {
                id: profile.clone(),
                name: "Rudra".into(),
                role: "reviewer".into(),
                description: "reads code".into(),
                icon: None,
                created_ms: now_ms(),
            },
        )));
        handle
            .spawn_agent(profile, "summarise the findings".into(), parent_id.clone())
            .expect("the agent starts");

        let child_id = handle
            .snapshot()
            .sessions
            .iter()
            .find(|s| s.id != parent_id)
            .expect("a child session")
            .id
            .clone();
        let child = handle.inner.session(&child_id).expect("the child session");

        // The child begins from a brief, not from the parent's transcript.
        let opening = child.messages();
        let brief = opening
            .first()
            .expect("the brief is the first thing the child holds")
            .text_body();
        assert!(
            brief.starts_with("[VIKRAM delegation brief]"),
            "got {brief}"
        );
        assert!(
            brief.contains("audit the payment module and report the gates it misses"),
            "the live instruction crosses verbatim: {brief}"
        );
        assert!(
            child.estimator().estimate_text(&brief) <= 200,
            "the brief respects its budget"
        );
        assert_eq!(
            child.parent().as_ref(),
            Some(&parent_id),
            "lineage in memory"
        );
        assert_eq!(child.depth(), 1, "one step from the operator's session");
        assert_eq!(
            handle
                .inner
                .store
                .session_lineage(&child_id)
                .expect("the store reads"),
            Some((parent_id.clone(), 1)),
            "lineage is durable"
        );

        // The answer comes back to the parent when the child's turn is done.
        let mut arrived = None;
        for _ in 0..200 {
            let messages = parent.messages();
            if let Some(found) = messages
                .iter()
                .rev()
                .map(|m| m.text_body())
                .find(|text| text.starts_with("[VIKRAM delegation result]"))
            {
                arrived = Some(found);
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        let answer = arrived.expect("the delegated answer reaches the parent");
        assert!(
            answer.contains("the gate is wired before the request"),
            "the child's statement arrives: {answer}"
        );
        assert!(
            !answer.contains("audit the payment module"),
            "the parent's own transcript is not echoed back: {answer}"
        );

        // A chain deeper than the floor is refused, not silently allowed to nest.
        child.set_depth(handle.inner.policy().max_delegation_depth());
        let refused = handle.spawn_agent(
            vikram_contracts::AgentId::new("delegatee"),
            "deeper".into(),
            child_id.clone(),
        );
        match refused {
            Err(crate::error::RuntimeError::InvalidState(reason)) => {
                assert!(reason.contains("delegation depth"), "{reason}")
            }
            other => panic!("expected a depth refusal, got {other:?}"),
        }
    }
    /// A tool that declares a plain read and only says it ran, under whatever name the test needs
    /// — the growth pipeline counts distinct tools, so one turn needs more than `probe`.
    struct NamedProbeTool(&'static str);

    impl Tool for NamedProbeTool {
        fn spec(&self) -> ToolSpec {
            ToolSpec {
                name: self.0.into(),
                description: "records that it ran".into(),
                schema: serde_json::json!({"type": "object", "properties": {}}),
            }
        }

        fn permission(&self, _args: &serde_json::Value) -> Result<PermissionRequest, VikramError> {
            Ok(PermissionRequest::new(
                ActionKind::Read,
                "probe.run",
                self.0,
                SideEffect::None,
                Trust::Model,
            ))
        }

        fn run(
            &self,
            _args: &serde_json::Value,
            _ctx: &ToolContext,
        ) -> Result<ToolOutput, VikramError> {
            Ok(ToolOutput {
                content: format!("{}: done", self.0),
                ok: true,
            })
        }
    }

    fn finished() -> ProviderEvent {
        ProviderEvent::Finished {
            input_tokens: None,
            output_tokens: None,
        }
    }

    /// A tool that stops the turn it is running in. The operator hitting cancel between two calls of
    /// one batch is the only way the loop can walk away from a call the model actually made.
    struct CancellingTool {
        handle: crate::runtime::RuntimeHandle,
        session: SessionId,
    }

    impl Tool for CancellingTool {
        fn spec(&self) -> ToolSpec {
            ToolSpec {
                name: "stop-turn".into(),
                description: "cancels the session it runs in".into(),
                schema: serde_json::json!({"type": "object", "properties": {}}),
            }
        }

        fn permission(&self, _args: &serde_json::Value) -> Result<PermissionRequest, VikramError> {
            Ok(PermissionRequest::new(
                ActionKind::Read,
                "probe.run",
                "stop-turn",
                SideEffect::None,
                Trust::Model,
            ))
        }

        fn run(
            &self,
            _args: &serde_json::Value,
            _ctx: &ToolContext,
        ) -> Result<ToolOutput, VikramError> {
            let _ = self.handle.cancel(&self.session);
            Ok(ToolOutput {
                content: "cancelled the turn".into(),
                ok: true,
            })
        }
    }

    /// Wait for the turn to end without reading its rows: a cancelled call answers with a failure,
    /// and `record` asserts that every probe succeeded.
    async fn run_to_end(events: &mut broadcast::Receiver<RuntimeEvent>) {
        let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(5);
        loop {
            if tokio::time::Instant::now() >= deadline {
                panic!("the turn never finished");
            }
            if let Ok(Ok(RuntimeEvent::TurnFinished { .. })) =
                tokio::time::timeout(std::time::Duration::from_millis(100), events.recv()).await
            {
                return;
            }
        }
    }

    #[tokio::test]
    async fn a_cancelled_batch_still_answers_the_call_that_never_ran() {
        let dir = tempfile::tempdir().expect("temp dir");
        let _home = isolated_home(dir.path());
        let stop = ToolCall::new("stop-turn", serde_json::json!({}));
        let unrun = ToolCall::new("probe-read", serde_json::json!({}));
        let unrun_id = unrun.id.clone();
        let provider = Arc::new(ScriptedProvider::new(vec![vec![
            ProviderEvent::ToolCall(stop),
            ProviderEvent::ToolCall(unrun),
            finished(),
        ]]));
        let runtime = Runtime::start_with_provider(config_for(dir.path()), provider)
            .await
            .expect("runtime starts");
        let handle = runtime.handle();
        let session = named_session(&handle);
        handle.inner.tools.register(Box::new(CancellingTool {
            handle: handle.clone(),
            session: session.clone(),
        }));
        let mut events = handle.events();
        handle
            .submit(&session, "stop this batch".into())
            .expect("the turn is accepted");
        run_to_end(&mut events).await;

        let messages = handle
            .inner
            .session(&session)
            .expect("the session survives the turn")
            .messages();
        let answered: Vec<String> = messages
            .iter()
            .flat_map(|m| m.blocks.iter())
            .filter_map(|block| match block {
                Block::ToolResult { call, content, .. } => Some(format!("{call}: {content}")),
                _ => None,
            })
            .collect();
        assert!(
            answered
                .iter()
                .any(|row| row.starts_with(&format!("{unrun_id}:"))),
            "a call the model made must not vanish from the transcript: {answered:?}"
        );
        assert!(
            answered
                .iter()
                .any(|row| row.starts_with(&format!("{unrun_id}:")) && row.contains("cancel")),
            "and it says so honestly: {answered:?}"
        );
    }

    /// A crash between recording a call and recording its result is the case replay has to repair,
    /// and it can only do that if the call row says which call it was.
    #[tokio::test]
    async fn the_journal_names_the_call_it_started() {
        let dir = tempfile::tempdir().expect("temp dir");
        let _home = isolated_home(dir.path());
        let call = ToolCall::new("probe", serde_json::json!({}));
        let call_id = call.id.clone();
        let runtime = Runtime::start_with_provider(
            config_for(dir.path()),
            Arc::new(ScriptedProvider::new(vec![vec![
                ProviderEvent::ToolCall(call),
                finished(),
            ]])),
        )
        .await
        .expect("runtime starts");
        let handle = runtime.handle();
        handle.inner.tools.register(Box::new(ProbeTool));
        let mut events = handle.events();
        let session = named_session(&handle);
        handle
            .submit(&session, "audit the code".into())
            .expect("the turn is accepted");
        run_to_end(&mut events).await;

        let records = handle
            .inner
            .store
            .replay_messages(&session)
            .expect("the journal reads back");
        let journaled: Vec<String> = records
            .iter()
            .filter(|r| r.kind == RecordKind::ToolCall)
            .map(|r| {
                r.payload
                    .get("call")
                    .and_then(|v| v.as_str())
                    .unwrap_or_default()
                    .to_owned()
            })
            .collect();
        assert_eq!(
            journaled,
            vec![call_id.as_str().to_owned()],
            "the call row carries the id the result row answers"
        );
    }

    /// Run one scripted turn over two tools and hand back what the runtime kept.
    #[allow(clippy::await_holding_lock)]
    async fn grow_through_tools(
        scripts: Vec<Vec<ProviderEvent>>,
        tool_names: &[&'static str],
    ) -> (Arc<ScriptedProvider>, crate::runtime::RuntimeHandle) {
        let dir = tempfile::tempdir().expect("temp dir");
        let _home = isolated_home(dir.path());
        let provider = Arc::new(ScriptedProvider::new(scripts));
        let model: Arc<dyn Provider> = provider.clone();
        let runtime = Runtime::start_with_provider(config_for(dir.path()), model)
            .await
            .expect("runtime starts");
        let handle = runtime.handle();
        for name in tool_names {
            handle.inner.tools.register(Box::new(NamedProbeTool(name)));
        }
        let mut events = handle.events();
        let session = named_session(&handle);
        handle
            .submit(&session, "audit the workspace".into())
            .expect("the turn is accepted");
        let _ = record(&mut events).await;
        (provider, handle)
    }

    #[tokio::test]
    async fn a_turn_that_used_tools_teaches_the_runtime_without_anyone_being_asked() {
        let first = ToolCall::new("probe-read", serde_json::json!({}));
        let second = ToolCall::new("probe-write", serde_json::json!({}));
        let (provider, handle) = grow_through_tools(
            vec![
                vec![ProviderEvent::ToolCall(first), finished()],
                vec![ProviderEvent::ToolCall(second), finished()],
                vec![
                    ProviderEvent::TextDelta("the workspace is clean".into()),
                    finished(),
                ],
            ],
            &["probe-read", "probe-write"],
        )
        .await;

        let experiences = handle
            .inner
            .evolution
            .lock()
            .expect("evolution lock")
            .experience
            .count()
            .expect("the store reads");
        assert_eq!(experiences, 1, "the turn became one experience");

        let pending = handle.inner.learning.pending_proposals();
        assert_eq!(
            pending.len(),
            1,
            "a two-tool success becomes exactly one staged proposal: {pending:?}"
        );
        assert_eq!(pending[0].kind, "skill");
        assert_eq!(
            pending[0].status, "pending",
            "the operator decides, not the model"
        );

        let audit = handle.inner.learning.audit_log();
        assert_eq!(audit.len(), 1);
        assert_eq!(audit[0].outcome, "staged");

        assert_eq!(
            provider.taken().len(),
            3,
            "the default config spends no tokens on a background review"
        );
    }

    /// A one-shot process that exits when the turn is announced finished would otherwise drop the
    /// review it started, so the runtime counts reviews in flight and can be waited on.
    #[tokio::test]
    #[allow(clippy::await_holding_lock)]
    async fn a_review_in_flight_is_counted_until_it_lands() {
        let dir = tempfile::tempdir().expect("temp dir");
        let _home = isolated_home(dir.path());
        let provider = Arc::new(ScriptedProvider::new(vec![vec![
            ProviderEvent::TextDelta(
                r#"[{"kind":"memory","target":"memory","action":"add","key":"probe","content":"the probe tools always succeed","reason":"observed"}]"#.into(),
            ),
            finished(),
        ]]));
        let model: Arc<dyn Provider> = provider.clone();
        let runtime = Runtime::start_with_provider(config_for(dir.path()), model)
            .await
            .expect("runtime starts");
        let handle = runtime.handle();
        let session = named_session(&handle);

        handle.trigger_learning_review(&session, "user: run the probe\ntools: probe");
        assert!(
            handle.learning_busy(),
            "a review is counted the moment it is started, not when it is polled"
        );
        assert!(
            handle
                .wait_for_learning(std::time::Duration::from_secs(5))
                .await,
            "and the runtime says when it is done learning"
        );
        assert!(!handle.learning_busy());
        let staged = handle.inner.learning.pending_proposals();
        assert!(
            staged
                .iter()
                .any(|p| p.kind == "memory" && p.content == "the probe tools always succeed"),
            "the review's answer reached the approval surface: {staged:?}"
        );
    }

    /// The paid half, end to end: a turn that earns a review asks the model, and the model's
    /// answer goes through the same policy gate as one the runtime derived itself.
    #[tokio::test]
    #[allow(clippy::await_holding_lock)]
    async fn an_earning_turn_asks_the_model_and_the_answer_lands_on_the_same_gate() {
        let dir = tempfile::tempdir().expect("temp dir");
        let _home = isolated_home(dir.path());
        let call = ToolCall::new("probe-read", serde_json::json!({}));
        let provider = Arc::new(ScriptedProvider::new(vec![
            vec![ProviderEvent::ToolCall(call), finished()],
            vec![
                ProviderEvent::TextDelta("the workspace is clean".into()),
                finished(),
            ],
            vec![
                ProviderEvent::TextDelta(
                    r#"[{"kind":"memory","target":"user","action":"add","key":"pref","content":"prefers pnpm","reason":"stated twice"}]"#.into(),
                ),
                finished(),
            ],
        ]));
        let model: Arc<dyn Provider> = Arc::clone(&provider) as Arc<dyn Provider>;
        let config = Config {
            model: "test-model".into(),
            workspace_root: Some(dir.path().to_path_buf()),
            learning_review_every: 1,
            ..Config::default()
        };
        let runtime = Runtime::start_with_provider(config, model)
            .await
            .expect("runtime starts");
        let handle = runtime.handle();
        handle
            .inner
            .tools
            .register(Box::new(NamedProbeTool("probe-read")));
        let mut events = handle.events();
        let session = named_session(&handle);
        handle
            .submit(&session, "audit the workspace".into())
            .expect("the turn is accepted");
        let _ = record(&mut events).await;

        let mut staged = Vec::new();
        for _ in 0..100 {
            let pending = handle.inner.learning.pending_proposals();
            if !pending.is_empty() {
                staged = pending;
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        assert_eq!(
            staged.len(),
            1,
            "the review's proposal waits for the operator like any other"
        );
        assert_eq!(staged[0].kind, "memory");
        assert_eq!(staged[0].content, "prefers pnpm");
        assert_eq!(
            handle
                .inner
                .memory
                .list("user")
                .expect("memory reads")
                .iter()
                .filter(|(_, value)| value == "prefers pnpm")
                .count(),
            0,
            "a model's sentence is not a memory until a human says yes"
        );
        assert_eq!(
            provider.taken().len(),
            3,
            "two turn requests and the review"
        );
    }

    #[tokio::test]
    async fn a_chat_only_turn_is_not_evidence_of_anything() {
        let (_provider, handle) = grow_through_tools(
            vec![vec![
                ProviderEvent::TextDelta("here is what I think".into()),
                finished(),
            ]],
            &[],
        )
        .await;
        let experiences = handle
            .inner
            .evolution
            .lock()
            .expect("evolution lock")
            .experience
            .count()
            .expect("the store reads");
        assert_eq!(experiences, 0, "nothing was done, so nothing was learned");
        assert!(handle.inner.learning.pending_proposals().is_empty());
    }

    #[tokio::test]
    async fn an_operator_that_disabled_learning_grows_nothing() {
        use crate::learning::LearningPolicy;
        let first = ToolCall::new("probe-read", serde_json::json!({}));
        let second = ToolCall::new("probe-write", serde_json::json!({}));
        let dir = tempfile::tempdir().expect("temp dir");
        let _home = isolated_home(dir.path());
        let provider = Arc::new(ScriptedProvider::new(vec![
            vec![ProviderEvent::ToolCall(first), finished()],
            vec![ProviderEvent::ToolCall(second), finished()],
            vec![finished()],
        ]));
        let model: Arc<dyn Provider> = provider.clone();
        let runtime = Runtime::start_with_provider(config_for(dir.path()), model)
            .await
            .expect("runtime starts");
        let handle = runtime.handle();
        handle
            .inner
            .tools
            .register(Box::new(NamedProbeTool("probe-read")));
        handle
            .inner
            .tools
            .register(Box::new(NamedProbeTool("probe-write")));
        handle
            .inner
            .learning
            .set_policy_memory(LearningPolicy::Disabled);
        handle
            .inner
            .learning
            .set_policy_skill(LearningPolicy::Disabled);
        let mut events = handle.events();
        let session = named_session(&handle);
        handle
            .submit(&session, "audit the workspace".into())
            .expect("the turn is accepted");
        let _ = record(&mut events).await;

        let experiences = handle
            .inner
            .evolution
            .lock()
            .expect("evolution lock")
            .experience
            .count()
            .expect("the store reads");
        assert_eq!(experiences, 0, "a disabled policy means off, not quiet");
        assert!(handle.inner.learning.pending_proposals().is_empty());
    }
}
