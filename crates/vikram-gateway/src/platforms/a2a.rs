//! A2A (Agent-to-Agent) protocol engine — transport-free JSON-RPC over HTTP.
//!
//! Rust port of hermes-agent `plugins/platforms/a2a/protocol.py` plus the JSON-RPC half of
//! `adapter.py`: the Agent Card, envelope validation, the task lifecycle (`message/send`,
//! `tasks/get`, `tasks/list`, `tasks/cancel`) and the terminal transitions a host drives
//! (`complete` / `fail`). There is no listener, no socket and no runtime here — [`A2aEngine`]
//! takes an already-parsed JSON-RPC request and returns the response envelope, so a host can
//! serve it from any HTTP stack and map a task's `contextId` onto its own session.
//!
//! Ported behaviour, with the hermes origin:
//! - Envelope validation (`adapter.do_POST`): a non-object body, a `jsonrpc` other than `"2.0"`,
//!   an `id` that is neither string nor number, or a non-string `method` is `-32600`; an unknown
//!   method is `-32601` and echoes the request `id` when one was readable.
//! - `message/send`: every `text` part of `params.message` is concatenated, the text is trimmed,
//!   and one message with no usable text is `-32602` (hermes ends such a task `rejected`; here the
//!   message is refused before a task exists). A new task starts `submitted` with an empty
//!   `artifacts` array and the inbound message in `history[0]`.
//! - `contextId` is read from the message (hermes v1.0 style) and then from the top-level params
//!   (legacy style); a message without one opens a fresh context, so every task is reachable by
//!   context for a host mapping tasks to sessions.
//! - `tasks/get` accepts `params.id` (this port's spec) and `params.taskId` (hermes); `-32001`
//!   `TaskNotFoundError` when unknown.
//! - `tasks/cancel` transitions a non-terminal task to `canceled`; an already terminal task is
//!   refused with `-32002` `TaskNotCancelableError` (hermes `_rpc_tasks_cancel`).
//! - `tasks/list` returns `{"tasks": [...]}` in creation order, optionally filtered by
//!   `params.contextId`. hermes also pages (`pageSize` / `pageToken` / `totalSize`); paging is a
//!   host concern here because the store is bounded only by the host's lifetime.
//! - Error codes `-32001` / `-32002` keep their A2A meaning; the engine adds no custom codes.
//!
//! Deliberately not ported (see `docs/gateway-port-plan.md`): the HTTP/SSE transport and its
//! `message/stream` / `tasks/subscribe` handlers, push-notification configs, the per-context turn
//! tracker, rate limiting, the audit log, conversation persistence, trust-gated peer identity and
//! served-agent (tenant) routing. `message/stream` is answered `-32601` rather than faked, and the
//! Agent Card advertises `streaming: false` / `pushNotifications: false` to match.
//!
//! Wire shape: A2A 0.2.x — lowercase task states, `kind: "task"` on the task, and parts tagged
//! with `type` (the engine also accepts untagged parts and 0.3 `kind`-tagged parts, since only the
//! `text` member is read). Timestamps are RFC-3339 UTC strings with millisecond precision, derived
//! from [`SystemTime`] so no date crate is needed.
//!
//! Store behaviour worth knowing: tasks live in memory for the lifetime of the engine, terminal
//! tasks included (so `tasks/get` keeps answering — hermes trims to the last 500, this port leaves
//! retention to the host), and every transition is idempotent-or-refused, so a replayed HTTP
//! request cannot move a task twice.

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Mutex, MutexGuard};
use std::time::{SystemTime, UNIX_EPOCH};

use serde_json::{Map, Value, json};

/// A2A protocol version this engine speaks: the 0.2.x JSON-RPC shape it actually serves
/// (lowercase states, `kind`-tagged task, `type`-tagged parts), advertised on the Agent Card.
pub const PROTOCOL_VERSION: &str = "0.2.5";

/// JSON-RPC 2.0 and A2A error codes the engine can return.
pub mod error_code {
    /// The request is not a usable JSON-RPC 2.0 request object.
    pub const INVALID_REQUEST: i64 = -32600;
    /// The method is unknown, or known but not served (`message/stream`).
    pub const METHOD_NOT_FOUND: i64 = -32601;
    /// The method exists but its params are unusable (no message, no text part).
    pub const INVALID_PARAMS: i64 = -32602;
    /// A2A `TaskNotFoundError`: no task with that id.
    pub const TASK_NOT_FOUND: i64 = -32001;
    /// A2A `TaskNotCancelableError`: the task already reached a terminal state.
    pub const TASK_NOT_CANCELABLE: i64 = -32002;
}

/// Task lifecycle state, in the lowercase wire form of A2A 0.2.x.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TaskState {
    /// Accepted, not yet picked up by the agent.
    Submitted,
    /// The host dispatched the task to a runtime session.
    Working,
    /// Finished successfully; its reply is in `artifacts`.
    Completed,
    /// Cancelled by a peer before it finished.
    Canceled,
    /// Finished unsuccessfully; the reason is in `status.message`.
    Failed,
}

impl TaskState {
    /// The wire spelling (`"submitted"`, `"completed"`, …).
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Submitted => "submitted",
            Self::Working => "working",
            Self::Completed => "completed",
            Self::Canceled => "canceled",
            Self::Failed => "failed",
        }
    }

    /// A terminal task never changes state again (`completed` / `canceled` / `failed`).
    pub fn is_terminal(self) -> bool {
        matches!(self, Self::Completed | Self::Canceled | Self::Failed)
    }
}

/// One stored A2A task: the unit a peer polls with `tasks/get` and a host resolves with
/// [`A2aEngine::complete`] or [`A2aEngine::fail`].
#[derive(Debug, Clone)]
pub struct Task {
    /// Creation order; keeps `tasks/list` deterministic over the hash map.
    pub seq: u64,
    /// The A2A task id (`task-…`), unique within one engine.
    pub id: String,
    /// The conversation this task belongs to (`ctx-…`), stable across a context's tasks.
    pub context_id: String,
    /// Current lifecycle state.
    pub state: TaskState,
    /// RFC-3339 UTC timestamp of the current state (A2A `status.timestamp`).
    pub timestamp: String,
    /// Output artifacts (`artifactId` + text parts); empty until [`A2aEngine::complete`].
    pub artifacts: Vec<Value>,
    /// Inbound messages of this task, oldest first — the original message is always index 0.
    pub history: Vec<Value>,
    /// Text carried on `status.message` (a failure reason); `None` when absent.
    pub status_message: Option<String>,
}

impl Task {
    /// Render as the A2A Task object every task method returns.
    pub fn to_value(&self) -> Value {
        let mut status = Map::new();
        status.insert(
            "state".to_string(),
            Value::String(self.state.as_str().to_string()),
        );
        status.insert(
            "timestamp".to_string(),
            Value::String(self.timestamp.clone()),
        );
        if let Some(text) = &self.status_message {
            let message_id = format!("{}-status", self.id);
            status.insert(
                "message".to_string(),
                agent_message(text, &self.context_id, &message_id),
            );
        }
        json!({
            "id": self.id,
            "contextId": self.context_id,
            "kind": "task",
            "status": Value::Object(status),
            "artifacts": Value::Array(self.artifacts.clone()),
            "history": Value::Array(self.history.clone()),
        })
    }
}

/// Transport-free A2A JSON-RPC engine.
///
/// Holds the in-memory task store and the identity the Agent Card advertises. Every method takes
/// `&self`, so a host can share one engine across HTTP worker threads (`Arc<A2aEngine>`); the
/// task store is behind a [`Mutex`] and ids come from a monotonic counter, so concurrent requests
/// never collide.
pub struct A2aEngine {
    tasks: Mutex<HashMap<String, Task>>,
    next_id: AtomicU64,
    agent_name: String,
    agent_description: String,
    agent_version: String,
}

impl A2aEngine {
    /// A fresh engine with the default VIKRAM identity and an empty task store.
    pub fn new() -> Self {
        Self {
            tasks: Mutex::new(HashMap::new()),
            next_id: AtomicU64::new(1),
            agent_name: "vikram".to_string(),
            agent_description: "VIKRAM terminal AI runtime".to_string(),
            agent_version: env!("CARGO_PKG_VERSION").to_string(),
        }
    }

    /// Replace the name / description / version the Agent Card advertises (builder form).
    pub fn with_identity(
        mut self,
        name: impl Into<String>,
        description: impl Into<String>,
        version: impl Into<String>,
    ) -> Self {
        self.agent_name = name.into();
        self.agent_description = description.into();
        self.agent_version = version.into();
        self
    }

    /// Agent Card served at `/.well-known/agent.json`.
    ///
    /// Streaming and push notifications are advertised `false` because this engine has neither;
    /// the card never claims a capability the transport cannot honour.
    pub fn agent_card(&self) -> Value {
        json!({
            "name": self.agent_name,
            "description": self.agent_description,
            "version": self.agent_version,
            "protocolVersion": PROTOCOL_VERSION,
            "capabilities": {
                "streaming": false,
                "pushNotifications": false,
            },
            "defaultInputModes": ["text/plain"],
            "defaultOutputModes": ["text/plain"],
            "skills": [],
        })
    }

    /// Handle one parsed JSON-RPC request and return the response envelope.
    ///
    /// The result is always a well-formed JSON-RPC 2.0 response — `result` on success, `error`
    /// (with a code from [`error_code`]) otherwise. Both the slash methods of this port and the
    /// hermes v1.0 PascalCase aliases (`SendMessage`, `GetTask`, `ListTasks`, `CancelTask`) are
    /// accepted; anything else, including the streaming methods, is `-32601`.
    pub fn handle_jsonrpc(&self, request: &Value) -> Value {
        let Some(fields) = request.as_object() else {
            return error_response(
                Value::Null,
                error_code::INVALID_REQUEST,
                "request must be a JSON object",
            );
        };
        let id = request_id(fields);
        if fields.get("jsonrpc").and_then(Value::as_str) != Some("2.0") {
            return error_response(id, error_code::INVALID_REQUEST, "jsonrpc must be \"2.0\"");
        }
        if id.is_null() {
            return error_response(
                id,
                error_code::INVALID_REQUEST,
                "id must be a string or a number",
            );
        }
        let Some(method) = fields.get("method").and_then(Value::as_str) else {
            return error_response(id, error_code::INVALID_REQUEST, "method must be a string");
        };
        let params = fields.get("params").cloned().unwrap_or(Value::Null);
        match method {
            "message/send" | "SendMessage" => self.message_send(id, &params),
            "tasks/get" | "GetTask" => self.tasks_get(id, &params),
            "tasks/list" | "ListTasks" => self.tasks_list(id, &params),
            "tasks/cancel" | "CancelTask" => self.tasks_cancel(id, &params),
            "message/stream" | "SendStreamingMessage" => error_response(
                id,
                error_code::METHOD_NOT_FOUND,
                "message/stream is not supported: this agent does not stream",
            ),
            other => error_response(
                id,
                error_code::METHOD_NOT_FOUND,
                format!("method not found: {other}"),
            ),
        }
    }

    /// Mark a still-running task `completed` and attach `text` as its reply artifact.
    ///
    /// Returns the updated task, or `None` when the task is unknown or already terminal — a
    /// completed task cannot be completed twice, and a late reply cannot resurrect a cancelled
    /// one. The artifact id is derived from the task id and the artifact count, so it is stable
    /// and unique within the task.
    pub fn complete(&self, task_id: &str, text: &str) -> Option<Value> {
        let mut store = self.store();
        let task = store.get_mut(task_id)?;
        if task.state.is_terminal() {
            return None;
        }
        let artifact_id = format!("{}-artifact-{}", task.id, task.artifacts.len() + 1);
        task.state = TaskState::Completed;
        task.timestamp = now_rfc3339();
        task.status_message = None;
        task.artifacts.push(json!({
            "artifactId": artifact_id,
            "parts": [{ "type": "text", "text": text }],
        }));
        Some(task.to_value())
    }

    /// Mark a still-running task `failed`, carrying `reason` on `status.message`.
    ///
    /// Returns the updated task, or `None` when the task is unknown or already terminal.
    pub fn fail(&self, task_id: &str, reason: &str) -> Option<Value> {
        let mut store = self.store();
        let task = store.get_mut(task_id)?;
        if task.state.is_terminal() {
            return None;
        }
        task.state = TaskState::Failed;
        task.timestamp = now_rfc3339();
        task.status_message = Some(reason.to_string());
        Some(task.to_value())
    }

    /// Mark a submitted task `working`: the host accepted it and dispatched it to a session.
    ///
    /// Returns the updated task, or `None` when the task is unknown or already terminal. A task
    /// already `working` simply stays `working` with a refreshed timestamp.
    pub fn mark_working(&self, task_id: &str) -> Option<Value> {
        let mut store = self.store();
        let task = store.get_mut(task_id)?;
        if task.state.is_terminal() {
            return None;
        }
        task.state = TaskState::Working;
        task.timestamp = now_rfc3339();
        Some(task.to_value())
    }

    /// The `contextId` of a task that is still pending (`submitted` / `working`).
    ///
    /// This is the handle a host needs to map an inbound A2A task onto its own conversation
    /// session: a pending task has work outstanding, so its context is the session that must be
    /// answered. Terminal tasks (and unknown ids) return `None`.
    pub fn pending_context(&self, task_id: &str) -> Option<String> {
        self.store()
            .get(task_id)
            .filter(|task| !task.state.is_terminal())
            .map(|task| task.context_id.clone())
    }

    /// One task's current rendering, free of the lock.
    fn snapshot(&self, task_id: &str) -> Option<Task> {
        self.store().get(task_id).cloned()
    }

    /// The task store. A poisoned lock is recovered rather than propagated: a panic in an
    /// unrelated thread must not leave the protocol engine permanently unusable.
    fn store(&self) -> MutexGuard<'_, HashMap<String, Task>> {
        match self.tasks.lock() {
            Ok(guard) => guard,
            Err(poisoned) => poisoned.into_inner(),
        }
    }

    /// Allocate the next id of the engine-wide monotonic sequence (`task-7`, `ctx-8`, `msg-9`).
    fn next(&self, prefix: &str) -> String {
        format!("{prefix}-{}", self.next_id.fetch_add(1, Ordering::Relaxed))
    }

    /// `message/send`: turn one inbound message into a `submitted` task.
    fn message_send(&self, id: Value, params: &Value) -> Value {
        let Some(params) = params.as_object() else {
            return error_response(id, error_code::INVALID_PARAMS, "params must be an object");
        };
        let Some(message) = params.get("message").and_then(Value::as_object) else {
            return error_response(
                id,
                error_code::INVALID_PARAMS,
                "params.message must be an object",
            );
        };
        let text = message_text(message);
        if text.is_empty() {
            return error_response(
                id,
                error_code::INVALID_PARAMS,
                "params.message has no text part",
            );
        }
        let context_id = message
            .get("contextId")
            .and_then(Value::as_str)
            .or_else(|| params.get("contextId").and_then(Value::as_str))
            .filter(|value| !value.trim().is_empty())
            .map(str::to_owned)
            .unwrap_or_else(|| self.next("ctx"));
        let message_id = message
            .get("messageId")
            .and_then(Value::as_str)
            .filter(|value| !value.trim().is_empty())
            .map(str::to_owned)
            .unwrap_or_else(|| self.next("msg"));
        let seq = self.next_id.fetch_add(1, Ordering::Relaxed);
        let task = Task {
            seq,
            id: format!("task-{seq}"),
            context_id,
            state: TaskState::Submitted,
            timestamp: now_rfc3339(),
            artifacts: Vec::new(),
            history: vec![user_message(&text, &message_id)],
            status_message: None,
        };
        let rendered = task.to_value();
        self.store().insert(task.id.clone(), task);
        ok_response(id, rendered)
    }

    /// `tasks/get`: the stored task, or `-32001`.
    fn tasks_get(&self, id: Value, params: &Value) -> Value {
        let Some(params) = params.as_object() else {
            return error_response(id, error_code::INVALID_PARAMS, "params must be an object");
        };
        let Some(task_id) = task_id_param(params) else {
            return error_response(
                id,
                error_code::INVALID_PARAMS,
                "params.id must be a non-empty string",
            );
        };
        match self.snapshot(&task_id) {
            Some(task) => ok_response(id, task.to_value()),
            None => error_response(
                id,
                error_code::TASK_NOT_FOUND,
                format!("task not found: {task_id}"),
            ),
        }
    }

    /// `tasks/cancel`: transition a non-terminal task to `canceled`, or `-32002`.
    fn tasks_cancel(&self, id: Value, params: &Value) -> Value {
        let Some(params) = params.as_object() else {
            return error_response(id, error_code::INVALID_PARAMS, "params must be an object");
        };
        let Some(task_id) = task_id_param(params) else {
            return error_response(
                id,
                error_code::INVALID_PARAMS,
                "params.id must be a non-empty string",
            );
        };
        let mut store = self.store();
        let Some(task) = store.get_mut(&task_id) else {
            return error_response(
                id,
                error_code::TASK_NOT_FOUND,
                format!("task not found: {task_id}"),
            );
        };
        if task.state.is_terminal() {
            return error_response(
                id,
                error_code::TASK_NOT_CANCELABLE,
                format!("task {task_id} is already {}", task.state.as_str()),
            );
        }
        task.state = TaskState::Canceled;
        task.timestamp = now_rfc3339();
        ok_response(id, task.to_value())
    }

    /// `tasks/list`: stored tasks in creation order, optionally filtered by `params.contextId`.
    fn tasks_list(&self, id: Value, params: &Value) -> Value {
        let wanted = params
            .get("contextId")
            .and_then(Value::as_str)
            .filter(|value| !value.is_empty());
        let mut tasks: Vec<Task> = self
            .store()
            .values()
            .filter(|task| wanted.is_none_or(|context| task.context_id == context))
            .cloned()
            .collect();
        tasks.sort_by_key(|task| task.seq);
        let rendered: Vec<Value> = tasks.iter().map(Task::to_value).collect();
        ok_response(id, json!({ "tasks": rendered }))
    }
}

impl Default for A2aEngine {
    fn default() -> Self {
        Self::new()
    }
}

/// A successful JSON-RPC response.
fn ok_response(id: Value, result: Value) -> Value {
    json!({ "jsonrpc": "2.0", "id": id, "result": result })
}

/// A JSON-RPC error response.
fn error_response(id: Value, code: i64, message: impl Into<String>) -> Value {
    json!({
        "jsonrpc": "2.0",
        "id": id,
        "error": { "code": code, "message": message.into() },
    })
}

/// The envelope `id`, echoed verbatim when it is a string or a number and `null` otherwise —
/// never a guess at a value the peer did not send.
fn request_id(fields: &Map<String, Value>) -> Value {
    match fields.get("id") {
        Some(id @ (Value::String(_) | Value::Number(_))) => id.clone(),
        _ => Value::Null,
    }
}

/// The task id from `params.id`, falling back to hermes' `params.taskId`.
fn task_id_param(params: &Map<String, Value>) -> Option<String> {
    params
        .get("id")
        .and_then(Value::as_str)
        .or_else(|| params.get("taskId").and_then(Value::as_str))
        .filter(|value| !value.trim().is_empty())
        .map(str::to_owned)
}

/// The concatenated text of every `text` part of an inbound message, trimmed.
///
/// Only the `text` member is read, so 0.2 `type`-tagged parts, 0.3 `kind`-tagged parts and
/// untagged parts all work; non-text parts are ignored.
fn message_text(message: &Map<String, Value>) -> String {
    let joined = message
        .get("parts")
        .and_then(Value::as_array)
        .map(|parts| {
            parts
                .iter()
                .filter_map(|part| part.get("text").and_then(Value::as_str))
                .collect::<Vec<&str>>()
                .join("\n")
        })
        .unwrap_or_default();
    joined.trim().to_string()
}

/// A user-role message with one text part.
fn user_message(text: &str, message_id: &str) -> Value {
    json!({
        "role": "user",
        "parts": [{ "type": "text", "text": text }],
        "messageId": message_id,
    })
}

/// An agent-role message with one text part (used for `status.message`).
fn agent_message(text: &str, context_id: &str, message_id: &str) -> Value {
    json!({
        "role": "agent",
        "parts": [{ "type": "text", "text": text }],
        "messageId": message_id,
        "contextId": context_id,
    })
}

/// The current UTC time as an RFC-3339 string with millisecond precision
/// (`2024-01-02T03:04:05.678Z`), derived from [`SystemTime`].
fn now_rfc3339() -> String {
    let since_epoch = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default();
    format_rfc3339(since_epoch.as_secs(), since_epoch.subsec_millis())
}

/// Format `unix_seconds` (+ `millis`) as an RFC-3339 UTC timestamp.
fn format_rfc3339(unix_seconds: u64, millis: u32) -> String {
    let days = (unix_seconds / 86_400) as i64;
    let second_of_day = unix_seconds % 86_400;
    let (year, month, day) = civil_from_days(days);
    format!(
        "{year:04}-{month:02}-{day:02}T{:02}:{:02}:{:02}.{millis:03}Z",
        second_of_day / 3600,
        (second_of_day % 3600) / 60,
        second_of_day % 60,
    )
}

/// Gregorian civil date for a count of days since 1970-01-01 (Howard Hinnant's `civil_from_days`).
///
/// Validity: every `days` from `i64::MIN / 2` to `i64::MAX / 2` produced by
/// [`format_rfc3339`] maps to a real date; the branch-free arithmetic is exact for that range.
fn civil_from_days(days: i64) -> (i64, i64, i64) {
    let shifted = days + 719_468;
    let era = if shifted >= 0 {
        shifted
    } else {
        shifted - 146_096
    } / 146_097;
    let day_of_era = shifted - era * 146_097;
    let year_of_era =
        (day_of_era - day_of_era / 1_460 + day_of_era / 36_524 - day_of_era / 146_096) / 365;
    let year = year_of_era + era * 400;
    let day_of_year = day_of_era - (365 * year_of_era + year_of_era / 4 - year_of_era / 100);
    let month_prime = (5 * day_of_year + 2) / 153;
    let day = day_of_year - (153 * month_prime + 2) / 5 + 1;
    let month = if month_prime < 10 {
        month_prime + 3
    } else {
        month_prime - 9
    };
    (if month <= 2 { year + 1 } else { year }, month, day)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn engine() -> A2aEngine {
        A2aEngine::new()
    }

    fn send_request(engine: &A2aEngine, text: &str) -> Value {
        engine.handle_jsonrpc(&json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "message/send",
            "params": {
                "message": { "role": "user", "parts": [{ "type": "text", "text": text }] },
            },
        }))
    }

    fn get_request(engine: &A2aEngine, task_id: &str) -> Value {
        engine.handle_jsonrpc(&json!({
            "jsonrpc": "2.0",
            "id": 2,
            "method": "tasks/get",
            "params": { "id": task_id },
        }))
    }

    fn cancel_request(engine: &A2aEngine, task_id: &str) -> Value {
        engine.handle_jsonrpc(&json!({
            "jsonrpc": "2.0",
            "id": 3,
            "method": "tasks/cancel",
            "params": { "id": task_id },
        }))
    }

    fn list_request(engine: &A2aEngine) -> Value {
        engine.handle_jsonrpc(&json!({
            "jsonrpc": "2.0",
            "id": 4,
            "method": "tasks/list",
            "params": {},
        }))
    }

    fn result_task(response: &Value) -> Value {
        assert!(
            response.get("error").is_none(),
            "unexpected error: {response}"
        );
        response.get("result").cloned().unwrap_or(Value::Null)
    }

    fn error_code_of(response: &Value) -> Option<i64> {
        response
            .get("error")
            .and_then(|error| error.get("code"))
            .and_then(Value::as_i64)
    }

    /// Walk `path` (object keys and array indices) and read the leaf as a string, `""` if absent.
    fn str_at<'a>(value: &'a Value, path: &[&str]) -> &'a str {
        let mut current = value;
        for key in path {
            current = match key.parse::<usize>() {
                Ok(index) => current.get(index).unwrap_or(&Value::Null),
                Err(_) => current.get(*key).unwrap_or(&Value::Null),
            };
        }
        current.as_str().unwrap_or_default()
    }

    fn task_state(response: &Value) -> String {
        str_at(&result_task(response), &["status", "state"]).to_string()
    }

    fn send_and_id(engine: &A2aEngine, text: &str) -> String {
        let response = send_request(engine, text);
        str_at(&result_task(&response), &["id"]).to_string()
    }

    #[test]
    fn agent_card_declares_only_served_capabilities() {
        let card = engine().agent_card();
        assert_eq!(str_at(&card, &["protocolVersion"]), PROTOCOL_VERSION);
        assert_eq!(str_at(&card, &["name"]), "vikram");
        assert!(!str_at(&card, &["description"]).is_empty());
        assert_eq!(str_at(&card, &["version"]), env!("CARGO_PKG_VERSION"));
        assert_eq!(card["capabilities"]["streaming"], false);
        assert_eq!(card["capabilities"]["pushNotifications"], false);
        assert_eq!(card["defaultInputModes"], json!(["text/plain"]));
        assert_eq!(card["defaultOutputModes"], json!(["text/plain"]));
        assert_eq!(card["skills"], json!([]));
    }

    #[test]
    fn agent_card_identity_is_configurable() {
        let card = engine()
            .with_identity("peer-agent", "a test peer", "9.9.9")
            .agent_card();
        assert_eq!(str_at(&card, &["name"]), "peer-agent");
        assert_eq!(str_at(&card, &["description"]), "a test peer");
        assert_eq!(str_at(&card, &["version"]), "9.9.9");
        assert_eq!(str_at(&card, &["protocolVersion"]), PROTOCOL_VERSION);
    }

    #[test]
    fn engine_is_shareable_across_threads() {
        fn assert_send_sync<T: Send + Sync>() {}
        assert_send_sync::<A2aEngine>();
    }

    #[test]
    fn malformed_requests_are_invalid_request() {
        let engine = engine();
        let malformed = [
            json!("not an object"),
            json!([{ "jsonrpc": "2.0", "id": 1, "method": "tasks/list" }]),
            json!({ "id": 1, "method": "tasks/list" }),
            json!({ "jsonrpc": "1.0", "id": 1, "method": "tasks/list" }),
            json!({ "jsonrpc": "2.0", "method": "tasks/list" }),
            json!({ "jsonrpc": "2.0", "id": null, "method": "tasks/list" }),
            json!({ "jsonrpc": "2.0", "id": { "nested": 1 }, "method": "tasks/list" }),
            json!({ "jsonrpc": "2.0", "id": 1, "method": 7 }),
        ];
        for request in malformed {
            let response = engine.handle_jsonrpc(&request);
            assert_eq!(
                error_code_of(&response),
                Some(error_code::INVALID_REQUEST),
                "request {request} produced {response}"
            );
            assert!(response.get("result").is_none(), "no result for {request}");
        }
        // An unreadable id is echoed as null, a readable one is echoed verbatim.
        let bad_method = engine.handle_jsonrpc(&json!({
            "jsonrpc": "2.0", "id": "abc", "method": 7,
        }));
        assert_eq!(bad_method["id"], json!("abc"));
    }

    #[test]
    fn unknown_and_streaming_methods_are_method_not_found() {
        let engine = engine();
        for method in [
            "tasks/subscribe",
            "tasks/pushNotificationConfig/create",
            "nope",
        ] {
            let response = engine.handle_jsonrpc(&json!({
                "jsonrpc": "2.0", "id": 11, "method": method,
            }));
            assert_eq!(
                error_code_of(&response),
                Some(error_code::METHOD_NOT_FOUND),
                "{method}"
            );
            assert_eq!(response["id"], 11);
        }
        for method in ["message/stream", "SendStreamingMessage"] {
            let response = engine.handle_jsonrpc(&json!({
                "jsonrpc": "2.0",
                "id": 12,
                "method": method,
                "params": { "message": { "role": "user", "parts": [{ "type": "text", "text": "hi" }] } },
            }));
            assert_eq!(
                error_code_of(&response),
                Some(error_code::METHOD_NOT_FOUND),
                "{method}"
            );
            assert!(
                response["error"]["message"]
                    .as_str()
                    .is_some_and(|message| message.contains("not supported")),
                "{response}"
            );
        }
    }

    #[test]
    fn message_send_creates_a_submitted_task_with_the_message() {
        let engine = engine();
        let response = send_request(&engine, "hello agent");
        assert_eq!(response["jsonrpc"], "2.0");
        assert_eq!(response["id"], 1);

        let task = result_task(&response);
        assert_eq!(str_at(&task, &["kind"]), "task");
        assert_eq!(str_at(&task, &["status", "state"]), "submitted");
        assert!(str_at(&task, &["id"]).starts_with("task-"));
        assert!(str_at(&task, &["contextId"]).starts_with("ctx-"));
        assert!(str_at(&task, &["status", "timestamp"]).ends_with('Z'));
        assert_eq!(task["artifacts"], json!([]));
        assert_eq!(task["history"].as_array().map(Vec::len), Some(1));
        assert_eq!(str_at(&task, &["history", "0", "role"]), "user");
        assert_eq!(
            str_at(&task, &["history", "0", "parts", "0", "text"]),
            "hello agent"
        );
        assert_eq!(
            str_at(&task, &["history", "0", "parts", "0", "type"]),
            "text"
        );
        assert!(!str_at(&task, &["history", "0", "messageId"]).is_empty());
    }

    #[test]
    fn message_send_joins_all_text_parts_and_trims() {
        let engine = engine();
        let response = engine.handle_jsonrpc(&json!({
            "jsonrpc": "2.0",
            "id": 5,
            "method": "message/send",
            "params": {
                "message": {
                    "role": "user",
                    "parts": [
                        { "type": "text", "text": "  first line  " },
                        { "kind": "data", "data": { "ignored": true } },
                        { "kind": "text", "text": "second line" },
                    ],
                    "messageId": "peer-msg-1",
                },
            },
        }));
        let task = result_task(&response);
        // Parts are joined with a newline and the result is trimmed at both ends only (hermes
        // `extract_text` + `.strip()`); interior spacing of a part is left alone.
        assert_eq!(
            str_at(&task, &["history", "0", "parts", "0", "text"]),
            "first line  \nsecond line"
        );
        assert_eq!(str_at(&task, &["history", "0", "messageId"]), "peer-msg-1");
    }

    #[test]
    fn message_send_rejects_a_message_without_text() {
        let engine = engine();
        let cases = [
            json!({ "jsonrpc": "2.0", "id": 6, "method": "message/send", "params": {} }),
            json!({ "jsonrpc": "2.0", "id": 6, "method": "message/send", "params": { "message": "hi" } }),
            json!({
                "jsonrpc": "2.0", "id": 6, "method": "message/send",
                "params": { "message": { "role": "user", "parts": [] } },
            }),
            json!({
                "jsonrpc": "2.0", "id": 6, "method": "message/send",
                "params": { "message": { "role": "user", "parts": [{ "type": "text", "text": "   " }] } },
            }),
            json!({ "jsonrpc": "2.0", "id": 6, "method": "message/send", "params": "nope" }),
        ];
        for request in cases {
            let response = engine.handle_jsonrpc(&request);
            assert_eq!(
                error_code_of(&response),
                Some(error_code::INVALID_PARAMS),
                "request {request}"
            );
        }
        // A refused message leaves no task behind.
        assert_eq!(result_task(&list_request(&engine))["tasks"], json!([]));
    }

    #[test]
    fn tasks_get_returns_the_stored_task() {
        let engine = engine();
        let created = result_task(&send_request(&engine, "remember me"));
        let task_id = str_at(&created, &["id"]).to_string();

        let fetched = result_task(&get_request(&engine, &task_id));
        assert_eq!(fetched, created);
        assert_eq!(
            str_at(&fetched, &["contextId"]),
            str_at(&created, &["contextId"])
        );
    }

    #[test]
    fn tasks_get_accepts_task_id_and_reports_unknown_tasks() {
        let engine = engine();
        let task_id = send_and_id(&engine, "taskId alias");
        let by_alias = engine.handle_jsonrpc(&json!({
            "jsonrpc": "2.0", "id": 7, "method": "tasks/get", "params": { "taskId": task_id },
        }));
        assert_eq!(str_at(&result_task(&by_alias), &["id"]), task_id);

        let missing = get_request(&engine, "task-999");
        assert_eq!(
            error_code_of(&missing),
            Some(error_code::TASK_NOT_FOUND),
            "{missing}"
        );
        assert!(
            missing["error"]["message"]
                .as_str()
                .is_some_and(|message| message.contains("task not found"))
        );

        let no_id = engine.handle_jsonrpc(&json!({
            "jsonrpc": "2.0", "id": 8, "method": "tasks/get", "params": {},
        }));
        assert_eq!(error_code_of(&no_id), Some(error_code::INVALID_PARAMS));
    }

    #[test]
    fn complete_attaches_an_artifact_and_completes_the_task() {
        let engine = engine();
        let task_id = send_and_id(&engine, "what is 2 + 2?");
        assert_eq!(
            str_at(
                &result_task(&get_request(&engine, &task_id)),
                &["status", "state"]
            ),
            "submitted"
        );

        let completed = engine.complete(&task_id, "4").unwrap_or(Value::Null);
        assert_eq!(str_at(&completed, &["status", "state"]), "completed");
        assert!(completed["status"].get("message").is_none());
        assert_eq!(completed["artifacts"].as_array().map(Vec::len), Some(1));
        assert!(str_at(&completed, &["artifacts", "0", "artifactId"]).starts_with("task-"));
        assert_eq!(
            str_at(&completed, &["artifacts", "0", "parts", "0", "type"]),
            "text"
        );
        assert_eq!(
            str_at(&completed, &["artifacts", "0", "parts", "0", "text"]),
            "4"
        );
        // The original message is still the first history entry.
        assert_eq!(
            str_at(&completed, &["history", "0", "parts", "0", "text"]),
            "what is 2 + 2?"
        );

        let stored = result_task(&get_request(&engine, &task_id));
        assert_eq!(stored["artifacts"], completed["artifacts"]);
        assert_eq!(str_at(&stored, &["status", "state"]), "completed");

        // Completing twice, or completing an unknown task, changes nothing.
        assert_eq!(engine.complete(&task_id, "again"), None);
        assert_eq!(engine.complete("task-404", "text"), None);
    }

    #[test]
    fn fail_records_the_reason_on_the_status_message() {
        let engine = engine();
        let task_id = send_and_id(&engine, "do the impossible");
        let failed = engine
            .fail(&task_id, "runtime refused the turn")
            .unwrap_or(Value::Null);

        assert_eq!(str_at(&failed, &["status", "state"]), "failed");
        assert_eq!(str_at(&failed, &["status", "message", "role"]), "agent");
        assert_eq!(
            str_at(&failed, &["status", "message", "parts", "0", "text"]),
            "runtime refused the turn"
        );
        assert_eq!(failed["artifacts"], json!([]));
        assert_eq!(
            str_at(&failed, &["history", "0", "parts", "0", "text"]),
            "do the impossible"
        );

        // A failed task is terminal: no later success, no second failure.
        assert_eq!(engine.complete(&task_id, "late reply"), None);
        assert_eq!(engine.fail(&task_id, "again"), None);
        assert_eq!(engine.fail("task-404", "unknown"), None);
    }

    #[test]
    fn cancel_moves_a_pending_task_to_canceled() {
        let engine = engine();
        let task_id = send_and_id(&engine, "cancel me");
        assert!(engine.mark_working(&task_id).is_some());

        let response = cancel_request(&engine, &task_id);
        assert_eq!(task_state(&response), "canceled");
        assert_eq!(str_at(&result_task(&response), &["id"]), task_id);

        // A late host reply cannot resurrect a cancelled task.
        assert_eq!(engine.complete(&task_id, "too late"), None);
        // And a second cancel is refused with the A2A not-cancelable code.
        let again = cancel_request(&engine, &task_id);
        assert_eq!(
            error_code_of(&again),
            Some(error_code::TASK_NOT_CANCELABLE),
            "{again}"
        );
        assert_eq!(
            error_code_of(&cancel_request(&engine, "task-404")),
            Some(error_code::TASK_NOT_FOUND)
        );
    }

    #[test]
    fn cancel_of_a_completed_task_is_refused() {
        let engine = engine();
        let task_id = send_and_id(&engine, "already done");
        assert!(engine.complete(&task_id, "done").is_some());

        let response = cancel_request(&engine, &task_id);
        assert_eq!(
            error_code_of(&response),
            Some(error_code::TASK_NOT_CANCELABLE),
            "{response}"
        );
        assert!(
            response["error"]["message"]
                .as_str()
                .is_some_and(|message| message.contains("completed"))
        );
        assert_eq!(
            str_at(
                &result_task(&get_request(&engine, &task_id)),
                &["status", "state"]
            ),
            "completed"
        );
    }

    #[test]
    fn tasks_get_distinct_ids_and_contexts_and_honour_supplied_contexts() {
        let engine = engine();
        let first = result_task(&send_request(&engine, "one"));
        let second = result_task(&send_request(&engine, "two"));
        assert_ne!(str_at(&first, &["id"]), str_at(&second, &["id"]));
        assert_ne!(
            str_at(&first, &["contextId"]),
            str_at(&second, &["contextId"])
        );
        assert_eq!(
            engine.pending_context(str_at(&first, &["id"])),
            Some(str_at(&first, &["contextId"]).to_string())
        );

        // A peer that supplies a context keeps every task in that context.
        let traced = engine.handle_jsonrpc(&json!({
            "jsonrpc": "2.0",
            "id": 9,
            "method": "message/send",
            "params": {
                "message": {
                    "role": "user",
                    "contextId": "ctx-from-peer",
                    "parts": [{ "type": "text", "text": "third" }],
                },
            },
        }));
        assert_eq!(
            str_at(&result_task(&traced), &["contextId"]),
            "ctx-from-peer"
        );

        // The legacy top-level contextId is accepted too.
        let legacy = engine.handle_jsonrpc(&json!({
            "jsonrpc": "2.0",
            "id": 10,
            "method": "message/send",
            "params": {
                "contextId": "ctx-legacy",
                "message": { "role": "user", "parts": [{ "type": "text", "text": "fourth" }] },
            },
        }));
        assert_eq!(str_at(&result_task(&legacy), &["contextId"]), "ctx-legacy");

        let filtered = engine.handle_jsonrpc(&json!({
            "jsonrpc": "2.0",
            "id": 11,
            "method": "tasks/list",
            "params": { "contextId": "ctx-from-peer" },
        }));
        assert_eq!(
            result_task(&filtered)["tasks"].as_array().map(Vec::len),
            Some(1)
        );
    }

    #[test]
    fn tasks_list_returns_every_task_in_creation_order() {
        let engine = engine();
        let first = send_and_id(&engine, "first");
        let second = send_and_id(&engine, "second");
        assert!(engine.complete(&first, "one done").is_some());

        let listed = result_task(&list_request(&engine));
        let tasks = listed["tasks"].as_array().cloned().unwrap_or_default();
        assert_eq!(tasks.len(), 2);
        assert_eq!(str_at(&tasks[0], &["id"]), first);
        assert_eq!(str_at(&tasks[0], &["status", "state"]), "completed");
        assert_eq!(str_at(&tasks[1], &["id"]), second);
        assert_eq!(str_at(&tasks[1], &["status", "state"]), "submitted");

        // Both slash and v1.0 alias spellings reach the same store.
        let alias = engine.handle_jsonrpc(&json!({
            "jsonrpc": "2.0", "id": 12, "method": "ListTasks",
        }));
        assert_eq!(
            result_task(&alias)["tasks"].as_array().map(Vec::len),
            Some(2)
        );
    }

    #[test]
    fn pending_context_tracks_only_unfinished_tasks() {
        let engine = engine();
        let task_id = send_and_id(&engine, "session mapping");
        let context = engine.pending_context(&task_id);
        assert!(context.is_some_and(|context| context.starts_with("ctx-")));

        assert!(engine.mark_working(&task_id).is_some());
        assert!(engine.pending_context(&task_id).is_some());
        assert!(engine.complete(&task_id, "done").is_some());
        assert_eq!(engine.pending_context(&task_id), None);
        assert_eq!(engine.pending_context("task-404"), None);

        let canceled = send_and_id(&engine, "cancel this");
        assert!(cancel_request(&engine, &canceled)["result"].is_object());
        assert_eq!(engine.pending_context(&canceled), None);
    }

    #[test]
    fn mark_working_is_refused_for_terminal_tasks() {
        let engine = engine();
        let task_id = send_and_id(&engine, "work");
        let working = engine.mark_working(&task_id).unwrap_or(Value::Null);
        assert_eq!(str_at(&working, &["status", "state"]), "working");
        assert!(engine.fail(&task_id, "boom").is_some());
        assert_eq!(engine.mark_working(&task_id), None);
        assert_eq!(engine.mark_working("task-404"), None);
    }

    #[test]
    fn timestamps_are_rfc3339_utc() {
        assert_eq!(format_rfc3339(0, 0), "1970-01-01T00:00:00.000Z");
        assert_eq!(
            format_rfc3339(1_700_000_000, 42),
            "2023-11-14T22:13:20.042Z"
        );
        // Leap day, to prove the civil-date arithmetic rather than a day-of-year guess.
        assert_eq!(format_rfc3339(951_782_400, 0), "2000-02-29T00:00:00.000Z");
        let now = now_rfc3339();
        assert_eq!(now.len(), 24, "{now}");
        assert!(now.ends_with('Z'), "{now}");
        assert!(now.starts_with("20"), "{now}");
    }

    #[test]
    fn default_engine_matches_new() {
        let from_default = A2aEngine::default().agent_card();
        assert_eq!(from_default, engine().agent_card());
    }
}
