//! Runtime facade.
//!
//! `Runtime` owns the process-wide state; `RuntimeHandle` is the cheap clone every view and task
//! holds. The handle exposes observations and explicit controls only — there is no method that
//! stops work as a side effect of changing what is on screen.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::atomic::Ordering;
use std::sync::{Arc, Mutex};
use tokio::sync::{broadcast, oneshot};
use vikram_contracts::journal::JournalSink;
use vikram_contracts::{
    ActionKind, AgentId, ApprovalId, Decision, Message, PermissionMode, PermissionRequest, Posture,
    RecordKind, SessionId,
};
use vikram_kernel::TokenMeter;
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
use crate::tools::{McpTool, ToolContext, ToolRegistry};
use crate::types::{
    AgentSnapshot, AgentState, ApprovalPrompt, BackgroundTask, Metrics, NoticeLevel, RuntimeEvent,
    RuntimeStatus, SessionSnapshot, StateSnapshot, TaskState, TeamSnapshot, now_ms,
};
use vikram_mcp::runtime::PrivateRuntime;
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
    pub bus: EventBus,
    pub tools: ToolRegistry,
    pub provider: Option<Arc<dyn Provider>>,
    pub sessions: Mutex<Vec<Arc<SessionRuntime>>>,
    pub agents: AgentManager,
    pub tasks: TaskManager,
    pub teams: TeamManager,
    pub mcp: std::sync::Arc<std::sync::Mutex<McpRegistry>>,
    pub mcp_visibility: std::sync::Mutex<VisibilityManager>,
    pub mcp_private_runtime: std::sync::Mutex<PrivateRuntime>,
    pub plugins: Mutex<PluginRegistry>,
    pub skills: SkillRegistry,
    pub approvals: Mutex<HashMap<ApprovalId, PendingApproval>>,
    /// `ask_user` requests awaiting an operator answer, keyed by request id.
    pub questions: Mutex<HashMap<String, PendingQuestion>>,
    /// The posture in force right now: how the runtime treats a request the rules only prompted for.
    pub mode: Mutex<PermissionMode>,
    /// Which postures the layers above this runtime refuse to let it take.
    pub availability: ModeAvailability,
    /// Permissions a human granted once and asked to keep.
    pub rules: Mutex<RuleBook>,
    pub workspace_root: PathBuf,
    pub started_ms: u64,
    pub last_turn_ms: Mutex<Option<u64>>,
    pub last_tokens_per_sec: Mutex<Option<f64>>,
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

    /// The evaluator for this runtime: the operator's saved rules first, then the built-in
    /// conventions, under a floor neither tier can widen.
    pub fn policy(&self) -> Policy {
        let root = self.workspace_root.to_string_lossy().into_owned();
        let floor = MandatoryFloor {
            workspace_root: Some(root.clone()),
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
        let context_used = sessions
            .iter()
            .map(|s| {
                s.messages()
                    .iter()
                    .map(|m| TokenMeter::estimate(&m.text_body()))
                    .sum::<u64>()
            })
            .max()
            .unwrap_or(0);
        let context_window = self
            .provider
            .as_ref()
            .and_then(|p| p.context_window())
            .or(self.config.context_window);
        let turn_elapsed_ms = sessions
            .iter()
            .filter_map(|s| s.turn_elapsed_ms(now_ms()))
            .max()
            .or(*self.last_turn_ms.lock().expect("turn metric"));
        let mcp = self.mcp.lock().expect("mcp lock");
        let plugins = self.plugins.lock().expect("plugins lock");
        Metrics {
            model: self
                .provider
                .as_ref()
                .map(|p| p.model().to_owned())
                .filter(|m| !m.is_empty())
                .or_else(|| sessions.iter().find_map(|s| s.model())),
            provider: self.provider.as_ref().map(|p| p.name().to_owned()),
            context_used,
            context_window,
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
            skills_loaded: 0,
            thinking: self.config.thinking,
        }
    }

    pub fn publish_metrics(&self) {
        self.emit(RuntimeEvent::Metrics(self.metrics()));
    }

    pub fn tool_context(&self) -> ToolContext {
        ToolContext {
            workspace_root: self.workspace_root.clone(),
        }
    }

    pub fn tools(&self) -> &crate::tools::ToolRegistry {
        &self.tools
    }

    #[allow(dead_code)]
    #[allow(dead_code)]
    pub fn mcp_visibility(&self) -> &std::sync::Mutex<VisibilityManager> {
        &self.mcp_visibility
    }

    #[allow(dead_code)]
    #[allow(dead_code)]
    pub fn mcp_private_runtime(&self) -> &std::sync::Mutex<PrivateRuntime> {
        &self.mcp_private_runtime
    }
    pub fn create_session(&self, title: Option<String>) -> Result<Arc<SessionRuntime>> {
        let now = now_ms();
        let title = title.unwrap_or_else(|| {
            let count = self.session_list().len() + 1;
            format!("Session {count}")
        });
        let row = self
            .store
            .create_session(
                &title,
                &self.workspace_root,
                if self.config.model.is_empty() {
                    None
                } else {
                    Some(self.config.model.as_str())
                },
                now,
            )
            .map_err(|e| RuntimeError::Store(e.to_string()))?;
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
        if self.provider.is_none() {
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

impl Runtime {
    pub async fn start(config: Config) -> Result<Self> {
        ensure_home()?;
        let store =
            Store::open(&database_path()).map_err(|e| RuntimeError::Store(e.to_string()))?;
        let memory = crate::MemoryStore::open(database_path())
            .map_err(|e| RuntimeError::Store(e.to_string()))?;
        let workspace_root = config
            .workspace_root
            .clone()
            .unwrap_or_else(|| std::env::current_dir().unwrap_or_else(|_| PathBuf::from(".")));

        let agents = AgentManager::new();
        match store.list_agents() {
            Ok(rows) => {
                for row in rows {
                    agents.add(Arc::new(ManagedAgent::from_profile(row.into())));
                }
            }
            Err(err) => return Err(RuntimeError::Store(err.to_string())),
        }

        let provider = crate::openai::OpenAiProvider::from_config(&config)?
            .map(|p| Arc::new(p) as Arc<dyn Provider>);

        let mcp = McpRegistry::load(McpConfig::load());
        let plugins = PluginRegistry::from_default_dir();
        let skills = SkillRegistry::for_workspace(&workspace_root);

        // The layers above this runtime can refuse a posture; a config that asked for one of those
        // starts on the most permissive thing still available rather than failing to boot.
        let availability = ModeAvailability::resolve(&config.security);
        let mode = if availability.permits(config.mode) {
            config.mode
        } else {
            availability.fallback_mode()
        };

        let inner = Arc::new(RuntimeInner {
            config,
            store,
            memory,
            bus: EventBus::new(),
            tools: ToolRegistry::with_builtins(),
            provider,
            sessions: Mutex::new(Vec::new()),
            agents,
            tasks: TaskManager::new(),
            teams: TeamManager::load(),
            mcp: Arc::new(Mutex::new(mcp)),
            mcp_visibility: Mutex::new(VisibilityManager::new()),
            mcp_private_runtime: Mutex::new(PrivateRuntime::new(
                workspace_root.join(".vikram").join("mcp-runtime"),
            )),
            plugins: Mutex::new(plugins),
            skills,
            approvals: Mutex::new(HashMap::new()),
            questions: Mutex::new(HashMap::new()),
            mode: Mutex::new(mode),
            availability,
            rules: Mutex::new(RuleBook::open()),
            workspace_root,
            started_ms: now_ms(),
            last_turn_ms: Mutex::new(None),
            last_tokens_per_sec: Mutex::new(None),
        });

        // Rehydrate durable sessions. Journal replay restores the conversation without re-applying
        // any effect: the journal is a record, not a queue.
        let rows = inner
            .store
            .list_sessions()
            .map_err(|e| RuntimeError::Store(e.to_string()))?;
        for row in rows {
            let session = Arc::new(SessionRuntime::from_row(row));
            if let Ok(records) = inner.store.replay_messages(&session.id) {
                let mut messages = Vec::new();
                for record in records {
                    if record.kind == RecordKind::Message
                        && let Some(message) = record.payload.get("message")
                        && let Ok(message) = serde_json::from_value::<Message>(message.clone())
                    {
                        messages.push(message);
                    }
                }
                *session.messages.lock().expect("messages lock") = messages;
            }
            inner.sessions.lock().expect("sessions lock").push(session);
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

    pub fn tools(&self) -> &crate::tools::ToolRegistry {
        self.inner.tools()
    }

    pub fn mcp_visibility(&self) -> &std::sync::Mutex<VisibilityManager> {
        &self.inner.mcp_visibility
    }

    pub fn mcp_private_runtime(&self) -> &std::sync::Mutex<PrivateRuntime> {
        &self.inner.mcp_private_runtime
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
        let saved = self.inner.rules.lock().expect("rules lock").add_amendment(amendment)?;
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

    pub fn create_session(&self, title: Option<String>) -> Result<SessionId> {
        let session = self.inner.create_session(title)?;
        self.inner.publish_metrics();
        Ok(session.id.clone())
    }

    pub fn rename_session(&self, session: &SessionId, title: String) -> Result<()> {
        let Some(session) = self.inner.session(session) else {
            return Err(RuntimeError::SessionNotFound(session.clone()));
        };
        session.set_title(&title, now_ms());
        self.inner
            .store
            .rename_session(&session.id, &title, now_ms())
            .map_err(|e| RuntimeError::Store(e.to_string()))?;
        self.inner.publish_session(&session);
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
        let Some(_parent) = self.inner.session(&session) else {
            return Err(RuntimeError::SessionNotFound(session));
        };
        let title = format!("{} · {}", agent.profile.name, truncate(&task, 28));
        let agent_session = self.inner.create_session(Some(title))?;
        agent_session.set_agent(Some(profile.clone()), now_ms());
        let _ = self
            .inner
            .store
            .set_session_agent(&agent_session.id, Some(&profile), now_ms());
        agent.set_task(Some(task.clone()));
        agent.set_state(AgentState::Thinking);
        agent.set_session(Some(agent_session.id.clone()));
        *agent.paused.lock().expect("pause") = false;
        *agent.cancel.lock().expect("cancel") = vikram_kernel::CancellationToken::new();
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
        if allow && persist {
            // Keep exactly the scope the prompt proposed. Nothing here widens a permission: the
            // saved rule is an operator-tier rule, still under the mandatory floor.
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

    pub fn memory_search<Q, S>(&self, query: Q, scope: S) -> Vec<(String, String)>
    where
        Q: AsRef<str>,
        S: AsRef<str>,
    {
        self.inner.memory.search(query, scope)
    }

    pub fn memory_list<S>(&self, scope: S) -> Vec<(String, String)>
    where
        S: AsRef<str>,
    {
        self.inner.memory.list(scope)
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
        self.inner.skills.list()
    }

    pub fn skill_get(&self, name: &str) -> Option<Skill> {
        self.inner.skills.get(name)
    }

    pub fn skill_count(&self) -> usize {
        self.inner.skills.count()
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

fn truncate(text: &str, limit: usize) -> String {
    if text.chars().count() <= limit {
        return text.to_owned();
    }
    let cut: String = text.chars().take(limit.saturating_sub(1)).collect();
    format!("{cut}…")
}

#[cfg(test)]
#[allow(
    clippy::await_holding_lock,
    reason = "isolated_home guards process-global env vars; releasing it at an await would let a concurrent test repoint VIKRAM_HOME mid-run"
)]
mod tests {
    use super::*;
    use crate::config::Config;
    use vikram_contracts::{SideEffect, Trust};

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
        handle.set_mode(&session, PermissionMode::Unattended).unwrap();
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
        handle.set_mode(&session, PermissionMode::AllowList).unwrap();

        let read = PermissionRequest::new(
            ActionKind::Read,
            "fs.read",
            "/etc/hosts",
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
        assert_eq!(handle.mode(), PermissionMode::Ask, "a refusal changes nothing");
        assert!(
            !handle.modes().contains(&PermissionMode::Yolo),
            "and it is not offered either"
        );

        let saved = handle
            .allow(&session, "allow execute shell.run resource=cargo test *")
            .expect_err("disable_always_allow forbids keeping permissions");
        assert!(saved.to_string().contains("disable_always_allow"), "{saved}");
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
}
