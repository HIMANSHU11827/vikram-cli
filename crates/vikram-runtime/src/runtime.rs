//! Runtime facade.
//!
//! `Runtime` owns the process-wide state; `RuntimeHandle` is the cheap clone every view and task
//! holds. The handle exposes observations and explicit controls only — there is no method that
//! stops work as a side effect of changing what is on screen.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use tokio::sync::{broadcast, oneshot};
use vikram_contracts::journal::JournalSink;
use vikram_contracts::{
    ActionKind, AgentId, ApprovalId, Decision, Message, PermissionMode, PermissionRequest, Posture,
    RecordKind, SessionId,
};
use vikram_policy::{MandatoryFloor, Policy, ResourcePattern, Rule};
use vikram_store::Store;

use crate::agents::{AgentManager, ManagedAgent, TaskManager};
use crate::bus::EventBus;
use crate::config::{Config, McpConfig, ModeAvailability, database_path, ensure_home};
use crate::error::{Result, RuntimeError};
use crate::mcp::McpRegistry;
use crate::plugins::PluginRegistry;
use crate::provider::Provider;
use crate::rules::RuleBook;
use crate::session::SessionRuntime;
use crate::skills::{Skill, SkillRegistry};
use crate::teams::{Team, TeamManager};
use crate::tickets::TicketManager;
use crate::titling;
use crate::tools::{McpTool, ToolContext, ToolRegistry};
use crate::types::{
    AgentSnapshot, AgentState, ApprovalPrompt, BackgroundTask, Metrics, NoticeLevel, RuntimeEvent,
    RuntimeStatus, SessionSnapshot, StateSnapshot, TaskState, TeamSnapshot, now_ms,
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
pub enum AgentControl {
    Pause,
    Resume,
    Stop,
}

pub(crate) struct RuntimeInner {
    pub config: Config,
    pub store: Store,
    pub memory: crate::MemoryStore,
    pub context: Arc<crate::context::ContextLoader>,
    pub hive: crate::HiveManager,
    pub learning: crate::LearningManager,
    pub bus: EventBus,
    pub tools: ToolRegistry,
    pub provider: std::sync::RwLock<Option<Arc<dyn Provider>>>,
    pub sessions: Mutex<Vec<Arc<SessionRuntime>>>,
    pub agents: AgentManager,
    pub tasks: TaskManager,
    pub teams: TeamManager,
    pub tickets: TicketManager,
    pub mcp: std::sync::Arc<std::sync::Mutex<McpRegistry>>,
    pub mcp_visibility: std::sync::Mutex<VisibilityManager>,
    pub plugins: Mutex<PluginRegistry>,
    pub skills: std::sync::Mutex<SkillRegistry>,
    pub evolution: std::sync::Mutex<vikram_evolution::EvolutionEngine>,
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
        inner
            .session(self)
            .map(|s| s.messages().len())
            .unwrap_or(0)
    }
}

impl RuntimeInner {
    pub fn emit(&self, event: RuntimeEvent) {
        self.bus.publish(event);
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
        self.sessions.lock().expect("sessions lock").clone()
    }

    pub fn publish_session(&self, session: &SessionRuntime) {
        self.emit(RuntimeEvent::SessionUpdated(session.snapshot()));
    }

    pub fn publish_agent(&self, agent: &ManagedAgent) {
        self.emit(RuntimeEvent::AgentChanged(agent.snapshot()));
    }

    pub fn touch(&self, session: &SessionRuntime) {
        let _ = self
            .store
            .touch_session(&session.id, session.updated_ms.load(Ordering::Relaxed));
    }

    /// Whether this session has crossed a memory-nudge boundary: its message count is a positive
    /// multiple of the configured `memory_nudge_interval`. A zero interval never nudges.
    pub fn memory_nudge_due<S: MemoryNudgeSession>(&self, session: &S) -> bool {
        let interval = self.config.memory_nudge_interval;
        if interval == 0 {
            return false;
        }
        let count = session.message_count(self) as u64;
        count > 0 && count % interval == 0
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
        let saved = self.rules.lock().expect("rules lock").snapshot();
        Policy::with_defaults(saved, defaults, floor)
    }

    pub fn mode(&self) -> PermissionMode {
        *self.mode.lock().expect("mode lock")
    }

    /// The model name every request is built with. Live, because the interface can pick one at any
    /// moment; the provider object owns the endpoint and the key, not the choice.
    pub fn model(&self) -> String {
        self.model.lock().expect("model lock").clone()
    }

    /// The whole decision for one request, in one place: the evaluator answers what the rules say,
    /// then the posture in force decides what to do with a prompt. Both entry points — the turn
    /// loop and the kernel dispatch — go through this, so a view cannot invent its own gate.
    pub fn judge(&self, request: &PermissionRequest) -> Decision {
        let policy = self.policy();
        let raw = policy.evaluate(request);
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
        *self.mode.lock().expect("mode lock") = mode;
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
        let context_window = context_window.or(self.config.context_window);
        let turn_elapsed_ms = sessions
            .iter()
            .filter_map(|s| s.turn_elapsed_ms(now_ms()))
            .max()
            .or(*self.last_turn_ms.lock().expect("turn metric"));
        let mcp = self.mcp.lock().expect("mcp lock");
        let plugins = self.plugins.lock().expect("plugins lock");
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
            tokens_per_sec: *self.last_tokens_per_sec.lock().expect("tps metric"),
            background_tasks: self.tasks.running_count(),
            mcp_connected: mcp.connected_count(),
            mcp_configured: mcp.configured_count(),
            plugins_loaded: plugins.loaded(),
            plugins_installed: plugins.installed(),
            subagents_active: self.agents.active_count(),
            skills_discovered: crate::skills::SkillRegistry::for_workspace(&self.workspace_root)
                .skills()
                .len(),
            skills_loaded: crate::skills::SkillRegistry::for_workspace(&self.workspace_root)
                .skills()
                .iter()
                .filter(|s| s.loaded)
                .count(),
            thinking: self.config.thinking,
        }
    }

    pub fn publish_metrics(&self) {
        self.emit(RuntimeEvent::Metrics(self.metrics()));
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

    /// Relevant long-term memory for this turn, inside the configured budget, or nothing at all.
    ///
    /// Terms come from the most recent instruction rather than the whole transcript, because the
    /// thing worth remembering is what the agent is being asked about now.
    pub fn retrieved_memory(&self, session: &Arc<SessionRuntime>) -> Option<String> {
        if !self.config.context.memory_retrieval {
            return None;
        }
        let transcript = session.messages();
        let query = transcript
            .iter()
            .rev()
            .find(|m| m.role == vikram_contracts::Role::User)
            .map(|m| m.text_body())
            .unwrap_or_default();
        if query.trim().is_empty() {
            return None;
        }
        let terms = Self::retrieval_terms(&query);
        // A conversation may draw on shared `global` notes and its own; nothing else in the shared
        // table. Entity/user-scoped recall waits until the host, not the model, assigns those scopes.
        let scopes = vec!["global".to_owned(), session.id.as_str().to_owned()];
        let Ok(entries) = self.memory.recall_terms(&terms, &scopes, 8) else {
            return None;
        };
        // Also pull free-form notes from MEMORY.md / USER.md (Hermes-style), matching terms.
        let mut file_entries: Vec<(String, String, String)> = Vec::new();
        let home = crate::config::vikram_home();
        let mf = crate::memory_files::memory_file(&home);
        let uf = crate::memory_files::user_file(&home);
        for (scope, e) in [("memory", mf), ("user", uf)] {
            for entry in e.load() {
                let lower = entry.to_lowercase();
                if terms.iter().any(|t| lower.contains(&t.to_lowercase())) {
                    file_entries.push((scope.to_owned(), scope.to_owned(), entry));
                }
            }
        }
        let mut all_entries = entries;
        all_entries.extend(file_entries);
        if all_entries.is_empty() {
            return None;
        }
        // use all_entries below
        let entries = all_entries;
        let estimator = session.estimator();
        let budget_tokens = self.config.context.memory_budget_tokens;
        let mut block = String::from(
            "Relevant memory (durable notes from earlier sessions; may be stale, verify before relying on it):\n",
        );
        let mut spent = estimator.estimate_text(&block);
        let mut used = 0usize;
        for (scope, key, value) in entries {
            let line = format!("- [{scope}] {key}: {value}\n");
            let cost = estimator.estimate_text(&line);
            if spent + cost > budget_tokens {
                break;
            }
            spent += cost;
            used += 1;
            block.push_str(&line);
        }
        if used == 0 {
            return None;
        }
        Some(block)
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
        terms.sort_by(|a, b| b.len().cmp(&a.len()));
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
            self.config.context_window,
            advertised,
            self.config.context.safety_margin_percent,
        );
        let budget = crate::context_engine::ContextBudget::new(
            limits,
            self.request_overhead(&estimator, prefix),
            self.config.max_output_tokens.map(u64::from),
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
        let policy = CompactionPolicy::from_config(&self.config.context);
        let mut transcript = session.messages();
        let usable = budget.usable_input();

        if self.config.context.auto_compact
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

        if !self.config.context.auto_compact && usable > 0 {
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
        let policy = crate::compaction::CompactionPolicy::from_config(&self.config.context);
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
        ToolContext::new(self.workspace_root.clone(), &self.config.security.sandbox)
    }

    /// A context that can reach this session's spool, so `output.read` resolves the handles the
    /// digests in this transcript advertise and nothing else.
    pub fn tool_context_for(&self, session: &SessionId) -> ToolContext {
        let mut context = self.tool_context();
        context.spill = crate::spill::Spill::for_session(self.workspace.home(), session);
        context.session = Some(session.clone());
        context
    }

    /// Deliver a delegated agent's bounded answer to the session that asked for it.
    ///
    /// The child's transcript stays its own; what the parent receives is the final statement, fitted
    /// to `context.delegation_budget_tokens` and journaled so it survives a restart of either session.
    pub fn deliver_delegation(&self, parent: &SessionId, child: &SessionRuntime) {
        let Some(parent_session) = self.session(parent) else {
            return;
        };
        let budget = self.config.context.delegation_budget_tokens;
        let Some(answer) =
            crate::compaction::delegation_answer(&child.estimator(), &child.messages(), budget)
        else {
            return;
        };
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
        self.publish_metrics();
    }

    /// Digest a result the transcript cannot carry, keeping the full text addressable.
    ///
    /// The ceiling is `context.tool_output_ceiling`, so the operator's budget knob decides when a
    /// result is too big; the spooled text is what `output.read` serves back.
    pub fn spool_output(&self, session: &SessionId, call: &str, content: String) -> String {
        use crate::compaction::digest_tool_output;
        let ceiling = self.config.context.tool_output_ceiling as usize;
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

    pub fn create_session(&self, title: Option<String>) -> Result<Arc<SessionRuntime>> {
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
        self.sessions
            .lock()
            .expect("sessions lock")
            .push(Arc::clone(&session));
        self.emit(RuntimeEvent::SessionCreated(session.snapshot()));
        Ok(session)
    }

    pub fn status(&self) -> RuntimeStatus {
        let busy =
            self.session_list().iter().any(|s| s.is_busy()) || self.agents.active_count() > 0;
        if self.provider.read().expect("provider lock").is_none() {
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
            agents: self.agents.snapshots(),
            teams: self
                .teams
                .list()
                .into_iter()
                .map(|t| TeamSnapshot {
                    id: vikram_contracts::TeamId::new(&t.name),
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

impl RuntimeInner {
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
            if inner.learning.policy_memory() == crate::LearningPolicy::Disabled
                && inner.learning.policy_skill() == crate::LearningPolicy::Disabled
            {
                return;
            }
            let provider = match inner.provider.read().expect("provider lock").clone() {
                Some(p) => p,
                None => return,
            };
            let request = crate::provider::CompletionRequest {
                model: inner.model(),
                messages: vec![vikram_contracts::Message::user(format!(
                    "{}\n\nConversation:\n{}",
                    inner.learning.review_prompt(),
                    snapshot_text
                ))],
                tools: Vec::new(),
                max_output_tokens: inner.config.max_output_tokens,
            };
            use futures_util::StreamExt;
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
            let provider = inner.provider.read().expect("provider lock").clone();
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
        match tokio::time::timeout(std::time::Duration::from_secs(2), drain).await {
            Ok(raw) => raw,
            Err(_) => String::new(),
        }
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
        if !self.config.title_model.trim().is_empty() {
            return self.config.title_model.trim().to_owned();
        }
        self.model()
    }
}

impl Runtime {
    pub async fn start(config: Config) -> Result<Self> {
        Self::boot(config, None).await
    }

    /// Boot with a provider the caller supplies instead of the one configuration builds. Only the
    /// turn loop's tests use it: a scripted stream is the only way to prove the order in which a
    /// turn publishes speech, tool rows and tool results without a network.
    #[cfg(test)]
    pub(crate) async fn start_with_provider(
        config: Config,
        provider: Arc<dyn Provider>,
    ) -> Result<Self> {
        Self::boot(config, Some(provider)).await
    }

    async fn boot(config: Config, provider_override: Option<Arc<dyn Provider>>) -> Result<Self> {
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

        let agents = AgentManager::new();
        match store.list_agents() {
            Ok(rows) => {
                for row in rows {
                    agents.add(Arc::new(ManagedAgent::from_profile(row.into())));
                }
            }
            Err(err) => return Err(RuntimeError::Store(err.to_string())),
        }
        let provider = match provider_override {
            Some(provider) => Some(provider),
            None => crate::openai::OpenAiProvider::from_config(&config)?
                .map(|p| Arc::new(p) as Arc<dyn Provider>),
        };

        let mcp = McpRegistry::load(McpConfig::load().into_specs());
        let plugins = PluginRegistry::from_default_dir();
        let skills = std::sync::Mutex::new(SkillRegistry::for_workspace(&workspace_root));
        let evolution_skills_root = crate::config::vikram_home().join("skills");
        let _ = std::fs::create_dir_all(&evolution_skills_root);
        let evolution = vikram_evolution::EvolutionEngine::open(
            crate::config::vikram_home().join("evolution.db"),
            evolution_skills_root,
        )
        .unwrap_or_else(|_| {
            vikram_evolution::EvolutionEngine::open_in_memory(
                crate::config::vikram_home().join("skills"),
            )
            .expect("evolution in-memory")
        });

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
            config,
            store,
            memory,
            context,
            hive,
            tickets,
            learning,
            bus: EventBus::new(),
            tools: ToolRegistry::with_builtins(),
            provider: std::sync::RwLock::new(provider),
            sessions: Mutex::new(Vec::new()),
            agents,
            tasks: TaskManager::new(),
            teams: TeamManager::load(),
            mcp: Arc::new(Mutex::new(mcp)),
            mcp_visibility: Mutex::new(VisibilityManager::new()),
            plugins: Mutex::new(plugins),
            skills,
            evolution: Mutex::new(evolution),
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
        });

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
            if let Ok(records) = inner.store.replay_messages(&session.id) {
                let messages = crate::compaction::project_from_journal(
                    records
                        .into_iter()
                        .map(|record| (record.seq, record.kind, record.payload)),
                );
                *session.messages.lock().expect("messages lock") = messages;
            }
            inner.sessions.lock().expect("sessions lock").push(session);
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
            if let Ok(user) = inner.store.create_user("default", now) {
                if let Ok(profile) = inner.store.create_profile(&user.id, "default", now) {
                    let _ = inner
                        .store
                        .set_active_identity(&vikram_store::ActiveIdentity {
                            user: Some(user.id),
                            profile: Some(profile.id),
                            main_agent: None,
                        });
                }
            }
        }

        if inner.session_list().is_empty() {
            inner.create_session(None)?;
        }

        let handle = RuntimeHandle {
            inner: Arc::clone(&inner),
        };

        // MCP probing and plugin discovery are independent of the UI's first frame.
        let mcp_inner = Arc::clone(&inner);
        tokio::spawn(async move {
            let configured = {
                let registry = mcp_inner.mcp.lock().expect("mcp lock");
                registry.configured_count()
            };
            let registry = {
                let mut guard = mcp_inner.mcp.lock().expect("mcp lock");
                guard.take()
            };
            let (registry, result) = registry.connect_all().await;
            let connected = registry.connected_count();
            let tool_count = registry.tools_count();
            let tool_defs = registry.tool_defs();
            {
                let mut guard = mcp_inner.mcp.lock().expect("mcp lock");
                *guard = registry;
            }
            for def in tool_defs {
                let mcp_tool = McpTool {
                    server: def.server,
                    tool: def.name,
                    description: def.description,
                    schema: def.input_schema,
                    registry: Arc::clone(&mcp_inner.mcp),
                };
                mcp_inner.tools.register(Box::new(mcp_tool));
            }
            if let Err(err) = result {
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
            let plugins = inner.plugins.lock().expect("plugins");
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
        &self.inner.learning
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

    /// The configuration this process started with. Read-only: a live edit here would change what
    /// the running provider was built from without rebuilding it, so `set_model` is the one live
    /// change and everything else needs a restart.
    pub fn config(&self) -> &Config {
        &self.inner.config
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
        self.inner.evolution.lock().expect("evolution lock")
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
        self.inner.rules.lock().expect("rules lock").snapshot()
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
        self.inner.rules.lock().expect("rules lock").remove(index)
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
        let Some(session) = self.inner.session(session) else {
            return Err(RuntimeError::SessionNotFound(session.clone()));
        };
        if session.is_busy() {
            let depth = session.enqueue(text);
            self.inner.emit(RuntimeEvent::Notice {
                level: NoticeLevel::Info,
                text: format!("queued ({depth} waiting)"),
            });
            self.inner.publish_session(&session);
            return Ok(());
        }
        crate::turn::spawn_turn(Arc::clone(&self.inner), session, text, None);
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
        *self.inner.model.lock().expect("model lock") = model.clone();
        session.set_model(Some(model.clone()), now);
        self.inner
            .store
            .set_session_model(&session.id, Some(&model), now)
            .map_err(|err| RuntimeError::Store(err.to_string()))?;

        let mut config = self.inner.config.clone();
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
        // operator naming a model here is exactly the moment the endpoint is known.
        let mut provider_slot = self.inner.provider.write().expect("provider lock");
        if provider_slot.is_none() {
            match crate::openai::OpenAiProvider::from_config(&config) {
                Ok(Some(provider)) => {
                    *provider_slot = Some(Arc::new(provider) as Arc<dyn Provider>);
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

    /// Read the endpoint's own model list without holding up the interface: the answer arrives as
    /// [`RuntimeEvent::ModelsListed`], including when the endpoint refuses or cannot be reached.
    pub fn refresh_models(&self) {
        let inner = Arc::clone(&self.inner);
        let base_url = inner.config.base_url.clone();
        let api_key = inner.config.api_key();
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
                        inner.config.api_key_env
                    )),
                    Some(key) => crate::openai::list_models(&base_url, &key).await,
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
        let Some(session) = self.inner.session(session) else {
            return Err(RuntimeError::SessionNotFound(session.clone()));
        };
        let depth = session.enqueue(text);
        self.inner.emit(RuntimeEvent::Notice {
            level: NoticeLevel::Info,
            text: format!("queued ({depth} waiting)"),
        });
        self.inner.publish_session(&session);
        Ok(())
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

    pub fn close_session(&self, session: &SessionId) -> Result<()> {
        let Some(session) = self.inner.session(session) else {
            return Err(RuntimeError::SessionNotFound(session.clone()));
        };
        session.cancel_handle().cancel();
        self.inner
            .store
            .delete_session(&session.id)
            .map_err(|e| RuntimeError::Store(e.to_string()))?;
        self.inner
            .sessions
            .lock()
            .expect("sessions lock")
            .retain(|s| s.id != session.id);
        self.inner
            .emit(RuntimeEvent::SessionClosed(session.id.clone()));
        self.inner.publish_metrics();
        Ok(())
    }

    pub fn spawn_agent(&self, profile: AgentId, task: String, session: SessionId) -> Result<()> {
        let Some(agent) = self.inner.agents.get(&profile) else {
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
        // A delegated session is named for the agent that owns it and the work it was given, so the
        // tab reads "Arun — Security Review". The identity stays in the label because two bots
        // working on the same task must never look like one session.
        let subject = titling::sanitize(&titling::fallback(&task))
            .unwrap_or_else(|| titling::clip(&task, 28));
        let title = format!("{} — {}", agent.profile.name, subject);
        let agent_session = self.inner.create_session(Some(title))?;
        agent_session.set_agent(Some(profile.clone()), now_ms());
        agent_session.set_parent(Some(parent.id.clone()));
        agent_session.set_depth(depth);
        let _ = self
            .inner
            .store
            .set_session_agent(&agent_session.id, Some(&profile), now_ms());
        self.inner
            .store
            .set_session_lineage(&agent_session.id, Some(&parent.id), depth)?;
        // What crosses the boundary is a brief, not the parent's transcript: the live instruction
        // plus the decisions, files and open errors, inside the delegation budget.
        let brief = crate::compaction::delegation_brief(
            &agent_session.estimator(),
            &parent.messages(),
            self.inner.config.context.delegation_budget_tokens,
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
        agent.set_state(AgentState::Thinking);
        agent.set_session(Some(agent_session.id.clone()));
        *agent.paused.lock().expect("pause") = false;
        *agent.cancel.lock().expect("cancel") = vikram_contracts::CancellationToken::new();
        let bg = self.inner.tasks.start(
            format!("agent: {}", agent.profile.name),
            agent_session.id.clone(),
        );
        self.inner.publish_agent(&agent);
        self.inner.task_snapshot(&bg);
        self.inner
            .emit(RuntimeEvent::SessionCreated(agent_session.snapshot()));
        self.inner.publish_metrics();

        crate::turn::spawn_turn(
            Arc::clone(&self.inner),
            agent_session,
            task,
            Some(crate::turn::AgentRun {
                agent,
                task_id: bg.id.clone(),
            }),
        );
        Ok(())
    }

    pub fn control_agent(&self, id: &AgentId, control: AgentControl) -> Result<()> {
        let Some(agent) = self.inner.agents.get(id) else {
            return Err(RuntimeError::AgentNotFound(id.to_string()));
        };
        match control {
            AgentControl::Pause => {
                *agent.paused.lock().expect("pause") = true;
                agent.set_state(AgentState::Paused);
                self.inner.emit(RuntimeEvent::Notice {
                    level: NoticeLevel::Info,
                    text: format!("{} paused", agent.profile.name),
                });
            }
            AgentControl::Resume => {
                *agent.paused.lock().expect("pause") = false;
                if agent.state() == AgentState::Paused {
                    agent.set_state(AgentState::Running);
                }
            }
            AgentControl::Stop => {
                agent.cancel_handle().cancel();
                agent.set_state(AgentState::Stopped);
                self.inner.emit(RuntimeEvent::Notice {
                    level: NoticeLevel::Warn,
                    text: format!("{} stopped", agent.profile.name),
                });
            }
        }
        self.inner.publish_agent(&agent);
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

    pub fn agent_snapshots(&self) -> Vec<AgentSnapshot> {
        self.inner.agents.snapshots()
    }

    pub fn tasks(&self) -> Vec<BackgroundTask> {
        self.inner.tasks.list()
    }

    pub fn complete_task(&self, id: &str, state: TaskState) {
        if let Some(task) = self.inner.tasks.finish(id, state) {
            self.inner.task_snapshot(&task);
        }
    }

    pub fn team_create(&self, name: impl Into<String>) -> crate::error::Result<()> {
        self.inner
            .teams
            .create(name.into())
            .map_err(RuntimeError::InvalidState)?;
        let _ = self.inner.teams.save();
        Ok(())
    }

    pub fn team_list(&self) -> Vec<Team> {
        self.inner.teams.list()
    }

    pub fn team_get(&self, name: impl Into<String>) -> Option<Team> {
        self.inner.teams.get(name.into())
    }

    pub fn team_add_member(
        &self,
        team: impl Into<String>,
        agent: AgentId,
    ) -> crate::error::Result<()> {
        self.inner
            .teams
            .add_member(team.into(), agent)
            .map_err(RuntimeError::InvalidState)?;
        let _ = self.inner.teams.save();
        Ok(())
    }

    pub fn team_remove_member(
        &self,
        team: impl Into<String>,
        agent: AgentId,
    ) -> crate::error::Result<()> {
        self.inner
            .teams
            .remove_member(team.into(), agent)
            .map_err(RuntimeError::InvalidState)?;
        let _ = self.inner.teams.save();
        Ok(())
    }

    pub fn team_delete(&self, name: impl Into<String>) -> crate::error::Result<()> {
        self.inner
            .teams
            .delete(name.into())
            .map_err(RuntimeError::InvalidState)?;
        let _ = self.inner.teams.save();
        Ok(())
    }

    pub fn team_delegate(
        &self,
        team: impl Into<String>,
        task: impl Into<String>,
        assigned_to: Option<AgentId>,
    ) -> crate::error::Result<()> {
        self.inner
            .teams
            .delegate_task(team.into(), task.into(), assigned_to)
            .map_err(RuntimeError::InvalidState)?;
        let _ = self.inner.teams.save();
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
        self.inner.hive.list().map_err(RuntimeError::InvalidState)
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
        self.inner.hive.get(id).map_err(RuntimeError::InvalidState)
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
        &self.inner.tickets
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
        let _ = self
            .inner
            .hive
            .get(hive)
            .map_err(RuntimeError::InvalidState)?
            .ok_or_else(|| RuntimeError::InvalidState("hive not found".into()))?;

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
            let budget_tokens = inner.config.context.hive_budget_tokens;
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
                let role = if m.sender == ent.name || m.sender == ent.slug {
                    vikram_contracts::Role::Assistant
                } else {
                    vikram_contracts::Role::User
                };
                request_messages.push(vikram_contracts::Message::text(
                    role,
                    m.content.clone(),
                    vikram_contracts::Trust::User,
                ));
            }
            let request = crate::provider::CompletionRequest {
                model: model.to_owned(),
                messages: request_messages,
                tools: Vec::new(),
                max_output_tokens: inner.config.max_output_tokens,
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
        self.inner
            .memory
            .save(key, value, scope)
            .map_err(|e| RuntimeError::Store(e.to_string()))
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
        self.inner
            .memory
            .delete(key, scope)
            .map_err(|e| RuntimeError::Store(e.to_string()))
    }

    pub fn skill_list(&self) -> Vec<Skill> {
        self.inner.skills.lock().expect("skills lock").list()
    }

    pub fn skill_get(&self, name: &str) -> Option<Skill> {
        self.inner.skills.lock().expect("skills lock").get(name)
    }

    pub fn skill_count(&self) -> usize {
        self.inner.skills.lock().expect("skills lock").count()
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
        *self.inner.skills.lock().expect("skills lock") = reg;
    }

    pub fn shutdown(&self) {
        self.inner
            .emit(RuntimeEvent::Status(RuntimeStatus::Stopping));
        for session in self.inner.session_list() {
            session.cancel_handle().cancel();
        }
        for agent in self.inner.agents.list() {
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
        assert_eq!(snapshot.agents.len(), 0);
        assert_eq!(snapshot.metrics.subagents_active, 0);
        assert_eq!(snapshot.metrics.background_tasks, 0);
        assert!(snapshot.metrics.model.is_some());
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
        ctx.scaffold_entity(&entity_dir, "Rudra", "agent", "Senior Developer");
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
            self.requests.lock().expect("requests lock").clone()
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
        handle
            .submit(&id, "compare AI coding agents".into())
            .unwrap();
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
}
