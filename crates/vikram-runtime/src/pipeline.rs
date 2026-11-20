//! The stage pipeline.
//!
//! A stage is a named, ordered step in the turn loop that a native module can replace wholesale.
//! Where a [`crate::hooks::HookRegistry`] hook observes or amends what the runtime is about to do,
//! a stage *is* the step: [`StageRegistry::run`] dispatches each [`StagePoint`] to exactly one
//! [`StageHandler`] — the highest-priority registration, or the builtin default when nothing is
//! registered — and the handler's writes to the [`StageContext`] payload are what the turn
//! continues with.
//!
//! Failure policy is fail-stage, never silent fallback: a handler that returns [`StageError`]
//! fails the stage and the turn reports it. Falling back to the default would hide a broken
//! module behind behaviour that only looks healthy.
//!
//! Process plugins cannot register stages — they are out-of-process — but they reach the same
//! seams through the hook chain (`context_build`, `memory_recall`, `model_request`,
//! `tool_call`, `tool_result`), which every builtin handler still dispatches.

use std::collections::HashMap;
use std::fmt;
use std::future::Future;
use std::pin::Pin;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, RwLock};

use vikram_contracts::{CancellationToken, Message, ToolCall, ToolCallId, TurnId};

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
            _ => None,
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
        }
    }
}

/// Everything a stage handler is given at one point: the runtime objects it may need, the turn's
/// prose, and the point's payload — inputs on the way in, outputs on the way out.
///
/// `inner`, `session` and `turn` are crate-internal: stage registration is for in-process/native
/// modules, which live in this crate. A payload-only handler in another crate can still rewrite
/// what a stage produces through the public fields.
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
    /// The stage completed; its outputs are in the context payload.
    Done,
    /// The stage declines to let the turn continue; `reason` is operator-facing. The caller maps
    /// it onto the same refusal path the point's hooks already had.
    Refused(String),
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
    /// when they are authoritative, [`StageOutcome::Refused`] to veto the turn, or [`StageError`]
    /// when the stage itself failed.
    fn handle<'a>(&'a self, ctx: &'a mut StageContext) -> StageFuture<'a>;
}

struct StageRegistration {
    name: &'static str,
    handler: Arc<dyn StageHandler>,
    priority: i32,
    /// Registration order; earlier wins a priority tie so dispatch stays deterministic.
    sequence: u64,
}

/// The stage registry: named points mapped to their winning handler.
///
/// At most one handler runs per point per dispatch — the highest `priority` registration, with
/// registration order breaking ties. When nothing is registered the builtin default runs, so the
/// pipeline is fully populated from boot and a registration *replaces* a step rather than
/// wrapping it.
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

    /// Register a native handler for `point`. Higher `priority` wins dispatch; ties go to the
    /// earlier registration. The builtin default runs only while nothing is registered.
    pub fn register(&self, point: StagePoint, handler: Arc<dyn StageHandler>, priority: i32) {
        let sequence = self.sequence.fetch_add(1, Ordering::SeqCst);
        let name = handler.name();
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
        });
        entries.sort_by(|a, b| {
            b.priority
                .cmp(&a.priority)
                .then_with(|| a.sequence.cmp(&b.sequence))
        });
    }

    /// Whether a native handler is registered for `point` (the builtin default does not count).
    pub fn is_registered(&self, point: StagePoint) -> bool {
        self.registrations
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .get(&point)
            .is_some_and(|entries| !entries.is_empty())
    }

    /// Dispatch `point` to its winning handler — a registration if any, else the builtin
    /// default — and return its outcome. A handler error is attributed and propagated: the stage
    /// fails, it does not silently fall back.
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
        let selected = {
            let registrations = self.registrations.read().unwrap_or_else(|e| e.into_inner());
            registrations
                .get(&point)
                .and_then(|entries| entries.first())
                .map(|entry| (Arc::clone(&entry.handler), entry.name))
        };
        let (handler, name) = match selected {
            Some(selected) => selected,
            None => match crate::turn::default_stage_handler(point) {
                Some(handler) => {
                    let name = handler.name();
                    (handler, name)
                }
                None => {
                    return Err(StageError::new(
                        point,
                        "registry",
                        format!(
                            "no handler registered for stage `{}` and no builtin default exists",
                            point.as_str()
                        ),
                    ));
                }
            },
        };
        handler
            .handle(ctx)
            .await
            .map_err(|err| StageError::new(point, name, err.message))
    }
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
}
