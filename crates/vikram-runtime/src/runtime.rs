//! Runtime facade.
//!
//! `Runtime` owns the process-wide state; `RuntimeHandle` is the cheap clone every view and task
//! holds. The handle exposes observations and explicit controls only — there is no method that
//! stops work as a side effect of changing what is on screen.

// The runtime's construction and control surfaces mirror the storage rows they map to; the long
// parameter lists are the row fields, and grouping them would hide which field is which.
#![allow(clippy::too_many_arguments)]

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use tokio::sync::{broadcast, oneshot};
use vikram_contracts::journal::JournalSink;
use vikram_contracts::{
    ActionKind, ApprovalId, BotId, Decision, Message, PermissionMode, PermissionRequest, Posture,
    RecordKind, SessionId,
};
use vikram_policy::{MandatoryFloor, Policy, ResourcePattern, Rule};
use vikram_store::Store;

use crate::bots::{BotManager, ManagedBot, TaskManager};
use crate::bus::EventBus;
use crate::config::{Config, McpConfig, ModeAvailability, database_path, ensure_home};
use crate::error::{Result, RuntimeError};
use crate::hive_roster::{HiveRoster, HiveRosterManager};
use crate::mcp::McpRegistry;
use crate::plugins::PluginRegistry;
use crate::provider::Provider;
use crate::rules::RuleBook;
use crate::session::SessionRuntime;
use crate::skills::{Skill, SkillRegistry};
use crate::tickets::TicketManager;
use crate::titling;
use crate::tools::{McpTool, ToolContext, ToolRegistry};
use crate::types::{
    ApprovalPrompt, BackgroundTask, BotSnapshot, BotState, HiveRosterSnapshot, Metrics,
    NoticeLevel, RuntimeEvent, RuntimeStatus, SessionSnapshot, SessionSummary, StateSnapshot,
    TaskState, now_ms,
};
use crate::workspace::WorkspaceManager;
use vikram_mcp::visibility::VisibilityManager;

pub(crate) struct PendingApproval {
    pub prompt: ApprovalPrompt,
    pub respond: oneshot::Sender<bool>,
}

/// An `ask_user` question waiting on the operator. The turn loop holds the receiver; answering
/// through [`RuntimeHandle::answer_question`] resolves it, exactly like an approval.
pub(crate) type PendingQuestion = oneshot::Sender<String>;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BotControl {
    Pause,
    Resume,
    Stop,
}

/// One message waiting behind a running turn, as [`RuntimeHandle::pending_list`] reports it.
#[derive(Clone, Debug, PartialEq)]
pub struct PendingMessage {
    /// Stable handle for the management calls: `"steer"` or `"q<index>"`.
    pub target: String,
    /// True for the steer row, false for a queued follow-up.
    pub steer: bool,
    pub text: String,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum PendingTarget {
    Steer,
    Queue(usize),
}

fn pending_index(target: &str) -> Option<PendingTarget> {
    if target == "steer" {
        return Some(PendingTarget::Steer);
    }
    target
        .strip_prefix('q')
        .and_then(|rest| rest.parse::<usize>().ok())
        .map(PendingTarget::Queue)
}

/// How often the runtime re-checks its own evolution store. Each tick runs only the
/// read-only `mapek.monitor()`/`analyze()` probes; the mutating plan/execute half of
/// MAPE-K stays operator-driven.
const SELF_CHECK_INTERVAL: std::time::Duration = std::time::Duration::from_secs(60 * 60);
/// Self-check ticks between improvement-opportunity scans — roughly daily at the
/// hourly cadence.
const SELF_CHECK_OPPORTUNITY_EVERY: u64 = 24;

/// Hive group-chat concerns (canonical: `hive_chat::HiveManager` over
/// `vikram-store`). Extracted from the god struct so hive construction
/// and helpers live together; `RuntimeInner::hive` is the field.
pub(crate) struct HiveState {
    pub manager: crate::HiveManager,
}

/// Ticket concerns (canonical: `tickets::TicketManager` over
/// `vikram-store`). Extracted alongside [`HiveState`].
pub(crate) struct TicketState {
    pub manager: TicketManager,
}

/// Thin wrappers that mirror [`crate::HiveManager`] for callers holding the runtime rather than the
/// manager. Some are only reachable through that manager today; they are kept as the complete
/// surface so the runtime never has to be widened later.
#[allow(dead_code)]
impl HiveState {
    pub fn store(&self) -> Arc<Store> {
        self.manager.store()
    }

    pub fn create(
        &self,
        owner: vikram_contracts::UserId,
        profile: Option<vikram_contracts::ProfileId>,
        name: &str,
        objective: &str,
        policy: &str,
        now_ms: u64,
    ) -> std::result::Result<vikram_store::HiveRow, String> {
        self.manager
            .create(owner, profile, name, objective, policy, now_ms)
    }

    pub fn list(&self) -> std::result::Result<Vec<vikram_store::HiveRow>, String> {
        self.manager.list()
    }

    pub fn list_by_user(
        &self,
        user: &vikram_contracts::UserId,
    ) -> std::result::Result<Vec<vikram_store::HiveRow>, String> {
        self.manager.list_by_user(user)
    }

    pub fn get(
        &self,
        id: &vikram_contracts::HiveId,
    ) -> std::result::Result<Option<vikram_store::HiveRow>, String> {
        self.manager.get(id)
    }

    pub fn delete(&self, id: &vikram_contracts::HiveId) -> std::result::Result<(), String> {
        self.manager.delete(id)
    }

    pub fn set_session(
        &self,
        id: &vikram_contracts::HiveId,
        session: Option<&vikram_contracts::SessionId>,
        now_ms: u64,
    ) -> std::result::Result<(), String> {
        self.manager.set_session(id, session, now_ms)
    }

    pub fn update_state(
        &self,
        id: &vikram_contracts::HiveId,
        state: &str,
        now_ms: u64,
    ) -> std::result::Result<(), String> {
        self.manager.update_state(id, state, now_ms)
    }

    pub fn add_participant(
        &self,
        hive: &vikram_contracts::HiveId,
        entity: &vikram_contracts::EntityId,
        kind: vikram_store::EntityKind,
        role: &str,
        now_ms: u64,
    ) -> std::result::Result<(), String> {
        self.manager
            .add_participant(hive, entity, kind, role, now_ms)
    }

    pub fn remove_participant(
        &self,
        hive: &vikram_contracts::HiveId,
        entity: &vikram_contracts::EntityId,
    ) -> std::result::Result<(), String> {
        self.manager.remove_participant(hive, entity)
    }

    pub fn participants(
        &self,
        hive: &vikram_contracts::HiveId,
    ) -> std::result::Result<Vec<vikram_store::HiveParticipantRow>, String> {
        self.manager.participants(hive)
    }

    pub fn append_message(
        &self,
        hive: &vikram_contracts::HiveId,
        sender: &str,
        sender_kind: &str,
        content: &str,
        task_ref: Option<&str>,
        now_ms: u64,
    ) -> std::result::Result<vikram_store::HiveMessageRow, String> {
        self.manager
            .append_message(hive, sender, sender_kind, content, task_ref, now_ms)
    }

    pub fn messages(
        &self,
        hive: &vikram_contracts::HiveId,
    ) -> std::result::Result<Vec<vikram_store::HiveMessageRow>, String> {
        self.manager.messages(hive)
    }
}

/// Thin wrappers that mirror [`crate::TicketManager`] for callers holding the runtime rather than
/// the manager.
#[allow(dead_code)]
impl TicketState {
    pub fn store(&self) -> Arc<Store> {
        self.manager.store()
    }

    pub fn create(
        &self,
        owner_key: &str,
        owner_kind: &str,
        kind: &str,
        title: &str,
        request: &str,
        description: &str,
        strategy: Option<&str>,
        parent_ticket: Option<vikram_contracts::TicketId>,
        hive_id: Option<vikram_contracts::HiveId>,
        session_id: Option<vikram_contracts::SessionId>,
        now_ms: u64,
    ) -> std::result::Result<vikram_store::TicketRow, String> {
        self.manager.create(
            owner_key,
            owner_kind,
            kind,
            title,
            request,
            description,
            strategy,
            parent_ticket,
            hive_id,
            session_id,
            now_ms,
        )
    }

    pub fn get(
        &self,
        id: &vikram_contracts::TicketId,
    ) -> std::result::Result<Option<vikram_store::TicketRow>, String> {
        self.manager.get(id)
    }

    pub fn list(&self) -> std::result::Result<Vec<vikram_store::TicketRow>, String> {
        self.manager.list()
    }

    pub fn list_by_owner(
        &self,
        owner_key: &str,
    ) -> std::result::Result<Vec<vikram_store::TicketRow>, String> {
        self.manager.list_by_owner(owner_key)
    }

    pub fn list_by_kind(
        &self,
        kind: &str,
    ) -> std::result::Result<Vec<vikram_store::TicketRow>, String> {
        self.manager.list_by_kind(kind)
    }

    pub fn list_by_status(
        &self,
        status: &str,
    ) -> std::result::Result<Vec<vikram_store::TicketRow>, String> {
        self.manager.list_by_status(status)
    }

    pub fn children(
        &self,
        parent: &vikram_contracts::TicketId,
    ) -> std::result::Result<Vec<vikram_store::TicketRow>, String> {
        self.manager.children(parent)
    }

    pub fn search(&self, query: &str) -> std::result::Result<Vec<vikram_store::TicketRow>, String> {
        self.manager.search(query)
    }

    pub fn update(&self, row: &vikram_store::TicketRow) -> std::result::Result<(), String> {
        self.manager.update(row)
    }

    pub fn set_status(
        &self,
        id: &vikram_contracts::TicketId,
        status: &str,
        actor: &str,
        now_ms: u64,
    ) -> std::result::Result<(), String> {
        self.manager.set_status(id, status, actor, now_ms)
    }

    pub fn close(
        &self,
        id: &vikram_contracts::TicketId,
        actor: &str,
        now_ms: u64,
    ) -> std::result::Result<(), String> {
        self.manager.close(id, actor, now_ms)
    }

    pub fn reopen(
        &self,
        id: &vikram_contracts::TicketId,
        actor: &str,
        now_ms: u64,
    ) -> std::result::Result<(), String> {
        self.manager.reopen(id, actor, now_ms)
    }

    pub fn cancel(
        &self,
        id: &vikram_contracts::TicketId,
        actor: &str,
        now_ms: u64,
    ) -> std::result::Result<(), String> {
        self.manager.cancel(id, actor, now_ms)
    }

    pub fn set_result(
        &self,
        id: &vikram_contracts::TicketId,
        result: &str,
        actor: &str,
        now_ms: u64,
    ) -> std::result::Result<(), String> {
        self.manager.set_result(id, result, actor, now_ms)
    }

    pub fn set_failure(
        &self,
        id: &vikram_contracts::TicketId,
        reason: &str,
        actor: &str,
        now_ms: u64,
    ) -> std::result::Result<(), String> {
        self.manager.set_failure(id, reason, actor, now_ms)
    }

    pub fn delete(&self, id: &vikram_contracts::TicketId) -> std::result::Result<(), String> {
        self.manager.delete(id)
    }

    pub fn add_participant(
        &self,
        row: &vikram_store::TicketParticipantRow,
    ) -> std::result::Result<(), String> {
        self.manager.add_participant(row)
    }

    pub fn remove_participant(
        &self,
        ticket: &vikram_contracts::TicketId,
        key: &str,
        actor: &str,
        now_ms: u64,
    ) -> std::result::Result<(), String> {
        self.manager.remove_participant(ticket, key, actor, now_ms)
    }

    pub fn participants(
        &self,
        ticket: &vikram_contracts::TicketId,
    ) -> std::result::Result<Vec<vikram_store::TicketParticipantRow>, String> {
        self.manager.participants(ticket)
    }

    pub fn participant(
        &self,
        ticket: &vikram_contracts::TicketId,
        key: &str,
    ) -> std::result::Result<Option<vikram_store::TicketParticipantRow>, String> {
        self.manager.participant(ticket, key)
    }

    pub fn is_participant(&self, ticket: &vikram_contracts::TicketId, key: &str) -> bool {
        self.manager.is_participant(ticket, key)
    }

    pub fn can_read(&self, ticket: &vikram_contracts::TicketId, key: &str) -> bool {
        self.manager.can_read(ticket, key)
    }

    pub fn can_write(&self, ticket: &vikram_contracts::TicketId, key: &str) -> bool {
        self.manager.can_write(ticket, key)
    }

    pub fn can_admin(&self, ticket: &vikram_contracts::TicketId, key: &str) -> bool {
        self.manager.can_admin(ticket, key)
    }

    pub fn is_owner(&self, ticket: &vikram_store::TicketRow, key: &str) -> bool {
        self.manager.is_owner(ticket, key)
    }

    pub fn post_message(
        &self,
        ticket: &vikram_contracts::TicketId,
        author_key: &str,
        author_kind: &str,
        content: &str,
        visibility: &str,
        task_ref: Option<&str>,
        now_ms: u64,
    ) -> std::result::Result<vikram_store::TicketMessageRow, String> {
        self.manager.post_message(
            ticket,
            author_key,
            author_kind,
            content,
            visibility,
            task_ref,
            now_ms,
        )
    }

    pub fn messages_for(
        &self,
        ticket: &vikram_contracts::TicketId,
        viewer_key: &str,
    ) -> std::result::Result<Vec<vikram_store::TicketMessageRow>, String> {
        self.manager.messages_for(ticket, viewer_key)
    }

    pub fn create_assignment(
        &self,
        ticket: &vikram_contracts::TicketId,
        assignee_key: &str,
        assignee_kind: &str,
        entity_id: Option<vikram_contracts::EntityId>,
        title: &str,
        description: &str,
        strategy: Option<&str>,
        parent_assignment: Option<vikram_contracts::AssignmentId>,
        now_ms: u64,
    ) -> std::result::Result<vikram_store::AssignmentRow, String> {
        self.manager.create_assignment(
            ticket,
            assignee_key,
            assignee_kind,
            entity_id,
            title,
            description,
            strategy,
            parent_assignment,
            now_ms,
        )
    }

    pub fn get_assignment(
        &self,
        id: &vikram_contracts::AssignmentId,
    ) -> std::result::Result<Option<vikram_store::AssignmentRow>, String> {
        self.manager.get_assignment(id)
    }

    pub fn assignments(
        &self,
        ticket: &vikram_contracts::TicketId,
    ) -> std::result::Result<Vec<vikram_store::AssignmentRow>, String> {
        self.manager.assignments(ticket)
    }

    pub fn assignments_for(
        &self,
        assignee_key: &str,
    ) -> std::result::Result<Vec<vikram_store::AssignmentRow>, String> {
        self.manager.assignments_for(assignee_key)
    }

    pub fn update_assignment_status(
        &self,
        id: &vikram_contracts::AssignmentId,
        status: &str,
        actor: &str,
        now_ms: u64,
    ) -> std::result::Result<(), String> {
        self.manager
            .update_assignment_status(id, status, actor, now_ms)
    }

    pub fn complete_assignment(
        &self,
        id: &vikram_contracts::AssignmentId,
        result: &str,
        actor: &str,
        now_ms: u64,
    ) -> std::result::Result<(), String> {
        self.manager.complete_assignment(id, result, actor, now_ms)
    }

    pub fn fail_assignment(
        &self,
        id: &vikram_contracts::AssignmentId,
        error: &str,
        actor: &str,
        now_ms: u64,
    ) -> std::result::Result<(), String> {
        self.manager.fail_assignment(id, error, actor, now_ms)
    }

    pub fn cancel_assignment(
        &self,
        id: &vikram_contracts::AssignmentId,
        actor: &str,
        now_ms: u64,
    ) -> std::result::Result<(), String> {
        self.manager.cancel_assignment(id, actor, now_ms)
    }

    pub fn record_event(
        &self,
        ticket: &vikram_contracts::TicketId,
        kind: &str,
        actor_key: Option<&str>,
        payload: serde_json::Value,
        now_ms: u64,
    ) -> std::result::Result<vikram_store::TicketEventRow, String> {
        self.manager
            .record_event(ticket, kind, actor_key, payload, now_ms)
    }

    pub fn events(
        &self,
        ticket: &vikram_contracts::TicketId,
    ) -> std::result::Result<Vec<vikram_store::TicketEventRow>, String> {
        self.manager.events(ticket)
    }

    pub fn create_hive_ticket(
        &self,
        hive_id: &vikram_contracts::HiveId,
        owner_key: &str,
        title: &str,
        request: &str,
        strategy: &str,
        member_keys: &[(String, String)],
        now_ms: u64,
    ) -> std::result::Result<vikram_store::TicketRow, String> {
        self.manager.create_hive_ticket(
            hive_id,
            owner_key,
            title,
            request,
            strategy,
            member_keys,
            now_ms,
        )
    }

    pub fn create_bot_ticket(
        &self,
        bot_key: &str,
        title: &str,
        request: &str,
        now_ms: u64,
    ) -> std::result::Result<vikram_store::TicketRow, String> {
        self.manager
            .create_bot_ticket(bot_key, title, request, now_ms)
    }

    pub fn create_subagent_assignment(
        &self,
        ticket: &vikram_contracts::TicketId,
        assignee_key: &str,
        assignee_kind: &str,
        entity_id: Option<vikram_contracts::EntityId>,
        title: &str,
        description: &str,
        now_ms: u64,
    ) -> std::result::Result<vikram_store::AssignmentRow, String> {
        self.manager.create_subagent_assignment(
            ticket,
            assignee_key,
            assignee_kind,
            entity_id,
            title,
            description,
            now_ms,
        )
    }
}

/// Learning/evolution concerns.
///
/// * `surface` — `learning::LearningManager` (JSON): the approval queue,
///   policies and audit log. Approval surface only.
/// * `evolution` — `vikram_evolution::EvolutionEngine` (SQLite): the
///   single source of truth for durable learning data. Invoked on boot
///   (recovery + drain) and on every tool-bearing turn finish
///   (`learning::on_turn_end`).
///
/// `RuntimeInner::evolution()` / `::learning()` delegate here, so new
/// learning state should be added to this struct, not to `RuntimeInner`.
pub(crate) struct LearningState {
    pub surface: crate::LearningManager,
    pub evolution: std::sync::Mutex<vikram_evolution::EvolutionEngine>,
}

pub(crate) struct RuntimeInner {
    pub config: std::sync::RwLock<Config>,
    pub store: Store,
    pub memory: crate::MemoryStore,
    /// The memory backend `config.memory_provider` selected at boot. Lifecycle events fan out to
    /// its providers; a mutex because an external provider may register after boot.
    pub memory_providers: Mutex<crate::MemoryProviderManager>,
    pub context: Arc<crate::context::ContextLoader>,
    pub hive: HiveState,
    pub learning: LearningState,
    pub tickets: TicketState,
    /// Vikram-native dialectic user modelling: derives preferences/habits/goals from
    /// conversation turns (heuristic, local, deterministic) and supplies the supplement
    /// layer of the turn prefix.
    pub dialectic: crate::dialectic::DialecticEngine,
    pub bus: EventBus,
    pub tools: ToolRegistry,
    pub provider: std::sync::RwLock<Option<Arc<dyn Provider>>>,
    pub sessions: Mutex<Vec<Arc<SessionRuntime>>>,
    pub bots: BotManager,
    pub tasks: TaskManager,
    pub rosters: HiveRosterManager,
    // `tickets` field lives above (grouped with hive/learning extraction);
    // removed duplicate declaration here.
    pub mcp: std::sync::Arc<std::sync::Mutex<McpRegistry>>,
    pub mcp_visibility: std::sync::Arc<std::sync::Mutex<VisibilityManager>>,
    pub plugins: Mutex<PluginRegistry>,
    /// The hook chain built from the loaded plugins. Rebuilt after every plugin mutation, so
    /// disabling a plugin removes its hooks without restarting the runtime.
    pub hooks: std::sync::RwLock<crate::hooks::HookRegistry>,
    /// The turn pipeline's stage registry: named, ordered, replaceable steps the turn loop
    /// dispatches through. Every [`crate::pipeline::StagePoint`] has a builtin default; a
    /// `register_stage` call adds or overrides a native handler by priority.
    pub stages: std::sync::Arc<crate::pipeline::StageRegistry>,
    /// The runtime's executor handle, captured at boot. Lifecycle stage dispatch is synchronous
    /// and drives handler futures through it on a helper thread — see
    /// [`crate::pipeline::StageRegistry::run_blocking`].
    pub executor: tokio::runtime::Handle,
    pub skills: std::sync::Mutex<SkillRegistry>,
    // `evolution` field moved into `LearningState` (see `learning` above).
    pub workspace: WorkspaceManager,
    pub approvals: Mutex<HashMap<ApprovalId, PendingApproval>>,
    /// `ask_user` requests awaiting an operator answer, keyed by request id.
    pub questions: Mutex<HashMap<String, PendingQuestion>>,
    /// The posture in force right now: how the runtime treats a request the rules only prompted for.
    pub mode: Mutex<PermissionMode>,
    /// The model in force right now. Live because `/model` picks one: the provider object is built
    /// once from the endpoint and the key, while the name is a field of every request, so choosing
    /// a model is a decision the runtime can take at any moment without rebuilding anything.
    pub model: Mutex<String>,
    /// Which postures the layers above this runtime refuse to let it take.
    pub availability: ModeAvailability,
    /// Permissions a human granted once and asked to keep.
    pub rules: Mutex<RuleBook>,
    pub workspace_root: PathBuf,
    pub started_ms: u64,
    pub last_turn_ms: Mutex<Option<u64>>,
    pub last_tokens_per_sec: Mutex<Option<f64>>,
    /// Learning reviews this runtime has started and not finished. A one-shot process that exits
    /// without waiting for these drops a review it already paid for.
    pub review_tasks: Arc<AtomicU64>,
    /// Set once the periodic self-check task is spawned; a second
    /// [`RuntimeInner::spawn_self_check`] call is refused so the check can never double-schedule.
    pub self_check_scheduled: AtomicBool,
    /// Self-check ticks completed, for tests and diagnostics.
    pub self_check_ticks: AtomicU64,
    /// Monotonic sequence for `RuntimeEvent::Activity`; ids derive from it so a consumer can order
    /// the stream without relying on arrival order.
    pub activity_seq: AtomicU64,
}

impl RuntimeInner {
    /// The turn pipeline's stage registry.
    pub fn stages(&self) -> &Arc<crate::pipeline::StageRegistry> {
        &self.stages
    }

    /// Register a native stage handler. Higher `priority` wins dispatch for the point; the
    /// builtin default runs only while nothing is registered. Process plugins register stages
    /// through `components.stages` and are rebuilt by [`Self::reload_plugin_stages`].
    pub fn register_stage(
        &self,
        point: crate::pipeline::StagePoint,
        handler: Arc<dyn crate::pipeline::StageHandler>,
        priority: i32,
    ) {
        self.stages.register(point, handler, priority);
    }

    /// Rebuild the process-plugin stage registrations from the loaded plugin set and return the
    /// declarations that were refused. Called at boot and after every plugin mutation, so a
    /// disabled plugin's stage handler stops dispatching on the next turn.
    pub(crate) fn reload_plugin_stages(&self) -> Vec<crate::pipeline::StageRejection> {
        let plugins = self.plugins.lock().unwrap_or_else(|e| {
            eprintln!("vikram: plugins lock mutex poisoned; recovering");
            e.into_inner()
        });
        self.stages.reload_plugin_stages(&plugins)
    }

    /// Dispatch one hook context through the chain. Blocking work (a process hook) happens on the
    /// caller's thread, which is why call sites use `spawn_blocking`.
    pub fn dispatch_hook(&self, ctx: &crate::hooks::HookContext) -> crate::hooks::HookOutcome {
        self.hooks
            .read()
            .unwrap_or_else(|e| {
                eprintln!("vikram: hooks lock mutex poisoned; recovering");
                e.into_inner()
            })
            .dispatch(ctx)
    }

    /// Journal what a dispatched hook outcome did, the same record the turn loop's `run_hook`
    /// writes. Callers that already ran [`Self::dispatch_hook`] use this; synchronous lifecycle
    /// sites use [`Self::dispatch_hook_journaled`].
    pub(crate) fn journal_hook(
        &self,
        session: &SessionId,
        point: &str,
        outcome: &crate::hooks::HookOutcome,
    ) {
        if outcome.applied.is_empty() {
            return;
        }
        self.journal(
            session,
            RecordKind::Hook,
            serde_json::json!({
                "point": point,
                "applied": outcome
                    .applied
                    .iter()
                    .map(|entry| serde_json::json!({
                        "plugin": entry.plugin,
                        "effect": entry.effect,
                    }))
                    .collect::<Vec<_>>(),
            }),
        );
    }

    /// Dispatch a hook from synchronous runtime code — session lifecycle and delegation run on
    /// paths with no async boundary to move a process hook onto, so the caller's thread blocks
    /// for at most `HOOK_TIMEOUT` per hook, the bound every dispatch already has.
    pub(crate) fn dispatch_hook_journaled(
        &self,
        session: &SessionId,
        ctx: &crate::hooks::HookContext,
    ) -> crate::hooks::HookOutcome {
        let point = ctx.point.as_str().to_owned();
        let outcome = self.dispatch_hook(ctx);
        self.journal_hook(session, &point, &outcome);
        outcome
    }
}

/// Run one lifecycle stage from a synchronous store path and apply the v1 veto policy.
///
/// Lifecycle stages observe store operations that already have honest failure semantics, so a
/// handler error is published as an event and never fails the operation. [`StageOutcome::Refused`]
/// vetoes only where `vetoable` — today that is `session_lifecycle`/`create` alone; everywhere
/// else a refusal is surfaced as a warning and the operation proceeds unchanged. `Err(reason)`
/// is returned only for an honored veto.
fn dispatch_lifecycle_stage(
    inner: &Arc<RuntimeInner>,
    point: crate::pipeline::StagePoint,
    session: &Arc<SessionRuntime>,
    request_text: &str,
    payload: crate::pipeline::StagePayload,
    vetoable: bool,
) -> std::result::Result<(), String> {
    match crate::pipeline::run_lifecycle_stage(inner, point, session, request_text, payload) {
        Ok(crate::pipeline::StageOutcome::Done) => Ok(()),
        Ok(crate::pipeline::StageOutcome::Refused(reason)) if vetoable => Err(reason),
        Ok(crate::pipeline::StageOutcome::Refused(reason)) => {
            inner.emit(RuntimeEvent::Notice {
                level: NoticeLevel::Warn,
                text: format!(
                    "the `{}` stage refused, but this operation only observes: {reason}",
                    point.as_str()
                ),
            });
            Ok(())
        }
        Err(err) => {
            inner.emit(RuntimeEvent::Error {
                text: err.to_string(),
            });
            Ok(())
        }
    }
}

/// Keeps that count honest even if the review task panics.
struct LearningGuard(Arc<AtomicU64>);

impl Drop for LearningGuard {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::SeqCst);
    }
}

/// A source of the message count a memory nudge is keyed on. Keeps `memory_nudge_due` usable
/// with a session handle, an `Arc` to one, or a session id that has to be looked up.
pub(crate) trait MemoryNudgeSession {
    fn message_count(&self, inner: &RuntimeInner) -> usize;
}

impl MemoryNudgeSession for crate::session::SessionRuntime {
    fn message_count(&self, _inner: &RuntimeInner) -> usize {
        self.messages().len()
    }
}

impl MemoryNudgeSession for std::sync::Arc<crate::session::SessionRuntime> {
    fn message_count(&self, _inner: &RuntimeInner) -> usize {
        self.messages().len()
    }
}

impl MemoryNudgeSession for vikram_contracts::SessionId {
    fn message_count(&self, inner: &RuntimeInner) -> usize {
        inner.session(self).map(|s| s.messages().len()).unwrap_or(0)
    }
}

impl RuntimeInner {
    pub fn emit(&self, event: RuntimeEvent) {
        self.bus.publish(event);
    }

    /// Publish a structured activity against the session it belongs to. The event carries no
    /// position in the transcript — the interface files it under that session's activity rows.
    #[allow(clippy::too_many_arguments)]
    pub fn emit_activity(
        &self,
        session: &SessionId,
        kind: crate::types::ActivityKind,
        summary: impl Into<String>,
        bot: Option<BotId>,
        parent_agent: Option<BotId>,
        ok: bool,
    ) {
        let seq = self.activity_seq.fetch_add(1, Ordering::SeqCst) + 1;
        self.emit(RuntimeEvent::Activity(crate::types::ActivityEvent {
            id: format!("activity-{seq}"),
            kind,
            bot,
            parent_agent,
            session: session.clone(),
            turn: None,
            call: None,
            summary: summary.into(),
            resource: None,
            content: None,
            detail: None,
            exit_code: None,
            ok,
            duration_ms: None,
            metadata: None,
            seq,
        }));
    }

    pub fn session(&self, id: &SessionId) -> Option<Arc<SessionRuntime>> {
        self.sessions
            .lock()
            .expect("sessions lock")
            .iter()
            .find(|s| s.id == *id)
            .cloned()
    }

    pub fn session_list(&self) -> Vec<Arc<SessionRuntime>> {
        self.sessions
            .lock()
            .unwrap_or_else(|e| {
                eprintln!("vikram: runtime sessions lock mutex poisoned; recovering");
                e.into_inner()
            })
            .clone()
    }

    pub fn memory_providers(&self) -> std::sync::MutexGuard<'_, crate::MemoryProviderManager> {
        self.memory_providers.lock().unwrap_or_else(|e| {
            eprintln!("vikram: memory providers lock mutex poisoned; recovering");
            e.into_inner()
        })
    }

    pub fn publish_session(&self, session: &SessionRuntime) {
        self.emit(RuntimeEvent::SessionUpdated(session.snapshot()));
    }

    pub fn publish_bot(&self, bot: &ManagedBot) {
        self.emit(RuntimeEvent::BotChanged(bot.snapshot()));
    }

    pub fn touch(&self, session: &SessionRuntime) {
        let _ = self
            .store
            .touch_session(&session.id, session.updated_ms.load(Ordering::Relaxed));
    }

    /// Whether this session has crossed a memory-nudge boundary: its message count is a positive
    /// multiple of the configured `memory_nudge_interval`. A zero interval never nudges.
    pub fn memory_nudge_due<S: MemoryNudgeSession>(&self, session: &S) -> bool {
        let interval = self
            .config
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .memory_nudge_interval;
        if interval == 0 {
            return false;
        }
        let count = session.message_count(self) as u64;
        count > 0 && count.is_multiple_of(interval)
    }

    /// The evaluator for this runtime: the operator's saved rules first, then the built-in
    /// conventions, under a floor neither tier can widen.
    pub fn policy(&self) -> Policy {
        let root = self.workspace_root.to_string_lossy().into_owned();
        let floor = MandatoryFloor {
            workspace_root: Some(root.clone()),
            // VIKRAM's own session workspaces under its home are host-declared writable so a
            // session can own its generated files. Fixed here, never from rules or model content.
            extra_write_roots: vec![
                crate::config::vikram_home()
                    .join("sessions")
                    .to_string_lossy()
                    .into_owned(),
            ],
            ..MandatoryFloor::default()
        };
        let defaults = vec![
            Rule::allow(ActionKind::Read, ResourcePattern::Any),
            Rule::allow(ActionKind::Write, ResourcePattern::Subtree(root)),
            Rule::ask(
                ActionKind::Execute,
                ResourcePattern::Any,
                "commands run with your authority",
            ),
            Rule::ask(
                ActionKind::Network,
                ResourcePattern::Any,
                "network access leaves this machine",
            ),
            Rule::allow(ActionKind::Delegate, ResourcePattern::Any),
        ];
        let saved = self
            .rules
            .lock()
            .unwrap_or_else(|e| {
                eprintln!("vikram: runtime rules lock mutex poisoned; recovering");
                e.into_inner()
            })
            .snapshot();
        Policy::with_defaults(saved, defaults, floor).with_tool_permissions({
            let mut merged = std::collections::HashMap::new();
            if let Ok(cfg) = self.config.read() {
                for (k, v) in &cfg.tool_permissions {
                    merged.insert(k.clone(), v.clone());
                }
                for (k, v) in &cfg.security.tool_permissions {
                    merged.insert(k.clone(), v.clone());
                }
            }
            merged
        })
    }

    pub fn mode(&self) -> PermissionMode {
        *self.mode.lock().unwrap_or_else(|e| {
            eprintln!("vikram: runtime mode lock mutex poisoned; recovering");
            e.into_inner()
        })
    }

    /// The model name every request is built with. Live, because the interface can pick one at any
    /// moment; the provider object owns the endpoint and the key, not the choice.
    pub fn model(&self) -> String {
        self.model
            .lock()
            .unwrap_or_else(|e| {
                eprintln!("vikram: runtime model lock mutex poisoned; recovering");
                e.into_inner()
            })
            .clone()
    }

    /// The whole decision for one request, in one place: the evaluator answers what the rules say,
    /// then the posture in force decides what to do with a prompt. Both entry points — the turn
    /// loop and the kernel dispatch — go through this, so a view cannot invent its own gate.
    pub fn judge(&self, request: &PermissionRequest) -> Decision {
        self.judge_at_depth(request, 0)
    }

    /// The same decision, evaluated at a delegation depth. The mandatory floor's
    /// `max_delegation_depth` veto only exists in the depth-aware path, so a session that is
    /// already deep must be judged here — `judge` alone would let it keep delegating.
    pub fn judge_at_depth(&self, request: &PermissionRequest, depth: u32) -> Decision {
        let policy = self.policy();
        let raw = policy.evaluate_at_depth(request, depth);
        let listed = policy.has_allow(request);
        let root = self.workspace_root.to_string_lossy().into_owned();
        self.mode().resolve(
            raw,
            request,
            Posture {
                workspace_root: Some(&root),
                listed,
            },
        )
    }

    /// Move to a posture, refused by the layers above this runtime if it is not available.
    pub fn set_mode(&self, mode: PermissionMode) -> Result<()> {
        if let Some((rule, layer)) = self.availability.refusal(mode) {
            return Err(RuntimeError::Denied(format!(
                "mode `{mode}` is refused by {layer}: `{rule}`"
            )));
        }
        *self.mode.lock().unwrap_or_else(|e| {
            eprintln!("vikram: runtime mode lock mutex poisoned; recovering");
            e.into_inner()
        }) = mode;
        Ok(())
    }

    /// Register an `ask_user` request and hand back the receiver the turn loop blocks on.
    /// Registering before the event is emitted means an answer can never arrive early and be lost.
    pub fn open_question(&self, request_id: &str) -> oneshot::Receiver<String> {
        let (tx, rx) = oneshot::channel();
        self.questions
            .lock()
            .expect("questions lock")
            .insert(request_id.to_owned(), tx);
        rx
    }

    /// Drop a question that was answered elsewhere or abandoned by a cancelled turn.
    pub fn close_question(&self, request_id: &str) {
        self.questions
            .lock()
            .expect("questions lock")
            .remove(request_id);
    }

    pub fn journal(&self, session: &SessionId, kind: RecordKind, payload: serde_json::Value) {
        let record = vikram_contracts::NewRecord::new(session.clone(), kind, payload).at(now_ms());
        if let Err(err) = self.store.journal().append(&record) {
            self.emit(RuntimeEvent::Error {
                text: format!("journal append failed: {err}"),
            });
        }
    }

    pub fn metrics(&self) -> Metrics {
        let sessions = self.session_list();
        // One session is the one the operator is looking at; the meter reports the largest live
        // transcript rather than an average of everything that exists.
        let status = self.context_status();
        let context_used = status.as_ref().map(|s| s.used).unwrap_or(0);
        let context_window = status.as_ref().map(|s| s.window);
        let context_measured = status.as_ref().is_some_and(|s| s.measured);
        let limit_assumed = status
            .as_ref()
            .is_some_and(|s| s.limits_source == crate::context_engine::LimitsSource::Assumed);
        let compactions: u64 = sessions.iter().map(|s| s.folds()).sum();
        let context_window = context_window.or(self
            .config
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .context_window);
        let turn_elapsed_ms = sessions
            .iter()
            .filter_map(|s| s.turn_elapsed_ms(now_ms()))
            .max()
            .or(*self.last_turn_ms.lock().unwrap_or_else(|e| {
                eprintln!("vikram: runtime turn metric mutex poisoned; recovering");
                e.into_inner()
            }));
        let mcp = self.mcp.lock().unwrap_or_else(|e| {
            eprintln!("vikram: runtime mcp lock mutex poisoned; recovering");
            e.into_inner()
        });
        let plugins = self.plugins.lock().unwrap_or_else(|e| {
            eprintln!("vikram: runtime plugins lock mutex poisoned; recovering");
            e.into_inner()
        });
        // One guard, one acquisition: `skills_discovered` and `skills_loaded` both read this
        // registry, and locking it twice inside the struct literal deadlocks because the first
        // temporary guard is not dropped until the whole expression ends.
        let (skills_discovered, skills_loaded) = {
            let skills = self.skills.lock().unwrap_or_else(|e| e.into_inner());
            (
                skills.count(),
                skills.skills().iter().filter(|s| s.loaded).count(),
            )
        };
        Metrics {
            model: {
                let live = self.model();
                if live.is_empty() {
                    sessions.iter().find_map(|s| s.model())
                } else {
                    Some(live)
                }
            },
            provider: self
                .provider
                .read()
                .expect("provider lock")
                .as_ref()
                .map(|p| p.name().to_owned()),
            context_used,
            context_window,
            context_measured,
            context_limit_assumed: limit_assumed,
            context_compactions: compactions,
            context_usable: status.as_ref().map(|s| s.usable).unwrap_or(0),
            turn_elapsed_ms,
            elapsed_ms: now_ms().saturating_sub(self.started_ms),
            tokens_per_sec: *self.last_tokens_per_sec.lock().unwrap_or_else(|e| {
                eprintln!("vikram: runtime tps metric mutex poisoned; recovering");
                e.into_inner()
            }),
            background_tasks: self.tasks.running_count(),
            mcp_connected: mcp.connected_count(),
            mcp_configured: mcp.configured_count(),
            plugins_loaded: plugins.loaded(),
            plugins_installed: plugins.installed(),
            bots_active: self.bots.active_count(),
            skills_discovered,
            skills_loaded,
            thinking: self
                .config
                .read()
                .unwrap_or_else(|e| e.into_inner())
                .thinking,
        }
    }

    pub fn publish_metrics(&self) {
        self.emit(RuntimeEvent::Metrics(self.metrics()));
    }

    /// Publish a live figure while a turn is still streaming.
    ///
    /// `finish` writes the authoritative rate when the turn ends; without this the meter shows the
    /// previous turn's rate until then, which reads as a stale number rather than a live one.
    pub fn note_stream_progress(&self, elapsed: std::time::Duration, output_tokens: u64) {
        let rate = if output_tokens > 0 && !elapsed.is_zero() {
            Some(output_tokens as f64 / elapsed.as_secs_f64())
        } else {
            None
        };
        *self
            .last_tokens_per_sec
            .lock()
            .unwrap_or_else(|e| e.into_inner()) = rate;
        self.publish_metrics();
    }

    /// The fixed cost of a request: tool schemas plus the system messages built for this turn.
    /// Measured with the same estimator as the transcript, or the two numbers cannot be added.
    fn request_overhead(
        &self,
        estimator: &crate::context_engine::TokenEstimator,
        prefix: &[Message],
    ) -> u64 {
        self.tool_schema_tokens(estimator) + estimator.estimate_messages(prefix)
    }

    /// What the tool declarations cost in every request, on their own. Surfaced separately because
    /// it is the one part of the overhead an operator can actually shrink, by disabling a server.
    pub fn tool_schema_tokens(&self, estimator: &crate::context_engine::TokenEstimator) -> u64 {
        let schemas = serde_json::to_string(&self.tools.specs()).unwrap_or_default();
        estimator.estimate_text(&schemas)
    }

    /// Whether a provider failure is the recoverable "you sent me too much" kind.
    pub(crate) fn context_limit_error(&self, text: &str) -> bool {
        let lower = text.to_lowercase();
        [
            "context_length_exceeded",
            "context length",
            "maximum context",
            "context window",
            "too many tokens",
            "prompt is too long",
            "input is too long",
            "reduce the length",
            "exceeds the model",
            "exceeds available",
        ]
        .iter()
        .any(|marker| lower.contains(marker))
    }

    /// The dialectic supplement (layer 2) for this turn's prefix, or nothing at all.
    /// Layer 1 is [`Self::retrieved_memory`] (base context); layer 2 is the user model
    /// derived heuristically from earlier turns. Both are host-assembled, budgeted, and
    /// labelled so the context meter can price them separately.
    pub fn dialectic_supplement(&self) -> Option<String> {
        self.dialectic.supplement()
    }

    /// Observe the just-finished user turn for the dialectic user model and persist any new
    /// insights. Runs on the configured cadence; local and deterministic, so it costs no
    /// model tokens.
    pub fn observe_dialectic_turn(&self, user_text: &str) {
        if self.dialectic.observe_turn(user_text).is_empty() {
            return;
        }
        let _ = self.dialectic.persist_to(&self.memory);
    }

    /// Relevant long-term memory for this turn, inside the configured budget, or nothing at all.
    ///
    /// Terms come from the most recent instruction rather than the whole transcript, because the
    /// thing worth remembering is what the agent is being asked about now.
    pub fn retrieved_memory(&self, session: &Arc<SessionRuntime>) -> Option<String> {
        if !self
            .config
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .context
            .memory_retrieval
        {
            return None;
        }
        let estimator = session.estimator();
        let budget_tokens = self
            .config
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .context
            .memory_budget_tokens;
        let mut block = String::new();
        let mut spent = 0u64;
        let transcript = session.messages();
        let query = transcript
            .iter()
            .rev()
            .find(|m| m.role == vikram_contracts::Role::User)
            .map(|m| m.text_body())
            .unwrap_or_default();
        if !query.trim().is_empty() {
            let terms = Self::retrieval_terms(&query);
            // A conversation may draw on shared `global` notes and its own; nothing else in the
            // shared table. Entity/user-scoped recall waits until the host assigns those scopes.
            let scopes = vec!["global".to_owned(), session.id.as_str().to_owned()];
            // Hybrid recall: BM25 (FTS5) + token overlap + recency/importance, MMR-reranked.
            // No local query embedding is available to the runtime (no cloud API is used), so
            // the cosine signal is skipped unless a future embedder supplies one.
            let opts = crate::HybridOptions::default();
            if let Ok(scored) = self.memory.hybrid_recall(&terms, &scopes, 8, &opts) {
                let mut entries: Vec<(String, String, String)> = scored
                    .into_iter()
                    .map(|s| (s.scope, s.key, s.value))
                    .collect();
                // Also pull free-form notes from MEMORY.md / USER.md (Hermes-style).
                let home = crate::config::vikram_home();
                let mf = crate::memory_files::memory_file(&home);
                let uf = crate::memory_files::user_file(&home);
                for (scope, e) in [("memory", mf), ("user", uf)] {
                    for entry in e.load() {
                        let lower = entry.to_lowercase();
                        if terms.iter().any(|t| lower.contains(&t.to_lowercase())) {
                            entries.push((scope.to_owned(), scope.to_owned(), entry));
                        }
                    }
                }
                // Reflection auto-apply writes correction-derived facts to the evolution
                // engine's layered memory under `agent:<id>` — a second store nothing else
                // reads. Same budget and same section; a value already injected above is
                // not repeated (auto-apply can land the same correction in both stores).
                let agent_id = session
                    .bot()
                    .map(|id| id.to_string())
                    .unwrap_or_else(|| "main".to_owned());
                let engine = self.learning.evolution.lock().unwrap_or_else(|e| {
                    eprintln!("vikram: runtime evolution lock mutex poisoned; recovering");
                    e.into_inner()
                });
                // An empty query recalls the scope's most recent live entries: correction
                // memories are few and agent-scoped, so recency is the relevance signal.
                if let Ok(hits) = engine
                    .memory
                    .recall(Some(&format!("agent:{agent_id}")), "", 4)
                {
                    let mut known: std::collections::HashSet<String> = entries
                        .iter()
                        .map(|(_, _, value)| value.trim().to_owned())
                        .collect();
                    for hit in hits {
                        let value = hit.value.trim();
                        if value.is_empty() || !known.insert(value.to_owned()) {
                            continue;
                        }
                        entries.push(("evolution".to_owned(), hit.key.clone(), hit.value.clone()));
                    }
                }
                drop(engine);
                if !entries.is_empty() {
                    let mut memory_block = String::from(
                        "Relevant memory (durable notes from earlier sessions; may be stale, verify before relying on it):\n",
                    );
                    let mut memory_spent = estimator.estimate_text(&memory_block);
                    let mut used = 0usize;
                    for (scope, key, value) in entries {
                        let line = format!("- [{scope}] {key}: {value}\n");
                        let cost = estimator.estimate_text(&line);
                        if memory_spent + cost > budget_tokens {
                            break;
                        }
                        memory_spent += cost;
                        used += 1;
                        memory_block.push_str(&line);
                    }
                    if used > 0 {
                        block.push_str(&memory_block);
                        spent = memory_spent;
                    }
                }
            }
        }
        // Experiences are write-mostly without this: the last few outcomes for this agent,
        // budgeted against whatever the memory lines left.
        let experience_lines = self.recent_experiences(session);
        if !experience_lines.is_empty() {
            let section = format!(
                "Recent experience (what earlier turns for this agent did; outcomes, not guarantees):\n{}\n",
                experience_lines.join("\n")
            );
            let cost = estimator.estimate_text(&section);
            if spent + cost <= budget_tokens {
                block.push_str(&section);
            }
        }
        if block.is_empty() { None } else { Some(block) }
    }

    /// The last few experiences recorded for this session's agent, as compact outcome lines,
    /// or nothing when the evolution store has none to offer.
    fn recent_experiences(&self, session: &Arc<SessionRuntime>) -> Vec<String> {
        let agent_id = session
            .bot()
            .map(|id| id.to_string())
            .unwrap_or_else(|| "main".to_owned());
        let engine = self.learning.evolution.lock().unwrap_or_else(|e| {
            eprintln!("vikram: runtime evolution lock mutex poisoned; recovering");
            e.into_inner()
        });
        let Ok(experiences) = engine.experience.list(Some(&agent_id), 3) else {
            return Vec::new();
        };
        drop(engine);
        experiences
            .iter()
            .map(|exp| {
                let request: String = exp.user_request.chars().take(120).collect();
                let mut line = format!(
                    "- {}: {request} -> {}",
                    exp.task_category,
                    exp.outcome.as_str()
                );
                if let Some(error) = exp.errors.first() {
                    let error: String = error.chars().take(120).collect();
                    line.push_str(&format!(" ({error})"));
                }
                line
            })
            .collect()
    }

    /// The instructions this conversation runs under: the active entity's own files when there is
    /// one, and always the workspace's `AGENTS.md`. Bounded by `context.project_budget_tokens`, so
    /// a large set of Markdown files cannot quietly eat the window a transcript needs.
    pub fn project_context(&self) -> Option<String> {
        let identity = self.store.get_active_identity().unwrap_or_default();
        let bundle = match identity
            .main_agent
            .as_ref()
            .and_then(|id| self.store.get_entity(id).ok().flatten())
        {
            Some(entity) => self.context.load_agent_or_bot(&entity),
            None => self.context.load_project(),
        };
        let text = bundle.render();
        if text.trim().is_empty() {
            None
        } else {
            Some(text)
        }
    }

    /// Content words worth matching on, longest first, capped so one broad instruction cannot
    /// pull in the entire memory table.
    fn retrieval_terms(query: &str) -> Vec<String> {
        const STOP: &[&str] = &[
            "the", "and", "with", "that", "this", "from", "have", "has", "are", "was", "were",
            "will", "would", "could", "should", "about", "into", "your", "you", "for", "not",
            "but", "can", "did", "does", "just", "please", "make", "them", "they", "their",
            "there", "then", "than", "here", "what", "when", "where", "which", "while", "all",
            "any", "some", "more", "most", "other", "each", "per", "use", "using", "used",
        ];
        let mut terms: Vec<String> = query
            .to_lowercase()
            .split(|c: char| !c.is_alphanumeric() && c != '_' && c != '-')
            .filter(|word| word.len() >= 4 && !STOP.contains(word))
            .map(str::to_owned)
            .collect();
        terms.sort_by_key(|word| std::cmp::Reverse(word.len()));
        terms.dedup();
        terms.truncate(8);
        terms
    }

    /// This session's budget, resolved from the model it is actually talking to.
    pub fn context_budget(
        &self,
        session: &SessionRuntime,
        prefix: &[Message],
    ) -> crate::context_engine::ContextBudget {
        let estimator = session.estimator();
        let advertised = self
            .provider
            .read()
            .expect("provider lock")
            .as_ref()
            .and_then(|p| p.context_window());
        let limits = crate::context_engine::ModelLimits::resolve_with_margin(
            &self.model(),
            self.config
                .read()
                .unwrap_or_else(|e| e.into_inner())
                .context_window,
            advertised,
            self.config
                .read()
                .unwrap_or_else(|e| e.into_inner())
                .context
                .safety_margin_percent,
        );
        let budget = crate::context_engine::ContextBudget::new(
            limits,
            self.request_overhead(&estimator, prefix),
            self.config
                .read()
                .unwrap_or_else(|e| e.into_inner())
                .max_output_tokens
                .map(u64::from),
        );
        match session.anchor() {
            Some(anchor) => budget.with_anchor(anchor),
            None => budget,
        }
    }

    /// Enforce the budget before a request is built, and return the transcript that may be sent.
    ///
    /// Two moves, in this order: fold the older conversation into a summary, then digest oversized
    /// tool results in the tail if folding alone left us over. Both are journaled and both are
    /// announced, because a context that quietly changed shape is a context nobody can reason about.
    pub fn prepare_context(
        &self,
        session: &Arc<SessionRuntime>,
        prefix: &[Message],
    ) -> (Vec<Message>, crate::context_engine::ContextBudget) {
        use crate::compaction::{CompactionPolicy, prune_transcript, should_compact};

        let estimator = session.estimator();
        let budget = self.context_budget(session, prefix);
        let policy = CompactionPolicy::from_config(
            &self
                .config
                .read()
                .unwrap_or_else(|e| e.into_inner())
                .context,
        );
        let mut transcript = session.messages();
        let usable = budget.usable_input();

        if self
            .config
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .context
            .auto_compact
            && should_compact(&estimator, &transcript, &budget, &policy)
            && let Some((folded, _)) =
                self.fold_once(&session.id, &transcript, &estimator, &budget, &policy)
        {
            transcript = folded;
        }

        if transcript.len() > policy.keep_recent
            && let Some((pruned, replaced)) =
                prune_transcript(&estimator, &budget, &transcript, policy.tool_output_ceiling)
        {
            let saved = estimator
                .estimate_messages(&transcript)
                .saturating_sub(estimator.estimate_messages(&pruned));
            session.replace_messages(pruned.clone(), now_ms());
            self.emit(RuntimeEvent::ContextPruned {
                session: session.id.clone(),
                replaced,
                saved_tokens: saved,
            });
            transcript = pruned;
        }

        if !self
            .config
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .context
            .auto_compact
            && usable > 0
        {
            let used = crate::compaction::active_context_tokens(&estimator, &transcript, &budget);
            if used > usable {
                // Compaction is off, so the only honest move left is to say so rather than send a
                // request the provider will reject.
                self.emit(RuntimeEvent::Error {
                    text: format!(
                        "context is {used} tokens over a {usable}-token budget and auto-compaction is disabled"
                    ),
                });
            }
        }

        (transcript, budget)
    }

    /// `/compact`: fold now, whether or not the meter has reached the trigger.
    pub fn compact_now(&self, session: &SessionId) -> Option<crate::compaction::CompactionReport> {
        let live = self.session(session)?;
        let estimator = live.estimator();
        let budget = self.context_budget(&live, &[]);
        let policy = crate::compaction::CompactionPolicy::from_config(
            &self
                .config
                .read()
                .unwrap_or_else(|e| e.into_inner())
                .context,
        );
        let transcript = live.messages();
        self.fold_once(session, &transcript, &estimator, &budget, &policy)
            .map(|(_, report)| report)
    }

    /// Fold the transcript once, journal the marker, publish the result, and hand back what to send.
    ///
    /// `None` means nothing was folded: the span was too short, the relief too small, a cut would
    /// have split a tool call from its result, or this exact fold has already happened. The same
    /// routine serves the automatic trigger and `/compact`, so a manual fold is journalled,
    /// announced and guarded exactly like an automatic one.
    fn fold_once(
        &self,
        session: &SessionId,
        transcript: &[Message],
        estimator: &crate::context_engine::TokenEstimator,
        budget: &crate::context_engine::ContextBudget,
        policy: &crate::compaction::CompactionPolicy,
    ) -> Option<(Vec<Message>, crate::compaction::CompactionReport)> {
        let through_seq = self
            .store
            .journal()
            .last_seq(session)
            .ok()
            .flatten()
            .unwrap_or(0);
        let length_before = transcript.len();
        let (folded, report) =
            crate::compaction::compact(estimator, transcript, budget, policy, through_seq)?;
        let live = self.session(session)?;
        if !live.note_compaction(length_before) {
            return None;
        }
        let summary = folded[0].clone();
        self.journal_compaction(session, &summary, &report);
        live.replace_messages(folded.clone(), now_ms());
        self.emit(RuntimeEvent::ContextCompacted {
            session: session.clone(),
            folded: report.folded,
            kept: report.kept,
            before_tokens: report.before_tokens,
            after_tokens: report.after_tokens,
            through_seq: report.through_seq,
        });
        Some((folded, report))
    }

    /// Append the compaction marker. The summary is stored with the number of messages it folded,
    /// which is the forget range a replay needs to rebuild this exact transcript.
    fn journal_compaction(
        &self,
        session: &SessionId,
        summary: &Message,
        report: &crate::compaction::CompactionReport,
    ) {
        let payload = serde_json::json!({
            crate::compaction::COMPACTION_SUMMARY_KEY: summary,
            crate::compaction::COMPACTION_THROUGH_KEY: report.through_seq,
            crate::compaction::COMPACTION_FOLDED_KEY: report.folded,
            "before_tokens": report.before_tokens,
            "after_tokens": report.after_tokens,
        });
        self.journal(session, RecordKind::Compaction, payload);
    }

    /// The live context figure for the interface: the busiest session, with the provenance of both
    /// the number and the limit attached.
    ///
    /// `used` comes from the session's calibrated estimator plus, when the provider has
    /// reported usage, the measured anchor — never a `len()/4` heuristic. When neither a
    /// provider measurement nor a calibrated estimator is available the estimate is still
    /// produced by `context_engine` (character-based, documented there) rather than a
    /// byte-count shortcut here.
    pub fn context_status(&self) -> Option<crate::context_engine::ContextStatus> {
        let session = self
            .session_list()
            .into_iter()
            .max_by_key(|s| s.messages().len())?;
        let budget = self.context_budget(&session, &[]);
        let estimator = session.estimator();
        let transcript = session.messages();
        let used = crate::compaction::active_context_tokens(&estimator, &transcript, &budget);
        let measured = session.anchor().is_some();
        Some(crate::context_engine::ContextStatus::new(
            &budget, used, measured,
        ))
    }

    pub fn tool_context(&self) -> ToolContext {
        ToolContext::new(
            self.workspace_root.clone(),
            &self
                .config
                .read()
                .unwrap_or_else(|e| e.into_inner())
                .security
                .sandbox,
        )
    }

    /// A context that can reach this session's spool, so `output.read` resolves the handles the
    /// digests in this transcript advertise and nothing else.
    pub fn tool_context_for(&self, session: &SessionId) -> ToolContext {
        let mut context = self.tool_context();
        context.spill = crate::spill::Spill::for_session(self.workspace.home(), session);
        context.session = Some(session.clone());
        // Which bot is speaking, if any. A bot's notes belong to that bot, and to it alone.
        context.bot = self
            .session(session)
            .and_then(|live| live.bot())
            .map(|bot| bot.to_string());
        context
    }

    /// Deliver a delegated agent's bounded answer to the session that asked for it.
    ///
    /// The child's transcript stays its own; what the parent receives is the final statement, fitted
    /// to `context.delegation_budget_tokens` and journaled so it survives a restart of either session.
    /// `self: &Arc<Self>` because the `agent_lifecycle` stage's context holds the runtime as
    /// an `Arc`; the turn loop already calls through one.
    pub fn deliver_delegation(self: &Arc<Self>, parent: &SessionId, child: &SessionRuntime) {
        // The child's live `Arc` is captured before the reap below can drop it from the live
        // set; the `agent_lifecycle` stage observes through it at the end.
        let child_live = self.session(&child.id);
        let mut delivered = false;
        if let Some(parent_session) = self.session(parent) {
            let budget = self
                .config
                .read()
                .unwrap_or_else(|e| e.into_inner())
                .context
                .delegation_budget_tokens;
            if let Some(answer) =
                crate::compaction::delegation_answer(&child.estimator(), &child.messages(), budget)
            {
                let text = format!(
                    "[VIKRAM delegation result] {} said:\n{answer}",
                    child.title()
                );
                let message = vikram_contracts::Message::text(
                    vikram_contracts::Role::System,
                    text,
                    vikram_contracts::Trust::Host,
                );
                parent_session.push_message(message.clone(), now_ms());
                self.journal(
                    parent,
                    RecordKind::Message,
                    serde_json::json!({"message": message}),
                );
                self.emit(RuntimeEvent::MessageCommitted {
                    session: parent.clone(),
                    message,
                });
                self.emit(RuntimeEvent::Notice {
                    level: NoticeLevel::Info,
                    text: format!("{} answered", child.title()),
                });
                self.emit_activity(
                    parent,
                    crate::types::ActivityKind::Delegated,
                    format!("{} answered", child.title()),
                    child.bot(),
                    parent_session.bot(),
                    true,
                );
                self.publish_metrics();
                delivered = true;
            }
        }
        // A delegated session is one-shot: its answer has been handed over (or there was no
        // parent left to hand it to), so it is closed rather than left live forever — startup
        // only rehydrates `active` rows, and `closed` keeps the transcript browsable. Only a
        // real delegation (`depth > 0`) is reaped: a fork also carries `parent_session` but is
        // an ordinary conversation the operator keeps. A queued follow-up keeps it alive.
        if child.depth() > 0 && child.queue_len() == 0 {
            let _ = self.store.set_session_status(&child.id, "closed", now_ms());
            self.sessions
                .lock()
                .expect("sessions lock")
                .retain(|s| s.id != child.id);
            self.emit(RuntimeEvent::SessionClosed(child.id.clone()));
            self.dispatch_hook_journaled(
                &child.id,
                &crate::hooks::HookContext::new(crate::hooks::HookPoint::SessionEnd)
                    .with_session(child.id.as_str())
                    .with_workspace(self.workspace_root.display().to_string())
                    .with_model(self.model())
                    .with_text(child.title())
                    .with_details(serde_json::json!({"title": child.title()})),
            );
            self.publish_metrics();
        }
        self.dispatch_hook_journaled(
            &child.id,
            &crate::hooks::HookContext::new(crate::hooks::HookPoint::DelegationEnd)
                .with_session(child.id.as_str())
                .with_workspace(self.workspace_root.display().to_string())
                .with_model(self.model())
                .with_text(child.title())
                .with_details(serde_json::json!({
                    "parent": parent.as_str(),
                    "bot": child.bot().map(|bot| bot.to_string()),
                })),
        );
        // Stage: agent_lifecycle — observe-only. The answer is already durable in the parent's
        // transcript (or there was none to deliver); the reap above may have dropped the child
        // from the live set, which is why its `Arc` was captured at entry.
        if let Some(child_live) = child_live {
            let _ = dispatch_lifecycle_stage(
                self,
                crate::pipeline::StagePoint::AgentLifecycle,
                &child_live,
                &child.title(),
                crate::pipeline::StagePayload::AgentLifecycle(
                    crate::pipeline::AgentLifecyclePayload {
                        op: crate::pipeline::AgentLifecycleOp::Deliver,
                        agent: child.bot(),
                        parent: parent.clone(),
                        session: child.id.clone(),
                        task: String::new(),
                        delivered,
                    },
                ),
                false,
            );
        }
    }

    /// Digest a result the transcript cannot carry, keeping the full text addressable.
    ///
    /// The ceiling is `context.tool_output_ceiling`, so the operator's budget knob decides when a
    /// result is too big; the spooled text is what `output.read` serves back.
    pub fn spool_output(&self, session: &SessionId, call: &str, content: String) -> String {
        use crate::compaction::digest_tool_output;
        let ceiling = self
            .config
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .context
            .tool_output_ceiling as usize;
        let chars = content.chars().count();
        if chars <= ceiling {
            return content;
        }
        let digested = digest_tool_output(&content, ceiling);
        match crate::spill::Spill::for_session(self.workspace.home(), session).write(call, &content)
        {
            Some(handle) => format!(
                "{digested}{}",
                crate::spill::pointer(&handle, chars - ceiling)
            ),
            None => format!(
                "{digested}\n\n[{} characters elided; no spool was available for this session]",
                chars - ceiling
            ),
        }
    }

    pub fn tools(&self) -> &crate::tools::ToolRegistry {
        &self.tools
    }

    /// `self: &Arc<Self>` because the `session_lifecycle` stage's context holds the runtime as
    /// an `Arc`; every caller already has one.
    pub fn create_session(self: &Arc<Self>, title: Option<String>) -> Result<Arc<SessionRuntime>> {
        let now = now_ms();
        // A session starts life named only when the caller actually named it. Otherwise it carries
        // the placeholder and stays `pending`, which is what lets the first real message earn it a
        // title. The old code derived a name from the session's position in the list, so identity
        // and label were tangled: deleting a session renumbered the ones behind it.
        let supplied = title.filter(|t| !t.trim().is_empty() && !titling::is_placeholder(t));
        let title = supplied
            .clone()
            .unwrap_or_else(|| titling::NEW_SESSION.to_owned());
        let model = self.model();
        let mut row = self
            .store
            .create_session(
                &title,
                &self.workspace_root,
                if model.is_empty() { None } else { Some(&model) },
                now,
            )
            .map_err(|e| RuntimeError::Store(e.to_string()))?;
        if supplied.is_none() {
            self.store
                .mark_session_title_pending(&row.id, vikram_store::TITLE_GENERIC)
                .map_err(|e| RuntimeError::Store(e.to_string()))?;
            row.title_source = vikram_store::TITLE_GENERIC.to_owned();
            row.title_status = vikram_store::TITLE_PENDING.to_owned();
        }
        // Stamp the ownership now so user/profile deletion cascades reach this session too.
        let identity = self.store.get_active_identity().unwrap_or_default();
        if let Some(user) = identity.user {
            self.store
                .set_session_user(&row.id, &user)
                .map_err(|e| RuntimeError::Store(e.to_string()))?;
        }
        if let Some(profile) = identity.profile {
            self.store
                .set_session_profile(&row.id, &profile)
                .map_err(|e| RuntimeError::Store(e.to_string()))?;
        }
        let session = Arc::new(SessionRuntime::from_row(row));
        // A session-start hook may refuse the session before it ever goes live; a refused row is
        // closed in the store so a restart does not resurrect it.
        let start = self.dispatch_hook_journaled(
            &session.id,
            &crate::hooks::HookContext::new(crate::hooks::HookPoint::SessionStart)
                .with_session(session.id.as_str())
                .with_workspace(self.workspace_root.display().to_string())
                .with_model(self.model())
                .with_text(session.title())
                .with_details(serde_json::json!({"title": session.title()})),
        );
        if let Some(reason) = &start.effect.deny {
            let _ = self.store.set_session_status(&session.id, "closed", now);
            return Err(RuntimeError::InvalidState(format!(
                "a plugin refused this session: {reason}"
            )));
        }
        // Stage: session_lifecycle — `create` is the one lifecycle operation a stage may veto.
        // The dispatch runs after the row and its runtime exist but before the session joins
        // the live set, so a refusal closes the row exactly like the hook denial above.
        if let Err(reason) = dispatch_lifecycle_stage(
            self,
            crate::pipeline::StagePoint::SessionLifecycle,
            &session,
            &session.title(),
            crate::pipeline::StagePayload::SessionLifecycle(
                crate::pipeline::SessionLifecyclePayload {
                    op: crate::pipeline::SessionLifecycleOp::Create,
                    session: session.id.clone(),
                    title: session.title(),
                },
            ),
            true,
        ) {
            let _ = self.store.set_session_status(&session.id, "closed", now);
            return Err(RuntimeError::InvalidState(format!(
                "a stage refused this session: {reason}"
            )));
        }
        self.sessions
            .lock()
            .expect("sessions lock")
            .push(Arc::clone(&session));
        self.memory_providers().initialize_all(session.id.as_str());
        self.emit(RuntimeEvent::SessionCreated(session.snapshot()));
        Ok(session)
    }

    pub fn status(&self) -> RuntimeStatus {
        let busy = self.session_list().iter().any(|s| s.is_busy()) || self.bots.active_count() > 0;
        if self
            .provider
            .read()
            .unwrap_or_else(|e| {
                eprintln!("vikram: runtime provider lock rwlock poisoned; recovering");
                e.into_inner()
            })
            .is_none()
        {
            RuntimeStatus::Error("model not configured".into())
        } else if busy {
            RuntimeStatus::Active
        } else {
            RuntimeStatus::Idle
        }
    }

    pub fn snapshot(&self) -> StateSnapshot {
        let mut sessions: Vec<SessionSnapshot> =
            self.session_list().iter().map(|s| s.snapshot()).collect();
        sessions.sort_by_key(|s| s.created_ms);
        StateSnapshot {
            status: self.status(),
            metrics: self.metrics(),
            sessions,
            bots: self.bots.snapshots(),
            rosters: self
                .rosters
                .list()
                .into_iter()
                .map(|t| HiveRosterSnapshot {
                    id: t.id.clone(),
                    name: t.name.clone(),
                    members: t.members.clone(),
                    session: None,
                    paused: false,
                    working: false,
                })
                .collect(),
            tasks: self.tasks.list(),
            approvals: self
                .approvals
                .lock()
                .expect("approvals lock")
                .values()
                .map(|p| p.prompt.clone())
                .collect(),
            skills: crate::skills::SkillRegistry::for_workspace(&self.workspace_root)
                .skills()
                .into_iter()
                .map(|s| s.name)
                .collect(),
            workspace_root: self.workspace_root.clone(),
            mode: self.mode(),
        }
    }

    pub fn task_snapshot(&self, task: &BackgroundTask) {
        self.emit(RuntimeEvent::TaskChanged(task.clone()));
    }
}

#[derive(Clone)]
pub struct RuntimeHandle {
    pub(crate) inner: Arc<RuntimeInner>,
}

pub struct Runtime {
    handle: RuntimeHandle,
}

/// What one self-check found. Carried out of the blocking store read so the
/// notices are emitted on the async side.
pub(crate) struct SelfCheckReport {
    pub status: vikram_evolution::HealthStatus,
    pub issues: Vec<vikram_evolution::mapek::Issue>,
    pub opportunities: Vec<vikram_evolution::self_improvement::ImprovementOpportunity>,
    /// Why the opportunity scan returned what it did (empty evidence, or a
    /// summary of the evidence used). `None` when no scan ran.
    pub opportunity_note: Option<String>,
}

impl RuntimeInner {
    /// Boot the periodic self-check: one task, one interval, findings surface as
    /// [`RuntimeEvent::Notice`]s. The first tick fires immediately. Returns `false`
    /// when a check is already scheduled, so callers cannot double-schedule it.
    pub(crate) fn spawn_self_check(self: &Arc<Self>, interval: std::time::Duration) -> bool {
        if self.self_check_scheduled.swap(true, Ordering::SeqCst) {
            return false;
        }
        let inner = Arc::clone(self);
        tokio::spawn(async move {
            let mut ticker = tokio::time::interval(interval);
            ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
            let mut tick: u64 = 0;
            loop {
                ticker.tick().await;
                tick += 1;
                inner.self_check_ticks.fetch_add(1, Ordering::SeqCst);
                inner
                    .self_check_tick(tick.is_multiple_of(SELF_CHECK_OPPORTUNITY_EVERY))
                    .await;
            }
        });
        true
    }

    async fn self_check_tick(self: &Arc<Self>, scan_opportunities: bool) {
        match self.run_self_check(scan_opportunities).await {
            Ok(report) => self.emit_self_check(report),
            Err(err) => self.emit(RuntimeEvent::Notice {
                level: NoticeLevel::Warn,
                text: format!("evolution self-check failed: {err}"),
            }),
        }
    }

    /// One self-check: `mapek.monitor()` + `mapek.analyze()` are read-only probes
    /// over the store and `self_improvement.identify_improvement_opportunities()`
    /// is a scan that stores nothing — it only reports. The mutating
    /// `plan`/`execute`/`run_cycle` half of MAPE-K is deliberately not run on a
    /// timer. All of it is synchronous store work, so it runs on the blocking
    /// pool like the boot recovery drain.
    pub(crate) async fn run_self_check(
        self: &Arc<Self>,
        scan_opportunities: bool,
    ) -> std::result::Result<SelfCheckReport, String> {
        let worker = Arc::clone(self);
        tokio::task::spawn_blocking(move || {
            let mut engine = worker
                .learning
                .evolution
                .lock()
                .unwrap_or_else(|e| e.into_inner());
            let data = engine.mapek.monitor().map_err(|e| e.to_string())?;
            let analysis = engine.mapek.analyze(&data).map_err(|e| e.to_string())?;
            let opportunities = if scan_opportunities {
                engine
                    .self_improvement
                    .identify_improvement_opportunities()
                    .map_err(|e| e.to_string())?
            } else {
                Vec::new()
            };
            let opportunity_note = if scan_opportunities {
                engine
                    .self_improvement
                    .last_opportunity_note()
                    .map(str::to_owned)
            } else {
                None
            };
            // A durable trail for `vikram evolution`; telemetry must not fail the check.
            let _ = engine.store.save_event(&vikram_evolution::LearningEvent {
                id: vikram_evolution::gen_id("selfcheck"),
                timestamp_ms: vikram_evolution::now_ms(),
                agent_id: "self-check".to_owned(),
                kind: "self_check".to_owned(),
                summary: format!(
                    "self-check: health {:?}, {} issue(s), {} opportunity scan result(s)",
                    data.runtime_health.overall_status,
                    analysis.issues.len(),
                    opportunities.len()
                ),
                payload: serde_json::json!({
                    "status": format!("{:?}", data.runtime_health.overall_status),
                    "issues": analysis
                        .issues
                        .iter()
                        .map(|i| format!("{}: {}", i.component, i.description))
                        .collect::<Vec<_>>(),
                    "opportunities": opportunities.len(),
                }),
            });
            Ok(SelfCheckReport {
                status: data.runtime_health.overall_status,
                issues: analysis.issues,
                opportunities,
                opportunity_note,
            })
        })
        .await
        .map_err(|e| format!("self-check task failed: {e}"))?
    }

    fn emit_self_check(&self, report: SelfCheckReport) {
        match report.status {
            vikram_evolution::HealthStatus::Unhealthy
            | vikram_evolution::HealthStatus::Critical => {
                let detail = report
                    .issues
                    .iter()
                    .take(3)
                    .map(|i| format!("{}: {}", i.component, i.description))
                    .collect::<Vec<_>>()
                    .join("; ");
                self.emit(RuntimeEvent::Notice {
                    level: NoticeLevel::Warn,
                    text: format!(
                        "evolution self-check: runtime health {:?} — {detail}",
                        report.status
                    ),
                });
            }
            vikram_evolution::HealthStatus::Degraded => self.emit(RuntimeEvent::Notice {
                level: NoticeLevel::Info,
                text: "evolution self-check: runtime health degraded".to_owned(),
            }),
            // Healthy needs no announcement; Unknown means the store has too
            // little recorded activity to judge — unverified, not well.
            vikram_evolution::HealthStatus::Healthy | vikram_evolution::HealthStatus::Unknown => {}
        }
        if !report.opportunities.is_empty() {
            let top = &report.opportunities[0];
            self.emit(RuntimeEvent::Notice {
                level: NoticeLevel::Info,
                text: format!(
                    "evolution self-check: {} improvement opportunit{} — top: {}",
                    report.opportunities.len(),
                    if report.opportunities.len() == 1 {
                        "y"
                    } else {
                        "ies"
                    },
                    top.description
                ),
            });
        } else if let Some(note) = &report.opportunity_note {
            self.emit(RuntimeEvent::Notice {
                level: NoticeLevel::Info,
                text: format!("evolution self-check: {note}"),
            });
        }
    }

    /// The paid half of the growth pipeline: ask the model what in this conversation is worth
    /// keeping, then hand every answer to the same gate a heuristic proposal goes through. A
    /// model's output is content, never a decision — it cannot widen a permission, and a disabled
    /// policy drops it here exactly as it drops one the runtime derived by itself.
    pub(crate) fn spawn_learning_review(
        self: &Arc<Self>,
        session: SessionId,
        snapshot_text: String,
    ) {
        let inner = Arc::clone(self);
        inner.review_tasks.fetch_add(1, Ordering::SeqCst);
        let guard = LearningGuard(Arc::clone(&inner.review_tasks));
        tokio::spawn(async move {
            let _guard = guard;
            if inner.learning.surface.policy_memory() == crate::LearningPolicy::Disabled
                && inner.learning.surface.policy_skill() == crate::LearningPolicy::Disabled
            {
                return;
            }
            let provider = match inner
                .provider
                .read()
                .unwrap_or_else(|e| {
                    eprintln!("vikram: runtime provider lock rwlock poisoned; recovering");
                    e.into_inner()
                })
                .clone()
            {
                Some(p) => p,
                None => return,
            };
            let request = crate::provider::CompletionRequest {
                model: inner.model(),
                messages: vec![vikram_contracts::Message::user(format!(
                    "{}\n\nConversation:\n{}",
                    inner.learning.surface.review_prompt(),
                    snapshot_text
                ))],
                tools: Vec::new(),
                max_output_tokens: inner
                    .config
                    .read()
                    .unwrap_or_else(|e| e.into_inner())
                    .max_output_tokens,
            };
            use futures_util::StreamExt;
            let mut stream = provider.stream(request);
            let mut collected = String::new();
            // Same discipline as the turn loop: a hung or silent endpoint must not park this
            // review (and `wait_for_learning` behind it) forever. Silence is measured from the
            // last event that produced text, on the configured cap.
            let silence_cap = std::time::Duration::from_secs(
                inner
                    .config
                    .read()
                    .unwrap_or_else(|e| e.into_inner())
                    .stream_silence_secs,
            );
            let mut last_progress = std::time::Instant::now();
            loop {
                let item = match tokio::time::timeout(
                    std::time::Duration::from_millis(250),
                    stream.next(),
                )
                .await
                {
                    Ok(Some(item)) => Some(item),
                    Ok(None) => break,
                    Err(_) => None,
                };
                if last_progress.elapsed() >= silence_cap {
                    break;
                }
                let Some(item) = item else { continue };
                match item {
                    Ok(crate::provider::ProviderEvent::TextDelta(t)) => {
                        if !t.is_empty() {
                            last_progress = std::time::Instant::now();
                            collected.push_str(&t);
                        }
                    }
                    Ok(crate::provider::ProviderEvent::Finished { .. }) => break,
                    Ok(_) => {}
                    Err(_) => break,
                }
            }
            let trimmed = collected.trim();
            if trimmed.is_empty() || trimmed == "Nothing to save." {
                return;
            }
            let json_str = trimmed
                .trim_start_matches("```json")
                .trim_start_matches("```")
                .trim_end_matches("```")
                .trim();
            let Ok(proposals) = serde_json::from_str::<Vec<serde_json::Value>>(json_str) else {
                return;
            };
            for proposal in proposals {
                let field = |name: &str, fallback: &str| -> String {
                    proposal
                        .get(name)
                        .and_then(|v| v.as_str())
                        .unwrap_or(fallback)
                        .to_owned()
                };
                let content = field("content", "");
                if content.trim().is_empty() {
                    continue;
                }
                let after = content.clone();
                crate::learning::apply_or_stage(
                    &inner,
                    crate::LearningProposal {
                        id: crate::learning::new_proposal_id(),
                        session: session.clone(),
                        kind: field("kind", "memory"),
                        target: field("target", "memory"),
                        action: field("action", "add"),
                        key: field("key", ""),
                        content,
                        reason: field("reason", ""),
                        status: "pending".to_owned(),
                        created_ms: crate::types::now_ms(),
                        before: None,
                        after: Some(after),
                    },
                );
            }
        });
    }
}

impl RuntimeInner {
    /// Name a session after what it is actually about, once, without the operator waiting on it.
    ///
    /// Three things gate it, and each exists because a naive version went wrong somewhere else:
    /// provenance (a title a human typed is theirs, not ours to replace), the pending flag (one
    /// attempt per session, so a ten-message conversation is not ten model calls), and the claim
    /// (two messages arriving at once still produce one title). When there is nothing worth naming
    /// yet — a session opened with "hi" — the claim is handed back so a later message can name it.
    pub(crate) fn maybe_title_session(self: &Arc<Self>, session: &Arc<SessionRuntime>) {
        if !session.accepts_derived_title() {
            return;
        }
        if session.title_status() != vikram_store::TITLE_PENDING {
            return;
        }
        if !session.claim_titling() {
            return;
        }
        let Some(seed) = titling::seed(&session.messages()) else {
            session.release_titling();
            return;
        };
        self.spawn_titling(Arc::clone(session), seed);
    }

    /// Run the naming request in the background. Reuses the learning review's task counter: a
    /// one-shot process that exits before this lands has paid for a title and thrown it away, and
    /// headless runs already wait on that counter before shutting down.
    pub(crate) fn spawn_titling(self: &Arc<Self>, session: Arc<SessionRuntime>, seed: String) {
        let inner = Arc::clone(self);
        inner.review_tasks.fetch_add(1, Ordering::SeqCst);
        let guard = LearningGuard(Arc::clone(&inner.review_tasks));
        tokio::spawn(async move {
            let _guard = guard;
            // Bound here, not in the `match`: a read guard held as the match scrutinee lives through
            // the await below, and that makes the future non-`Send`.
            let provider = inner
                .provider
                .read()
                .unwrap_or_else(|e| {
                    eprintln!("vikram: runtime provider lock rwlock poisoned; recovering");
                    e.into_inner()
                })
                .clone();
            let derived = match provider {
                Some(provider) => {
                    let request = crate::provider::CompletionRequest {
                        model: inner.title_model(),
                        messages: vec![vikram_contracts::Message::user(titling::prompt(&seed))],
                        tools: Vec::new(),
                        // A title is a few words. The cap is what keeps this a cheap call.
                        max_output_tokens: Some(24),
                    };
                    let raw = inner.collect_title(&provider, request).await;
                    // Whatever the model said is checked, and if it was unusable the session is
                    // named from its own words rather than left anonymous.
                    titling::sanitize(&raw).or_else(|| titling::sanitize(&titling::fallback(&seed)))
                }
                // No model configured is not a failure to report, it is the ordinary offline case:
                // name the session from its own words instead.
                None => titling::sanitize(&titling::fallback(&seed)),
            };
            match derived {
                Some(title) => inner.apply_derived_title(&session, &title),
                None => {
                    session.set_title_status(vikram_store::TITLE_FAILED);
                    let _ = inner
                        .store
                        .set_session_title_status(&session.id, vikram_store::TITLE_FAILED);
                    inner.publish_session(&session);
                }
            }
        });
    }

    /// Drain a naming stream to text. Returns whatever came out, usable or not; [`titling::sanitize`]
    /// is the one that decides.
    async fn collect_title(
        &self,
        provider: &Arc<dyn Provider>,
        request: crate::provider::CompletionRequest,
    ) -> String {
        use futures_util::StreamExt;
        // A slow or hung endpoint must not hold a process open: naming is worth a second, not a turn.
        let drain = async {
            let mut stream = provider.stream(request);
            let mut collected = String::new();
            while let Some(item) = stream.next().await {
                match item {
                    Ok(crate::provider::ProviderEvent::TextDelta(t)) => collected.push_str(&t),
                    Ok(crate::provider::ProviderEvent::Finished { .. }) => break,
                    Ok(_) => {}
                    Err(_) => break,
                }
            }
            collected
        };
        tokio::time::timeout(std::time::Duration::from_secs(2), drain)
            .await
            .unwrap_or_default()
    }

    /// Store the derived name — but only if the session still has no name of its own. The store
    /// re-checks provenance in its `WHERE`, so a rename that landed while the model was answering
    /// wins: this returns quietly and the operator's title survives.
    pub(crate) fn apply_derived_title(
        self: &Arc<Self>,
        session: &Arc<SessionRuntime>,
        title: &str,
    ) {
        if !session.accepts_derived_title() {
            return;
        }
        let now = now_ms();
        match self.store.set_session_auto_title(&session.id, title, now) {
            Ok(true) => {
                session.set_title(title, now);
                session.set_title_source(vikram_store::TITLE_AUTO);
                session.set_title_status(vikram_store::TITLE_READY);
                self.publish_session(session);
            }
            Ok(false) => {
                session.set_title_source(vikram_store::TITLE_MANUAL);
            }
            Err(_) => {}
        }
    }

    /// The cheap model used for naming, if one is configured. Naming a session is not worth the
    /// price of the reasoning model the operator is chatting with.
    fn title_model(&self) -> String {
        if !self
            .config
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .title_model
            .trim()
            .is_empty()
        {
            return self
                .config
                .read()
                .unwrap_or_else(|e| e.into_inner())
                .title_model
                .trim()
                .to_owned();
        }
        self.model()
    }
}

impl Runtime {
    pub async fn start(config: Config) -> Result<Self> {
        Self::boot(config, None, SELF_CHECK_INTERVAL).await
    }

    /// Boot with a provider the caller supplies instead of the one configuration builds. Only the
    /// turn loop's tests use it: a scripted stream is the only way to prove the order in which a
    /// turn publishes speech, tool rows and tool results without a network.
    #[cfg(test)]
    pub(crate) async fn start_with_provider(
        config: Config,
        provider: Arc<dyn Provider>,
    ) -> Result<Self> {
        Self::boot(config, Some(provider), SELF_CHECK_INTERVAL).await
    }

    /// Boot with a self-check interval short enough for a test to observe ticks.
    #[cfg(test)]
    pub(crate) async fn start_with_self_check_interval(
        config: Config,
        interval: std::time::Duration,
    ) -> Result<Self> {
        Self::boot(config, None, interval).await
    }

    async fn boot(
        config: Config,
        provider_override: Option<Arc<dyn Provider>>,
        self_check_interval: std::time::Duration,
    ) -> Result<Self> {
        ensure_home()?;
        let store =
            Store::open(&database_path()).map_err(|e| RuntimeError::Store(e.to_string()))?;
        let memory = crate::MemoryStore::open(database_path())
            .map_err(|e| RuntimeError::Store(e.to_string()))?;
        let workspace_root = config
            .workspace_root
            .clone()
            .unwrap_or_else(|| std::env::current_dir().unwrap_or_else(|_| PathBuf::from(".")));
        let workspace_root = std::fs::canonicalize(&workspace_root).unwrap_or(workspace_root);
        let mut rules = RuleBook::open();
        rules.set_workspace_root(Some(workspace_root.clone()));

        let context = Arc::new(
            crate::context::ContextLoader::new(
                crate::config::vikram_home(),
                workspace_root.clone(),
            )
            .with_budget(config.context.project_budget_tokens),
        );

        let hive = crate::HiveManager::new(std::sync::Arc::new(store.clone()));
        let tickets = crate::TicketManager::new(std::sync::Arc::new(store.clone()));
        let learning =
            crate::LearningManager::open(Some(crate::config::vikram_home().join("learning.json")));
        let dialectic = crate::dialectic::DialecticEngine::new(config.context.dialectic.clone());

        let bots = BotManager::new();
        match store.list_bots() {
            Ok(rows) => {
                for row in rows {
                    bots.add(Arc::new(ManagedBot::from_profile(row.into())));
                }
            }
            Err(err) => return Err(RuntimeError::Store(err.to_string())),
        }
        let provider = match provider_override {
            Some(provider) => Some(provider),
            None => crate::provider::for_config(&config)?,
        };
        let memory_providers = Mutex::new(crate::MemoryProviderManager::from_config(
            &config,
            memory.clone(),
        )?);

        // Visibility state persists next to the MCP config so `toggle_mcp_visibility`
        // and `set_exposed_tools` survive restarts; the manager loads existing
        // entries from this path at startup.
        let mcp = McpRegistry::load(McpConfig::load().into_specs());
        let mcp_visibility_path =
            crate::config::mcp_config_path().with_file_name("mcp-visibility.json");
        let mcp_visibility = VisibilityManager::with_persist_path(mcp_visibility_path);
        let plugins = PluginRegistry::from_default_dir();
        let hooks = std::sync::RwLock::new(crate::hooks::HookRegistry::from_plugins(&plugins));
        let stages = std::sync::Arc::new(crate::pipeline::StageRegistry::new());
        let skills = std::sync::Mutex::new(SkillRegistry::for_workspace(&workspace_root));
        let evolution_skills_root = crate::config::vikram_home().join("skills");
        if let Err(e) = std::fs::create_dir_all(&evolution_skills_root) {
            eprintln!(
                "vikram: evolution skills dir {}: {e}",
                evolution_skills_root.display()
            );
        }
        let evolution_db = crate::config::vikram_home().join("evolution.db");
        let mut evolution_fallback: Option<String> = None;
        let evolution = match vikram_evolution::EvolutionEngine::open(
            evolution_db.clone(),
            evolution_skills_root,
        ) {
            Ok(engine) => engine,
            Err(e) => {
                let reason = e.to_string();
                eprintln!(
                    "vikram: evolution store at {} failed ({reason}); running on in-memory fallback — evolution history will not persist",
                    evolution_db.display()
                );
                evolution_fallback = Some(reason);
                match vikram_evolution::EvolutionEngine::open_in_memory(
                    crate::config::vikram_home().join("skills"),
                ) {
                    Ok(engine) => engine,
                    Err(inner) => {
                        return Err(RuntimeError::Store(format!(
                            "evolution in-memory fallback failed: {inner}"
                        )));
                    }
                }
            }
        };

        // The layers above this runtime can refuse a posture; a config that asked for one of those
        // starts on the most permissive thing still available rather than failing to boot.
        let availability = ModeAvailability::resolve(&config.security);
        let mode = if availability.permits(config.mode) {
            config.mode
        } else {
            availability.fallback_mode()
        };

        let model_name = config.model.clone();

        let workspace = WorkspaceManager::new(store.clone(), crate::config::vikram_home());

        let inner = Arc::new(RuntimeInner {
            config: std::sync::RwLock::new(config),
            store,
            memory,
            memory_providers,
            context,
            hive: HiveState { manager: hive },
            tickets: TicketState { manager: tickets },
            learning: LearningState {
                surface: learning,
                evolution: Mutex::new(evolution),
            },
            dialectic,
            bus: EventBus::new(),
            tools: ToolRegistry::with_builtins(),
            provider: std::sync::RwLock::new(provider),
            sessions: Mutex::new(Vec::new()),
            bots,
            tasks: TaskManager::new(),
            rosters: HiveRosterManager::load(),
            mcp: Arc::new(Mutex::new(mcp)),
            mcp_visibility: std::sync::Arc::new(Mutex::new(mcp_visibility)),
            plugins: Mutex::new(plugins),
            hooks,
            stages,
            executor: tokio::runtime::Handle::current(),
            skills,
            workspace,
            approvals: Mutex::new(HashMap::new()),
            questions: Mutex::new(HashMap::new()),
            mode: Mutex::new(mode),
            model: Mutex::new(model_name),
            availability,
            rules: Mutex::new(rules),
            workspace_root,
            started_ms: now_ms(),
            last_turn_ms: Mutex::new(None),
            last_tokens_per_sec: Mutex::new(None),
            review_tasks: Arc::new(AtomicU64::new(0)),
            self_check_scheduled: AtomicBool::new(false),
            self_check_ticks: AtomicU64::new(0),
            activity_seq: AtomicU64::new(0),
        });

        if let Some(reason) = evolution_fallback {
            inner.emit(RuntimeEvent::Notice {
                level: NoticeLevel::Warn,
                text: format!(
                    "evolution store unavailable ({reason}); running on in-memory fallback — evolution history will not persist"
                ),
            });
        }

        // Plugins declaring `components.stages` become stage handlers, competing with native
        // registrations by manifest priority; each still wraps the builtin default.
        for rejection in inner.reload_plugin_stages() {
            inner.emit(RuntimeEvent::Notice {
                level: NoticeLevel::Warn,
                text: format!(
                    "plugin `{}` stage refused: {}",
                    rejection.plugin, rejection.reason
                ),
            });
        }

        // Reload previously derived dialectic insights so the user model survives restarts.
        // Best-effort: a fresh store simply means an empty model.
        let _ = inner.dialectic.load_from(&inner.memory);

        // Rehydrate durable sessions. Journal replay restores the conversation without re-applying
        // any effect: the journal is a record, not a queue.
        let rows = inner
            .store
            .list_sessions()
            .map_err(|e| RuntimeError::Store(e.to_string()))?;
        for row in rows {
            let session = Arc::new(SessionRuntime::from_row(row));
            if let Ok(Some((parent, depth))) = inner.store.session_lineage(&session.id) {
                session.set_parent(Some(parent));
                session.set_depth(depth);
            }
            let records = match inner.store.replay_messages(&session.id) {
                Ok(records) => records,
                Err(err) => {
                    inner.emit(RuntimeEvent::Notice {
                        level: NoticeLevel::Warn,
                        text: format!("could not replay the transcript: {err}"),
                    });
                    Vec::new()
                }
            };
            if !records.is_empty() {
                let messages = crate::compaction::project_from_journal(
                    records
                        .into_iter()
                        .map(|record| (record.seq, record.kind, record.payload)),
                );
                *session.messages.lock().unwrap_or_else(|e| {
                    eprintln!("vikram: runtime messages lock mutex poisoned; recovering");
                    e.into_inner()
                }) = messages;
            }
            inner.memory_providers().initialize_all(session.id.as_str());
            inner
                .sessions
                .lock()
                .unwrap_or_else(|e| {
                    eprintln!("vikram: runtime sessions lock mutex poisoned; recovering");
                    e.into_inner()
                })
                .push(Arc::clone(&session));
            // Stage: session_lifecycle — boot rehydration is the `load` operation. Observe-only:
            // a stage failure is reported, never allowed to keep a stored session from loading.
            let _ = dispatch_lifecycle_stage(
                &inner,
                crate::pipeline::StagePoint::SessionLifecycle,
                &session,
                &session.title(),
                crate::pipeline::StagePayload::SessionLifecycle(
                    crate::pipeline::SessionLifecyclePayload {
                        op: crate::pipeline::SessionLifecycleOp::Load,
                        session: session.id.clone(),
                        title: session.title(),
                    },
                ),
                false,
            );
        }

        // Bootstrap a default user/profile/identity if none exist, so multi-user isolation has a
        // starting account without assuming a single global user.
        if inner
            .store
            .list_users()
            .map(|u| u.is_empty())
            .unwrap_or(true)
        {
            let now = now_ms();
            if let Ok(user) = inner.store.create_user("default", now)
                && let Ok(profile) = inner.store.create_profile(&user.id, "default", now)
            {
                let _ = inner
                    .store
                    .set_active_identity(&vikram_store::ActiveIdentity {
                        user: Some(user.id),
                        profile: Some(profile.id),
                        main_agent: None,
                    });
            }
        }

        if inner.session_list().is_empty() {
            inner.create_session(None)?;
        }

        let handle = RuntimeHandle {
            inner: Arc::clone(&inner),
        };

        // Reflection jobs queued before a crash are durable; drain them off the boot path so a
        // backlog cannot hold the first frame. Idempotent, so every boot runs it.
        let recovery = Arc::clone(&inner);
        tokio::spawn(async move {
            let worker = Arc::clone(&recovery);
            let drained = tokio::task::spawn_blocking(move || {
                let engine = worker
                    .learning
                    .evolution
                    .lock()
                    .unwrap_or_else(|e| e.into_inner());
                engine.recover_and_process()
            })
            .await;
            match drained {
                Ok(Ok((recovered, processed))) => {
                    if recovered + processed > 0 {
                        recovery.emit(RuntimeEvent::Notice {
                            level: NoticeLevel::Info,
                            text: format!(
                                "evolution: recovered {recovered} interrupted reflection job(s), drained {processed}"
                            ),
                        });
                    }
                }
                Ok(Err(err)) => recovery.emit(RuntimeEvent::Notice {
                    level: NoticeLevel::Warn,
                    text: format!("evolution job recovery failed: {err}"),
                }),
                Err(err) => recovery.emit(RuntimeEvent::Notice {
                    level: NoticeLevel::Warn,
                    text: format!("evolution job recovery task failed: {err}"),
                }),
            }
        });

        // The periodic self-check watches the same store the turn-end pipeline writes; it only
        // ever reads (monitor/analyze/opportunity scan) and reports findings as Notices.
        inner.spawn_self_check(self_check_interval);

        // MCP probing and plugin discovery are independent of the UI's first frame.
        let mcp_inner = Arc::clone(&inner);
        tokio::spawn(async move {
            let configured = {
                let registry = mcp_inner.mcp.lock().unwrap_or_else(|e| {
                    eprintln!("vikram: runtime mcp lock mutex poisoned; recovering");
                    e.into_inner()
                });
                registry.configured_count()
            };
            let registry = {
                let mut guard = mcp_inner.mcp.lock().unwrap_or_else(|e| {
                    eprintln!("vikram: runtime mcp lock mutex poisoned; recovering");
                    e.into_inner()
                });
                guard.take()
            };
            // `connect_all` is Err when any configured server failed; the
            // registry still holds the servers that connected and every
            // ServerState carries its own error. Surface both the aggregate
            // and the per-server failures, and register visibility entries
            // for every configured server so they are listed even when down.
            let (registry, result) = crate::mcp::connect_all(registry).await;
            let connected = registry.connected_count();
            let tool_count = registry.tools_count();
            let tool_defs = registry.tool_defs();
            let server_states = registry.servers().to_vec();
            {
                let mut guard = mcp_inner.mcp.lock().unwrap_or_else(|e| {
                    eprintln!("vikram: runtime mcp lock mutex poisoned; recovering");
                    e.into_inner()
                });
                *guard = registry;
            }
            for state in &server_states {
                if let Ok(mut manager) = mcp_inner.mcp_visibility.lock() {
                    manager.register(state.name.clone());
                }
            }
            for def in tool_defs {
                // A server the operator marked Private keeps its tools out of the model's
                // tool list entirely; `McpTool::run` re-checks so a mid-session toggle also
                // takes effect.
                let exposed = mcp_inner
                    .mcp_visibility
                    .lock()
                    .map(|m| m.is_public(&def.server))
                    .unwrap_or(false);
                if !exposed {
                    continue;
                }
                let mcp_tool = McpTool {
                    server: def.server,
                    tool: def.name,
                    description: def.description,
                    schema: def.input_schema,
                    registry: Arc::clone(&mcp_inner.mcp),
                    visibility: std::sync::Arc::clone(&mcp_inner.mcp_visibility),
                };
                mcp_inner.tools.register(Box::new(mcp_tool));
            }
            if let Err(err) = result {
                for state in &server_states {
                    if let Some(server_err) = &state.error {
                        eprintln!("vikram: MCP server `{}` failed: {server_err}", state.name);
                    }
                }
                mcp_inner.emit(RuntimeEvent::Notice {
                    level: NoticeLevel::Warn,
                    text: format!("MCP discovery: {err}"),
                });
            }
            mcp_inner.emit(RuntimeEvent::McpChanged {
                configured,
                connected,
                tools: tool_count,
            });
            mcp_inner.publish_metrics();
        });

        let (installed, loaded) = {
            let plugins = inner.plugins.lock().unwrap_or_else(|e| {
                eprintln!("vikram: runtime plugins mutex poisoned; recovering");
                e.into_inner()
            });
            (plugins.installed(), plugins.loaded())
        };
        inner.emit(RuntimeEvent::PluginsChanged { installed, loaded });
        inner.emit(RuntimeEvent::Status(inner.status()));
        inner.publish_metrics();

        Ok(Self { handle })
    }

    pub fn handle(&self) -> RuntimeHandle {
        self.handle.clone()
    }
}

impl RuntimeHandle {
    /// Register a native stage handler on the running pipeline. See
    /// [`RuntimeInner::register_stage`].
    pub fn register_stage(
        &self,
        point: crate::pipeline::StagePoint,
        handler: Arc<dyn crate::pipeline::StageHandler>,
        priority: i32,
    ) {
        self.inner.register_stage(point, handler, priority);
    }

    pub fn tool_context(&self) -> ToolContext {
        self.inner.tool_context()
    }

    pub fn context(&self) -> Arc<crate::context::ContextLoader> {
        Arc::clone(&self.inner.context)
    }

    pub fn active_identity(&self) -> vikram_store::ActiveIdentity {
        self.inner.store.get_active_identity().unwrap_or_default()
    }

    pub fn set_active_identity(&self, identity: &vikram_store::ActiveIdentity) -> Result<()> {
        self.inner
            .store
            .set_active_identity(identity)
            .map_err(|e| RuntimeError::Store(e.to_string()))
    }

    pub fn learning(&self) -> &crate::LearningManager {
        &self.inner.learning.surface
    }

    /// Whether the runtime is still working out what a finished turn taught it.
    pub fn learning_busy(&self) -> bool {
        self.inner.review_tasks.load(Ordering::SeqCst) > 0
    }

    /// Wait for in-flight learning to land, up to the deadline; `true` means it settled. The result
    /// is staged either way — this only decides whether the caller sees it before exiting.
    pub async fn wait_for_learning(&self, timeout: std::time::Duration) -> bool {
        let deadline = tokio::time::Instant::now() + timeout;
        while self.learning_busy() {
            if tokio::time::Instant::now() >= deadline {
                return false;
            }
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }
        true
    }

    pub fn trigger_learning_review(&self, session: &SessionId, snapshot_text: &str) {
        self.inner
            .spawn_learning_review(session.clone(), snapshot_text.to_owned());
    }

    pub fn tools(&self) -> &crate::tools::ToolRegistry {
        self.inner.tools()
    }

    /// The configuration this process is running with. Re-read from disk with [`Self::reload_config`].
    pub fn config(&self) -> Config {
        self.inner
            .config
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
    }

    /// Re-read the configuration from disk and update the running copy. Rebuilds the provider when
    /// the model, endpoint, key, or provider kind changed.
    pub fn reload_config(&self) -> Result<()> {
        let new_config = Config::load()?;
        let old = self.inner.config.read().unwrap_or_else(|e| e.into_inner());
        let needs_rebuild = old.model != new_config.model
            || old.provider != new_config.provider
            || old.base_url != new_config.base_url
            || old.api_key_env != new_config.api_key_env
            || old.context_window != new_config.context_window
            || old.api_key() != new_config.api_key()
            || old.fallback_model != new_config.fallback_model
            || old.memory_provider != new_config.memory_provider;
        let memory_provider_changed = old.memory_provider != new_config.memory_provider;
        // Drop the read guard before taking the write lock.
        drop(old);

        *self.inner.model.lock().unwrap_or_else(|e| e.into_inner()) = new_config.model.clone();
        *self.inner.config.write().unwrap_or_else(|e| e.into_inner()) = new_config.clone();

        if needs_rebuild {
            let mut provider_slot = self.inner.provider.write().unwrap_or_else(|e| {
                eprintln!("vikram: runtime provider lock rwlock poisoned; recovering");
                e.into_inner()
            });
            let built = crate::provider::for_config(&new_config);
            match built {
                Ok(Some(provider)) => {
                    *provider_slot = Some(provider);
                }
                Ok(None) => {
                    *provider_slot = None;
                }
                Err(err) => {
                    // A bad key/endpoint must not silently drop the provider — the next turn
                    // would fail with "no provider" and no hint why.
                    *provider_slot = None;
                    self.inner.emit(RuntimeEvent::Notice {
                        level: NoticeLevel::Warn,
                        text: format!("provider reload failed: {err}"),
                    });
                }
            }
            if memory_provider_changed {
                match crate::MemoryProviderManager::from_config(
                    &new_config,
                    self.inner.memory.clone(),
                ) {
                    Ok(manager) => {
                        self.inner.memory_providers().shutdown_all();
                        *self.inner.memory_providers() = manager;
                    }
                    Err(err) => {
                        self.inner.emit(RuntimeEvent::Notice {
                            level: NoticeLevel::Warn,
                            text: format!("memory provider reload failed: {err}"),
                        });
                    }
                }
            }
        }
        Ok(())
    }

    /// Re-scan the skill directories and replace the skill registry. Returns the new count.
    pub fn reload_skills(&self) -> usize {
        let reg = SkillRegistry::for_workspace(&self.inner.workspace_root);
        let count = reg.count();
        *self.inner.skills.lock().unwrap_or_else(|e| {
            eprintln!("vikram: runtime skills lock mutex poisoned; recovering");
            e.into_inner()
        }) = reg;
        self.inner.emit(RuntimeEvent::SkillsChanged {
            discovered: count,
            loaded: 0,
        });
        count
    }

    /// Flip one MCP server between private and public and return the new state as a label. The
    /// decision lives here because the runtime owns the manager and the file it persists to; an
    /// interface that flipped a copy would report a change that never happened.
    pub fn toggle_mcp_visibility(&self, server: &str) -> Result<String> {
        use vikram_mcp::visibility::Visibility;
        let mut manager = self.inner.mcp_visibility.lock().map_err(|_| {
            crate::error::RuntimeError::InvalidState("mcp visibility is locked".into())
        })?;
        let current = manager
            .get(server)
            .map(|entry| entry.visibility.clone())
            .unwrap_or_default();
        let next = match current {
            Visibility::Public => Visibility::Private,
            Visibility::Private => Visibility::Public,
        };
        manager.set_visibility(server, next.clone());
        Ok(next.label().to_owned())
    }

    /// The MCP servers this process knows about, as `(name, visibility label, exposed tools)`.
    pub fn mcp_servers(&self) -> Vec<(String, String, usize)> {
        let Ok(manager) = self.inner.mcp_visibility.lock() else {
            return Vec::new();
        };
        let mut servers: Vec<(String, String, usize)> = manager
            .list()
            .into_iter()
            .map(|entry| {
                (
                    entry.server_name.clone(),
                    entry.visibility.label().to_owned(),
                    entry.exposed_tools.len(),
                )
            })
            .collect();
        servers.sort_by(|a, b| a.0.cmp(&b.0));
        servers
    }

    pub fn workspace(&self) -> &WorkspaceManager {
        &self.inner.workspace
    }
    pub fn evolution(&self) -> std::sync::MutexGuard<'_, vikram_evolution::EvolutionEngine> {
        self.inner.learning.evolution.lock().unwrap_or_else(|e| {
            eprintln!("vikram: runtime evolution lock mutex poisoned; recovering");
            e.into_inner()
        })
    }
    pub fn events(&self) -> broadcast::Receiver<RuntimeEvent> {
        self.inner.bus.subscribe()
    }

    pub fn snapshot(&self) -> StateSnapshot {
        self.inner.snapshot()
    }

    pub fn status(&self) -> RuntimeStatus {
        self.inner.status()
    }

    /// The posture in force for this process.
    pub fn mode(&self) -> PermissionMode {
        self.inner.mode()
    }

    /// The postures this runtime is allowed to take, in interface order.
    pub fn modes(&self) -> Vec<PermissionMode> {
        self.inner.availability.available()
    }

    /// Switch posture, and record it. A posture refused by a layer above this runtime is an error
    /// naming that layer, never a silent step down to something else.
    pub fn set_mode(&self, session: &SessionId, mode: PermissionMode) -> Result<()> {
        let previous = self.inner.mode();
        self.inner.set_mode(mode)?;
        if previous != mode {
            self.inner.journal(
                session,
                RecordKind::Policy,
                serde_json::json!({
                    "decision": "mode",
                    "from": previous.as_str(),
                    "to": mode.as_str(),
                }),
            );
            self.inner.emit(RuntimeEvent::ModeChanged {
                session: session.clone(),
                previous,
                mode,
            });
        }
        // A posture is a standing choice, not a per-turn one. It is written down the same way the
        // model is, so a restart starts on what the operator picked instead of on the default.
        // The file is re-read first: the running copy predates any model chosen since boot, and
        // saving that copy would quietly undo it.
        let mut config = crate::config::Config::load()?;
        config.mode = mode;
        config.save()?;
        Ok(())
    }

    /// The durable allow-list, in saved order.
    pub fn allow_list(&self) -> Vec<Rule> {
        self.inner
            .rules
            .lock()
            .unwrap_or_else(|e| {
                eprintln!("vikram: runtime rules lock mutex poisoned; recovering");
                e.into_inner()
            })
            .snapshot()
    }

    /// Keep an approval past this request. Refused when a layer forbids keeping permissions at all.
    pub fn allow(&self, session: &SessionId, amendment: &str) -> Result<()> {
        if let Some((rule, layer)) = self.inner.availability.always_allow_refusal() {
            return Err(RuntimeError::Denied(format!(
                "approvals cannot be saved: {layer} sets `{rule}`"
            )));
        }
        let saved = self
            .inner
            .rules
            .lock()
            .expect("rules lock")
            .add_amendment(amendment)?;
        self.inner.journal(
            session,
            RecordKind::Policy,
            serde_json::json!({
                "decision": "allow-list",
                "rule": amendment,
                "saved": saved,
            }),
        );
        self.inner.emit(RuntimeEvent::Notice {
            level: NoticeLevel::Info,
            text: if saved {
                format!("saved to the allow-list: {amendment}")
            } else {
                "already on the allow-list".to_owned()
            },
        });
        Ok(())
    }

    /// Drop the allow-list entry at `index` in [`Self::allow_list`].
    pub fn unallow(&self, index: usize) -> Result<Rule> {
        self.inner
            .rules
            .lock()
            .unwrap_or_else(|e| {
                eprintln!("vikram: runtime rules lock mutex poisoned; recovering");
                e.into_inner()
            })
            .remove(index)
    }

    /// Queue if a turn is running; otherwise start one immediately.
    /// Put a message into a session that already exists, the way a delegation result arrives: the
    /// transcript, the journal and the event stream all get it, and no model turn starts.
    ///
    /// This is how work that finished outside a conversation — a scheduled job's report, a bot's
    /// answer — lands back in one without pretending the operator typed it. It refuses a session
    /// this runtime does not hold, because a message written into a copy nobody is reading is not
    /// a delivery.
    pub fn post_message(&self, session: &SessionId, text: String) -> Result<()> {
        let Some(target) = self.inner.session(session) else {
            return Err(RuntimeError::SessionNotFound(session.clone()));
        };
        let message = vikram_contracts::Message::text(
            vikram_contracts::Role::System,
            text,
            vikram_contracts::Trust::Host,
        );
        target.push_message(message.clone(), crate::types::now_ms());
        self.inner.journal(
            session,
            RecordKind::Message,
            serde_json::json!({"message": message}),
        );
        self.inner.emit(RuntimeEvent::MessageCommitted {
            session: session.clone(),
            message,
        });
        Ok(())
    }

    pub fn submit(&self, session: &SessionId, text: String) -> Result<()> {
        self.submit_message(session, Message::user(text))
    }

    /// Submit a turn the caller has already built, which is the only way an image reaches a model:
    /// [`Self::submit`] can only carry text.
    ///
    /// The queue behind a busy session is text-only, so images are supported on an idle session
    /// only for now. A queued message that carries images is refused rather than silently stripped:
    /// the turn would otherwise run without the attachment the operator meant it to have.
    pub fn submit_message(&self, session: &SessionId, message: Message) -> Result<()> {
        let Some(session) = self.inner.session(session) else {
            return Err(RuntimeError::SessionNotFound(session.clone()));
        };
        if session.is_busy() {
            if message.images().next().is_some() {
                return Err(RuntimeError::InvalidState(
                    "images cannot be queued behind a running turn; wait for it to finish".into(),
                ));
            }
            let Some(depth) = session.enqueue(message.text_body()) else {
                return Err(RuntimeError::InvalidState(format!(
                    "the follow-up queue is full ({} waiting); let the turn finish or cancel it first",
                    SessionRuntime::MAX_QUEUED_FOLLOW_UPS
                )));
            };
            self.inner.emit(RuntimeEvent::Notice {
                level: NoticeLevel::Info,
                text: format!("queued ({depth} waiting)"),
            });
            self.inner.publish_session(&session);
            return Ok(());
        }
        crate::turn::spawn_turn(Arc::clone(&self.inner), session, message, None);
        Ok(())
    }

    /// Run a command the operator typed at the `!` prompt. It is gated, approved and journaled by
    /// the same path a model-issued tool call takes; the only difference is provenance — this one
    /// is authored by the human at the keyboard, so it carries `Trust::User`.
    ///
    /// Returns as soon as the command is accepted. The `$ command` line, any approval prompt and
    /// the output all arrive on the event stream, so the interface never blocks on the process.
    pub fn run_user_command(&self, session: &SessionId, command: String) -> Result<()> {
        let Some(session) = self.inner.session(session) else {
            return Err(RuntimeError::SessionNotFound(session.clone()));
        };
        if command.trim().is_empty() {
            return Err(RuntimeError::InvalidState("no command to run".into()));
        }
        let inner = Arc::clone(&self.inner);
        let session_id = session.id.clone();
        inner.emit(RuntimeEvent::MessageCommitted {
            session: session_id.clone(),
            message: Message::system(format!("$ {command}")),
        });
        let call =
            vikram_contracts::ToolCall::new("shell.run", serde_json::json!({ "command": command }));
        tokio::spawn(async move {
            let (content, ok) = crate::turn::run_operator_tool(&inner, &session, &call).await;
            // A command that produced no output needs no transcript line; a refusal always does.
            if !ok || !content.trim().is_empty() {
                let text = if ok {
                    content
                } else {
                    format!("error: {content}")
                };
                inner.emit(RuntimeEvent::MessageCommitted {
                    session: session_id,
                    message: Message::system(text),
                });
            }
        });
        Ok(())
    }

    /// Choose the model every later request carries, and keep the choice: the live name, the
    /// session row, `$VIKRAM_HOME/config.json` and the journal all move together, so a restart
    /// starts on the model that was picked rather than on the one that happened to be configured.
    pub fn set_model(&self, session: &SessionId, model: String) -> Result<()> {
        let model = model.trim().to_owned();
        if model.is_empty() {
            return Err(RuntimeError::InvalidState(
                "a model name cannot be empty".into(),
            ));
        }
        let Some(session) = self.inner.session(session) else {
            return Err(RuntimeError::SessionNotFound(session.clone()));
        };
        let now = now_ms();
        *self.inner.model.lock().unwrap_or_else(|e| {
            eprintln!("vikram: runtime model lock mutex poisoned; recovering");
            e.into_inner()
        }) = model.clone();
        session.set_model(Some(model.clone()), now);
        self.inner
            .store
            .set_session_model(&session.id, Some(&model), now)
            .map_err(|err| RuntimeError::Store(err.to_string()))?;

        let mut config = self
            .inner
            .config
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .clone();
        config.model = model.clone();
        config.save()?;

        self.inner.journal(
            &session.id,
            RecordKind::Status,
            serde_json::json!({ "model": model, "set_by": "command" }),
        );
        self.inner.publish_session(&session);
        self.inner.publish_metrics();

        // A provider that was never built at boot (no key yet, e.g.) can still come up now: the
        // operator naming a model here is exactly the moment the endpoint is known. A live
        // provider built for a *different* model must also be rebuilt: its requests carry
        // the model it was constructed with, so keeping it would silently ignore the choice.
        let mut provider_slot = self.inner.provider.write().unwrap_or_else(|e| {
            eprintln!("vikram: runtime provider lock rwlock poisoned; recovering");
            e.into_inner()
        });
        let needs_rebuild = match provider_slot.as_ref() {
            None => true,
            Some(p) => p.model() != model,
        };
        if needs_rebuild {
            let built = crate::provider::for_config(&config);
            match built {
                Ok(Some(provider)) => {
                    *provider_slot = Some(provider);
                    self.inner.emit(RuntimeEvent::Notice {
                        level: NoticeLevel::Info,
                        text: format!("provider connected for `{model}`; turns can start now"),
                    });
                }
                _ => {
                    drop(provider_slot);
                    self.inner.emit(RuntimeEvent::Notice {
                        level: NoticeLevel::Warn,
                        text: format!(
                            "model set to `{model}`; no endpoint is configured yet, so a turn cannot start"
                        ),
                    });
                    return Ok(());
                }
            }
        }
        Ok(())
    }

    /// Point this process at another endpoint, live.
    ///
    /// The choice is written to `$VIKRAM_HOME/config.json` and the provider is rebuilt from it, so
    /// the next turn costs no restart. A key the new endpoint has not been given yet is not a
    /// failure: the switch is kept and the notice names the variable that is still empty.
    pub fn set_provider(
        &self,
        base_url: String,
        api_key_env: String,
        provider: String,
    ) -> Result<()> {
        let base_url = base_url.trim().to_owned();
        if base_url.is_empty() {
            return Err(RuntimeError::InvalidState(
                "an endpoint cannot be empty".into(),
            ));
        }
        let api_key_env = api_key_env.trim().to_owned();
        if api_key_env.is_empty() {
            return Err(RuntimeError::InvalidState(
                "a key variable cannot be empty".into(),
            ));
        }
        // Start from the configuration this process is actually running with, the way `set_model`
        // does: the model may have been chosen live from a config file that does not exist yet, and
        // saving a fresh read would quietly drop it.
        let mut config = self.config();
        config.base_url = base_url.clone();
        config.api_key_env = api_key_env.clone();
        config.provider = if provider.trim().is_empty() {
            "openai".to_owned()
        } else {
            provider.trim().to_owned()
        };
        config.save()?;
        self.reload_config()?;

        let key_state = if self.config().api_key().is_some() {
            format!("${api_key_env} is set")
        } else {
            format!("${api_key_env} is empty — add it to $VIKRAM_HOME/.env")
        };
        let (level, text) = if self.inner.provider.read().expect("provider lock").is_some() {
            (
                NoticeLevel::Info,
                format!("endpoint now {base_url} ({key_state})"),
            )
        } else {
            (
                NoticeLevel::Warn,
                format!("endpoint saved as {base_url}, but no provider could start: {key_state}"),
            )
        };
        self.inner.emit(RuntimeEvent::Notice { level, text });
        self.inner.publish_metrics();
        Ok(())
    }

    /// Read the endpoint's own model list without holding up the interface: the answer arrives as
    /// [`RuntimeEvent::ModelsListed`], including when the endpoint refuses or cannot be reached.
    pub fn refresh_models(&self) {
        let inner = Arc::clone(&self.inner);
        let base_url = inner
            .config
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .base_url
            .clone();
        let api_key = inner
            .config
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .api_key();
        tokio::spawn(async move {
            let outcome = if base_url.trim().is_empty() {
                Err("no endpoint is configured (`base_url`)".to_owned())
            } else {
                match api_key {
                    // The listing is a read of the operator's own endpoint, so it needs the key the
                    // config names — a local server accepts any bearer token, but the variable is
                    // still what says which key to send.
                    None => Err(format!(
                        "no key found: `${}` is empty",
                        inner
                            .config
                            .read()
                            .unwrap_or_else(|e| e.into_inner())
                            .api_key_env
                    )),
                    Some(key) => {
                        // A listing that fails on the wire is retried once: the first attempt is
                        // often a cold TLS handshake, and an empty panel is a worse answer than a
                        // second try that costs a fraction of a second.
                        let mut attempt = 0u32;
                        loop {
                            match crate::openai::list_models(&base_url, &key).await {
                                Err(err) if attempt == 0 && err.starts_with("could not reach") => {
                                    attempt += 1;
                                    tokio::time::sleep(std::time::Duration::from_millis(400)).await;
                                }
                                outcome => break outcome,
                            }
                        }
                    }
                }
            };
            let (models, error) = match outcome {
                Ok(models) => (models, None),
                Err(err) => (Vec::new(), Some(err)),
            };
            inner.emit(RuntimeEvent::ModelsListed { models, error });
        });
    }

    pub fn queue(&self, session: &SessionId, text: String) -> Result<()> {
        // Submitting is the honest implementation of "send this": it enqueues behind a running turn
        // and starts one when nothing is running. Enqueueing unconditionally — which is what this
        // did — stranded the text on an idle session forever, because the queue is only drained at
        // the end of a turn and no turn was ever going to end.
        self.submit(session, text)
    }

    /// Steer a running turn: inject an instruction at the next safe point (between tool calls).
    /// If no turn is running, falls back to a normal submit.
    pub fn steer(&self, session: &SessionId, text: String) -> Result<()> {
        let Some(session) = self.inner.session(session) else {
            return Err(RuntimeError::SessionNotFound(session.clone()));
        };
        if session.is_busy() {
            session.steer(text);
            self.inner.emit(RuntimeEvent::Notice {
                level: NoticeLevel::Info,
                text: "steered".into(),
            });
            self.inner.publish_session(&session);
            Ok(())
        } else {
            self.submit(&session.id, text)
        }
    }

    /// Everything waiting behind the running turn: the pending steer first, then the queue in run
    /// order. `target` is the stable handle the management calls take — `"steer"` or `"q<index>"`.
    pub fn pending_list(&self, session: &SessionId) -> Result<Vec<PendingMessage>> {
        let Some(session) = self.inner.session(session) else {
            return Err(RuntimeError::SessionNotFound(session.clone()));
        };
        let mut items = Vec::new();
        if let Some(text) = session.pending_steer() {
            items.push(PendingMessage {
                target: "steer".into(),
                steer: true,
                text,
            });
        }
        for (index, text) in session.queue_items().into_iter().enumerate() {
            items.push(PendingMessage {
                target: format!("q{index}"),
                steer: false,
                text,
            });
        }
        Ok(items)
    }

    /// Rewrite a pending message. `target` is `"steer"` or `"q<index>"` from [`Self::pending_list`].
    pub fn pending_update(&self, session: &SessionId, target: &str, text: String) -> Result<()> {
        let session = self.pending_session(session)?;
        let ok = match pending_index(target) {
            Some(PendingTarget::Steer) => {
                session.steer(text);
                true
            }
            Some(PendingTarget::Queue(index)) => session.update_queue(index, text),
            None => false,
        };
        if ok {
            self.inner.publish_session(&session);
            Ok(())
        } else {
            Err(RuntimeError::InvalidState(format!(
                "no pending message `{target}`"
            )))
        }
    }

    /// Drop a pending message without running it.
    pub fn pending_remove(&self, session: &SessionId, target: &str) -> Result<()> {
        let session = self.pending_session(session)?;
        let ok = match pending_index(target) {
            Some(PendingTarget::Steer) => {
                session.clear_steer();
                true
            }
            Some(PendingTarget::Queue(index)) => session.remove_queue(index).is_some(),
            None => false,
        };
        if ok {
            self.inner.publish_session(&session);
            Ok(())
        } else {
            Err(RuntimeError::InvalidState(format!(
                "no pending message `{target}`"
            )))
        }
    }

    /// Reorder a queued message by `delta` positions; the steer row does not move.
    pub fn pending_move(&self, session: &SessionId, target: &str, delta: isize) -> Result<()> {
        let session = self.pending_session(session)?;
        let ok = match pending_index(target) {
            Some(PendingTarget::Queue(index)) => {
                let to = index as isize + delta;
                session.move_queue(index, to.max(0) as usize)
            }
            _ => false,
        };
        if ok {
            self.inner.publish_session(&session);
            Ok(())
        } else {
            Err(RuntimeError::InvalidState(format!(
                "cannot move `{target}`"
            )))
        }
    }

    /// Flip a message between steer and queue: the steer demotes to the back of the queue, a queued
    /// message promotes to steer (any steer already waiting drops back into the queue first).
    pub fn pending_flip(&self, session: &SessionId, target: &str) -> Result<()> {
        let session = self.pending_session(session)?;
        let ok = match pending_index(target) {
            Some(PendingTarget::Steer) => {
                session.steer_to_queue();
                true
            }
            Some(PendingTarget::Queue(index)) => session.queue_to_steer(index),
            None => false,
        };
        if ok {
            self.inner.publish_session(&session);
            Ok(())
        } else {
            Err(RuntimeError::InvalidState(format!(
                "cannot flip `{target}`"
            )))
        }
    }

    fn pending_session(&self, id: &SessionId) -> Result<Arc<SessionRuntime>> {
        self.inner
            .session(id)
            .ok_or_else(|| RuntimeError::SessionNotFound(id.clone()))
    }

    pub fn cancel(&self, session: &SessionId) -> Result<()> {
        let Some(session) = self.inner.session(session) else {
            return Err(RuntimeError::SessionNotFound(session.clone()));
        };
        session.cancel_handle().cancel();
        self.inner.emit(RuntimeEvent::Notice {
            level: NoticeLevel::Warn,
            text: "cancelling".into(),
        });
        Ok(())
    }

    /// Fold this conversation now. `/compact`.
    pub fn compact_now(&self, session: &SessionId) -> Result<crate::compaction::CompactionReport> {
        if self.inner.session(session).is_none() {
            return Err(RuntimeError::SessionNotFound(session.clone()));
        }
        self.inner
            .compact_now(session)
            .ok_or_else(|| RuntimeError::InvalidState("nothing to fold: the span is shorter than the minimum this policy allows, or an earlier fold already covered it".into()))
            .inspect(|_| self.inner.publish_metrics())
    }

    pub fn create_session(&self, title: Option<String>) -> Result<SessionId> {
        let session = self.inner.create_session(title)?;
        self.inner.publish_metrics();
        Ok(session.id.clone())
    }

    /// Name a session by hand. This is the operator's own title and it is permanent: provenance
    /// moves to `manual`, and automatic naming is barred from this session from here on — including
    /// after a restart, because the flag is stored with the row. An empty title is refused rather
    /// than silently reverting to a placeholder.
    pub fn rename_session(&self, session: &SessionId, title: String) -> Result<()> {
        let Some(session) = self.inner.session(session) else {
            return Err(RuntimeError::SessionNotFound(session.clone()));
        };
        let title = titling::clip(title.trim(), titling::MAX_TITLE_CHARS);
        if title.chars().filter(|c| !c.is_whitespace()).count() < 2 {
            return Err(RuntimeError::InvalidState(
                "a session title needs at least two characters".into(),
            ));
        }
        session.set_title(&title, now_ms());
        session.set_title_source(vikram_store::TITLE_MANUAL);
        session.set_title_status(vikram_store::TITLE_READY);
        self.inner
            .store
            .rename_session(&session.id, &title, now_ms())
            .map_err(|e| RuntimeError::Store(e.to_string()))?;
        self.inner.publish_session(&session);
        Ok(())
    }

    /// Throw away the current name and derive a fresh one from the conversation. `/retitle`.
    ///
    /// `force` is what makes this usable on a session the operator renamed earlier: it clears the
    /// `manual` bar, because asking for a regenerated title *is* the consent to let naming write
    /// again. Without it this only works on sessions that never got a name.
    pub fn retitle_session(&self, session: &SessionId, force: bool) -> Result<()> {
        let Some(session) = self.inner.session(session) else {
            return Err(RuntimeError::SessionNotFound(session.clone()));
        };
        if !force && !session.accepts_derived_title() {
            return Err(RuntimeError::InvalidState(
                "this title was set by hand; renaming is how you change it".into(),
            ));
        }
        let Some(seed) = titling::seed(&session.messages()) else {
            return Err(RuntimeError::InvalidState(
                "nothing to name this session after yet".into(),
            ));
        };
        session.set_title_source(vikram_store::TITLE_GENERIC);
        session.set_title_status(vikram_store::TITLE_PENDING);
        session.release_titling();
        self.inner
            .store
            .mark_session_title_pending(&session.id, vikram_store::TITLE_GENERIC)
            .map_err(|e| RuntimeError::Store(e.to_string()))?;
        self.inner.publish_session(&session);
        session.claim_titling();
        self.inner.spawn_titling(session, seed);
        Ok(())
    }

    /// Close a tab. The session stops running and leaves the live set, but nothing is deleted: the
    /// tab was a view onto a transcript, and the transcript lives in the store. A closed session
    /// comes back whole through [`Self::open_session`]. Only [`Self::delete_session`] destroys one.
    pub fn close_session(&self, session: &SessionId) -> Result<()> {
        let Some(session) = self.inner.session(session) else {
            return Err(RuntimeError::SessionNotFound(session.clone()));
        };
        // Stage: session_lifecycle — close observes while the session is still live; the
        // transcript stays in the store either way, so a refusal cannot veto this.
        let _ = dispatch_lifecycle_stage(
            &self.inner,
            crate::pipeline::StagePoint::SessionLifecycle,
            &session,
            &session.title(),
            crate::pipeline::StagePayload::SessionLifecycle(
                crate::pipeline::SessionLifecyclePayload {
                    op: crate::pipeline::SessionLifecycleOp::Close,
                    session: session.id.clone(),
                    title: session.title(),
                },
            ),
            false,
        );
        session.cancel_handle().cancel();
        self.inner
            .sessions
            .lock()
            .expect("sessions lock")
            .retain(|s| s.id != session.id);
        // The close has to reach the store, or it only lasts until the process exits: startup
        // rehydrates every row that is still `active`, so a tab the operator dismissed came back on
        // the next launch. Nothing is deleted here — the row and its transcript stay, and
        // `open_session` flips it back to `active` when it is reopened.
        self.inner
            .store
            .set_session_status(&session.id, "closed", now_ms())
            .map_err(|e| RuntimeError::Store(e.to_string()))?;
        self.inner
            .emit(RuntimeEvent::SessionClosed(session.id.clone()));
        self.inner.dispatch_hook_journaled(
            &session.id,
            &crate::hooks::HookContext::new(crate::hooks::HookPoint::SessionEnd)
                .with_session(session.id.as_str())
                .with_workspace(self.inner.workspace_root.display().to_string())
                .with_model(self.inner.model())
                .with_text(session.title())
                .with_details(serde_json::json!({"title": session.title()})),
        );
        self.inner.memory_providers().on_session_end(
            session
                .messages()
                .iter()
                .filter_map(|m| serde_json::to_value(m).ok())
                .collect(),
        );
        self.inner.publish_metrics();
        Ok(())
    }

    /// Destroy a session and everything journalled against it. Deliberately separate from closing a
    /// tab, so that the destructive path cannot be reached by clicking the wrong cell.
    pub fn delete_session(&self, session: &SessionId) -> Result<()> {
        if let Some(live) = self.inner.session(session) {
            live.cancel_handle().cancel();
        }
        let title = self
            .inner
            .session(session)
            .map(|live| live.title())
            .or_else(|| {
                self.inner
                    .store
                    .get_session(session)
                    .ok()
                    .flatten()
                    .map(|row| row.title)
            })
            .unwrap_or_default();
        let ended_messages = self
            .inner
            .session(session)
            .map(|live| live.messages())
            .unwrap_or_default();
        // Stage: session_lifecycle — delete observes before the store row goes. A stored-only
        // session gets a runtime built from its row for the context; it is never pushed live.
        let lifecycle_session = self.inner.session(session).or_else(|| {
            self.inner
                .store
                .get_session(session)
                .ok()
                .flatten()
                .map(|row| Arc::new(SessionRuntime::from_row(row)))
        });
        if let Some(lifecycle_session) = lifecycle_session {
            let _ = dispatch_lifecycle_stage(
                &self.inner,
                crate::pipeline::StagePoint::SessionLifecycle,
                &lifecycle_session,
                &lifecycle_session.title(),
                crate::pipeline::StagePayload::SessionLifecycle(
                    crate::pipeline::SessionLifecyclePayload {
                        op: crate::pipeline::SessionLifecycleOp::Delete,
                        session: session.clone(),
                        title: lifecycle_session.title(),
                    },
                ),
                false,
            );
        }
        self.inner
            .store
            .delete_session(session)
            .map_err(|e| RuntimeError::Store(e.to_string()))?;
        self.inner
            .sessions
            .lock()
            .expect("sessions lock")
            .retain(|s| &s.id != session);
        self.inner
            .emit(RuntimeEvent::SessionClosed(session.clone()));
        self.inner.dispatch_hook_journaled(
            session,
            &crate::hooks::HookContext::new(crate::hooks::HookPoint::SessionEnd)
                .with_session(session.as_str())
                .with_workspace(self.inner.workspace_root.display().to_string())
                .with_model(self.inner.model())
                .with_text(&title)
                .with_details(serde_json::json!({"title": title, "deleted": true})),
        );
        self.inner.memory_providers().on_session_end(
            ended_messages
                .iter()
                .filter_map(|m| serde_json::to_value(m).ok())
                .collect(),
        );
        self.inner.publish_metrics();
        Ok(())
    }

    /// Bring a stored session back into the live set with its transcript replayed. The same work the
    /// startup pass does for every row, done for one: build the runtime from the row, replay the
    /// journal, then announce it so any open view can pick it up.
    pub fn open_session(&self, session: &SessionId) -> Result<SessionId> {
        if let Some(live) = self.inner.session(session) {
            return Ok(live.id.clone());
        }
        let row = self
            .inner
            .store
            .get_session(session)
            .map_err(|e| RuntimeError::Store(e.to_string()))?
            .ok_or_else(|| RuntimeError::SessionNotFound(session.clone()))?;
        // Reopening a dismissed tab puts it back on the active list, so the next launch and the CLI
        // listings see it again instead of treating it as still closed.
        self.inner
            .store
            .set_session_status(&row.id, "active", now_ms())
            .map_err(|e| RuntimeError::Store(e.to_string()))?;
        let live = Arc::new(SessionRuntime::from_row(row));
        if let Ok(Some((parent, depth))) = self.inner.store.session_lineage(&live.id) {
            live.set_parent(Some(parent));
            live.set_depth(depth);
        }
        let records = match self.inner.store.replay_messages(&live.id) {
            Ok(records) => records,
            Err(err) => {
                self.inner.emit(RuntimeEvent::Notice {
                    level: NoticeLevel::Warn,
                    text: format!("could not replay the transcript: {err}"),
                });
                Vec::new()
            }
        };
        if !records.is_empty() {
            let messages = crate::compaction::project_from_journal(
                records
                    .into_iter()
                    .map(|record| (record.seq, record.kind, record.payload)),
            );
            *live.messages.lock().unwrap_or_else(|e| {
                eprintln!("vikram: runtime messages lock mutex poisoned; recovering");
                e.into_inner()
            }) = messages;
        }
        self.inner
            .sessions
            .lock()
            .expect("sessions lock")
            .push(Arc::clone(&live));
        self.inner
            .memory_providers()
            .initialize_all(live.id.as_str());
        self.inner
            .emit(RuntimeEvent::SessionUpdated(live.snapshot()));
        // Stage: session_lifecycle — open observes the session now live; nothing left to veto.
        let _ = dispatch_lifecycle_stage(
            &self.inner,
            crate::pipeline::StagePoint::SessionLifecycle,
            &live,
            &live.title(),
            crate::pipeline::StagePayload::SessionLifecycle(
                crate::pipeline::SessionLifecyclePayload {
                    op: crate::pipeline::SessionLifecycleOp::Open,
                    session: live.id.clone(),
                    title: live.title(),
                },
            ),
            false,
        );
        self.inner.publish_metrics();
        Ok(live.id.clone())
    }

    /// Every session the store knows, live ones first. Built from both halves because a closed tab is
    /// not a deleted session: the live set says what is running, the store says what exists.
    pub fn session_catalogue(&self) -> Vec<SessionSummary> {
        let live = self.inner.session_list();
        let mut rows = Vec::with_capacity(live.len());
        for session in &live {
            rows.push(SessionSummary {
                id: session.id.clone(),
                title: session.title.lock().map(|t| t.clone()).unwrap_or_default(),
                model: session.model.lock().ok().and_then(|m| m.clone()),
                messages: session.messages.lock().map(|m| m.len()).unwrap_or(0),
                open: true,
                updated_ms: session.updated_ms.load(Ordering::Relaxed),
            });
        }
        if let Ok(stored) = self.inner.store.browse_sessions() {
            for (row, count) in stored {
                if rows.iter().any(|s| s.id == row.id) {
                    continue;
                }
                rows.push(SessionSummary {
                    id: row.id,
                    title: row.title,
                    model: row.model,
                    messages: count as usize,
                    open: false,
                    updated_ms: row.updated_ms,
                });
            }
        }
        rows
    }

    /// Fork a session: a new session that continues from the source's transcript. The journal
    /// is copied, the source is recorded as the fork's parent, and the new session is loaded
    /// live so the operator can keep talking to it immediately.
    pub fn fork_session(&self, session: &SessionId) -> Result<SessionId> {
        if self.inner.session(session).is_none() {
            return Err(RuntimeError::SessionNotFound(session.clone()));
        }
        let fork = self
            .inner
            .store
            .fork_session(session, now_ms())
            .map_err(|e| RuntimeError::Store(e.to_string()))?
            .ok_or_else(|| RuntimeError::SessionNotFound(session.clone()))?;
        // Load the fork live, with the same transcript a restart would replay for it.
        let row = self
            .inner
            .store
            .get_session(&fork.id)
            .map_err(|e| RuntimeError::Store(e.to_string()))?
            .ok_or_else(|| RuntimeError::SessionNotFound(fork.id.clone()))?;
        let live = Arc::new(SessionRuntime::from_row(row));
        let records = match self.inner.store.replay_messages(&live.id) {
            Ok(records) => records,
            Err(err) => {
                self.inner.emit(RuntimeEvent::Notice {
                    level: NoticeLevel::Warn,
                    text: format!("could not replay the transcript: {err}"),
                });
                Vec::new()
            }
        };
        if !records.is_empty() {
            let messages = crate::compaction::project_from_journal(
                records
                    .into_iter()
                    .map(|record| (record.seq, record.kind, record.payload)),
            );
            *live.messages.lock().unwrap_or_else(|e| {
                eprintln!("vikram: runtime messages lock mutex poisoned; recovering");
                e.into_inner()
            }) = messages;
        }
        self.inner
            .sessions
            .lock()
            .expect("sessions lock")
            .push(Arc::clone(&live));
        self.inner
            .emit(RuntimeEvent::SessionCreated(live.snapshot()));
        // Stage: session_lifecycle — a fork is a create the operator named differently; the
        // stage observes it like `open`, with no veto.
        let _ = dispatch_lifecycle_stage(
            &self.inner,
            crate::pipeline::StagePoint::SessionLifecycle,
            &live,
            &live.title(),
            crate::pipeline::StagePayload::SessionLifecycle(
                crate::pipeline::SessionLifecyclePayload {
                    op: crate::pipeline::SessionLifecycleOp::Fork,
                    session: live.id.clone(),
                    title: live.title(),
                },
            ),
            false,
        );
        self.inner.publish_metrics();
        Ok(live.id.clone())
    }

    /// Export a session as text: JSON for machines, Markdown for people. Reads the store, so
    /// a session this process is not holding can still be exported exactly.
    pub fn export_session(
        &self,
        session: &SessionId,
        format: vikram_store::ExportFormat,
    ) -> Result<String> {
        self.inner
            .store
            .export_session_text(session, format, now_ms())
            .map_err(|e| RuntimeError::Store(e.to_string()))?
            .ok_or_else(|| RuntimeError::SessionNotFound(session.clone()))
    }

    /// Sessions whose journal ends mid-work, as the store reports them.
    pub fn interrupted_sessions(&self) -> Result<Vec<vikram_store::InterruptedSession>> {
        self.inner
            .store
            .list_interrupted_sessions()
            .map_err(|e| RuntimeError::Store(e.to_string()))
    }

    /// Recover one interrupted session: complete its pending tool calls in the journal so the
    /// transcript is whole again. Returns the calls that were answered.
    pub fn recover_session(
        &self,
        session: &SessionId,
    ) -> Result<Vec<vikram_contracts::ToolCallId>> {
        let answered = self
            .inner
            .store
            .recover_interrupted_session(session, now_ms())
            .map_err(|e| RuntimeError::Store(e.to_string()))?;
        // Rebuild the live transcript from the journal if this session is loaded, so the
        // recovery is visible immediately rather than on the next restart.
        if let Some(live) = self.inner.session(session) {
            let records = match self.inner.store.replay_messages(&live.id) {
                Ok(records) => records,
                Err(err) => {
                    self.inner.emit(RuntimeEvent::Notice {
                        level: NoticeLevel::Warn,
                        text: format!("could not replay the transcript: {err}"),
                    });
                    Vec::new()
                }
            };
            if records.is_empty() {
                return Ok(answered);
            }
            let messages = crate::compaction::project_from_journal(
                records
                    .into_iter()
                    .map(|record| (record.seq, record.kind, record.payload)),
            );
            *live.messages.lock().unwrap_or_else(|e| {
                eprintln!("vikram: runtime messages lock mutex poisoned; recovering");
                e.into_inner()
            }) = messages;
            self.inner.publish_session(&live);
        }
        Ok(answered)
    }

    pub fn spawn_agent(&self, profile: BotId, task: String, session: SessionId) -> Result<()> {
        let Some(agent) = self.inner.bots.get(&profile) else {
            return Err(RuntimeError::AgentNotFound(profile.to_string()));
        };
        let Some(parent) = self.inner.session(&session) else {
            return Err(RuntimeError::SessionNotFound(session));
        };
        // The floor already refuses a Delegate *action* past the limit; this refuses the spawn
        // itself, so a deep chain cannot be built by a path that skipped the gate.
        let depth = parent.depth() + 1;
        let limit = self.inner.policy().max_delegation_depth();
        if depth > limit {
            return Err(RuntimeError::InvalidState(format!(
                "delegation depth {depth} exceeds the limit of {limit}"
            )));
        }
        // One run per bot. A second spawn while the first is still working would replace the
        // session, task and cancel token underneath it, and both turns would answer as the same
        // bot. Everything below is synchronous, so this check cannot be raced mid-spawn.
        let running = agent.state().is_active()
            || agent.is_paused()
            || agent
                .session
                .lock()
                .unwrap_or_else(|e| {
                    eprintln!("vikram: bot session lock mutex poisoned; recovering");
                    e.into_inner()
                })
                .clone()
                .and_then(|id| self.inner.session(&id))
                .is_some_and(|session| session.is_busy());
        if running {
            return Err(RuntimeError::InvalidState(format!(
                "{} already has a run in flight",
                agent.profile.name
            )));
        }
        // A delegation hook may refuse the spawn before a session exists for it; everything above
        // is a check that costs nothing, so a denial here is cheap and clean.
        let delegation = self.inner.dispatch_hook_journaled(
            &parent.id,
            &crate::hooks::HookContext::new(crate::hooks::HookPoint::DelegationStart)
                .with_session(parent.id.as_str())
                .with_workspace(self.inner.workspace_root.display().to_string())
                .with_model(self.inner.model())
                .with_text(task.as_str())
                .with_details(serde_json::json!({
                    "bot": profile.to_string(),
                    "task": task.as_str(),
                    "depth": depth,
                })),
        );
        if let Some(reason) = &delegation.effect.deny {
            return Err(RuntimeError::InvalidState(format!(
                "a plugin refused this delegation: {reason}"
            )));
        }
        // A delegated session is named for the agent that owns it and the work it was given, so the
        // tab reads "Arun — Security Review". The identity stays in the label because two bots
        // working on the same task must never look like one session.
        let subject = titling::sanitize(&titling::fallback(&task))
            .unwrap_or_else(|| titling::clip(&task, 28));
        let title = format!("{} — {}", agent.profile.name, subject);
        let agent_session = self.inner.create_session(Some(title.clone()))?;
        agent_session.set_bot(Some(profile.clone()), now_ms());
        agent_session.set_parent(Some(parent.id.clone()));
        agent_session.set_depth(depth);
        let _ = self
            .inner
            .store
            .set_session_bot(&agent_session.id, Some(&profile), now_ms());
        self.inner
            .store
            .set_session_lineage(&agent_session.id, Some(&parent.id), depth)?;
        // What crosses the boundary is a brief, not the parent's transcript: the live instruction
        // plus the decisions, files and open errors, inside the delegation budget.
        let brief = crate::compaction::delegation_brief(
            &agent_session.estimator(),
            &parent.messages(),
            self.inner
                .config
                .read()
                .unwrap_or_else(|e| e.into_inner())
                .context
                .delegation_budget_tokens,
        );
        if let Some(brief) = brief {
            let message = vikram_contracts::Message::text(
                vikram_contracts::Role::System,
                brief,
                vikram_contracts::Trust::Host,
            );
            agent_session.push_message(message.clone(), now_ms());
            self.inner.journal(
                &agent_session.id,
                RecordKind::Message,
                serde_json::json!({"message": message}),
            );
        }
        agent.set_task(Some(task.clone()));
        agent.set_state(BotState::Thinking);
        agent.set_session(Some(agent_session.id.clone()));
        *agent.paused.lock().unwrap_or_else(|e| {
            eprintln!("vikram: runtime pause mutex poisoned; recovering");
            e.into_inner()
        }) = false;
        *agent.cancel.lock().unwrap_or_else(|e| {
            eprintln!("vikram: runtime cancel mutex poisoned; recovering");
            e.into_inner()
        }) = vikram_contracts::CancellationToken::new();
        let bg = self.inner.tasks.start(
            format!("bot: {}", agent.profile.name),
            agent_session.id.clone(),
        );
        self.inner.publish_bot(&agent);
        self.inner.task_snapshot(&bg);
        self.inner
            .emit(RuntimeEvent::SessionCreated(agent_session.snapshot()));
        // The delegation belongs to the session that asked for it: without this row the parent's
        // transcript shows a child tab appear and an answer arrive with nothing between.
        self.inner.emit_activity(
            &parent.id,
            crate::types::ActivityKind::Delegated,
            title.clone(),
            Some(profile.clone()),
            parent.bot(),
            true,
        );
        self.inner.publish_metrics();

        // Stage: agent_lifecycle — the spawn is already durable (session, lineage, bot state);
        // the stage observes it as the delegated turn is about to start.
        let _ = dispatch_lifecycle_stage(
            &self.inner,
            crate::pipeline::StagePoint::AgentLifecycle,
            &agent_session,
            &task,
            crate::pipeline::StagePayload::AgentLifecycle(crate::pipeline::AgentLifecyclePayload {
                op: crate::pipeline::AgentLifecycleOp::Spawn,
                agent: Some(profile.clone()),
                parent: parent.id.clone(),
                session: agent_session.id.clone(),
                task: task.clone(),
                delivered: false,
            }),
            false,
        );

        crate::turn::spawn_turn(
            Arc::clone(&self.inner),
            agent_session,
            Message::user(task),
            Some(crate::turn::AgentRun {
                agent,
                task_id: bg.id.clone(),
            }),
        );
        Ok(())
    }

    pub fn control_bot(&self, id: &BotId, control: BotControl) -> Result<()> {
        let Some(agent) = self.inner.bots.get(id) else {
            return Err(RuntimeError::AgentNotFound(id.to_string()));
        };
        match control {
            BotControl::Pause => {
                *agent.paused.lock().unwrap_or_else(|e| {
                    eprintln!("vikram: runtime pause mutex poisoned; recovering");
                    e.into_inner()
                }) = true;
                agent.set_state(BotState::Paused);
                self.inner.emit(RuntimeEvent::Notice {
                    level: NoticeLevel::Info,
                    text: format!("{} paused", agent.profile.name),
                });
            }
            BotControl::Resume => {
                *agent.paused.lock().unwrap_or_else(|e| {
                    eprintln!("vikram: runtime pause mutex poisoned; recovering");
                    e.into_inner()
                }) = false;
                if agent.state() == BotState::Paused {
                    agent.set_state(BotState::Running);
                }
            }
            BotControl::Stop => {
                agent.cancel_handle().cancel();
                // The turn snapshots the *session's* token, not the bot's, so cancelling only
                // the bot token stopped nothing. Cancel the session the run is actually in.
                let session_id = agent
                    .session
                    .lock()
                    .unwrap_or_else(|e| {
                        eprintln!("vikram: bot session lock mutex poisoned; recovering");
                        e.into_inner()
                    })
                    .clone();
                if let Some(session) = session_id.and_then(|id| self.inner.session(&id)) {
                    session.cancel_handle().cancel();
                }
                agent.set_state(BotState::Stopped);
                self.inner.emit(RuntimeEvent::Notice {
                    level: NoticeLevel::Warn,
                    text: format!("{} stopped", agent.profile.name),
                });
            }
        }
        self.inner.publish_bot(&agent);
        self.inner.publish_metrics();
        Ok(())
    }

    pub fn answer_approval(&self, approval: &ApprovalId, allow: bool, persist: bool) -> Result<()> {
        let pending = self
            .inner
            .approvals
            .lock()
            .expect("approvals lock")
            .remove(approval);
        let Some(pending) = pending else {
            return Err(RuntimeError::InvalidState(
                "approval is no longer pending".into(),
            ));
        };
        if persist {
            // Keep exactly the scope the prompt proposed. Nothing here widens a permission: the
            // saved rule is an operator-tier rule, still under the mandatory floor. Denying this
            // call while saving the rule is a real answer: settle this one as a refusal, and stop
            // asking about the same shape of request next time.
            match pending.prompt.amendment.as_deref() {
                Some(amendment) => self.allow(&pending.prompt.session, amendment)?,
                None => self.inner.emit(RuntimeEvent::Notice {
                    level: NoticeLevel::Warn,
                    text: format!(
                        "this approval cannot be saved: `{}` has no scope proposal",
                        pending.prompt.verb
                    ),
                }),
            }
        }
        let _ = pending.respond.send(allow);
        self.inner.emit(RuntimeEvent::ApprovalResolved {
            approval: approval.clone(),
            allowed: allow,
        });
        Ok(())
    }

    /// Answer a pending `ask_user` question. The sender is removed before it is used, so a request
    /// can be answered exactly once; the waiting turn then resumes with the answer.
    pub fn answer_question(&self, request_id: &str, answer: String) -> Result<()> {
        let sender = self
            .inner
            .questions
            .lock()
            .expect("questions lock")
            .remove(request_id);
        let Some(sender) = sender else {
            return Err(RuntimeError::InvalidState(
                "question is no longer pending".into(),
            ));
        };
        let _ = sender.send(answer.clone());
        self.inner.emit(RuntimeEvent::UserAnswerResolved {
            request_id: request_id.to_owned(),
            answer,
        });
        Ok(())
    }

    /// Abandon a pending `ask_user` question. Dropping the sender resolves the turn's wait as a
    /// cancellation, so the call fails honestly instead of blocking on an answer nobody will give.
    pub fn dismiss_question(&self, request_id: &str) {
        self.inner.close_question(request_id);
        self.inner.emit(RuntimeEvent::Notice {
            level: NoticeLevel::Warn,
            text: "question dismissed".into(),
        });
    }

    pub fn bot_snapshots(&self) -> Vec<BotSnapshot> {
        self.inner.bots.snapshots()
    }

    /// Create a bot profile, persist it, and register it with the manager — the same path the
    /// store loader takes on startup, so a bot made here survives a restart.
    pub fn bot_create(&self, name: &str, role: &str, description: &str) -> Result<BotId> {
        let row = vikram_store::BotRow {
            id: BotId::generate(),
            name: name.to_owned(),
            role: role.to_owned(),
            description: description.to_owned(),
            icon: None,
            created_ms: now_ms(),
            soul: String::new(),
        };
        self.inner.store.save_bot(&row)?;
        let bot = Arc::new(ManagedBot::from_profile(row.into()));
        self.inner.bots.add(bot.clone());
        self.inner.publish_bot(&bot);
        Ok(bot.profile.id.clone())
    }

    /// Rename a bot profile. The store row is the source of truth; the live `ManagedBot` is
    /// rebuilt from it, which resets transient runtime state (task, progress, unread) — the
    /// profile itself, including its id, is preserved.
    pub fn bot_rename(&self, id: &BotId, name: &str) -> Result<()> {
        let name = name.trim();
        if name.is_empty() {
            return Err(RuntimeError::InvalidState("a bot needs a name".into()));
        }
        let rows = self.inner.store.list_bots()?;
        let Some(mut row) = rows.into_iter().find(|row| row.id == *id) else {
            return Err(RuntimeError::AgentNotFound(id.to_string()));
        };
        row.name = name.to_owned();
        self.inner.store.save_bot(&row)?;
        self.inner.bots.remove(id);
        let bot = Arc::new(ManagedBot::from_profile(row.into()));
        self.inner.bots.add(bot.clone());
        self.inner.publish_bot(&bot);
        Ok(())
    }

    /// The conversation a bot owns.
    ///
    /// Pressing a bot's row means "talk to it", so the row asks for a chat and gets one: the live
    /// session when the bot is running, the newest stored session it already worked in, and — for a
    /// bot that has never run — a fresh session named after it. The old path opened a spawn prompt
    /// instead, which turned "open my chat" into "answer a question first".
    pub fn bot_chat(&self, id: &BotId) -> Result<SessionId> {
        let Some(bot) = self.inner.bots.get(id) else {
            return Err(RuntimeError::AgentNotFound(id.to_string()));
        };
        let name = bot.profile.name.clone();
        let live = {
            let guard = bot.session.lock().unwrap_or_else(|e| {
                eprintln!("vikram: bot session lock mutex poisoned; recovering");
                e.into_inner()
            });
            guard.clone()
        };
        if let Some(session) = live {
            return self.open_session(&session);
        }
        if let Some(session) = self
            .inner
            .store
            .session_for_bot(id)
            .map_err(|e| RuntimeError::Store(e.to_string()))?
        {
            let opened = self.open_session(&session)?;
            *bot.session.lock().unwrap_or_else(|e| {
                eprintln!("vikram: bot session lock mutex poisoned; recovering");
                e.into_inner()
            }) = Some(opened.clone());
            return Ok(opened);
        }
        let session = self.create_session(Some(name))?;
        self.inner
            .store
            .set_session_bot(&session, Some(id), now_ms())
            .map_err(|e| RuntimeError::Store(e.to_string()))?;
        if let Some(live) = self.inner.session(&session) {
            live.set_bot(Some(id.clone()), now_ms());
        }
        *bot.session.lock().unwrap_or_else(|e| {
            eprintln!("vikram: bot session lock mutex poisoned; recovering");
            e.into_inner()
        }) = Some(session.clone());
        Ok(session)
    }

    /// Start a fresh conversation with a bot, leaving the older ones where they are.
    ///
    /// Opening a bot gives back the conversation it already has, because continuity is the point of
    /// a bot. But a conversation can go stale or start badly, and the operator then needs a clean
    /// page without deleting history first. This makes one and points the bot at it.
    pub fn bot_new_chat(&self, id: &BotId) -> Result<SessionId> {
        let Some(bot) = self.inner.bots.get(id) else {
            return Err(RuntimeError::AgentNotFound(id.to_string()));
        };
        let name = bot.profile.name.clone();
        let session = self.create_session(Some(name))?;
        self.inner
            .store
            .set_session_bot(&session, Some(id), now_ms())
            .map_err(|e| RuntimeError::Store(e.to_string()))?;
        if let Some(live) = self.inner.session(&session) {
            live.set_bot(Some(id.clone()), now_ms());
        }
        *bot.session.lock().unwrap_or_else(|e| {
            eprintln!("vikram: bot session lock mutex poisoned; recovering");
            e.into_inner()
        }) = Some(session.clone());
        Ok(session)
    }

    /// Set the tags a bot wears, comma separated, or clear them with an empty string.
    ///
    /// Tags are the operator's words for what the bot is, so nothing is invented here: `assistant`
    /// used to be written into every new profile whether or not anyone asked for it. The list is
    /// normalised on the way in — split, trimmed, duplicates dropped, joined with `", "` — so the
    /// store never holds `coder,,coder ` for the panel to render as three chips.
    pub fn bot_set_role(&self, id: &BotId, role: &str) -> Result<()> {
        let mut tags: Vec<&str> = Vec::new();
        for tag in role.split(',') {
            let tag = tag.trim();
            if tag.is_empty() || tags.iter().any(|seen| seen.eq_ignore_ascii_case(tag)) {
                continue;
            }
            tags.push(tag);
        }
        let role = tags.join(", ");
        let rows = self.inner.store.list_bots()?;
        let Some(mut row) = rows.into_iter().find(|row| row.id == *id) else {
            return Err(RuntimeError::AgentNotFound(id.to_string()));
        };
        row.role = role;
        self.inner.store.save_bot(&row)?;
        self.inner.bots.remove(id);
        let bot = Arc::new(ManagedBot::from_profile(row.into()));
        self.inner.bots.add(bot.clone());
        self.inner.publish_bot(&bot);
        Ok(())
    }

    /// Replace the standing instructions a bot carries — its `soul.md`.
    ///
    /// The text is stored verbatim (trimmed) and goes into the bot's system preamble on every turn
    /// in its own session, so the bot behaves the way the operator asked long after the message that
    /// asked for it has scrolled away. An empty string clears them: nothing is assumed.
    pub fn bot_set_soul(&self, id: &BotId, soul: &str) -> Result<()> {
        let rows = self.inner.store.list_bots()?;
        let Some(mut row) = rows.into_iter().find(|row| row.id == *id) else {
            return Err(RuntimeError::AgentNotFound(id.to_string()));
        };
        row.soul = soul.trim().to_owned();
        self.inner.store.save_bot(&row)?;
        self.inner.bots.remove(id);
        let bot = Arc::new(ManagedBot::from_profile(row.into()));
        self.inner.bots.add(bot.clone());
        self.inner.publish_bot(&bot);
        Ok(())
    }

    /// Delete a bot profile and drop its managed instance.
    pub fn bot_delete(&self, id: &BotId) -> Result<()> {
        self.inner.store.delete_bot(id)?;
        self.inner.bots.remove(id);
        Ok(())
    }

    /// Installed plugins as `(id, name, state)` for pickers. State is `loaded`, `disabled` or
    /// `error` — the same labels the `vikram plugins` CLI prints.
    pub fn plugin_states(&self) -> Vec<(String, String, String)> {
        let registry = self.inner.plugins.lock().unwrap_or_else(|e| {
            eprintln!("vikram: plugins lock mutex poisoned; recovering");
            e.into_inner()
        });
        registry
            .plugins()
            .iter()
            .map(|plugin| {
                let state = if let Some(err) = &plugin.error {
                    format!("error: {err}")
                } else if plugin.loaded {
                    "loaded".to_owned()
                } else {
                    "disabled".to_owned()
                };
                (plugin.id(), plugin.name.clone(), state)
            })
            .collect()
    }

    fn plugin_mutate(
        &self,
        op: impl FnOnce(&mut PluginRegistry) -> std::result::Result<(), String>,
    ) -> Result<()> {
        let mut registry = self.inner.plugins.lock().unwrap_or_else(|e| {
            eprintln!("vikram: plugins lock mutex poisoned; recovering");
            e.into_inner()
        });
        op(&mut registry).map_err(RuntimeError::Store)?;
        let installed = registry.installed();
        let loaded = registry.plugins().iter().filter(|p| p.loaded).count();
        drop(registry);
        self.reload_hooks();
        self.inner
            .emit(RuntimeEvent::PluginsChanged { installed, loaded });
        Ok(())
    }

    /// Rebuild the hook chain from the loaded plugins.
    ///
    /// Called after every install/uninstall/enable/disable, so a disabled plugin's hooks stop
    /// running on the next dispatch rather than at the next restart.
    fn reload_hooks(&self) {
        let plugins = self.inner.plugins.lock().unwrap_or_else(|e| {
            eprintln!("vikram: plugins lock mutex poisoned; recovering");
            e.into_inner()
        });
        let fresh = crate::hooks::HookRegistry::from_plugins(&plugins);
        let active = fresh.hooks().len();
        let rejected = fresh.rejected().to_vec();
        drop(plugins);
        *self.inner.hooks.write().unwrap_or_else(|e| {
            eprintln!("vikram: hooks lock mutex poisoned; recovering");
            e.into_inner()
        }) = fresh;
        for rejection in rejected {
            self.inner.emit(RuntimeEvent::Notice {
                level: NoticeLevel::Warn,
                text: format!(
                    "plugin `{}` hook refused: {}",
                    rejection.plugin, rejection.reason
                ),
            });
        }
        // Plugin stage handlers are rebuilt on the same trigger, so a disabled plugin's stage
        // stops dispatching here rather than at the next restart.
        for rejection in self.inner.reload_plugin_stages() {
            self.inner.emit(RuntimeEvent::Notice {
                level: NoticeLevel::Warn,
                text: format!(
                    "plugin `{}` stage refused: {}",
                    rejection.plugin, rejection.reason
                ),
            });
        }
        if active == 0 {
            self.inner.emit(RuntimeEvent::Notice {
                level: NoticeLevel::Info,
                text: "no plugin hooks are active".to_owned(),
            });
        }
    }

    /// The active hook contributions, in execution order.
    pub fn plugin_hooks(&self) -> Vec<crate::hooks::HookInfo> {
        self.inner
            .hooks
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .entries()
            .to_vec()
    }

    /// Hooks a manifest asked for but did not have the permissions (or the spelling) to register.
    pub fn plugin_hook_rejections(&self) -> Vec<crate::hooks::HookRejection> {
        self.inner
            .hooks
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .rejected()
            .to_vec()
    }

    pub fn plugin_install(&self, path: &std::path::Path) -> Result<String> {
        let registry = self.inner.plugins.lock().unwrap_or_else(|e| {
            eprintln!("vikram: plugins lock mutex poisoned; recovering");
            e.into_inner()
        });
        let id = registry.install(path).map_err(RuntimeError::Store)?;
        let installed = registry.installed();
        let loaded = registry.plugins().iter().filter(|p| p.loaded).count();
        drop(registry);
        self.reload_hooks();
        self.inner
            .emit(RuntimeEvent::PluginsChanged { installed, loaded });
        Ok(id)
    }

    pub fn plugin_uninstall(&self, id: &str) -> Result<()> {
        self.plugin_mutate(|registry| registry.uninstall(id))
    }

    pub fn plugin_set_enabled(&self, id: &str, enabled: bool) -> Result<()> {
        self.plugin_mutate(|registry| {
            if enabled {
                registry.enable(id)
            } else {
                registry.disable(id)
            }
        })
    }

    pub fn tasks(&self) -> Vec<BackgroundTask> {
        self.inner.tasks.list()
    }

    pub fn complete_task(&self, id: &str, state: TaskState) {
        if let Some(task) = self.inner.tasks.finish(id, state) {
            self.inner.task_snapshot(&task);
        }
    }

    pub fn hive_roster_create(&self, name: impl Into<String>) -> crate::error::Result<()> {
        self.inner
            .rosters
            .create(name.into())
            .map_err(RuntimeError::InvalidState)?;
        self.inner
            .rosters
            .save()
            .map_err(RuntimeError::InvalidState)?;
        Ok(())
    }

    pub fn hive_roster_list(&self) -> Vec<HiveRoster> {
        self.inner.rosters.list()
    }

    pub fn hive_roster_get(&self, name: impl Into<String>) -> Option<HiveRoster> {
        self.inner.rosters.get(name.into())
    }

    pub fn hive_roster_add_member(
        &self,
        roster: impl Into<String>,
        bot: BotId,
    ) -> crate::error::Result<()> {
        self.inner
            .rosters
            .add_member(roster.into(), bot)
            .map_err(RuntimeError::InvalidState)?;
        self.inner
            .rosters
            .save()
            .map_err(RuntimeError::InvalidState)?;
        Ok(())
    }

    pub fn hive_roster_remove_member(
        &self,
        roster: impl Into<String>,
        bot: BotId,
    ) -> crate::error::Result<()> {
        self.inner
            .rosters
            .remove_member(roster.into(), bot)
            .map_err(RuntimeError::InvalidState)?;
        self.inner
            .rosters
            .save()
            .map_err(RuntimeError::InvalidState)?;
        Ok(())
    }

    pub fn hive_roster_delete(&self, name: impl Into<String>) -> crate::error::Result<()> {
        self.inner
            .rosters
            .delete(name.into())
            .map_err(RuntimeError::InvalidState)?;
        self.inner
            .rosters
            .save()
            .map_err(RuntimeError::InvalidState)?;
        Ok(())
    }

    pub fn hive_roster_delegate(
        &self,
        roster: impl Into<String>,
        task: impl Into<String>,
        assigned_to: Option<BotId>,
    ) -> crate::error::Result<()> {
        self.inner
            .rosters
            .delegate_task(roster.into(), task.into(), assigned_to)
            .map_err(RuntimeError::InvalidState)?;
        self.inner
            .rosters
            .save()
            .map_err(RuntimeError::InvalidState)?;
        Ok(())
    }

    // ---- hive group chat -------------------------------------------------

    pub fn hive_create(
        &self,
        owner: vikram_contracts::UserId,
        profile: Option<vikram_contracts::ProfileId>,
        name: impl Into<String>,
        objective: impl Into<String>,
        policy: impl Into<String>,
    ) -> Result<vikram_contracts::HiveId> {
        let now = now_ms();
        let row = self
            .inner
            .hive
            .create(
                owner,
                profile,
                &name.into(),
                &objective.into(),
                &policy.into(),
                now,
            )
            .map_err(RuntimeError::InvalidState)?;
        Ok(row.id)
    }

    pub fn hive_list(&self) -> Result<Vec<vikram_store::HiveRow>> {
        self.inner
            .hive
            .manager
            .list()
            .map_err(RuntimeError::InvalidState)
    }

    pub fn hive_list_by_user(
        &self,
        user: &vikram_contracts::UserId,
    ) -> Result<Vec<vikram_store::HiveRow>> {
        self.inner
            .hive
            .list_by_user(user)
            .map_err(RuntimeError::InvalidState)
    }

    pub fn hive_get(&self, id: &vikram_contracts::HiveId) -> Result<Option<vikram_store::HiveRow>> {
        self.inner
            .hive
            .manager
            .get(id)
            .map_err(RuntimeError::InvalidState)
    }

    pub fn hive_delete(&self, id: &vikram_contracts::HiveId) -> Result<()> {
        self.inner
            .hive
            .delete(id)
            .map_err(RuntimeError::InvalidState)
    }

    pub fn hive_add_participant(
        &self,
        hive: &vikram_contracts::HiveId,
        entity: &vikram_contracts::EntityId,
        kind: vikram_store::EntityKind,
        role: impl Into<String>,
    ) -> Result<()> {
        self.inner
            .hive
            .add_participant(hive, entity, kind, &role.into(), now_ms())
            .map_err(RuntimeError::InvalidState)
    }

    pub fn hive_remove_participant(
        &self,
        hive: &vikram_contracts::HiveId,
        entity: &vikram_contracts::EntityId,
    ) -> Result<()> {
        self.inner
            .hive
            .remove_participant(hive, entity)
            .map_err(RuntimeError::InvalidState)
    }

    pub fn hive_participants(
        &self,
        hive: &vikram_contracts::HiveId,
    ) -> Result<Vec<vikram_store::HiveParticipantRow>> {
        self.inner
            .hive
            .participants(hive)
            .map_err(RuntimeError::InvalidState)
    }

    pub fn hive_append_message(
        &self,
        hive: &vikram_contracts::HiveId,
        sender: &str,
        sender_kind: &str,
        content: &str,
        task_ref: Option<&str>,
    ) -> Result<vikram_store::HiveMessageRow> {
        self.inner
            .hive
            .append_message(hive, sender, sender_kind, content, task_ref, now_ms())
            .map_err(RuntimeError::InvalidState)
    }

    pub fn hive_messages(
        &self,
        hive: &vikram_contracts::HiveId,
    ) -> Result<Vec<vikram_store::HiveMessageRow>> {
        self.inner
            .hive
            .messages(hive)
            .map_err(RuntimeError::InvalidState)
    }

    pub fn hive_set_session(
        &self,
        hive: &vikram_contracts::HiveId,
        session: Option<&vikram_contracts::SessionId>,
    ) -> Result<()> {
        self.inner
            .hive
            .set_session(hive, session, now_ms())
            .map_err(RuntimeError::InvalidState)
    }

    // ---- unified ticket system ------------------------------------------

    pub fn tickets(&self) -> &TicketManager {
        &self.inner.tickets.manager
    }

    pub fn ticket_create(
        &self,
        owner_key: &str,
        owner_kind: &str,
        kind: &str,
        title: &str,
        request: &str,
    ) -> Result<vikram_contracts::TicketId> {
        let now = now_ms();
        let row = self
            .inner
            .tickets
            .create(
                owner_key, owner_kind, kind, title, request, "", None, None, None, None, now,
            )
            .map_err(RuntimeError::InvalidState)?;
        Ok(row.id)
    }

    pub fn ticket_get(
        &self,
        id: &vikram_contracts::TicketId,
    ) -> Result<Option<vikram_store::TicketRow>> {
        self.inner
            .tickets
            .get(id)
            .map_err(RuntimeError::InvalidState)
    }

    pub fn ticket_list(&self) -> Result<Vec<vikram_store::TicketRow>> {
        self.inner
            .tickets
            .list()
            .map_err(RuntimeError::InvalidState)
    }

    pub fn ticket_list_by_owner(&self, owner_key: &str) -> Result<Vec<vikram_store::TicketRow>> {
        self.inner
            .tickets
            .list_by_owner(owner_key)
            .map_err(RuntimeError::InvalidState)
    }

    pub fn ticket_list_by_kind(&self, kind: &str) -> Result<Vec<vikram_store::TicketRow>> {
        self.inner
            .tickets
            .list_by_kind(kind)
            .map_err(RuntimeError::InvalidState)
    }

    pub fn ticket_list_by_status(&self, status: &str) -> Result<Vec<vikram_store::TicketRow>> {
        self.inner
            .tickets
            .list_by_status(status)
            .map_err(RuntimeError::InvalidState)
    }

    pub fn ticket_search(&self, query: &str) -> Result<Vec<vikram_store::TicketRow>> {
        self.inner
            .tickets
            .search(query)
            .map_err(RuntimeError::InvalidState)
    }

    pub fn ticket_close(&self, id: &vikram_contracts::TicketId, actor: &str) -> Result<()> {
        self.inner
            .tickets
            .close(id, actor, now_ms())
            .map_err(RuntimeError::InvalidState)
    }

    pub fn ticket_reopen(&self, id: &vikram_contracts::TicketId, actor: &str) -> Result<()> {
        self.inner
            .tickets
            .reopen(id, actor, now_ms())
            .map_err(RuntimeError::InvalidState)
    }

    pub fn ticket_cancel(&self, id: &vikram_contracts::TicketId, actor: &str) -> Result<()> {
        self.inner
            .tickets
            .cancel(id, actor, now_ms())
            .map_err(RuntimeError::InvalidState)
    }

    pub fn ticket_set_result(
        &self,
        id: &vikram_contracts::TicketId,
        result: &str,
        actor: &str,
    ) -> Result<()> {
        self.inner
            .tickets
            .set_result(id, result, actor, now_ms())
            .map_err(RuntimeError::InvalidState)
    }

    pub fn ticket_set_failure(
        &self,
        id: &vikram_contracts::TicketId,
        reason: &str,
        actor: &str,
    ) -> Result<()> {
        self.inner
            .tickets
            .set_failure(id, reason, actor, now_ms())
            .map_err(RuntimeError::InvalidState)
    }

    pub fn ticket_delete(&self, id: &vikram_contracts::TicketId) -> Result<()> {
        self.inner
            .tickets
            .delete(id)
            .map_err(RuntimeError::InvalidState)
    }

    pub fn ticket_add_participant(&self, row: &vikram_store::TicketParticipantRow) -> Result<()> {
        self.inner
            .tickets
            .add_participant(row)
            .map_err(RuntimeError::InvalidState)
    }

    pub fn ticket_participants(
        &self,
        id: &vikram_contracts::TicketId,
    ) -> Result<Vec<vikram_store::TicketParticipantRow>> {
        self.inner
            .tickets
            .participants(id)
            .map_err(RuntimeError::InvalidState)
    }

    pub fn ticket_post_message(
        &self,
        ticket: &vikram_contracts::TicketId,
        author_key: &str,
        author_kind: &str,
        content: &str,
        visibility: &str,
    ) -> Result<vikram_store::TicketMessageRow> {
        let now = now_ms();
        self.inner
            .tickets
            .post_message(
                ticket,
                author_key,
                author_kind,
                content,
                visibility,
                None,
                now,
            )
            .map_err(RuntimeError::InvalidState)
    }

    pub fn ticket_messages(
        &self,
        ticket: &vikram_contracts::TicketId,
        viewer_key: &str,
    ) -> Result<Vec<vikram_store::TicketMessageRow>> {
        self.inner
            .tickets
            .messages_for(ticket, viewer_key)
            .map_err(RuntimeError::InvalidState)
    }

    pub fn ticket_create_assignment(
        &self,
        ticket: &vikram_contracts::TicketId,
        assignee_key: &str,
        assignee_kind: &str,
        entity_id: Option<vikram_contracts::EntityId>,
        title: &str,
        description: &str,
    ) -> Result<vikram_store::AssignmentRow> {
        let now = now_ms();
        self.inner
            .tickets
            .create_assignment(
                ticket,
                assignee_key,
                assignee_kind,
                entity_id,
                title,
                description,
                None,
                None,
                now,
            )
            .map_err(RuntimeError::InvalidState)
    }

    pub fn ticket_assignments(
        &self,
        ticket: &vikram_contracts::TicketId,
    ) -> Result<Vec<vikram_store::AssignmentRow>> {
        self.inner
            .tickets
            .assignments(ticket)
            .map_err(RuntimeError::InvalidState)
    }

    pub fn ticket_complete_assignment(
        &self,
        id: &vikram_contracts::AssignmentId,
        result: &str,
        actor: &str,
    ) -> Result<()> {
        self.inner
            .tickets
            .complete_assignment(id, result, actor, now_ms())
            .map_err(RuntimeError::InvalidState)
    }

    pub fn ticket_fail_assignment(
        &self,
        id: &vikram_contracts::AssignmentId,
        error: &str,
        actor: &str,
    ) -> Result<()> {
        self.inner
            .tickets
            .fail_assignment(id, error, actor, now_ms())
            .map_err(RuntimeError::InvalidState)
    }

    pub fn ticket_update_assignment_status(
        &self,
        id: &vikram_contracts::AssignmentId,
        status: &str,
        actor: &str,
    ) -> Result<()> {
        self.inner
            .tickets
            .update_assignment_status(id, status, actor, now_ms())
            .map_err(RuntimeError::InvalidState)
    }

    pub fn ticket_events(
        &self,
        ticket: &vikram_contracts::TicketId,
    ) -> Result<Vec<vikram_store::TicketEventRow>> {
        self.inner
            .tickets
            .events(ticket)
            .map_err(RuntimeError::InvalidState)
    }

    // Hive-ticket integration: create a shared-collaboration ticket for a hive.
    pub fn ticket_create_hive(
        &self,
        hive_id: &vikram_contracts::HiveId,
        owner_key: &str,
        title: &str,
        request: &str,
        strategy: &str,
        member_keys: &[(String, String)],
    ) -> Result<vikram_contracts::TicketId> {
        let now = now_ms();
        let row = self
            .inner
            .tickets
            .create_hive_ticket(
                hive_id,
                owner_key,
                title,
                request,
                strategy,
                member_keys,
                now,
            )
            .map_err(RuntimeError::InvalidState)?;
        Ok(row.id)
    }

    // Bot-ticket integration: create an independent bot-owned ticket.
    pub fn ticket_create_bot(
        &self,
        bot_key: &str,
        title: &str,
        request: &str,
    ) -> Result<vikram_contracts::TicketId> {
        let now = now_ms();
        let row = self
            .inner
            .tickets
            .create_bot_ticket(bot_key, title, request, now)
            .map_err(RuntimeError::InvalidState)?;
        Ok(row.id)
    }

    // Sub-agent assignment under a parent ticket.
    pub fn ticket_create_subagent_assignment(
        &self,
        ticket: &vikram_contracts::TicketId,
        assignee_key: &str,
        assignee_kind: &str,
        entity_id: Option<vikram_contracts::EntityId>,
        title: &str,
        description: &str,
    ) -> Result<vikram_store::AssignmentRow> {
        let now = now_ms();
        self.inner
            .tickets
            .create_subagent_assignment(
                ticket,
                assignee_key,
                assignee_kind,
                entity_id,
                title,
                description,
                now,
            )
            .map_err(RuntimeError::InvalidState)
    }

    pub async fn hive_run(
        &self,
        hive: &vikram_contracts::HiveId,
        initial: &str,
        parallel: bool,
    ) -> Result<Vec<vikram_store::HiveMessageRow>> {
        self.inner
            .hive
            .manager
            .ensure_runnable(hive)
            .map_err(RuntimeError::InvalidState)?;
        // One run per hive at a time: concurrent calls would otherwise interleave their
        // transcript appends into a single shared log.
        let _run_claim = self
            .inner
            .hive
            .manager
            .claim_run(hive)
            .map_err(RuntimeError::InvalidState)?;

        let now = now_ms();
        self.inner
            .hive
            .append_message(hive, "vikram", "main_agent", initial, None, now)
            .map_err(RuntimeError::InvalidState)?;

        let participants = self
            .inner
            .hive
            .participants(hive)
            .map_err(RuntimeError::InvalidState)?;
        let mut entities: Vec<vikram_store::EntityRow> = Vec::new();
        for p in &participants {
            if let Ok(Some(e)) = self.inner.store.get_entity(&p.entity) {
                entities.push(e);
            }
        }

        let provider = self
            .inner
            .provider
            .read()
            .expect("provider lock")
            .clone()
            .ok_or_else(|| RuntimeError::InvalidState("no provider configured".into()))?;
        let model = self.inner.model();

        // Shared transcript snapshot taken before this run (used by the parallel mode so every
        // participant sees the same starting context).
        let base_msgs = self
            .inner
            .hive
            .messages(hive)
            .map_err(RuntimeError::InvalidState)?;

        // Helper: collect one participant's response given a transcript snapshot.
        async fn collect_one(
            inner: &Arc<RuntimeInner>,
            provider: &Arc<dyn Provider>,
            model: &str,
            ent: &vikram_store::EntityRow,
            msgs: &[vikram_store::HiveMessageRow],
        ) -> String {
            let ctx = inner.context.load_agent_or_bot(ent);
            let system_text = ctx.render();
            // A hive turn is shared context, and authorising a share is not the same as emptying
            // the log into the prompt: the participant answers from the most recent exchange that
            // fits its budget, and nothing it was not party to.
            let budget_tokens = inner
                .config
                .read()
                .unwrap_or_else(|e| e.into_inner())
                .context
                .hive_budget_tokens;
            let estimator = crate::context_engine::TokenEstimator::default();
            let mut spent = estimator.estimate_text(&system_text);
            let mut kept: Vec<&vikram_store::HiveMessageRow> = Vec::new();
            for m in msgs.iter().rev() {
                let cost = estimator.estimate_text(&m.content) + 4;
                if !kept.is_empty() && spent + cost > budget_tokens {
                    break;
                }
                spent += cost;
                kept.push(m);
            }
            kept.reverse();
            let mut request_messages: Vec<vikram_contracts::Message> = Vec::new();
            if !system_text.trim().is_empty() {
                request_messages.push(vikram_contracts::Message::system(system_text));
            }
            for m in kept {
                let own = m.sender == ent.name || m.sender == ent.slug;
                let role = if own {
                    vikram_contracts::Role::Assistant
                } else {
                    vikram_contracts::Role::User
                };
                // Only the operator's prompt — relayed as `vikram` — is user-trusted.
                // Peer participants speak through their own models, so their words
                // carry model trust and can never read as user instruction.
                let trust = if own || m.sender == "vikram" {
                    vikram_contracts::Trust::User
                } else {
                    vikram_contracts::Trust::Model
                };
                request_messages.push(vikram_contracts::Message::text(
                    role,
                    m.content.clone(),
                    trust,
                ));
            }
            let request = crate::provider::CompletionRequest {
                model: model.to_owned(),
                messages: request_messages,
                tools: Vec::new(),
                max_output_tokens: inner
                    .config
                    .read()
                    .unwrap_or_else(|e| e.into_inner())
                    .max_output_tokens,
            };
            let mut stream = provider.stream(request);
            let mut collected = String::new();
            use futures_util::StreamExt;
            while let Some(item) = stream.next().await {
                match item {
                    Ok(crate::provider::ProviderEvent::TextDelta(t)) => collected.push_str(&t),
                    Ok(crate::provider::ProviderEvent::Finished { .. }) => break,
                    Ok(_) => {}
                    Err(_) => break,
                }
            }
            collected
        }

        if parallel {
            // Issue all participant requests concurrently, then append responses in order.
            let mut handles = Vec::new();
            for ent in &entities {
                let inner = Arc::clone(&self.inner);
                let provider = Arc::clone(&provider);
                let model = model.clone();
                let ent = ent.clone();
                let msgs = base_msgs.clone();
                handles.push(tokio::spawn(async move {
                    collect_one(&inner, &provider, &model, &ent, &msgs).await
                }));
            }
            let mut responses: Vec<String> = Vec::new();
            for h in handles {
                responses.push(h.await.unwrap_or_default());
            }
            for (ent, resp) in entities.iter().zip(responses.iter()) {
                if !resp.trim().is_empty() {
                    let ts = now_ms();
                    self.inner
                        .hive
                        .append_message(hive, &ent.name, ent.kind.as_str(), resp, None, ts)
                        .map_err(RuntimeError::InvalidState)?;
                }
            }
        } else {
            // Sequential: each participant sees prior responses.
            for ent in &entities {
                let msgs = self
                    .inner
                    .hive
                    .messages(hive)
                    .map_err(RuntimeError::InvalidState)?;
                let resp = collect_one(&self.inner, &provider, &model, ent, &msgs).await;
                if !resp.trim().is_empty() {
                    let ts = now_ms();
                    self.inner
                        .hive
                        .append_message(hive, &ent.name, ent.kind.as_str(), &resp, None, ts)
                        .map_err(RuntimeError::InvalidState)?;
                }
            }
        }

        self.inner
            .hive
            .messages(hive)
            .map_err(RuntimeError::InvalidState)
    }

    pub fn memory_save<K, V, S>(&self, key: K, value: V, scope: S) -> Result<()>
    where
        K: AsRef<str>,
        V: AsRef<str>,
        S: AsRef<str>,
    {
        let (key, value, scope) = (key.as_ref(), value.as_ref(), scope.as_ref());
        self.inner
            .memory
            .save(key, value, scope)
            .map_err(|e| RuntimeError::Store(e.to_string()))?;
        self.inner
            .memory_providers()
            .on_memory_write("save", key, value);
        Ok(())
    }

    pub fn memory_search<Q, S>(&self, query: Q, scope: S) -> Result<Vec<(String, String)>>
    where
        Q: AsRef<str>,
        S: AsRef<str>,
    {
        self.inner.memory.search(query, scope)
    }

    pub fn memory_list<S>(&self, scope: S) -> Result<Vec<(String, String)>>
    where
        S: AsRef<str>,
    {
        self.inner.memory.list(scope)
    }

    /// Every remembered key, as `(scope, key, value)`. The panel lists them all because a scope the
    /// operator cannot see is a scope they cannot ask about.
    pub fn memory_list_all(&self) -> Result<Vec<(String, String, String)>> {
        self.inner.memory.list_all()
    }

    pub fn memory_delete<K, S>(&self, key: K, scope: S) -> Result<bool>
    where
        K: AsRef<str>,
        S: AsRef<str>,
    {
        let (key, scope) = (key.as_ref(), scope.as_ref());
        let deleted = self
            .inner
            .memory
            .delete(key, scope)
            .map_err(|e| RuntimeError::Store(e.to_string()))?;
        if deleted {
            self.inner
                .memory_providers()
                .on_memory_write("delete", key, "");
        }
        Ok(deleted)
    }

    /// The memory backend manager this runtime booted with: which provider
    /// `config.memory_provider` selected, and where an external one registers.
    pub fn memory_providers(&self) -> std::sync::MutexGuard<'_, crate::MemoryProviderManager> {
        self.inner.memory_providers()
    }

    pub fn skill_list(&self) -> Vec<Skill> {
        self.inner
            .skills
            .lock()
            .unwrap_or_else(|e| {
                eprintln!("vikram: runtime skills lock mutex poisoned; recovering");
                e.into_inner()
            })
            .list()
    }

    pub fn skill_get(&self, name: &str) -> Option<Skill> {
        self.inner
            .skills
            .lock()
            .unwrap_or_else(|e| {
                eprintln!("vikram: runtime skills lock mutex poisoned; recovering");
                e.into_inner()
            })
            .get(name)
    }

    pub fn skill_count(&self) -> usize {
        self.inner
            .skills
            .lock()
            .unwrap_or_else(|e| {
                eprintln!("vikram: runtime skills lock mutex poisoned; recovering");
                e.into_inner()
            })
            .count()
    }

    /// Enable a skill by name (removes it from the disabled list). Re-discovers afterwards.
    pub fn skill_enable(&self, name: &str) -> Result<()> {
        let mgr = vikram_skills::Manager::new();
        mgr.enable(name)
            .map_err(|e| RuntimeError::Store(e.to_string()))?;
        self.rescan_skills();
        self.inner.emit(RuntimeEvent::SkillsChanged {
            discovered: self.skill_count(),
            loaded: 0,
        });
        Ok(())
    }

    /// Disable a skill by name (adds it to the disabled list). Re-discovers afterwards.
    pub fn skill_disable(&self, name: &str) -> Result<()> {
        let mgr = vikram_skills::Manager::new();
        mgr.disable(name)
            .map_err(|e| RuntimeError::Store(e.to_string()))?;
        self.rescan_skills();
        self.inner.emit(RuntimeEvent::SkillsChanged {
            discovered: self.skill_count(),
            loaded: 0,
        });
        Ok(())
    }

    /// Delete a skill (moves it to trash). Re-discovers afterwards.
    pub fn skill_delete(&self, name: &str) -> Result<()> {
        let mgr = vikram_skills::Manager::new();
        mgr.delete(name)
            .map_err(|e| RuntimeError::Store(e.to_string()))?;
        self.rescan_skills();
        self.inner.emit(RuntimeEvent::SkillsChanged {
            discovered: self.skill_count(),
            loaded: 0,
        });
        Ok(())
    }

    /// Create a new skill with name + description + body. Re-discovers afterwards.
    pub fn skill_create(&self, name: &str, description: &str, body: &str) -> Result<()> {
        let mgr = vikram_skills::Manager::new();
        mgr.create(name, description, body)
            .map_err(|e| RuntimeError::Store(e.to_string()))?;
        self.rescan_skills();
        self.inner.emit(RuntimeEvent::SkillsChanged {
            discovered: self.skill_count(),
            loaded: 0,
        });
        Ok(())
    }

    /// Rename a skill by copying its directory to the new name and removing the old one, so every
    /// file the skill ships survives. Re-discovers afterwards.
    pub fn skill_rename(&self, name: &str, new_name: &str) -> Result<()> {
        let new_name = new_name.trim();
        if new_name.is_empty() {
            return Err(RuntimeError::InvalidState("a skill needs a name".into()));
        }
        let dir = {
            let skills = self.inner.skills.lock().unwrap_or_else(|e| {
                eprintln!("vikram: runtime skills lock mutex poisoned; recovering");
                e.into_inner()
            });
            skills
                .get(name)
                .and_then(|skill| skill.path.parent().map(|p| p.to_path_buf()))
                .ok_or_else(|| RuntimeError::InvalidState(format!("no skill `{name}`")))?
        };
        let mgr = vikram_skills::Manager::new();
        mgr.create_from(new_name, &dir)
            .map_err(|e| RuntimeError::Store(e.to_string()))?;
        if let Err(e) = mgr.delete(name) {
            // Roll back the copy so a failed rename does not leave a shadowing duplicate.
            let _ = mgr.delete(new_name);
            return Err(RuntimeError::Store(e.to_string()));
        }
        self.rescan_skills();
        self.inner.emit(RuntimeEvent::SkillsChanged {
            discovered: self.skill_count(),
            loaded: 0,
        });
        Ok(())
    }

    /// Import a skill from a directory path. Re-discovers afterwards.
    pub fn skill_import(&self, path: &str) -> Result<()> {
        let mgr = vikram_skills::Manager::new();
        mgr.import(std::path::Path::new(path))
            .map_err(|e| RuntimeError::Store(e.to_string()))?;
        self.rescan_skills();
        self.inner.emit(RuntimeEvent::SkillsChanged {
            discovered: self.skill_count(),
            loaded: 0,
        });
        Ok(())
    }

    fn rescan_skills(&self) {
        let reg = SkillRegistry::for_workspace(&self.inner.workspace_root);
        *self.inner.skills.lock().unwrap_or_else(|e| {
            eprintln!("vikram: runtime skills lock mutex poisoned; recovering");
            e.into_inner()
        }) = reg;
    }

    pub fn shutdown(&self) {
        self.inner
            .emit(RuntimeEvent::Status(RuntimeStatus::Stopping));
        for session in self.inner.session_list() {
            session.cancel_handle().cancel();
        }
        for agent in self.inner.bots.list() {
            agent.cancel_handle().cancel();
        }
        self.inner.emit(RuntimeEvent::Shutdown);
    }
}

#[cfg(test)]
#[allow(
    clippy::await_holding_lock,
    reason = "isolated_home guards process-global env vars; releasing it at an await would let a concurrent test repoint VIKRAM_HOME mid-run"
)]
mod tests {
    use super::*;
    use crate::config::Config;
    use vikram_contracts::{Block, SideEffect, Trust};

    /// `VIKRAM_HOME` and the security switches are process-global, so a test that points the home at
    /// a temporary directory holds the shared guard for its whole body.
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

    #[tokio::test]
    async fn startup_creates_a_session_and_reports_real_metrics() {
        let dir = tempfile::tempdir().unwrap();
        let _home = isolated_home(dir.path());
        let config = config_for(dir.path());
        let runtime = Runtime::start(config).await.unwrap();
        let snapshot = runtime.handle().snapshot();
        assert_eq!(snapshot.sessions.len(), 1);
        assert_eq!(snapshot.bots.len(), 0);
        assert_eq!(snapshot.metrics.bots_active, 0);
        assert_eq!(snapshot.metrics.background_tasks, 0);
        assert!(snapshot.metrics.model.is_some());
    }

    #[tokio::test]
    async fn queueing_on_an_idle_session_starts_the_turn_instead_of_stranding_it() {
        let dir = tempfile::tempdir().unwrap();
        let _home = isolated_home(dir.path());
        let runtime = Runtime::start(config_for(dir.path())).await.unwrap();
        let handle = runtime.handle();
        let id = handle.snapshot().sessions[0].id.clone();

        handle
            .queue(&id, "hello".into())
            .expect("the text is accepted");

        assert_eq!(
            handle.snapshot().sessions[0].queued,
            0,
            "a queue nothing drains must never be where an idle session's message ends up"
        );
    }

    #[tokio::test]
    async fn an_endpoint_switch_is_saved_and_keeps_the_model_that_was_chosen() {
        let dir = tempfile::tempdir().unwrap();
        let _home = isolated_home(dir.path());
        let runtime = Runtime::start(config_for(dir.path())).await.unwrap();
        let handle = runtime.handle();

        handle
            .set_provider(
                "https://openrouter.ai/api/v1".into(),
                "VIKRAM_TEST_OPENROUTER_KEY".into(),
                "openai".into(),
            )
            .expect("a new endpoint is a config edit, not a refusal");

        let saved = Config::load().unwrap();
        assert_eq!(saved.base_url, "https://openrouter.ai/api/v1");
        assert_eq!(saved.api_key_env, "VIKRAM_TEST_OPENROUTER_KEY");
        assert_eq!(
            handle.config().model,
            "test-model",
            "changing where the requests go must not change which model they name"
        );
        assert!(
            handle.config().api_key().is_none(),
            "the switch is kept even when the new variable holds no key yet"
        );
    }

    #[tokio::test]
    async fn an_empty_endpoint_is_refused_rather_than_written() {
        let dir = tempfile::tempdir().unwrap();
        let _home = isolated_home(dir.path());
        let runtime = Runtime::start(config_for(dir.path())).await.unwrap();
        let handle = runtime.handle();
        let before = handle.config().base_url;

        assert!(
            handle
                .set_provider("   ".into(), "SOME_KEY".into(), "openai".into())
                .is_err()
        );
        assert_eq!(handle.config().base_url, before);
    }

    #[tokio::test]
    async fn a_session_can_be_created_renamed_and_closed() {
        let dir = tempfile::tempdir().unwrap();
        let _home = isolated_home(dir.path());
        let runtime = Runtime::start(config_for(dir.path())).await.unwrap();
        let handle = runtime.handle();
        let id = handle.create_session(Some("Research".into())).unwrap();
        handle.rename_session(&id, "Research 2".into()).unwrap();
        assert!(
            handle
                .snapshot()
                .sessions
                .iter()
                .any(|s| s.title == "Research 2")
        );
        handle.close_session(&id).unwrap();
        assert!(!handle.snapshot().sessions.iter().any(|s| s.id == id));
    }

    /// A memory provider that only counts the lifecycle events the runtime fans out, so a test can
    /// prove the seam is live without depending on what any real backend does with them.
    struct RecordingMemoryProvider {
        session_ends: std::sync::Arc<std::sync::atomic::AtomicUsize>,
        memory_writes: std::sync::Arc<std::sync::atomic::AtomicUsize>,
    }

    impl crate::MemoryProvider for RecordingMemoryProvider {
        fn name(&self) -> &str {
            "recorder"
        }
        fn is_available(&self) -> bool {
            true
        }
        fn initialize(&self, _session_id: &str) {}
        fn on_session_end(&self, _messages: Vec<serde_json::Value>) {
            self.session_ends
                .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        }
        fn on_memory_write(&self, _action: &str, _target: &str, _content: &str) {
            self.memory_writes
                .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        }
    }

    #[tokio::test]
    async fn boot_builds_the_memory_provider_the_config_names() {
        let dir = tempfile::tempdir().unwrap();
        let _home = isolated_home(dir.path());
        let runtime = Runtime::start(config_for(dir.path())).await.unwrap();
        let handle = runtime.handle();
        assert!(
            handle.memory_providers().get_provider("builtin").is_some(),
            "the default config must boot the builtin memory provider"
        );
    }

    #[tokio::test]
    async fn an_unknown_memory_provider_fails_boot_instead_of_falling_back() {
        let dir = tempfile::tempdir().unwrap();
        let _home = isolated_home(dir.path());
        let config = Config {
            memory_provider: "bogus".into(),
            ..config_for(dir.path())
        };
        let err = Runtime::start(config)
            .await
            .err()
            .expect("an unknown memory provider must fail boot");
        assert!(
            err.to_string().contains("unknown memory provider"),
            "unexpected boot error: {err}"
        );
    }

    #[tokio::test]
    async fn session_close_and_memory_writes_reach_registered_providers() {
        let dir = tempfile::tempdir().unwrap();
        let _home = isolated_home(dir.path());
        let runtime = Runtime::start(config_for(dir.path())).await.unwrap();
        let handle = runtime.handle();
        let session_ends = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let memory_writes = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        handle
            .memory_providers()
            .add_external(Box::new(RecordingMemoryProvider {
                session_ends: std::sync::Arc::clone(&session_ends),
                memory_writes: std::sync::Arc::clone(&memory_writes),
            }))
            .expect("no external memory provider is registered yet");

        let id = handle.snapshot().sessions[0].id.clone();
        handle.close_session(&id).unwrap();
        assert_eq!(
            session_ends.load(std::sync::atomic::Ordering::SeqCst),
            1,
            "closing a session must notify memory providers once"
        );

        handle.memory_save("k", "v", "global").unwrap();
        handle.memory_delete("k", "global").unwrap();
        assert_eq!(
            memory_writes.load(std::sync::atomic::Ordering::SeqCst),
            2,
            "a save and a successful delete are the two memory writes"
        );
    }

    #[tokio::test]
    async fn a_closed_tab_keeps_its_transcript_and_reopens_from_the_catalogue() {
        let dir = tempfile::tempdir().unwrap();
        let _home = isolated_home(dir.path());
        let (runtime, _) = named_runtime(dir.path(), "Rust JWT Authentication").await;
        let handle = runtime.handle();
        let id = handle.create_session(Some("Closed work".into())).unwrap();

        handle.close_session(&id).unwrap();

        // The tab is gone, the conversation is not.
        assert!(!handle.snapshot().sessions.iter().any(|s| s.id == id));
        let closed = handle
            .session_catalogue()
            .into_iter()
            .find(|entry| entry.id == id)
            .expect("a closed session stays in the catalogue");
        assert!(!closed.open, "a closed session must not claim to be open");

        handle.open_session(&id).unwrap();
        let reopened = handle
            .session_catalogue()
            .into_iter()
            .find(|entry| entry.id == id)
            .expect("the reopened session is catalogued");
        assert!(reopened.open, "the reopened session must be open");
        assert!(handle.snapshot().sessions.iter().any(|s| s.id == id));

        handle.delete_session(&id).unwrap();
        assert!(
            !handle
                .session_catalogue()
                .iter()
                .any(|entry| entry.id == id),
            "delete is the one path that removes the record"
        );
    }

    #[tokio::test]
    async fn submitting_without_a_provider_fails_visibly_instead_of_faking_output() {
        let dir = tempfile::tempdir().unwrap();
        let _home = isolated_home(dir.path());
        let config = Config {
            model: String::new(),
            workspace_root: Some(dir.path().to_path_buf()),
            ..Config::default()
        };
        let runtime = Runtime::start(config).await.unwrap();
        let handle = runtime.handle();
        let mut events = handle.events();
        let id = handle.snapshot().sessions[0].id.clone();
        handle.submit(&id, "hello".into()).unwrap();
        let mut saw_error = false;
        for _ in 0..20 {
            match tokio::time::timeout(std::time::Duration::from_millis(200), events.recv()).await {
                Ok(Ok(RuntimeEvent::Error { text })) => {
                    assert!(text.contains("model"));
                    saw_error = true;
                    break;
                }
                Ok(Ok(_)) => continue,
                _ => break,
            }
        }
        assert!(saw_error, "unconfigured provider must surface an error");
    }

    #[tokio::test]
    async fn a_pending_question_is_answered_once_through_the_handle() {
        let dir = tempfile::tempdir().unwrap();
        let _home = isolated_home(dir.path());
        let runtime = Runtime::start(config_for(dir.path())).await.unwrap();
        let handle = runtime.handle();
        let mut events = handle.events();

        // What the turn loop does before it blocks: register, then wait.
        let receiver = handle.inner.open_question("ask-test-1");
        handle
            .answer_question("ask-test-1", "the answer".into())
            .unwrap();
        assert_eq!(receiver.await.unwrap(), "the answer");
        // The sender is consumed, so a second answer is refused instead of silently dropped.
        assert!(
            handle
                .answer_question("ask-test-1", "again".into())
                .is_err()
        );

        let mut resolved = false;
        for _ in 0..20 {
            match tokio::time::timeout(std::time::Duration::from_millis(200), events.recv()).await {
                Ok(Ok(RuntimeEvent::UserAnswerResolved { request_id, answer })) => {
                    assert_eq!(request_id, "ask-test-1");
                    assert_eq!(answer, "the answer");
                    resolved = true;
                    break;
                }
                Ok(Ok(_)) => continue,
                _ => break,
            }
        }
        assert!(resolved, "the answer must be published as an event");
    }

    #[tokio::test]
    async fn a_posture_changes_what_a_prompt_becomes_and_never_what_a_refusal_is() {
        let dir = tempfile::tempdir().unwrap();
        let _home = isolated_home(dir.path());
        let runtime = Runtime::start(config_for(dir.path())).await.unwrap();
        let handle = runtime.handle();
        let session = handle.create_session(Some("posture".into())).unwrap();
        let inner = &handle.inner;
        let request = |action: ActionKind, resource: &str, effect: SideEffect| {
            PermissionRequest::new(action, "shell.run", resource, effect, Trust::Model)
        };

        assert_eq!(handle.mode(), PermissionMode::Ask, "the default asks");
        let command = request(ActionKind::Execute, "cargo test", SideEffect::LocalMutation);
        assert!(
            matches!(inner.judge(&command), Decision::RequireApproval { .. }),
            "ask leaves the prompt alone"
        );

        // yolo stops prompting; it does not reach past a refusal.
        handle.set_mode(&session, PermissionMode::Yolo).unwrap();
        assert!(inner.judge(&command).is_allowed());
        let outside = request(
            ActionKind::Write,
            "/home/someone/.ssh/authorized_keys",
            SideEffect::LocalMutation,
        );
        assert!(
            matches!(inner.judge(&outside), Decision::Deny { .. }),
            "a bypass posture is still not a floor override"
        );
        let self_grant = request(ActionKind::Admin, "grant.scope", SideEffect::ExternalChange);
        assert!(
            matches!(inner.judge(&self_grant), Decision::Deny { .. }),
            "model output cannot grant itself authority, in any posture"
        );

        // plan refuses mutations the rules would have allowed.
        handle.set_mode(&session, PermissionMode::Plan).unwrap();
        let inside_write = request(
            ActionKind::Write,
            &format!("{}/notes.md", dir.path().display()),
            SideEffect::LocalMutation,
        );
        assert!(
            matches!(inner.judge(&inside_write), Decision::Deny { reason } if reason.contains("read-only")),
            "plan is enforcement, not a preference"
        );

        // unattended refuses a prompt without ever emitting it.
        handle
            .set_mode(&session, PermissionMode::Unattended)
            .unwrap();
        assert!(
            matches!(inner.judge(&command), Decision::Deny { reason } if reason.contains("do not retry")),
            "a headless posture answers its own prompts with no"
        );
    }

    #[tokio::test]
    async fn allow_list_mode_runs_only_what_a_human_kept() {
        let dir = tempfile::tempdir().unwrap();
        let _home = isolated_home(dir.path());
        let runtime = Runtime::start(config_for(dir.path())).await.unwrap();
        let handle = runtime.handle();
        let session = handle.create_session(Some("listed".into())).unwrap();
        let inner = &handle.inner;
        handle
            .set_mode(&session, PermissionMode::AllowList)
            .unwrap();

        let read_target = format!("{}/hosts", dir.path().display());
        let read = PermissionRequest::new(
            ActionKind::Read,
            "fs.read",
            &read_target,
            SideEffect::None,
            Trust::Model,
        );
        // The evaluator allows reads; this posture will not run one until it is listed.
        assert!(inner.policy().evaluate(&read).is_allowed());
        match inner.judge(&read) {
            Decision::RequireApproval { reason, amendment } => {
                assert!(reason.contains("not on the list"), "{reason}");
                let proposal = amendment.expect("the prompt proposes what to save");
                handle.allow(&session, &proposal).unwrap();
            }
            other => panic!("an unlisted read must ask, got {other:?}"),
        }
        assert!(
            inner.judge(&read).is_allowed(),
            "keeping the approval ends the prompt"
        );
    }

    /// The invariant the retired kernel enforced at issue time (`ApprovalBroker::issue` refused a
    /// policy-side auto-approval): a decision nobody was asked about cannot become a saved
    /// permission, and an approval id nobody was given cannot be answered at all.
    #[tokio::test]
    async fn a_decision_the_runtime_made_alone_saves_no_permission() {
        let dir = tempfile::tempdir().unwrap();
        let _home = isolated_home(dir.path());
        let runtime = Runtime::start(config_for(dir.path())).await.unwrap();
        let handle = runtime.handle();
        let session = handle.create_session(Some("unasked".into())).unwrap();
        handle
            .set_mode(&session, PermissionMode::Yolo)
            .expect("yolo is available here");

        let command = PermissionRequest::new(
            ActionKind::Execute,
            "shell.run",
            "cargo test",
            SideEffect::LocalMutation,
            Trust::Model,
        );
        assert!(
            handle.inner.judge(&command).is_allowed(),
            "the posture resolves it without asking"
        );
        assert!(
            handle.snapshot().approvals.is_empty(),
            "and nothing becomes pending: an auto-approval is not an approval"
        );

        let forged = vikram_contracts::ApprovalId::generate();
        let err = handle
            .answer_approval(&forged, true, true)
            .expect_err("an id that was never issued cannot be answered");
        assert!(
            err.to_string().contains("no longer pending"),
            "the refusal says why: {err}"
        );
        assert!(
            handle.allow_list().is_empty(),
            "the allow-list is unchanged: {:?}",
            handle.allow_list()
        );
    }

    #[tokio::test]
    async fn a_layer_above_the_runtime_can_refuse_a_posture_and_saving() {
        let dir = tempfile::tempdir().unwrap();
        let _home = isolated_home(dir.path());
        let config = Config {
            security: crate::config::SecurityConfig {
                disable_yolo: true,
                disable_always_allow: true,
                ..Default::default()
            },
            ..config_for(dir.path())
        };
        let runtime = Runtime::start(config).await.unwrap();
        let handle = runtime.handle();
        let session = handle.create_session(Some("locked".into())).unwrap();

        let err = handle
            .set_mode(&session, PermissionMode::Yolo)
            .expect_err("disable_yolo refuses it");
        assert!(
            err.to_string().contains("disable_yolo"),
            "the refusal names the rule: {err}"
        );
        assert_eq!(
            handle.mode(),
            PermissionMode::Ask,
            "a refusal changes nothing"
        );
        assert!(
            !handle.modes().contains(&PermissionMode::Yolo),
            "and it is not offered either"
        );

        let saved = handle
            .allow(&session, "allow execute shell.run resource=cargo test *")
            .expect_err("disable_always_allow forbids keeping permissions");
        assert!(
            saved.to_string().contains("disable_always_allow"),
            "{saved}"
        );
        assert!(handle.allow_list().is_empty());
    }

    #[tokio::test]
    async fn a_start_mode_that_is_refused_falls_back_instead_of_failing_to_boot() {
        let dir = tempfile::tempdir().unwrap();
        let _home = isolated_home(dir.path());
        let config = Config {
            mode: PermissionMode::Yolo,
            security: crate::config::SecurityConfig {
                disable_yolo: true,
                ..Default::default()
            },
            ..config_for(dir.path())
        };
        let runtime = Runtime::start(config).await.unwrap();
        assert_eq!(
            runtime.handle().mode(),
            PermissionMode::Ask,
            "a forbidden start posture is not silently used"
        );
        assert_eq!(runtime.handle().snapshot().mode, PermissionMode::Ask);
    }

    #[tokio::test]
    async fn a_kept_approval_becomes_a_rule_that_cannot_reach_further() {
        let dir = tempfile::tempdir().unwrap();
        let _home = isolated_home(dir.path());
        let runtime = Runtime::start(config_for(dir.path())).await.unwrap();
        let handle = runtime.handle();
        let session = handle.create_session(Some("keep".into())).unwrap();
        let inner = &handle.inner;

        let execute = |command: &str| {
            PermissionRequest::new(
                ActionKind::Execute,
                "shell.run",
                command,
                SideEffect::LocalMutation,
                Trust::Model,
            )
        };
        let cargo = execute("cargo");
        assert!(
            !inner.policy().has_allow(&cargo),
            "nothing is listed before a human keeps it"
        );

        handle
            .allow(&session, "allow execute shell.run resource=cargo *")
            .unwrap();
        assert!(inner.policy().has_allow(&cargo));
        assert!(
            !inner.policy().has_allow(&execute("rm -rf /")),
            "a kept approval covers the command it was given, not the shell"
        );
        assert_eq!(handle.allow_list().len(), 1);

        // A line claiming more than any approval granted is not an entry.
        assert!(
            handle
                .allow(&session, "allow execute shell.run resource=*")
                .is_err(),
            "the whole verb is not a saved approval"
        );
        assert_eq!(handle.allow_list().len(), 1);

        handle.unallow(0).unwrap();
        assert!(!inner.policy().has_allow(&cargo));
        assert!(handle.unallow(0).is_err());
    }

    #[tokio::test]
    async fn a_typed_command_beside_a_running_turn_leaves_that_turn_alone() {
        let dir = tempfile::tempdir().unwrap();
        let _home = isolated_home(dir.path());
        let runtime = Runtime::start(config_for(dir.path())).await.unwrap();
        let handle = runtime.handle();
        let session = handle.snapshot().sessions[0].id.clone();
        handle.set_mode(&session, PermissionMode::Yolo).unwrap();
        let live = handle.inner.session(&session).unwrap();
        live.set_turn(crate::types::TurnState::Streaming);

        let mut rec = Recorder::new(&handle);
        handle
            .run_user_command(&session, "echo beside-a-turn".into())
            .unwrap();
        assert!(
            rec.wait_until(|texts, _| texts.iter().any(|t| t.contains("beside-a-turn")))
                .await,
            "the command still runs while a turn is in flight"
        );
        assert_eq!(
            live.turn(),
            crate::types::TurnState::Streaming,
            "a side command must not claim to own the state of a turn already running"
        );
    }

    #[tokio::test]
    async fn choosing_a_model_moves_the_live_choice_and_everything_that_reads_it() {
        let dir = tempfile::tempdir().unwrap();
        let _home = isolated_home(dir.path());
        let runtime = Runtime::start(config_for(dir.path())).await.unwrap();
        let handle = runtime.handle();
        let session = handle.snapshot().sessions[0].id.clone();

        handle
            .set_model(&session, "qwen2.5-coder:32b".into())
            .unwrap();

        // The name a turn is built with, the row the session was stored under, and the file a
        // restart reads are one decision, not three places that can disagree.
        assert_eq!(handle.inner.model(), "qwen2.5-coder:32b");
        assert_eq!(
            handle.snapshot().metrics.model.as_deref(),
            Some("qwen2.5-coder:32b")
        );
        let saved: Config = serde_json::from_str(
            &std::fs::read_to_string(dir.path().join("home").join("config.json")).unwrap(),
        )
        .unwrap();
        assert_eq!(saved.model, "qwen2.5-coder:32b");
        assert_eq!(
            handle.snapshot().sessions[0].model.as_deref(),
            Some("qwen2.5-coder:32b"),
            "the session row carries the model it is on"
        );
    }

    #[tokio::test]
    async fn choosing_a_posture_is_remembered_across_a_restart() {
        let dir = tempfile::tempdir().unwrap();
        let _home = isolated_home(dir.path());
        let runtime = Runtime::start(config_for(dir.path())).await.unwrap();
        let handle = runtime.handle();
        let session = handle.snapshot().sessions[0].id.clone();

        handle
            .set_mode(&session, PermissionMode::AcceptEdits)
            .unwrap();

        // The live posture, the one a view draws, and the file a restart reads are one decision.
        assert_eq!(handle.inner.mode(), PermissionMode::AcceptEdits);
        assert_eq!(handle.snapshot().mode, PermissionMode::AcceptEdits);
        let saved: Config = serde_json::from_str(
            &std::fs::read_to_string(dir.path().join("home").join("config.json")).unwrap(),
        )
        .unwrap();
        assert_eq!(
            saved.mode,
            PermissionMode::AcceptEdits,
            "a restart starts on the posture the operator picked, not on the default"
        );
    }

    #[tokio::test]
    async fn a_blank_model_name_is_refused_instead_of_becoming_the_choice() {
        let dir = tempfile::tempdir().unwrap();
        let _home = isolated_home(dir.path());
        let runtime = Runtime::start(config_for(dir.path())).await.unwrap();
        let handle = runtime.handle();
        let session = handle.snapshot().sessions[0].id.clone();

        assert!(handle.set_model(&session, "   ".into()).is_err());
        assert_eq!(handle.inner.model(), "test-model");
    }

    #[tokio::test]
    async fn hive_create_participants_messages_work() {
        let dir = tempfile::tempdir().unwrap();
        let _home = isolated_home(dir.path());
        let runtime = Runtime::start(config_for(dir.path())).await.unwrap();
        let handle = runtime.handle();

        let user = handle.inner.store.create_user("U", now_ms()).unwrap();
        let hive_id = handle
            .hive_create(
                user.id.clone(),
                None,
                "Forge".to_string(),
                "build".to_string(),
                "cooperative".to_string(),
            )
            .unwrap();
        let ent = vikram_store::EntityRow {
            id: vikram_contracts::EntityId::generate(),
            owner_user: None,
            kind: vikram_store::EntityKind::Agent,
            name: "Rudra".into(),
            slug: "rudra".into(),
            title: "Senior Developer".into(),
            role: "coder".into(),
            specialization: "rust,testing".into(),
            description: String::new(),
            icon: None,
            model: None,
            profile_id: None,
            status: "active".into(),
            created_ms: now_ms(),
            updated_ms: now_ms(),
        };
        handle.inner.store.save_entity(&ent).unwrap();
        handle
            .hive_add_participant(&hive_id, &ent.id, vikram_store::EntityKind::Agent, "member")
            .unwrap();
        assert_eq!(handle.hive_participants(&hive_id).unwrap().len(), 1);
        handle
            .hive_append_message(&hive_id, "rudra", "agent", "hello", None)
            .unwrap();
        assert_eq!(handle.hive_messages(&hive_id).unwrap().len(), 1);
        handle.hive_delete(&hive_id).unwrap();
        assert!(handle.hive_get(&hive_id).unwrap().is_none());
    }

    #[tokio::test]
    async fn hive_run_marks_peer_output_as_model_trust_not_user() {
        let dir = tempfile::tempdir().unwrap();
        let _home = isolated_home(dir.path());
        let (runtime, provider) = named_runtime(dir.path(), "ack").await;
        let handle = runtime.handle();

        let user = handle.inner.store.create_user("U", now_ms()).unwrap();
        let hive_id = handle
            .hive_create(
                user.id.clone(),
                None,
                "Forge".to_string(),
                "build".to_string(),
                "cooperative".to_string(),
            )
            .unwrap();
        for (name, slug) in [("Rudra", "rudra"), ("Meera", "meera")] {
            let ent = vikram_store::EntityRow {
                id: vikram_contracts::EntityId::generate(),
                owner_user: None,
                kind: vikram_store::EntityKind::Agent,
                name: name.into(),
                slug: slug.into(),
                title: String::new(),
                role: String::new(),
                specialization: String::new(),
                description: String::new(),
                icon: None,
                model: None,
                profile_id: None,
                status: "active".into(),
                created_ms: now_ms(),
                updated_ms: now_ms(),
            };
            handle.inner.store.save_entity(&ent).unwrap();
            handle
                .hive_add_participant(&hive_id, &ent.id, vikram_store::EntityKind::Agent, "member")
                .unwrap();
        }

        handle
            .hive_run(&hive_id, "initial prompt", false)
            .await
            .unwrap();

        let requests = provider.asked();
        let hive_requests: Vec<_> = requests
            .iter()
            .filter(|msgs| msgs.iter().any(|m| m.text_body() == "initial prompt"))
            .collect();
        assert_eq!(hive_requests.len(), 2, "one request per participant");
        // The second participant's request carries the first participant's reply;
        // only that request can contain an `ack` transcript line.
        let second = hive_requests
            .iter()
            .find(|msgs| msgs.iter().any(|m| m.text_body() == "ack"))
            .expect("a later participant sees the earlier reply");
        let peer = second
            .iter()
            .find(|m| m.text_body() == "ack")
            .expect("the peer reply is in the transcript");
        assert_eq!(peer.role, vikram_contracts::Role::User);
        assert_eq!(
            peer.blocks.iter().find_map(|b| match b {
                Block::Text { trust, .. } => Some(*trust),
                _ => None,
            }),
            Some(Trust::Model),
            "another participant's model output must not carry user trust"
        );
        let prompt = second
            .iter()
            .find(|m| m.text_body() == "initial prompt")
            .expect("the relayed operator prompt is in the transcript");
        assert_eq!(
            prompt.blocks.iter().find_map(|b| match b {
                Block::Text { trust, .. } => Some(*trust),
                _ => None,
            }),
            Some(Trust::User),
            "the operator's own prompt stays user-trusted"
        );
    }

    #[tokio::test]
    async fn context_loader_scaffolds_and_reads() {
        let dir = tempfile::tempdir().unwrap();
        let _home = isolated_home(dir.path());
        let handle = Runtime::start(config_for(dir.path()))
            .await
            .unwrap()
            .handle();
        let base = crate::config::vikram_home();
        let ctx = handle.context();
        let entity_dir = base.join("agents").join("rudra");
        ctx.scaffold_entity(&entity_dir, "Rudra", "agent", "Senior Developer")
            .unwrap();
        assert!(entity_dir.join("SOUL.md").exists());
        let bundle = ctx.load_agent("rudra", None, Some("task ctx"));
        assert!(!bundle.sections.is_empty());
        assert!(bundle.render().contains("SOUL"));
    }

    /// What the runtime published about a command. The `$ command` line, the outcome and any
    /// approval all arrive on the event stream, because the command itself runs on its own task.
    struct Recorder {
        events: broadcast::Receiver<RuntimeEvent>,
        texts: Vec<String>,
        approvals: Vec<ApprovalId>,
    }

    impl Recorder {
        fn new(handle: &RuntimeHandle) -> Self {
            Self {
                events: handle.events(),
                texts: Vec::new(),
                approvals: Vec::new(),
            }
        }

        /// Drain until `stop` is satisfied or five seconds pass, so a test waits on the evidence it
        /// asserts rather than on a fixed sleep.
        async fn wait_until(&mut self, stop: impl Fn(&[String], &[ApprovalId]) -> bool) -> bool {
            let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(5);
            while !stop(&self.texts, &self.approvals) {
                if tokio::time::Instant::now() >= deadline {
                    return false;
                }
                let Ok(Ok(event)) =
                    tokio::time::timeout(std::time::Duration::from_millis(100), self.events.recv())
                        .await
                else {
                    continue;
                };
                match event {
                    RuntimeEvent::MessageCommitted { message, .. } => {
                        if let Some(Block::Text { text, .. }) = message.blocks.first() {
                            self.texts.push(text.clone());
                        }
                    }
                    RuntimeEvent::ApprovalRequested(prompt) => self.approvals.push(prompt.approval),
                    _ => {}
                }
            }
            true
        }
    }

    #[tokio::test]
    async fn a_typed_command_stops_at_the_prompt_and_then_really_runs() {
        let dir = tempfile::tempdir().unwrap();
        let _home = isolated_home(dir.path());
        let runtime = Runtime::start(config_for(dir.path())).await.unwrap();
        let handle = runtime.handle();
        let session = handle.snapshot().sessions[0].id.clone();
        let mut rec = Recorder::new(&handle);

        handle
            .run_user_command(&session, "touch before-approval".into())
            .unwrap();

        assert!(
            rec.wait_until(|_, approvals| !approvals.is_empty()).await,
            "`ask` mode must put a typed command to the human"
        );
        assert!(
            !dir.path().join("before-approval").exists(),
            "a command that is still awaiting approval must not have run"
        );

        let approval = rec.approvals[0].clone();
        handle.answer_approval(&approval, true, false).unwrap();
        assert!(
            rec.wait_until(|_, _| dir.path().join("before-approval").exists())
                .await,
            "answering yes has to run the command for real, not report that it did"
        );
    }

    #[tokio::test]
    async fn a_saved_deny_rule_refuses_a_typed_command_without_asking() {
        let dir = tempfile::tempdir().unwrap();
        let _home = isolated_home(dir.path());
        let runtime = Runtime::start(config_for(dir.path())).await.unwrap();
        let handle = runtime.handle();
        let session = handle.snapshot().sessions[0].id.clone();
        handle
            .allow(&session, "deny execute shell.run resource=touch *")
            .unwrap();

        let mut rec = Recorder::new(&handle);
        handle
            .run_user_command(&session, "touch refused".into())
            .unwrap();

        assert!(
            rec.wait_until(|texts, _| texts.iter().any(|t| t.contains("permission denied")))
                .await,
            "the refusal has to reach the transcript, not vanish into a task"
        );
        assert!(
            rec.approvals.is_empty(),
            "a refusal is not something to approve around"
        );
        assert!(!dir.path().join("refused").exists());
    }

    #[tokio::test]
    async fn the_posture_in_force_decides_what_a_typed_command_costs() {
        let dir = tempfile::tempdir().unwrap();
        let _home = isolated_home(dir.path());
        let runtime = Runtime::start(config_for(dir.path())).await.unwrap();
        let handle = runtime.handle();
        let session = handle.snapshot().sessions[0].id.clone();

        handle.set_mode(&session, PermissionMode::Plan).unwrap();
        let mut rec = Recorder::new(&handle);
        handle
            .run_user_command(&session, "touch in-plan".into())
            .unwrap();
        assert!(
            rec.wait_until(|texts, _| texts.iter().any(|t| t.contains("plan mode is read-only")))
                .await,
            "plan is read-only for the human's own commands too"
        );
        assert!(rec.approvals.is_empty(), "plan refuses, it does not ask");
        assert!(!dir.path().join("in-plan").exists());

        handle.set_mode(&session, PermissionMode::Yolo).unwrap();
        let mut rec = Recorder::new(&handle);
        handle
            .run_user_command(&session, "touch in-yolo".into())
            .unwrap();
        assert!(
            rec.wait_until(|_, _| dir.path().join("in-yolo").exists())
                .await,
            "`yolo` means the typed command runs without a prompt"
        );
        assert!(rec.approvals.is_empty());
    }

    #[tokio::test]
    async fn keeping_a_typed_command_on_the_list_stops_the_next_one_asking() {
        let dir = tempfile::tempdir().unwrap();
        let _home = isolated_home(dir.path());
        let runtime = Runtime::start(config_for(dir.path())).await.unwrap();
        let handle = runtime.handle();
        let session = handle.snapshot().sessions[0].id.clone();
        let mut rec = Recorder::new(&handle);

        handle
            .run_user_command(&session, "touch kept-1".into())
            .unwrap();
        assert!(
            rec.wait_until(|_, approvals| !approvals.is_empty()).await,
            "the first one asks"
        );
        let approval = rec.approvals[0].clone();
        handle.answer_approval(&approval, true, true).unwrap();
        assert!(
            rec.wait_until(|_, _| dir.path().join("kept-1").exists())
                .await,
            "keeping it still has to run this one"
        );
        assert_eq!(
            handle
                .allow_list()
                .iter()
                .filter(|rule| rule.verb_prefix.as_deref() == Some("shell.run"))
                .count(),
            1,
            "the kept shape is a rule on the shell verb, not on the whole machine"
        );

        handle
            .run_user_command(&session, "touch kept-2".into())
            .unwrap();
        assert!(
            rec.wait_until(|_, _| dir.path().join("kept-2").exists())
                .await,
            "the same shape must not ask twice"
        );
        assert_eq!(
            rec.approvals.len(),
            1,
            "a kept command is on the list; nothing re-prompts"
        );
    }

    #[tokio::test]
    async fn adding_to_the_list_without_approving_refuses_this_one_and_saves_the_rule() {
        let dir = tempfile::tempdir().unwrap();
        let _home = isolated_home(dir.path());
        let runtime = Runtime::start(config_for(dir.path())).await.unwrap();
        let handle = runtime.handle();
        let session = handle.snapshot().sessions[0].id.clone();
        let mut rec = Recorder::new(&handle);

        handle
            .run_user_command(&session, "touch remembered-1".into())
            .unwrap();
        assert!(
            rec.wait_until(|_, approvals| !approvals.is_empty()).await,
            "the first one asks"
        );
        let approval = rec.approvals[0].clone();
        handle.answer_approval(&approval, false, true).unwrap();
        assert!(
            !dir.path().join("remembered-1").exists(),
            "saving the rule must not run the command it refused"
        );
        assert_eq!(
            handle
                .allow_list()
                .iter()
                .filter(|rule| rule.verb_prefix.as_deref() == Some("shell.run"))
                .count(),
            1,
            "the rule is saved anyway, so the next shape stops asking"
        );

        handle
            .run_user_command(&session, "touch remembered-2".into())
            .unwrap();
        assert!(
            rec.wait_until(|_, _| dir.path().join("remembered-2").exists())
                .await,
            "the saved rule lets the next one through"
        );
        assert_eq!(rec.approvals.len(), 1, "and it does not ask again");
    }

    /// A provider that answers every request with the same text. Naming races the turn for the
    /// provider, so a script popping one batch per call would only prove whichever request won.
    /// A constant answer proves the naming path whatever the order, and `asked()` shows what was
    /// actually put in front of the model.
    struct NamingProvider {
        answer: String,
        requests: Mutex<Vec<Vec<Message>>>,
    }

    impl NamingProvider {
        fn new(answer: &str) -> Arc<Self> {
            Arc::new(Self {
                answer: answer.to_owned(),
                requests: Mutex::new(Vec::new()),
            })
        }

        fn asked(&self) -> Vec<Vec<Message>> {
            self.requests
                .lock()
                .unwrap_or_else(|e| {
                    eprintln!("vikram: runtime requests lock mutex poisoned; recovering");
                    e.into_inner()
                })
                .clone()
        }

        /// Whether naming itself was ever asked — the turn's own request is not a naming call.
        fn named(&self) -> bool {
            self.naming_calls() > 0
        }

        /// How many times the model was asked to name a session.
        fn naming_calls(&self) -> usize {
            self.asked()
                .iter()
                .filter(|msgs| {
                    msgs.iter()
                        .any(|m| m.text_body().contains("Write a short title"))
                })
                .count()
        }
    }

    impl Provider for NamingProvider {
        fn name(&self) -> &str {
            "naming"
        }

        fn model(&self) -> &str {
            "test-model"
        }

        fn context_window(&self) -> Option<u64> {
            Some(8192)
        }

        fn stream(
            &self,
            request: crate::provider::CompletionRequest,
        ) -> crate::provider::ProviderStream {
            self.requests
                .lock()
                .expect("requests lock")
                .push(request.messages);
            let events = vec![
                crate::provider::ProviderEvent::TextDelta(self.answer.clone()),
                crate::provider::ProviderEvent::Finished {
                    input_tokens: None,
                    output_tokens: Some(1),
                },
            ];
            Box::pin(futures_util::stream::iter(
                events
                    .into_iter()
                    .map(Ok::<_, vikram_contracts::VikramError>),
            ))
        }
    }

    fn title_of(handle: &RuntimeHandle, id: &SessionId) -> (String, String, String) {
        let view = handle
            .snapshot()
            .sessions
            .iter()
            .find(|s| s.id == *id)
            .expect("session is in the snapshot")
            .clone();
        (view.title, view.title_source, view.title_status)
    }

    /// Wait until a submitted turn has genuinely run: the model answered, the turn closed, and no
    /// background naming task is still outstanding. Waiting on the task counter alone is not enough —
    /// it reads zero before the turn has even been scheduled, so a test could assert against a state
    /// the runtime had not reached yet and pass for the wrong reason.
    async fn settle(handle: &RuntimeHandle, id: &SessionId, answers: usize) {
        let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(5);
        loop {
            let view = handle
                .snapshot()
                .sessions
                .iter()
                .find(|s| s.id == *id)
                .cloned()
                .expect("session is in the snapshot");
            let answered = view
                .messages
                .iter()
                .filter(|m| matches!(m.role, vikram_contracts::Role::Assistant))
                .count();
            let closed = matches!(
                view.turn,
                crate::types::TurnState::Idle
                    | crate::types::TurnState::Failed
                    | crate::types::TurnState::Cancelled
            );
            // A turn that could not reach a model at all leaves no reply behind, so its own end
            // state has to count as finished.
            let turned = answered >= answers
                || matches!(
                    view.turn,
                    crate::types::TurnState::Failed | crate::types::TurnState::Cancelled
                );
            if turned && closed && !handle.learning_busy() {
                return;
            }
            if tokio::time::Instant::now() >= deadline {
                panic!("the turn never settled for {id}");
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
    }

    async fn named_runtime(dir: &std::path::Path, answer: &str) -> (Runtime, Arc<NamingProvider>) {
        let provider = NamingProvider::new(answer);
        let runtime = Runtime::start_with_provider(
            config_for(dir),
            Arc::clone(&provider) as Arc<dyn Provider>,
        )
        .await
        .expect("runtime boots with the naming provider");
        (runtime, provider)
    }

    #[tokio::test]
    async fn a_new_session_is_anonymous_and_awaits_its_first_message() {
        let dir = tempfile::tempdir().unwrap();
        let _home = isolated_home(dir.path());
        let (runtime, _provider) = named_runtime(dir.path(), "Rust JWT Authentication").await;
        let handle = runtime.handle();
        let id = handle.create_session(None).unwrap();
        let (title, source, status) = title_of(&handle, &id);
        assert_eq!(
            title,
            titling::NEW_SESSION,
            "a brand new session is a placeholder"
        );
        assert_eq!(source, vikram_store::TITLE_GENERIC);
        assert_eq!(status, vikram_store::TITLE_PENDING);
    }

    #[tokio::test]
    async fn the_first_real_message_earns_a_title_and_it_is_written_to_the_store() {
        let dir = tempfile::tempdir().unwrap();
        let _home = isolated_home(dir.path());
        let (runtime, provider) = named_runtime(dir.path(), "Rust JWT Authentication").await;
        let handle = runtime.handle();
        let id = handle.create_session(None).unwrap();
        handle
            .submit(&id, "build jwt authentication in rust".into())
            .unwrap();
        settle(&handle, &id, 1).await;
        let (title, source, status) = title_of(&handle, &id);
        assert_eq!(title, "Rust JWT Authentication");
        assert_eq!(source, vikram_store::TITLE_AUTO);
        assert_eq!(status, vikram_store::TITLE_READY);
        assert!(provider.named(), "the model was asked to name the session");

        // Durable: the same name is what a fresh reader of the database finds.
        let store = vikram_store::Store::open(&database_path()).unwrap();
        let row = store.get_session(&id).unwrap().expect("session row");
        assert_eq!(row.title, "Rust JWT Authentication");
        assert_eq!(row.title_source, vikram_store::TITLE_AUTO);
    }

    #[tokio::test]
    async fn a_greeting_names_nothing_and_leaves_the_session_waiting() {
        let dir = tempfile::tempdir().unwrap();
        let _home = isolated_home(dir.path());
        let (runtime, provider) = named_runtime(dir.path(), "Login Page Bug Fix").await;
        let handle = runtime.handle();
        let id = handle.create_session(None).unwrap();
        handle.submit(&id, "hi".into()).unwrap();
        settle(&handle, &id, 1).await;
        let (title, _, status) = title_of(&handle, &id);
        assert_eq!(title, titling::NEW_SESSION, "\"hi\" is not a topic");
        assert_eq!(status, vikram_store::TITLE_PENDING);
        assert!(
            !provider.named(),
            "no model request spent naming a greeting"
        );

        // The attempt was handed back, so the next message still names it.
        handle
            .submit(&id, "fix the login page after deploy".into())
            .unwrap();
        settle(&handle, &id, 2).await;
        assert_eq!(title_of(&handle, &id).0, "Login Page Bug Fix");
    }

    #[tokio::test]
    async fn continuing_the_conversation_does_not_rename_it_again() {
        let dir = tempfile::tempdir().unwrap();
        let _home = isolated_home(dir.path());
        let (runtime, provider) = named_runtime(dir.path(), "Rust JWT Authentication").await;
        let handle = runtime.handle();
        let id = handle.create_session(None).unwrap();
        handle
            .submit(&id, "build jwt authentication in rust".into())
            .unwrap();
        settle(&handle, &id, 1).await;
        assert_eq!(title_of(&handle, &id).0, "Rust JWT Authentication");

        let naming_calls = provider.naming_calls();
        for (turn, message) in ["now add refresh tokens", "and write tests for it"]
            .iter()
            .enumerate()
        {
            handle.submit(&id, (*message).into()).unwrap();
            settle(&handle, &id, turn + 2).await;
        }
        assert_eq!(
            title_of(&handle, &id).0,
            "Rust JWT Authentication",
            "the title is derived once, not on every message"
        );
        assert_eq!(
            provider.naming_calls(),
            naming_calls,
            "the two extra turns asked for answers, never for another title"
        );
    }

    /// The image surface is only real if a submitted message's attachment reaches the request. The
    /// assertion is made against every request the model was asked, because naming races the turn
    /// for the provider and a script would only prove whichever one won.
    #[tokio::test]
    async fn a_submitted_message_carries_its_images_into_the_provider_request() {
        let dir = tempfile::tempdir().unwrap();
        let _home = isolated_home(dir.path());
        let (runtime, provider) = named_runtime(dir.path(), "Window Review").await;
        let handle = runtime.handle();
        let id = handle.create_session(None).unwrap();
        let image = vikram_contracts::ImageSource::data("image/png", "iVBORw0KGgo=");
        handle
            .submit_message(
                &id,
                Message::user_with_images("what is wrong with this window?", vec![image.clone()]),
            )
            .unwrap();
        settle(&handle, &id, 1).await;

        let sent = provider
            .asked()
            .into_iter()
            .find(|messages| messages.iter().any(|m| m.images().next().is_some()))
            .expect("the turn's request carried the attachment");
        let user = sent
            .iter()
            .find(|message| message.role == vikram_contracts::Role::User)
            .expect("the user turn travels with it");
        assert!(
            user.blocks.iter().any(|block| matches!(
                block,
                Block::Image { source, trust } if *source == image && *trust == Trust::External
            )),
            "the image block is in the request: {:?}",
            user.blocks
        );
        assert_eq!(user.text_body(), "what is wrong with this window?");
    }

    /// The queue behind a running turn is text-only, so images are supported on an idle session
    /// only: a busy one refuses them rather than running the turn without its attachment.
    #[tokio::test]
    async fn a_message_with_an_image_is_refused_while_the_session_is_busy() {
        let dir = tempfile::tempdir().unwrap();
        let _home = isolated_home(dir.path());
        let (runtime, _provider) = named_runtime(dir.path(), "Window Review").await;
        let handle = runtime.handle();
        let id = handle.create_session(None).unwrap();
        let session = handle.inner.session(&id).expect("the session exists");

        session.set_turn(crate::types::TurnState::Streaming);
        let image = vikram_contracts::ImageSource::url("https://example.test/a.png");
        assert!(
            handle
                .submit_message(&id, Message::user_with_images("look", vec![image]))
                .is_err(),
            "an attachment cannot ride a text-only queue"
        );

        handle.submit(&id, "look at the tests".into()).unwrap();
        assert_eq!(
            session.queue_len(),
            1,
            "text is still queued behind the turn"
        );
    }

    #[tokio::test]
    async fn a_title_the_operator_chose_is_never_replaced_by_the_model() {
        let dir = tempfile::tempdir().unwrap();
        let _home = isolated_home(dir.path());
        let (runtime, provider) = named_runtime(dir.path(), "Rust JWT Authentication").await;
        let handle = runtime.handle();
        let id = handle.create_session(None).unwrap();
        handle
            .rename_session(&id, "Auth sprint — week 3".into())
            .unwrap();
        assert_eq!(title_of(&handle, &id).1, vikram_store::TITLE_MANUAL);

        handle
            .submit(&id, "build jwt authentication in rust".into())
            .unwrap();
        settle(&handle, &id, 1).await;

        let (title, source, _) = title_of(&handle, &id);
        assert_eq!(
            title, "Auth sprint — week 3",
            "a human's title outranks the model's"
        );
        assert_eq!(source, vikram_store::TITLE_MANUAL);
        assert!(!provider.named(), "and it is not even asked for");

        // The bar is stored, not just remembered: a fresh store read still says manual.
        let store = vikram_store::Store::open(&database_path()).unwrap();
        assert_eq!(
            store.get_session(&id).unwrap().unwrap().title_source,
            vikram_store::TITLE_MANUAL
        );
    }

    #[tokio::test]
    async fn renaming_to_nothing_is_refused_instead_of_reverting_to_a_placeholder() {
        let dir = tempfile::tempdir().unwrap();
        let _home = isolated_home(dir.path());
        let (runtime, _) = named_runtime(dir.path(), "Rust JWT Authentication").await;
        let handle = runtime.handle();
        let id = handle.create_session(None).unwrap();
        assert!(
            handle.rename_session(&id, "   ".into()).is_err(),
            "an empty rename cannot silently un-name a session"
        );
        assert_eq!(title_of(&handle, &id).0, titling::NEW_SESSION);
    }

    #[tokio::test]
    async fn retitle_asks_for_a_fresh_name_and_refuses_a_untitled_session() {
        let dir = tempfile::tempdir().unwrap();
        let _home = isolated_home(dir.path());
        let (runtime, provider) = named_runtime(dir.path(), "Hermes Self-Evolution").await;
        let handle = runtime.handle();
        let id = handle.create_session(None).unwrap();

        // Nothing to name it after yet.
        assert!(
            handle.retitle_session(&id, true).is_err(),
            "an empty conversation cannot be retitled"
        );

        handle
            .submit(&id, "research hermes self evolution".into())
            .unwrap();
        settle(&handle, &id, 1).await;
        assert_eq!(title_of(&handle, &id).0, "Hermes Self-Evolution");

        // A hand-written title is protected until the operator explicitly hands it back.
        handle.rename_session(&id, "My research".into()).unwrap();
        assert!(
            handle.retitle_session(&id, false).is_err(),
            "regenerating over a manual title needs the explicit opt-in"
        );
        handle.retitle_session(&id, true).unwrap();
        settle(&handle, &id, 1).await;
        assert_eq!(
            title_of(&handle, &id).0,
            "Hermes Self-Evolution",
            "with consent, naming runs again"
        );
        assert!(provider.named());
    }

    #[tokio::test]
    async fn with_no_model_the_session_is_still_named_from_its_own_request() {
        let dir = tempfile::tempdir().unwrap();
        let _home = isolated_home(dir.path());
        // No provider configured at all: this is the offline path, and it must not leave
        // "Session 7" behind.
        let runtime = Runtime::start(config_for(dir.path())).await.unwrap();
        let handle = runtime.handle();
        let id = handle.create_session(None).unwrap();
        handle
            .submit(&id, "build a minecraft hosting website with prices".into())
            .unwrap();
        settle(&handle, &id, 1).await;
        let (title, source, _) = title_of(&handle, &id);
        assert!(!titling::is_placeholder(&title), "got {title:?}");
        assert!(
            title.to_lowercase().contains("minecraft"),
            "the fallback still says what the session is about: {title:?}"
        );
        assert_eq!(source, vikram_store::TITLE_AUTO);
    }

    #[tokio::test]
    async fn a_model_that_ignores_the_instructions_is_not_stored_as_a_title() {
        let dir = tempfile::tempdir().unwrap();
        let _home = isolated_home(dir.path());
        // The model answered with exactly the kind of nothing the prompt forbids.
        let (runtime, _) = named_runtime(dir.path(), "General Chat").await;
        let handle = runtime.handle();
        let id = handle.create_session(None).unwrap();
        handle.submit(&id, "compare AI coding bots".into()).unwrap();
        settle(&handle, &id, 1).await;
        let (title, _, status) = title_of(&handle, &id);
        assert_ne!(title, "General Chat", "a forbidden title is refused");
        assert!(!titling::is_placeholder(&title));
        assert_eq!(status, vikram_store::TITLE_READY, "the fallback named it");
    }

    #[tokio::test]
    async fn closing_a_session_never_renumbers_or_renames_the_others() {
        let dir = tempfile::tempdir().unwrap();
        let _home = isolated_home(dir.path());
        let (runtime, _) = named_runtime(dir.path(), "Rust JWT Authentication").await;
        let handle = runtime.handle();
        let first = handle.create_session(Some("Alpha work".into())).unwrap();
        let second = handle.create_session(Some("Beta work".into())).unwrap();
        let third = handle.create_session(Some("Gamma work".into())).unwrap();

        handle.close_session(&second).unwrap();

        let titles: Vec<(String, String)> = handle
            .snapshot()
            .sessions
            .iter()
            .map(|s| (s.title.clone(), s.id.to_string()))
            .collect();
        assert!(
            !titles.iter().any(|(_, id)| *id == second.to_string()),
            "the closed session left the list"
        );
        // The runtime opens a session of its own at boot, so the survivors are looked up by the
        // identity they were given rather than counted.
        for (id, title) in [(&first, "Alpha work"), (&third, "Gamma work")] {
            assert_eq!(
                titles
                    .iter()
                    .find(|(_, listed)| *listed == id.to_string())
                    .map(|(title, _)| title.clone()),
                Some(title.to_owned()),
                "closing a different session never moves this one's name or id"
            );
        }

        // A session made after the deletion does not inherit the freed slot's name.
        let fourth = handle.create_session(None).unwrap();
        assert_eq!(title_of(&handle, &fourth).0, titling::NEW_SESSION);
        assert_ne!(fourth, second, "an identity is never handed out twice");
    }

    #[tokio::test]
    async fn titles_and_provenance_survive_a_restart() {
        let dir = tempfile::tempdir().unwrap();
        let _home = isolated_home(dir.path());
        let (runtime, _) = named_runtime(dir.path(), "Minecraft Hosting Website").await;
        let handle = runtime.handle();
        let auto = handle.create_session(None).unwrap();
        handle
            .submit(
                &auto,
                "i need a site for my minecraft server hosting".into(),
            )
            .unwrap();
        settle(&handle, &auto, 1).await;
        let manual = handle.create_session(None).unwrap();
        handle
            .rename_session(&manual, "Quarterly review".into())
            .unwrap();
        drop(runtime);

        // Reopen the same home exactly the way a fresh `vikram` process does.
        let runtime = Runtime::start(config_for(dir.path())).await.unwrap();
        let handle = runtime.handle();
        let sessions = handle.snapshot().sessions;
        let reopened_auto = sessions
            .iter()
            .find(|s| s.id == auto)
            .expect("auto session restored");
        let reopened_manual = sessions
            .iter()
            .find(|s| s.id == manual)
            .expect("manual session restored");
        assert_eq!(reopened_auto.title, "Minecraft Hosting Website");
        assert_eq!(reopened_auto.title_source, vikram_store::TITLE_AUTO);
        assert_eq!(reopened_manual.title, "Quarterly review");
        assert_eq!(reopened_manual.title_source, vikram_store::TITLE_MANUAL);

        // And the restored manual title is still protected from naming.
        handle
            .submit(&manual, "more messages about something else".into())
            .unwrap();
        settle(&handle, &manual, 1).await;
        assert_eq!(title_of(&handle, &manual).0, "Quarterly review");
    }

    #[tokio::test]
    async fn reload_config_picks_up_a_changed_model_from_disk() {
        let dir = tempfile::tempdir().unwrap();
        let _home = isolated_home(dir.path());
        let runtime = Runtime::start(Config {
            workspace_root: Some(dir.path().to_path_buf()),
            ..Config::default()
        })
        .await
        .unwrap();
        let handle = runtime.handle();

        let cfg = Config {
            model: "reloaded-model".into(),
            workspace_root: Some(dir.path().to_path_buf()),
            ..Config::default()
        };
        cfg.save().unwrap();

        handle.reload_config().unwrap();
        assert_eq!(handle.config().model, "reloaded-model");
    }

    #[tokio::test]
    async fn reload_skills_rescans_and_reports_the_new_count() {
        let dir = tempfile::tempdir().unwrap();
        let _home = isolated_home(dir.path());
        let runtime = Runtime::start(Config {
            workspace_root: Some(dir.path().to_path_buf()),
            ..Config::default()
        })
        .await
        .unwrap();
        let handle = runtime.handle();

        let skill_dir = dir.path().join("skills").join("alpha");
        std::fs::create_dir_all(&skill_dir).unwrap();
        std::fs::write(
            skill_dir.join("SKILL.md"),
            "---\nname: alpha\ndescription: desc\n---\nbody",
        )
        .unwrap();

        let before = handle.skill_count();
        let reloaded = handle.reload_skills();
        assert!(reloaded > before, "reloaded={reloaded} before={before}");
        assert!(handle.skill_list().iter().any(|s| s.name == "alpha"));
    }

    /// A provider that accepts the request and then says nothing — a turn against it stays in
    /// flight until something cancels it, which is exactly what the bot-control tests need.
    struct HangingProvider;

    impl Provider for HangingProvider {
        fn name(&self) -> &str {
            "hanging"
        }

        fn model(&self) -> &str {
            "test-model"
        }

        fn context_window(&self) -> Option<u64> {
            Some(8192)
        }

        fn stream(
            &self,
            _request: crate::provider::CompletionRequest,
        ) -> crate::provider::ProviderStream {
            Box::pin(futures_util::stream::pending::<
                std::result::Result<crate::provider::ProviderEvent, vikram_contracts::VikramError>,
            >())
        }
    }

    /// Wait until the bot's delegated session is actually mid-turn.
    async fn bot_turn_busy(handle: &RuntimeHandle, bot: &BotId) -> SessionId {
        let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(5);
        loop {
            let agent = handle.inner.bots.get(bot).expect("bot is registered");
            if let Some(id) = agent
                .session
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .clone()
                && let Some(session) = handle.inner.session(&id)
                && session.is_busy()
            {
                return id;
            }
            if tokio::time::Instant::now() >= deadline {
                panic!("the delegated turn never started");
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
    }

    #[tokio::test]
    async fn stopping_a_bot_cancels_the_turn_it_is_actually_running() {
        let dir = tempfile::tempdir().unwrap();
        let _home = isolated_home(dir.path());
        let runtime =
            Runtime::start_with_provider(config_for(dir.path()), Arc::new(HangingProvider))
                .await
                .unwrap();
        let handle = runtime.handle();
        let parent = handle.snapshot().sessions[0].id.clone();
        let bot = handle
            .bot_create("Stopbot", "tester", "stops things")
            .unwrap();
        handle
            .spawn_agent(bot.clone(), "watch the build".into(), parent.clone())
            .unwrap();
        let child = bot_turn_busy(&handle, &bot).await;
        let mut events = handle.events();

        // A second spawn while the first runs must be refused, not raced.
        assert!(
            handle
                .spawn_agent(bot.clone(), "again".into(), parent.clone())
                .is_err(),
            "one run per bot"
        );

        handle.control_bot(&bot, BotControl::Stop).unwrap();

        // `Stopped` is set synchronously by the control call; what proves the stop reached the
        // turn is the turn itself ending Cancelled.
        let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(5);
        let mut outcome = None;
        while tokio::time::Instant::now() < deadline {
            let Ok(Ok(event)) =
                tokio::time::timeout(std::time::Duration::from_millis(100), events.recv()).await
            else {
                continue;
            };
            if let RuntimeEvent::TurnFinished { state, .. } = event {
                outcome = Some(state);
                break;
            }
        }
        assert_eq!(
            outcome,
            Some(crate::types::TurnState::Cancelled),
            "the in-flight turn observed the stop"
        );
        let agent = handle.inner.bots.get(&bot).expect("bot is registered");
        assert_eq!(agent.state(), BotState::Stopped);
        // The reaped child may already be gone from the live set; if it is still there its turn
        // must not still be streaming.
        if let Some(session) = handle.inner.session(&child) {
            assert_ne!(session.turn(), crate::types::TurnState::Streaming);
        }
    }

    #[tokio::test]
    async fn a_delegated_session_is_closed_once_it_has_answered() {
        let dir = tempfile::tempdir().unwrap();
        let _home = isolated_home(dir.path());
        let (runtime, _provider) = named_runtime(dir.path(), "the audit is clean").await;
        let handle = runtime.handle();
        let parent = handle.snapshot().sessions[0].id.clone();
        let bot = handle
            .bot_create("Auditbot", "auditor", "audits things")
            .unwrap();
        handle
            .spawn_agent(bot.clone(), "audit the code".into(), parent)
            .unwrap();
        // `spawn_agent` records the child session synchronously; a fast turn may already be
        // done, so there is nothing to wait for but the reap itself.
        let child = {
            let agent = handle.inner.bots.get(&bot).expect("bot is registered");
            agent
                .session
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .clone()
                .expect("the delegated session is recorded")
        };

        // Wait for the delegated turn to finish and the child to be reaped.
        let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(10);
        loop {
            if handle.inner.session(&child).is_none() {
                break;
            }
            if tokio::time::Instant::now() >= deadline {
                panic!("the delegated session was never reaped");
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        let store = vikram_store::Store::open(&database_path()).unwrap();
        let row = store.get_session(&child).unwrap().expect("row survives");
        assert_eq!(row.status, "closed", "closed, not deleted");
    }

    fn seeded_session(
        handle: &RuntimeHandle,
        text: &str,
    ) -> (SessionId, std::sync::Arc<crate::session::SessionRuntime>) {
        let id = handle.snapshot().sessions[0].id.clone();
        let session = handle.inner.session(&id).expect("session is live");
        session.push_message(vikram_contracts::Message::user(text), 1);
        (id, session)
    }

    fn remember_correction(handle: &RuntimeHandle, scope: &str, value: &str) {
        let engine = handle.evolution();
        engine
            .memory
            .remember(
                vikram_evolution::MemoryLayer::User,
                scope,
                &format!("correction-test-{}", value.len()),
                value,
                vikram_evolution::KnowledgeStatus::UserStated,
                0.9,
                None,
                "test",
            )
            .expect("evolution memory accepts the entry");
    }

    #[tokio::test]
    async fn retrieved_memory_surfaces_evolution_layered_memories() {
        let dir = tempfile::tempdir().unwrap();
        let _home = isolated_home(dir.path());
        let runtime = Runtime::start(config_for(dir.path())).await.unwrap();
        let handle = runtime.handle();
        let (_id, session) = seeded_session(&handle, "check the deployment checklist again");
        remember_correction(&handle, "agent:main", "always run migrations before deploy");
        // A different agent's scope must never leak into this session's recall.
        remember_correction(&handle, "agent:other", "other agent secret note");

        let block = handle
            .inner
            .retrieved_memory(&session)
            .expect("the seeded correction produces a memory block");
        assert!(block.contains("always run migrations before deploy"));
        assert!(block.contains("[evolution]"));
        assert!(!block.contains("other agent secret note"));
    }

    #[tokio::test]
    async fn retrieved_memory_does_not_double_inject_a_correction_in_both_stores() {
        let dir = tempfile::tempdir().unwrap();
        let _home = isolated_home(dir.path());
        let runtime = Runtime::start(config_for(dir.path())).await.unwrap();
        let handle = runtime.handle();
        let (id, session) = seeded_session(&handle, "check the deployment checklist again");
        let shared = "deployment checklist says run migrations first";
        handle
            .inner
            .memory
            .save("deploy-notes", shared, id.as_str())
            .expect("memory store accepts the entry");
        remember_correction(&handle, "agent:main", shared);

        let block = handle
            .inner
            .retrieved_memory(&session)
            .expect("the shared correction produces a memory block");
        assert_eq!(
            block.matches(shared).count(),
            1,
            "the same correction text must be injected once, not once per store"
        );
    }

    #[tokio::test]
    async fn retrieved_memory_respects_the_shared_memory_budget() {
        let dir = tempfile::tempdir().unwrap();
        let _home = isolated_home(dir.path());
        let runtime = Runtime::start(config_for(dir.path())).await.unwrap();
        let handle = runtime.handle();
        let (_id, session) = seeded_session(&handle, "check the deployment checklist again");
        remember_correction(&handle, "agent:main", "always run migrations before deploy");
        handle
            .inner
            .config
            .write()
            .unwrap_or_else(|e| e.into_inner())
            .context
            .memory_budget_tokens = 1;

        let block = handle.inner.retrieved_memory(&session);
        assert!(
            block
                .map(|b| !b.contains("always run migrations before deploy"))
                .unwrap_or(true),
            "a one-token budget cannot fit even the section header"
        );
    }

    #[tokio::test]
    async fn retrieved_memory_survives_the_in_memory_evolution_fallback() {
        let dir = tempfile::tempdir().unwrap();
        let _home = isolated_home(dir.path());
        // A directory where evolution.db belongs makes the durable open fail, so the
        // runtime falls back to an in-memory engine — recall must still be safe.
        std::fs::create_dir_all(dir.path().join("home").join("evolution.db")).unwrap();
        let runtime = Runtime::start(config_for(dir.path())).await.unwrap();
        let handle = runtime.handle();
        let (_id, session) = seeded_session(&handle, "check the deployment checklist again");
        remember_correction(&handle, "agent:main", "always run migrations before deploy");

        let block = handle
            .inner
            .retrieved_memory(&session)
            .expect("the in-memory engine still answers recall");
        assert!(block.contains("always run migrations before deploy"));
    }

    fn failing_experience(i: usize) -> vikram_evolution::Experience {
        vikram_evolution::Experience {
            id: String::new(),
            timestamp_ms: 0,
            agent_id: "main".to_owned(),
            project_id: None,
            task_category: "build".to_owned(),
            user_request: format!("failing request {i}"),
            outcome: vikram_evolution::ExperienceOutcome::Failure,
            tools_used: vec!["shell".to_owned()],
            errors: vec!["boom".to_owned()],
            corrections: Vec::new(),
            feedback: None,
            reusable: false,
            sensitivity: vikram_evolution::Sensitivity::Private,
            metadata: serde_json::json!({}),
        }
    }

    #[tokio::test]
    async fn self_check_reports_unverified_health_on_a_fresh_store() {
        let dir = tempfile::tempdir().unwrap();
        let _home = isolated_home(dir.path());
        let runtime = Runtime::start(config_for(dir.path())).await.unwrap();
        let handle = runtime.handle();

        let report = handle
            .inner
            .run_self_check(true)
            .await
            .expect("a fresh store still answers the read-only probes");
        assert_eq!(report.status, vikram_evolution::HealthStatus::Unknown);
        assert!(
            report.opportunities.is_empty(),
            "no fabricated opportunities on an empty store"
        );
        assert!(
            report
                .opportunity_note
                .as_deref()
                .is_some_and(|n| n.contains("no improvement opportunities")),
            "the scan explains why the list is empty"
        );
    }

    #[tokio::test]
    async fn self_check_surfaces_recorded_failures_as_a_warn_notice() {
        let dir = tempfile::tempdir().unwrap();
        let _home = isolated_home(dir.path());
        let runtime = Runtime::start(config_for(dir.path())).await.unwrap();
        let handle = runtime.handle();
        {
            let engine = handle.evolution();
            for i in 0..vikram_evolution::MIN_SAMPLE {
                engine
                    .experience
                    .record(failing_experience(i))
                    .expect("experience records");
            }
        }
        let mut events = handle.events();

        let report = handle
            .inner
            .run_self_check(true)
            .await
            .expect("recorded failures are still readable");
        assert_eq!(report.status, vikram_evolution::HealthStatus::Unhealthy);
        assert!(
            !report.opportunities.is_empty(),
            "failing 'build' experiences must produce an opportunity"
        );

        handle.inner.emit_self_check(report);
        let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(5);
        let mut warn = None;
        while tokio::time::Instant::now() < deadline {
            let Ok(Ok(event)) =
                tokio::time::timeout(std::time::Duration::from_millis(100), events.recv()).await
            else {
                continue;
            };
            if let RuntimeEvent::Notice { level, text } = event
                && level == NoticeLevel::Warn
                && text.contains("self-check")
            {
                warn = Some(text);
                break;
            }
        }
        assert!(
            warn.is_some(),
            "unhealthy findings must surface as a Warn notice"
        );
    }

    #[tokio::test]
    async fn self_check_interval_fires_and_cannot_be_scheduled_twice() {
        let dir = tempfile::tempdir().unwrap();
        let _home = isolated_home(dir.path());
        let runtime = Runtime::start_with_self_check_interval(
            config_for(dir.path()),
            std::time::Duration::from_millis(15),
        )
        .await
        .unwrap();
        let handle = runtime.handle();

        let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(5);
        while handle.inner.self_check_ticks.load(Ordering::SeqCst) < 3 {
            if tokio::time::Instant::now() >= deadline {
                panic!("the self-check interval never fired");
            }
            tokio::time::sleep(std::time::Duration::from_millis(5)).await;
        }
        assert!(
            !handle
                .inner
                .spawn_self_check(std::time::Duration::from_millis(1)),
            "boot already scheduled the check; a second spawn must be refused"
        );

        // The durable trail proves a tick actually ran the store probes, not just the counter.
        let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(5);
        loop {
            let events = handle
                .evolution()
                .store
                .list_events(Some("self-check"), 10)
                .unwrap_or_default();
            if events.iter().any(|e| e.kind == "self_check") {
                break;
            }
            if tokio::time::Instant::now() >= deadline {
                panic!("no self_check event reached the evolution store");
            }
            tokio::time::sleep(std::time::Duration::from_millis(5)).await;
        }
    }

    #[tokio::test]
    async fn self_check_recovers_a_poisoned_evolution_lock() {
        let dir = tempfile::tempdir().unwrap();
        let _home = isolated_home(dir.path());
        let runtime = Runtime::start(config_for(dir.path())).await.unwrap();
        let handle = runtime.handle();

        let poisoned = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let _held = handle.evolution();
            panic!("poison the evolution lock on purpose");
        }))
        .is_err();
        assert!(poisoned, "the test must actually poison the mutex");

        let report = handle
            .inner
            .run_self_check(false)
            .await
            .expect("a poisoned lock is recovered, not a check failure");
        assert_eq!(report.status, vikram_evolution::HealthStatus::Unknown);
        assert!(report.opportunity_note.is_none(), "no scan was asked for");
    }
}
