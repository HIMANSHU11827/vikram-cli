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
            model: {
                let live = self.model();
                if live.is_empty() {
                    sessions.iter().find_map(|s| s.model())
                } else {
                    Some(live)
                }
            },
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
        let model = self.model();
        let row = self
            .store
            .create_session(
                &title,
                &self.workspace_root,
                if model.is_empty() { None } else { Some(&model) },
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

        let model_name = config.model.clone();

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
            model: Mutex::new(model_name),
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
        if self.inner.provider.is_none() {
            self.inner.emit(RuntimeEvent::Notice {
                level: NoticeLevel::Warn,
                text: format!(
                    "model set to `{model}`; no endpoint is configured yet, so a turn cannot start"
                ),
            });
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

    /// Every remembered key, as `(scope, key, value)`. The panel lists them all because a scope the
    /// operator cannot see is a scope they cannot ask about.
    pub fn memory_list_all(&self) -> Vec<(String, String, String)> {
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
    async fn a_blank_model_name_is_refused_instead_of_becoming_the_choice() {
        let dir = tempfile::tempdir().unwrap();
        let _home = isolated_home(dir.path());
        let runtime = Runtime::start(config_for(dir.path())).await.unwrap();
        let handle = runtime.handle();
        let session = handle.snapshot().sessions[0].id.clone();

        assert!(handle.set_model(&session, "   ".into()).is_err());
        assert_eq!(handle.inner.model(), "test-model");
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
}
