use std::sync::Arc;

use serde_json::{Value, json};
use vikram_contracts::{ActionKind, PermissionRequest, SideEffect, Trust, VikramError};

use crate::orchestrator::{WorkerId, WorkerStatus};
use crate::tools::*;

/// `worker.note` — leave a note in a sibling worker's mailbox. The mailbox is the recipient's
/// durable worker row, so the note survives a restart and the recipient reads it exactly once.
pub(crate) struct WorkerNote;

impl Tool for WorkerNote {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "worker.note".into(),
            description: "Leave a note for another worker; it is read once by that worker.".into(),
            schema: json!({
                "type": "object",
                "properties": {
                    "from": {"type": "string", "description": "the authoring worker id; omit to use the calling worker's own id"},
                    "to": {"type": "string", "description": "the recipient worker id"},
                    "text": {"type": "string", "description": "what the recipient needs to know; a pointer to work, not a report"}
                },
                "required": ["to", "text"]
            }),
        }
    }

    fn permission(&self, args: &Value) -> Result<PermissionRequest, VikramError> {
        let to = args
            .get("to")
            .and_then(Value::as_str)
            .unwrap_or("worker")
            .to_owned();
        Ok(PermissionRequest::new(
            ActionKind::Delegate,
            "worker.note",
            to,
            SideEffect::LocalMutation,
            Trust::Model,
        ))
    }

    fn run(&self, args: &Value, ctx: &ToolContext) -> Result<ToolOutput, VikramError> {
        let to = WorkerId::new(arg_str(args, "to")?);
        let text = arg_str(args, "text")?;
        let Some(runtime) = ctx.runtime.as_ref() else {
            return Ok(ToolOutput {
                content: "worker.note needs a live runtime".into(),
                ok: false,
            });
        };
        let from = match args.get("from").and_then(Value::as_str) {
            Some(from) => WorkerId::new(from),
            None => {
                let Some(session) = ctx.session.as_ref() else {
                    return Ok(ToolOutput {
                        content: "worker.note needs `from`: this call does not come from a worker's session".into(),
                        ok: false,
                    });
                };
                let Some(record) = runtime.workers.by_session(session) else {
                    return Ok(ToolOutput {
                        content: format!(
                            "worker.note needs `from`: session {session} is not a spawned worker"
                        ),
                        ok: false,
                    });
                };
                record.id
            }
        };
        match runtime.workers.post_note(&from, &to, &text) {
            Ok(note) => Ok(ToolOutput {
                content: format!(
                    "note `{}` -> `{}` queued at {}ms",
                    note.from_worker, note.to_worker, note.at_ms
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

/// `worker.notes` — drain the mailbox of one worker. Reading is what empties it: a note comes
/// back once, and a second drain is empty rather than a repeat.
pub(crate) struct WorkerNotes;

impl Tool for WorkerNotes {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "worker.notes".into(),
            description: "Take the notes addressed to a worker; taking them empties the mailbox."
                .into(),
            schema: json!({
                "type": "object",
                "properties": {
                    "worker": {"type": "string", "description": "the worker whose mailbox to drain"}
                },
                "required": ["worker"]
            }),
        }
    }

    fn permission(&self, args: &Value) -> Result<PermissionRequest, VikramError> {
        let worker = args
            .get("worker")
            .and_then(Value::as_str)
            .unwrap_or("worker")
            .to_owned();
        Ok(PermissionRequest::new(
            ActionKind::Delegate,
            "worker.notes",
            worker,
            SideEffect::None,
            Trust::Model,
        ))
    }

    fn run(&self, args: &Value, ctx: &ToolContext) -> Result<ToolOutput, VikramError> {
        let worker = WorkerId::new(arg_str(args, "worker")?);
        let Some(runtime) = ctx.runtime.as_ref() else {
            return Ok(ToolOutput {
                content: "worker.notes needs a live runtime".into(),
                ok: false,
            });
        };
        let notes = match runtime.workers.take_notes(&worker) {
            Ok(notes) => notes,
            Err(err) => {
                return Ok(ToolOutput {
                    content: err.to_string(),
                    ok: false,
                });
            }
        };
        if notes.is_empty() {
            return Ok(ToolOutput {
                content: format!("no notes for `{worker}`"),
                ok: true,
            });
        }
        let content = notes
            .iter()
            .map(|note| {
                format!(
                    "from `{}` at {}ms: {}",
                    note.from_worker, note.at_ms, note.text
                )
            })
            .collect::<Vec<_>>()
            .join("\n");
        Ok(ToolOutput {
            content: truncated(content),
            ok: true,
        })
    }
}

/// `worker.resume` — put a partial worker back to work. A worker that hit its turn budget keeps
/// its session and transcript, so resuming continues from where it stopped rather than starting
/// the brief again. Only a partial worker has work left to continue.
#[allow(
    dead_code,
    reason = "registered in tools/core.rs; the registry line is the owner's, so the lib build reaches the struct only once it lands"
)]
pub(crate) struct WorkerResume;

impl Tool for WorkerResume {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "worker.resume".into(),
            description:
                "Resume a partial worker from where it stopped; its session and brief are kept."
                    .into(),
            schema: json!({
                "type": "object",
                "properties": {
                    "worker": {"type": "string", "description": "the id of the partial worker to resume"}
                },
                "required": ["worker"],
                "additionalProperties": false
            }),
        }
    }

    fn permission(&self, args: &Value) -> Result<PermissionRequest, VikramError> {
        let worker = args
            .get("worker")
            .and_then(Value::as_str)
            .unwrap_or("worker")
            .to_owned();
        Ok(PermissionRequest::new(
            ActionKind::Delegate,
            "worker.resume",
            worker,
            SideEffect::LocalMutation,
            Trust::Model,
        ))
    }

    fn run(&self, args: &Value, ctx: &ToolContext) -> Result<ToolOutput, VikramError> {
        let worker = WorkerId::new(arg_str(args, "worker")?);
        let Some(runtime) = ctx.runtime.as_ref() else {
            return Ok(ToolOutput {
                content: "worker.resume needs a live runtime".into(),
                ok: false,
            });
        };
        let Some(record) = runtime.workers.by_id(&worker) else {
            let resumable = runtime
                .workers
                .list()
                .into_iter()
                .filter(|record| record.status == WorkerStatus::Partial)
                .map(|record| format!("`{}`", record.id))
                .collect::<Vec<_>>();
            let list = if resumable.is_empty() {
                "no worker is resumable right now".to_owned()
            } else {
                format!("resumable workers: {}", resumable.join(", "))
            };
            return Ok(ToolOutput {
                content: format!("no worker named `{worker}`; {list}"),
                ok: false,
            });
        };
        if record.status != WorkerStatus::Partial {
            return Ok(ToolOutput {
                content: format!(
                    "worker `{worker}` is {}; only a partial worker can be resumed",
                    record.status.as_str()
                ),
                ok: false,
            });
        }
        let session = record.session.clone();
        let handle = crate::runtime::RuntimeHandle {
            inner: Arc::clone(runtime),
        };
        match handle.resume_worker(&worker) {
            Ok(()) => Ok(ToolOutput {
                content: format!(
                    "worker `{worker}` in session `{session}` resumed: a continuation turn is running under the same brief"
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
    use crate::orchestrator::WorkerRecord;
    use crate::runtime::Runtime;
    use vikram_contracts::SessionId;
    use vikram_policy::CapabilityScope;

    fn isolated_home(dir: &std::path::Path) -> std::sync::MutexGuard<'static, ()> {
        let guard = crate::config::lock_security_env();
        unsafe { std::env::set_var("VIKRAM_HOME", dir.join("home")) };
        guard
    }

    async fn runtime_with_workers(
        dir: &std::path::Path,
        workers: &[(&str, &str)],
    ) -> crate::runtime::RuntimeHandle {
        let config = crate::config::Config {
            model: "test-model".into(),
            workspace_root: Some(dir.to_path_buf()),
            ..crate::config::Config::default()
        };
        let runtime = Runtime::start(config)
            .await
            .expect("a runtime with no provider still boots");
        let handle = runtime.handle();
        for (id, session) in workers {
            handle.inner.workers.insert(worker(id, session));
        }
        handle
    }

    fn worker(id: &str, session: &str) -> WorkerRecord {
        WorkerRecord {
            id: WorkerId::new(id),
            brief: crate::orchestrator::WorkerBrief {
                objective: "read the spill report".into(),
                output_format: "a plain paragraph".into(),
                tool_allowlist: vec!["worker.notes".into()],
                file_scope: vec![],
                boundary: "never edit a file".into(),
                model: None,
                effort: None,
            },
            scope: CapabilityScope::default(),
            session: SessionId::new(session),
            parent: SessionId::new("ses-parent"),
            depth: 1,
            status: WorkerStatus::Running,
            attempts: 1,
            max_turns: None,
            artifact: None,
            notes: Vec::new(),
        }
    }

    fn ctx(handle: &crate::runtime::RuntimeHandle, session: Option<&str>) -> ToolContext {
        ToolContext {
            runtime: Some(std::sync::Arc::clone(&handle.inner)),
            session: session.map(SessionId::new),
            ..ToolContext::default()
        }
    }

    #[tokio::test]
    async fn a_note_posted_by_one_worker_is_read_once_by_another() {
        let dir = tempfile::tempdir().unwrap();
        let _home = isolated_home(dir.path());
        let handle =
            runtime_with_workers(dir.path(), &[("wrk-a", "ses-a"), ("wrk-b", "ses-b")]).await;

        let posted = WorkerNote
            .run(
                &json!({"to": "wrk-b", "text": "the spill report is in artifacts/"}),
                &ctx(&handle, Some("ses-a")),
            )
            .unwrap();
        assert!(posted.ok, "{}", posted.content);
        assert!(
            posted.content.contains("`wrk-a` -> `wrk-b`"),
            "{}",
            posted.content
        );

        let taken = WorkerNotes
            .run(&json!({"worker": "wrk-b"}), &ctx(&handle, None))
            .unwrap();
        assert!(taken.ok, "{}", taken.content);
        assert!(
            taken.content.contains("from `wrk-a` at ")
                && taken.content.contains("the spill report is in artifacts/"),
            "{}",
            taken.content
        );

        let again = WorkerNotes
            .run(&json!({"worker": "wrk-b"}), &ctx(&handle, None))
            .unwrap();
        assert!(again.ok, "{}", again.content);
        assert_eq!(again.content, "no notes for `wrk-b`");
    }

    #[tokio::test]
    async fn a_note_to_a_worker_that_does_not_exist_is_refused() {
        let dir = tempfile::tempdir().unwrap();
        let _home = isolated_home(dir.path());
        let handle = runtime_with_workers(dir.path(), &[("wrk-a", "ses-a")]).await;

        let refused = WorkerNote
            .run(
                &json!({"from": "wrk-a", "to": "wrk-ghost", "text": "anyone there?"}),
                &ctx(&handle, None),
            )
            .unwrap();
        assert!(!refused.ok, "{}", refused.content);
        assert!(
            refused.content.contains("no worker named `wrk-ghost`"),
            "{}",
            refused.content
        );
    }

    #[tokio::test]
    async fn a_partial_worker_can_be_resumed_through_the_tool() {
        let dir = tempfile::tempdir().unwrap();
        let _home = isolated_home(dir.path());
        let handle = runtime_with_workers(dir.path(), &[]).await;
        let session = handle.snapshot().sessions[0].id.clone();
        let mut record = worker("wrk-partial", session.as_str());
        record.status = WorkerStatus::Partial;
        handle.inner.workers.insert(record);

        let resumed = WorkerResume
            .run(&json!({"worker": "wrk-partial"}), &ctx(&handle, None))
            .unwrap();
        assert!(resumed.ok, "{}", resumed.content);
        assert!(
            resumed.content.contains("`wrk-partial`")
                && resumed.content.contains(&format!("`{session}`"))
                && resumed.content.contains("resumed"),
            "{}",
            resumed.content
        );
        assert_eq!(
            handle
                .inner
                .workers
                .by_id(&WorkerId::new("wrk-partial"))
                .unwrap()
                .status,
            WorkerStatus::Running,
            "a resumed worker is running again, not left partial"
        );
    }

    #[tokio::test]
    async fn resuming_a_finished_worker_is_refused_with_its_status() {
        let dir = tempfile::tempdir().unwrap();
        let _home = isolated_home(dir.path());
        let handle = runtime_with_workers(dir.path(), &[("wrk-done", "ses-done")]).await;
        handle
            .inner
            .workers
            .complete(&SessionId::new("ses-done"), "artifact-1".into());

        let refused = WorkerResume
            .run(&json!({"worker": "wrk-done"}), &ctx(&handle, None))
            .unwrap();
        assert!(!refused.ok, "{}", refused.content);
        assert!(
            refused.content.contains("`wrk-done` is done")
                && refused
                    .content
                    .contains("only a partial worker can be resumed"),
            "{}",
            refused.content
        );
    }

    #[tokio::test]
    async fn an_unknown_worker_lists_the_ones_that_can_be_resumed() {
        let dir = tempfile::tempdir().unwrap();
        let _home = isolated_home(dir.path());
        let handle = runtime_with_workers(dir.path(), &[("wrk-busy", "ses-busy")]).await;
        let mut partial = worker("wrk-partial", "ses-partial");
        partial.status = WorkerStatus::Partial;
        handle.inner.workers.insert(partial);

        let refused = WorkerResume
            .run(&json!({"worker": "wrk-ghost"}), &ctx(&handle, None))
            .unwrap();
        assert!(!refused.ok, "{}", refused.content);
        assert!(
            refused.content.contains("no worker named `wrk-ghost`")
                && refused.content.contains("resumable workers:")
                && refused.content.contains("`wrk-partial`"),
            "{}",
            refused.content
        );
        assert!(
            !refused.content.contains("`wrk-busy`"),
            "a running worker is not offered as resumable: {}",
            refused.content
        );
    }

    #[test]
    fn the_tools_are_registered_and_advertise_their_schemas() {
        let registry = ToolRegistry::with_builtins();

        let post = registry
            .get("worker.note")
            .expect("worker.note is registered");
        let spec = post.spec();
        assert_eq!(spec.name, "worker.note");
        let required: Vec<&str> = spec.schema["required"]
            .as_array()
            .unwrap()
            .iter()
            .map(|value| value.as_str().unwrap())
            .collect();
        assert_eq!(required, vec!["to", "text"]);
        for property in ["from", "to", "text"] {
            assert_eq!(
                spec.schema["properties"][property]["type"], "string",
                "`{property}` is a string"
            );
        }

        let drain = registry
            .get("worker.notes")
            .expect("worker.notes is registered");
        let spec = drain.spec();
        assert_eq!(spec.name, "worker.notes");
        assert_eq!(spec.schema["required"][0], "worker");
        assert_eq!(spec.schema["properties"]["worker"]["type"], "string");

        let permission = post
            .permission(&json!({"to": "wrk-b", "text": "x"}))
            .unwrap();
        assert_eq!(permission.action, ActionKind::Delegate);
        assert_eq!(permission.side_effect, SideEffect::LocalMutation);
        let permission = drain.permission(&json!({"worker": "wrk-b"})).unwrap();
        assert_eq!(permission.action, ActionKind::Delegate);
        assert_eq!(permission.side_effect, SideEffect::None);
    }
}
