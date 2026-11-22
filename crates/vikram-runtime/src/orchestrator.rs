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
use crate::runtime::RuntimeHandle;
use crate::tools::*;
use crate::types::{RuntimeEvent, now_ms};

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
        script: std::sync::Mutex<std::collections::VecDeque<Vec<ProviderEvent>>>,
    }

    impl ScriptedProvider {
        fn new(batches: Vec<Vec<ProviderEvent>>) -> Arc<Self> {
            Arc::new(Self {
                script: std::sync::Mutex::new(batches.into()),
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
            Some(8192)
        }

        fn stream(&self, _request: CompletionRequest) -> ProviderStream {
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
}
