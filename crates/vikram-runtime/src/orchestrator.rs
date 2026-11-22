//! The orchestrator's workers.
//!
//! A worker is a session with a typed brief and a capability envelope the policy machine
//! enforces. Three invariants hold by construction:
//! 1. A worker's scope is the intersection of its parent's envelope and its brief, so child
//!    trust is monotonically narrower — no descendant and no sibling can re-obtain what any
//!    ancestor was denied.
//! 2. Results cross the boundary as artifacts in the store, never as transcripts: the
//!    orchestrator receives a reference and reads it back on demand.
//! 3. A worker that runs out of turns or fails never ends the team: it is marked partial and
//!    resumable, or retried once and reassigned, and the rest keep running.

use serde_json::{Value, json};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use vikram_contracts::{
    ActionKind, Message, PermissionRequest, RecordKind, Role, SessionId, SideEffect, Trust,
    VikramError,
};
use vikram_policy::CapabilityScope;

use crate::error::{Result, RuntimeError};
use crate::runtime::{RuntimeHandle, RuntimeInner};
use crate::tools::*;
use crate::types::{NoticeLevel, RuntimeEvent, TurnState, now_ms};

/// Unique identity of one spawned worker, stable for the life of the record.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct WorkerId(String);

impl WorkerId {
    pub fn new(value: impl Into<String>) -> Self {
        Self(value.into())
    }

    pub fn generate() -> Self {
        static SEQUENCE: AtomicU64 = AtomicU64::new(0);
        let next = SEQUENCE.fetch_add(1, Ordering::Relaxed);
        Self(format!("wrk-{}-{next}", crate::types::now_ms()))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Display for WorkerId {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(&self.0)
    }
}

/// What an orchestrator hands a worker. Typed, not prose: the runtime routes on it, the policy
/// narrows it, and the worker is told it verbatim.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct WorkerBrief {
    pub objective: String,
    pub output_format: String,
    /// Tool names the worker may call. Empty means the worker may call nothing.
    pub tool_allowlist: Vec<String>,
    /// Absolute subtrees the worker may touch. Empty means no file access.
    pub file_scope: Vec<String>,
    /// What the worker must never do, whatever else it decides.
    pub boundary: String,
    /// Model the worker runs on; `None` inherits the orchestrator's.
    pub model: Option<String>,
    /// Reasoning effort the worker runs at; `None` inherits the orchestrator's.
    pub effort: Option<String>,
}

impl WorkerBrief {
    /// Parse the typed brief from `spawn_worker` arguments. Strict: a field with the wrong shape
    /// is a refused call, not a silently defaulted one.
    fn from_args(args: &Value) -> std::result::Result<Self, VikramError> {
        Ok(Self {
            objective: arg_str(args, "objective")?,
            output_format: arg_str(args, "output_format")?,
            tool_allowlist: str_list(args, "tool_allowlist")?,
            file_scope: str_list(args, "file_scope")?,
            boundary: arg_str(args, "boundary")?,
            model: opt_str(args, "model"),
            effort: opt_str(args, "effort"),
        })
    }

    /// The brief as the worker is told it, in its own system message.
    fn render(&self) -> String {
        let tools = if self.tool_allowlist.is_empty() {
            "(none)".to_owned()
        } else {
            self.tool_allowlist.join(", ")
        };
        let files = if self.file_scope.is_empty() {
            "(none)".to_owned()
        } else {
            self.file_scope.join(", ")
        };
        format!(
            "You are a worker spawned by an orchestrator. Complete exactly this brief.\n\
             Objective: {}\n\
             Output format: {}\n\
             Tools you may call: {tools}\n\
             Files you may touch: {files}\n\
             Boundary: {}\n\
             When the brief is complete, end your turn with the finished result in the \
             requested format.",
            self.objective, self.output_format, self.boundary
        )
    }
}

fn str_list(args: &Value, key: &str) -> std::result::Result<Vec<String>, VikramError> {
    let missing = || VikramError::ProviderRejected(format!("missing array argument `{key}`"));
    let wrong =
        || VikramError::ProviderRejected(format!("argument `{key}` must be an array of strings"));
    let list = args
        .get(key)
        .ok_or_else(missing)?
        .as_array()
        .ok_or_else(wrong)?;
    list.iter()
        .map(|item| item.as_str().map(str::to_owned).ok_or_else(wrong))
        .collect()
}

fn opt_str(args: &Value, key: &str) -> Option<String> {
    args.get(key).and_then(Value::as_str).map(str::to_owned)
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum WorkerStatus {
    Running,
    /// Hit its turn budget with work left; the session is kept so a resume continues it.
    Partial,
    /// Finished; `artifact` names the result in the store.
    Done,
    /// Failed twice; its brief was reassigned to a replacement or forgone.
    Degraded,
}

#[derive(Clone, Debug)]
pub struct WorkerRecord {
    pub id: WorkerId,
    pub brief: WorkerBrief,
    pub scope: CapabilityScope,
    pub session: SessionId,
    pub parent: SessionId,
    pub depth: u32,
    pub status: WorkerStatus,
    /// 1 is the first run, 2 the single retry. A third attempt never happens.
    pub attempts: u32,
    pub max_turns: Option<u64>,
    pub artifact: Option<String>,
}

/// What a failed attempt earns: the one retry, or the reassign-or-degrade decision.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AttemptOutcome {
    Retry,
    Reassign,
}

#[derive(Debug, Default)]
pub struct WorkerRegistry {
    workers: std::sync::Mutex<Vec<WorkerRecord>>,
}

impl WorkerRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn insert(&self, record: WorkerRecord) {
        self.workers.lock().expect("workers lock").push(record);
    }

    pub fn list(&self) -> Vec<WorkerRecord> {
        self.workers.lock().expect("workers lock").clone()
    }

    pub fn by_id(&self, id: &WorkerId) -> Option<WorkerRecord> {
        self.workers
            .lock()
            .expect("workers lock")
            .iter()
            .find(|record| record.id == *id)
            .cloned()
    }

    pub fn by_session(&self, session: &SessionId) -> Option<WorkerRecord> {
        self.workers
            .lock()
            .expect("workers lock")
            .iter()
            .find(|record| record.session == *session)
            .cloned()
    }

    pub fn mark_partial(&self, session: &SessionId) {
        let mut workers = self.workers.lock().expect("workers lock");
        if let Some(record) = workers
            .iter_mut()
            .find(|record| record.session == *session && record.status == WorkerStatus::Running)
        {
            record.status = WorkerStatus::Partial;
        }
    }

    pub fn set_running(&self, session: &SessionId) {
        let mut workers = self.workers.lock().expect("workers lock");
        if let Some(record) = workers.iter_mut().find(|record| record.session == *session) {
            record.status = WorkerStatus::Running;
        }
    }

    pub fn complete(&self, session: &SessionId, artifact: String) {
        let mut workers = self.workers.lock().expect("workers lock");
        if let Some(record) = workers.iter_mut().find(|record| record.session == *session) {
            record.status = WorkerStatus::Done;
            record.artifact = Some(artifact);
        }
    }

    pub fn degrade(&self, id: &WorkerId) {
        let mut workers = self.workers.lock().expect("workers lock");
        if let Some(record) = workers.iter_mut().find(|record| record.id == *id) {
            record.status = WorkerStatus::Degraded;
        }
    }

    /// Record one failed attempt. The first failure earns the single retry; the second failure
    /// is the reassign-or-degrade decision, never a third run of the same worker.
    pub fn attempt_failed(&self, session: &SessionId) -> AttemptOutcome {
        let mut workers = self.workers.lock().expect("workers lock");
        let Some(record) = workers.iter_mut().find(|record| record.session == *session) else {
            return AttemptOutcome::Reassign;
        };
        match record.attempts {
            1 => {
                record.attempts = 2;
                AttemptOutcome::Retry
            }
            _ => AttemptOutcome::Reassign,
        }
    }
}

/// `worker.collect` — how an orchestrator consumes a worker's result. It holds a reference from
/// the moment the worker finished; this reads the artifact the reference names, so the child's
/// transcript is never the delivery vehicle.
pub(crate) struct WorkerCollect;

impl Tool for WorkerCollect {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "worker.collect".into(),
            description: "Read a spawned worker's stored result by worker id.".into(),
            schema: json!({
                "type": "object",
                "properties": {
                    "id": {"type": "string", "description": "the worker id returned by spawn_worker"}
                },
                "required": ["id"]
            }),
        }
    }

    fn permission(&self, args: &Value) -> std::result::Result<PermissionRequest, VikramError> {
        let id = arg_str(args, "id")?;
        Ok(PermissionRequest::new(
            ActionKind::Read,
            "worker.collect",
            id,
            SideEffect::None,
            Trust::Model,
        ))
    }

    fn run(&self, args: &Value, ctx: &ToolContext) -> std::result::Result<ToolOutput, VikramError> {
        let id = arg_str(args, "id")?;
        let Some(runtime) = ctx.runtime.as_ref() else {
            return Ok(ToolOutput {
                content: "worker.collect needs a live runtime".into(),
                ok: false,
            });
        };
        let Some(record) = runtime.workers.by_id(&WorkerId::new(id.clone())) else {
            return Ok(ToolOutput {
                content: format!("no worker named `{id}`"),
                ok: false,
            });
        };
        let Some(artifact) = record.artifact.clone() else {
            return Ok(ToolOutput {
                content: format!(
                    "worker {id} has no stored result yet (status {:?})",
                    record.status
                ),
                ok: false,
            });
        };
        let row = runtime
            .store
            .get_artifact(&artifact)
            .map_err(|e| VikramError::Storage(e.to_string()))?;
        let Some(row) = row else {
            return Ok(ToolOutput {
                content: format!("artifact {artifact} is no longer registered"),
                ok: false,
            });
        };
        match std::fs::read_to_string(&row.path) {
            Ok(content) => Ok(ToolOutput {
                content: truncated(content),
                ok: true,
            }),
            Err(err) => Ok(ToolOutput {
                content: format!("artifact {artifact} could not be read: {err}"),
                ok: false,
            }),
        }
    }
}

impl RuntimeInner {
    /// What a worker's finished turn owes its orchestrator: an artifact in the store and a
    /// reference in the parent's transcript — never the child transcript itself. The worker
    /// session is one-shot like a delegation: its work is done and its result is durable.
    pub(crate) fn finish_worker(
        self: &Arc<Self>,
        session: &Arc<crate::session::SessionRuntime>,
        outcome: crate::types::TurnState,
    ) {
        let Some(record) = self.workers.by_session(&session.id) else {
            return;
        };
        if outcome == TurnState::Failed {
            // Exactly one retry, then reassign-or-degrade. Either way the team keeps running:
            // nothing here cancels a sibling, interrupts the orchestrator, or ends the run.
            match self.workers.attempt_failed(&session.id) {
                AttemptOutcome::Retry => {
                    let message = Message::text(
                        Role::System,
                        "Your run failed before it could finish. Retry the brief from the \
                         start; the failure was not yours to answer for.",
                        Trust::Host,
                    );
                    session.push_message(message.clone(), now_ms());
                    self.journal(
                        &session.id,
                        RecordKind::Message,
                        serde_json::json!({"message": message}),
                    );
                    self.notify_parent(
                        &record,
                        format!(
                            "[worker {} failed once — retrying the same brief]",
                            record.id
                        ),
                    );
                    // The turn slot is still held by the finishing turn, so the retry queues
                    // and the finishing turn's drain spawns it, exactly like a racing submit.
                    crate::turn::spawn_turn(
                        Arc::clone(self),
                        Arc::clone(session),
                        Message::user("Retry the brief from the start."),
                        None,
                    );
                }
                AttemptOutcome::Reassign => {
                    let replacement = RuntimeHandle {
                        inner: Arc::clone(self),
                    }
                    .spawn_worker(
                        record.parent.clone(),
                        record.brief.clone(),
                        record.max_turns,
                    );
                    match replacement {
                        Ok(fresh) => {
                            self.workers.degrade(&record.id);
                            self.notify_parent(
                                &record,
                                format!(
                                    "[worker {} failed twice — its brief was reassigned to \
                                     worker {fresh}]",
                                    record.id
                                ),
                            );
                        }
                        Err(err) => {
                            // No parent left to serve, or the floor refuses a replacement: the
                            // result is forgone, the worker degrades, and the team goes on.
                            self.workers.degrade(&record.id);
                            self.emit(RuntimeEvent::Notice {
                                level: NoticeLevel::Warn,
                                text: format!(
                                    "worker {} failed twice and could not be reassigned ({err}) \
                                     — degraded",
                                    record.id
                                ),
                            });
                        }
                    }
                    self.reap_worker_session(session);
                    self.publish_metrics();
                }
            }
            return;
        }
        if outcome != TurnState::Idle {
            // A cancellation ends the run without a handoff and without a retry.
            return;
        }
        // A partial worker stopped at its turn budget: its session and transcript are kept for
        // a resume, and the orchestrator is told what happened. The team keeps running.
        if record.status == WorkerStatus::Partial {
            if let Some(parent) = self.session(&record.parent) {
                let text = format!(
                    "[worker {} stopped at its turn budget — partial; its state is kept and can \
                     be resumed]",
                    record.id
                );
                let message = Message::text(Role::System, text, Trust::Host);
                parent.push_message(message.clone(), now_ms());
                self.journal(
                    &record.parent,
                    RecordKind::Message,
                    serde_json::json!({"message": message}),
                );
                self.emit(RuntimeEvent::MessageCommitted {
                    session: record.parent.clone(),
                    message,
                });
            }
            self.emit(RuntimeEvent::Notice {
                level: NoticeLevel::Info,
                text: format!("worker {} is partial and resumable", record.id),
            });
            self.publish_metrics();
            return;
        }
        let answer = session
            .messages()
            .iter()
            .rev()
            .filter(|message| matches!(message.role, Role::Assistant))
            .find_map(|message| {
                let body = message.text_body();
                (!body.trim().is_empty()).then_some(body)
            })
            .unwrap_or_else(|| "(the worker finished without a final statement)".to_owned());
        let artifacts_dir = crate::config::vikram_home().join("artifacts");
        let path = artifacts_dir.join(format!("{}.md", record.id.as_str()));
        if let Err(err) =
            std::fs::create_dir_all(&artifacts_dir).and_then(|_| std::fs::write(&path, &answer))
        {
            self.emit(RuntimeEvent::Notice {
                level: NoticeLevel::Warn,
                text: format!(
                    "worker {} produced a result that could not be stored: {err}",
                    record.id
                ),
            });
            return;
        }
        let row = match self
            .workspace
            .register_artifact(&session.id, None, &path, Some("worker"))
        {
            Ok(row) => row,
            Err(err) => {
                self.emit(RuntimeEvent::Notice {
                    level: NoticeLevel::Warn,
                    text: format!(
                        "worker {} produced a result that could not be registered: {err}",
                        record.id
                    ),
                });
                return;
            }
        };
        self.workers.complete(&session.id, row.id.clone());
        self.notify_parent(
            &record,
            format!(
                "[worker {} finished — result stored as artifact {}; read it with \
                 worker.collect {{\"id\": \"{}\"}}]",
                record.id, row.id, record.id
            ),
        );
        self.emit(RuntimeEvent::Notice {
            level: NoticeLevel::Info,
            text: format!("worker {} answered", record.id),
        });
        self.reap_worker_session(session);
        self.publish_metrics();
    }

    /// Put a line in the orchestrator's transcript and journal. Workers speak to their parent
    /// through this — a reference, a state change, a reassignment — never through transcripts.
    fn notify_parent(&self, record: &WorkerRecord, text: String) {
        let Some(parent) = self.session(&record.parent) else {
            return;
        };
        let message = Message::text(Role::System, text, Trust::Host);
        parent.push_message(message.clone(), now_ms());
        self.journal(
            &record.parent,
            RecordKind::Message,
            serde_json::json!({"message": message}),
        );
        self.emit(RuntimeEvent::MessageCommitted {
            session: record.parent.clone(),
            message,
        });
    }

    /// Close a worker session whose run is over: durable `closed` row, live handle dropped, one
    /// last event so an interface stops showing it as busy.
    fn reap_worker_session(&self, session: &Arc<crate::session::SessionRuntime>) {
        let _ = self
            .store
            .set_session_status(&session.id, "closed", now_ms());
        self.sessions
            .lock()
            .unwrap_or_else(|e| {
                eprintln!("vikram: runtime sessions lock mutex poisoned; recovering");
                e.into_inner()
            })
            .retain(|live| live.id != session.id);
        self.emit(RuntimeEvent::SessionClosed(session.id.clone()));
    }
}

impl RuntimeHandle {
    /// Spawn a worker under an orchestrator session. The child gets a fresh session carrying
    /// the typed brief — never the parent's transcript — and a capability envelope narrowed to
    /// the intersection of what the parent holds and what the brief asks for.
    pub fn spawn_worker(
        &self,
        parent: SessionId,
        brief: WorkerBrief,
        max_turns: Option<u64>,
    ) -> Result<WorkerId> {
        let Some(parent_session) = self.inner.session(&parent) else {
            return Err(RuntimeError::SessionNotFound(parent));
        };
        // The floor already refuses a Delegate action past the limit; refusing the spawn itself
        // keeps a chain from being built by a path that skipped the gate.
        let depth = parent_session.depth() + 1;
        let limit = self.inner.policy().max_delegation_depth();
        if depth > limit {
            return Err(RuntimeError::InvalidState(format!(
                "delegation depth {depth} exceeds the limit of {limit}"
            )));
        }
        // The child's envelope is the intersection of the parent's and the brief's asks, so a
        // worker can never hold — or re-grant — more than the session that spawned it.
        let scope = CapabilityScope::narrowed(
            &parent_session.capability_scope(),
            &brief.tool_allowlist,
            &brief.file_scope,
        );
        if scope.tools.as_ref().is_some_and(|tools| tools.is_empty()) {
            return Err(RuntimeError::InvalidState(
                "the brief names no tool this session can grant".into(),
            ));
        }
        let subject = crate::titling::sanitize(&crate::titling::fallback(&brief.objective))
            .unwrap_or_else(|| crate::titling::clip(&brief.objective, 28));
        let child = self
            .inner
            .create_session(Some(format!("Worker — {subject}")))?;
        child.set_parent(Some(parent.clone()));
        child.set_depth(depth);
        child.set_capability_scope(Some(scope.clone()));
        child.set_worker_max_turns(max_turns.unwrap_or(0));
        self.inner
            .store
            .set_session_lineage(&child.id, Some(&parent), depth)?;
        // Routing: an explicit brief choice overrides, an absent field inherits the
        // orchestrator's live choice.
        let model = brief.model.clone().or_else(|| parent_session.model());
        let now = now_ms();
        child.set_model(model.clone(), now);
        let _ = self
            .inner
            .store
            .set_session_model(&child.id, model.as_deref(), now);
        child.set_effort(brief.effort.clone().or_else(|| parent_session.effort()));

        // What crosses the boundary is the typed brief, never the parent's transcript.
        let message = Message::text(Role::System, brief.render(), Trust::Host);
        child.push_message(message.clone(), now);
        self.inner.journal(
            &child.id,
            RecordKind::Message,
            serde_json::json!({"message": message}),
        );

        let id = WorkerId::generate();
        self.inner.workers.insert(WorkerRecord {
            id: id.clone(),
            brief: brief.clone(),
            scope,
            session: child.id.clone(),
            parent: parent.clone(),
            depth,
            status: WorkerStatus::Running,
            attempts: 1,
            max_turns,
            artifact: None,
        });
        // The spawn belongs to the orchestrator's audit trail: without this row the parent's
        // transcript shows worker ids it never learned about.
        self.inner.journal(
            &parent,
            RecordKind::Status,
            serde_json::json!({
                "worker": id.as_str(),
                "objective": brief.objective,
                "depth": depth,
                "tools": brief.tool_allowlist,
                "file_scope": brief.file_scope,
            }),
        );
        self.inner
            .emit(RuntimeEvent::SessionCreated(child.snapshot()));
        self.inner.publish_metrics();
        crate::turn::spawn_turn(
            Arc::clone(&self.inner),
            Arc::clone(&child),
            Message::user(brief.objective),
            None,
        );
        Ok(id)
    }

    /// Resume a partial worker: its session and transcript were kept at the budget wall; a
    /// continuation turn runs from where it stopped, under the same brief and the same narrowed
    /// scope.
    pub fn resume_worker(&self, id: &WorkerId) -> Result<()> {
        let Some(record) = self.inner.workers.by_id(id) else {
            return Err(RuntimeError::InvalidState(format!(
                "no worker named `{id}`"
            )));
        };
        if record.status != WorkerStatus::Partial {
            return Err(RuntimeError::InvalidState(format!(
                "worker {id} is {:?}, not partial",
                record.status
            )));
        }
        let Some(session) = self.inner.session(&record.session) else {
            return Err(RuntimeError::SessionNotFound(record.session.clone()));
        };
        self.inner.workers.set_running(&record.session);
        crate::turn::spawn_turn(
            Arc::clone(&self.inner),
            session,
            Message::user(format!(
                "Continue your brief from where you stopped. Objective: {}",
                record.brief.objective
            )),
            None,
        );
        Ok(())
    }
}

/// `spawn_worker` — an orchestrator's only way to grow the team. Whether a session may call it
/// at all is the policy machine's decision: a worker's allowlist does not include it unless the
/// orchestrator deliberately granted it, and the depth floor cuts runaway chains.
pub(crate) struct SpawnWorker;

impl Tool for SpawnWorker {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "spawn_worker".into(),
            description: "Spawn a worker with a typed brief; returns the worker id.".into(),
            schema: json!({
                "type": "object",
                "properties": {
                    "objective": {"type": "string", "description": "what the worker must accomplish"},
                    "output_format": {"type": "string", "description": "the shape its result must take"},
                    "tool_allowlist": {"type": "array", "items": {"type": "string"}, "description": "tool names the worker may call"},
                    "file_scope": {"type": "array", "items": {"type": "string"}, "description": "absolute subtrees the worker may touch"},
                    "boundary": {"type": "string", "description": "what the worker must never do"},
                    "model": {"type": "string", "description": "model the worker runs on; omit to inherit"},
                    "effort": {"type": "string", "description": "reasoning effort; omit to inherit"},
                    "max_turns": {"type": "integer", "description": "turn budget before the worker is marked partial"}
                },
                "required": ["objective", "output_format", "tool_allowlist", "file_scope", "boundary"]
            }),
        }
    }

    fn permission(&self, args: &Value) -> std::result::Result<PermissionRequest, VikramError> {
        let objective = arg_str(args, "objective")?;
        Ok(PermissionRequest::new(
            ActionKind::Delegate,
            "spawn_worker",
            objective,
            SideEffect::LocalMutation,
            Trust::Model,
        ))
    }

    fn run(&self, args: &Value, ctx: &ToolContext) -> std::result::Result<ToolOutput, VikramError> {
        let brief = WorkerBrief::from_args(args)?;
        let max_turns = args.get("max_turns").and_then(Value::as_u64);
        let Some(runtime) = ctx.runtime.as_ref() else {
            return Ok(ToolOutput {
                content: "spawn_worker needs a live runtime".into(),
                ok: false,
            });
        };
        let Some(parent) = ctx.session.clone() else {
            return Ok(ToolOutput {
                content: "spawn_worker needs a session to delegate from".into(),
                ok: false,
            });
        };
        let handle = RuntimeHandle {
            inner: Arc::clone(runtime),
        };
        match handle.spawn_worker(parent, brief, max_turns) {
            Ok(id) => Ok(ToolOutput {
                content: format!(
                    "worker {id} started; collect its result with worker.collect {{\"id\": \"{id}\"}}"
                ),
                ok: true,
            }),
            Err(err) => Ok(ToolOutput {
                content: err.to_string(),
                ok: false,
            }),
        }
    }
}

#[cfg(test)]
#[allow(
    clippy::await_holding_lock,
    reason = "isolated_home guards process-global env vars; releasing it at an await would let a concurrent test repoint VIKRAM_HOME mid-run"
)]
mod tests {
    use super::*;
    use crate::provider::{CompletionRequest, Provider, ProviderEvent, ProviderStream};
    use crate::runtime::Runtime;

    fn isolated_home(dir: &std::path::Path) -> std::sync::MutexGuard<'static, ()> {
        let guard = crate::config::lock_security_env();
        unsafe { std::env::set_var("VIKRAM_HOME", dir.join("home")) };
        guard
    }

    fn config_for(dir: &std::path::Path) -> crate::config::Config {
        crate::config::Config {
            model: "test-model".into(),
            workspace_root: Some(dir.to_path_buf()),
            ..crate::config::Config::default()
        }
    }

    /// A provider that replays one canned stream per request; running out of script yields an
    /// empty stream, which ends a turn the same way a model with nothing more to say would.
    struct ScriptedProvider {
        script: std::sync::Mutex<
            std::collections::VecDeque<Vec<std::result::Result<ProviderEvent, VikramError>>>,
        >,
        models: std::sync::Mutex<Vec<String>>,
    }

    impl ScriptedProvider {
        fn new(batches: Vec<Vec<ProviderEvent>>) -> Arc<Self> {
            Arc::new(Self {
                script: std::sync::Mutex::new(
                    batches
                        .into_iter()
                        .map(|batch| batch.into_iter().map(Ok).collect())
                        .collect(),
                ),
                models: std::sync::Mutex::new(Vec::new()),
            })
        }

        /// The model each request actually carried, in request order.
        fn models(&self) -> Vec<String> {
            self.models.lock().expect("models lock").clone()
        }

        /// A script of raw results, for turns that must fail mid-stream.
        fn raw(batches: Vec<Vec<std::result::Result<ProviderEvent, VikramError>>>) -> Arc<Self> {
            Arc::new(Self {
                script: std::sync::Mutex::new(batches.into()),
                models: std::sync::Mutex::new(Vec::new()),
            })
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
            Some(32_768)
        }

        fn stream(&self, request: CompletionRequest) -> ProviderStream {
            self.models
                .lock()
                .expect("models lock")
                .push(request.model.clone());
            let batch = self
                .script
                .lock()
                .expect("script lock")
                .pop_front()
                .unwrap_or_default();
            Box::pin(futures_util::stream::iter(batch))
        }
    }

    /// Wait until the worker's turn has genuinely finished, so assertions read settled state
    /// instead of racing a turn that has not started or is still streaming. The receiver must be
    /// subscribed before the spawn: a broadcast channel does not replay events for late joiners.
    async fn settle_worker(
        handle: &RuntimeHandle,
        events: &mut tokio::sync::broadcast::Receiver<RuntimeEvent>,
        id: &WorkerId,
    ) -> WorkerRecord {
        let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(5);
        loop {
            let record = handle
                .inner
                .workers
                .by_id(id)
                .expect("worker is registered");
            match events.try_recv() {
                Ok(RuntimeEvent::TurnFinished { session, .. }) if session == record.session => {
                    return record;
                }
                Ok(_) => {}
                Err(tokio::sync::broadcast::error::TryRecvError::Empty)
                | Err(tokio::sync::broadcast::error::TryRecvError::Lagged(_)) => {}
                Err(tokio::sync::broadcast::error::TryRecvError::Closed) => {
                    return record;
                }
            }
            if tokio::time::Instant::now() >= deadline {
                panic!("worker {id} never settled");
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
    }

    fn plain_brief(dir: &std::path::Path) -> WorkerBrief {
        WorkerBrief {
            objective: "audit the policy crate".into(),
            output_format: "a markdown findings report".into(),
            tool_allowlist: vec!["fs.read".into(), "fs.write".into()],
            file_scope: vec![dir.join("scope").to_string_lossy().into_owned()],
            boundary: "never edit a file outside the scope".into(),
            model: None,
            effort: None,
        }
    }

    #[tokio::test]
    async fn spawn_worker_creates_child_with_brief() {
        let dir = tempfile::tempdir().unwrap();
        let _home = isolated_home(dir.path());
        let provider = ScriptedProvider::new(vec![vec![
            ProviderEvent::TextDelta("working".into()),
            ProviderEvent::Finished {
                input_tokens: Some(4),
                output_tokens: Some(4),
            },
        ]]);
        let runtime = Runtime::start_with_provider(
            config_for(dir.path()),
            Arc::clone(&provider) as Arc<dyn Provider>,
        )
        .await
        .expect("runtime boots with the scripted provider");
        let handle = runtime.handle();
        let parent = handle.snapshot().sessions[0].id.clone();
        let mut events = handle.events();

        let brief = plain_brief(dir.path());
        let id = handle
            .spawn_worker(parent.clone(), brief.clone(), Some(3))
            .expect("the orchestrator spawns a worker");

        let record = handle
            .inner
            .workers
            .by_id(&id)
            .expect("the worker is registered");
        assert_eq!(record.brief, brief);
        assert_eq!(record.max_turns, Some(3));
        assert_eq!(record.status, WorkerStatus::Running);
        assert_eq!(record.parent, parent);
        assert_eq!(record.attempts, 1);

        let child = handle
            .inner
            .session(&record.session)
            .expect("the child session is live");
        assert_eq!(child.parent(), Some(parent.clone()));
        assert_eq!(child.depth(), 1);
        assert_eq!(child.worker_max_turns(), 3);

        let transcript = child.messages();
        let brief_message = transcript
            .iter()
            .find(|m| m.text_body().contains("audit the policy crate"))
            .expect("the brief reached the child");
        assert!(
            brief_message
                .text_body()
                .contains("a markdown findings report")
        );
        assert!(
            brief_message
                .text_body()
                .contains("never edit a file outside the scope")
        );

        let scope = child.capability_scope();
        assert!(scope.allows_verb("fs.read"));
        assert!(scope.allows_verb("fs.write"));
        assert!(!scope.allows_verb("shell.run"));

        // The spawn is journaled on the orchestrator, not copied into its transcript.
        let parent_session = handle.inner.session(&parent).expect("parent is live");
        assert!(
            parent_session
                .messages()
                .iter()
                .all(|m| !m.text_body().contains("Output format"))
        );

        let _ = settle_worker(&handle, &mut events, &id).await;
    }

    #[tokio::test]
    async fn worker_cannot_write_outside_file_scope() {
        let dir = tempfile::tempdir().unwrap();
        let _home = isolated_home(dir.path());
        let provider = ScriptedProvider::new(vec![vec![
            ProviderEvent::ToolCall(vikram_contracts::ToolCall::new(
                "fs.write",
                serde_json::json!({"path": "elsewhere/out.txt", "content": "nope"}),
            )),
            ProviderEvent::Finished {
                input_tokens: Some(4),
                output_tokens: Some(4),
            },
        ]]);
        let runtime = Runtime::start_with_provider(
            config_for(dir.path()),
            Arc::clone(&provider) as Arc<dyn Provider>,
        )
        .await
        .expect("runtime boots with the scripted provider");
        let handle = runtime.handle();
        let parent = handle.snapshot().sessions[0].id.clone();
        let mut events = handle.events();

        let mut brief = plain_brief(dir.path());
        brief.tool_allowlist = vec!["fs.read".into(), "fs.write".into()];
        brief.file_scope = vec![dir.path().join("scope").to_string_lossy().into_owned()];
        let id = handle
            .spawn_worker(parent, brief, None)
            .expect("the orchestrator spawns a worker");
        let worker = handle.inner.workers.by_id(&id).expect("worker record");
        let _ = settle_worker(&handle, &mut events, &id).await;

        // The denial is audit-logged in the worker's own journal, as a policy decision.
        let store = vikram_store::Store::open(&crate::config::database_path()).unwrap();
        let records = store.replay_messages(&worker.session).unwrap();
        let denial = records
            .iter()
            .find(|record| {
                record.kind == vikram_contracts::RecordKind::Policy
                    && record
                        .payload
                        .get("verb")
                        .and_then(serde_json::Value::as_str)
                        == Some("fs.write")
            })
            .expect("the out-of-scope write is journaled as a policy decision");
        assert_eq!(
            denial
                .payload
                .get("evaluated")
                .and_then(serde_json::Value::as_str),
            Some("deny"),
            "the policy machine, not a prompt, refused it"
        );
        let reason = denial
            .payload
            .get("reason")
            .and_then(serde_json::Value::as_str)
            .unwrap_or_default();
        assert!(reason.contains("file scope"), "{reason}");

        // Refused before execution: the file was never created.
        assert!(
            !dir.path().join("elsewhere").join("out.txt").exists(),
            "a denied write must leave no effect behind"
        );
    }

    #[tokio::test]
    async fn child_trust_is_monotonic_narrower() {
        let dir = tempfile::tempdir().unwrap();
        let _home = isolated_home(dir.path());
        let scope_a = dir.path().join("scope").to_string_lossy().into_owned();
        let outside = dir.path().join("OUTSIDE");
        // One scripted round, consumed by the first worker spawned; every later turn sees an
        // empty stream and ends immediately, so phases stay independent of turn scheduling.
        let provider = ScriptedProvider::new(vec![vec![
            ProviderEvent::ToolCall(vikram_contracts::ToolCall::new(
                "spawn_worker",
                serde_json::json!({
                    "objective": "spawn a worker that can write anywhere",
                    "output_format": "text",
                    "tool_allowlist": ["fs.write"],
                    "file_scope": ["/"],
                    "boundary": "none",
                }),
            )),
            ProviderEvent::Finished {
                input_tokens: Some(4),
                output_tokens: Some(4),
            },
        ]]);
        let runtime = Runtime::start_with_provider(
            config_for(dir.path()),
            Arc::clone(&provider) as Arc<dyn Provider>,
        )
        .await
        .expect("runtime boots with the scripted provider");
        let handle = runtime.handle();
        let parent = handle.snapshot().sessions[0].id.clone();
        let mut events = handle.events();

        // Phase one — a worker without the grant tries to mint trust by spawning.
        let mut escape = plain_brief(dir.path());
        escape.tool_allowlist = vec!["fs.read".into()];
        escape.file_scope = vec![scope_a.clone()];
        let escape_id = handle
            .spawn_worker(parent.clone(), escape, None)
            .expect("the ungranted worker spawns");
        let escape_record = handle.inner.workers.by_id(&escape_id).unwrap();
        let _ = settle_worker(&handle, &mut events, &escape_id).await;

        // The escape was refused by the policy machine, and it stayed childless: a denial
        // cannot be routed around by minting a new worker.
        let store = vikram_store::Store::open(&crate::config::database_path()).unwrap();
        let records = store.replay_messages(&escape_record.session).unwrap();
        let denial = records
            .iter()
            .find(|record| {
                record.kind == vikram_contracts::RecordKind::Policy
                    && record
                        .payload
                        .get("verb")
                        .and_then(serde_json::Value::as_str)
                        == Some("spawn_worker")
            })
            .expect("the escape attempt is journaled as a policy decision");
        assert_eq!(
            denial
                .payload
                .get("evaluated")
                .and_then(serde_json::Value::as_str),
            Some("deny")
        );
        let reason = denial
            .payload
            .get("reason")
            .and_then(serde_json::Value::as_str)
            .unwrap_or_default();
        assert!(reason.contains("tool allowlist"), "{reason}");
        assert_eq!(
            handle.inner.workers.list().len(),
            1,
            "the refused spawn created nothing"
        );

        // Phase two — a worker WITH the grant still cannot hand down more than it holds. The
        // lineage is built synchronously; turn starts happen only at the next await, so every
        // session in the chain is alive while its child is being spawned.
        let mut granted = plain_brief(dir.path());
        granted.tool_allowlist = vec!["fs.read".into(), "fs.write".into(), "spawn_worker".into()];
        granted.file_scope = vec![scope_a.clone()];
        let granted_id = handle
            .spawn_worker(parent.clone(), granted, None)
            .expect("the granted worker spawns");
        let granted_record = handle.inner.workers.by_id(&granted_id).unwrap();

        // The granted worker's child asked for more than its parent holds — and got the
        // intersection, not the ask.
        let mut greedy = plain_brief(dir.path());
        greedy.tool_allowlist = vec![
            "fs.read".into(),
            "fs.write".into(),
            "spawn_worker".into(),
            "shell.run".into(),
        ];
        greedy.file_scope = vec![scope_a.clone(), outside.to_string_lossy().into_owned()];
        let child_id = handle
            .spawn_worker(granted_record.session.clone(), greedy, None)
            .expect("a granted worker may spawn");
        let child = handle.inner.workers.by_id(&child_id).unwrap();

        // The chain is built to the floor in the same synchronous stretch.
        let mut chain = plain_brief(dir.path());
        chain.tool_allowlist = vec!["fs.read".into()];
        chain.file_scope = vec![scope_a.clone()];
        let grandchild_id = handle
            .spawn_worker(child.session.clone(), chain.clone(), None)
            .expect("depth 3 is the last link the floor permits");
        let grandchild = handle.inner.workers.by_id(&grandchild_id).unwrap();
        let refused = handle
            .spawn_worker(grandchild.session.clone(), chain, None)
            .expect_err("depth 4 exceeds the delegation floor");
        assert!(
            refused.to_string().contains("delegation depth"),
            "{refused}"
        );
        let _ = settle_worker(&handle, &mut events, &granted_id).await;
        assert_eq!(child.depth, 2);
        assert_eq!(
            child.scope.tools,
            Some(vec![
                "fs.read".to_owned(),
                "fs.write".to_owned(),
                "spawn_worker".to_owned(),
            ]),
            "`shell.run` was never the parent's to grant"
        );
        assert_eq!(
            child.scope.file_scope,
            Some(vec![scope_a.clone()]),
            "the outside path was cut at spawn"
        );
        // Nothing the child holds reaches outside its parent, whatever the brief claimed.
        let parent_scope = granted_record.scope.clone();
        for tool in child.scope.tools.as_deref().unwrap_or(&[]) {
            assert!(parent_scope.allows_verb(tool), "child gained `{tool}`");
        }
        for root in child.scope.file_scope.as_deref().unwrap_or(&[]) {
            assert!(
                parent_scope.contains_path(root),
                "child gained file reach `{root}`"
            );
        }

        // A denial the parent earned is inherited by every descendant; a denial the
        // orchestrator itself earned (the floor) is inherited by every worker and sibling.
        let beyond = PermissionRequest::new(
            vikram_contracts::ActionKind::Write,
            "fs.write",
            outside.join("x.txt").to_string_lossy().into_owned(),
            vikram_contracts::SideEffect::LocalMutation,
            Trust::Model,
        );
        for (scope, depth) in [
            (granted_record.scope.clone(), 1u32),
            (child.scope.clone(), 2),
        ] {
            match handle.inner.judge_scoped(&beyond, &scope, depth) {
                vikram_contracts::policy::Decision::Deny { reason } => {
                    assert!(reason.contains("file scope"), "{reason}")
                }
                other => panic!("the inherited denial was lost, got {other:?}"),
            }
        }
        let floor_denied = PermissionRequest::new(
            vikram_contracts::ActionKind::Write,
            "fs.write",
            "/etc/passwd",
            vikram_contracts::SideEffect::LocalMutation,
            Trust::Model,
        );
        let mut sibling = plain_brief(dir.path());
        sibling.file_scope = vec![scope_a.clone(), outside.to_string_lossy().into_owned()];
        let sibling_id = handle
            .spawn_worker(parent.clone(), sibling, None)
            .expect("a sibling may spawn with its own brief");
        let sibling_scope = handle.inner.workers.by_id(&sibling_id).unwrap().scope;
        for scope in [
            CapabilityScope::unrestricted(),
            granted_record.scope.clone(),
            child.scope.clone(),
            sibling_scope.clone(),
        ] {
            assert!(
                matches!(
                    handle.inner.judge_scoped(&floor_denied, &scope, 1),
                    vikram_contracts::policy::Decision::Deny { .. }
                ),
                "what the orchestrator cannot do, nobody below it can re-obtain"
            );
        }
    }

    #[tokio::test]
    async fn worker_results_handoff_via_store() {
        let dir = tempfile::tempdir().unwrap();
        let _home = isolated_home(dir.path());
        let provider = ScriptedProvider::new(vec![vec![
            ProviderEvent::TextDelta("the audit found three findings: A, B, C".into()),
            ProviderEvent::Finished {
                input_tokens: Some(8),
                output_tokens: Some(8),
            },
        ]]);
        let runtime = Runtime::start_with_provider(
            config_for(dir.path()),
            Arc::clone(&provider) as Arc<dyn Provider>,
        )
        .await
        .expect("runtime boots with the scripted provider");
        let handle = runtime.handle();
        let parent = handle.snapshot().sessions[0].id.clone();
        let mut events = handle.events();

        let id = handle
            .spawn_worker(parent.clone(), plain_brief(dir.path()), None)
            .expect("the orchestrator spawns a worker");
        let record = handle.inner.workers.by_id(&id).unwrap();
        let _ = settle_worker(&handle, &mut events, &id).await;

        // The finished run is an artifact in the store, registered against the worker session.
        let settled = handle.inner.workers.by_id(&id).unwrap();
        assert_eq!(settled.status, WorkerStatus::Done);
        let artifact_id = settled.artifact.clone().expect("an artifact was stored");
        let store = vikram_store::Store::open(&crate::config::database_path()).unwrap();
        let row = store
            .get_artifact(&artifact_id)
            .unwrap()
            .expect("the artifact row exists");
        assert_eq!(row.session, record.session);
        assert_eq!(row.source.as_deref(), Some("worker"));
        let content = std::fs::read_to_string(&row.path).unwrap();
        assert!(
            content.contains("the audit found three findings: A, B, C"),
            "{content}"
        );

        // The parent learned a reference, and never the child's transcript.
        let parent_session = handle.inner.session(&parent).expect("parent is live");
        let transcript = parent_session.messages();
        assert!(
            transcript
                .iter()
                .any(|m| m.text_body().contains(&artifact_id)),
            "the reference reached the orchestrator"
        );
        assert!(
            transcript
                .iter()
                .all(|m| !m.text_body().contains("the audit found three findings")),
            "the child's answer text never crosses the boundary"
        );

        // One-shot: the finished worker's session is reaped, its row durable.
        assert!(
            handle.inner.session(&record.session).is_none(),
            "the finished worker's session is reaped"
        );
        let closed = store
            .get_session(&record.session)
            .unwrap()
            .expect("the row survives");
        assert_eq!(closed.status, "closed");

        // The orchestrator consumes the reference through the tool, not the transcript.
        let ctx = ToolContext {
            runtime: Some(Arc::clone(&handle.inner)),
            session: Some(parent.clone()),
            ..ToolContext::default()
        };
        let out = WorkerCollect
            .run(&serde_json::json!({"id": id.to_string()}), &ctx)
            .unwrap();
        assert!(out.ok, "{}", out.content);
        assert!(
            out.content
                .contains("the audit found three findings: A, B, C"),
            "{}",
            out.content
        );
    }

    #[tokio::test]
    async fn max_turns_marks_partial_and_resumable() {
        let dir = tempfile::tempdir().unwrap();
        let _home = isolated_home(dir.path());
        // Round one spends the worker's single turn on a tool call; round two is what the
        // resumed run consumes.
        let provider = ScriptedProvider::new(vec![
            vec![
                ProviderEvent::ToolCall(vikram_contracts::ToolCall::new(
                    "fs.read",
                    serde_json::json!({"path": "scope"}),
                )),
                ProviderEvent::Finished {
                    input_tokens: Some(4),
                    output_tokens: Some(4),
                },
            ],
            vec![
                ProviderEvent::TextDelta("resumed and finished the audit".into()),
                ProviderEvent::Finished {
                    input_tokens: Some(4),
                    output_tokens: Some(4),
                },
            ],
        ]);
        let runtime = Runtime::start_with_provider(
            config_for(dir.path()),
            Arc::clone(&provider) as Arc<dyn Provider>,
        )
        .await
        .expect("runtime boots with the scripted provider");
        let handle = runtime.handle();
        let parent = handle.snapshot().sessions[0].id.clone();
        let mut events = handle.events();

        let mut brief = plain_brief(dir.path());
        brief.tool_allowlist = vec!["fs.read".into(), "fs.write".into()];
        brief.file_scope = vec![dir.path().join("scope").to_string_lossy().into_owned()];
        let id = handle
            .spawn_worker(parent.clone(), brief, Some(1))
            .expect("the orchestrator spawns a budgeted worker");
        let record = handle.inner.workers.by_id(&id).unwrap();
        let _ = settle_worker(&handle, &mut events, &id).await;

        // The budget wall: partial, not failed, and the session is kept for the resume.
        let partial = handle.inner.workers.by_id(&id).unwrap();
        assert_eq!(partial.status, WorkerStatus::Partial);
        assert!(
            handle.inner.session(&record.session).is_some(),
            "a partial worker's session is kept, not reaped"
        );
        let parent_session = handle.inner.session(&parent).expect("parent is live");
        assert!(
            parent_session
                .messages()
                .iter()
                .any(|m| m.text_body().contains("stopped at its turn budget")),
            "the orchestrator was told the worker is partial"
        );

        // The team keeps running: the resume continues the same worker, same session.
        handle.resume_worker(&id).expect("a partial worker resumes");
        assert_eq!(
            handle.inner.workers.by_id(&id).unwrap().status,
            WorkerStatus::Running
        );
        let _ = settle_worker(&handle, &mut events, &id).await;

        let done = handle.inner.workers.by_id(&id).unwrap();
        assert_eq!(done.status, WorkerStatus::Done);
        let artifact_id = done
            .artifact
            .clone()
            .expect("the resumed run produced a result");
        let store = vikram_store::Store::open(&crate::config::database_path()).unwrap();
        let row = store
            .get_artifact(&artifact_id)
            .unwrap()
            .expect("the artifact row exists");
        let content = std::fs::read_to_string(&row.path).unwrap();
        assert!(
            content.contains("resumed and finished the audit"),
            "{content}"
        );
        assert!(
            handle.inner.session(&record.session).is_none(),
            "the finished worker is reaped one-shot"
        );
        // And the orchestrator itself was never interrupted by any of it.
        assert!(handle.inner.session(&parent).is_some());
    }

    #[tokio::test]
    async fn worker_failure_one_retry_then_reassign() {
        let dir = tempfile::tempdir().unwrap();
        let _home = isolated_home(dir.path());
        // Run one fails, the single retry fails, the replacement finishes.
        let provider = ScriptedProvider::raw(vec![
            vec![Err(VikramError::ProviderRejected("boom".into()))],
            vec![Err(VikramError::ProviderRejected("boom again".into()))],
            vec![
                Ok(ProviderEvent::TextDelta(
                    "the replacement finished it".into(),
                )),
                Ok(ProviderEvent::Finished {
                    input_tokens: Some(4),
                    output_tokens: Some(4),
                }),
            ],
        ]);
        let runtime = Runtime::start_with_provider(
            config_for(dir.path()),
            Arc::clone(&provider) as Arc<dyn Provider>,
        )
        .await
        .expect("runtime boots with the scripted provider");
        let handle = runtime.handle();
        let parent = handle.snapshot().sessions[0].id.clone();

        let brief = plain_brief(dir.path());
        let original_id = handle
            .spawn_worker(parent.clone(), brief.clone(), None)
            .expect("the orchestrator spawns a worker");

        // The chain settles on its own: fail, retry, fail, reassign, replacement runs.
        let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(10);
        let (original, replacement) = loop {
            let workers = handle.inner.workers.list();
            let original = workers
                .iter()
                .find(|record| record.id == original_id)
                .expect("the original record survives its own failure");
            let replacement = workers.iter().find(|record| {
                record.id != original_id
                    && record.parent == parent
                    && record.brief == brief
                    && record.status == WorkerStatus::Done
            });
            if original.status == WorkerStatus::Degraded
                && original.attempts == 2
                && let Some(replacement) = replacement
            {
                break (original.clone(), replacement.clone());
            }
            if tokio::time::Instant::now() >= deadline {
                panic!("the fail-retry-reassign chain never settled: {workers:?}");
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        };

        // Exactly one retry, then the brief moved on.
        assert_eq!(
            original.attempts, 2,
            "the second failure earns no third run"
        );
        assert_eq!(original.status, WorkerStatus::Degraded);
        assert_eq!(
            replacement.brief, brief,
            "the replacement carries the same brief"
        );
        let artifact_id = replacement
            .artifact
            .clone()
            .expect("the replacement produced a result");
        let store = vikram_store::Store::open(&crate::config::database_path()).unwrap();
        let row = store
            .get_artifact(&artifact_id)
            .unwrap()
            .expect("the artifact row exists");
        let content = std::fs::read_to_string(&row.path).unwrap();
        assert!(content.contains("the replacement finished it"), "{content}");

        // The orchestrator was told at every step, and nothing about it was interrupted.
        let parent_session = handle.inner.session(&parent).expect("parent is live");
        let transcript = parent_session.messages();
        assert!(
            transcript
                .iter()
                .any(|m| m.text_body().contains("failed once — retrying")),
            "the orchestrator was told about the retry"
        );
        assert!(
            transcript
                .iter()
                .any(|m| m.text_body().contains("failed twice")),
            "the orchestrator was told about the reassignment"
        );

        // The team keeps running: a sibling spawns and settles long after the failure.
        let sibling_id = handle
            .spawn_worker(parent.clone(), plain_brief(dir.path()), None)
            .expect("the team still accepts work");
        let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(10);
        loop {
            if handle
                .inner
                .workers
                .by_id(&sibling_id)
                .is_some_and(|record| record.status == WorkerStatus::Done)
            {
                break;
            }
            if tokio::time::Instant::now() >= deadline {
                panic!("the sibling never finished");
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        assert!(handle.inner.session(&parent).is_some());
    }

    #[tokio::test]
    async fn role_model_routing_overrides_default() {
        let dir = tempfile::tempdir().unwrap();
        let _home = isolated_home(dir.path());
        let provider = ScriptedProvider::new(vec![
            vec![
                ProviderEvent::TextDelta("routed to my own model".into()),
                ProviderEvent::Finished {
                    input_tokens: Some(4),
                    output_tokens: Some(4),
                },
            ],
            vec![
                ProviderEvent::TextDelta("ran on the default".into()),
                ProviderEvent::Finished {
                    input_tokens: Some(4),
                    output_tokens: Some(4),
                },
            ],
        ]);
        let runtime = Runtime::start_with_provider(
            config_for(dir.path()),
            Arc::clone(&provider) as Arc<dyn Provider>,
        )
        .await
        .expect("runtime boots with the scripted provider");
        let handle = runtime.handle();
        let parent = handle.snapshot().sessions[0].id.clone();
        let mut events = handle.events();

        // A brief that names its model and effort: both are stamped on the child, and the
        // request the child's turn sends actually carries the choice.
        let mut routed = plain_brief(dir.path());
        routed.model = Some("big-model".into());
        routed.effort = Some("high".into());
        let routed_id = handle
            .spawn_worker(parent.clone(), routed, None)
            .expect("the routed worker spawns");
        let routed_record = handle.inner.workers.by_id(&routed_id).unwrap();
        let routed_session = handle
            .inner
            .session(&routed_record.session)
            .expect("the child is live before its turn ends");
        assert_eq!(routed_session.model().as_deref(), Some("big-model"));
        assert_eq!(routed_session.effort().as_deref(), Some("high"));
        let store = vikram_store::Store::open(&crate::config::database_path()).unwrap();
        let row = store.get_session(&routed_record.session).unwrap().unwrap();
        assert_eq!(row.model.as_deref(), Some("big-model"));
        let _ = settle_worker(&handle, &mut events, &routed_id).await;
        assert_eq!(provider.models()[0], "big-model");

        // A brief with absent fields: the worker inherits the orchestrator's live choices.
        let inherited_id = handle
            .spawn_worker(parent, plain_brief(dir.path()), None)
            .expect("the inheriting worker spawns");
        let inherited_record = handle.inner.workers.by_id(&inherited_id).unwrap();
        let inherited_session = handle
            .inner
            .session(&inherited_record.session)
            .expect("the child is live before its turn ends");
        assert_eq!(inherited_session.model().as_deref(), Some("test-model"));
        assert_eq!(inherited_session.effort(), None);
        let _ = settle_worker(&handle, &mut events, &inherited_id).await;
        assert_eq!(provider.models()[1], "test-model");
    }
}
