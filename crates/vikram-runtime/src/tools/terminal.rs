use serde_json::{Value, json};
use std::collections::BTreeMap;
use vikram_contracts::{ActionKind, PermissionRequest, SideEffect, ToolCall, Trust, VikramError};
use vikram_terminals::{DockerBackend, ExecResult, SshBackend, TerminalRegistry, TerminalSpec};

use crate::tools::*;
use crate::types::ApprovalPreview;

const DEFAULT_TIMEOUT_MS: u64 = 30_000;
const MIN_TIMEOUT_MS: u64 = 1_000;
const MAX_TIMEOUT_MS: u64 = 120_000;

/// A resolved backend and the target it needs. `image` belongs to docker and `host`/`user` to ssh;
/// exactly one pair is populated, so a call can never build a backend from another's fields.
struct Target {
    backend: String,
    image: Option<String>,
    host: Option<String>,
    user: Option<String>,
}

fn field(args: &Value, key: &str) -> Result<String, VikramError> {
    let value = arg_str(args, key)?;
    if value.trim().is_empty() {
        return Err(VikramError::ProviderRejected(format!(
            "`{key}` must not be empty"
        )));
    }
    Ok(value)
}

fn endpoint(value: &str, key: &str, extra: &[char]) -> Result<(), VikramError> {
    let valid = !value.starts_with('-')
        && value
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || extra.contains(&c));
    if valid {
        Ok(())
    } else {
        Err(VikramError::ProviderRejected(format!(
            "`{key}` is not a valid target: `{value}`"
        )))
    }
}

fn target_from(args: &Value) -> Result<Target, VikramError> {
    let backend = field(args, "backend")?;
    match backend.as_str() {
        "docker" => {
            let image = field(args, "image")?;
            endpoint(&image, "image", &['.', '_', '-', ':', '/', '@'])?;
            Ok(Target {
                backend,
                image: Some(image),
                host: None,
                user: None,
            })
        }
        "ssh" => {
            let host = field(args, "host")?;
            endpoint(&host, "host", &['.', '_', '-', ':', '_', '[', ']'])?;
            let user = match args.get("user").and_then(Value::as_str) {
                Some(raw) => {
                    let user = raw.trim();
                    if user.is_empty() {
                        return Err(VikramError::ProviderRejected(
                            "`user` must not be empty".into(),
                        ));
                    }
                    endpoint(user, "user", &['.', '_', '-'])?;
                    Some(user.to_owned())
                }
                None => None,
            };
            Ok(Target {
                backend,
                image: None,
                host: Some(host),
                user,
            })
        }
        other => Err(VikramError::ProviderRejected(format!(
            "unknown backend `{other}`; expected `docker` or `ssh`"
        ))),
    }
}

fn registry_for(target: &Target) -> TerminalRegistry {
    let mut registry = TerminalRegistry::new();
    match (&target.image, &target.host) {
        (Some(image), _) => registry.register(Box::new(DockerBackend::new(image.clone()))),
        (_, Some(host)) => {
            registry.register(Box::new(SshBackend::new(host.clone(), target.user.clone())))
        }
        _ => {}
    }
    registry
}

fn block_on<F: std::future::Future>(future: F) -> Result<F::Output, VikramError> {
    match tokio::runtime::Handle::try_current() {
        Ok(handle) => Ok(handle.block_on(future)),
        Err(_) => {
            let runtime = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .map_err(|e| VikramError::EffectFailed(e.to_string()))?;
            Ok(runtime.block_on(future))
        }
    }
}

fn execute(target: &Target, command: &str, timeout_ms: u64) -> Result<ExecResult, String> {
    let registry = registry_for(target);
    let backend = registry.get(&target.backend).ok_or_else(|| {
        format!(
            "`{}` backend is not configured; pass `image` for docker or `host` for ssh",
            target.backend
        )
    })?;
    let id = format!("{}-exec", target.backend);
    let spec = TerminalSpec {
        id: id.clone(),
        backend: target.backend.clone(),
        cwd: None,
        env: BTreeMap::new(),
        cols: 80,
        rows: 24,
    };
    block_on(backend.create(spec))
        .map_err(|e| format!("terminal {} backend: {e}", target.backend))?
        .map_err(|e| format!("terminal {} backend: {e}", target.backend))?;
    block_on(backend.exec(&id, command, timeout_ms))
        .map_err(|e| format!("terminal {} backend: {e}", target.backend))?
        .map_err(|e| format!("terminal {} backend: {e}", target.backend))
}

fn render(result: &ExecResult) -> String {
    let mut body = result.stdout.clone();
    let stderr = &result.stderr;
    if !stderr.trim().is_empty() {
        if !body.is_empty() && !body.ends_with('\n') {
            body.push('\n');
        }
        body.push_str("[stderr]\n");
        body.push_str(stderr);
    }
    if body.trim().is_empty() {
        body = "(no output)".to_owned();
    }
    body.push_str(&format!("\n[exit {}]", result.exit_code.unwrap_or(-1)));
    body
}

pub(crate) struct TerminalExec;

impl Tool for TerminalExec {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "terminal.exec".into(),
            description:
                "Run one command in a container or on a remote host through the configured terminal backend."
                    .into(),
            schema: json!({
                "type": "object",
                "properties": {
                    "backend": {
                        "type": "string",
                        "enum": ["docker", "ssh"],
                        "description": "where the command runs"
                    },
                    "command": {
                        "type": "string",
                        "description": "shell command to run on that backend"
                    },
                    "image": {
                        "type": "string",
                        "description": "container image; required when backend is docker"
                    },
                    "host": {
                        "type": "string",
                        "description": "remote host; required when backend is ssh"
                    },
                    "user": {
                        "type": "string",
                        "description": "ssh user; optional, defaults to ssh's own user"
                    },
                    "timeout_ms": {
                        "type": "integer",
                        "description": "wall-clock limit for the command"
                    }
                },
                "required": ["backend", "command"]
            }),
        }
    }

    fn permission(&self, args: &Value) -> Result<PermissionRequest, VikramError> {
        let _target = target_from(args)?;
        let command = field(args, "command")?;
        let side_effect = if looks_destructive(&command) {
            SideEffect::Destructive
        } else {
            SideEffect::ExternalChange
        };
        Ok(PermissionRequest::new(
            ActionKind::Execute,
            "terminal.exec",
            command,
            side_effect,
            Trust::Model,
        ))
    }

    fn preview(&self, call: &ToolCall, _ctx: &ToolContext) -> Option<ApprovalPreview> {
        let args = &call.arguments;
        let backend = args.get("backend")?.as_str()?;
        let command = args.get("command")?.as_str()?;
        let target = match backend {
            "docker" => format!("image {}", args.get("image")?.as_str()?),
            "ssh" => match args.get("user").and_then(Value::as_str) {
                Some(user) => format!("{user}@{}", args.get("host")?.as_str()?),
                None => format!("host {}", args.get("host")?.as_str()?),
            },
            _ => backend.to_owned(),
        };
        Some(ApprovalPreview::Body {
            title: format!("terminal.exec ({backend}: {target})"),
            text: format!("$ {command}"),
        })
    }

    fn run(&self, args: &Value, _ctx: &ToolContext) -> Result<ToolOutput, VikramError> {
        let target = match target_from(args) {
            Ok(target) => target,
            Err(error) => {
                return Ok(ToolOutput {
                    content: error.to_string(),
                    ok: false,
                });
            }
        };
        let command = match field(args, "command") {
            Ok(command) => command,
            Err(error) => {
                return Ok(ToolOutput {
                    content: error.to_string(),
                    ok: false,
                });
            }
        };
        let timeout_ms = args
            .get("timeout_ms")
            .and_then(Value::as_u64)
            .unwrap_or(DEFAULT_TIMEOUT_MS)
            .clamp(MIN_TIMEOUT_MS, MAX_TIMEOUT_MS);
        let result = execute(&target, &command, timeout_ms);
        Ok(match result {
            Ok(result) => ToolOutput {
                content: truncated(render(&result)),
                ok: result.exit_code == Some(0),
            },
            Err(error) => ToolOutput {
                content: truncated(error),
                ok: false,
            },
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use vikram_contracts::Decision;
    use vikram_policy::{MandatoryFloor, Policy, ResourcePattern, Rule};

    fn run(args: Value) -> ToolOutput {
        TerminalExec.run(&args, &ToolContext::default()).unwrap()
    }

    #[test]
    fn unknown_backend_is_a_clean_error() {
        let args = json!({"backend": "telnet", "command": "id"});
        let error = TerminalExec.permission(&args).unwrap_err();
        assert!(
            matches!(error, VikramError::ProviderRejected(ref message) if message.contains("unknown backend")),
            "{error:?}"
        );
        let out = run(args);
        assert!(!out.ok);
        assert!(out.content.contains("unknown backend"), "{}", out.content);
    }

    #[test]
    fn ssh_without_a_host_is_a_clean_error() {
        let args = json!({"backend": "ssh", "command": "id"});
        let error = TerminalExec.permission(&args).unwrap_err();
        assert!(
            matches!(error, VikramError::ProviderRejected(ref message) if message.contains("host")),
            "{error:?}"
        );
        let out = run(args);
        assert!(!out.ok);
        assert!(out.content.contains("`host`"), "{}", out.content);
    }

    #[test]
    fn docker_without_an_image_is_a_clean_error() {
        let args = json!({"backend": "docker", "command": "id"});
        let error = TerminalExec.permission(&args).unwrap_err();
        assert!(
            matches!(error, VikramError::ProviderRejected(ref message) if message.contains("image")),
            "{error:?}"
        );
        let out = run(args);
        assert!(!out.ok);
        assert!(out.content.contains("`image`"), "{}", out.content);
    }

    #[test]
    fn a_target_cannot_smuggle_an_ssh_option() {
        let args = json!({"backend": "ssh", "host": "-oProxyCommand=evil", "command": "id"});
        assert!(TerminalExec.permission(&args).is_err());
        let out = run(args);
        assert!(!out.ok);
        assert!(
            out.content.contains("not a valid target"),
            "{}",
            out.content
        );
    }

    #[test]
    fn the_schema_states_what_each_backend_needs() {
        let spec = TerminalExec.spec();
        assert_eq!(spec.name, "terminal.exec");
        assert_eq!(spec.schema["required"], json!(["backend", "command"]));
        assert_eq!(
            spec.schema["properties"]["backend"]["enum"],
            json!(["docker", "ssh"])
        );
        let image = spec.schema["properties"]["image"]["description"]
            .as_str()
            .expect("image description");
        assert!(image.contains("docker"), "{image}");
        let host = spec.schema["properties"]["host"]["description"]
            .as_str()
            .expect("host description");
        assert!(host.contains("ssh"), "{host}");
        assert!(spec.schema["properties"]["user"].is_object());
        assert!(spec.schema["properties"]["timeout_ms"].is_object());
    }

    #[test]
    fn the_tool_goes_through_the_process_execution_permission() {
        let request = TerminalExec
            .permission(&json!({
                "backend": "docker",
                "image": "ubuntu:22.04",
                "command": "id"
            }))
            .unwrap();
        assert_eq!(request.action, ActionKind::Execute);
        assert_eq!(request.verb, "terminal.exec");
        assert_eq!(request.resource, "id");
        assert_eq!(request.side_effect, SideEffect::ExternalChange);
        let destructive = TerminalExec
            .permission(&json!({
                "backend": "ssh",
                "host": "example.com",
                "command": "rm -rf /"
            }))
            .unwrap();
        assert_eq!(destructive.side_effect, SideEffect::Destructive);
    }

    #[test]
    fn a_policy_that_denies_process_execution_refuses_the_call() {
        let request = TerminalExec
            .permission(&json!({
                "backend": "ssh",
                "host": "example.com",
                "command": "id"
            }))
            .unwrap();

        assert!(
            matches!(Policy::empty().evaluate(&request), Decision::Deny { .. }),
            "no rule permits process execution"
        );

        let allowed = Policy::new(
            vec![Rule::allow(ActionKind::Execute, ResourcePattern::Any)],
            MandatoryFloor::default(),
        );
        assert!(allowed.evaluate(&request).is_allowed());

        let denied = Policy::new(
            vec![
                Rule::allow(ActionKind::Execute, ResourcePattern::Any),
                Rule::deny(
                    ActionKind::Execute,
                    ResourcePattern::Any,
                    "remote commands are refused here",
                ),
            ],
            MandatoryFloor::default(),
        );
        assert!(matches!(
            denied.evaluate(&request),
            Decision::Deny { reason } if reason.contains("remote commands")
        ));
    }
}
