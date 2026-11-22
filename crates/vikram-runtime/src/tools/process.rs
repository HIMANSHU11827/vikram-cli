use serde_json::{Value, json};
use std::path::PathBuf;
use vikram_contracts::{ActionKind, PermissionRequest, SideEffect, Trust, VikramError};

use crate::tools::*;
pub(crate) struct Process;

impl Tool for Process {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "process".into(),
            description: "List or kill system processes.".into(),
            schema: json!({
                "type": "object",
                "properties": {
                    "action": {"type": "string", "enum": ["list", "kill"]},
                    "pid": {"type": "integer"}
                },
                "required": ["action"]
            }),
        }
    }

    fn permission(&self, args: &Value) -> Result<PermissionRequest, VikramError> {
        // The resource is what the approval prompt and the journal show, so a kill names the pid
        // it would terminate rather than the bare word "process".
        let action = args.get("action").and_then(Value::as_str).unwrap_or("list");
        let resource = match (action, args.get("pid").and_then(Value::as_i64)) {
            ("kill", Some(pid)) if pid > 0 => format!("process kill pid={pid}"),
            ("kill", Some(pid)) => {
                return Err(VikramError::ProviderRejected(format!(
                    "`pid` must be a positive integer, got {pid}"
                )));
            }
            ("kill", None) => "process kill".to_string(),
            _ => format!("process {action}"),
        };
        if action == "kill"
            && let Some(pid) = args.get("pid").and_then(Value::as_i64)
            && kills_own_tree(pid)
        {
            return Err(VikramError::ProviderRejected(format!(
                "refusing to kill pid {pid}: it is the runtime's own process or an ancestor"
            )));
        }
        // Killing an arbitrary pid — including the runtime's own tree — is not a routine local
        // mutation, so `Smart` must not auto-allow it.
        let side_effect = if action == "kill" {
            SideEffect::Destructive
        } else {
            SideEffect::LocalMutation
        };
        Ok(PermissionRequest::new(
            ActionKind::Execute,
            "process.run",
            resource,
            side_effect,
            Trust::Model,
        ))
    }

    fn run(&self, args: &Value, _ctx: &ToolContext) -> Result<ToolOutput, VikramError> {
        let action = args.get("action").and_then(Value::as_str).unwrap_or("list");
        match action {
            "list" => {
                #[cfg(windows)]
                let mut cmd = {
                    let mut c = std::process::Command::new("tasklist");
                    c
                };
                #[cfg(not(windows))]
                let mut cmd = {
                    let mut c = std::process::Command::new("ps");
                    c.arg("aux");
                    c
                };
                let output = cmd
                    .output()
                    .map_err(|e| VikramError::EffectFailed(e.to_string()))?;
                let mut body = String::from_utf8_lossy(&output.stdout).to_string();
                let stderr = String::from_utf8_lossy(&output.stderr);
                if !stderr.trim().is_empty() {
                    if !body.is_empty() && !body.ends_with('\n') {
                        body.push('\n');
                    }
                    body.push_str("[stderr]\n");
                    body.push_str(&stderr);
                }
                if body.trim().is_empty() {
                    body = format!("(no output, exit {:?})", output.status.code());
                }
                Ok(ToolOutput {
                    content: truncated(body),
                    ok: output.status.success(),
                })
            }
            "kill" => {
                let pid = args.get("pid").and_then(Value::as_i64).ok_or_else(|| {
                    VikramError::ProviderRejected("missing integer argument `pid`".into())
                })?;
                // pid 0 (and negatives) address a process group or nothing at all, never one
                // process the operator could have meant to approve.
                if pid <= 0 {
                    return Err(VikramError::ProviderRejected(format!(
                        "`pid` must be a positive integer, got {pid}"
                    )));
                }
                if kills_own_tree(pid) {
                    return Err(VikramError::ProviderRejected(format!(
                        "refusing to kill pid {pid}: it is the runtime's own process or an ancestor"
                    )));
                }
                #[cfg(windows)]
                let mut cmd = {
                    let mut c = std::process::Command::new("taskkill");
                    c.args(["/PID", &pid.to_string()]);
                    c
                };
                #[cfg(not(windows))]
                let mut cmd = {
                    let mut c = std::process::Command::new("kill");
                    c.arg(pid.to_string());
                    c
                };
                let output = cmd
                    .output()
                    .map_err(|e| VikramError::EffectFailed(e.to_string()))?;
                let mut body = String::from_utf8_lossy(&output.stdout).to_string();
                let stderr = String::from_utf8_lossy(&output.stderr);
                if !stderr.trim().is_empty() {
                    if !body.is_empty() && !body.ends_with('\n') {
                        body.push('\n');
                    }
                    body.push_str("[stderr]\n");
                    body.push_str(&stderr);
                }
                if body.trim().is_empty() {
                    body = format!("(no output, exit {:?})", output.status.code());
                }
                Ok(ToolOutput {
                    content: truncated(body),
                    ok: output.status.success(),
                })
            }
            other => Ok(ToolOutput {
                content: format!("unknown action `{other}`; expected list or kill"),
                ok: false,
            }),
        }
    }
}

/// True when `pid` is the runtime itself, its process group, or an ancestor — the pids a
/// `kill` must never reach even with approval, because terminating them takes the agent down
/// with the target. Ancestors are walked through `/proc` where it exists (Linux); elsewhere
/// only the self pid and process group are known.
#[cfg(unix)]
fn kills_own_tree(pid: i64) -> bool {
    let own = std::process::id() as i64;
    if pid == own {
        return true;
    }
    // SAFETY: getpgrp has no failure mode that matters here; -1 just never matches a pid.
    let pgid = unsafe { libc::getpgrp() };
    if pgid > 0 && pid == pgid as i64 {
        return true;
    }
    let mut current = own;
    for _ in 0..64 {
        let status = match std::fs::read_to_string(format!("/proc/{current}/status")) {
            Ok(status) => status,
            Err(_) => return false,
        };
        let parent = status
            .lines()
            .find_map(|line| line.strip_prefix("PPid:"))
            .and_then(|value| value.trim().parse::<i64>().ok());
        match parent {
            Some(parent) if parent > 1 => {
                if pid == parent {
                    return true;
                }
                current = parent;
            }
            _ => return false,
        }
    }
    false
}

#[cfg(not(unix))]
fn kills_own_tree(pid: i64) -> bool {
    pid == std::process::id() as i64
}

// Task storage helpers
fn tasks_path() -> PathBuf {
    crate::config::vikram_home().join("tasks.json")
}

fn load_tasks() -> Vec<Value> {
    let path = tasks_path();
    std::fs::read_to_string(&path)
        .ok()
        .and_then(|raw| serde_json::from_str::<Value>(&raw).ok())
        .and_then(|v| v.as_array().cloned())
        .unwrap_or_default()
}

fn save_tasks(tasks: &[Value]) -> Result<(), VikramError> {
    let path = tasks_path();
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|e| VikramError::Storage(e.to_string()))?;
    }
    let body =
        serde_json::to_string_pretty(tasks).map_err(|e| VikramError::Storage(e.to_string()))?;
    std::fs::write(&path, body).map_err(|e| VikramError::Storage(e.to_string()))?;
    Ok(())
}

pub(crate) struct Task;

impl Tool for Task {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "task".into(),
            description: "Create, list, or update tasks persisted in $VIKRAM_HOME/tasks.json."
                .into(),
            schema: json!({
                "type": "object",
                "properties": {
                    "action": {"type": "string", "enum": ["create", "list", "update"]},
                    "title": {"type": "string"},
                    "description": {"type": "string"},
                    "id": {"type": "integer"},
                    "status": {"type": "string", "enum": ["pending", "active", "completed", "failed"]}
                },
                "required": ["action"]
            }),
        }
    }

    fn permission(&self, _args: &Value) -> Result<PermissionRequest, VikramError> {
        Ok(PermissionRequest::new(
            ActionKind::Write,
            "task.manage",
            "tasks",
            SideEffect::LocalMutation,
            Trust::Model,
        ))
    }

    fn run(&self, args: &Value, _ctx: &ToolContext) -> Result<ToolOutput, VikramError> {
        let action = args.get("action").and_then(Value::as_str).unwrap_or("list");
        match action {
            "create" => {
                let title = arg_str(args, "title")?;
                let description = args
                    .get("description")
                    .and_then(Value::as_str)
                    .unwrap_or("")
                    .to_string();
                let mut tasks = load_tasks();
                let next_id = tasks
                    .iter()
                    .filter_map(|t| t.get("id").and_then(Value::as_u64))
                    .max()
                    .unwrap_or(0)
                    + 1;
                let task = json!({
                    "id": next_id,
                    "title": title,
                    "description": description,
                    "status": "pending"
                });
                tasks.push(task);
                save_tasks(&tasks)?;
                Ok(ToolOutput {
                    content: format!("created task {next_id}"),
                    ok: true,
                })
            }
            "list" => {
                let tasks = load_tasks();
                if tasks.is_empty() {
                    return Ok(ToolOutput {
                        content: "no tasks".into(),
                        ok: true,
                    });
                }
                let body = tasks
                    .iter()
                    .map(|t| {
                        let id = t.get("id").and_then(Value::as_u64).unwrap_or(0);
                        let status = t.get("status").and_then(Value::as_str).unwrap_or("pending");
                        let title = t.get("title").and_then(Value::as_str).unwrap_or("");
                        let desc = t.get("description").and_then(Value::as_str).unwrap_or("");
                        if desc.is_empty() {
                            format!("{id} [{status}] {title}")
                        } else {
                            format!("{id} [{status}] {title} - {desc}")
                        }
                    })
                    .collect::<Vec<_>>()
                    .join("\n");
                Ok(ToolOutput {
                    content: truncated(body),
                    ok: true,
                })
            }
            "update" => {
                let id = args.get("id").and_then(Value::as_u64).ok_or_else(|| {
                    VikramError::ProviderRejected("missing integer argument `id`".into())
                })?;
                let status = arg_str(args, "status")?;
                let mut tasks = load_tasks();
                let mut found = false;
                for t in tasks.iter_mut() {
                    if t.get("id").and_then(Value::as_u64) == Some(id) {
                        t["status"] = json!(status);
                        found = true;
                    }
                }
                if !found {
                    return Ok(ToolOutput {
                        content: format!("task not found: {id}"),
                        ok: false,
                    });
                }
                save_tasks(&tasks)?;
                Ok(ToolOutput {
                    content: format!("updated task {id} -> {status}"),
                    ok: true,
                })
            }
            other => Ok(ToolOutput {
                content: format!("unknown action `{other}`; expected create, list, or update"),
                ok: false,
            }),
        }
    }
}

/// One step of a session's plan, as the model wrote it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PlanStep {
    pub text: String,
    pub status: PlanStatus,
}

/// Where a step stands. Three states, because a checklist with more is a project tracker and this
/// is what the agent is doing right now.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PlanStatus {
    Pending,
    InProgress,
    Done,
}

impl PlanStatus {
    fn parse(raw: &str) -> Option<Self> {
        match raw {
            "pending" => Some(Self::Pending),
            "in_progress" => Some(Self::InProgress),
            "done" => Some(Self::Done),
            _ => None,
        }
    }

    /// The word the model writes, which is the word an interface reads back.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Pending => "pending",
            Self::InProgress => "in_progress",
            Self::Done => "done",
        }
    }

    fn mark(self) -> &'static str {
        match self {
            Self::Pending => "[ ]",
            Self::InProgress => "[>]",
            Self::Done => "[x]",
        }
    }
}

/// The live plan for one session. It replaces the previous plan wholesale on every write: a plan
/// that could only be appended to would grow a second copy of itself every time the agent changed
/// its mind, which is the failure mode of a task list.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Plan {
    pub steps: Vec<PlanStep>,
    /// 1 for the first write, then one per replacement. A UI can use it to tell a re-render from a
    /// real change.
    pub revision: u64,
}

impl Plan {
    /// The checklist as text: what the model is handed back, and what an interface draws.
    pub fn render(&self) -> String {
        let done = self
            .steps
            .iter()
            .filter(|step| step.status == PlanStatus::Done)
            .count();
        let mut out = format!("plan: {} steps ({done} done)\n", self.steps.len());
        for step in &self.steps {
            out.push_str(step.status.mark());
            out.push(' ');
            out.push_str(&step.text);
            out.push('\n');
        }
        out.pop();
        out
    }
}

/// Plans are per session and live for the life of the process. They are deliberately not the
/// global `tasks.json`: a task is something the operator keeps, a plan is what the agent is doing
/// in this conversation, and merging the two would make every session's checklist one list.
fn plans() -> &'static std::sync::Mutex<std::collections::HashMap<String, Plan>> {
    static PLANS: std::sync::OnceLock<std::sync::Mutex<std::collections::HashMap<String, Plan>>> =
        std::sync::OnceLock::new();
    PLANS.get_or_init(Default::default)
}

/// The plan the model last wrote for `session`, if it wrote one. This is the accessor an interface
/// renders: the checklist is state, not a message, so it does not have to be re-read from the
/// transcript.
pub fn plan_for(session: &vikram_contracts::SessionId) -> Option<Plan> {
    plans()
        .lock()
        .expect("plans lock")
        .get(session.as_str())
        .cloned()
}

/// Every live plan, in session order, for an interface that lists more than one session.
pub fn all_plans() -> Vec<(String, Plan)> {
    let mut out: Vec<(String, Plan)> = plans()
        .lock()
        .expect("plans lock")
        .iter()
        .map(|(session, plan)| (session.clone(), plan.clone()))
        .collect();
    out.sort_by(|a, b| a.0.cmp(&b.0));
    out
}

/// A step count beyond this is a tracker, not a plan, and would spend the context it is meant to
/// save. Refused rather than silently clipped: a clipped plan lies about what is left.
const MAX_PLAN_STEPS: usize = 50;

/// `update_plan` — the checklist the operator can watch. It replaces the session's plan and hands
/// the rendered result back, so the model sees the same list the human does.
pub(crate) struct UpdatePlan;

impl Tool for UpdatePlan {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "update_plan".into(),
            description: "Replace this session's plan with an ordered checklist. Send every step \
                          each time: the list you send is the whole plan, and the previous one is \
                          dropped. Use it for work that takes more than a step, and mark steps \
                          done as you finish them."
                .into(),
            schema: json!({
                "type": "object",
                "properties": {
                    "steps": {
                        "type": "array",
                        "description": "the whole plan, in order",
                        "items": {
                            "type": "object",
                            "properties": {
                                "text": {"type": "string", "description": "one step, in the imperative"},
                                "status": {
                                    "type": "string",
                                    "enum": ["pending", "in_progress", "done"]
                                }
                            },
                            "required": ["text", "status"]
                        }
                    }
                },
                "required": ["steps"]
            }),
        }
    }

    fn permission(&self, _args: &Value) -> Result<PermissionRequest, VikramError> {
        // Session state, not a file or a process: `SideEffect::None` is what keeps a plan update
        // from stopping a turn for an approval nobody needs to give.
        Ok(PermissionRequest::new(
            ActionKind::Write,
            "session.plan",
            "plan",
            SideEffect::None,
            Trust::Model,
        ))
    }

    fn run(&self, args: &Value, ctx: &ToolContext) -> Result<ToolOutput, VikramError> {
        let raw = args
            .get("steps")
            .ok_or_else(|| VikramError::ProviderRejected("missing array argument `steps`".into()))?
            .as_array()
            .ok_or_else(|| {
                VikramError::ProviderRejected("argument `steps` must be an array of steps".into())
            })?;
        if raw.len() > MAX_PLAN_STEPS {
            return Err(VikramError::ProviderRejected(format!(
                "a plan of {} steps is too long; the limit is {MAX_PLAN_STEPS}. Send the next few \
                 steps, not the whole project.",
                raw.len()
            )));
        }
        let mut steps = Vec::with_capacity(raw.len());
        for (index, item) in raw.iter().enumerate() {
            let text = item
                .get("text")
                .and_then(Value::as_str)
                .map(str::trim)
                .filter(|text| !text.is_empty())
                .ok_or_else(|| {
                    VikramError::ProviderRejected(format!(
                        "step {index} has no `text`: every step needs one imperative line"
                    ))
                })?;
            let status = item.get("status").and_then(Value::as_str).ok_or_else(|| {
                VikramError::ProviderRejected(format!("step {index} has no `status`"))
            })?;
            let status = PlanStatus::parse(status).ok_or_else(|| {
                VikramError::ProviderRejected(format!(
                    "step {index} has status `{status}`; the only statuses are `pending`, \
                     `in_progress` and `done`"
                ))
            })?;
            steps.push(PlanStep {
                text: text.to_owned(),
                status,
            });
        }
        let key = ctx
            .session
            .as_ref()
            .map(|session| session.as_str().to_owned())
            .unwrap_or_default();
        let plan = {
            let mut plans = plans().lock().expect("plans lock");
            let revision = plans.get(&key).map_or(1, |plan| plan.revision + 1);
            let plan = Plan { steps, revision };
            plans.insert(key, plan.clone());
            plan
        };
        // A plan the operator cannot see is a plan nobody can hold the agent to. The event carries
        // only what a view draws, and only a call made from a session publishes it: a direct tool
        // call has no interface to update and no session to file it under.
        if let (Some(runtime), Some(session)) = (ctx.runtime.as_ref(), ctx.session.as_ref()) {
            runtime.emit(crate::types::RuntimeEvent::PlanUpdated {
                session: session.clone(),
                plan: plan_view(&plan),
            });
        }
        Ok(ToolOutput {
            content: plan.render(),
            ok: true,
        })
    }
}

/// The plan in the shape an event carries: titles and status words, no tool types.
fn plan_view(plan: &Plan) -> crate::types::PlanView {
    crate::types::PlanView {
        steps: plan
            .steps
            .iter()
            .map(|step| crate::types::PlanStepView {
                title: step.text.clone(),
                status: step.status.as_str().to_owned(),
            })
            .collect(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use vikram_contracts::SessionId;

    fn ctx(session: &SessionId) -> ToolContext {
        ToolContext {
            session: Some(session.clone()),
            ..ToolContext::default()
        }
    }

    fn steps(value: Value) -> Value {
        json!({ "steps": value })
    }

    #[test]
    fn update_plan_replaces_the_plan_rather_than_appending_to_it() {
        let session = SessionId::generate();
        let first = UpdatePlan
            .run(
                &steps(json!([
                    {"text": "read the failing test", "status": "in_progress"},
                    {"text": "fix it", "status": "pending"},
                    {"text": "run the suite", "status": "pending"},
                ])),
                &ctx(&session),
            )
            .expect("the first plan is accepted");
        assert!(first.ok, "{}", first.content);
        assert_eq!(plan_for(&session).expect("the plan is kept").steps.len(), 3);

        let second = UpdatePlan
            .run(
                &steps(json!([
                    {"text": "read the failing test", "status": "done"},
                    {"text": "fix it", "status": "in_progress"},
                ])),
                &ctx(&session),
            )
            .expect("the second plan is accepted");
        assert!(second.ok, "{}", second.content);

        let plan = plan_for(&session).expect("the plan is kept");
        assert_eq!(
            plan.steps.len(),
            2,
            "the second plan is the whole plan: {plan:?}"
        );
        assert_eq!(plan.steps[1].text, "fix it");
        assert_eq!(plan.steps[1].status, PlanStatus::InProgress);
        assert_eq!(plan.revision, 2, "a replacement is a new revision");
        assert!(
            second.content.contains("[>] fix it"),
            "the model is handed the same checklist the operator sees: {}",
            second.content
        );
        assert!(
            !second.content.contains("run the suite"),
            "a dropped step does not linger in the rendered plan: {}",
            second.content
        );
    }

    #[test]
    fn update_plan_validates_every_status() {
        let session = SessionId::generate();
        let out = UpdatePlan
            .run(
                &steps(json!([
                    {"text": "a", "status": "pending"},
                    {"text": "b", "status": "in_progress"},
                    {"text": "c", "status": "done"},
                ])),
                &ctx(&session),
            )
            .expect("all three statuses are legal");
        let plan = plan_for(&session).expect("the plan is kept");
        assert_eq!(
            plan.steps.iter().map(|s| s.status).collect::<Vec<_>>(),
            vec![
                PlanStatus::Pending,
                PlanStatus::InProgress,
                PlanStatus::Done
            ]
        );
        assert!(out.content.contains("(1 done)"), "{}", out.content);
    }

    #[test]
    fn update_plan_rejects_an_unknown_status() {
        let session = SessionId::generate();
        let err = UpdatePlan
            .run(
                &steps(json!([{"text": "fix it", "status": "doing"}])),
                &ctx(&session),
            )
            .expect_err("an unknown status is refused, not defaulted");
        let text = err.to_string();
        assert!(text.contains("doing"), "{text}");
        assert!(text.contains("in_progress"), "{text}");
        assert!(
            plan_for(&session).is_none(),
            "a refused plan does not replace the last good one"
        );
    }

    #[test]
    fn update_plan_rejects_a_step_without_text_and_an_oversized_plan() {
        let session = SessionId::generate();
        assert!(
            UpdatePlan
                .run(
                    &steps(json!([{"text": "  ", "status": "pending"}])),
                    &ctx(&session)
                )
                .is_err(),
            "an empty step is not a step"
        );
        let too_many: Vec<Value> = (0..MAX_PLAN_STEPS + 1)
            .map(|n| json!({"text": format!("step {n}"), "status": "pending"}))
            .collect();
        assert!(
            UpdatePlan
                .run(&steps(Value::Array(too_many)), &ctx(&session))
                .is_err(),
            "a plan longer than the limit is refused, never clipped"
        );
    }

    /// `VIKRAM_HOME` is process-global, so a test that points the home at a temporary directory
    /// holds the shared guard for its whole body.
    fn isolated_home(dir: &std::path::Path) -> std::sync::MutexGuard<'static, ()> {
        let guard = crate::config::lock_security_env();
        unsafe { std::env::set_var("VIKRAM_HOME", dir.join("home")) };
        guard
    }

    #[tokio::test]
    // The home guard must outlive the boot: releasing it early would let a parallel test repoint
    // `VIKRAM_HOME` at its own directory mid-startup. Same reason the runtime's own tests allow it.
    #[allow(clippy::await_holding_lock)]
    async fn update_plan_publishes_the_plan_to_the_interface() {
        let dir = tempfile::tempdir().expect("tempdir");
        let _home = isolated_home(dir.path());
        let config = crate::config::Config {
            model: "test-model".into(),
            workspace_root: Some(dir.path().to_path_buf()),
            ..crate::config::Config::default()
        };
        let runtime = crate::runtime::Runtime::start(config)
            .await
            .expect("a runtime with no provider still boots");
        let handle = runtime.handle();
        let session = handle.snapshot().sessions[0].id.clone();
        let mut events = handle.events();
        let ctx = ToolContext {
            runtime: Some(std::sync::Arc::clone(&handle.inner)),
            session: Some(session.clone()),
            ..ToolContext::default()
        };

        UpdatePlan
            .run(
                &steps(json!([
                    {"text": "read the failing test", "status": "done"},
                    {"text": "fix it", "status": "in_progress"},
                ])),
                &ctx,
            )
            .expect("the plan is accepted");

        let plan = tokio::time::timeout(std::time::Duration::from_secs(5), async {
            loop {
                if let crate::types::RuntimeEvent::PlanUpdated { session: sid, plan } =
                    events.recv().await.expect("the bus stays open")
                    && sid == session
                {
                    return plan;
                }
            }
        })
        .await
        .expect("the plan event arrives");

        assert_eq!(plan.steps.len(), 2);
        assert_eq!(plan.steps[0].title, "read the failing test");
        assert_eq!(plan.steps[0].status, "done");
        assert_eq!(plan.steps[1].title, "fix it");
        assert_eq!(plan.steps[1].status, "in_progress");
        assert_eq!(plan.done(), 1);
        assert_eq!(plan.current(), Some("fix it"));
    }

    #[test]
    fn a_plan_update_never_needs_an_approval() {
        let request = UpdatePlan
            .permission(&steps(json!([])))
            .expect("the permission shape is valid");
        assert_eq!(
            request.side_effect,
            SideEffect::None,
            "session state, so no posture has to stop for it"
        );
    }
}
