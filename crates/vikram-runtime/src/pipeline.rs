//! The stage pipeline.
//!
//! A stage is a named, ordered step in the turn loop that a native module can replace wholesale.
//! Where a [`crate::hooks::HookRegistry`] hook observes or amends what the runtime is about to do,
//! a stage *is* the step: [`StageRegistry::run`] dispatches each [`StagePoint`] through its
//! registration chain in priority order — each handler may mutate the [`StageContext`] payload
//! and then [`StageOutcome::Pass`] to hand the dispatch to the next registration, or finish it
//! with [`StageOutcome::Done`]. When every registration passes, the builtin default runs, so the
//! pipeline is fully populated from boot and a registration *replaces* a step rather than
//! wrapping it unless it declines.
//!
//! Failure policy is fail-stage, never silent fallback: a handler that returns [`StageError`]
//! fails the stage and the turn reports it. Falling back to the default would hide a broken
//! module behind behaviour that only looks healthy.
//!
//! Process plugins reach the same seams two ways: a plugin declaring `components.hooks` runs
//! inside the builtin handlers through the hook chain, and a plugin declaring
//! `components.stages` registers a [`ProcessStageHandler`] that wraps the builtin default —
//! the default does the stage's real work first, then the plugin's entry may deny or amend the
//! result through the same effect fields a hook replies with.

use std::collections::HashMap;
use std::fmt;
use std::future::Future;
use std::path::PathBuf;
use std::pin::Pin;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, RwLock};

use serde_json::{Value, json};
use vikram_contracts::{
    BotId, CancellationToken, Message, SessionId, ToolCall, ToolCallId, TurnId,
};

use crate::hooks::{HookContext, HookEffect, HookPoint};
use crate::plugins::{
    PluginManifest, PluginPermission, PluginRegistry, ensure_execution_permitted, entry_path,
    parse_permissions,
};
use crate::provider::CompletionRequest;
use crate::runtime::RuntimeInner;
use crate::session::SessionRuntime;
use crate::turn::AgentRun;
use crate::types::{BotProfile, TurnState};

/// The boxed future a [`StageHandler`] returns. Boxed because the trait must be object-safe.
pub type StageFuture<'a> =
    Pin<Box<dyn Future<Output = Result<StageOutcome, StageError>> + Send + 'a>>;

/// A named step in the turn pipeline that a registered [`StageHandler`] can replace.
///
/// The points mirror the hook points the builtin handlers still dispatch through, so a process
/// plugin and a native stage meet at the same seam from opposite sides.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum StagePoint {
    /// Assemble the system prefix for one provider round — skills catalogue, project and bot
    /// context, dialectic supplement, code index — and apply the `context_build` hooks.
    ContextBuild,
    /// Retrieve durable memory for this step and apply the `memory_recall` hooks.
    MemoryRecall,
    /// Fit the transcript to the budget, apply the `model_request` hooks and build the
    /// [`CompletionRequest`] the provider is sent.
    ProviderRequest,
    /// Fan one round of model tool calls out across conflict lanes and collect their outcomes.
    /// Policy, approvals and the per-call hook chain stay inside `run_tool`.
    ToolDispatch,
    /// Commit tool results to the transcript, journal and event stream.
    ResultCommit,
    /// Accept, normalize and journal the operator's message before any model work begins —
    /// the point the `user_input` hooks already meet. The stage may rewrite the message or
    /// refuse the input outright.
    UserInput,
    /// Commit one assistant message — text, reasoning and the tool calls it announced — to the
    /// transcript, journal and event stream. Distinct from [`StagePoint::ResultCommit`], which
    /// commits tool results.
    ResponseCommit,
    /// Report the turn's decided outcome, step count and token figures before `finish`
    /// publishes `TurnFinished`. The outcome is already decided, so the stage observes rather
    /// than refuses.
    TurnEnd,
    /// Session lifecycle: create, open, close, delete and boot-time load run on synchronous
    /// store paths with no turn and no async boundary, so this point dispatches through
    /// [`StageRegistry::run_blocking`] rather than [`StageRegistry::run`]. Of the operations
    /// only `create` may be vetoed; the rest observe.
    SessionLifecycle,
    /// Agent lifecycle: a delegated run's spawn and the delivery of its bounded answer back to
    /// the session that asked. Observe-only, dispatched through
    /// [`StageRegistry::run_blocking`].
    AgentLifecycle,
}

impl StagePoint {
    pub fn as_str(self) -> &'static str {
        match self {
            StagePoint::ContextBuild => "context_build",
            StagePoint::MemoryRecall => "memory_recall",
            StagePoint::ProviderRequest => "provider_request",
            StagePoint::ToolDispatch => "tool_dispatch",
            StagePoint::ResultCommit => "result_commit",
            StagePoint::UserInput => "user_input",
            StagePoint::ResponseCommit => "response_commit",
            StagePoint::TurnEnd => "turn_end",
            StagePoint::SessionLifecycle => "session_lifecycle",
            StagePoint::AgentLifecycle => "agent_lifecycle",
        }
    }

    /// Parse a manifest-style spelling. Hyphens and `.` are accepted for `_`.
    pub fn parse(raw: &str) -> Option<Self> {
        let normalized = raw.trim().to_ascii_lowercase().replace(['-', '.'], "_");
        match normalized.as_str() {
            "context_build" | "context" => Some(StagePoint::ContextBuild),
            "memory_recall" | "memory_read" => Some(StagePoint::MemoryRecall),
            "provider_request" | "model_request" | "model" => Some(StagePoint::ProviderRequest),
            "tool_dispatch" | "tool_call" => Some(StagePoint::ToolDispatch),
            "result_commit" | "tool_result" => Some(StagePoint::ResultCommit),
            "user_input" | "input" => Some(StagePoint::UserInput),
            "response_commit" | "response_delivery" | "assistant_commit" => {
                Some(StagePoint::ResponseCommit)
            }
            "turn_end" | "turn" => Some(StagePoint::TurnEnd),
            "session_lifecycle" | "session" => Some(StagePoint::SessionLifecycle),
            "agent_lifecycle" | "agent" | "delegation" => Some(StagePoint::AgentLifecycle),
            _ => None,
        }
    }

    /// The hook point at the same seam. Stage permissions reuse the hook point's surface
    /// mapping, so a `tool_dispatch` stage needs `tools` exactly like a `tool_call` hook.
    pub fn hook_point(self) -> HookPoint {
        match self {
            StagePoint::ContextBuild => HookPoint::ContextBuild,
            StagePoint::MemoryRecall => HookPoint::MemoryRecall,
            StagePoint::ProviderRequest => HookPoint::ModelRequest,
            StagePoint::ToolDispatch => HookPoint::ToolCall,
            StagePoint::ResultCommit => HookPoint::ToolResult,
            StagePoint::UserInput => HookPoint::UserInput,
            StagePoint::ResponseCommit => HookPoint::ResponseDelivery,
            StagePoint::TurnEnd => HookPoint::TurnEnd,
            StagePoint::SessionLifecycle => HookPoint::SessionStart,
            StagePoint::AgentLifecycle => HookPoint::DelegationStart,
        }
    }
}

/// What the `memory_recall` stage is given and what it leaves behind.
#[derive(Default)]
pub struct MemoryRecallPayload {
    /// The text the stage produced for injection ahead of the transcript. `None` means the step
    /// has nothing to add — retrieval found nothing, or the stage withheld it.
    pub text: Option<String>,
}

/// What the `context_build` stage is given and what it leaves behind.
pub struct ContextBuildPayload {
    /// Context fragments the `user_input`/`turn_start` hooks asked to place ahead of everything
    /// the host assembles, in the order the hooks ran.
    pub hook_injected: Vec<String>,
    /// The speaking identity resolved for this turn, when the session is a bot's.
    pub profile: Option<BotProfile>,
    /// What the `memory_recall` stage produced for this step.
    pub memory_text: Option<String>,
    /// The assembled system prefix, in injection order. Written by the stage.
    pub prefix: Vec<Message>,
    /// The context-meter label for each prefix message, parallel to `prefix`. Written by the stage.
    pub labels: Vec<&'static str>,
}

/// What the `provider_request` stage is given and what it leaves behind.
pub struct ProviderRequestPayload {
    /// The assembled request — prefix plus the transcript fitted to the budget — which the stage
    /// may extend before building the request.
    pub messages: Vec<Message>,
    /// The [`CompletionRequest`] the stage built. `None` after `Done` is a stage bug; the turn
    /// fails on it rather than sending nothing.
    pub request: Option<CompletionRequest>,
}

/// What the `tool_dispatch` stage is given and what it leaves behind.
pub struct ToolDispatchPayload {
    /// The calls the model asked for this round, in stream order.
    pub calls: Vec<ToolCall>,
    /// Cancellation for the whole turn.
    pub cancel: CancellationToken,
    /// The delegated run this turn belongs to, when there is one.
    pub agent: Option<AgentRun>,
    /// `(content, ok)` per call, aligned with `calls`. Written by the stage.
    pub outcomes: Vec<(String, bool)>,
}

/// What the `result_commit` stage is given.
pub struct ResultCommitPayload {
    /// `(call id, content, ok)` in the order they commit to the transcript.
    pub results: Vec<(ToolCallId, String, bool)>,
}

/// What the `user_input` stage is given and what it leaves behind.
pub struct UserInputPayload {
    /// The operator's message. Written by the stage — the `user_input` hooks may have rewritten
    /// its prose — and what the turn journals and the model reads.
    pub message: Message,
    /// The `user_input` hook's refusal reason, when it denied the input. Carried rather than
    /// surfaced as [`StageOutcome::Refused`] so the turn keeps its existing deny path: the
    /// message is still durable before the turn reports the refusal.
    pub deny: Option<String>,
    /// Context fragments the `user_input` hook asked to place ahead of the assembled prefix.
    /// Written by the stage.
    pub prepend: Vec<String>,
    /// Context fragments the `user_input` hook asked to place behind the assembled prefix.
    /// Written by the stage.
    pub append: Vec<String>,
}

/// What the `response_commit` stage is given and what it leaves behind.
pub struct ResponseCommitPayload {
    /// The assistant prose for this commit, before the `response_delivery` hooks see it.
    pub text: String,
    /// The assistant reasoning for this commit.
    pub reasoning: String,
    /// The tool calls this assistant message announces.
    pub calls: Vec<ToolCall>,
}

/// What the `turn_end` stage is given.
pub struct TurnEndPayload {
    /// How the turn ended. Already decided — the stage observes, it does not choose.
    pub outcome: TurnState,
    /// Provider rounds the turn spent.
    pub steps: u64,
    /// Output tokens the turn produced, as counted so far.
    pub output_tokens: u64,
}

/// The lifecycle operation a `session_lifecycle` dispatch reports.
#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum SessionLifecycleOp {
    /// A session row was created and is about to go live. The dispatch runs after the row and
    /// its [`SessionRuntime`] exist but before the session joins the live set, so a
    /// [`StageOutcome::Refused`] closes the row exactly like a `session_start` hook denial.
    /// This is the only lifecycle operation a stage may veto.
    Create,
    /// A stored session was brought back into the live set by [`RuntimeHandle::open_session`].
    ///
    /// [`RuntimeHandle::open_session`]: crate::runtime::RuntimeHandle::open_session
    Open,
    /// A live session was closed; its transcript stays in the store. Observe-only.
    Close,
    /// A session was destroyed — live or stored-only. Observe-only.
    Delete,
    /// Boot rehydration loaded a stored session into the live set. Observe-only.
    Load,
    /// A session was forked from another — new row, copied journal, loaded live. Observe-only.
    Fork,
}

/// What the `session_lifecycle` stage is given.
pub struct SessionLifecyclePayload {
    /// Which lifecycle operation this dispatch reports.
    pub op: SessionLifecycleOp,
    /// The session the operation acts on.
    pub session: SessionId,
    /// The session's title at dispatch time.
    pub title: String,
}

/// The lifecycle operation an `agent_lifecycle` dispatch reports.
#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum AgentLifecycleOp {
    /// A delegated run was spawned: its session exists and its first turn is starting.
    Spawn,
    /// A delegated run finished and its bounded answer was (or was not) handed back to the
    /// session that asked.
    Deliver,
}

/// What the `agent_lifecycle` stage is given. Observe-only: the spawn and the delivery are
/// already decided store operations by the time the stage runs.
pub struct AgentLifecyclePayload {
    /// Which lifecycle operation this dispatch reports.
    pub op: AgentLifecycleOp,
    /// The bot that owns the run; `None` for a forked session, which carries a parent but no
    /// bot.
    pub agent: Option<BotId>,
    /// The session the work was delegated from.
    pub parent: SessionId,
    /// The session the agent works in.
    pub session: SessionId,
    /// The delegated task, on `spawn`.
    pub task: String,
    /// `deliver` only: whether the bounded answer reached the parent session.
    pub delivered: bool,
}

/// The per-point input/output carrier. Exactly one variant is live per dispatch, matching the
/// [`StagePoint`] the registry was called with.
pub enum StagePayload {
    MemoryRecall(MemoryRecallPayload),
    ContextBuild(ContextBuildPayload),
    ProviderRequest(ProviderRequestPayload),
    ToolDispatch(ToolDispatchPayload),
    ResultCommit(ResultCommitPayload),
    UserInput(UserInputPayload),
    ResponseCommit(ResponseCommitPayload),
    TurnEnd(TurnEndPayload),
    SessionLifecycle(SessionLifecyclePayload),
    AgentLifecycle(AgentLifecyclePayload),
}

impl StagePayload {
    fn point(&self) -> StagePoint {
        match self {
            StagePayload::MemoryRecall(_) => StagePoint::MemoryRecall,
            StagePayload::ContextBuild(_) => StagePoint::ContextBuild,
            StagePayload::ProviderRequest(_) => StagePoint::ProviderRequest,
            StagePayload::ToolDispatch(_) => StagePoint::ToolDispatch,
            StagePayload::ResultCommit(_) => StagePoint::ResultCommit,
            StagePayload::UserInput(_) => StagePoint::UserInput,
            StagePayload::ResponseCommit(_) => StagePoint::ResponseCommit,
            StagePayload::TurnEnd(_) => StagePoint::TurnEnd,
            StagePayload::SessionLifecycle(_) => StagePoint::SessionLifecycle,
            StagePayload::AgentLifecycle(_) => StagePoint::AgentLifecycle,
        }
    }
}

/// Everything a stage handler is given at one point: the runtime objects it may need, the turn's
/// prose, and the point's payload — inputs on the way in, outputs on the way out.
///
/// `inner`, `session` and `turn` are crate-internal: native stage modules live in this crate,
/// and a process plugin only ever sees the serialized form of this context — never the objects.
pub struct StageContext {
    pub(crate) inner: Arc<RuntimeInner>,
    pub(crate) session: Arc<SessionRuntime>,
    pub(crate) turn: TurnId,
    /// The [`StagePoint`] this dispatch is for, taken from the payload.
    pub point: StagePoint,
    /// The operator's request text for this turn — the prose hooks and stages annotate.
    pub request_text: String,
    /// The point's input/output carrier.
    pub payload: StagePayload,
}

impl StageContext {
    pub(crate) fn new(
        inner: &Arc<RuntimeInner>,
        session: &Arc<SessionRuntime>,
        turn: &TurnId,
        request_text: &str,
        payload: StagePayload,
    ) -> Self {
        Self {
            point: payload.point(),
            inner: Arc::clone(inner),
            session: Arc::clone(session),
            turn: turn.clone(),
            request_text: request_text.to_owned(),
            payload,
        }
    }
}

/// What a stage handler decided.
#[derive(Debug)]
pub enum StageOutcome {
    /// The stage completed; its outputs are in the context payload. Terminal: no later
    /// registration or the builtin default runs for this dispatch.
    Done,
    /// The stage declines to let the turn continue; `reason` is operator-facing. The caller maps
    /// it onto the same refusal path the point's hooks already had. Terminal like [`Self::Done`].
    Refused(String),
    /// This handler does not resolve the dispatch — it may still have amended `ctx.payload`,
    /// which the next handler in the chain sees. The registry tries the next registration in
    /// priority order, and the builtin default when every registration passes. `Pass` never
    /// escapes [`StageRegistry::run`]: a chain in which nothing resolves the stage is a
    /// [`StageError`], matching the fail-stage policy.
    Pass,
}

/// A stage handler failed. The registry re-attributes the error to the point and handler that
/// were actually dispatching, so a handler only has to say what went wrong.
#[derive(Debug)]
pub struct StageError {
    /// What went wrong, in the handler's own words.
    pub message: String,
    /// The point that was dispatching when the handler failed.
    pub point: StagePoint,
    /// The registered or builtin handler that failed.
    pub handler: String,
}

impl StageError {
    pub fn new(point: StagePoint, handler: impl Into<String>, message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
            point,
            handler: handler.into(),
        }
    }
}

impl fmt::Display for StageError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "stage `{}` handler `{}` failed: {}",
            self.point.as_str(),
            self.handler,
            self.message
        )
    }
}

impl std::error::Error for StageError {}

/// One replaceable step of the turn pipeline.
///
/// Implementations must be `Send + Sync`: a dispatch may run on any executor thread, and the
/// registry holds handlers for the runtime's whole life.
pub trait StageHandler: Send + Sync + 'static {
    /// Stable identifier used in errors and diagnostics.
    fn name(&self) -> &'static str;

    /// Run the stage. Mutate `ctx.payload` with the stage's outputs; return [`StageOutcome::Done`]
    /// when they are authoritative, [`StageOutcome::Refused`] to veto the turn,
    /// [`StageOutcome::Pass`] to hand the dispatch to the next handler in the chain, or
    /// [`StageError`] when the stage itself failed.
    fn handle<'a>(&'a self, ctx: &'a mut StageContext) -> StageFuture<'a>;
}

/// A process plugin acting as a stage handler.
///
/// The builtin default runs first — a subprocess cannot assemble a context or dispatch tools —
/// then the plugin's entry is spawned with the stage point as `argv[1]` and a hook-shaped JSON
/// context on stdin carrying the stage payload (its inputs plus the outputs the builtin just
/// produced, inside `details`). The reply is a [`HookEffect`] JSON object: `deny` refuses the
/// stage and the remaining fields amend the payload. Spawn failure, non-zero exit, timeout or a
/// malformed reply is a [`StageError`]: the stage fails, it does not silently fall back.
pub struct ProcessStageHandler {
    plugin: String,
    dir: PathBuf,
    entry: String,
    inherit_env: bool,
}

impl ProcessStageHandler {
    pub fn new(
        plugin: impl Into<String>,
        dir: impl Into<PathBuf>,
        entry: impl Into<String>,
    ) -> Self {
        Self {
            plugin: plugin.into(),
            dir: dir.into(),
            entry: entry.into(),
            inherit_env: false,
        }
    }

    /// Let the stage subprocess inherit the full environment; only a manifest declaring `env`
    /// may opt in. The default is the minimal allowlist.
    pub fn with_inherited_env(mut self, inherit_env: bool) -> Self {
        self.inherit_env = inherit_env;
        self
    }

    /// The stage dispatch as the plugin receives it: the usual hook context fields, with the
    /// point spelled as the stage name and the whole payload under `details`.
    fn request_payload(&self, ctx: &StageContext) -> String {
        let mut value = HookContext::new(ctx.point.hook_point())
            .with_session(ctx.session.id.as_str())
            .with_turn(ctx.turn.as_str())
            .with_workspace(ctx.inner.workspace_root.display().to_string())
            .with_model(ctx.inner.model())
            .with_text(ctx.request_text.clone())
            .with_details(stage_payload_details(&ctx.payload))
            .to_json();
        if let Value::Object(map) = &mut value {
            map.insert(
                "point".to_owned(),
                Value::String(ctx.point.as_str().to_owned()),
            );
        }
        value.to_string()
    }
}

impl StageHandler for ProcessStageHandler {
    fn name(&self) -> &'static str {
        "plugin-stage"
    }

    fn handle<'a>(&'a self, ctx: &'a mut StageContext) -> StageFuture<'a> {
        Box::pin(async move {
            if let Some(builtin) = crate::turn::default_stage_handler(ctx.point) {
                match builtin.handle(ctx).await? {
                    StageOutcome::Done | StageOutcome::Pass => {}
                    StageOutcome::Refused(reason) => return Ok(StageOutcome::Refused(reason)),
                }
            }
            let command = crate::hooks::plugin_command(
                &self.plugin,
                &self.dir,
                &self.entry,
                self.inherit_env,
            )
            .map_err(|reason| StageError::new(ctx.point, self.name(), reason))?;
            let payload = self.request_payload(ctx);
            let point = ctx.point.as_str();
            let reply = tokio::task::spawn_blocking(move || {
                crate::hooks::run_plugin_process(command, point, &payload)
            })
            .await
            .map_err(|err| {
                StageError::new(
                    ctx.point,
                    self.name(),
                    format!("stage plugin task failed: {err}"),
                )
            })?
            .map_err(|reason| StageError::new(ctx.point, self.name(), reason))?;
            if !reply.status.success() {
                return Err(StageError::new(
                    ctx.point,
                    self.name(),
                    format!(
                        "plugin `{}` exited with {}",
                        self.plugin,
                        reply
                            .status
                            .code()
                            .map(|code| code.to_string())
                            .unwrap_or_else(|| "no status".to_owned())
                    ),
                ));
            }
            let text = reply.stdout;
            if text.trim().is_empty() {
                return Ok(StageOutcome::Done);
            }
            let value: Value = serde_json::from_str(text.trim()).map_err(|err| {
                StageError::new(
                    ctx.point,
                    self.name(),
                    format!("stage reply is not JSON: {err}"),
                )
            })?;
            let effect = HookEffect::from_json(&value)
                .map_err(|reason| StageError::new(ctx.point, self.name(), reason))?;
            apply_stage_effect(ctx, &self.plugin, &effect)
        })
    }
}

/// The stage payload as the process plugin sees it: inputs and the outputs already produced,
/// serialized into the hook context's `details` slot. Fields a subprocess cannot usefully
/// carry (`cancel`, `agent`) are omitted.
fn stage_payload_details(payload: &StagePayload) -> Value {
    match payload {
        StagePayload::MemoryRecall(payload) => json!({ "text": payload.text }),
        StagePayload::ContextBuild(payload) => json!({
            "hook_injected": payload.hook_injected,
            "profile": payload.profile,
            "memory_text": payload.memory_text,
            "prefix": payload.prefix,
            "labels": payload.labels,
        }),
        StagePayload::ProviderRequest(payload) => json!({
            "messages": payload.messages,
            "request": payload.request.as_ref().map(|request| json!({
                "model": request.model,
                "messages": request.messages,
                "tools": request.tools,
                "max_output_tokens": request.max_output_tokens,
            })),
        }),
        StagePayload::ToolDispatch(payload) => json!({
            "calls": payload.calls,
            "outcomes": payload.outcomes,
        }),
        StagePayload::ResultCommit(payload) => json!({
            "results": payload.results,
        }),
        StagePayload::UserInput(payload) => json!({
            "message": payload.message,
            "deny": payload.deny,
            "prepend": payload.prepend,
            "append": payload.append,
        }),
        StagePayload::ResponseCommit(payload) => json!({
            "text": payload.text,
            "reasoning": payload.reasoning,
            "calls": payload.calls,
        }),
        StagePayload::TurnEnd(payload) => json!({
            "outcome": payload.outcome,
            "steps": payload.steps,
            "output_tokens": payload.output_tokens,
        }),
        StagePayload::SessionLifecycle(payload) => json!({
            "op": payload.op,
            "session": payload.session,
            "title": payload.title,
        }),
        StagePayload::AgentLifecycle(payload) => json!({
            "op": payload.op,
            "agent": payload.agent,
            "parent": payload.parent,
            "session": payload.session,
            "task": payload.task,
            "delivered": payload.delivered,
        }),
    }
}

/// Apply a process stage's [`HookEffect`] to the payload the builtin produced. Effect fields
/// with no carrier at this point are ignored, matching how hooks treat inapplicable effects.
fn apply_stage_effect(
    ctx: &mut StageContext,
    plugin: &str,
    effect: &HookEffect,
) -> Result<StageOutcome, StageError> {
    let point = ctx.point;
    if let Some(reason) = &effect.deny {
        if let StagePayload::UserInput(payload) = &mut ctx.payload {
            // The same path the builtin's hook denial takes: the message stays durable and the
            // turn reports the refusal through `payload.deny`.
            payload.deny = Some(reason.clone());
            return Ok(StageOutcome::Done);
        }
        return Ok(StageOutcome::Refused(reason.clone()));
    }
    match &mut ctx.payload {
        StagePayload::UserInput(payload) => {
            if let Some(text) = &effect.text {
                crate::turn::rewrite_message_text(&mut payload.message, text.clone());
            }
            payload.prepend.extend(effect.prepend.iter().cloned());
            payload.append.extend(effect.append.iter().cloned());
        }
        StagePayload::MemoryRecall(payload) => {
            let mut parts = effect.prepend.clone();
            if let Some(text) = effect.text.clone().or_else(|| payload.text.take()) {
                parts.push(text);
            }
            parts.extend(effect.append.iter().cloned());
            if !parts.is_empty() {
                payload.text = Some(parts.join("\n"));
            }
        }
        StagePayload::ContextBuild(payload) => {
            for (index, text) in effect.prepend.iter().enumerate() {
                payload.prefix.insert(index, Message::system(text.clone()));
                payload.labels.insert(index, "plugin");
            }
            for text in effect.append.iter().chain(effect.text.iter()) {
                payload.prefix.push(Message::system(text.clone()));
                payload.labels.push("plugin");
            }
        }
        StagePayload::ProviderRequest(payload) => {
            let Some(request) = &mut payload.request else {
                return Err(StageError::new(
                    point,
                    plugin,
                    "provider_request produced no request to amend".to_owned(),
                ));
            };
            if let Some(max) = effect.max_output_tokens {
                request.max_output_tokens = Some(max);
            }
            for (index, text) in effect.prepend.iter().enumerate() {
                request
                    .messages
                    .insert(index, Message::system(text.clone()));
            }
            for text in effect.append.iter().chain(effect.text.iter()) {
                request.messages.push(Message::system(text.clone()));
            }
        }
        StagePayload::ToolDispatch(payload) => {
            // The calls already ran inside the builtin; what a stage plugin can still amend is
            // what will be committed, through the outcomes.
            if let Some(content) = &effect.content {
                for (text, _) in payload.outcomes.iter_mut() {
                    *text = content.clone();
                }
            }
            if let Some(ok) = effect.ok {
                for (_, flag) in payload.outcomes.iter_mut() {
                    *flag = ok;
                }
            }
        }
        StagePayload::ResultCommit(payload) => {
            if let Some(content) = &effect.content {
                for (_, text, _) in payload.results.iter_mut() {
                    *text = content.clone();
                }
            }
            if let Some(ok) = effect.ok {
                for (_, _, flag) in payload.results.iter_mut() {
                    *flag = ok;
                }
            }
        }
        StagePayload::ResponseCommit(payload) => {
            if !effect.prepend.is_empty() {
                let mut joined = effect.prepend.join("\n");
                if !payload.text.is_empty() {
                    joined.push('\n');
                }
                joined.push_str(&payload.text);
                payload.text = joined;
            }
            if let Some(text) = &effect.text {
                payload.text = text.clone();
            }
            for text in &effect.append {
                payload.text.push('\n');
                payload.text.push_str(text);
            }
        }
        StagePayload::TurnEnd(_)
        | StagePayload::SessionLifecycle(_)
        | StagePayload::AgentLifecycle(_) => {}
    }
    Ok(StageOutcome::Done)
}

struct StageRegistration {
    name: String,
    handler: Arc<dyn StageHandler>,
    priority: i32,
    /// Registration order; earlier wins a priority tie so dispatch stays deterministic.
    sequence: u64,
    /// Plugin registrations are rebuilt by [`StageRegistry::reload_plugin_stages`]; native
    /// `register` entries survive a plugin reload.
    from_plugin: bool,
}

/// A stage registration that was refused, kept so the operator can see why a plugin is not
/// handling a point.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct StageRejection {
    pub plugin: String,
    pub reason: String,
}

/// The stage registry: named points mapped to their registration chain.
///
/// Dispatch walks a point's registrations in priority order — highest first, registration
/// order breaking ties — and each handler decides the dispatch's fate: [`StageOutcome::Done`]
/// and [`StageOutcome::Refused`] are terminal, [`StageOutcome::Pass`] hands the same context —
/// including any payload mutations earlier handlers left — to the next registration, and
/// [`StageError`] fails the stage outright. The builtin default is appended to every chain as
/// the last resort, so it runs whenever nothing is registered *and* whenever every
/// registration declines; a registration that resolves the stage still replaces the step
/// wholesale, exactly as before.
pub struct StageRegistry {
    registrations: RwLock<HashMap<StagePoint, Vec<StageRegistration>>>,
    sequence: AtomicU64,
}

impl StageRegistry {
    pub fn new() -> Self {
        Self {
            registrations: RwLock::new(HashMap::new()),
            sequence: AtomicU64::new(0),
        }
    }

    /// Register a native handler for `point`. Higher `priority` runs earlier in the dispatch
    /// chain; ties go to the earlier registration. The builtin default runs only while no
    /// earlier handler resolves the stage — it is the chain's last resort, not a wrapper.
    pub fn register(&self, point: StagePoint, handler: Arc<dyn StageHandler>, priority: i32) {
        self.push_registration(point, handler.name().to_owned(), handler, priority, false);
    }

    fn push_registration(
        &self,
        point: StagePoint,
        name: String,
        handler: Arc<dyn StageHandler>,
        priority: i32,
        from_plugin: bool,
    ) {
        let sequence = self.sequence.fetch_add(1, Ordering::SeqCst);
        let mut registrations = self
            .registrations
            .write()
            .unwrap_or_else(|e| e.into_inner());
        let entries = registrations.entry(point).or_default();
        entries.push(StageRegistration {
            name,
            handler,
            priority,
            sequence,
            from_plugin,
        });
        entries.sort_by(|a, b| {
            b.priority
                .cmp(&a.priority)
                .then_with(|| a.sequence.cmp(&b.sequence))
        });
    }

    /// Rebuild every process-plugin stage registration from `plugins`, returning the
    /// declarations that were refused. Native registrations are untouched; a plugin that no
    /// longer declares a point — or was disabled — loses its handler, so the builtin default
    /// (or a native registration) runs for it again.
    pub fn reload_plugin_stages(&self, plugins: &PluginRegistry) -> Vec<StageRejection> {
        {
            let mut registrations = self
                .registrations
                .write()
                .unwrap_or_else(|e| e.into_inner());
            for entries in registrations.values_mut() {
                entries.retain(|entry| !entry.from_plugin);
            }
            registrations.retain(|_, entries| !entries.is_empty());
        }
        let mut rejected = Vec::new();
        for state in plugins.plugins() {
            // Same trust rule as hooks: only installed plugins may act inside a turn.
            if !state.loaded || !state.installed {
                continue;
            }
            let Ok(manifest) = PluginManifest::from_dir(&state.path) else {
                continue;
            };
            rejected.extend(self.absorb_stage_manifest(&manifest, &state.path));
        }
        rejected
    }

    /// Register one plugin's declared stage points. Mirrors `HookRegistry::absorb_manifest`:
    /// every refusal is reported through the returned [`StageRejection`]s, never dropped.
    fn absorb_stage_manifest(
        &self,
        manifest: &PluginManifest,
        dir: &std::path::Path,
    ) -> Vec<StageRejection> {
        let id = manifest.id().trim().to_owned();
        let mut rejected = Vec::new();
        let (points, unknown) = manifest.declared_stages();
        for raw in unknown {
            rejected.push(StageRejection {
                plugin: id.clone(),
                reason: format!("unknown stage point `{raw}`"),
            });
        }
        if points.is_empty() {
            return rejected;
        }
        let granted = parse_permissions(&manifest.permissions);
        if let Some(missing) = points
            .iter()
            .find(|point| !granted.contains(&point.hook_point().permission()))
        {
            rejected.push(StageRejection {
                plugin: id.clone(),
                reason: format!(
                    "stage point `{}` requires the `{}` permission",
                    missing.as_str(),
                    missing.hook_point().permission().as_str()
                ),
            });
            return rejected;
        }
        let Some(entry) = manifest.entry_point() else {
            rejected.push(StageRejection {
                plugin: id.clone(),
                reason: "stage points are declared but no entry point is".to_owned(),
            });
            return rejected;
        };
        if let Err(reason) = ensure_execution_permitted(&id, manifest) {
            rejected.push(StageRejection {
                plugin: id.clone(),
                reason,
            });
            return rejected;
        }
        if let Err(reason) = entry_path(dir, entry) {
            rejected.push(StageRejection {
                plugin: id.clone(),
                reason: format!("entry point: {reason}"),
            });
            return rejected;
        }
        let inherit_env = granted.contains(&PluginPermission::Environment);
        let handler: Arc<dyn StageHandler> = Arc::new(
            ProcessStageHandler::new(id.clone(), dir, entry).with_inherited_env(inherit_env),
        );
        for point in points {
            self.push_registration(
                point,
                format!("plugin:{id}"),
                Arc::clone(&handler),
                manifest.priority,
                true,
            );
        }
        rejected
    }

    /// Whether a native handler is registered for `point` (the builtin default does not count).
    pub fn is_registered(&self, point: StagePoint) -> bool {
        self.registrations
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .get(&point)
            .is_some_and(|entries| !entries.is_empty())
    }

    /// The handlers `point` dispatches through, in order: every registration in priority order,
    /// then the builtin default as the last resort. A handler's [`StageOutcome::Pass`] advances
    /// the chain; anything else ends it.
    fn handler_chain(&self, point: StagePoint) -> Vec<(Arc<dyn StageHandler>, String)> {
        let mut chain: Vec<(Arc<dyn StageHandler>, String)> = {
            let registrations = self.registrations.read().unwrap_or_else(|e| e.into_inner());
            registrations
                .get(&point)
                .map(|entries| {
                    entries
                        .iter()
                        .map(|entry| (Arc::clone(&entry.handler), entry.name.clone()))
                        .collect()
                })
                .unwrap_or_default()
        };
        if let Some(builtin) = crate::turn::default_stage_handler(point) {
            let name = builtin.name().to_owned();
            chain.push((builtin, name));
        }
        chain
    }

    /// Dispatch `point` through its handler chain — registrations in priority order, then the
    /// builtin default — and return the outcome of the first handler that resolves the stage.
    /// [`StageOutcome::Pass`] advances the chain with the payload mutations intact; a handler
    /// error is attributed to that handler and propagated: the stage fails, it does not
    /// silently fall back. A chain in which every handler passes is a stage failure too.
    pub async fn run(
        &self,
        point: StagePoint,
        ctx: &mut StageContext,
    ) -> Result<StageOutcome, StageError> {
        if ctx.point != point {
            return Err(StageError::new(
                point,
                "registry",
                format!(
                    "context carries `{}` but the registry dispatched `{}`",
                    ctx.point.as_str(),
                    point.as_str()
                ),
            ));
        }
        for (handler, name) in self.handler_chain(point) {
            match handler
                .handle(ctx)
                .await
                .map_err(|err| StageError::new(point, name.as_str(), err.message))?
            {
                StageOutcome::Pass => continue,
                outcome => return Ok(outcome),
            }
        }
        Err(StageError::new(
            point,
            "registry",
            format!(
                "every handler for stage `{}` passed and none resolved it",
                point.as_str()
            ),
        ))
    }

    /// Dispatch `point` from synchronous code — the session and agent lifecycle call sites run
    /// on store paths with no async boundary to `.await` on.
    ///
    /// The handler's future is still a [`StageFuture`], so it is driven to completion on a
    /// scoped helper thread through `executor`'s [`tokio::runtime::Handle::block_on`]. That is
    /// the honest minimal bridge: `Handle::block_on` panics on a thread that already has a
    /// runtime context, and `block_in_place` does not exist on a current-thread runtime, so the
    /// helper thread is the only form that works from every caller — runtime worker, blocking
    /// thread or plain CLI thread — while a handler's own `spawn_blocking` still lands on the
    /// runtime's blocking pool. The cost is one short-lived thread per lifecycle dispatch;
    /// these operations are rare enough that it is noise.
    ///
    /// A panic inside the handler surfaces as a [`StageError`], matching the fail-stage policy
    /// of [`Self::run`].
    pub fn run_blocking(
        &self,
        point: StagePoint,
        ctx: &mut StageContext,
        executor: &tokio::runtime::Handle,
    ) -> Result<StageOutcome, StageError> {
        if ctx.point != point {
            return Err(StageError::new(
                point,
                "registry",
                format!(
                    "context carries `{}` but the registry dispatched `{}`",
                    ctx.point.as_str(),
                    point.as_str()
                ),
            ));
        }
        for (handler, name) in self.handler_chain(point) {
            let future = handler.handle(ctx);
            let outcome = std::thread::scope(|scope| {
                scope
                    .spawn(|| executor.block_on(future))
                    .join()
                    .unwrap_or_else(|_| {
                        Err(StageError::new(
                            point,
                            name.as_str(),
                            "stage handler thread panicked".to_owned(),
                        ))
                    })
            })
            .map_err(|err| StageError::new(point, name.as_str(), err.message))?;
            match outcome {
                StageOutcome::Pass => continue,
                outcome => return Ok(outcome),
            }
        }
        Err(StageError::new(
            point,
            "registry",
            format!(
                "every handler for stage `{}` passed and none resolved it",
                point.as_str()
            ),
        ))
    }
}

/// Dispatch a lifecycle [`StagePoint`] — [`StagePoint::SessionLifecycle`] or
/// [`StagePoint::AgentLifecycle`] — from synchronous runtime code. The stage still runs through
/// the registry, on a helper thread via [`StageRegistry::run_blocking`]; only the boundary
/// between the sync store path and the async handler trait lives here.
pub(crate) fn run_lifecycle_stage(
    inner: &Arc<RuntimeInner>,
    point: StagePoint,
    session: &Arc<SessionRuntime>,
    request_text: &str,
    payload: StagePayload,
) -> Result<StageOutcome, StageError> {
    let mut ctx = StageContext::new(inner, session, &TurnId::generate(), request_text, payload);
    inner
        .stages()
        .run_blocking(point, &mut ctx, &inner.executor)
}

impl Default for StageRegistry {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::await_holding_lock)]

    use super::*;
    use crate::config::Config;
    use crate::provider::{Provider, ProviderEvent, ProviderStream};
    use crate::runtime::Runtime;
    use crate::types::RuntimeEvent;
    use std::sync::Mutex;
    use std::sync::atomic::AtomicUsize;

    /// `VIKRAM_HOME` is process-global, so a test that points it at a temporary directory holds
    /// the shared guard for its whole body.
    fn isolated_home(dir: &std::path::Path) -> std::sync::MutexGuard<'static, ()> {
        let guard = crate::config::lock_security_env();
        unsafe { std::env::set_var("VIKRAM_HOME", dir.join("home")) };
        guard
    }

    fn config_for(dir: &std::path::Path) -> Config {
        Config {
            model: "test-model".into(),
            workspace_root: Some(dir.to_path_buf()),
            ..Config::default()
        }
    }

    /// A provider that records each request and answers with one text delta and a finish, so a
    /// submitted turn runs exactly one provider round.
    struct RecordingProvider {
        requests: Mutex<Vec<Vec<Message>>>,
    }

    impl Provider for RecordingProvider {
        fn name(&self) -> &str {
            "recording"
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
            let events = vec![
                Ok(ProviderEvent::TextDelta("done".into())),
                Ok(ProviderEvent::Finished {
                    input_tokens: Some(1),
                    output_tokens: Some(1),
                }),
            ];
            Box::pin(futures_util::stream::iter(events))
        }
    }

    async fn runtime_and_session() -> (
        tempfile::TempDir,
        std::sync::MutexGuard<'static, ()>,
        Runtime,
        vikram_contracts::SessionId,
        Arc<SessionRuntime>,
    ) {
        let dir = tempfile::tempdir().expect("temp dir");
        let home = isolated_home(dir.path());
        let provider = Arc::new(RecordingProvider {
            requests: Mutex::new(Vec::new()),
        });
        let runtime = Runtime::start_with_provider(config_for(dir.path()), provider)
            .await
            .expect("runtime starts");
        let handle = runtime.handle();
        let session = handle.snapshot().sessions[0].id.clone();
        handle
            .rename_session(&session, "stage test".into())
            .expect("name the session");
        let live = handle.inner.session(&session).expect("the session is live");
        (dir, home, runtime, session, live)
    }

    fn context_for(
        inner: &Arc<RuntimeInner>,
        session: &Arc<SessionRuntime>,
        payload: StagePayload,
    ) -> StageContext {
        StageContext::new(inner, session, &TurnId::generate(), "test request", payload)
    }

    /// A handler that stamps a marker into whichever payload it is given.
    struct Marker {
        text: &'static str,
        calls: Arc<AtomicUsize>,
    }

    impl StageHandler for Marker {
        fn name(&self) -> &'static str {
            "test.marker"
        }

        fn handle<'a>(&'a self, ctx: &'a mut StageContext) -> StageFuture<'a> {
            Box::pin(async move {
                self.calls.fetch_add(1, Ordering::SeqCst);
                match &mut ctx.payload {
                    StagePayload::MemoryRecall(payload) => {
                        payload.text = Some(self.text.to_owned());
                    }
                    StagePayload::ContextBuild(payload) => {
                        payload.prefix = vec![Message::text(
                            vikram_contracts::Role::System,
                            self.text,
                            vikram_contracts::Trust::External,
                        )];
                        payload.labels = vec!["test marker"];
                    }
                    _ => {}
                }
                Ok(StageOutcome::Done)
            })
        }
    }

    struct Failing;

    impl StageHandler for Failing {
        fn name(&self) -> &'static str {
            "test.failing"
        }

        fn handle<'a>(&'a self, ctx: &'a mut StageContext) -> StageFuture<'a> {
            Box::pin(async move {
                Err(StageError::new(
                    ctx.point,
                    self.name(),
                    "deliberate test failure",
                ))
            })
        }
    }

    fn recall_payload() -> StagePayload {
        StagePayload::MemoryRecall(MemoryRecallPayload::default())
    }

    #[tokio::test]
    async fn an_unregistered_point_runs_its_builtin_default() {
        let (_dir, _home, runtime, _session, live) = runtime_and_session().await;
        let handle = runtime.handle();
        let registry = StageRegistry::new();
        assert!(!registry.is_registered(StagePoint::MemoryRecall));

        let mut ctx = context_for(&handle.inner, &live, recall_payload());
        let outcome = registry
            .run(StagePoint::MemoryRecall, &mut ctx)
            .await
            .expect("the builtin default runs");
        assert!(matches!(outcome, StageOutcome::Done));
    }

    #[tokio::test]
    async fn the_higher_priority_registration_wins_dispatch() {
        let (_dir, _home, runtime, _session, live) = runtime_and_session().await;
        let handle = runtime.handle();
        let registry = StageRegistry::new();
        let low_calls = Arc::new(AtomicUsize::new(0));
        let high_calls = Arc::new(AtomicUsize::new(0));
        registry.register(
            StagePoint::MemoryRecall,
            Arc::new(Marker {
                text: "low",
                calls: Arc::clone(&low_calls),
            }),
            1,
        );
        registry.register(
            StagePoint::MemoryRecall,
            Arc::new(Marker {
                text: "high",
                calls: Arc::clone(&high_calls),
            }),
            9,
        );
        let mut ctx = context_for(&handle.inner, &live, recall_payload());
        registry
            .run(StagePoint::MemoryRecall, &mut ctx)
            .await
            .expect("dispatch succeeds");
        let StagePayload::MemoryRecall(payload) = ctx.payload else {
            panic!("wrong payload");
        };
        assert_eq!(payload.text.as_deref(), Some("high"));
        assert_eq!(high_calls.load(Ordering::SeqCst), 1);
        assert_eq!(low_calls.load(Ordering::SeqCst), 0, "the loser never ran");
    }

    #[tokio::test]
    async fn an_equal_priority_tie_goes_to_the_earlier_registration() {
        let (_dir, _home, runtime, _session, live) = runtime_and_session().await;
        let handle = runtime.handle();
        let registry = StageRegistry::new();
        let first = Arc::new(Marker {
            text: "first",
            calls: Arc::new(AtomicUsize::new(0)),
        });
        let second = Arc::new(Marker {
            text: "second",
            calls: Arc::new(AtomicUsize::new(0)),
        });
        registry.register(StagePoint::MemoryRecall, first, 5);
        registry.register(StagePoint::MemoryRecall, second, 5);
        let mut ctx = context_for(&handle.inner, &live, recall_payload());
        registry
            .run(StagePoint::MemoryRecall, &mut ctx)
            .await
            .expect("dispatch succeeds");
        let StagePayload::MemoryRecall(payload) = ctx.payload else {
            panic!("wrong payload");
        };
        assert_eq!(payload.text.as_deref(), Some("first"));
    }

    #[tokio::test]
    async fn a_failing_handler_fails_the_stage_instead_of_falling_back() {
        let (_dir, _home, runtime, _session, live) = runtime_and_session().await;
        let handle = runtime.handle();
        let registry = StageRegistry::new();
        registry.register(StagePoint::MemoryRecall, Arc::new(Failing), 1);
        let mut ctx = context_for(&handle.inner, &live, recall_payload());
        let err = registry
            .run(StagePoint::MemoryRecall, &mut ctx)
            .await
            .expect_err("the handler error propagates");
        assert_eq!(err.point, StagePoint::MemoryRecall);
        assert_eq!(err.handler, "test.failing");
        assert!(err.to_string().contains("deliberate test failure"));
        let StagePayload::MemoryRecall(payload) = ctx.payload else {
            panic!("wrong payload");
        };
        assert!(
            payload.text.is_none(),
            "no silent fallback rewrote the payload"
        );
    }

    /// Stamps the `memory_recall` payload, then declines so the next handler in the chain
    /// resolves the stage with the stamp still in place.
    struct Passing {
        stamp: &'static str,
        calls: Arc<AtomicUsize>,
    }

    impl StageHandler for Passing {
        fn name(&self) -> &'static str {
            "test.passing"
        }

        fn handle<'a>(&'a self, ctx: &'a mut StageContext) -> StageFuture<'a> {
            Box::pin(async move {
                self.calls.fetch_add(1, Ordering::SeqCst);
                if let StagePayload::MemoryRecall(payload) = &mut ctx.payload {
                    payload.text = Some(match payload.text.take() {
                        Some(existing) => format!("{existing}|{}", self.stamp),
                        None => self.stamp.to_owned(),
                    });
                }
                Ok(StageOutcome::Pass)
            })
        }
    }

    /// Resolves `memory_recall` by appending to whatever text earlier handlers left behind.
    struct Appending {
        suffix: &'static str,
        calls: Arc<AtomicUsize>,
    }

    impl StageHandler for Appending {
        fn name(&self) -> &'static str {
            "test.appending"
        }

        fn handle<'a>(&'a self, ctx: &'a mut StageContext) -> StageFuture<'a> {
            Box::pin(async move {
                self.calls.fetch_add(1, Ordering::SeqCst);
                if let StagePayload::MemoryRecall(payload) = &mut ctx.payload {
                    payload.text = Some(format!(
                        "{}+{}",
                        payload.text.take().unwrap_or_default(),
                        self.suffix
                    ));
                }
                Ok(StageOutcome::Done)
            })
        }
    }

    /// Declines every dispatch without touching the payload.
    struct AlwaysPass {
        calls: Arc<AtomicUsize>,
    }

    impl StageHandler for AlwaysPass {
        fn name(&self) -> &'static str {
            "test.always_pass"
        }

        fn handle<'a>(&'a self, _ctx: &'a mut StageContext) -> StageFuture<'a> {
            Box::pin(async move {
                self.calls.fetch_add(1, Ordering::SeqCst);
                Ok(StageOutcome::Pass)
            })
        }
    }

    /// Refuses every `memory_recall` dispatch.
    struct Refusing {
        calls: Arc<AtomicUsize>,
    }

    impl StageHandler for Refusing {
        fn name(&self) -> &'static str {
            "test.refusing"
        }

        fn handle<'a>(&'a self, _ctx: &'a mut StageContext) -> StageFuture<'a> {
            Box::pin(async move {
                self.calls.fetch_add(1, Ordering::SeqCst);
                Ok(StageOutcome::Refused("the stage says no".to_owned()))
            })
        }
    }

    #[tokio::test]
    async fn a_passing_handler_hands_the_dispatch_to_the_next_registration() {
        let (_dir, _home, runtime, _session, live) = runtime_and_session().await;
        let handle = runtime.handle();
        let registry = StageRegistry::new();
        let pass_calls = Arc::new(AtomicUsize::new(0));
        let append_calls = Arc::new(AtomicUsize::new(0));
        registry.register(
            StagePoint::MemoryRecall,
            Arc::new(Passing {
                stamp: "outer",
                calls: Arc::clone(&pass_calls),
            }),
            9,
        );
        registry.register(
            StagePoint::MemoryRecall,
            Arc::new(Appending {
                suffix: "inner",
                calls: Arc::clone(&append_calls),
            }),
            1,
        );
        let mut ctx = context_for(&handle.inner, &live, recall_payload());
        let outcome = registry
            .run(StagePoint::MemoryRecall, &mut ctx)
            .await
            .expect("dispatch succeeds");
        assert!(matches!(outcome, StageOutcome::Done));
        let StagePayload::MemoryRecall(payload) = ctx.payload else {
            panic!("wrong payload");
        };
        assert_eq!(
            payload.text.as_deref(),
            Some("outer+inner"),
            "the passer's mutation is what the next handler resolved"
        );
        assert_eq!(pass_calls.load(Ordering::SeqCst), 1);
        assert_eq!(append_calls.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn every_registration_passing_falls_through_to_the_builtin_default() {
        let (_dir, _home, runtime, _session, live) = runtime_and_session().await;
        let handle = runtime.handle();
        let registry = StageRegistry::new();
        let high_calls = Arc::new(AtomicUsize::new(0));
        let low_calls = Arc::new(AtomicUsize::new(0));
        registry.register(
            StagePoint::MemoryRecall,
            Arc::new(AlwaysPass {
                calls: Arc::clone(&high_calls),
            }),
            9,
        );
        registry.register(
            StagePoint::MemoryRecall,
            Arc::new(AlwaysPass {
                calls: Arc::clone(&low_calls),
            }),
            1,
        );
        let mut ctx = context_for(&handle.inner, &live, recall_payload());
        let outcome = registry
            .run(StagePoint::MemoryRecall, &mut ctx)
            .await
            .expect("the builtin default resolves the stage");
        assert!(matches!(outcome, StageOutcome::Done));
        assert_eq!(high_calls.load(Ordering::SeqCst), 1);
        assert_eq!(low_calls.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn a_refusal_is_terminal_and_lower_registrations_never_run() {
        let (_dir, _home, runtime, _session, live) = runtime_and_session().await;
        let handle = runtime.handle();
        let registry = StageRegistry::new();
        let refuse_calls = Arc::new(AtomicUsize::new(0));
        let marker_calls = Arc::new(AtomicUsize::new(0));
        registry.register(
            StagePoint::MemoryRecall,
            Arc::new(Refusing {
                calls: Arc::clone(&refuse_calls),
            }),
            9,
        );
        registry.register(
            StagePoint::MemoryRecall,
            Arc::new(Marker {
                text: "low",
                calls: Arc::clone(&marker_calls),
            }),
            1,
        );
        let mut ctx = context_for(&handle.inner, &live, recall_payload());
        let outcome = registry
            .run(StagePoint::MemoryRecall, &mut ctx)
            .await
            .expect("dispatch succeeds");
        assert!(matches!(outcome, StageOutcome::Refused(reason) if reason == "the stage says no"));
        assert_eq!(refuse_calls.load(Ordering::SeqCst), 1);
        assert_eq!(
            marker_calls.load(Ordering::SeqCst),
            0,
            "the loser never ran"
        );
    }

    #[tokio::test]
    async fn a_registered_context_build_handler_replaces_the_stage_and_its_output_flows() {
        let dir = tempfile::tempdir().expect("temp dir");
        let _home = isolated_home(dir.path());
        let recorder = Arc::new(RecordingProvider {
            requests: Mutex::new(Vec::new()),
        });
        let runtime = Runtime::start_with_provider(
            config_for(dir.path()),
            Arc::clone(&recorder) as Arc<dyn Provider>,
        )
        .await
        .expect("runtime starts");
        let handle = runtime.handle();
        let session = handle.snapshot().sessions[0].id.clone();
        handle
            .rename_session(&session, "stage test".into())
            .expect("name the session");
        let calls = Arc::new(AtomicUsize::new(0));
        handle.register_stage(
            StagePoint::ContextBuild,
            Arc::new(Marker {
                text: "STAGE-MARKER",
                calls: Arc::clone(&calls),
            }),
            10,
        );
        assert!(handle.inner.stages.is_registered(StagePoint::ContextBuild));

        let mut events = handle.events();
        handle
            .submit(&session, "hello".into())
            .expect("the turn is accepted");
        let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(10);
        loop {
            assert!(
                tokio::time::Instant::now() < deadline,
                "the turn never finished"
            );
            let Ok(Ok(event)) =
                tokio::time::timeout(std::time::Duration::from_millis(100), events.recv()).await
            else {
                continue;
            };
            if matches!(event, RuntimeEvent::TurnFinished { .. }) {
                break;
            }
        }

        assert_eq!(calls.load(Ordering::SeqCst), 1, "the stage ran once");
        // The marker the handler wrote must be what the provider was sent.
        let requests = recorder.requests.lock().expect("requests lock");
        assert_eq!(requests.len(), 1, "exactly one provider round ran");
        let marker = requests[0].iter().any(|message| {
            message.blocks.iter().any(|block| {
                matches!(block, vikram_contracts::Block::Text { text, .. } if text == "STAGE-MARKER")
            })
        });
        assert!(marker, "the stage's prefix reached the provider request");
    }

    /// A `user_input` stage that rewrites the message, then hands it to the builtin so the
    /// durable commit — push, journal, `UserMessage` — still runs exactly as it does by default.
    struct RewriteInput {
        text: &'static str,
        calls: Arc<AtomicUsize>,
    }

    impl StageHandler for RewriteInput {
        fn name(&self) -> &'static str {
            "test.rewrite_input"
        }

        fn handle<'a>(&'a self, ctx: &'a mut StageContext) -> StageFuture<'a> {
            Box::pin(async move {
                self.calls.fetch_add(1, Ordering::SeqCst);
                if let StagePayload::UserInput(payload) = &mut ctx.payload {
                    payload.message = Message::user(self.text);
                }
                let default = crate::turn::default_stage_handler(StagePoint::UserInput)
                    .expect("the builtin user_input handler exists");
                default.handle(ctx).await
            })
        }
    }

    /// Counts dispatches at whichever point it is registered for, without touching the payload.
    struct Counting {
        calls: Arc<AtomicUsize>,
    }

    impl StageHandler for Counting {
        fn name(&self) -> &'static str {
            "test.counting"
        }

        fn handle<'a>(&'a self, _ctx: &'a mut StageContext) -> StageFuture<'a> {
            Box::pin(async move {
                self.calls.fetch_add(1, Ordering::SeqCst);
                Ok(StageOutcome::Done)
            })
        }
    }

    /// Records the outcome figures the `turn_end` stage is shown.
    struct TurnEndSpy {
        seen: Arc<Mutex<Vec<(crate::types::TurnState, u64, u64)>>>,
    }

    impl StageHandler for TurnEndSpy {
        fn name(&self) -> &'static str {
            "test.turn_end_spy"
        }

        fn handle<'a>(&'a self, ctx: &'a mut StageContext) -> StageFuture<'a> {
            Box::pin(async move {
                if let StagePayload::TurnEnd(payload) = &ctx.payload {
                    self.seen.lock().expect("seen lock").push((
                        payload.outcome,
                        payload.steps,
                        payload.output_tokens,
                    ));
                }
                Ok(StageOutcome::Done)
            })
        }
    }

    /// Runs one submitted turn to `TurnFinished` and returns the state it finished with.
    async fn run_one_turn(
        handle: &crate::runtime::RuntimeHandle,
        session: &vikram_contracts::SessionId,
        text: &str,
    ) -> crate::types::TurnState {
        let mut events = handle.events();
        handle
            .submit(session, text.into())
            .expect("the turn is accepted");
        let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(10);
        loop {
            assert!(
                tokio::time::Instant::now() < deadline,
                "the turn never finished"
            );
            let Ok(Ok(event)) =
                tokio::time::timeout(std::time::Duration::from_millis(100), events.recv()).await
            else {
                continue;
            };
            if let RuntimeEvent::TurnFinished { state, .. } = event {
                return state;
            }
        }
    }

    #[tokio::test]
    async fn a_registered_user_input_stage_rewrites_what_is_journaled_and_sent() {
        let dir = tempfile::tempdir().expect("temp dir");
        let _home = isolated_home(dir.path());
        let recorder = Arc::new(RecordingProvider {
            requests: Mutex::new(Vec::new()),
        });
        let runtime = Runtime::start_with_provider(
            config_for(dir.path()),
            Arc::clone(&recorder) as Arc<dyn Provider>,
        )
        .await
        .expect("runtime starts");
        let handle = runtime.handle();
        let session = handle.snapshot().sessions[0].id.clone();
        handle
            .rename_session(&session, "stage test".into())
            .expect("name the session");
        let calls = Arc::new(AtomicUsize::new(0));
        handle.register_stage(
            StagePoint::UserInput,
            Arc::new(RewriteInput {
                text: "STAGE-REWRITE",
                calls: Arc::clone(&calls),
            }),
            10,
        );

        run_one_turn(&handle, &session, "original words").await;

        assert_eq!(calls.load(Ordering::SeqCst), 1, "the stage ran once");
        let requests = recorder.requests.lock().expect("requests lock");
        assert_eq!(requests.len(), 1, "exactly one provider round ran");
        let rewritten = requests[0]
            .iter()
            .any(|message| message.text_body().contains("STAGE-REWRITE"));
        assert!(
            rewritten,
            "the rewritten input reached the provider request"
        );
        let original = requests[0]
            .iter()
            .any(|message| message.text_body().contains("original words"));
        assert!(!original, "the pre-stage text never reached the provider");
    }

    #[tokio::test]
    async fn user_input_response_commit_and_turn_end_stages_fire_at_their_sites() {
        let dir = tempfile::tempdir().expect("temp dir");
        let _home = isolated_home(dir.path());
        let runtime = Runtime::start_with_provider(
            config_for(dir.path()),
            Arc::new(RecordingProvider {
                requests: Mutex::new(Vec::new()),
            }) as Arc<dyn Provider>,
        )
        .await
        .expect("runtime starts");
        let handle = runtime.handle();
        let session = handle.snapshot().sessions[0].id.clone();
        handle
            .rename_session(&session, "stage test".into())
            .expect("name the session");

        let user_calls = Arc::new(AtomicUsize::new(0));
        let response_calls = Arc::new(AtomicUsize::new(0));
        let turn_end_seen = Arc::new(Mutex::new(Vec::new()));
        handle.register_stage(
            StagePoint::UserInput,
            Arc::new(RewriteInput {
                text: "stage input",
                calls: Arc::clone(&user_calls),
            }),
            10,
        );
        handle.register_stage(
            StagePoint::ResponseCommit,
            Arc::new(Counting {
                calls: Arc::clone(&response_calls),
            }),
            10,
        );
        handle.register_stage(
            StagePoint::TurnEnd,
            Arc::new(TurnEndSpy {
                seen: Arc::clone(&turn_end_seen),
            }),
            10,
        );

        let finished = run_one_turn(&handle, &session, "hello").await;

        assert_eq!(user_calls.load(Ordering::SeqCst), 1, "user_input ran once");
        assert_eq!(
            response_calls.load(Ordering::SeqCst),
            1,
            "response_commit ran once for the one assistant message"
        );
        let seen = turn_end_seen.lock().expect("seen lock");
        assert_eq!(seen.len(), 1, "turn_end ran once");
        assert_eq!(
            seen[0].0, finished,
            "the stage saw the outcome the turn finished with"
        );
    }

    #[test]
    fn stage_point_parse_and_names_cover_the_turn_level_points() {
        for (raw, point) in [
            ("user-input", StagePoint::UserInput),
            ("response.commit", StagePoint::ResponseCommit),
            ("turn_end", StagePoint::TurnEnd),
        ] {
            assert_eq!(StagePoint::parse(raw), Some(point));
            assert_eq!(StagePoint::parse(point.as_str()), Some(point));
        }
    }

    /// Write a plugin that declares `stages` under `components` plus an executable `stage.sh`.
    fn stage_plugin(root: &std::path::Path, id: &str, manifest: serde_json::Value, script: &str) {
        let dir = root.join(id);
        std::fs::create_dir_all(&dir).expect("plugin dir");
        std::fs::write(dir.join("plugin.json"), manifest.to_string()).expect("manifest");
        std::fs::write(dir.join("stage.sh"), script).expect("entry");
    }

    fn stage_manifest(
        id: &str,
        stages: &[&str],
        permissions: &[&str],
        priority: i32,
    ) -> serde_json::Value {
        serde_json::json!({
            "id": id,
            "components": {"stages": stages},
            "permissions": permissions,
            "entry": "stage.sh",
            "priority": priority,
        })
    }

    /// The effect script every stage test shares shape with: consume the context, print one
    /// hook-effect object.
    const EFFECT_SCRIPT: &str =
        "#!/bin/sh\ncat >/dev/null\nprintf '%s' '{\"append\":\"MARKER\"}'\n";

    #[tokio::test]
    async fn a_process_stage_plugin_rewrites_context_build_for_the_turn() {
        let dir = tempfile::tempdir().expect("temp dir");
        let _home = isolated_home(dir.path());
        let plugins = dir.path().join("home").join("plugins");
        stage_plugin(
            &plugins,
            "ctxy",
            stage_manifest("ctxy", &["context_build"], &["agents", "process"], 10),
            "#!/bin/sh\ncat >/dev/null\nprintf '%s' '{\"append\":\"PLUGIN-STAGE-MARKER\"}'\n",
        );
        let recorder = Arc::new(RecordingProvider {
            requests: Mutex::new(Vec::new()),
        });
        let runtime = Runtime::start_with_provider(
            config_for(dir.path()),
            Arc::clone(&recorder) as Arc<dyn Provider>,
        )
        .await
        .expect("runtime starts");
        let handle = runtime.handle();
        let session = handle.snapshot().sessions[0].id.clone();
        handle
            .rename_session(&session, "stage test".into())
            .expect("name the session");

        run_one_turn(&handle, &session, "hello").await;

        let requests = recorder.requests.lock().expect("requests lock");
        assert_eq!(requests.len(), 1, "exactly one provider round ran");
        let marker = requests[0].iter().any(|message| {
            message
                .blocks
                .iter()
                .any(|block| matches!(block, vikram_contracts::Block::Text { text, .. } if text == "PLUGIN-STAGE-MARKER"))
        });
        assert!(
            marker,
            "the plugin's context amendment reached the provider"
        );
    }

    #[tokio::test]
    async fn a_crashing_stage_plugin_fails_the_stage_instead_of_falling_back() {
        let (_dir, _home, runtime, _session, live) = runtime_and_session().await;
        let handle = runtime.handle();
        let plugins = _dir.path().join("plugins");
        stage_plugin(
            &plugins,
            "crashy",
            stage_manifest("crashy", &["memory_recall"], &["tools", "process"], 10),
            "#!/bin/sh\nexit 3\n",
        );
        let registry = StageRegistry::new();
        let scanned = PluginRegistry::scan(&plugins);
        let rejected = registry.reload_plugin_stages(&scanned);
        assert!(
            rejected.is_empty(),
            "the manifest itself is valid: {rejected:?}"
        );
        assert!(registry.is_registered(StagePoint::MemoryRecall));

        let mut ctx = context_for(&handle.inner, &live, recall_payload());
        let err = registry
            .run(StagePoint::MemoryRecall, &mut ctx)
            .await
            .expect_err("a crashed plugin fails the stage");
        assert_eq!(err.point, StagePoint::MemoryRecall);
        assert_eq!(err.handler, "plugin:crashy");
        assert!(
            err.to_string().contains("exited with 3"),
            "the failure names the plugin and its exit: {err}"
        );
    }

    #[tokio::test]
    async fn a_stage_point_without_its_permission_is_refused() {
        let dir = tempfile::tempdir().expect("temp dir");
        let _home = isolated_home(dir.path());
        let plugins = dir.path().join("plugins");
        // tool_dispatch maps to the tool_call hook surface, which requires `tools`.
        stage_plugin(
            &plugins,
            "sneaky",
            stage_manifest("sneaky", &["tool_dispatch"], &["agents", "process"], 10),
            EFFECT_SCRIPT,
        );
        stage_plugin(
            &plugins,
            "typo",
            stage_manifest("typo", &["not_a_point"], &["agents", "process"], 10),
            EFFECT_SCRIPT,
        );
        let registry = StageRegistry::new();
        let scanned = PluginRegistry::scan(&plugins);
        let rejected = registry.reload_plugin_stages(&scanned);
        assert!(
            !registry.is_registered(StagePoint::ToolDispatch),
            "the under-privileged plugin never registers"
        );
        assert!(
            rejected
                .iter()
                .any(|rejection| rejection.reason.contains("requires the `tools` permission")),
            "the permission refusal is reported: {rejected:?}"
        );
        assert!(
            rejected
                .iter()
                .any(|rejection| rejection.reason.contains("unknown stage point")),
            "the unknown point is reported: {rejected:?}"
        );
    }

    #[tokio::test]
    async fn the_higher_priority_stage_plugin_wins_dispatch() {
        let (_dir, _home, runtime, _session, live) = runtime_and_session().await;
        let handle = runtime.handle();
        let plugins = _dir.path().join("plugins");
        for (id, priority, marker) in [("low", 1, "LOW-MARKER"), ("high", 9, "HIGH-MARKER")] {
            stage_plugin(
                &plugins,
                id,
                stage_manifest(id, &["memory_recall"], &["tools", "process"], priority),
                &format!("#!/bin/sh\ncat >/dev/null\nprintf '%s' '{{\"append\":\"{marker}\"}}'\n"),
            );
        }
        let registry = StageRegistry::new();
        let scanned = PluginRegistry::scan(&plugins);
        registry.reload_plugin_stages(&scanned);
        assert!(registry.is_registered(StagePoint::MemoryRecall));

        let mut ctx = context_for(&handle.inner, &live, recall_payload());
        registry
            .run(StagePoint::MemoryRecall, &mut ctx)
            .await
            .expect("dispatch succeeds");
        let StagePayload::MemoryRecall(payload) = ctx.payload else {
            panic!("wrong payload");
        };
        let text = payload.text.unwrap_or_default();
        assert!(text.contains("HIGH-MARKER"), "{text}");
        assert!(!text.contains("LOW-MARKER"), "the loser never ran: {text}");
    }

    #[tokio::test]
    async fn a_plugin_stage_handler_is_removed_when_the_plugin_is() {
        let (_dir, _home, runtime, _session, live) = runtime_and_session().await;
        let handle = runtime.handle();
        let registry = StageRegistry::new();
        let plugins = _dir.path().join("plugins");
        stage_plugin(
            &plugins,
            "gone",
            stage_manifest("gone", &["context_build"], &["agents", "process"], 10),
            EFFECT_SCRIPT,
        );
        let scanned = PluginRegistry::scan(&plugins);
        registry.reload_plugin_stages(&scanned);
        assert!(registry.is_registered(StagePoint::ContextBuild));

        // A rescan with the plugin gone drops the plugin registration; native ones survive.
        let native_calls = Arc::new(AtomicUsize::new(0));
        registry.register(
            StagePoint::ContextBuild,
            Arc::new(Marker {
                text: "native",
                calls: Arc::clone(&native_calls),
            }),
            1,
        );
        let empty = PluginRegistry::scan(&_dir.path().join("elsewhere"));
        registry.reload_plugin_stages(&empty);
        assert!(registry.is_registered(StagePoint::ContextBuild));

        // The plugin handler is gone: the surviving native registration wins the dispatch.
        let mut ctx = context_for(
            &handle.inner,
            &live,
            StagePayload::ContextBuild(ContextBuildPayload {
                hook_injected: Vec::new(),
                profile: None,
                memory_text: None,
                prefix: Vec::new(),
                labels: Vec::new(),
            }),
        );
        registry
            .run(StagePoint::ContextBuild, &mut ctx)
            .await
            .expect("the native handler still dispatches");
        assert_eq!(native_calls.load(Ordering::SeqCst), 1);
        let StagePayload::ContextBuild(payload) = ctx.payload else {
            panic!("wrong payload");
        };
        assert!(
            payload
                .prefix
                .iter()
                .any(|message| message.text_body() == "native"),
            "the native marker is what the stage produced"
        );
    }

    /// Records every `session_lifecycle` dispatch as `(op, session, title)`.
    struct LifecycleSpy {
        seen: Arc<Mutex<Vec<(SessionLifecycleOp, SessionId, String)>>>,
    }

    impl StageHandler for LifecycleSpy {
        fn name(&self) -> &'static str {
            "test.lifecycle_spy"
        }

        fn handle<'a>(&'a self, ctx: &'a mut StageContext) -> StageFuture<'a> {
            Box::pin(async move {
                if let StagePayload::SessionLifecycle(payload) = &ctx.payload {
                    self.seen.lock().expect("seen lock").push((
                        payload.op,
                        payload.session.clone(),
                        payload.title.clone(),
                    ));
                }
                Ok(StageOutcome::Done)
            })
        }
    }

    /// Refuses `create` and passes every other lifecycle operation.
    struct RefuseCreate;

    impl StageHandler for RefuseCreate {
        fn name(&self) -> &'static str {
            "test.refuse_create"
        }

        fn handle<'a>(&'a self, ctx: &'a mut StageContext) -> StageFuture<'a> {
            Box::pin(async move {
                if matches!(
                    &ctx.payload,
                    StagePayload::SessionLifecycle(SessionLifecyclePayload {
                        op: SessionLifecycleOp::Create,
                        ..
                    })
                ) {
                    return Ok(StageOutcome::Refused("the stage says no".to_owned()));
                }
                Ok(StageOutcome::Done)
            })
        }
    }

    /// Refuses every lifecycle operation; only `create` is allowed to honor that.
    struct RefuseAll;

    impl StageHandler for RefuseAll {
        fn name(&self) -> &'static str {
            "test.refuse_all"
        }

        fn handle<'a>(&'a self, ctx: &'a mut StageContext) -> StageFuture<'a> {
            Box::pin(async move {
                if matches!(ctx.payload, StagePayload::SessionLifecycle(_)) {
                    return Ok(StageOutcome::Refused("observe-only".to_owned()));
                }
                Ok(StageOutcome::Done)
            })
        }
    }

    #[tokio::test]
    async fn session_lifecycle_stage_fires_on_create_open_close_and_delete() {
        let (_dir, _home, runtime, _session, _live) = runtime_and_session().await;
        let handle = runtime.handle();
        let seen = Arc::new(Mutex::new(Vec::new()));
        handle.register_stage(
            StagePoint::SessionLifecycle,
            Arc::new(LifecycleSpy {
                seen: Arc::clone(&seen),
            }),
            10,
        );

        let created = handle
            .create_session(Some("lifecycle".into()))
            .expect("create");
        handle.close_session(&created).expect("close");
        handle.open_session(&created).expect("open");
        handle.delete_session(&created).expect("delete");

        let seen = seen.lock().expect("seen lock");
        let ops: Vec<SessionLifecycleOp> = seen.iter().map(|(op, _, _)| *op).collect();
        assert_eq!(
            ops,
            vec![
                SessionLifecycleOp::Create,
                SessionLifecycleOp::Close,
                SessionLifecycleOp::Open,
                SessionLifecycleOp::Delete,
            ]
        );
        for (_, session, title) in seen.iter() {
            assert_eq!(
                session, &created,
                "every dispatch names the operated session"
            );
            assert_eq!(title, "lifecycle");
        }
    }

    #[tokio::test]
    async fn a_session_lifecycle_stage_can_veto_create() {
        let (_dir, _home, runtime, _session, _live) = runtime_and_session().await;
        let handle = runtime.handle();
        let live_before = handle.snapshot().sessions.len();
        handle.register_stage(StagePoint::SessionLifecycle, Arc::new(RefuseCreate), 10);

        let err = handle
            .create_session(Some("vetoed".into()))
            .expect_err("the stage vetoes the create");
        assert!(
            err.to_string().contains("a stage refused this session"),
            "{err}"
        );
        assert_eq!(
            handle.snapshot().sessions.len(),
            live_before,
            "the vetoed session never went live"
        );
    }

    #[tokio::test]
    async fn a_refusal_on_an_observe_only_operation_does_not_block_it() {
        let (_dir, _home, runtime, session, _live) = runtime_and_session().await;
        let handle = runtime.handle();
        handle.register_stage(StagePoint::SessionLifecycle, Arc::new(RefuseAll), 10);

        handle.close_session(&session).expect("close proceeds");
        handle.open_session(&session).expect("open proceeds");
        handle.delete_session(&session).expect("delete proceeds");
    }

    /// One recorded `agent_lifecycle` dispatch: `(op, agent, parent, session, task, delivered)`.
    type AgentDispatch = (
        AgentLifecycleOp,
        Option<BotId>,
        SessionId,
        SessionId,
        String,
        bool,
    );

    /// Records every `agent_lifecycle` dispatch.
    struct AgentSpy {
        seen: Arc<Mutex<Vec<AgentDispatch>>>,
    }

    impl StageHandler for AgentSpy {
        fn name(&self) -> &'static str {
            "test.agent_spy"
        }

        fn handle<'a>(&'a self, ctx: &'a mut StageContext) -> StageFuture<'a> {
            Box::pin(async move {
                if let StagePayload::AgentLifecycle(payload) = &ctx.payload {
                    self.seen.lock().expect("seen lock").push((
                        payload.op,
                        payload.agent.clone(),
                        payload.parent.clone(),
                        payload.session.clone(),
                        payload.task.clone(),
                        payload.delivered,
                    ));
                }
                Ok(StageOutcome::Done)
            })
        }
    }

    #[tokio::test]
    async fn agent_lifecycle_stage_observes_spawn_and_deliver() {
        let dir = tempfile::tempdir().expect("temp dir");
        let _home = isolated_home(dir.path());
        let runtime = Runtime::start_with_provider(
            config_for(dir.path()),
            Arc::new(RecordingProvider {
                requests: Mutex::new(Vec::new()),
            }) as Arc<dyn Provider>,
        )
        .await
        .expect("runtime starts");
        let handle = runtime.handle();
        let parent = handle.snapshot().sessions[0].id.clone();
        let seen = Arc::new(Mutex::new(Vec::new()));
        handle.register_stage(
            StagePoint::AgentLifecycle,
            Arc::new(AgentSpy {
                seen: Arc::clone(&seen),
            }),
            10,
        );
        let bot = handle
            .bot_create("Spybot", "tester", "watches things")
            .expect("bot created");
        handle
            .spawn_agent(bot.clone(), "audit the code".into(), parent.clone())
            .expect("spawn");

        // Spawn is synchronous: it is already recorded.
        {
            let seen = seen.lock().expect("seen lock");
            assert_eq!(seen.len(), 1, "spawn dispatched once");
            let (op, agent, seen_parent, _child, task, delivered) = &seen[0];
            assert_eq!(*op, AgentLifecycleOp::Spawn);
            assert_eq!(agent.as_ref(), Some(&bot));
            assert_eq!(seen_parent, &parent);
            assert_eq!(task.as_str(), "audit the code");
            assert!(!*delivered);
        }

        // Deliver lands when the delegated turn finishes; RecordingProvider answers in one
        // round, so this is a poll, not a wait on anything slow.
        let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(10);
        loop {
            if seen
                .lock()
                .expect("seen lock")
                .iter()
                .any(|(op, ..)| *op == AgentLifecycleOp::Deliver)
            {
                break;
            }
            assert!(
                tokio::time::Instant::now() < deadline,
                "the deliver dispatch never arrived"
            );
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        let seen = seen.lock().expect("seen lock");
        let deliver = seen
            .iter()
            .find(|(op, ..)| *op == AgentLifecycleOp::Deliver)
            .expect("deliver recorded");
        let (_, agent, seen_parent, child, _, delivered) = deliver;
        assert_eq!(agent.as_ref(), Some(&bot));
        assert_eq!(seen_parent, &parent);
        assert!(*delivered, "the answer reached the live parent");
        let spawn_child = seen[0].3.clone();
        assert_eq!(child, &spawn_child, "deliver names the spawned session");
    }
}
