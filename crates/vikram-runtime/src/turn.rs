//! The turn loop.
//!
//! One turn is: provider stream -> assistant text -> zero or more tool calls, each gated by the
//! policy crate -> tool results -> repeat, bounded by a step budget. The loop owns no durable
//! state of its own: every observation is published, every message is journaled, and cancellation
//! is checked between every step.

use futures_util::StreamExt;
use std::collections::HashMap;
use std::sync::Arc;
use std::time::Instant;
use tokio::sync::oneshot;
use vikram_contracts::policy::Decision;
use vikram_contracts::{
    ActionKind, ApprovalId, Block, ImageSource, Message, PermissionMode, PermissionRequest,
    RecordKind, Role, SideEffect, ToolCall, ToolCallId, Trust, TurnId,
};

use crate::bots::ManagedBot;
use crate::learning::TurnGrowth;
use crate::pipeline::{
    ContextBuildPayload, MemoryRecallPayload, ProviderRequestPayload, ResponseCommitPayload,
    ResultCommitPayload, StageContext, StageError, StageFuture, StageHandler, StageOutcome,
    StagePayload, StagePoint, ToolDispatchPayload, TurnEndPayload, UserInputPayload,
};
use crate::provider::{CompletionRequest, ProviderEvent};
use crate::runtime::{PendingApproval, RuntimeInner};
use crate::session::SessionRuntime;
use crate::skills::SkillRegistry;
use crate::types::{
    ApprovalPrompt, BotState, ContextMeter, ContextPart, NoticeLevel, RuntimeEvent, TaskState,
    TurnState, now_ms,
};

/// How often a streaming turn republishes its live rate. Fast enough to read as real time, slow
/// enough that building the metrics snapshot costs nothing measurable.
const LIVE_METRIC_INTERVAL: std::time::Duration = std::time::Duration::from_millis(250);

/// An agent turn carries the task record it must close when it finishes.
pub struct AgentRun {
    pub agent: Arc<ManagedBot>,
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

/// Said to a bot on every turn in its own session, in place of the app's preamble.
///
/// A bot is not the application. It used to be told "You are VIKRAM" on every turn and then
/// introduced itself to the operator as VIKRAM, in a chat whose title carried the bot's own name —
/// so the operator was talking to their own bot and being answered by the app. Here it is told its
/// name, the tags and description the operator gave it, and nothing else about who it is; the app
/// never speaks through it.
fn bot_preamble(profile: &crate::types::BotProfile) -> String {
    let mut text = format!(
        "You are {}, a bot the operator keeps in VIKRAM. You are not VIKRAM: never introduce \
         yourself as VIKRAM or as the app, and never speak on the app's behalf. You are you — the \
         bot named {} — and you say so when a name is needed.",
        profile.name, profile.name
    );
    let tags: Vec<&str> = profile
        .role
        .split(',')
        .map(str::trim)
        .filter(|tag| !tag.is_empty())
        .collect();
    if !tags.is_empty() {
        text.push_str(&format!(" The operator tagged you {}.", tags.join(", ")));
    }
    let description = profile.description.trim();
    if !description.is_empty() {
        text.push_str(&format!(
            " About you, in the operator's words: {description}"
        ));
    }
    // The operator's standing instructions, if they wrote any. They are given the same weight as the
    // identity above: they are what the bot is supposed to do, every turn, without being asked again.
    let soul = profile.soul.trim();
    if !soul.is_empty() {
        text.push_str(&format!(
            " Standing instructions the operator wrote for you — follow them every turn: {soul}"
        ));
    }
    text.push_str(
        " Alongside your tool calls, write one or two short sentences of progress in the present \
         tense: what you are about to do and why, naming the file, command or question involved. \
         Write one before you start a piece of work, and again whenever what you are doing changes. \
         These lines are shown to the operator as your voice between the tool rows, so each must \
         stand alone. Do not narrate every step, and never describe your private reasoning.",
    );
    text
}

/// Extract a likely symbol/token query from the user's request for code-index lookup:
/// the longest word-like token, or "" when nothing looks like an identifier.
fn codeindex_query(request: &str) -> String {
    request
        .split(|c: char| !c.is_alphanumeric() && c != '_')
        .filter(|w| w.len() >= 4 && w.chars().any(|c| c.is_alphabetic()))
        .max_by_key(|w| w.len())
        .unwrap_or("")
        .to_owned()
}

pub(crate) fn spawn_turn(
    inner: Arc<RuntimeInner>,
    session: Arc<SessionRuntime>,
    message: Message,
    agent: Option<AgentRun>,
) {
    // The claim is taken before the task exists, not inside it: `is_busy` and the spawn are
    // separate steps, so two submits can both see an idle session. The loser queues its text —
    // the same slot the queue drain claims through here — rather than starting a second turn.
    if !session.claim_turn() {
        match session.enqueue(message.text_body()) {
            Some(depth) => inner.emit(RuntimeEvent::Notice {
                level: NoticeLevel::Info,
                text: format!("queued ({depth} waiting)"),
            }),
            None => inner.emit(RuntimeEvent::Notice {
                level: NoticeLevel::Warn,
                text: format!(
                    "the follow-up queue is full ({} waiting); the message was not queued",
                    SessionRuntime::MAX_QUEUED_FOLLOW_UPS
                ),
            }),
        }
        return;
    }
    tokio::spawn(async move {
        run_turn(inner, session, message, agent).await;
    });
}

pub(crate) async fn run_turn(
    inner: Arc<RuntimeInner>,
    session: Arc<SessionRuntime>,
    message: Message,
    agent: Option<AgentRun>,
) {
    let turn = TurnId::generate();
    let started = Instant::now();
    let cancel = session.cancel_handle();
    // The claim was taken in `spawn_turn` before this task existed, so a panic that unwinds past
    // `finish` would strand it: the slot stays held, `is_busy` never clears, and every later
    // submit queues behind a turn that no longer exists. The guard is the unwind path `finish`
    // cannot be — armed for the whole turn and disarmed only after `finish` hands the slot back.
    let claim_guard = TurnGuard {
        inner: Arc::clone(&inner),
        session: Arc::clone(&session),
        turn: turn.clone(),
        agent: agent.as_ref().map(|run| AgentRun {
            agent: Arc::clone(&run.agent),
            task_id: run.task_id.clone(),
        }),
        armed: std::cell::Cell::new(true),
    };

    // What this turn actually did, collected as it happens and handed to the growth pipeline.
    let mut growth = TurnGrowth::default();
    let mut outcome = TurnState::Idle;
    let mut output_tokens = 0u64;
    let mut reported_output: Option<u64> = None;
    let mut reported_input: Option<u64> = None;
    let mut last_estimated_input = 0u64;
    let mut over_limit_retries = 0u32;
    let max_steps = u64::from(
        inner
            .config
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .max_steps,
    );
    let mut steps_used = 0u64;
    // Watches the run of failures so the model can be handed a different plan. It never blocks a
    // call: a ✗ is information, and a turn that runs out of permitted attempts answers in promises.
    let mut guard = RepeatGuard::new();
    let mut step_notice_sent = false;
    let mut live_metrics = Instant::now();

    // Stage: user_input — accept, normalize and journal the operator's message before any model
    // work begins. The `user_input` hooks run inside the builtin; a native module can replace
    // the whole accept path — rewriting the message or refusing the input outright.
    let submitted_text = message.text_body();
    let mut input_ctx = StageContext::new(
        &inner,
        &session,
        &turn,
        &submitted_text,
        StagePayload::UserInput(UserInputPayload {
            message,
            deny: None,
            prepend: Vec::new(),
            append: Vec::new(),
        }),
    );
    if run_stage(
        &inner,
        &session,
        &agent,
        &turn,
        &submitted_text,
        &growth,
        started,
        output_tokens,
        steps_used,
        StagePoint::UserInput,
        &mut input_ctx,
    )
    .await
    {
        claim_guard.disarm();
        return;
    }
    let (request_text, input_deny, input_prepend, input_append) = match input_ctx.payload {
        StagePayload::UserInput(payload) => (
            payload.message.text_body(),
            payload.deny,
            payload.prepend,
            payload.append,
        ),
        _ => (submitted_text, None, Vec::new(), Vec::new()),
    };

    session.begin_turn(now_ms());
    // The user message is already durable, so the count of user-role messages is this turn's
    // ordinal. Providers that mirror the conversation hear about the turn here; the builtin
    // ignores it.
    let turn_number = session
        .messages()
        .iter()
        .filter(|m| m.role == Role::User)
        .count();
    inner
        .memory_providers()
        .on_turn_start(turn_number, &request_text);
    inner.emit(RuntimeEvent::StreamStarted {
        session: session.id.clone(),
        turn: turn.clone(),
    });
    if let Some(run) = &agent {
        run.agent.set_state(BotState::Thinking);
        inner.publish_bot(&run.agent);
    }
    inner.emit(RuntimeEvent::Status(inner.status()));
    inner.publish_metrics();

    if let Some(reason) = &input_deny {
        inner.emit(RuntimeEvent::Error {
            text: format!("a plugin refused this input: {reason}"),
        });
        turn_end_hook(&inner, &session, &turn, TurnState::Failed, 0, output_tokens).await;
        turn_end_stage(
            &inner,
            &session,
            &turn,
            &request_text,
            TurnState::Failed,
            0,
            output_tokens,
        )
        .await;
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
                growth: &growth,
            },
        );
        claim_guard.disarm();
        return;
    }

    // A turn-start hook may refuse the request or attach context the turn then carries. It runs
    // before the provider gate so a refusal is reported even when no model is configured.
    let turn_start = run_hook(
        &inner,
        &session,
        crate::hooks::HookContext::new(crate::hooks::HookPoint::TurnStart)
            .with_session(session.id.as_str())
            .with_turn(turn.as_str())
            .with_workspace(inner.workspace_root.display().to_string())
            .with_model(inner.model())
            .with_text(&request_text),
    )
    .await;
    if let Some(reason) = &turn_start.effect.deny {
        inner.emit(RuntimeEvent::Error {
            text: format!("a plugin refused this turn: {reason}"),
        });
        turn_end_hook(&inner, &session, &turn, TurnState::Failed, 0, output_tokens).await;
        turn_end_stage(
            &inner,
            &session,
            &turn,
            &request_text,
            TurnState::Failed,
            0,
            output_tokens,
        )
        .await;
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
                growth: &growth,
            },
        );
        claim_guard.disarm();
        return;
    }

    let Some(provider) = inner.provider.read().expect("provider lock").clone() else {
        inner.emit(RuntimeEvent::Error {
            text: "no model configured: set `model` and the API key named by `api_key_env`".into(),
        });
        turn_end_hook(&inner, &session, &turn, TurnState::Failed, 0, output_tokens).await;
        turn_end_stage(
            &inner,
            &session,
            &turn,
            &request_text,
            TurnState::Failed,
            0,
            output_tokens,
        )
        .await;
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
        claim_guard.disarm();
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

        // Stage: memory_recall — durable memory for this step. A native module can replace
        // retrieval wholesale; the builtin still runs the memory_recall hooks.
        let mut recall_ctx = StageContext::new(
            &inner,
            &session,
            &turn,
            &request_text,
            StagePayload::MemoryRecall(MemoryRecallPayload::default()),
        );
        if run_stage(
            &inner,
            &session,
            &agent,
            &turn,
            &request_text,
            &growth,
            started,
            output_tokens,
            steps_used,
            StagePoint::MemoryRecall,
            &mut recall_ctx,
        )
        .await
        {
            claim_guard.disarm();
            return;
        }
        let memory_text = match recall_ctx.payload {
            StagePayload::MemoryRecall(payload) => payload.text,
            _ => None,
        };

        // What the input and turn-start hooks asked to add goes in front of everything the host
        // assembled, in the order the hooks ran.
        let mut hook_injected: Vec<String> = input_prepend.clone();
        hook_injected.extend(input_append.iter().cloned());
        hook_injected.extend(turn_start.effect.prepend.iter().cloned());
        hook_injected.extend(turn_start.effect.append.iter().cloned());
        // Who is speaking. A bot carries its own identity — the profile's name, tags and
        // description — and must not be told it is the app. Resolved from the delegated run when
        // there is one, otherwise from the session's own bot, so an operator typing straight into a
        // bot's tab is answered by that bot and not by VIKRAM wearing its tab.
        let profile = agent
            .as_ref()
            .map(|run| run.agent.profile.clone())
            .or_else(|| {
                session
                    .bot()
                    .and_then(|id| inner.bots.get(&id))
                    .map(|bot| bot.profile.clone())
            });
        // Stage: context_build — the system prefix for this provider round.
        let mut build_ctx = StageContext::new(
            &inner,
            &session,
            &turn,
            &request_text,
            StagePayload::ContextBuild(ContextBuildPayload {
                hook_injected,
                profile,
                memory_text,
                prefix: Vec::new(),
                labels: Vec::new(),
            }),
        );
        if run_stage(
            &inner,
            &session,
            &agent,
            &turn,
            &request_text,
            &growth,
            started,
            output_tokens,
            steps_used,
            StagePoint::ContextBuild,
            &mut build_ctx,
        )
        .await
        {
            claim_guard.disarm();
            return;
        }
        let (prefix, labels) = match build_ctx.payload {
            StagePayload::ContextBuild(payload) => (payload.prefix, payload.labels),
            _ => (Vec::new(), Vec::new()),
        };
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
        // Stage: provider_request — the model_request hooks and the request itself. A native
        // module can replace request assembly; the builtin still runs the hooks.
        let mut request_ctx = StageContext::new(
            &inner,
            &session,
            &turn,
            &request_text,
            StagePayload::ProviderRequest(ProviderRequestPayload {
                messages: request_messages,
                request: None,
            }),
        );
        if run_stage(
            &inner,
            &session,
            &agent,
            &turn,
            &request_text,
            &growth,
            started,
            output_tokens,
            steps_used,
            StagePoint::ProviderRequest,
            &mut request_ctx,
        )
        .await
        {
            claim_guard.disarm();
            return;
        }
        let request = match request_ctx.payload {
            StagePayload::ProviderRequest(payload) => payload.request,
            _ => None,
        };
        let Some(request) = request else {
            // A stage that reports Done without a request is a bug, not a fallback case: the
            // turn fails honestly rather than sending nothing.
            inner.emit(RuntimeEvent::Error {
                text: "the `provider_request` stage produced no request".into(),
            });
            turn_end_hook(
                &inner,
                &session,
                &turn,
                TurnState::Failed,
                steps_used,
                output_tokens,
            )
            .await;
            turn_end_stage(
                &inner,
                &session,
                &turn,
                &request_text,
                TurnState::Failed,
                steps_used,
                output_tokens,
            )
            .await;
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
                    growth: &growth,
                },
            );
            claim_guard.disarm();
            return;
        };

        let mut stream = provider.stream(request);
        let mut dropped_for_limit = false;
        let mut text_buffer = String::new();
        let mut reasoning_buffer = String::new();
        let mut calls: Vec<ToolCall> = Vec::new();
        let mut stream_failed = false;
        // Silence is measured from the last *event that produced something visible*, not from a
        // one-shot "did anything ever arrive" flag. A model can emit one token and then go quiet
        // while it streams a long tool-call argument block (which the provider buffers without
        // emitting), and the old one-shot guard switched itself off after the first event — so the
        // turn sat on the client's whole timeout with a frozen clock and a blank screen. The idle
        // measure keeps the watchdog armed for the whole stream, however late the stall begins.
        let stream_started = Instant::now();
        let mut last_progress = stream_started;
        let silence_cap = std::time::Duration::from_secs(
            inner
                .config
                .read()
                .unwrap_or_else(|e| e.into_inner())
                .stream_silence_secs,
        );

        loop {
            let item = match tokio::time::timeout(LIVE_METRIC_INTERVAL, stream.next()).await {
                Ok(Some(item)) => Some(item),
                Ok(None) => break,
                Err(_) => None,
            };
            // Checked on every pass, not only when a quiet window closes empty. A provider that
            // drips keep-alives, empty deltas or usage chunks never misses a 250ms timeout, so a
            // watchdog that only runs inside the timeout arm can be postponed forever: the pane
            // stays blank, the clock keeps climbing, and nothing ever aborts the turn.
            if last_progress.elapsed() >= silence_cap {
                let partial = if text_buffer.is_empty() && reasoning_buffer.is_empty() {
                    String::new()
                } else {
                    " The part it had already written is kept.".to_owned()
                };
                inner.emit(RuntimeEvent::Error {
                    text: format!(
                        "the model sent nothing for {}s, so the stream was abandoned.{} \
                         Send again, or switch model with `vikram config set model <name>`. \
                         Raise the wait with `vikram config set stream_silence_secs <n>` if \
                         this model is genuinely that slow.",
                        last_progress.elapsed().as_secs(),
                        partial
                    ),
                });
                stream_failed = true;
                break;
            }
            // Publishes on a timer rather than on a timeout, so the elapsed time and the rate keep
            // moving even while the endpoint is dribbling events that render nothing.
            if live_metrics.elapsed() >= LIVE_METRIC_INTERVAL {
                live_metrics = Instant::now();
                inner.note_stream_progress(started.elapsed(), output_tokens);
            }
            // Cancellation is checked on every pass, including the passes where the endpoint said
            // nothing. This used to sit below the `continue`, so it only ran when a delta arrived:
            // a free or reasoning endpoint sitting on its first token for twenty seconds answered
            // the operator's stop with silence until that token finally came.
            if cancel.is_cancelled() {
                break;
            }
            let Some(item) = item else { continue };
            match item {
                Ok(ProviderEvent::Retrying {
                    attempt,
                    of,
                    wait_ms,
                    status,
                }) => {
                    // The wait counts as progress: it is the endpoint working through its own
                    // limit, not a model that has gone quiet, and the silence watchdog must not
                    // abandon a turn that is legitimately backing off.
                    last_progress = Instant::now();
                    let cause = if status == 0 {
                        "the connection failed".to_owned()
                    } else {
                        format!("the endpoint answered {status}")
                    };
                    inner.emit(RuntimeEvent::Notice {
                        level: NoticeLevel::Warn,
                        text: format!(
                            "{cause} — retrying in {:.1}s (attempt {attempt} of {of})",
                            wait_ms as f64 / 1000.0
                        ),
                    });
                }
                Ok(ProviderEvent::TextDelta(delta)) => {
                    if !delta.is_empty() {
                        last_progress = Instant::now();
                        output_tokens += vikram_contracts::TokenMeter::estimate(&delta);
                        text_buffer.push_str(&delta);
                        inner.emit(RuntimeEvent::StreamDelta {
                            session: session.id.clone(),
                            turn: turn.clone(),
                            text: delta,
                        });
                        if live_metrics.elapsed() >= LIVE_METRIC_INTERVAL {
                            live_metrics = Instant::now();
                            inner.note_stream_progress(started.elapsed(), output_tokens);
                        }
                        if let Some(run) = &agent
                            && run.agent.state() != BotState::Running
                        {
                            run.agent.set_state(BotState::Running);
                            inner.publish_bot(&run.agent);
                        }
                    }
                }
                Ok(ProviderEvent::ThinkingDelta(delta)) => {
                    if !delta.is_empty() {
                        last_progress = Instant::now();
                        reasoning_buffer.push_str(&delta);
                        inner.emit(RuntimeEvent::StreamThinkingDelta {
                            session: session.id.clone(),
                            turn: turn.clone(),
                            text: delta,
                        });
                        if live_metrics.elapsed() >= LIVE_METRIC_INTERVAL {
                            live_metrics = Instant::now();
                            inner.note_stream_progress(started.elapsed(), output_tokens);
                        }
                    }
                }
                Ok(ProviderEvent::ToolCall(call)) => {
                    last_progress = Instant::now();
                    calls.push(call);
                }
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
                    inner.emit(RuntimeEvent::Error { text });
                    stream_failed = true;
                    break;
                }
            }
        }

        if dropped_for_limit {
            // The rejected stream may have produced partial text, reasoning or call fragments
            // before the error arrived. Spend them now so the retried request cannot commit
            // stale fragments alongside the fresh stream's output.
            text_buffer.clear();
            reasoning_buffer.clear();
            calls.clear();
            // Nothing was produced; rebuild the request from the shortened transcript.
            continue;
        }
        if cancel.is_cancelled() {
            if !text_buffer.is_empty()
                && commit_assistant(
                    &inner,
                    &session,
                    &agent,
                    &turn,
                    &request_text,
                    &growth,
                    started,
                    output_tokens,
                    steps_used,
                    &text_buffer,
                    &reasoning_buffer,
                    &[],
                )
                .await
            {
                claim_guard.disarm();
                return;
            }
            reasoning_buffer.clear();
            outcome = TurnState::Cancelled;
            break;
        }
        if stream_failed {
            if (!text_buffer.is_empty() || !reasoning_buffer.is_empty())
                && commit_assistant(
                    &inner,
                    &session,
                    &agent,
                    &turn,
                    &request_text,
                    &growth,
                    started,
                    output_tokens,
                    steps_used,
                    &text_buffer,
                    &reasoning_buffer,
                    &[],
                )
                .await
            {
                claim_guard.disarm();
                return;
            }
            text_buffer.clear();
            reasoning_buffer.clear();
            outcome = TurnState::Failed;
            break;
        }

        // Whatever the model said or thought before it reached for a tool belongs at this point in
        // the transcript, not at the end of the turn. Committing only when there was text left a
        // reasoning-only step buffered until some later message carried it, which is how thoughts
        // ended up piled under the answer. Both buffers are cleared because they were just spent.
        //
        // The calls go in the same message: a tool result is only readable to the model when the
        // assistant turn that asked for it carries the `tool_calls` it answers. Without them the
        // transcript says "assistant said nothing" followed by an unexplained result, and a model
        // that cannot see its own call runs the same tool again under a new name.
        if (!text_buffer.is_empty() || !reasoning_buffer.is_empty() || !calls.is_empty())
            && commit_assistant(
                &inner,
                &session,
                &agent,
                &turn,
                &request_text,
                &growth,
                started,
                output_tokens,
                steps_used,
                &text_buffer,
                &reasoning_buffer,
                &calls,
            )
            .await
        {
            claim_guard.disarm();
            return;
        }
        text_buffer.clear();
        reasoning_buffer.clear();

        if calls.is_empty() {
            break;
        }

        let mut results: Vec<(ToolCallId, String, bool)> = Vec::new();

        // The model asked for the whole batch at once, so calls run together unless they share
        // a resource: reads, writes to different files, commands on different terminal or
        // browser sessions and different tools all proceed side by side, while calls that
        // could observe or overwrite each other keep their order inside one lane.
        // One provider round is one step, however many calls it asked for: counting `calls.len()`
        // made a single step that emitted 30 parallel calls report "30 steps" and trip the budget
        // notice on the first round.
        steps_used += 1;
        if steps_used >= max_steps && !step_notice_sent {
            // A budget, not a wall. The operator can see the count and stop the turn
            // themselves; ending it here would throw away a turn that is still finding
            // its way, which is the one thing an autonomous agent must never do.
            step_notice_sent = true;
            inner.emit(RuntimeEvent::Notice {
                level: NoticeLevel::Warn,
                text: format!(
                    "step budget reached ({steps_used} steps) — still working; Esc stops the turn"
                ),
            });
        }

        // Stage: tool_dispatch — fan the round's calls out across conflict lanes and collect
        // their outcomes. Policy, approvals and the per-call hook chain stay inside `run_tool`.
        let mut dispatch_ctx = StageContext::new(
            &inner,
            &session,
            &turn,
            &request_text,
            StagePayload::ToolDispatch(ToolDispatchPayload {
                calls: calls.clone(),
                cancel: cancel.clone(),
                agent: agent.as_ref().map(|run| AgentRun {
                    agent: Arc::clone(&run.agent),
                    task_id: run.task_id.clone(),
                }),
                outcomes: Vec::new(),
            }),
        );
        if run_stage(
            &inner,
            &session,
            &agent,
            &turn,
            &request_text,
            &growth,
            started,
            output_tokens,
            steps_used,
            StagePoint::ToolDispatch,
            &mut dispatch_ctx,
        )
        .await
        {
            claim_guard.disarm();
            return;
        }
        let outcomes = match dispatch_ctx.payload {
            StagePayload::ToolDispatch(payload) => payload.outcomes,
            _ => Vec::new(),
        };
        if outcomes.len() != calls.len() {
            // A stage that reports Done with the wrong arity is a bug, not a fallback case.
            inner.emit(RuntimeEvent::Error {
                text: format!(
                    "the `tool_dispatch` stage returned {} outcomes for {} calls",
                    outcomes.len(),
                    calls.len()
                ),
            });
            turn_end_hook(
                &inner,
                &session,
                &turn,
                TurnState::Failed,
                steps_used,
                output_tokens,
            )
            .await;
            turn_end_stage(
                &inner,
                &session,
                &turn,
                &request_text,
                TurnState::Failed,
                steps_used,
                output_tokens,
            )
            .await;
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
                    growth: &growth,
                },
            );
            claim_guard.disarm();
            return;
        }
        for (call, (content, ok)) in calls.iter().zip(outcomes) {
            guard.record(call, ok);
            growth.record_call(&call.name);
            if call.name == "verify" {
                growth.record_verification(ok, &content);
            } else if !ok {
                growth.record_failure(&call.name, &content);
            }
            results.push((call.id.clone(), content, ok));
        }
        let cancelled_mid_tools = cancel.is_cancelled();

        // Stage: result_commit — tool results become transcript messages, journal records and
        // events. A native module can replace what a result means to the conversation.
        let mut commit_ctx = StageContext::new(
            &inner,
            &session,
            &turn,
            &request_text,
            StagePayload::ResultCommit(ResultCommitPayload { results }),
        );
        if run_stage(
            &inner,
            &session,
            &agent,
            &turn,
            &request_text,
            &growth,
            started,
            output_tokens,
            steps_used,
            StagePoint::ResultCommit,
            &mut commit_ctx,
        )
        .await
        {
            claim_guard.disarm();
            return;
        }

        // The guards above only see repetition. A model that rewords its query and rotates its
        // tools never repeats itself, so they cannot see a turn that is failing in fresh ways.
        // Consecutive failures are that signal. They do not end the turn — an agent that keeps
        // trying is doing its job — they buy it a different plan and let it keep working.
        if guard.needs_advice() {
            let repeated = guard
                .worst_tool()
                .map(|(name, failures)| format!(" ({name} has failed {failures} times)"))
                .unwrap_or_default();
            inner.emit(RuntimeEvent::Notice {
                level: NoticeLevel::Warn,
                text: format!(
                    "{} tool calls in a row failed{repeated}. Changing the approach, still working.",
                    RepeatGuard::ADVICE_AFTER
                ),
            });
            let msg = Message::system(
                "Your last tool calls all failed. Do not repeat a call that already failed: it will fail the same way. Change the approach instead — a different tool for the same goal (web.search failed: try browser.fetch or browser.open on a real URL, then browser.text or browser.links; or http against a site's own endpoint, or search.text for the local machine), a different source or site, a narrower query, or one specific question to the operator. Keep working until you have the result, or until you can say precisely what is blocking you.",
            );
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

    // A steer that arrived after the last tool-call checkpoint has no safe point left in this
    // turn. Leaving it pending would inject it into the *next* turn, landing after newer queued
    // instructions the operator wrote later, so it is spent here: demoted to the back of the
    // queue, where it runs as an ordinary follow-up in order.
    if let Some(steer_text) = session.take_steer() {
        session.enqueue(steer_text);
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
    turn_end_hook(&inner, &session, &turn, outcome, steps_used, output_tokens).await;
    turn_end_stage(
        &inner,
        &session,
        &turn,
        &request_text,
        outcome,
        steps_used,
        output_tokens,
    )
    .await;
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
    claim_guard.disarm();
}

/// The resource a call touches, when two calls touching the same one must keep their order.
/// `None` means the call shares nothing with any sibling and may run beside all of them.
fn conflict_key(inner: &Arc<RuntimeInner>, call: &ToolCall) -> Option<String> {
    let tool = inner.tools.get(&call.name)?;
    let action = tool.permission(&call.arguments).ok()?.action;
    match action {
        ActionKind::Read => None,
        ActionKind::Write => {
            for key in ["path", "file", "file_path", "target", "resource"] {
                if let Some(raw) = call.arguments.get(key).and_then(|v| v.as_str()) {
                    let resolved = crate::config::resolve_within(
                        &inner.workspace_root,
                        std::path::Path::new(raw),
                    )
                    .map(|p| p.display().to_string())
                    .unwrap_or_else(|| raw.to_owned());
                    return Some(format!("path:{resolved}"));
                }
            }
            Some(format!("write:{}", call.name))
        }
        _ => {
            // Session-scoped tools (terminal, browser.*, …) name the session they drive; calls
            // on different sessions are independent and run together.
            for key in ["session", "id", "name"] {
                if let Some(value) = call.arguments.get(key).and_then(|v| v.as_str()) {
                    return Some(format!("{}:{value}", call.name));
                }
            }
            // A terminal `open` without a name starts a fresh PTY every time, so each one is
            // independent; everything else of its kind shares the tool's own lane.
            if call.name == "terminal"
                && call.arguments.get("action").and_then(|v| v.as_str()) == Some("open")
            {
                return Some(format!("pty-open:{}", call.id.as_str()));
            }
            Some(format!("exec:{}", call.name))
        }
    }
}

/// The turn's memory of what has been failing.
///
/// It never blocks a call. A failed tool is one ✗ in a list: the agent's job is to read it and try
/// something else, and every attempt it is forbidden from making is an attempt it spends writing a
/// promise instead. All this does is notice a run of failures and hand the model a better plan,
/// periodically, for as long as it keeps working.
struct RepeatGuard {
    /// Tool name -> failures this turn, across every set of arguments. Read only by the advice, so
    /// it can name the tool that keeps failing instead of describing it.
    by_tool: HashMap<String, u32>,
    /// Failures since the last successful call, regardless of which tool produced them.
    failures_in_row: u32,
    /// The streak length the turn was last advised at, so one streak produces one nudge.
    advised_at: u32,
}

impl RepeatGuard {
    /// Failures in a row — across every tool and every set of arguments — before the turn is
    /// handed a different plan. Guidance, not a wall: an agent that keeps trying is doing its job,
    /// it just needs to stop trying the same way.
    const ADVICE_AFTER: u32 = 3;

    fn new() -> Self {
        Self {
            by_tool: HashMap::new(),
            failures_in_row: 0,
            advised_at: 0,
        }
    }

    /// Records the outcome of a call that actually ran.
    fn record(&mut self, call: &ToolCall, ok: bool) {
        if ok {
            // One success is enough to say the turn is still working on something it can reach.
            self.failures_in_row = 0;
            self.advised_at = 0;
            return;
        }
        self.failures_in_row += 1;
        *self.by_tool.entry(call.name.clone()).or_insert(0) += 1;
    }

    /// The tool that has failed most this turn, or `None` before anything has failed.
    fn worst_tool(&self) -> Option<(&str, u32)> {
        self.by_tool
            .iter()
            .max_by_key(|(_, failures)| **failures)
            .map(|(name, failures)| (name.as_str(), *failures))
    }

    /// Whether the turn has failed enough times in a row that it should be handed a different
    /// plan. Counted across tools and arguments, because a change of wording is not a change of
    /// outcome. Returns true once per streak length, so a long run of failures is nudged
    /// periodically rather than nagged on every step.
    fn needs_advice(&mut self) -> bool {
        if self.failures_in_row < Self::ADVICE_AFTER
            || !self.failures_in_row.is_multiple_of(Self::ADVICE_AFTER)
            || self.advised_at == self.failures_in_row
        {
            return false;
        }
        self.advised_at = self.failures_in_row;
        true
    }
}

/// Replace a message's prose, keeping every non-text block (images) in place. The first text
/// block takes the rewrite and the rest are dropped, so a multi-paragraph input collapses to the
/// single text a hook produced.
pub(crate) fn rewrite_message_text(message: &mut Message, text: String) {
    let mut rewritten = false;
    let mut blocks = Vec::with_capacity(message.blocks.len());
    for block in message.blocks.drain(..) {
        match block {
            Block::Text { trust, .. } if !rewritten => {
                blocks.push(Block::Text {
                    text: text.clone(),
                    trust,
                });
                rewritten = true;
            }
            Block::Text { .. } => {}
            other => blocks.push(other),
        }
    }
    if !rewritten {
        blocks.insert(
            0,
            Block::Text {
                text,
                trust: Trust::User,
            },
        );
    }
    message.blocks = blocks;
}

/// Dispatch the `response_commit` stage for one assistant message. Returns `true` when the
/// stage refused the turn or its handler failed — the turn is already ended honestly and the
/// caller only unwinds.
#[allow(clippy::too_many_arguments)]
async fn commit_assistant(
    inner: &Arc<RuntimeInner>,
    session: &Arc<SessionRuntime>,
    agent: &Option<AgentRun>,
    turn: &TurnId,
    request_text: &str,
    growth: &TurnGrowth,
    started: Instant,
    output_tokens: u64,
    steps_used: u64,
    text: &str,
    reasoning: &str,
    calls: &[ToolCall],
) -> bool {
    let mut ctx = StageContext::new(
        inner,
        session,
        turn,
        request_text,
        StagePayload::ResponseCommit(ResponseCommitPayload {
            text: text.to_owned(),
            reasoning: reasoning.to_owned(),
            calls: calls.to_vec(),
        }),
    );
    run_stage(
        inner,
        session,
        agent,
        turn,
        request_text,
        growth,
        started,
        output_tokens,
        steps_used,
        StagePoint::ResponseCommit,
        &mut ctx,
    )
    .await
}

/// The `response_commit` stage's builtin body: `response_delivery` hooks see the prose — a
/// denial withholds the text but never the reasoning or tool calls it announced, and a `text`
/// effect rewrites what lands — then the message commits to transcript, journal and events.
async fn deliver_assistant_message(
    inner: &Arc<RuntimeInner>,
    session: &Arc<SessionRuntime>,
    turn: &TurnId,
    text: &str,
    reasoning: &str,
    calls: &[ToolCall],
) {
    let mut delivered = text.to_owned();
    let mut withheld = false;
    if !delivered.is_empty() {
        let hook = run_hook(
            inner,
            session,
            crate::hooks::HookContext::new(crate::hooks::HookPoint::ResponseDelivery)
                .with_session(session.id.as_str())
                .with_turn(turn.as_str())
                .with_workspace(inner.workspace_root.display().to_string())
                .with_model(inner.model())
                .with_text(&delivered)
                .with_details(serde_json::json!({"tool_calls": calls.len()})),
        )
        .await;
        if hook.effect.deny.is_some() {
            delivered.clear();
            withheld = true;
        } else if let Some(rewritten) = hook.effect.text {
            delivered = rewritten;
        }
    }
    // A denied reply sends nothing at all: the artifacts a tool produced stay in the workspace
    // rather than riding out on a response the operator's plugin refused.
    let images = if withheld {
        Vec::new()
    } else {
        pending_image_artifacts(inner, session)
    };
    commit_assistant_text(inner, session, &delivered, reasoning, calls, &images);
}

/// The turn's closing dispatch: outcome, step count and token figures for observers. It runs
/// before `finish` publishes `TurnFinished`, while the turn still reads as open. The outcome is
/// already decided, so effects other than observation have nothing left to refuse.
async fn turn_end_hook(
    inner: &Arc<RuntimeInner>,
    session: &Arc<SessionRuntime>,
    turn: &TurnId,
    outcome: TurnState,
    steps: u64,
    output_tokens: u64,
) {
    let outcome_name = format!("{outcome:?}").to_lowercase();
    let _ = run_hook(
        inner,
        session,
        crate::hooks::HookContext::new(crate::hooks::HookPoint::TurnEnd)
            .with_session(session.id.as_str())
            .with_turn(turn.as_str())
            .with_workspace(inner.workspace_root.display().to_string())
            .with_model(inner.model())
            .with_text(&outcome_name)
            .with_details(serde_json::json!({
                "outcome": outcome_name,
                "steps": steps,
                "output_tokens": output_tokens,
            })),
    )
    .await;
}

/// The `turn_end` stage dispatch: native observers see the decided outcome, step count and
/// token figures. The builtin default is a pass-through — the `turn_end` hooks dispatched just
/// before already carry every effect — and the outcome is already decided, so a stage failure
/// is reported rather than allowed to fail a turn that has already ended.
async fn turn_end_stage(
    inner: &Arc<RuntimeInner>,
    session: &Arc<SessionRuntime>,
    turn: &TurnId,
    request_text: &str,
    outcome: TurnState,
    steps: u64,
    output_tokens: u64,
) {
    let mut ctx = StageContext::new(
        inner,
        session,
        turn,
        request_text,
        StagePayload::TurnEnd(TurnEndPayload {
            outcome,
            steps,
            output_tokens,
        }),
    );
    if let Err(err) = inner.stages().run(StagePoint::TurnEnd, &mut ctx).await {
        inner.emit(RuntimeEvent::Error {
            text: err.to_string(),
        });
    }
}

/// Which memory-scoped point a tool call belongs to, when it is one at all. Reads recall,
/// everything else the memory tools do mutates the store.
fn memory_hook_point(name: &str) -> Option<crate::hooks::HookPoint> {
    match name {
        "memory.search" | "memory.list" => Some(crate::hooks::HookPoint::MemoryRecall),
        "memory.save" | "memory.delete" | "memory.consolidate" | "memory.note" => {
            Some(crate::hooks::HookPoint::MemoryWrite)
        }
        _ => None,
    }
}

fn commit_assistant_text(
    inner: &Arc<RuntimeInner>,
    session: &Arc<SessionRuntime>,
    text: &str,
    reasoning: &str,
    calls: &[ToolCall],
    images: &[ImageSource],
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
    for call in calls {
        blocks.push(Block::ToolCall { call: call.clone() });
    }
    for source in images {
        // Pixels are external content: they can inform the model, never authorize an action.
        blocks.push(Block::Image {
            source: source.clone(),
            trust: Trust::External,
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

/// The line a media-producing tool ends its output with to name the artifact file it wrote. The
/// host reads it back off the committed tool result; the model only ever sees it as text.
const IMAGE_ARTIFACT_PREFIX: &str = "IMAGE:";

/// Tools whose `IMAGE:` lines name real files a reply may carry. Anything else — a fetched page,
/// a file's contents — can print the marker but is never honored.
const IMAGE_ARTIFACT_TOOLS: &[&str] = &[
    "browser.screenshot",
    "computer.screenshot",
    "computer.read_screen",
];

/// One reply carries at most this many artifacts, each at most this many bytes. A skipped image
/// loses delivery, not the artifact: the file stays in the workspace either way.
const MAX_IMAGE_ARTIFACTS: usize = 4;
const MAX_IMAGE_ARTIFACT_BYTES: u64 = 4 * 1024 * 1024;

const PNG_SIGNATURE: &[u8; 8] = b"\x89PNG\r\n\x1a\n";

/// The images the tool results committed since the last assistant message asked the reply to
/// carry. Scanning stops at the previous assistant or user message, so each artifact attaches to
/// exactly one reply and a turn whose commit was refused cannot leak its artifacts into the next.
fn pending_image_artifacts(
    inner: &Arc<RuntimeInner>,
    session: &Arc<SessionRuntime>,
) -> Vec<ImageSource> {
    let messages = session.messages();
    let mut names: HashMap<&ToolCallId, &str> = HashMap::new();
    for message in &messages {
        for block in &message.blocks {
            if let Block::ToolCall { call } = block {
                names.insert(&call.id, call.name.as_str());
            }
        }
    }
    let mut artifacts = Vec::new();
    let mut claimed = std::collections::HashSet::new();
    'scan: for message in messages.iter().rev() {
        if matches!(message.role, Role::Assistant | Role::User | Role::System) {
            break;
        }
        for block in &message.blocks {
            let Block::ToolResult {
                call, content, ok, ..
            } = block
            else {
                continue;
            };
            if !*ok
                || !names
                    .get(call)
                    .is_some_and(|name| IMAGE_ARTIFACT_TOOLS.contains(name))
            {
                continue;
            }
            for line in content.lines() {
                let Some(raw) = line.strip_prefix(IMAGE_ARTIFACT_PREFIX) else {
                    continue;
                };
                if let Some(source) = image_artifact(inner, raw.trim(), &mut claimed) {
                    artifacts.push(source);
                    if artifacts.len() >= MAX_IMAGE_ARTIFACTS {
                        break 'scan;
                    }
                }
            }
        }
    }
    artifacts.reverse();
    artifacts
}

/// One `IMAGE:` path as inline image bytes, or `None` when the file is not a real, reasonably
/// sized PNG inside the workspace. The signature check is what keeps a forged marker from
/// turning the reply into a file reader.
fn image_artifact(
    inner: &Arc<RuntimeInner>,
    raw: &str,
    claimed: &mut std::collections::HashSet<std::path::PathBuf>,
) -> Option<ImageSource> {
    let resolved = crate::config::resolve_within(&inner.workspace_root, std::path::Path::new(raw))?;
    if !claimed.insert(resolved.clone()) {
        return None;
    }
    let size = std::fs::metadata(&resolved).ok()?.len();
    if size > MAX_IMAGE_ARTIFACT_BYTES {
        inner.emit(RuntimeEvent::Notice {
            level: NoticeLevel::Warn,
            text: format!(
                "image artifact `{}` is {} bytes, over the {} byte cap; not attached",
                resolved.display(),
                size,
                MAX_IMAGE_ARTIFACT_BYTES
            ),
        });
        return None;
    }
    let bytes = std::fs::read(&resolved).ok()?;
    if bytes.len() < PNG_SIGNATURE.len() || bytes[..PNG_SIGNATURE.len()] != *PNG_SIGNATURE {
        return None;
    }
    Some(ImageSource::data("image/png", base64_encode(&bytes)))
}

fn base64_encode(data: &[u8]) -> String {
    const TABLE: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(data.len().div_ceil(3) * 4);
    for chunk in data.chunks(3) {
        let n = ((chunk[0] as u32) << 16)
            | ((*chunk.get(1).unwrap_or(&0) as u32) << 8)
            | (*chunk.get(2).unwrap_or(&0) as u32);
        out.push(TABLE[((n >> 18) & 63) as usize] as char);
        out.push(TABLE[((n >> 12) & 63) as usize] as char);
        out.push(if chunk.len() > 1 {
            TABLE[((n >> 6) & 63) as usize] as char
        } else {
            '='
        });
        out.push(if chunk.len() > 2 {
            TABLE[(n & 63) as usize] as char
        } else {
            '='
        });
    }
    out
}

/// The unwind path `finish` cannot be. `run_turn` is spawned with its `JoinHandle` dropped, so
/// a panic inside it surfaces to nobody: without this guard the turn slot stays claimed forever
/// and the session wedges. The guard fires only while the turn still looks mid-flight — a
/// claimed slot *and* a busy state — which is exactly the state a panic leaves behind and never
/// the state a finished (or already-reported) turn is in.
struct TurnGuard {
    inner: Arc<RuntimeInner>,
    session: Arc<SessionRuntime>,
    turn: TurnId,
    agent: Option<AgentRun>,
    armed: std::cell::Cell<bool>,
}

impl TurnGuard {
    fn disarm(&self) {
        self.armed.set(false);
    }
}

impl Drop for TurnGuard {
    fn drop(&mut self) {
        if !self.armed.get() || !self.session.turn_claimed() || !self.session.is_busy() {
            return;
        }
        self.session.finish_turn(TurnState::Failed, now_ms());
        if let Some(run) = &self.agent {
            run.agent.set_state(BotState::Failed);
            run.agent.set_task(None);
            self.inner.publish_bot(&run.agent);
            if let Some(task) = self.inner.tasks.finish(&run.task_id, TaskState::Failed) {
                self.inner.task_snapshot(&task);
            }
        }
        self.inner.emit(RuntimeEvent::TurnFinished {
            session: self.session.id.clone(),
            turn: self.turn.clone(),
            state: TurnState::Failed,
        });
        self.session.release_turn();
    }
}

/// The assistant prose a finished turn committed, for providers that sync the exchange. Only
/// assistant messages after the turn's own user message count; anything earlier belongs to a
/// previous turn. An empty string means the turn produced no prose.
fn last_turn_assistant_text(session: &SessionRuntime, request: &str) -> String {
    let messages = session.messages();
    let start = messages
        .iter()
        .rposition(|m| m.role == Role::User && m.text_body() == request)
        .map(|index| index + 1)
        .or_else(|| {
            messages
                .iter()
                .rposition(|m| m.role == Role::User)
                .map(|index| index + 1)
        })
        .unwrap_or(0);
    messages[start..]
        .iter()
        .filter(|m| m.role == Role::Assistant)
        .map(|m| m.text_body())
        .filter(|text| !text.trim().is_empty())
        .collect::<Vec<_>>()
        .join("\n")
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
    // Providers that mirror the conversation get the exchange after it is durable; the builtin
    // ignores it, so this changes nothing for the default backend. A turn that failed before
    // any prose exists has nothing to mirror.
    let assistant_text = last_turn_assistant_text(session, request);
    if !(outcome == TurnState::Failed && assistant_text.is_empty()) {
        inner
            .memory_providers()
            .sync_all(request, &assistant_text, session.id.as_str());
    }
    // Dialectic user modeling: derive preferences/habits/goals from this turn's user
    // request (heuristic, local, deterministic) for future turns' supplement layer.
    if outcome != TurnState::Cancelled {
        inner.observe_dialectic_turn(request);
    }
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
            TurnState::Cancelled => (BotState::Stopped, TaskState::Cancelled),
            TurnState::Failed => (BotState::Failed, TaskState::Failed),
            _ => (BotState::Completed, TaskState::Completed),
        };
        run.agent.set_state(state);
        run.agent.set_task(None);
        inner.publish_bot(&run.agent);
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

    // Pop the next follow-up while this turn still holds the claim, then hand the slot back:
    // the spawned turn re-claims it inside `spawn_turn`, and a submit racing the gap queues
    // behind the drain instead of running beside it. The queue drains on every outcome —
    // including Cancelled and Failed — because a stranded queue is input the operator already
    // sent and is still owed an answer.
    let next = session.dequeue();
    session.release_turn();
    if let Some(next) = next {
        // A queued follow-up keeps the same agent (if any) but gets its own
        // background task id so progress and completion are tracked rather than
        // being silently dropped (the previous `String::new()` lost the task).
        let queued_agent = agent.as_ref().map(|run| {
            run.agent.set_state(BotState::Thinking);
            run.agent.set_task(Some(next.clone()));
            inner.publish_bot(&run.agent);
            let bg = inner.tasks.start(
                format!("agent: {}", run.agent.profile.name),
                session.id.clone(),
            );
            AgentRun {
                agent: Arc::clone(&run.agent),
                task_id: bg.id.clone(),
            }
        });
        // The queue holds text only, so a follow-up that was queued while this turn ran starts
        // without the images that were refused at submit time.
        spawn_turn(
            Arc::clone(inner),
            Arc::clone(session),
            Message::user(next),
            queued_agent,
        );
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

/// Faults worth another attempt: the call never completed. A tool that answered — even with
/// "0 results" or a 404 — is not retried, because the second answer would be the same and the model
/// can re-plan faster than the clock can be wasted.
const TRANSIENT_ATTEMPTS: usize = 3;

/// Whether a failure is the transport's fault rather than the tool's answer.
fn is_transient(content: &str) -> bool {
    let lower = content.to_ascii_lowercase();
    [
        "timed out",
        "timeout",
        "connection reset",
        "connection refused",
        "connection closed",
        "error sending request",
        "failed to connect",
        "temporarily unavailable",
        "rate limit",
        "too many requests",
        "status 429",
        "status 502",
        "status 503",
        "status 504",
    ]
    .iter()
    .any(|needle| lower.contains(needle))
}

/// Whether a failed call may be attempted again. Only a tool whose permission request declares
/// `SideEffect::None` — a read, a listing, a search — is safe to retry: it cannot have half-applied
/// a write, sent a message or spent money before the transport gave up. Anything with a side
/// effect gets exactly one attempt, because a retry of a non-idempotent call duplicates it.
fn retryable_on_transient(inner: &Arc<RuntimeInner>, call: &ToolCall) -> bool {
    inner
        .tools
        .get(&call.name)
        .and_then(|tool| tool.permission(&call.arguments).ok())
        .is_some_and(|request| request.side_effect == SideEffect::None)
}

/// Run one call, giving a request that never completed up to three attempts. The retry lives here
/// rather than in the turn loop so the model is never handed a failure that was only the network
/// hiccuping — it would answer that with a promise instead of a result.
async fn run_tool(
    inner: &Arc<RuntimeInner>,
    session: &Arc<SessionRuntime>,
    call: &ToolCall,
    cancel: &vikram_contracts::CancellationToken,
    agent: &Option<AgentRun>,
    origin: Origin,
) -> (String, bool) {
    let retryable = retryable_on_transient(inner, call);
    let mut attempt = 1;
    loop {
        let (content, ok) = run_tool_once(inner, session, call, cancel, agent, origin).await;
        if ok
            || !retryable
            || attempt >= TRANSIENT_ATTEMPTS
            || !is_transient(&content)
            || cancel.is_cancelled()
        {
            return (content, ok);
        }
        attempt += 1;
        inner.emit(RuntimeEvent::Notice {
            level: NoticeLevel::Warn,
            text: format!(
                "{} failed: {}. Retrying — attempt {attempt} of {TRANSIENT_ATTEMPTS}.",
                call.name,
                truncate(&content, 80)
            ),
        });
        tokio::time::sleep(std::time::Duration::from_millis(250 * attempt as u64)).await;
    }
}

/// Publish the start/finish pair for a call a plugin refused before it ran: the tool never
/// executed, but the transcript still shows the attempt and the reason.
fn emit_tool_denial(
    inner: &Arc<RuntimeInner>,
    session: &Arc<SessionRuntime>,
    call: &ToolCall,
    content: &str,
    arguments_text: &str,
    cwd: &str,
    started: Instant,
) {
    inner.emit(RuntimeEvent::ToolStarted {
        session: session.id.clone(),
        call: call.id.clone(),
        name: call.name.clone(),
        summary: content.to_owned(),
        resource: String::new(),
        arguments: arguments_text.to_owned(),
        cwd: cwd.to_owned(),
    });
    inner.emit(RuntimeEvent::ToolFinished {
        session: session.id.clone(),
        call: call.id.clone(),
        name: call.name.clone(),
        ok: false,
        summary: content.to_owned(),
        content: content.to_owned(),
        resource: String::new(),
        arguments: arguments_text.to_owned(),
        cwd: cwd.to_owned(),
        duration_ms: started.elapsed().as_millis() as u64,
    });
}

async fn run_tool_once(
    inner: &Arc<RuntimeInner>,
    session: &Arc<SessionRuntime>,
    call: &ToolCall,
    cancel: &vikram_contracts::CancellationToken,
    agent: &Option<AgentRun>,
    origin: Origin,
) -> (String, bool) {
    let started = std::time::Instant::now();
    let cwd = inner.tool_context().workspace_root.display().to_string();
    let mut arguments_text = truncate(&call.arguments.to_string(), 8_000);
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
    // The call hook runs before policy so a rewritten argument is the one policy judges, and a
    // denial costs nothing: the tool never runs and the model is told why.
    let mut call = call.clone();
    let call_hook = run_hook(
        inner,
        session,
        crate::hooks::HookContext::new(crate::hooks::HookPoint::ToolCall)
            .with_session(session.id.as_str())
            .with_workspace(inner.workspace_root.display().to_string())
            .with_model(inner.model())
            .with_text(call.name.clone())
            .with_tool(call.name.clone(), call.arguments.clone()),
    )
    .await;
    if let Some(reason) = &call_hook.effect.deny {
        let content = format!("denied by plugin: {reason}");
        emit_tool_denial(
            inner,
            session,
            &call,
            &content,
            &arguments_text,
            &cwd,
            started,
        );
        return (content, false);
    }
    if let Some(arguments) = call_hook.effect.arguments {
        call.arguments = arguments;
        arguments_text = truncate(&call.arguments.to_string(), 8_000);
    }

    // A `memory.*` call gets its own point after the generic call hook, so it sees the arguments
    // that survived `tool_call` rewrites and can refuse the store operation on its own.
    if let Some(point) = memory_hook_point(&call.name) {
        let memory_hook = run_hook(
            inner,
            session,
            crate::hooks::HookContext::new(point)
                .with_session(session.id.as_str())
                .with_workspace(inner.workspace_root.display().to_string())
                .with_model(inner.model())
                .with_text(call.name.clone())
                .with_tool(call.name.clone(), call.arguments.clone()),
        )
        .await;
        if let Some(reason) = &memory_hook.effect.deny {
            let content = format!("denied by plugin: {reason}");
            emit_tool_denial(
                inner,
                session,
                &call,
                &content,
                &arguments_text,
                &cwd,
                started,
            );
            return (content, false);
        }
        if let Some(arguments) = memory_hook.effect.arguments {
            call.arguments = arguments;
            arguments_text = truncate(&call.arguments.to_string(), 8_000);
        }
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
    // real if the gate can see that: a child may not keep spawning children. The decision must be
    // the depth-aware one — judging at depth 0 computed the veto and then ignored it.
    let depth = session.depth();
    let raw = inner.policy().evaluate_at_depth(&request, depth);
    let decision = inner.judge_at_depth(&request, depth);
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
                run.agent.set_state(BotState::Waiting);
                inner.publish_bot(&run.agent);
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
                // Drop the pending entry — a late answer must not resolve (or worse, persist an
                // allow-rule for) a call that never ran.
                inner
                    .approvals
                    .lock()
                    .expect("approvals lock")
                    .remove(&approval);
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
                run.agent.set_state(BotState::Running);
                inner.publish_bot(&run.agent);
            }
        }
        Decision::Allow => {}
    }

    // Arguments are journaled for replay, but a command can carry a credential (`curl -H
    // "Authorization: Bearer …"`); the journal is plaintext, so secrets are redacted first.
    let mut journaled_arguments = call.arguments.clone();
    crate::redact::redact_json(&mut journaled_arguments);
    inner.journal(
        &session.id,
        RecordKind::ToolCall,
        serde_json::json!({"call": call.id.as_str(), "name": call.name, "arguments": journaled_arguments}),
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
        run.agent.set_state(BotState::Running);
        inner.publish_bot(&run.agent);
    }

    let mut context = inner.tool_context_for(&session.id);
    // Tools whose subject is the running system (delegation) need the live runtime, not a file.
    context.runtime = Some(Arc::clone(inner));
    // Enrich with session/agent identity and the turn's cancellation handle so a
    // blocking tool can observe cancellation directly instead of only via abort.
    context.agent = agent.as_ref().map(|run| run.agent.profile.name.clone());
    context.cancel = Some(cancel.clone());
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
    // its own call. Cancellation resolves the wait and fails the call instead of hanging. Only
    // `ask_user` may pose a question: any other tool's output that happens to start with
    // `QUESTION:` (a file, a search hit, a page) used to park the whole turn waiting for an answer
    // nobody was asked for.
    if ok
        && call.name == "ask_user"
        && let Some((request_id, question, choices)) = parse_question(&content)
    {
        let receiver = inner.open_question(&request_id);
        inner.emit(RuntimeEvent::UserQuestion {
            session: session.id.clone(),
            question,
            choices,
            request_id: request_id.clone(),
        });
        if let Some(run) = agent {
            run.agent.set_state(BotState::Waiting);
            inner.publish_bot(&run.agent);
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

    // A result hook sees what the tool produced before the transcript does, so a plugin can
    // annotate or withhold it without replacing the tool.
    let result_hook = run_hook(
        inner,
        session,
        crate::hooks::HookContext::new(crate::hooks::HookPoint::ToolResult)
            .with_session(session.id.as_str())
            .with_workspace(inner.workspace_root.display().to_string())
            .with_model(inner.model())
            .with_text(call.name.clone())
            .with_tool(call.name.clone(), call.arguments.clone())
            .with_result(content.clone(), ok),
    )
    .await;
    if let Some(reason) = &result_hook.effect.deny {
        content = format!("the result was withheld by a plugin: {reason}");
        ok = false;
    }
    if let Some(replacement) = result_hook.effect.content {
        content = replacement;
    }
    if let Some(flag) = result_hook.effect.ok {
        ok = flag;
    }
    for note in &result_hook.effect.append {
        content.push('\n');
        content.push_str(note);
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
            "content": truncate(&crate::redact::redact_secrets(&content), 64_000),
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

/// Dispatch one hook context off the async threads and journal every decision it produced.
///
/// Process hooks are blocking by nature, so the chain runs on the blocking pool. A dispatch that
/// cannot complete is a warning, not a failed turn: the default behaviour stands.
async fn run_hook(
    inner: &Arc<RuntimeInner>,
    session: &Arc<SessionRuntime>,
    ctx: crate::hooks::HookContext,
) -> crate::hooks::HookOutcome {
    let point = ctx.point.as_str();
    let runtime = Arc::clone(inner);
    let outcome = match tokio::task::spawn_blocking(move || runtime.dispatch_hook(&ctx)).await {
        Ok(outcome) => outcome,
        Err(err) => {
            inner.emit(RuntimeEvent::Notice {
                level: NoticeLevel::Warn,
                text: format!("a plugin hook could not be dispatched: {err}"),
            });
            crate::hooks::HookOutcome::default()
        }
    };
    inner.journal_hook(&session.id, point, &outcome);
    outcome
}

/// The operator-facing text for a stage's refusal, matching the wording the point's hooks had.
fn stage_refusal(point: StagePoint, reason: &str) -> String {
    match point {
        StagePoint::ContextBuild => format!("a plugin refused this turn's context: {reason}"),
        StagePoint::ProviderRequest => format!("a plugin refused the model request: {reason}"),
        StagePoint::UserInput => format!("a plugin refused this input: {reason}"),
        _ => format!("the `{}` stage refused: {reason}", point.as_str()),
    }
}

/// Dispatch one pipeline stage. Returns `true` when the stage refused the turn or its handler
/// failed — the turn is already ended honestly (the reason is published, `turn_end` dispatched
/// and `finish` run) and the caller only unwinds. A stage failure is never swallowed into a
/// silent fallback: a broken handler fails the turn.
#[allow(clippy::too_many_arguments)]
async fn run_stage(
    inner: &Arc<RuntimeInner>,
    session: &Arc<SessionRuntime>,
    agent: &Option<AgentRun>,
    turn: &TurnId,
    request_text: &str,
    growth: &TurnGrowth,
    started: Instant,
    output_tokens: u64,
    steps_used: u64,
    point: StagePoint,
    ctx: &mut StageContext,
) -> bool {
    let failed = match inner.stages().run(point, ctx).await {
        Ok(StageOutcome::Done) => return false,
        // The registry resolves `Pass` inside the chain, so this is unreachable; treat it as a
        // stage failure rather than letting a turn continue on an undecided payload.
        Ok(StageOutcome::Pass) => {
            format!(
                "the `{}` stage passed without a handler resolving it",
                point.as_str()
            )
        }
        Ok(StageOutcome::Refused(reason)) => stage_refusal(point, &reason),
        Err(err) => err.to_string(),
    };
    inner.emit(RuntimeEvent::Error { text: failed });
    turn_end_hook(
        inner,
        session,
        turn,
        TurnState::Failed,
        steps_used,
        output_tokens,
    )
    .await;
    turn_end_stage(
        inner,
        session,
        turn,
        request_text,
        TurnState::Failed,
        steps_used,
        output_tokens,
    )
    .await;
    finish(
        inner,
        session,
        agent,
        turn.clone(),
        TurnReport {
            outcome: TurnState::Failed,
            started,
            output_tokens,
            request: request_text,
            growth,
        },
    );
    true
}

/// The `memory_recall` stage's builtin body: retrieve durable memory for this step and let the
/// `memory_recall` hooks withhold or rewrite it before it is injected.
async fn recall_memory(
    inner: &Arc<RuntimeInner>,
    session: &Arc<SessionRuntime>,
    turn: &TurnId,
) -> Option<String> {
    // Retrieval is a SQLite query plus disk reads; like the code-index fallback in context
    // assembly it runs on the blocking pool so it cannot hold an executor thread the stream
    // loop shares.
    let memory_text = {
        let inner = Arc::clone(inner);
        let session = Arc::clone(session);
        tokio::task::spawn_blocking(move || inner.retrieved_memory(&session))
            .await
            .unwrap_or_default()
    };
    // A recall hook sees what the store is about to inject — and may withhold it or rewrite
    // it — before the prefix is assembled. It fires once per step because retrieval does.
    match memory_text {
        Some(text) => {
            let recall = run_hook(
                inner,
                session,
                crate::hooks::HookContext::new(crate::hooks::HookPoint::MemoryRecall)
                    .with_session(session.id.as_str())
                    .with_turn(turn.as_str())
                    .with_workspace(inner.workspace_root.display().to_string())
                    .with_model(inner.model())
                    .with_text(&text),
            )
            .await;
            if recall.effect.deny.is_some() {
                None
            } else {
                Some(recall.effect.text.unwrap_or(text))
            }
        }
        None => None,
    }
}

/// The `context_build` stage's builtin body: assemble the system prefix for one provider round
/// and apply the `context_build` hooks. `Err(reason)` is the hook's refusal of the turn's
/// context, surfaced as [`StageOutcome::Refused`].
async fn assemble_context(
    inner: &Arc<RuntimeInner>,
    session: &Arc<SessionRuntime>,
    turn: &TurnId,
    request_text: &str,
    hook_injected: Vec<String>,
    profile: Option<crate::types::BotProfile>,
    memory_text: Option<String>,
) -> Result<(Vec<Message>, Vec<&'static str>), String> {
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
    labels.push("turn voice");
    // The operator sees the model's own words between the tool rows, so the model has to be
    // told that those words exist and what shape they take — unless a bot profile says who the
    // voice actually is.
    match profile {
        Some(profile) => {
            labels.push("bot identity");
            prefix.push(Message::text(
                Role::System,
                bot_preamble(&profile),
                Trust::External,
            ));
        }
        None => prefix.push(Message::text(Role::System, PREAMBLE, Trust::External)),
    }
    // Memory and project context are assembled against the budget that is left after the
    // transcript, not on top of it.
    // Two-layer context injection:
    //   Layer 1 (base context) — durable retrieved memory, project/agent identity.
    //   Layer 2 (dialectic supplement) — the heuristic user model derived from earlier
    //   turns (preferences/habits/goals). Labelled separately so the meter can price each.
    if let Some(memory_text) = memory_text {
        labels.insert(0, "retrieved memory");
        prefix.insert(0, Message::text(Role::System, memory_text, Trust::External));
    }
    if let Some(dialectic_text) = inner.dialectic_supplement() {
        labels.insert(1.min(labels.len()), "dialectic supplement");
        prefix.insert(
            1.min(prefix.len()),
            Message::text(Role::System, dialectic_text, Trust::External),
        );
    }
    // Code-index ranked context: definitions/references for the user's request,
    // falling back to a grep slice when no index exists. Bounded and labelled.
    {
        let q = codeindex_query(request_text);
        if !q.is_empty() {
            // The fallback scan reads files, so it runs on the blocking pool: even bounded,
            // it must not hold an executor thread the stream loop and tools share.
            let root = inner.workspace_root.clone();
            let ctx = tokio::task::spawn_blocking(move || {
                crate::tools::codeindex::codeindex_context(&root, &q)
            })
            .await
            .unwrap_or_default();
            if !ctx.trim().is_empty() {
                // Only inject when something useful was found (index hits or grep hits).
                if ctx.contains("no index") {
                    // Degraded: tell the model a grep fallback ran, without flooding.
                    if let Some(marker) = ctx.find("grep fallback") {
                        let slice = bounded_prefix(&ctx[marker..], 600);
                        labels.push("code index (grep fallback)");
                        prefix.push(Message::text(Role::System, slice, Trust::External));
                    }
                } else {
                    labels.push("code index");
                    prefix.push(Message::text(Role::System, ctx, Trust::External));
                }
            }
        }
    }
    let context_hook = run_hook(
        inner,
        session,
        crate::hooks::HookContext::new(crate::hooks::HookPoint::ContextBuild)
            .with_session(session.id.as_str())
            .with_turn(turn.as_str())
            .with_workspace(inner.workspace_root.display().to_string())
            .with_model(inner.model())
            .with_text(request_text),
    )
    .await;
    if let Some(reason) = &context_hook.effect.deny {
        return Err(reason.clone());
    }
    let mut injected: Vec<String> = hook_injected;
    injected.extend(context_hook.effect.prepend.iter().cloned());
    for (offset, text) in injected.into_iter().enumerate() {
        prefix.insert(offset, Message::text(Role::System, text, Trust::External));
        labels.insert(offset, "plugin context");
    }
    for text in &context_hook.effect.append {
        prefix.push(Message::text(Role::System, text.clone(), Trust::External));
        labels.push("plugin context");
    }
    Ok((prefix, labels))
}

/// The `provider_request` stage's builtin body: apply the `model_request` hooks to the assembled
/// messages and build the [`CompletionRequest`] the provider is sent. `Err(reason)` is the
/// hook's refusal, surfaced as [`StageOutcome::Refused`].
async fn build_completion_request(
    inner: &Arc<RuntimeInner>,
    session: &Arc<SessionRuntime>,
    turn: &TurnId,
    request_text: &str,
    mut messages: Vec<Message>,
) -> Result<CompletionRequest, String> {
    let model_hook = run_hook(
        inner,
        session,
        crate::hooks::HookContext::new(crate::hooks::HookPoint::ModelRequest)
            .with_session(session.id.as_str())
            .with_turn(turn.as_str())
            .with_workspace(inner.workspace_root.display().to_string())
            .with_model(inner.model())
            .with_text(request_text),
    )
    .await;
    if let Some(reason) = &model_hook.effect.deny {
        return Err(reason.clone());
    }
    for text in model_hook.effect.prepend.iter().rev() {
        messages.insert(
            0,
            Message::text(Role::System, text.clone(), Trust::External),
        );
    }
    for text in &model_hook.effect.append {
        messages.push(Message::text(Role::System, text.clone(), Trust::External));
    }
    Ok(CompletionRequest {
        model: inner.model(),
        messages,
        tools: inner.tools.specs(),
        max_output_tokens: model_hook
            .effect
            .max_output_tokens
            .or_else(|| {
                inner
                    .config
                    .read()
                    .unwrap_or_else(|e| e.into_inner())
                    .max_output_tokens
            })
            // Nothing explicit: the registry's per-model figure, so Anthropic stops
            // hard-capping every turn at its 4096 default. Unknown models send no cap —
            // the assumed floor is a budgeting guess, not a provider limit.
            .or_else(|| crate::context_engine::ModelLimits::declared_output_cap(&inner.model())),
    })
}

/// The `tool_dispatch` stage's builtin body: run one round of calls across conflict lanes —
/// reads and calls on different resources proceed side by side, calls that could observe or
/// overwrite each other keep their order — and return `(content, ok)` per call, in call order.
async fn dispatch_tool_calls(
    inner: &Arc<RuntimeInner>,
    session: &Arc<SessionRuntime>,
    calls: &[ToolCall],
    cancel: &vikram_contracts::CancellationToken,
    agent: &Option<AgentRun>,
) -> Vec<(String, bool)> {
    let mut lanes: Vec<Vec<usize>> = Vec::new();
    let mut lane_for: HashMap<String, usize> = HashMap::new();
    for (index, call) in calls.iter().enumerate() {
        match conflict_key(inner, call) {
            Some(key) => match lane_for.get(&key) {
                Some(&lane) => lanes[lane].push(index),
                None => {
                    lane_for.insert(key, lanes.len());
                    lanes.push(vec![index]);
                }
            },
            None => lanes.push(vec![index]),
        }
    }

    let lane_results = futures_util::future::join_all(lanes.iter().map(|lane| async move {
        let mut out: Vec<(usize, String, bool)> = Vec::with_capacity(lane.len());
        for &index in lane {
            if cancel.is_cancelled() {
                out.push((index, "cancelled before completion".to_owned(), false));
                continue;
            }
            let (content, ok) =
                run_tool(inner, session, &calls[index], cancel, agent, Origin::Model).await;
            out.push((index, content, ok));
        }
        out
    }))
    .await;

    let mut by_index: Vec<Option<(String, bool)>> = (0..calls.len()).map(|_| None).collect();
    for lane in lane_results {
        for (index, content, ok) in lane {
            by_index[index] = Some((content, ok));
        }
    }
    by_index
        .into_iter()
        .map(|slot| slot.unwrap_or_else(|| ("cancelled before completion".to_owned(), false)))
        .collect()
}

/// The `result_commit` stage's builtin body: tool results become transcript messages, journal
/// records and events.
fn commit_tool_results(
    inner: &Arc<RuntimeInner>,
    session: &Arc<SessionRuntime>,
    results: Vec<(ToolCallId, String, bool)>,
) {
    for (call_id, content, ok) in results {
        let result = Message::tool_result(call_id, content, ok);
        session.push_message(result.clone(), now_ms());
        let mut journaled = serde_json::json!({"message": result});
        crate::redact::redact_json(&mut journaled);
        inner.journal(&session.id, RecordKind::ToolResult, journaled);
        inner.emit(RuntimeEvent::MessageCommitted {
            session: session.id.clone(),
            message: result,
        });
    }
}

struct DefaultMemoryRecall;

impl StageHandler for DefaultMemoryRecall {
    fn name(&self) -> &'static str {
        "builtin:memory_recall"
    }

    fn handle<'a>(&'a self, ctx: &'a mut StageContext) -> StageFuture<'a> {
        Box::pin(async move {
            let StagePayload::MemoryRecall(payload) = &mut ctx.payload else {
                return Err(StageError::new(
                    ctx.point,
                    self.name(),
                    "memory_recall stage received a different payload",
                ));
            };
            payload.text = recall_memory(&ctx.inner, &ctx.session, &ctx.turn).await;
            Ok(StageOutcome::Done)
        })
    }
}

struct DefaultContextBuild;

impl StageHandler for DefaultContextBuild {
    fn name(&self) -> &'static str {
        "builtin:context_build"
    }

    fn handle<'a>(&'a self, ctx: &'a mut StageContext) -> StageFuture<'a> {
        Box::pin(async move {
            let StagePayload::ContextBuild(payload) = &mut ctx.payload else {
                return Err(StageError::new(
                    ctx.point,
                    self.name(),
                    "context_build stage received a different payload",
                ));
            };
            let hook_injected = std::mem::take(&mut payload.hook_injected);
            let profile = payload.profile.take();
            let memory_text = payload.memory_text.take();
            match assemble_context(
                &ctx.inner,
                &ctx.session,
                &ctx.turn,
                &ctx.request_text,
                hook_injected,
                profile,
                memory_text,
            )
            .await
            {
                Ok((prefix, labels)) => {
                    payload.prefix = prefix;
                    payload.labels = labels;
                    Ok(StageOutcome::Done)
                }
                Err(reason) => Ok(StageOutcome::Refused(reason)),
            }
        })
    }
}

struct DefaultProviderRequest;

impl StageHandler for DefaultProviderRequest {
    fn name(&self) -> &'static str {
        "builtin:provider_request"
    }

    fn handle<'a>(&'a self, ctx: &'a mut StageContext) -> StageFuture<'a> {
        Box::pin(async move {
            let StagePayload::ProviderRequest(payload) = &mut ctx.payload else {
                return Err(StageError::new(
                    ctx.point,
                    self.name(),
                    "provider_request stage received a different payload",
                ));
            };
            let messages = std::mem::take(&mut payload.messages);
            match build_completion_request(
                &ctx.inner,
                &ctx.session,
                &ctx.turn,
                &ctx.request_text,
                messages,
            )
            .await
            {
                Ok(request) => {
                    payload.request = Some(request);
                    Ok(StageOutcome::Done)
                }
                Err(reason) => Ok(StageOutcome::Refused(reason)),
            }
        })
    }
}

struct DefaultToolDispatch;

impl StageHandler for DefaultToolDispatch {
    fn name(&self) -> &'static str {
        "builtin:tool_dispatch"
    }

    fn handle<'a>(&'a self, ctx: &'a mut StageContext) -> StageFuture<'a> {
        Box::pin(async move {
            let StagePayload::ToolDispatch(payload) = &mut ctx.payload else {
                return Err(StageError::new(
                    ctx.point,
                    self.name(),
                    "tool_dispatch stage received a different payload",
                ));
            };
            let outcomes = dispatch_tool_calls(
                &ctx.inner,
                &ctx.session,
                &payload.calls,
                &payload.cancel,
                &payload.agent,
            )
            .await;
            payload.outcomes = outcomes;
            Ok(StageOutcome::Done)
        })
    }
}

struct DefaultResultCommit;

impl StageHandler for DefaultResultCommit {
    fn name(&self) -> &'static str {
        "builtin:result_commit"
    }

    fn handle<'a>(&'a self, ctx: &'a mut StageContext) -> StageFuture<'a> {
        Box::pin(async move {
            let StagePayload::ResultCommit(payload) = &mut ctx.payload else {
                return Err(StageError::new(
                    ctx.point,
                    self.name(),
                    "result_commit stage received a different payload",
                ));
            };
            let results = std::mem::take(&mut payload.results);
            commit_tool_results(&ctx.inner, &ctx.session, results);
            Ok(StageOutcome::Done)
        })
    }
}

struct DefaultUserInput;

impl StageHandler for DefaultUserInput {
    fn name(&self) -> &'static str {
        "builtin:user_input"
    }

    fn handle<'a>(&'a self, ctx: &'a mut StageContext) -> StageFuture<'a> {
        Box::pin(async move {
            let StagePayload::UserInput(payload) = &mut ctx.payload else {
                return Err(StageError::new(
                    ctx.point,
                    self.name(),
                    "user_input stage received a different payload",
                ));
            };
            // A user-input hook sees the message before it is durable: it may refuse it or
            // rewrite the prose the model will read. A rewrite lands in the journaled message
            // itself, so what the plugin changed is what the transcript and the model carry.
            let hook = run_hook(
                &ctx.inner,
                &ctx.session,
                crate::hooks::HookContext::new(crate::hooks::HookPoint::UserInput)
                    .with_session(ctx.session.id.as_str())
                    .with_turn(ctx.turn.as_str())
                    .with_workspace(ctx.inner.workspace_root.display().to_string())
                    .with_model(ctx.inner.model())
                    .with_text(payload.message.text_body()),
            )
            .await;
            if hook.effect.deny.is_none()
                && let Some(text) = hook.effect.text.clone()
            {
                rewrite_message_text(&mut payload.message, text);
            }
            // The user's message is durable before any model work begins.
            let message = payload.message.clone();
            let request_text = message.text_body();
            ctx.session.push_message(message.clone(), now_ms());
            ctx.inner.journal(
                &ctx.session.id,
                RecordKind::Message,
                serde_json::json!({"message": message}),
            );
            ctx.inner.emit(RuntimeEvent::UserMessage {
                session: ctx.session.id.clone(),
                turn: ctx.turn.clone(),
                text: request_text,
            });
            ctx.inner.touch(&ctx.session);
            // The conversation now has something to be named after. Naming runs in the
            // background and is gated to one attempt per session, so this costs nothing on the
            // turn the operator is waiting for.
            ctx.inner.maybe_title_session(&ctx.session);
            payload.deny = hook.effect.deny;
            payload.prepend = hook.effect.prepend;
            payload.append = hook.effect.append;
            Ok(StageOutcome::Done)
        })
    }
}

struct DefaultResponseCommit;

impl StageHandler for DefaultResponseCommit {
    fn name(&self) -> &'static str {
        "builtin:response_commit"
    }

    fn handle<'a>(&'a self, ctx: &'a mut StageContext) -> StageFuture<'a> {
        Box::pin(async move {
            let StagePayload::ResponseCommit(payload) = &mut ctx.payload else {
                return Err(StageError::new(
                    ctx.point,
                    self.name(),
                    "response_commit stage received a different payload",
                ));
            };
            let text = std::mem::take(&mut payload.text);
            let reasoning = std::mem::take(&mut payload.reasoning);
            let calls = std::mem::take(&mut payload.calls);
            deliver_assistant_message(
                &ctx.inner,
                &ctx.session,
                &ctx.turn,
                &text,
                &reasoning,
                &calls,
            )
            .await;
            Ok(StageOutcome::Done)
        })
    }
}

struct DefaultTurnEnd;

impl StageHandler for DefaultTurnEnd {
    fn name(&self) -> &'static str {
        "builtin:turn_end"
    }

    fn handle<'a>(&'a self, ctx: &'a mut StageContext) -> StageFuture<'a> {
        Box::pin(async move {
            if !matches!(ctx.payload, StagePayload::TurnEnd(_)) {
                return Err(StageError::new(
                    ctx.point,
                    self.name(),
                    "turn_end stage received a different payload",
                ));
            }
            // Pass-through: the `turn_end` hooks dispatched alongside already carry every
            // effect, and the outcome is decided — there is nothing left for the stage to do.
            Ok(StageOutcome::Done)
        })
    }
}

struct DefaultSessionLifecycle;

impl StageHandler for DefaultSessionLifecycle {
    fn name(&self) -> &'static str {
        "builtin:session_lifecycle"
    }

    fn handle<'a>(&'a self, ctx: &'a mut StageContext) -> StageFuture<'a> {
        Box::pin(async move {
            if !matches!(ctx.payload, StagePayload::SessionLifecycle(_)) {
                return Err(StageError::new(
                    ctx.point,
                    self.name(),
                    "session_lifecycle stage received a different payload",
                ));
            }
            // Pass-through: the store operation the call site is running is the stage's whole
            // context — `create` may still be vetoed by the outcome at the call site, and every
            // other operation only observes.
            Ok(StageOutcome::Done)
        })
    }
}

struct DefaultAgentLifecycle;

impl StageHandler for DefaultAgentLifecycle {
    fn name(&self) -> &'static str {
        "builtin:agent_lifecycle"
    }

    fn handle<'a>(&'a self, ctx: &'a mut StageContext) -> StageFuture<'a> {
        Box::pin(async move {
            if !matches!(ctx.payload, StagePayload::AgentLifecycle(_)) {
                return Err(StageError::new(
                    ctx.point,
                    self.name(),
                    "agent_lifecycle stage received a different payload",
                ));
            }
            // Pass-through: the spawn and the delivery already happened; there is nothing for
            // the stage to do but observe.
            Ok(StageOutcome::Done)
        })
    }
}

/// The builtin handler for `point`. Called by [`crate::pipeline::StageRegistry::run`] whenever
/// nothing is registered for the point.
pub(crate) fn default_stage_handler(point: StagePoint) -> Option<Arc<dyn StageHandler>> {
    Some(match point {
        StagePoint::ContextBuild => Arc::new(DefaultContextBuild),
        StagePoint::MemoryRecall => Arc::new(DefaultMemoryRecall),
        StagePoint::ProviderRequest => Arc::new(DefaultProviderRequest),
        StagePoint::ToolDispatch => Arc::new(DefaultToolDispatch),
        StagePoint::ResultCommit => Arc::new(DefaultResultCommit),
        StagePoint::UserInput => Arc::new(DefaultUserInput),
        StagePoint::ResponseCommit => Arc::new(DefaultResponseCommit),
        StagePoint::TurnEnd => Arc::new(DefaultTurnEnd),
        StagePoint::SessionLifecycle => Arc::new(DefaultSessionLifecycle),
        StagePoint::AgentLifecycle => Arc::new(DefaultAgentLifecycle),
    })
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

/// The first `limit` bytes of `text`, cut on a char boundary. The grep-fallback excerpt is
/// sliced out of workspace file contents — arbitrary UTF-8 — so a raw byte cut can land inside
/// a multi-byte character and panic the whole turn.
fn bounded_prefix(text: &str, limit: usize) -> String {
    if text.len() <= limit {
        return text.to_owned();
    }
    let mut end = limit;
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    text[..end].to_owned()
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
    // The process-global home lock is deliberately held across the turn: it is what keeps two
    // tests from sharing one `VIKRAM_HOME`, and the guard is not used as a channel between tasks.
    #![allow(clippy::await_holding_lock)]

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

    fn call(name: &str, arguments: serde_json::Value) -> ToolCall {
        ToolCall::new(name, arguments)
    }

    #[test]
    fn a_bot_preambles_itself_with_the_operators_standing_instructions() {
        let profile = crate::types::BotProfile {
            id: vikram_contracts::BotId::new("bot-one"),
            name: "Bot One".into(),
            role: "tag-one, tag-two".into(),
            description: "described by the operator".into(),
            icon: None,
            created_ms: 0,
            soul: "the standing line this bot carries".into(),
        };
        let preamble = bot_preamble(&profile);
        assert!(preamble.contains("You are Bot One"));
        assert!(preamble.contains("never introduce yourself as VIKRAM"));
        assert!(preamble.contains("tag-one, tag-two"));
        assert!(
            preamble.contains("the standing line this bot carries"),
            "the operator's instructions must reach the model: {preamble}"
        );

        // An empty soul adds nothing — the prompt never claims instructions nobody wrote.
        let bare = crate::types::BotProfile {
            soul: String::new(),
            ..profile
        };
        assert!(!bot_preamble(&bare).contains("Standing instructions"));
    }

    #[test]
    fn advice_arrives_once_per_streak_of_three() {
        let mut guard = RepeatGuard::new();

        // The loop from the bug report: a new query and a new tool each time. Nothing is refused,
        // so the only help the turn gets is the plan that arrives with the third failure.
        for (index, name) in ["web.search", "web.search", "browser.dom"]
            .iter()
            .enumerate()
        {
            guard.record(&call(name, serde_json::json!({"query": index})), false);
            if index < 2 {
                assert!(!guard.needs_advice(), "attempt {index} is not a streak yet");
            }
        }

        assert!(
            guard.needs_advice(),
            "three in a row earns a different plan"
        );
        assert!(
            !guard.needs_advice(),
            "the same streak is not nudged twice in a row"
        );
    }

    #[test]
    fn the_worst_tool_is_the_one_the_advice_names() {
        let mut guard = RepeatGuard::new();
        for attempt in 0..3 {
            guard.record(
                &call("browser.dom", serde_json::json!({"url": attempt})),
                false,
            );
        }
        guard.record(
            &call("web.search", serde_json::json!({"query": "x"})),
            false,
        );

        let (name, failures) = guard.worst_tool().expect("a failure was recorded");
        assert_eq!(name, "browser.dom");
        assert_eq!(failures, 3);
    }

    #[test]
    fn a_success_resets_the_failure_streak() {
        let mut guard = RepeatGuard::new();
        guard.record(
            &call("web.search", serde_json::json!({"query": "a"})),
            false,
        );
        guard.record(
            &call("web.search", serde_json::json!({"query": "b"})),
            false,
        );
        guard.record(&call("fs.read", serde_json::json!({"path": "x"})), true);

        guard.record(&call("fs.read", serde_json::json!({"path": "y"})), false);
        assert!(
            !guard.needs_advice(),
            "the turn made progress, so the streak starts over"
        );
    }

    #[test]
    fn a_request_that_never_completed_is_worth_another_attempt() {
        // The transport's fault: this has to be retried, because the model would answer it with a
        // promise instead of a result.
        for content in [
            "error sending request: connection reset by peer",
            "browser.fetch timed out after 15000ms",
            "http answered with status 503",
            "rate limit reached, too many requests",
        ] {
            assert!(is_transient(content), "should be retried: {content}");
        }
    }

    #[test]
    fn a_tool_that_answered_is_never_retried() {
        // The tool's answer, however unhelpful. A second attempt returns the same nothing, and the
        // model can re-plan faster than the clock can be wasted.
        for content in [
            "web search failed: the backend answered with zero results",
            "browser.fetch: 404 not found",
            "read 0 files: no match for *.rs in /tmp",
            "ok",
        ] {
            assert!(!is_transient(content), "should not be retried: {content}");
        }
    }

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

    /// A provider that accepts the request and then says nothing at all. Not an error and not a
    /// close: the connection is open and silent, which is exactly how a stalled endpoint behaves.
    struct SilentProvider;

    impl Provider for SilentProvider {
        fn name(&self) -> &str {
            "silent"
        }

        fn model(&self) -> &str {
            "test-model"
        }

        fn context_window(&self) -> Option<u64> {
            Some(8192)
        }

        fn stream(&self, _request: CompletionRequest) -> ProviderStream {
            Box::pin(futures_util::stream::pending::<
                Result<ProviderEvent, VikramError>,
            >())
        }
    }

    /// A provider that says one word and then holds the stream open forever. This is the shape a
    /// model takes when it starts an answer and then stalls, or streams a long tool-call argument
    /// block that the provider buffers without emitting. The turn must not wait on it forever just
    /// because the first event already arrived.
    struct StallingAfterFirstTokenProvider;

    impl Provider for StallingAfterFirstTokenProvider {
        fn name(&self) -> &str {
            "stalling-after-first-token"
        }

        fn model(&self) -> &str {
            "test-model"
        }

        fn context_window(&self) -> Option<u64> {
            Some(8192)
        }

        fn stream(&self, _request: CompletionRequest) -> ProviderStream {
            use futures_util::StreamExt;
            let first =
                futures_util::stream::iter(vec![Ok(ProviderEvent::TextDelta("thinking".into()))]);
            let rest = futures_util::stream::pending::<Result<ProviderEvent, VikramError>>();
            Box::pin(first.chain(rest))
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

    /// A read-only tool that reports how many siblings were running beside it. Used to prove the
    /// batch really is concurrent rather than merely scheduled together.
    struct OverlapTool {
        active: std::sync::Arc<std::sync::atomic::AtomicUsize>,
        peak: std::sync::Arc<std::sync::atomic::AtomicUsize>,
    }

    impl Tool for OverlapTool {
        fn spec(&self) -> ToolSpec {
            ToolSpec {
                name: "overlap".into(),
                description: "observes its own concurrency".into(),
                schema: serde_json::json!({"type": "object", "properties": {}}),
            }
        }

        fn permission(&self, _args: &serde_json::Value) -> Result<PermissionRequest, VikramError> {
            Ok(PermissionRequest::new(
                ActionKind::Read,
                "overlap.run",
                "overlap",
                SideEffect::None,
                Trust::Model,
            ))
        }

        fn run(
            &self,
            _args: &serde_json::Value,
            _ctx: &ToolContext,
        ) -> Result<ToolOutput, VikramError> {
            use std::sync::atomic::Ordering;
            let now = self.active.fetch_add(1, Ordering::SeqCst) + 1;
            self.peak.fetch_max(now, Ordering::SeqCst);
            std::thread::sleep(std::time::Duration::from_millis(150));
            self.active.fetch_sub(1, Ordering::SeqCst);
            Ok(ToolOutput {
                content: "overlap: done".into(),
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
        run_scripted_with(script, vec![Box::new(ProbeTool)]).await
    }

    /// [`run_scripted`] with the tool set the test chooses.
    #[allow(clippy::await_holding_lock)]
    async fn run_scripted_with(
        script: Vec<Vec<ProviderEvent>>,
        tools: Vec<Box<dyn Tool>>,
    ) -> (Vec<Seen>, Vec<Message>) {
        let dir = tempfile::tempdir().expect("temp dir");
        let _home = isolated_home(dir.path());
        let runtime = Runtime::start_with_provider(
            config_for(dir.path()),
            Arc::new(ScriptedProvider::new(script)),
        )
        .await
        .expect("runtime starts");
        let handle = runtime.handle();
        for tool in tools {
            handle.inner.tools.register(tool);
        }
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

    /// A real 1x1 PNG: the artifact tests need bytes a signature check accepts, not a mock.
    const PNG_1X1: &[u8] = &[
        0x89, 0x50, 0x4E, 0x47, 0x0D, 0x0A, 0x1A, 0x0A, 0x00, 0x00, 0x00, 0x0D, 0x49, 0x48, 0x44,
        0x52, 0x00, 0x00, 0x00, 0x01, 0x00, 0x00, 0x00, 0x01, 0x08, 0x06, 0x00, 0x00, 0x00, 0x1F,
        0x15, 0xC4, 0x89, 0x00, 0x00, 0x00, 0x0A, 0x49, 0x44, 0x41, 0x54, 0x78, 0x9C, 0x63, 0x00,
        0x01, 0x00, 0x00, 0x05, 0x00, 0x01, 0x0D, 0x0A, 0x2D, 0xB4, 0x00, 0x00, 0x00, 0x00, 0x49,
        0x45, 0x4E, 0x44, 0xAE, 0x42, 0x60, 0x82,
    ];

    /// Stands in for `browser.screenshot` without a browser: writes a real PNG into the workspace
    /// and reports it with the same `IMAGE:` marker line the real tool emits.
    struct FakeScreenshot;

    impl Tool for FakeScreenshot {
        fn spec(&self) -> ToolSpec {
            ToolSpec {
                name: "browser.screenshot".into(),
                description: "writes a canned PNG".into(),
                schema: serde_json::json!({"type": "object", "properties": {}}),
            }
        }

        fn permission(&self, _args: &serde_json::Value) -> Result<PermissionRequest, VikramError> {
            Ok(PermissionRequest::new(
                ActionKind::Read,
                "browser.screenshot",
                "shot.png",
                SideEffect::None,
                Trust::Model,
            ))
        }

        fn run(
            &self,
            _args: &serde_json::Value,
            ctx: &ToolContext,
        ) -> Result<ToolOutput, VikramError> {
            let path = ctx.workspace_root.join("shot.png");
            std::fs::write(&path, PNG_1X1).expect("the fake screenshot writes");
            Ok(ToolOutput {
                content: format!("wrote 1x1 screenshot\nIMAGE:{}", path.display()),
                ok: true,
            })
        }
    }

    /// A tool that is not on the artifact allowlist but prints the marker anyway: a fetched page
    /// or file contents could say the same words, so the host must not honor them.
    struct MarkerTool;

    impl Tool for MarkerTool {
        fn spec(&self) -> ToolSpec {
            ToolSpec {
                name: "marker".into(),
                description: "prints an IMAGE: line".into(),
                schema: serde_json::json!({"type": "object", "properties": {}}),
            }
        }

        fn permission(&self, _args: &serde_json::Value) -> Result<PermissionRequest, VikramError> {
            Ok(PermissionRequest::new(
                ActionKind::Read,
                "marker.run",
                "marker",
                SideEffect::None,
                Trust::Model,
            ))
        }

        fn run(
            &self,
            _args: &serde_json::Value,
            ctx: &ToolContext,
        ) -> Result<ToolOutput, VikramError> {
            let path = ctx.workspace_root.join("decoy.png");
            std::fs::write(&path, PNG_1X1).expect("the decoy writes");
            Ok(ToolOutput {
                content: format!("look at this\nIMAGE:{}", path.display()),
                ok: true,
            })
        }
    }

    #[test]
    fn base64_encode_matches_the_rfc_vectors() {
        for (raw, expected) in [
            ("", ""),
            ("f", "Zg=="),
            ("fo", "Zm8="),
            ("foo", "Zm9v"),
            ("foob", "Zm9vYg=="),
            ("fooba", "Zm9vYmE="),
            ("foobar", "Zm9vYmFy"),
        ] {
            assert_eq!(base64_encode(raw.as_bytes()), expected, "{raw:?}");
        }
        assert_eq!(base64_encode(&[0xFB, 0xFF, 0xFE]), "+//+");
        assert_eq!(base64_encode(&[0xFB, 0xFF]), "+/8=");
    }

    #[tokio::test]
    async fn a_screenshot_artifact_rides_the_reply_that_follows_its_result() {
        let (_, messages) = run_scripted_with(
            vec![
                vec![
                    ProviderEvent::ToolCall(ToolCall::new(
                        "browser.screenshot",
                        serde_json::json!({}),
                    )),
                    ProviderEvent::Finished {
                        input_tokens: None,
                        output_tokens: None,
                    },
                ],
                vec![
                    ProviderEvent::TextDelta("here is the shot".into()),
                    ProviderEvent::Finished {
                        input_tokens: None,
                        output_tokens: None,
                    },
                ],
            ],
            vec![Box::new(FakeScreenshot)],
        )
        .await;

        let with_images: Vec<&Message> = messages
            .iter()
            .filter(|message| message.images().next().is_some())
            .collect();
        assert_eq!(
            with_images.len(),
            1,
            "the artifact attaches to exactly one assistant message: {messages:?}"
        );
        let reply = with_images[0];
        assert_eq!(reply.role, Role::Assistant);
        assert_eq!(reply.text_body(), "here is the shot");
        let expected = ImageSource::data("image/png", base64_encode(PNG_1X1));
        assert_eq!(reply.images().next(), Some(&expected));
        // The message that carried the tool call must not carry the artifact: the file did not
        // exist yet when it was committed.
        let asking = messages
            .iter()
            .find(|message| message.tool_calls().next().is_some())
            .expect("the call was committed");
        assert_eq!(asking.images().count(), 0);
    }

    #[tokio::test]
    async fn an_image_marker_from_an_unlisted_tool_is_never_honored() {
        let (_, messages) = run_scripted_with(
            vec![
                vec![
                    ProviderEvent::ToolCall(ToolCall::new("marker", serde_json::json!({}))),
                    ProviderEvent::Finished {
                        input_tokens: None,
                        output_tokens: None,
                    },
                ],
                vec![
                    ProviderEvent::TextDelta("done".into()),
                    ProviderEvent::Finished {
                        input_tokens: None,
                        output_tokens: None,
                    },
                ],
            ],
            vec![Box::new(MarkerTool)],
        )
        .await;

        assert!(
            messages
                .iter()
                .all(|message| message.images().next().is_none()),
            "no message may carry an image the allowlist did not produce"
        );
    }

    /// Push one tool round into a session by hand: an assistant message holding the call, then
    /// the result that answers it.
    fn push_tool_round(session: &Arc<SessionRuntime>, name: &str, result: &str, ok: bool) {
        let call = ToolCall::new(name, serde_json::json!({}));
        session.push_message(
            Message {
                role: Role::Assistant,
                blocks: vec![Block::ToolCall { call: call.clone() }],
            },
            now_ms(),
        );
        session.push_message(
            Message::tool_result(call.id, result.to_owned(), ok),
            now_ms(),
        );
    }

    #[tokio::test]
    async fn image_artifacts_must_be_real_pngs_inside_the_workspace() {
        let dir = tempfile::tempdir().expect("temp dir");
        let _home = isolated_home(dir.path());
        let outside = tempfile::tempdir().expect("outside temp dir");
        std::fs::write(outside.path().join("out.png"), PNG_1X1).expect("outside png");
        std::fs::write(dir.path().join("text.png"), b"not a png at all").expect("fake png");
        let runtime = Runtime::start_with_provider(
            config_for(dir.path()),
            Arc::new(ScriptedProvider::new(vec![])),
        )
        .await
        .expect("runtime starts");
        let handle = runtime.handle();
        let session_id = named_session(&handle);
        let session = handle.inner.session(&session_id).expect("session");
        session.push_message(Message::user("audit"), now_ms());

        let inside = dir.path().join("shot.png");
        std::fs::write(&inside, PNG_1X1).expect("inside png");

        // A real screenshot artifact attaches, once even when a result names the same file twice.
        let marker = format!("wrote it\nIMAGE:{0}\nIMAGE:{0}", inside.display());
        push_tool_round(&session, "browser.screenshot", &marker, true);
        let artifacts = pending_image_artifacts(&handle.inner, &session);
        assert_eq!(artifacts.len(), 1, "duplicates collapse to one image");
        assert_eq!(
            artifacts[0],
            ImageSource::data("image/png", base64_encode(PNG_1X1))
        );

        // Committed means consumed: once an assistant message follows the results, the same
        // artifacts are not offered again.
        session.push_message(
            Message::text(Role::Assistant, "here", Trust::Model),
            now_ms(),
        );
        assert!(pending_image_artifacts(&handle.inner, &session).is_empty());

        // `computer.screenshot` is on the same allowlist: its destination is workspace-confined
        // by the tool itself, so an `IMAGE:` line it emits names a file the host can honor.
        let desktop = dir.path().join("desktop.png");
        std::fs::write(&desktop, PNG_1X1).expect("desktop png");
        push_tool_round(
            &session,
            "computer.screenshot",
            &format!("screenshot saved\nIMAGE:{}", desktop.display()),
            true,
        );
        let artifacts = pending_image_artifacts(&handle.inner, &session);
        assert_eq!(artifacts.len(), 1, "computer.screenshot attaches its PNG");
        assert_eq!(
            artifacts[0],
            ImageSource::data("image/png", base64_encode(PNG_1X1))
        );
        session.push_message(
            Message::text(Role::Assistant, "the desktop", Trust::Model),
            now_ms(),
        );
        assert!(pending_image_artifacts(&handle.inner, &session).is_empty());

        // `computer.read_screen` keeps its capture under `.vikram/shots/` and marks it the same
        // way, so the OCR result's screenshot attaches too.
        let screen = dir.path().join(".vikram/shots/read_screen-latest.png");
        std::fs::create_dir_all(screen.parent().expect("shots dir")).expect("shots dir");
        std::fs::write(&screen, PNG_1X1).expect("screen png");
        push_tool_round(
            &session,
            "computer.read_screen",
            &format!("some text\nIMAGE:{}", screen.display()),
            true,
        );
        let artifacts = pending_image_artifacts(&handle.inner, &session);
        assert_eq!(artifacts.len(), 1, "computer.read_screen attaches its PNG");
        assert_eq!(
            artifacts[0],
            ImageSource::data("image/png", base64_encode(PNG_1X1))
        );
        session.push_message(
            Message::text(Role::Assistant, "the screen text", Trust::Model),
            now_ms(),
        );
        assert!(pending_image_artifacts(&handle.inner, &session).is_empty());

        // Everything the marker could lie about is refused: other tools, failed results, paths
        // outside the workspace, files that are not PNGs, and files over the size cap.
        let mut big = PNG_1X1.to_vec();
        big.resize(MAX_IMAGE_ARTIFACT_BYTES as usize + 1, 0);
        std::fs::write(dir.path().join("big.png"), &big).expect("oversized png");
        let outside_marker = format!("IMAGE:{}", outside.path().join("out.png").display());
        let text_marker = format!("IMAGE:{}", dir.path().join("text.png").display());
        let big_marker = format!("IMAGE:{}", dir.path().join("big.png").display());
        for (name, result, ok) in [
            ("web.fetch", marker.as_str(), true),
            ("browser.screenshot", marker.as_str(), false),
            ("browser.screenshot", outside_marker.as_str(), true),
            ("computer.screenshot", outside_marker.as_str(), true),
            ("computer.read_screen", outside_marker.as_str(), true),
            ("browser.screenshot", text_marker.as_str(), true),
            ("computer.screenshot", text_marker.as_str(), true),
            ("computer.read_screen", text_marker.as_str(), true),
            ("browser.screenshot", big_marker.as_str(), true),
            ("computer.screenshot", big_marker.as_str(), true),
            ("computer.read_screen", big_marker.as_str(), true),
        ] {
            push_tool_round(&session, name, result, ok);
            assert!(
                pending_image_artifacts(&handle.inner, &session).is_empty(),
                "{name} result must not attach: {result}"
            );
        }
    }

    #[tokio::test]
    async fn a_provider_that_never_answers_is_abandoned_instead_of_freezing_the_turn() {
        let dir = tempfile::tempdir().expect("temp dir");
        let _home = isolated_home(dir.path());
        let mut config = config_for(dir.path());
        // One second of patience: the watchdog is checked on every pass now, so it would fire on
        // the first iteration with zero and prove nothing about a real quiet window.
        config.stream_silence_secs = 1;
        let runtime = Runtime::start_with_provider(config, Arc::new(SilentProvider))
            .await
            .expect("runtime starts");
        let handle = runtime.handle();
        let mut events = handle.events();
        let session = named_session(&handle);
        handle
            .submit(&session, "tell me the news".into())
            .expect("the turn is accepted");

        let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(5);
        let mut complaint = None;
        while tokio::time::Instant::now() < deadline {
            let Ok(Ok(event)) =
                tokio::time::timeout(std::time::Duration::from_millis(100), events.recv()).await
            else {
                continue;
            };
            match event {
                RuntimeEvent::Error { text } => complaint = Some(text),
                RuntimeEvent::TurnFinished { .. } => break,
                _ => {}
            }
        }

        let text = complaint.expect("the operator is told the endpoint went silent");
        assert!(text.contains("sent nothing for"), "unexpected text: {text}");
    }

    #[tokio::test]
    async fn a_stream_that_stalls_after_its_first_token_is_abandoned() {
        let dir = tempfile::tempdir().expect("temp dir");
        let _home = isolated_home(dir.path());
        let mut config = config_for(dir.path());
        // One second, not zero: the check runs before any item is read, so a zero cap would abort
        // before the first token and never exercise the "stall after output" case.
        config.stream_silence_secs = 1;
        let runtime =
            Runtime::start_with_provider(config, Arc::new(StallingAfterFirstTokenProvider))
                .await
                .expect("runtime starts");
        let handle = runtime.handle();
        let mut events = handle.events();
        let session = named_session(&handle);
        handle
            .submit(&session, "tell me the news".into())
            .expect("the turn is accepted");

        let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(5);
        let mut complaint = None;
        while tokio::time::Instant::now() < deadline {
            let Ok(Ok(event)) =
                tokio::time::timeout(std::time::Duration::from_millis(100), events.recv()).await
            else {
                continue;
            };
            match event {
                RuntimeEvent::Error { text } => complaint = Some(text),
                RuntimeEvent::TurnFinished { .. } => break,
                _ => {}
            }
        }

        let text = complaint.expect("a stall after the first token must still be reported");
        assert!(text.contains("sent nothing for"), "unexpected text: {text}");
        assert!(
            text.contains("already written is kept"),
            "partial work should be acknowledged: {text}"
        );
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
    async fn a_tool_only_step_still_records_the_call_it_made() {
        // No speech, no reasoning, one call: the transcript must still contain the assistant turn
        // that asked for it. Otherwise the next request is an unexplained tool result and a model
        // that cannot see its own call repeats it under a new name.
        let call = ToolCall::new("probe", serde_json::json!({"step": 1}));
        let call_id = call.id.clone();
        let (_, messages) = run_scripted(vec![
            vec![
                ProviderEvent::ToolCall(call),
                ProviderEvent::Finished {
                    input_tokens: None,
                    output_tokens: None,
                },
            ],
            vec![
                ProviderEvent::TextDelta("done".into()),
                ProviderEvent::Finished {
                    input_tokens: None,
                    output_tokens: None,
                },
            ],
        ])
        .await;

        let made: Vec<&ToolCallId> = messages
            .iter()
            .flat_map(|message| &message.blocks)
            .filter_map(|block| match block {
                Block::ToolCall { call } => Some(&call.id),
                _ => None,
            })
            .collect();
        assert_eq!(made, vec![&call_id], "the call travels in the transcript");
        let asking = messages
            .iter()
            .position(|message| message.role == Role::Assistant)
            .expect("an assistant turn was committed");
        let answered = messages
            .iter()
            .position(|message| message.role == Role::Tool)
            .expect("the result was committed");
        assert!(
            asking < answered,
            "the call must be recorded before the result that answers it"
        );
        assert!(
            crate::compaction::pairing_is_consistent(&messages),
            "every call keeps its result, so a fold cannot orphan one"
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
    async fn read_only_calls_in_one_batch_run_concurrently() {
        use std::sync::atomic::{AtomicUsize, Ordering};
        let dir = tempfile::tempdir().expect("temp dir");
        let _home = isolated_home(dir.path());
        let runtime = Runtime::start_with_provider(
            config_for(dir.path()),
            Arc::new(ScriptedProvider::new(vec![
                vec![
                    ProviderEvent::ToolCall(ToolCall::new("overlap", serde_json::json!({}))),
                    ProviderEvent::ToolCall(ToolCall::new("overlap", serde_json::json!({}))),
                    finished(),
                ],
                vec![finished()],
            ])),
        )
        .await
        .expect("runtime starts");
        let handle = runtime.handle();
        let active = Arc::new(AtomicUsize::new(0));
        let peak = Arc::new(AtomicUsize::new(0));
        handle.inner.tools.register(Box::new(OverlapTool {
            active: Arc::clone(&active),
            peak: Arc::clone(&peak),
        }));
        let mut events = handle.events();
        let session = named_session(&handle);
        handle
            .submit(&session, "look at both at once".into())
            .expect("the turn is accepted");
        run_to_end(&mut events).await;

        assert_eq!(
            peak.load(Ordering::SeqCst),
            2,
            "both read-only calls must be in flight together"
        );
        let results = handle
            .inner
            .session(&session)
            .expect("session survives the turn")
            .messages()
            .iter()
            .filter(|m| m.role == Role::Tool)
            .count();
        assert_eq!(results, 2, "every call still gets a result message");
    }

    #[tokio::test]
    async fn a_model_repeating_a_failing_call_keeps_working_instead_of_being_cut_off() {
        use std::sync::atomic::{AtomicUsize, Ordering};
        let dir = tempfile::tempdir().expect("temp dir");
        let _home = isolated_home(dir.path());
        let stuck = serde_json::json!({"url": "https://techcrunch.com/tagged/ai/"});
        let mut script: Vec<Vec<ProviderEvent>> = (0..8)
            .map(|_| {
                vec![
                    ProviderEvent::ToolCall(ToolCall::new("flaky", stuck.clone())),
                    finished(),
                ]
            })
            .collect();
        script.push(vec![finished()]);
        let runtime = Runtime::start_with_provider(
            config_for(dir.path()),
            Arc::new(ScriptedProvider::new(script)),
        )
        .await
        .expect("runtime starts");
        let handle = runtime.handle();
        let runs = Arc::new(AtomicUsize::new(0));
        handle.inner.tools.register(Box::new(AlwaysFailsTool {
            runs: Arc::clone(&runs),
        }));
        let mut events = handle.events();
        let session = named_session(&handle);
        handle
            .submit(&session, "what is the news".into())
            .expect("the turn is accepted");
        run_to_end(&mut events).await;

        assert_eq!(
            runs.load(Ordering::SeqCst),
            8,
            "every call the model asked for runs, however many have failed before it"
        );
        let messages = handle
            .inner
            .session(&session)
            .expect("session survives the turn")
            .messages();
        let refusals = messages
            .iter()
            .filter(|message| message.role == Role::Tool && message.text_body().contains("refused"))
            .count();
        assert_eq!(refusals, 0, "nothing is refused on a counter");
        assert_eq!(
            messages.iter().filter(|m| m.role == Role::Tool).count(),
            8,
            "every call the model asked for still gets an answer"
        );
    }

    /// The loop from the bug report: every attempt used a new query or a new tool, so the
    /// repetition guards (exact call, then tool name) never fired and the turn spent the whole
    /// step budget saying "let me try" in different words. Three failures in a row is the signal
    /// the guards cannot see, so the turn is told to answer instead of buying a fourth attempt.
    #[tokio::test]
    async fn a_turn_that_fails_in_new_ways_answers_instead_of_retrying_forever() {
        use std::sync::atomic::{AtomicUsize, Ordering};
        let dir = tempfile::tempdir().expect("temp dir");
        let _home = isolated_home(dir.path());
        // Three failing attempts, then the model answers. The remaining batches are spare: the turn
        // should never reach them.
        let mut script: Vec<Vec<ProviderEvent>> = (0..3)
            .map(|attempt| {
                vec![
                    ProviderEvent::ToolCall(ToolCall::new(
                        "flaky",
                        serde_json::json!({"query": format!("today news {attempt}")}),
                    )),
                    finished(),
                ]
            })
            .collect();
        script.push(vec![
            ProviderEvent::TextDelta("I could not reach the news: search returned nothing.".into()),
            finished(),
        ]);
        script.extend((3..8).map(|attempt| {
            vec![
                ProviderEvent::ToolCall(ToolCall::new(
                    "flaky",
                    serde_json::json!({"query": format!("today news {attempt}")}),
                )),
                finished(),
            ]
        }));
        let runtime = Runtime::start_with_provider(
            config_for(dir.path()),
            Arc::new(ScriptedProvider::new(script)),
        )
        .await
        .expect("runtime starts");
        let handle = runtime.handle();
        let runs = Arc::new(AtomicUsize::new(0));
        handle.inner.tools.register(Box::new(AlwaysFailsTool {
            runs: Arc::clone(&runs),
        }));
        let mut events = handle.events();
        let session = named_session(&handle);
        handle
            .submit(&session, "tell me today's news".into())
            .expect("the turn is accepted");
        run_to_end(&mut events).await;

        assert_eq!(
            runs.load(Ordering::SeqCst),
            3,
            "three attempts, then the model was handed a new plan and answered instead of retrying"
        );
        let messages = handle
            .inner
            .session(&session)
            .expect("session survives the turn")
            .messages();
        assert!(
            messages.iter().any(|m| m.role == Role::System
                && m.text_body()
                    .contains("Do not repeat a call that already failed")),
            "the model is handed a different plan in the transcript"
        );
        assert!(
            messages.iter().any(|m| m.role == Role::Assistant
                && m.text_body().contains("I could not reach the news")),
            "the operator still gets the honest answer when the work runs out"
        );
    }

    /// Failures never take the tools away: the model keeps its whole toolbox for the whole turn.
    #[tokio::test]
    async fn a_model_that_keeps_reaching_for_tools_is_never_stopped_by_the_count() {
        use std::sync::atomic::{AtomicUsize, Ordering};
        let dir = tempfile::tempdir().expect("temp dir");
        let _home = isolated_home(dir.path());
        let script: Vec<Vec<ProviderEvent>> = (0..8)
            .map(|attempt| {
                vec![
                    ProviderEvent::ToolCall(ToolCall::new(
                        "flaky",
                        serde_json::json!({"url": format!("https://example.com/{attempt}")}),
                    )),
                    finished(),
                ]
            })
            .collect();
        let runtime = Runtime::start_with_provider(
            config_for(dir.path()),
            Arc::new(ScriptedProvider::new(script)),
        )
        .await
        .expect("runtime starts");
        let handle = runtime.handle();
        let runs = Arc::new(AtomicUsize::new(0));
        handle.inner.tools.register(Box::new(AlwaysFailsTool {
            runs: Arc::clone(&runs),
        }));
        let mut events = handle.events();
        let session = named_session(&handle);
        handle
            .submit(&session, "tell me today's news".into())
            .expect("the turn is accepted");
        run_to_end(&mut events).await;

        assert_eq!(
            runs.load(Ordering::SeqCst),
            8,
            "eight distinct attempts all reach the tool, because none of them repeated"
        );
        let messages = handle
            .inner
            .session(&session)
            .expect("session survives the turn")
            .messages();
        assert!(
            !messages
                .iter()
                .any(|m| m.role == Role::Tool && m.text_body().contains("refused")),
            "nothing was refused: a new attempt is not a repetition"
        );
    }

    struct AlwaysFailsTool {
        runs: Arc<std::sync::atomic::AtomicUsize>,
    }

    impl Tool for AlwaysFailsTool {
        fn spec(&self) -> ToolSpec {
            ToolSpec {
                name: "flaky".into(),
                description: "fails every time".into(),
                schema: serde_json::json!({"type": "object", "properties": {"url": {"type": "string"}}}),
            }
        }

        fn permission(&self, _args: &serde_json::Value) -> Result<PermissionRequest, VikramError> {
            Ok(PermissionRequest::new(
                ActionKind::Read,
                "flaky.run",
                "flaky",
                SideEffect::None,
                Trust::Model,
            ))
        }

        fn run(
            &self,
            _args: &serde_json::Value,
            _ctx: &ToolContext,
        ) -> Result<ToolOutput, VikramError> {
            self.runs.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            Ok(ToolOutput {
                content: "fetch failed: 403 Forbidden".into(),
                ok: false,
            })
        }
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
            sent.iter().any(crate::compaction::is_compaction_summary),
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

        let profile = vikram_contracts::BotId::new("delegatee");
        handle.inner.bots.add(Arc::new(ManagedBot::from_profile(
            crate::types::BotProfile {
                id: profile.clone(),
                name: "Rudra".into(),
                role: "reviewer".into(),
                description: "reads code".into(),
                icon: None,
                created_ms: now_ms(),
                soul: String::new(),
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

        // A chain deeper than the floor is refused, not silently allowed to nest. The child was
        // reaped once it answered, so it is reopened before it can be asked to delegate again.
        if handle.inner.session(&child_id).is_none() {
            handle
                .open_session(&child_id)
                .expect("the reaped child reopens");
        }
        let child = handle.inner.session(&child_id).expect("the child session");
        child.set_depth(handle.inner.policy().max_delegation_depth());
        let refused = handle.spawn_agent(
            vikram_contracts::BotId::new("delegatee"),
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
                ActionKind::Write,
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
    async fn the_stop_key_ends_a_turn_that_has_not_produced_a_token_yet() {
        let dir = tempfile::tempdir().expect("temp dir");
        let _home = isolated_home(dir.path());
        let runtime =
            Runtime::start_with_provider(config_for(dir.path()), Arc::new(SilentProvider))
                .await
                .expect("runtime starts");
        let handle = runtime.handle();
        let session = named_session(&handle);
        handle
            .submit(&session, "say something".into())
            .expect("the turn is accepted");
        // Long enough for the turn to be waiting on a stream that will never speak.
        tokio::time::sleep(std::time::Duration::from_millis(80)).await;
        handle.cancel(&session).expect("the stop is accepted");

        let deadline = Instant::now() + std::time::Duration::from_secs(2);
        while handle
            .inner
            .session(&session)
            .expect("the session survives")
            .is_busy()
        {
            assert!(
                Instant::now() < deadline,
                "a stop must not wait for a token that is not coming"
            );
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }
    }

    #[tokio::test]
    async fn a_cancelled_batch_still_answers_the_call_that_never_ran() {
        let dir = tempfile::tempdir().expect("temp dir");
        let _home = isolated_home(dir.path());
        let stop = ToolCall::new("stop-turn", serde_json::json!({}));
        // Same tool, so both calls share one lane: the second is still queued when the first
        // cancels the turn, which is the only way a call can genuinely never run.
        let unrun = ToolCall::new("stop-turn", serde_json::json!({}));
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
            .learning
            .evolution
            .lock()
            .expect("evolution lock")
            .experience
            .count()
            .expect("the store reads");
        assert_eq!(experiences, 1, "the turn became one experience");

        let pending = handle.inner.learning.surface.pending_proposals();
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

        let audit = handle.inner.learning.surface.audit_log();
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
        let staged = handle.inner.learning.surface.pending_proposals();
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
            let pending = handle.inner.learning.surface.pending_proposals();
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
            .learning
            .evolution
            .lock()
            .expect("evolution lock")
            .experience
            .count()
            .expect("the store reads");
        assert_eq!(experiences, 0, "nothing was done, so nothing was learned");
        assert!(handle.inner.learning.surface.pending_proposals().is_empty());
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
            .surface
            .set_policy_memory(LearningPolicy::Disabled);
        handle
            .inner
            .learning
            .surface
            .set_policy_skill(LearningPolicy::Disabled);
        let mut events = handle.events();
        let session = named_session(&handle);
        handle
            .submit(&session, "audit the workspace".into())
            .expect("the turn is accepted");
        let _ = record(&mut events).await;

        let experiences = handle
            .inner
            .learning
            .evolution
            .lock()
            .expect("evolution lock")
            .experience
            .count()
            .expect("the store reads");
        assert_eq!(experiences, 0, "a disabled policy means off, not quiet");
        assert!(handle.inner.learning.surface.pending_proposals().is_empty());
    }

    #[tokio::test]
    async fn memory_nudge_due_uses_session_message_count() {
        let dir = tempfile::tempdir().expect("temp dir");
        let _home = isolated_home(dir.path());
        let config = Config {
            model: "test-model".into(),
            workspace_root: Some(dir.path().to_path_buf()),
            memory_nudge_interval: 2,
            ..Config::default()
        };
        let runtime = Runtime::start_with_provider(config, Arc::new(ScriptedProvider::new(vec![])))
            .await
            .expect("runtime starts");
        let handle = runtime.handle();
        let session_id = handle.snapshot().sessions[0].id.clone();
        let session = handle.inner.session(&session_id).expect("session");
        assert!(!handle.inner.memory_nudge_due(&session));
        session.push_message(vikram_contracts::Message::user("one"), 1);
        assert!(!handle.inner.memory_nudge_due(&session));
        session.push_message(vikram_contracts::Message::user("two"), 2);
        assert!(handle.inner.memory_nudge_due(&session));

        // A zero interval never nudges.
        //
        // `isolated_home` takes a process-global, non-reentrant lock. Take turns instead of
        // holding the first guard while asking for the second, or this test deadlocks itself and
        // every sibling test queued behind it.
        drop(_home);
        let dir2 = tempfile::tempdir().expect("temp dir");
        let _home2 = isolated_home(dir2.path());
        let config2 = Config {
            model: "test-model".into(),
            workspace_root: Some(dir2.path().to_path_buf()),
            memory_nudge_interval: 0,
            ..Config::default()
        };
        let runtime2 =
            Runtime::start_with_provider(config2, Arc::new(ScriptedProvider::new(vec![])))
                .await
                .expect("runtime starts");
        let handle2 = runtime2.handle();
        let sid2 = handle2.snapshot().sessions[0].id.clone();
        let session2 = handle2.inner.session(&sid2).expect("session");
        session2.push_message(vikram_contracts::Message::user("one"), 1);
        assert!(!handle2.inner.memory_nudge_due(&session2));
    }

    fn install_hook_plugin(dir: &Path, id: &str, manifest: serde_json::Value) {
        let plugin = dir.join("home").join("plugins").join(id);
        std::fs::create_dir_all(&plugin).expect("plugin directory");
        std::fs::write(plugin.join("plugin.json"), manifest.to_string()).expect("plugin manifest");
    }

    #[tokio::test]
    #[allow(clippy::await_holding_lock)]
    async fn a_tool_result_hook_rewrites_what_the_model_reads() {
        let dir = tempfile::tempdir().expect("temp dir");
        let _home = isolated_home(dir.path());
        install_hook_plugin(
            dir.path(),
            "redact-probe",
            serde_json::json!({
                "id": "redact-probe",
                "version": "1.0.0",
                "components": {"hooks": ["tool_result"]},
                "permissions": ["tools"],
                "priority": 50,
                "hook_rules": [{
                    "point": "tool_result",
                    "tool": "probe",
                    "result_contain": "probe: done",
                    "replace_result": "probe: withheld by a plugin"
                }]
            }),
        );
        let call = ToolCall::new("probe", serde_json::json!({}));
        let provider = Arc::new(ScriptedProvider::new(vec![
            vec![ProviderEvent::ToolCall(call), finished()],
            vec![ProviderEvent::TextDelta("finished".into()), finished()],
        ]));
        let runtime = Runtime::start_with_provider(config_for(dir.path()), provider)
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

        let content = seen
            .iter()
            .find_map(|event| match event {
                Seen::ToolResult(_, content) => Some(content.clone()),
                _ => None,
            })
            .expect("the tool ran and produced a result");
        assert_eq!(content, "probe: withheld by a plugin");
    }

    #[tokio::test]
    #[allow(clippy::await_holding_lock)]
    async fn a_tool_call_hook_stops_the_tool_before_it_runs() {
        let dir = tempfile::tempdir().expect("temp dir");
        let _home = isolated_home(dir.path());
        install_hook_plugin(
            dir.path(),
            "deny-probe",
            serde_json::json!({
                "id": "deny-probe",
                "version": "1.0.0",
                "components": {"hooks": ["tool_call"]},
                "permissions": ["tools"],
                "priority": 100,
                "hook_rules": [{
                    "point": "tool_call",
                    "tool": "probe",
                    "deny": "probe is disabled by a plugin"
                }]
            }),
        );
        let call = ToolCall::new("probe", serde_json::json!({}));
        let provider = Arc::new(ScriptedProvider::new(vec![vec![
            ProviderEvent::ToolCall(call),
            finished(),
        ]]));
        let runtime = Runtime::start_with_provider(config_for(dir.path()), provider.clone())
            .await
            .expect("runtime starts");
        let handle = runtime.handle();
        handle.inner.tools.register(Box::new(ProbeTool));
        let mut events = handle.events();
        let session = named_session(&handle);
        handle
            .submit(&session, "audit the code".into())
            .expect("the turn is accepted");
        let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(5);
        let mut failed = false;
        let mut refused = None;
        while tokio::time::Instant::now() < deadline {
            let Ok(Ok(event)) =
                tokio::time::timeout(std::time::Duration::from_millis(100), events.recv()).await
            else {
                continue;
            };
            match event {
                RuntimeEvent::ToolFinished { name, ok, .. } if name == "probe" => {
                    failed |= !ok;
                }
                RuntimeEvent::MessageCommitted { message, .. } if message.role == Role::Tool => {
                    for block in message.blocks {
                        if let Block::ToolResult { content, .. } = block {
                            refused = Some(content);
                        }
                    }
                }
                RuntimeEvent::TurnFinished { .. } => break,
                _ => {}
            }
        }

        assert!(failed, "the refusal is published as a failed call");
        let content = refused.expect("the refusal is recorded as the call's result");
        assert!(
            content.contains("denied by plugin: probe is disabled by a plugin"),
            "unexpected result: {content}"
        );
        let requests = provider.taken();
        assert_eq!(
            requests.len(),
            2,
            "the model is asked to react to the refusal"
        );
        assert!(
            requests
                .last()
                .into_iter()
                .flat_map(|messages| messages.iter())
                .flat_map(|message| message.blocks.iter())
                .any(|block| matches!(block, Block::ToolResult { content, .. } if content.contains("denied by plugin"))),
            "the second request carries the refusal"
        );
    }

    #[test]
    fn the_grep_fallback_slice_never_cuts_a_multibyte_character() {
        // 598 ASCII bytes after the marker put the 600-byte cut inside the three-byte '€'.
        let mut text = "grep fallback ".to_owned();
        text.push_str(&"a".repeat(598));
        text.push('€');
        text.push_str(&"b".repeat(50));
        let excerpt = bounded_prefix(&text["grep fallback".len()..], 600);
        assert_eq!(excerpt.len(), 599, "the cut walks back to a boundary");
        assert!(!excerpt.contains('€'));
        assert!(excerpt.is_char_boundary(excerpt.len()));
    }

    #[tokio::test]
    async fn a_guard_dropped_mid_turn_releases_the_claim_and_reports_failure() {
        let dir = tempfile::tempdir().expect("temp dir");
        let _home = isolated_home(dir.path());
        let runtime = Runtime::start_with_provider(
            config_for(dir.path()),
            Arc::new(ScriptedProvider::new(vec![])),
        )
        .await
        .expect("runtime starts");
        let handle = runtime.handle();
        let session = named_session(&handle);
        let live = handle.inner.session(&session).expect("live session");
        live.claim_turn();
        live.set_turn(TurnState::Streaming);

        let guard = TurnGuard {
            inner: Arc::clone(&handle.inner),
            session: Arc::clone(&live),
            turn: TurnId::generate(),
            agent: None,
            armed: std::cell::Cell::new(true),
        };
        drop(guard);

        assert!(
            !live.turn_claimed(),
            "the panic path must hand the slot back"
        );
        assert_eq!(live.turn(), TurnState::Failed);
        assert!(!live.is_busy(), "the session must not stay wedged");
    }

    #[tokio::test]
    async fn a_disarmed_guard_leaves_the_finished_turn_alone() {
        let dir = tempfile::tempdir().expect("temp dir");
        let _home = isolated_home(dir.path());
        let runtime = Runtime::start_with_provider(
            config_for(dir.path()),
            Arc::new(ScriptedProvider::new(vec![])),
        )
        .await
        .expect("runtime starts");
        let handle = runtime.handle();
        let session = named_session(&handle);
        let live = handle.inner.session(&session).expect("live session");
        live.claim_turn();
        live.set_turn(TurnState::Streaming);

        let guard = TurnGuard {
            inner: Arc::clone(&handle.inner),
            session: Arc::clone(&live),
            turn: TurnId::generate(),
            agent: None,
            armed: std::cell::Cell::new(true),
        };
        guard.disarm();
        drop(guard);

        assert!(live.turn_claimed(), "finish owns the release once disarmed");
        assert_eq!(live.turn(), TurnState::Streaming);
    }

    /// A provider whose stream panics the moment it is polled — the shape a transport bug or a
    /// poisoned lock inside a provider takes. The turn must still end: the guard reports it
    /// failed and frees the slot instead of wedging the session.
    struct PanickingProvider;

    impl Provider for PanickingProvider {
        fn name(&self) -> &str {
            "panicking"
        }

        fn model(&self) -> &str {
            "test-model"
        }

        fn context_window(&self) -> Option<u64> {
            Some(8192)
        }

        fn stream(&self, _request: CompletionRequest) -> ProviderStream {
            Box::pin(futures_util::stream::poll_fn(|_| {
                panic!("provider exploded mid-stream")
            }))
        }
    }

    #[tokio::test]
    async fn a_panicking_turn_still_ends_and_frees_the_session() {
        let dir = tempfile::tempdir().expect("temp dir");
        let _home = isolated_home(dir.path());
        let runtime =
            Runtime::start_with_provider(config_for(dir.path()), Arc::new(PanickingProvider))
                .await
                .expect("runtime starts");
        let handle = runtime.handle();
        let mut events = handle.events();
        let session = named_session(&handle);
        handle
            .submit(&session, "audit the code".into())
            .expect("the turn is accepted");

        let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(5);
        let mut finished = None;
        while tokio::time::Instant::now() < deadline {
            let Ok(Ok(event)) =
                tokio::time::timeout(std::time::Duration::from_millis(100), events.recv()).await
            else {
                continue;
            };
            if let RuntimeEvent::TurnFinished { state, .. } = event {
                finished = Some(state);
                break;
            }
        }
        assert_eq!(
            finished,
            Some(TurnState::Failed),
            "the guard reports the panicked turn as failed"
        );
        let live = handle.inner.session(&session).expect("session survives");
        assert!(!live.turn_claimed(), "the wedged claim is released");
        assert!(!live.is_busy());
    }

    /// The hook points a session's journal recorded, in order.
    fn hook_points(handle: &crate::runtime::RuntimeHandle, session: &SessionId) -> Vec<String> {
        handle
            .inner
            .store
            .replay_messages(session)
            .unwrap_or_default()
            .into_iter()
            .filter(|record| record.kind == RecordKind::Hook)
            .filter_map(|record| {
                record
                    .payload
                    .get("point")
                    .and_then(|point| point.as_str())
                    .map(str::to_owned)
            })
            .collect()
    }

    /// Drain events until the turn ends.
    async fn drain_turn(events: &mut broadcast::Receiver<RuntimeEvent>) {
        let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(5);
        while tokio::time::Instant::now() < deadline {
            let Ok(Ok(event)) =
                tokio::time::timeout(std::time::Duration::from_millis(100), events.recv()).await
            else {
                continue;
            };
            if matches!(event, RuntimeEvent::TurnFinished { .. }) {
                return;
            }
        }
        panic!("the turn never finished");
    }

    #[tokio::test]
    #[allow(clippy::await_holding_lock)]
    async fn a_user_input_hook_rewrites_what_the_model_reads() {
        let dir = tempfile::tempdir().expect("temp dir");
        let _home = isolated_home(dir.path());
        install_hook_plugin(
            dir.path(),
            "rewrite-input",
            serde_json::json!({
                "id": "rewrite-input",
                "version": "1.0.0",
                "components": {"hooks": ["user_input"]},
                "permissions": ["agents"],
                "hook_rules": [{
                    "point": "user_input",
                    "text_contain": "audit",
                    "replace_text": "a rewritten request"
                }]
            }),
        );
        let provider = Arc::new(ScriptedProvider::new(vec![vec![
            ProviderEvent::TextDelta("done".into()),
            finished(),
        ]]));
        let runtime = Runtime::start_with_provider(config_for(dir.path()), provider.clone())
            .await
            .expect("runtime starts");
        let handle = runtime.handle();
        let mut events = handle.events();
        let session = named_session(&handle);
        handle
            .submit(&session, "audit the code".into())
            .expect("the turn is accepted");
        let _seen = record(&mut events).await;

        let requests = provider.taken();
        let user_text = requests
            .first()
            .and_then(|messages| messages.iter().find(|m| m.role == Role::User))
            .map(|m| m.text_body());
        assert_eq!(
            user_text.as_deref(),
            Some("a rewritten request"),
            "the model reads the rewrite, not the typed text"
        );
        let live = handle.inner.session(&session).expect("session survives");
        assert!(
            live.messages()
                .iter()
                .any(|m| m.role == Role::User && m.text_body() == "a rewritten request"),
            "the durable transcript carries the rewrite"
        );
    }

    #[tokio::test]
    #[allow(clippy::await_holding_lock)]
    async fn a_user_input_hook_can_refuse_the_message_before_the_model() {
        let dir = tempfile::tempdir().expect("temp dir");
        let _home = isolated_home(dir.path());
        install_hook_plugin(
            dir.path(),
            "gate-input",
            serde_json::json!({
                "id": "gate-input",
                "version": "1.0.0",
                "components": {"hooks": ["user_input"]},
                "permissions": ["agents"],
                "hook_rules": [{
                    "point": "user_input",
                    "text_contain": "audit",
                    "deny": "this input is refused"
                }]
            }),
        );
        let provider = Arc::new(ScriptedProvider::new(vec![]));
        let runtime = Runtime::start_with_provider(config_for(dir.path()), provider.clone())
            .await
            .expect("runtime starts");
        let handle = runtime.handle();
        let mut events = handle.events();
        let session = named_session(&handle);
        handle
            .submit(&session, "audit the code".into())
            .expect("the turn is accepted");
        drain_turn(&mut events).await;

        assert!(
            provider.taken().is_empty(),
            "a refused input never reaches the provider"
        );
        let live = handle.inner.session(&session).expect("session survives");
        assert_eq!(live.turn(), TurnState::Failed);
    }

    #[tokio::test]
    #[allow(clippy::await_holding_lock)]
    async fn a_turn_end_hook_is_journaled_with_the_turn() {
        let dir = tempfile::tempdir().expect("temp dir");
        let _home = isolated_home(dir.path());
        install_hook_plugin(
            dir.path(),
            "turn-end-note",
            serde_json::json!({
                "id": "turn-end-note",
                "version": "1.0.0",
                "components": {"hooks": ["turn_end"]},
                "permissions": ["agents"],
                "hook_rules": [{"point": "turn_end", "append": "the turn is over"}]
            }),
        );
        let runtime = Runtime::start_with_provider(
            config_for(dir.path()),
            Arc::new(ScriptedProvider::new(vec![vec![
                ProviderEvent::TextDelta("done".into()),
                finished(),
            ]])),
        )
        .await
        .expect("runtime starts");
        let handle = runtime.handle();
        let mut events = handle.events();
        let session = named_session(&handle);
        handle
            .submit(&session, "audit the code".into())
            .expect("the turn is accepted");
        let _seen = record(&mut events).await;

        let points = hook_points(&handle, &session);
        assert!(
            points.contains(&"turn_end".to_owned()),
            "the closing point fired: {points:?}"
        );
    }

    #[tokio::test]
    #[allow(clippy::await_holding_lock)]
    async fn a_response_delivery_hook_rewrites_the_reply() {
        let dir = tempfile::tempdir().expect("temp dir");
        let _home = isolated_home(dir.path());
        install_hook_plugin(
            dir.path(),
            "delivery-edit",
            serde_json::json!({
                "id": "delivery-edit",
                "version": "1.0.0",
                "components": {"hooks": ["response_delivery"]},
                "permissions": ["agents"],
                "hook_rules": [{
                    "point": "response_delivery",
                    "replace_text": "delivered by a plugin"
                }]
            }),
        );
        let provider = Arc::new(ScriptedProvider::new(vec![vec![
            ProviderEvent::TextDelta("raw answer".into()),
            finished(),
        ]]));
        let runtime = Runtime::start_with_provider(config_for(dir.path()), provider)
            .await
            .expect("runtime starts");
        let handle = runtime.handle();
        let mut events = handle.events();
        let session = named_session(&handle);
        handle
            .submit(&session, "audit the code".into())
            .expect("the turn is accepted");
        let seen = record(&mut events).await;

        assert!(
            seen.contains(&Seen::Speech("delivered by a plugin".to_owned())),
            "the committed reply is the rewrite: {seen:?}"
        );
    }

    #[tokio::test]
    #[allow(clippy::await_holding_lock)]
    async fn a_memory_write_hook_stops_the_write_before_policy() {
        let dir = tempfile::tempdir().expect("temp dir");
        let _home = isolated_home(dir.path());
        install_hook_plugin(
            dir.path(),
            "memory-guard",
            serde_json::json!({
                "id": "memory-guard",
                "version": "1.0.0",
                "components": {"hooks": ["memory_write"]},
                "permissions": ["tools"],
                "hook_rules": [{
                    "point": "memory_write",
                    "tool": "memory.save",
                    "deny": "the store is closed"
                }]
            }),
        );
        let call = ToolCall::new("memory.save", serde_json::json!({"key": "k", "value": "v"}));
        let runtime = Runtime::start_with_provider(
            config_for(dir.path()),
            Arc::new(ScriptedProvider::new(vec![
                vec![ProviderEvent::ToolCall(call), finished()],
                vec![ProviderEvent::TextDelta("ok".into()), finished()],
            ])),
        )
        .await
        .expect("runtime starts");
        let handle = runtime.handle();
        handle
            .inner
            .tools
            .register(Box::new(NamedProbeTool("memory.save")));
        let mut events = handle.events();
        let session = named_session(&handle);
        handle
            .submit(&session, "remember this".into())
            .expect("the turn is accepted");
        drain_turn(&mut events).await;

        let live = handle.inner.session(&session).expect("session survives");
        let refused = live.messages().iter().any(|m| {
            m.blocks.iter().any(|b| {
                matches!(b, Block::ToolResult { content, ok: false, .. }
                    if content.contains("denied by plugin: the store is closed"))
            })
        });
        assert!(
            refused,
            "the memory write was refused: {:?}",
            live.messages()
        );
        let points = hook_points(&handle, &session);
        assert!(points.contains(&"memory_write".to_owned()), "{points:?}");
    }

    #[tokio::test]
    #[allow(clippy::await_holding_lock)]
    async fn a_memory_recall_hook_fires_for_memory_search() {
        let dir = tempfile::tempdir().expect("temp dir");
        let _home = isolated_home(dir.path());
        install_hook_plugin(
            dir.path(),
            "recall-guard",
            serde_json::json!({
                "id": "recall-guard",
                "version": "1.0.0",
                "components": {"hooks": ["memory_recall"]},
                "permissions": ["tools"],
                "hook_rules": [{
                    "point": "memory_recall",
                    "tool": "memory.search",
                    "deny": "recall is disabled"
                }]
            }),
        );
        let call = ToolCall::new("memory.search", serde_json::json!({"query": "k"}));
        let runtime = Runtime::start_with_provider(
            config_for(dir.path()),
            Arc::new(ScriptedProvider::new(vec![
                vec![ProviderEvent::ToolCall(call), finished()],
                vec![ProviderEvent::TextDelta("ok".into()), finished()],
            ])),
        )
        .await
        .expect("runtime starts");
        let handle = runtime.handle();
        handle
            .inner
            .tools
            .register(Box::new(NamedProbeTool("memory.search")));
        let mut events = handle.events();
        let session = named_session(&handle);
        handle
            .submit(&session, "what do you remember".into())
            .expect("the turn is accepted");
        drain_turn(&mut events).await;

        let points = hook_points(&handle, &session);
        assert!(
            points.contains(&"memory_recall".to_owned()),
            "the memory point fired: {points:?}"
        );
    }

    #[tokio::test]
    #[allow(clippy::await_holding_lock)]
    async fn session_hooks_bracket_the_session_lifecycle() {
        let dir = tempfile::tempdir().expect("temp dir");
        let _home = isolated_home(dir.path());
        install_hook_plugin(
            dir.path(),
            "session-hooks",
            serde_json::json!({
                "id": "session-hooks",
                "version": "1.0.0",
                "components": {"hooks": ["session_start", "session_end"]},
                "permissions": ["agents"],
                "hook_rules": [
                    {"point": "session_start", "append": "opened"},
                    {"point": "session_end", "append": "closed"}
                ]
            }),
        );
        let runtime = Runtime::start_with_provider(
            config_for(dir.path()),
            Arc::new(ScriptedProvider::new(vec![])),
        )
        .await
        .expect("runtime starts");
        let handle = runtime.handle();
        let session = handle
            .create_session(Some("hooked session".into()))
            .expect("the session is created");
        handle.close_session(&session).expect("the session closes");

        let points = hook_points(&handle, &session);
        assert!(
            points.contains(&"session_start".to_owned())
                && points.contains(&"session_end".to_owned()),
            "both lifecycle points fired: {points:?}"
        );
    }

    #[tokio::test]
    #[allow(clippy::await_holding_lock)]
    async fn delegation_hooks_wrap_the_spawned_run() {
        let dir = tempfile::tempdir().expect("temp dir");
        let _home = isolated_home(dir.path());
        install_hook_plugin(
            dir.path(),
            "delegation-hooks",
            serde_json::json!({
                "id": "delegation-hooks",
                "version": "1.0.0",
                "components": {"hooks": ["delegation_start", "delegation_end", "session_end"]},
                "permissions": ["agents"],
                "hook_rules": [
                    {"point": "delegation_start", "append": "spawned"},
                    {"point": "delegation_end", "append": "answered"},
                    {"point": "session_end", "append": "reaped"}
                ]
            }),
        );
        let provider = Arc::new(ScriptedProvider::new(vec![vec![
            ProviderEvent::TextDelta("the delegated answer".into()),
            finished(),
        ]]));
        let runtime = Runtime::start_with_provider(config_for(dir.path()), provider)
            .await
            .expect("runtime starts");
        let handle = runtime.handle();
        let parent_id = handle.snapshot().sessions[0].id.clone();
        let profile = vikram_contracts::BotId::new("hooked-delegate");
        handle.inner.bots.add(Arc::new(ManagedBot::from_profile(
            crate::types::BotProfile {
                id: profile.clone(),
                name: "Delegate".into(),
                role: "reviewer".into(),
                description: "reads code".into(),
                icon: None,
                created_ms: now_ms(),
                soul: String::new(),
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

        // Wait for the answer to land, which is also when the delegation end hooks have run.
        let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(5);
        loop {
            let arrived = handle
                .inner
                .session(&parent_id)
                .expect("parent")
                .messages()
                .iter()
                .any(|m| m.text_body().contains("delegation result"));
            if arrived {
                break;
            }
            assert!(tokio::time::Instant::now() < deadline, "no answer arrived");
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }

        let parent_points = hook_points(&handle, &parent_id);
        assert!(
            parent_points.contains(&"delegation_start".to_owned()),
            "the spawn was seen: {parent_points:?}"
        );
        let child_points = hook_points(&handle, &child_id);
        for point in ["delegation_end", "session_end"] {
            assert!(
                child_points.contains(&point.to_owned()),
                "{point} fired on the child: {child_points:?}"
            );
        }
    }
}
