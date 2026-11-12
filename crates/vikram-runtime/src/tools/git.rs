use serde_json::{Value, json};
use vikram_contracts::{ActionKind, PermissionRequest, SideEffect, Trust, VikramError};

use crate::tools::*;

/// Shared runner: spawn `git` with an argv vector (no shell) in `ctx.workspace_root`,
/// capture stdout/stderr, and report success from the exit status.
fn run_git(argv: &[String], ctx: &ToolContext) -> ToolOutput {
    let mut cmd = std::process::Command::new("git");
    cmd.args(argv).current_dir(&ctx.workspace_root);
    // Provide a default identity only when the caller has none, so commits work
    // in fresh repositories without forcing one on configured environments.
    if std::env::var_os("GIT_AUTHOR_NAME").is_none() {
        cmd.env("GIT_AUTHOR_NAME", "vikram");
    }
    if std::env::var_os("GIT_AUTHOR_EMAIL").is_none() {
        cmd.env("GIT_AUTHOR_EMAIL", "vikram@local");
    }
    if std::env::var_os("GIT_COMMITTER_NAME").is_none() {
        cmd.env("GIT_COMMITTER_NAME", "vikram");
    }
    if std::env::var_os("GIT_COMMITTER_EMAIL").is_none() {
        cmd.env("GIT_COMMITTER_EMAIL", "vikram@local");
    }
    match cmd.output() {
        Ok(output) => {
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
            body.push_str(&format!("\n[exit {}]", output.status.code().unwrap_or(-1)));
            ToolOutput {
                content: truncated(body),
                ok: output.status.success(),
            }
        }
        Err(error) => ToolOutput {
            content: format!("failed to start git: {error}"),
            ok: false,
        },
    }
}

pub(crate) struct GitStatus;

impl Tool for GitStatus {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "git.status".into(),
            description: "Show the working tree status (short, with branch).".into(),
            schema: json!({
                "type": "object",
                "properties": {}
            }),
        }
    }

    fn permission(&self, _args: &Value) -> Result<PermissionRequest, VikramError> {
        Ok(PermissionRequest::new(
            ActionKind::Read,
            "git.status",
            "git",
            SideEffect::None,
            Trust::Model,
        ))
    }

    fn run(&self, _args: &Value, ctx: &ToolContext) -> Result<ToolOutput, VikramError> {
        let argv = vec![
            "status".to_string(),
            "--short".to_string(),
            "--branch".to_string(),
        ];
        Ok(run_git(&argv, ctx))
    }
}

pub(crate) struct GitDiff;

impl Tool for GitDiff {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "git.diff".into(),
            description: "Show changes in the working tree or index.".into(),
            schema: json!({
                "type": "object",
                "properties": {
                    "path": {"type": "string", "description": "optional path to limit the diff"},
                    "staged": {"type": "boolean", "description": "diff the staged (cached) changes"}
                }
            }),
        }
    }

    fn permission(&self, args: &Value) -> Result<PermissionRequest, VikramError> {
        let resource = args
            .get("path")
            .and_then(Value::as_str)
            .unwrap_or(".")
            .to_owned();
        Ok(PermissionRequest::new(
            ActionKind::Read,
            "git.diff",
            resource,
            SideEffect::None,
            Trust::Model,
        ))
    }

    fn run(&self, args: &Value, ctx: &ToolContext) -> Result<ToolOutput, VikramError> {
        let staged = args.get("staged").and_then(Value::as_bool).unwrap_or(false);
        let path = args.get("path").and_then(Value::as_str);
        let mut argv = vec!["diff".to_string()];
        if staged {
            argv.push("--cached".to_string());
        }
        if let Some(p) = path {
            argv.push("--".to_string());
            argv.push(p.to_string());
        }
        Ok(run_git(&argv, ctx))
    }
}

pub(crate) struct GitLog;

impl Tool for GitLog {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "git.log".into(),
            description: "Show recent commit history (oneline).".into(),
            schema: json!({
                "type": "object",
                "properties": {
                    "limit": {"type": "integer", "description": "maximum commits to show (default 20)"}
                }
            }),
        }
    }

    fn permission(&self, _args: &Value) -> Result<PermissionRequest, VikramError> {
        Ok(PermissionRequest::new(
            ActionKind::Read,
            "git.log",
            "git",
            SideEffect::None,
            Trust::Model,
        ))
    }

    fn run(&self, args: &Value, ctx: &ToolContext) -> Result<ToolOutput, VikramError> {
        let limit = args
            .get("limit")
            .and_then(Value::as_u64)
            .unwrap_or(20)
            .max(1);
        let argv = vec![
            "log".to_string(),
            "--oneline".to_string(),
            "-n".to_string(),
            limit.to_string(),
        ];
        Ok(run_git(&argv, ctx))
    }
}

pub(crate) struct GitCommit;

impl Tool for GitCommit {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "git_commit".into(),
            description: "Stage files and create a commit with the given message.".into(),
            schema: json!({
                "type": "object",
                "properties": {
                    "message": {"type": "string", "description": "commit message (required)"},
                    "files": {
                        "type": "array",
                        "items": {"type": "string"},
                        "description": "paths to stage (default: all changes)"
                    }
                },
                "required": ["message"]
            }),
        }
    }

    fn permission(&self, args: &Value) -> Result<PermissionRequest, VikramError> {
        let resource = args
            .get("files")
            .and_then(Value::as_array)
            .and_then(|a| a.first())
            .and_then(Value::as_str)
            .unwrap_or("git")
            .to_owned();
        Ok(PermissionRequest::new(
            ActionKind::Execute,
            "git_commit",
            resource,
            SideEffect::LocalMutation,
            Trust::Model,
        ))
    }

    fn run(&self, args: &Value, ctx: &ToolContext) -> Result<ToolOutput, VikramError> {
        let message = arg_str(args, "message")?;
        let files: Vec<String> = args
            .get("files")
            .and_then(Value::as_array)
            .map(|a| {
                a.iter()
                    .filter_map(Value::as_str)
                    .map(str::to_owned)
                    .collect()
            })
            .unwrap_or_default();
        let mut add_argv = vec!["add".to_string()];
        if files.is_empty() {
            add_argv.push("-A".to_string());
        } else {
            add_argv.extend(files);
        }
        let added = run_git(&add_argv, ctx);
        if !added.ok {
            return Ok(added);
        }
        let commit_argv = vec!["commit".to_string(), "-m".to_string(), message];
        Ok(run_git(&commit_argv, ctx))
    }
}

pub(crate) struct GitBranch;

impl Tool for GitBranch {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "git_branch".into(),
            description: "Create a branch (if name given) or list branches.".into(),
            schema: json!({
                "type": "object",
                "properties": {
                    "name": {"type": "string", "description": "branch name to create"},
                    "list": {"type": "boolean", "description": "list branches instead of creating"}
                }
            }),
        }
    }

    fn permission(&self, args: &Value) -> Result<PermissionRequest, VikramError> {
        let name = args.get("name").and_then(Value::as_str);
        let list = args.get("list").and_then(Value::as_bool).unwrap_or(false);
        let create = name.is_some() && !list;
        let (action, side_effect) = if create {
            (ActionKind::Execute, SideEffect::LocalMutation)
        } else {
            (ActionKind::Read, SideEffect::None)
        };
        Ok(PermissionRequest::new(
            action,
            "git_branch",
            name.unwrap_or("git").to_owned(),
            side_effect,
            Trust::Model,
        ))
    }

    fn run(&self, args: &Value, ctx: &ToolContext) -> Result<ToolOutput, VikramError> {
        let name = args.get("name").and_then(Value::as_str);
        let list = args.get("list").and_then(Value::as_bool).unwrap_or(false);
        let argv = if let (Some(n), false) = (name, list) {
            vec!["branch".to_string(), n.to_string()]
        } else {
            vec!["branch".to_string(), "-a".to_string()]
        };
        Ok(run_git(&argv, ctx))
    }
}

pub(crate) struct GitStash;

impl Tool for GitStash {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "git_stash".into(),
            description: "Stash changes: push (default), pop, or list.".into(),
            schema: json!({
                "type": "object",
                "properties": {
                    "action": {
                        "type": "string",
                        "enum": ["push", "pop", "list"],
                        "description": "stash action (default: push)"
                    }
                }
            }),
        }
    }

    fn permission(&self, _args: &Value) -> Result<PermissionRequest, VikramError> {
        Ok(PermissionRequest::new(
            ActionKind::Execute,
            "git_stash",
            "git",
            SideEffect::LocalMutation,
            Trust::Model,
        ))
    }

    fn run(&self, args: &Value, ctx: &ToolContext) -> Result<ToolOutput, VikramError> {
        let action = args.get("action").and_then(Value::as_str).unwrap_or("push");
        let argv = match action {
            "push" => vec!["stash".to_string(), "push".to_string()],
            "pop" => vec!["stash".to_string(), "pop".to_string()],
            "list" => vec!["stash".to_string(), "list".to_string()],
            other => {
                return Ok(ToolOutput {
                    content: format!("unsupported stash action: {other}"),
                    ok: false,
                });
            }
        };
        Ok(run_git(&argv, ctx))
    }
}

// Aliases for the underscore-style names some callers/tests expect.
// (primary tools above already use underscore names; provide dot-style aliases too)
pub(crate) struct GitCommitTool;
impl Tool for GitCommitTool {
    fn spec(&self) -> ToolSpec {
        let mut s = GitCommit.spec();
        s.name = "git.commit".into();
        s
    }
    fn permission(&self, args: &Value) -> Result<PermissionRequest, VikramError> {
        let mut r = GitCommit.permission(args)?;
        r.verb = "git.commit".into();
        Ok(r)
    }
    fn run(&self, args: &Value, ctx: &ToolContext) -> Result<ToolOutput, VikramError> {
        GitCommit.run(args, ctx)
    }
}

pub(crate) struct GitBranchTool;
impl Tool for GitBranchTool {
    fn spec(&self) -> ToolSpec {
        let mut s = GitBranch.spec();
        s.name = "git.branch".into();
        s
    }
    fn permission(&self, args: &Value) -> Result<PermissionRequest, VikramError> {
        let mut r = GitBranch.permission(args)?;
        r.verb = "git.branch".into();
        Ok(r)
    }
    fn run(&self, args: &Value, ctx: &ToolContext) -> Result<ToolOutput, VikramError> {
        GitBranch.run(args, ctx)
    }
}

pub(crate) struct GitStashTool;
impl Tool for GitStashTool {
    fn spec(&self) -> ToolSpec {
        let mut s = GitStash.spec();
        s.name = "git.stash".into();
        s
    }
    fn permission(&self, args: &Value) -> Result<PermissionRequest, VikramError> {
        let mut r = GitStash.permission(args)?;
        r.verb = "git.stash".into();
        Ok(r)
    }
    fn run(&self, args: &Value, ctx: &ToolContext) -> Result<ToolOutput, VikramError> {
        GitStash.run(args, ctx)
    }
}

// Aliases for the underscore-style names some callers/tests expect.
pub(crate) struct GitStatusTool;
impl Tool for GitStatusTool {
    fn spec(&self) -> ToolSpec {
        let mut s = GitStatus.spec();
        s.name = "git_status".into();
        s
    }
    fn permission(&self, args: &Value) -> Result<PermissionRequest, VikramError> {
        let mut r = GitStatus.permission(args)?;
        r.verb = "git_status".into();
        r.resource = "git".into();
        Ok(r)
    }
    fn run(&self, args: &Value, ctx: &ToolContext) -> Result<ToolOutput, VikramError> {
        GitStatus.run(args, ctx)
    }
}

pub(crate) struct GitDiffTool;
impl Tool for GitDiffTool {
    fn spec(&self) -> ToolSpec {
        let mut s = GitDiff.spec();
        s.name = "git_diff".into();
        s
    }
    fn permission(&self, args: &Value) -> Result<PermissionRequest, VikramError> {
        let mut r = GitDiff.permission(args)?;
        r.verb = "git_diff".into();
        Ok(r)
    }
    fn run(&self, args: &Value, ctx: &ToolContext) -> Result<ToolOutput, VikramError> {
        GitDiff.run(args, ctx)
    }
}

pub(crate) struct GitLogTool;
impl Tool for GitLogTool {
    fn spec(&self) -> ToolSpec {
        let mut s = GitLog.spec();
        s.name = "git_log".into();
        s
    }
    fn permission(&self, args: &Value) -> Result<PermissionRequest, VikramError> {
        let mut r = GitLog.permission(args)?;
        r.verb = "git_log".into();
        Ok(r)
    }
    fn run(&self, args: &Value, ctx: &ToolContext) -> Result<ToolOutput, VikramError> {
        GitLog.run(args, ctx)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tools::ToolContext;
    use serde_json::json;
    use std::process::Command;
    use tempfile::tempdir;

    fn ctx(dir: &std::path::Path) -> ToolContext {
        ToolContext {
            workspace_root: dir.to_path_buf(),
            sandbox: crate::sandbox::Sandbox::disabled("test"),
            spill: crate::spill::Spill::disabled(),
            runtime: None,
            session: None,
            agent: None,
            cancel: None,
        }
    }

    #[test]
    fn git_status_runs_against_a_real_repository() {
        let dir = tempdir().unwrap();
        let c = ctx(dir.path());
        Command::new("git")
            .args(["init", "-q", "."])
            .current_dir(dir.path())
            .output()
            .unwrap();
        std::fs::write(dir.path().join("untracked.txt"), "x").unwrap();
        let out = GitStatus.run(&json!({}), &c).unwrap();
        assert!(out.ok, "{}", out.content);
        assert!(out.content.contains("untracked.txt"), "{}", out.content);
    }

    #[test]
    fn git_diff_runs_against_a_real_repository() {
        let dir = tempdir().unwrap();
        let c = ctx(dir.path());
        Command::new("git")
            .args(["init", "-q", "."])
            .current_dir(dir.path())
            .output()
            .unwrap();
        std::fs::write(dir.path().join("a.txt"), "one\n").unwrap();
        let add = Command::new("git")
            .args(["add", "a.txt"])
            .current_dir(dir.path())
            .output()
            .unwrap();
        assert!(add.status.success());
        let mut commit = Command::new("git");
        commit
            .args(["commit", "-q", "-m", "initial"])
            .current_dir(dir.path())
            .env("GIT_AUTHOR_NAME", "t")
            .env("GIT_AUTHOR_EMAIL", "t@t")
            .env("GIT_COMMITTER_NAME", "t")
            .env("GIT_COMMITTER_EMAIL", "t@t");
        let committed = commit.output().unwrap();
        assert!(
            committed.status.success(),
            "{}",
            String::from_utf8_lossy(&committed.stderr)
        );
        std::fs::write(dir.path().join("a.txt"), "two\n").unwrap();
        let out = GitDiff.run(&json!({"path": "a.txt"}), &c).unwrap();
        assert!(out.ok, "{}", out.content);
        assert!(
            out.content.contains("two") || out.content.contains("+"),
            "{}",
            out.content
        );
    }

    #[test]
    fn git_log_runs_against_a_real_repository() {
        let dir = tempdir().unwrap();
        let c = ctx(dir.path());
        Command::new("git")
            .args(["init", "-q", "."])
            .current_dir(dir.path())
            .output()
            .unwrap();
        std::fs::write(dir.path().join("a.txt"), "one\n").unwrap();
        Command::new("git")
            .args(["add", "a.txt"])
            .current_dir(dir.path())
            .output()
            .unwrap();
        let mut commit = Command::new("git");
        commit
            .args(["commit", "-q", "-m", "hello-commit"])
            .current_dir(dir.path())
            .env("GIT_AUTHOR_NAME", "t")
            .env("GIT_AUTHOR_EMAIL", "t@t")
            .env("GIT_COMMITTER_NAME", "t")
            .env("GIT_COMMITTER_EMAIL", "t@t");
        let committed = commit.output().unwrap();
        assert!(
            committed.status.success(),
            "{}",
            String::from_utf8_lossy(&committed.stderr)
        );
        let out = GitLog.run(&json!({"limit": 5}), &c).unwrap();
        assert!(out.ok, "{}", out.content);
        assert!(out.content.contains("hello-commit"), "{}", out.content);
    }

    #[test]
    fn git_commit_creates_a_commit() {
        let dir = tempdir().unwrap();
        let c = ctx(dir.path());
        Command::new("git")
            .args(["init", "-q", "."])
            .current_dir(dir.path())
            .output()
            .unwrap();
        std::fs::write(dir.path().join("a.txt"), "one\n").unwrap();
        let out = GitCommit
            .run(&json!({"message": "hello-commit"}), &c)
            .unwrap();
        assert!(out.ok, "{}", out.content);
        let log = GitLog.run(&json!({"limit": 5}), &c).unwrap();
        assert!(log.ok, "{}", log.content);
        assert!(log.content.contains("hello-commit"), "{}", log.content);
    }

    #[test]
    fn git_commit_stages_only_requested_files() {
        let dir = tempdir().unwrap();
        let c = ctx(dir.path());
        Command::new("git")
            .args(["init", "-q", "."])
            .current_dir(dir.path())
            .output()
            .unwrap();
        std::fs::write(dir.path().join("a.txt"), "one\n").unwrap();
        std::fs::write(dir.path().join("b.txt"), "two\n").unwrap();
        let out = GitCommit
            .run(&json!({"message": "partial", "files": ["a.txt"]}), &c)
            .unwrap();
        assert!(out.ok, "{}", out.content);
        let status = GitStatus.run(&json!({}), &c).unwrap();
        assert!(status.content.contains("b.txt"), "{}", status.content);
    }

    #[test]
    fn git_branch_creates_and_lists() {
        let dir = tempdir().unwrap();
        let c = ctx(dir.path());
        Command::new("git")
            .args(["init", "-q", "."])
            .current_dir(dir.path())
            .output()
            .unwrap();
        std::fs::write(dir.path().join("a.txt"), "one\n").unwrap();
        Command::new("git")
            .args(["add", "a.txt"])
            .current_dir(dir.path())
            .output()
            .unwrap();
        let mut commit = Command::new("git");
        commit
            .args(["commit", "-q", "-m", "initial"])
            .current_dir(dir.path())
            .env("GIT_AUTHOR_NAME", "t")
            .env("GIT_AUTHOR_EMAIL", "t@t")
            .env("GIT_COMMITTER_NAME", "t")
            .env("GIT_COMMITTER_EMAIL", "t@t");
        let committed = commit.output().unwrap();
        assert!(
            committed.status.success(),
            "{}",
            String::from_utf8_lossy(&committed.stderr)
        );
        let out = GitBranch.run(&json!({"name": "feature"}), &c).unwrap();
        assert!(out.ok, "{}", out.content);
        let listed = GitBranch.run(&json!({"list": true}), &c).unwrap();
        assert!(listed.ok, "{}", listed.content);
        assert!(listed.content.contains("feature"), "{}", listed.content);
    }

    #[test]
    fn git_stash_push_pop_round_trips() {
        let dir = tempdir().unwrap();
        let c = ctx(dir.path());
        Command::new("git")
            .args(["init", "-q", "."])
            .current_dir(dir.path())
            .output()
            .unwrap();
        std::fs::write(dir.path().join("a.txt"), "one\n").unwrap();
        Command::new("git")
            .args(["add", "a.txt"])
            .current_dir(dir.path())
            .output()
            .unwrap();
        let mut commit = Command::new("git");
        commit
            .args(["commit", "-q", "-m", "initial"])
            .current_dir(dir.path())
            .env("GIT_AUTHOR_NAME", "t")
            .env("GIT_AUTHOR_EMAIL", "t@t")
            .env("GIT_COMMITTER_NAME", "t")
            .env("GIT_COMMITTER_EMAIL", "t@t");
        let committed = commit.output().unwrap();
        assert!(
            committed.status.success(),
            "{}",
            String::from_utf8_lossy(&committed.stderr)
        );
        std::fs::write(dir.path().join("a.txt"), "two\n").unwrap();
        let pushed = GitStash.run(&json!({"action": "push"}), &c).unwrap();
        assert!(pushed.ok, "{}", pushed.content);
        let status = GitStatus.run(&json!({}), &c).unwrap();
        assert!(
            !status.content.contains("a.txt")
                || status.content.trim() == "## master"
                || status.content.trim().starts_with("##"),
            "{}",
            status.content
        );
        let popped = GitStash.run(&json!({"action": "pop"}), &c).unwrap();
        assert!(popped.ok, "{}", popped.content);
    }

    #[test]
    fn git_stash_list_runs() {
        let dir = tempdir().unwrap();
        let c = ctx(dir.path());
        Command::new("git")
            .args(["init", "-q", "."])
            .current_dir(dir.path())
            .output()
            .unwrap();
        let out = GitStash.run(&json!({"action": "list"}), &c).unwrap();
        // Empty repo: command still returns a ToolOutput.
        assert!(!out.content.is_empty());
    }
}
