//! The scheduler, reachable from a conversation.
//!
//! Until this module existed the daemon could only be driven from the shell, so "remind me in
//! thirty minutes" was a sentence the agent had no way to act on. Every tool here calls the same
//! function the CLI calls, so a job created by talking and a job created by typing cannot drift
//! apart, and nothing is claimed to be scheduled that is not in the store.
//!
//! Registering the family is the host's job (`install`, called once at startup), because the
//! runtime must not depend on this crate: the dependency already runs the other way.

use serde_json::{Value, json};
use vikram_contracts::{ActionKind, PermissionRequest, SideEffect, Trust, VikramError};
use vikram_runtime::tools::{Tool, ToolContext, ToolOutput, ToolRegistry, ToolSpec};

use crate::cli;
use crate::store::JobMode;

/// Register the scheduler tools into a registry the runtime is building.
pub fn register(registry: &ToolRegistry) {
    for tool in family() {
        registry.register(tool);
    }
}

/// Install the family for every registry this process builds afterwards.
pub fn install() {
    vikram_runtime::tools::install_extension(register);
}

fn family() -> Vec<Box<dyn Tool>> {
    vec![
        Box::new(Simple::new(
            "schedule.list",
            "List scheduled jobs with their schedule, next run and last result.",
            json!({"type": "object", "properties": {}}),
            ActionKind::Read,
            SideEffect::None,
            |_| cli::cron_list(),
        )),
        Box::new(Simple::new(
            "schedule.show",
            "Show one scheduled job, its configuration and its most recent runs.",
            json!({
                "type": "object",
                "properties": {"id": {"type": "string", "description": "job id or name"}},
                "required": ["id"]
            }),
            ActionKind::Read,
            SideEffect::None,
            |args| cli::cron_show(&need(args, "id")?),
        )),
        Box::new(Simple::new(
            "schedule.add",
            "Schedule work: a prompt for an agent, a shell script, or a delegated task. Schedule \
             accepts `every 10m`, `in 30m`, `at 2026-11-04T09:00`, `@daily`, or a five-field cron \
             expression.",
            json!({
                "type": "object",
                "properties": {
                    "name": {"type": "string", "description": "short name the operator will say"},
                    "schedule": {"type": "string", "description": "every 10m | in 30m | at <time> | @daily | cron"},
                    "payload": {"type": "string", "description": "the prompt, the script, or the command"},
                    "mode": {"type": "string", "description": "agent (default), script, workflow or delegate"},
                    "target": {"type": "string", "description": "local (default), session:<id>, webhook:<url>"},
                    "timezone": {"type": "string", "description": "local (default), UTC, or +HH:MM"},
                    "description": {"type": "string"}
                },
                "required": ["name", "schedule", "payload"]
            }),
            ActionKind::Write,
            SideEffect::LocalMutation,
            |args| {
                let name = need(args, "name")?;
                let schedule = need(args, "schedule")?;
                let payload = need(args, "payload")?;
                let mut draft = cli::Draft {
                    payload: Some(payload),
                    enabled: Some(true),
                    ..Default::default()
                };
                if let Some(mode) = text(args, "mode") {
                    draft.mode = Some(JobMode::parse(&mode).ok_or_else(|| {
                        format!("unknown mode `{mode}`; choose agent, script, workflow or delegate")
                    })?);
                }
                draft.target = text(args, "target");
                draft.timezone = text(args, "timezone");
                draft.description = text(args, "description");
                cli::cron_add(&name, &schedule, &draft)
            },
        )),
        Box::new(Simple::new(
            "schedule.pause",
            "Stop a scheduled job from running until it is resumed. Its history is kept.",
            json!({"type": "object", "properties": {"id": {"type": "string"}}, "required": ["id"]}),
            ActionKind::Write,
            SideEffect::None,
            |args| cli::cron_pause(&need(args, "id")?),
        )),
        Box::new(Simple::new(
            "schedule.resume",
            "Resume a paused job. It runs on its next slot rather than replaying what it missed.",
            json!({"type": "object", "properties": {"id": {"type": "string"}}, "required": ["id"]}),
            ActionKind::Write,
            SideEffect::None,
            |args| cli::cron_resume(&need(args, "id")?),
        )),
        Box::new(Simple::new(
            "schedule.run",
            "Run a scheduled job now instead of waiting for its next slot.",
            json!({"type": "object", "properties": {"id": {"type": "string"}}, "required": ["id"]}),
            ActionKind::Execute,
            SideEffect::LocalMutation,
            |args| cli::cron_run(&need(args, "id")?),
        )),
        Box::new(Simple::new(
            "schedule.remove",
            "Delete a scheduled job and its run history. Other jobs are untouched.",
            json!({"type": "object", "properties": {"id": {"type": "string"}}, "required": ["id"]}),
            ActionKind::Write,
            SideEffect::Destructive,
            |args| cli::cron_remove(&need(args, "id")?),
        )),
        Box::new(Simple::new(
            "schedule.history",
            "What a scheduled job has done: each run, its status and its result.",
            json!({
                "type": "object",
                "properties": {
                    "id": {"type": "string"},
                    "limit": {"type": "integer", "description": "how many runs, default 10"}
                },
                "required": ["id"]
            }),
            ActionKind::Read,
            SideEffect::None,
            |args| {
                let limit = args.get("limit").and_then(Value::as_u64).unwrap_or(10) as usize;
                cli::cron_history(&need(args, "id")?, limit)
            },
        )),
        Box::new(Simple::new(
            "daemon.status",
            "Whether the background runtime is running, what it holds, and how the heartbeat stands.",
            json!({"type": "object", "properties": {}}),
            ActionKind::Read,
            SideEffect::None,
            |_| cli::status(),
        )),
        Box::new(Simple::new(
            "daemon.heartbeat",
            "The three heartbeats — runtime, agent, workers — with interval, last beat and what needs attention.",
            json!({"type": "object", "properties": {}}),
            ActionKind::Read,
            SideEffect::None,
            |_| cli::heartbeat_status(),
        )),
        Box::new(Simple::new(
            "jobs.list",
            "Recent background work: what ran, whether it worked, and what it said.",
            json!({"type": "object", "properties": {"limit": {"type": "integer"}}}),
            ActionKind::Read,
            SideEffect::None,
            |args| {
                let limit = args.get("limit").and_then(Value::as_u64).unwrap_or(20) as usize;
                cli::jobs_list(limit)
            },
        )),
        Box::new(Simple::new(
            "jobs.inspect",
            "One background run in full: its job, timing, output, error and where the result went.",
            json!({"type": "object", "properties": {"id": {"type": "string"}}, "required": ["id"]}),
            ActionKind::Read,
            SideEffect::None,
            |args| cli::jobs_inspect(&need(args, "id")?),
        )),
        Box::new(Simple::new(
            "jobs.cancel",
            "Ask for a running background job to stop. A run that has already finished is left alone.",
            json!({"type": "object", "properties": {"id": {"type": "string"}}, "required": ["id"]}),
            ActionKind::Execute,
            SideEffect::LocalMutation,
            |args| cli::jobs_cancel(&need(args, "id")?),
        )),
    ]
}

fn text(args: &Value, name: &str) -> Option<String> {
    args.get(name)
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(str::to_owned)
}

fn need(args: &Value, name: &str) -> Result<String, String> {
    text(args, name).ok_or_else(|| format!("{name} is required"))
}

/// One tool with no state: a spec, the permission shape of the call, and the function that does
/// the work. The family is thirteen of these, and writing them out by hand would have buried the
/// one thing that differs.
struct Simple {
    spec: ToolSpec,
    action: ActionKind,
    effect: SideEffect,
    call: fn(&Value) -> Result<String, String>,
}

impl Simple {
    fn new(
        name: &str,
        description: &str,
        schema: Value,
        action: ActionKind,
        effect: SideEffect,
        call: fn(&Value) -> Result<String, String>,
    ) -> Self {
        Self {
            spec: ToolSpec {
                name: name.to_owned(),
                description: description.to_owned(),
                schema,
            },
            action,
            effect,
            call,
        }
    }
}

impl Tool for Simple {
    fn spec(&self) -> ToolSpec {
        self.spec.clone()
    }

    fn permission(&self, args: &Value) -> Result<PermissionRequest, VikramError> {
        // The resource names the job the call is about, so a rule can allow `schedule.show` for
        // everything and `schedule.remove` for nothing without knowing the tool's internals.
        let resource = text(args, "id")
            .or_else(|| text(args, "name"))
            .unwrap_or_else(|| "schedule".to_owned());
        Ok(PermissionRequest::new(
            self.action,
            self.spec.name.clone(),
            resource,
            self.effect,
            Trust::Model,
        ))
    }

    fn run(&self, args: &Value, _ctx: &ToolContext) -> Result<ToolOutput, VikramError> {
        Ok(match (self.call)(args) {
            Ok(content) => ToolOutput { content, ok: true },
            Err(error) => ToolOutput {
                content: error,
                ok: false,
            },
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fixture() -> tempfile::TempDir {
        let dir = tempfile::tempdir().expect("tempdir");
        // The whole crate reads its home from the environment, so a test that schedules anything
        // has to own it. `unsafe` because this is process-wide and the tests run in parallel —
        // only the one test that writes touches it.
        unsafe { std::env::set_var("VIKRAM_HOME", dir.path()) };
        dir
    }

    fn run(name: &str, args: Value) -> ToolOutput {
        let registry = ToolRegistry::with_builtins();
        let tool = registry
            .get(name)
            .unwrap_or_else(|| panic!("{name} is registered"));
        tool.run(
            &args,
            &vikram_runtime::tools::ToolContext::new(
                std::env::temp_dir(),
                &vikram_runtime::config::SandboxSettings::default(),
            ),
        )
        .expect("tool runs")
    }

    #[test]
    fn the_family_is_registered_once_and_every_name_is_there() {
        install();
        install();
        let registry = ToolRegistry::with_builtins();
        for name in [
            "schedule.list",
            "schedule.show",
            "schedule.add",
            "schedule.pause",
            "schedule.resume",
            "schedule.run",
            "schedule.remove",
            "schedule.history",
            "daemon.status",
            "daemon.heartbeat",
            "jobs.list",
            "jobs.inspect",
            "jobs.cancel",
        ] {
            assert!(registry.get(name).is_some(), "{name} is missing");
        }
        assert_eq!(
            registry
                .names()
                .iter()
                .filter(|name| name.as_str() == "schedule.list")
                .count(),
            1
        );
    }

    #[test]
    fn scheduling_by_talking_creates_a_job_the_cli_can_see() {
        let _home = fixture();
        install();
        let added = run(
            "schedule.add",
            json!({
                "name": "server check",
                "schedule": "every 10m",
                "payload": "curl -s localhost:8080/health",
                "mode": "script"
            }),
        );
        assert!(added.ok, "{}", added.content);
        assert!(added.content.contains("server-check"), "{}", added.content);
        let listed = run("schedule.list", json!({}));
        assert!(listed.ok, "{}", listed.content);
        assert!(
            listed.content.contains("server-check"),
            "{}",
            listed.content
        );
    }

    #[test]
    fn a_bad_argument_is_refused_instead_of_guessed() {
        let _home = fixture();
        install();
        let refused = run(
            "schedule.add",
            json!({"name": "x", "schedule": "every 10m", "payload": "true", "mode": "nonsense"}),
        );
        assert!(!refused.ok);
        assert!(
            refused.content.contains("unknown mode"),
            "{}",
            refused.content
        );
        let missing = run("schedule.show", json!({}));
        assert!(!missing.ok);
        assert!(missing.content.contains("id is required"));
    }

    #[test]
    fn reading_the_state_of_nothing_is_an_answer_not_an_error() {
        let _home = fixture();
        install();
        for (name, args) in [
            ("schedule.list", json!({})),
            ("jobs.list", json!({})),
            ("daemon.status", json!({})),
            ("daemon.heartbeat", json!({})),
        ] {
            let out = run(name, args);
            assert!(out.ok, "{name} failed: {}", out.content);
            assert!(!out.content.trim().is_empty(), "{name} said nothing");
        }
    }
}
