//! Tool executor.
//!
//! A tool never decides whether it may run. It only declares the permission shape of a call
//! ([`Tool::permission`]); the host evaluates that against policy and only then invokes
//! [`Tool::run`]. Tool output is external content by definition, so it re-enters the conversation
//! as [`vikram_contracts::Trust::External`].

use serde_json::{Value, json};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};
use vikram_contracts::{ActionKind, AgentId, PermissionRequest, SideEffect, Trust, VikramError};

use crate::MemoryStore;
use crate::PluginRegistry;
use crate::SkillRegistry;
use crate::config::{Config, database_path, resolve_within};
use crate::lsp::{LspClient, LspError, LspPool};
use crate::mcp::McpRegistry;
use crate::teams::TeamManager;
use vikram_store::{AgentRow, Store};

/// Output larger than this is truncated with an explicit marker rather than silently cut.
pub const MAX_TOOL_OUTPUT: usize = 20_000;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ToolOutput {
    pub content: String,
    pub ok: bool,
}

#[derive(Clone, Debug)]
pub struct ToolContext {
    pub workspace_root: PathBuf,
}

pub use crate::provider::ToolSpec;

pub trait Tool: Send + Sync {
    fn spec(&self) -> ToolSpec;
    /// How the host should describe this call to policy.
    fn permission(&self, args: &Value) -> Result<PermissionRequest, VikramError>;
    fn run(&self, args: &Value, ctx: &ToolContext) -> Result<ToolOutput, VikramError>;
}

#[derive(Default)]
pub struct ToolRegistry {
    tools: std::sync::Mutex<BTreeMap<String, Arc<dyn Tool>>>,
    disabled: std::sync::Mutex<std::collections::HashSet<String>>,
}

impl ToolRegistry {
    pub fn new() -> Self {
        let home = crate::config::vikram_home();
        let path = home.join("disabled_tools.json");
        let disabled = std::fs::read_to_string(&path)
            .ok()
            .and_then(|s| serde_json::from_str::<Vec<String>>(&s).ok())
            .map(|v| v.into_iter().collect())
            .unwrap_or_default();
        Self {
            tools: std::sync::Mutex::new(BTreeMap::new()),
            disabled: std::sync::Mutex::new(disabled),
        }
    }

    /// The default set. Registering a tool is the only way to make it callable; there is no
    /// dynamic loading from model output.
    pub fn with_builtins() -> Self {
        let registry = Self::new();
        registry.register(Box::new(ReadFile));
        registry.register(Box::new(ListDir));
        registry.register(Box::new(WriteFile));
        registry.register(Box::new(EditFile));
        registry.register(Box::new(DeleteFile));
        registry.register(Box::new(SearchText));
        registry.register(Box::new(RunShell));
        registry.register(Box::new(Git));
        registry.register(Box::new(BuildRun));
        registry.register(Box::new(TestRun));
        registry.register(Box::new(MemorySave));
        registry.register(Box::new(MemorySearch));
        registry.register(Box::new(MemoryList));
        registry.register(Box::new(MemoryDelete));
        registry.register(Box::new(WebSearch));
        registry.register(Box::new(TeamCreate));
        registry.register(Box::new(TeamList));
        registry.register(Box::new(TeamDelegate));
        registry.register(Box::new(HiveCreate));
        registry.register(Box::new(HiveList));
        registry.register(Box::new(HiveShow));
        registry.register(Box::new(HiveDelegate));
        registry.register(Box::new(HiveDelete));
        registry.register(Box::new(BotList));
        registry.register(Box::new(BotShow));
        registry.register(Box::new(BotCreate));
        registry.register(Box::new(AgentList));
        registry.register(Box::new(AgentShow));
        registry.register(Box::new(SkillList));
        registry.register(Box::new(SkillLoad));
        registry.register(Box::new(SkillSearch));
        registry.register(Box::new(SkillRead));
        registry.register(Box::new(Diagnostics));
        registry.register(Box::new(Process));
        registry.register(Box::new(Task));
        registry.register(Box::new(AskUser));
        registry.register(Box::new(PermissionCheck));
        registry.register(Box::new(BrowserFetch));
        registry.register(Box::new(BrowserDom));
        registry.register(Box::new(BrowserLinks));
        registry.register(Box::new(BrowserScreenshot));
        registry.register(Box::new(BrowserOpen));
        registry.register(Box::new(BrowserEval));
        registry.register(Box::new(BrowserClick));
        registry.register(Box::new(BrowserType));
        registry.register(Box::new(BrowserScroll));
        registry.register(Box::new(BrowserText));
        registry.register(Box::new(BrowserClose));
        registry.register(Box::new(BrowserSessions));
        registry.register(Box::new(CodeReview));
        registry.register(Box::new(LspHover));
        registry.register(Box::new(LspDefinition));
        registry.register(Box::new(LspReferences));
        registry.register(Box::new(LspSymbols));
        registry.register(Box::new(LspDiagnostics));
        registry.register(Box::new(PluginExecute));
        registry.register(Box::new(PluginList));
        registry.register(Box::new(PluginInstall));
        registry.register(Box::new(PluginUninstall));
        registry.register(Box::new(PluginEnable));
        registry.register(Box::new(PluginDisable));
        registry.register(Box::new(PluginValidate));
        registry.register(Box::new(PluginInspect));
        registry.register(Box::new(PluginDeps));
        registry.register(Box::new(TerminalTool));
        crate::computer::register(&registry);
        registry
    }

    pub fn register(&self, tool: Box<dyn Tool>) {
        self.tools
            .lock()
            .expect("tools lock")
            .insert(tool.spec().name, Arc::from(tool));
    }

    /// Cloned handle so a tool can be moved into a blocking task.
    pub fn get(&self, name: &str) -> Option<Arc<dyn Tool>> {
        self.tools.lock().expect("tools lock").get(name).cloned()
    }

    pub fn specs(&self) -> Vec<ToolSpec> {
        let disabled = self.disabled.lock().expect("disabled lock").clone();
        self.tools
            .lock()
            .expect("tools lock")
            .values()
            .filter(|t| !disabled.contains(&t.spec().name))
            .map(|t| t.spec())
            .collect()
    }

    pub fn names(&self) -> Vec<String> {
        self.tools
            .lock()
            .expect("tools lock")
            .keys()
            .cloned()
            .collect()
    }

    pub fn is_enabled(&self, name: &str) -> bool {
        !self.disabled.lock().expect("disabled lock").contains(name)
    }

    pub fn tool_enable(&self, name: &str) {
        self.disabled.lock().expect("disabled lock").remove(name);
        self.persist_disabled();
    }

    pub fn tool_disable(&self, name: &str) {
        self.disabled.lock().expect("disabled lock").insert(name.to_owned());
        self.persist_disabled();
    }

    fn persist_disabled(&self) {
        let disabled: Vec<String> = self.disabled.lock().expect("disabled lock").iter().cloned().collect();
        let path = crate::config::vikram_home().join("disabled_tools.json");
        let _ = std::fs::create_dir_all(crate::config::vikram_home());
        let _ = std::fs::write(&path, serde_json::to_string(&disabled).unwrap_or_default());
    }

    /// Iterate over registered tools (snapshot of Arcs).
    pub fn all(&self) -> Vec<Arc<dyn Tool>> {
        self.tools
            .lock()
            .expect("tools lock")
            .values()
            .cloned()
            .collect()
    }
}

fn truncated(mut text: String) -> String {
    if text.len() > MAX_TOOL_OUTPUT {
        let cut = text
            .char_indices()
            .take_while(|(i, _)| *i < MAX_TOOL_OUTPUT)
            .last()
            .map(|(i, c)| i + c.len_utf8())
            .unwrap_or(0);
        text.truncate(cut);
        text.push_str("\n… [output truncated]");
    }
    text
}

fn arg_str(args: &Value, key: &str) -> Result<String, VikramError> {
    args.get(key)
        .and_then(Value::as_str)
        .map(str::to_owned)
        .ok_or_else(|| VikramError::ProviderRejected(format!("missing string argument `{key}`")))
}

fn canonical_resource(root: &Path, raw: &str) -> Result<(PathBuf, String), VikramError> {
    let resolved =
        resolve_within(root, Path::new(raw)).ok_or_else(|| VikramError::PolicyDenied {
            reason: format!("`{raw}` is outside the workspace root `{}`", root.display()),
        })?;
    let display = resolved.to_string_lossy().into_owned();
    Ok((resolved, display))
}

struct ReadFile;

impl Tool for ReadFile {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "fs.read".into(),
            description: "Read a text file from the workspace.".into(),
            schema: json!({
                "type": "object",
                "properties": {"path": {"type": "string"}},
                "required": ["path"]
            }),
        }
    }

    fn permission(&self, args: &Value) -> Result<PermissionRequest, VikramError> {
        let path = arg_str(args, "path")?;
        Ok(PermissionRequest::new(
            ActionKind::Read,
            "fs.read",
            path,
            SideEffect::None,
            Trust::Model,
        ))
    }

    fn run(&self, args: &Value, ctx: &ToolContext) -> Result<ToolOutput, VikramError> {
        let raw = arg_str(args, "path")?;
        let (path, _) = canonical_resource(&ctx.workspace_root, &raw)?;
        match std::fs::read_to_string(&path) {
            Ok(text) => Ok(ToolOutput {
                content: truncated(text),
                ok: true,
            }),
            Err(e) => Ok(ToolOutput {
                content: format!("cannot read `{}`: {e}", path.display()),
                ok: false,
            }),
        }
    }
}

struct ListDir;

impl Tool for ListDir {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "fs.list".into(),
            description: "List a directory in the workspace.".into(),
            schema: json!({
                "type": "object",
                "properties": {"path": {"type": "string"}}
            }),
        }
    }

    fn permission(&self, args: &Value) -> Result<PermissionRequest, VikramError> {
        let path = args.get("path").and_then(Value::as_str).unwrap_or(".");
        Ok(PermissionRequest::new(
            ActionKind::Read,
            "fs.list",
            path,
            SideEffect::None,
            Trust::Model,
        ))
    }

    fn run(&self, args: &Value, ctx: &ToolContext) -> Result<ToolOutput, VikramError> {
        let raw = args.get("path").and_then(Value::as_str).unwrap_or(".");
        let (path, _) = canonical_resource(&ctx.workspace_root, raw)?;
        let mut entries: Vec<String> = Vec::new();
        match std::fs::read_dir(&path) {
            Ok(iter) => {
                for entry in iter.flatten() {
                    let name = entry.file_name().to_string_lossy().into_owned();
                    if name == ".git" || name == "target" {
                        continue;
                    }
                    let is_dir = entry.file_type().map(|t| t.is_dir()).unwrap_or(false);
                    entries.push(if is_dir { format!("{name}/") } else { name });
                }
            }
            Err(e) => {
                return Ok(ToolOutput {
                    content: format!("cannot list `{}`: {e}", path.display()),
                    ok: false,
                });
            }
        }
        entries.sort();
        Ok(ToolOutput {
            content: truncated(entries.join("\n")),
            ok: true,
        })
    }
}

struct WriteFile;

impl Tool for WriteFile {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "fs.write".into(),
            description: "Write a text file inside the workspace, creating parents.".into(),
            schema: json!({
                "type": "object",
                "properties": {
                    "path": {"type": "string"},
                    "content": {"type": "string"}
                },
                "required": ["path", "content"]
            }),
        }
    }

    fn permission(&self, args: &Value) -> Result<PermissionRequest, VikramError> {
        let path = arg_str(args, "path")?;
        Ok(PermissionRequest::new(
            ActionKind::Write,
            "fs.write",
            path,
            SideEffect::LocalMutation,
            Trust::Model,
        ))
    }

    fn run(&self, args: &Value, ctx: &ToolContext) -> Result<ToolOutput, VikramError> {
        let raw = arg_str(args, "path")?;
        let content = arg_str(args, "content")?;
        let (path, _) = canonical_resource(&ctx.workspace_root, &raw)?;
        if let Some(parent) = path.parent() {
            if let Err(e) = std::fs::create_dir_all(parent) {
                return Ok(ToolOutput {
                    content: format!("cannot create `{}`: {e}", parent.display()),
                    ok: false,
                });
            }
        }
        let bytes = content.len();
        match std::fs::write(&path, content) {
            Ok(()) => Ok(ToolOutput {
                content: format!("wrote {} bytes to {}", bytes, path.display()),
                ok: true,
            }),
            Err(e) => Ok(ToolOutput {
                content: format!("cannot write `{}`: {e}", path.display()),
                ok: false,
            }),
        }
    }
}

struct EditFile;

impl Tool for EditFile {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "fs.edit".into(),
            description: "Edit a file by replacing an exact string.".into(),
            schema: json!({
                "type": "object",
                "properties": {
                    "path": {"type": "string"},
                    "old_string": {"type": "string"},
                    "new_string": {"type": "string"}
                },
                "required": ["path", "old_string", "new_string"]
            }),
        }
    }

    fn permission(&self, args: &Value) -> Result<PermissionRequest, VikramError> {
        let path = arg_str(args, "path")?;
        Ok(PermissionRequest::new(
            ActionKind::Write,
            "fs.edit",
            path,
            SideEffect::LocalMutation,
            Trust::Model,
        ))
    }

    fn run(&self, args: &Value, ctx: &ToolContext) -> Result<ToolOutput, VikramError> {
        let raw = arg_str(args, "path")?;
        let old_string = arg_str(args, "old_string")?;
        let new_string = arg_str(args, "new_string")?;
        let (path, _) = canonical_resource(&ctx.workspace_root, &raw)?;
        let content = match std::fs::read_to_string(&path) {
            Ok(text) => text,
            Err(e) => {
                return Ok(ToolOutput {
                    content: format!("cannot read `{}`: {e}", path.display()),
                    ok: false,
                });
            }
        };
        let count = content.matches(&old_string).count();
        if count == 0 {
            return Ok(ToolOutput {
                content: "old_string not found in file".into(),
                ok: false,
            });
        }
        if count > 1 {
            return Ok(ToolOutput {
                content: format!("old_string appears {count} times, must be unique"),
                ok: false,
            });
        }
        let new_content = content.replace(&old_string, &new_string);
        if let Some(parent) = path.parent() {
            if let Err(e) = std::fs::create_dir_all(parent) {
                return Ok(ToolOutput {
                    content: format!("cannot create `{}`: {e}", parent.display()),
                    ok: false,
                });
            }
        }
        match std::fs::write(&path, &new_content) {
            Ok(()) => Ok(ToolOutput {
                content: truncated(format!(
                    "edited {}: {} -> {} bytes",
                    raw,
                    content.len(),
                    new_content.len()
                )),
                ok: true,
            }),
            Err(e) => Ok(ToolOutput {
                content: format!("cannot write `{}`: {e}", path.display()),
                ok: false,
            }),
        }
    }
}

struct DeleteFile;

impl Tool for DeleteFile {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "fs.delete".into(),
            description: "Delete a file inside the workspace.".into(),
            schema: json!({
                "type": "object",
                "properties": {
                    "path": {"type": "string"}
                },
                "required": ["path"]
            }),
        }
    }

    fn permission(&self, args: &Value) -> Result<PermissionRequest, VikramError> {
        let path = arg_str(args, "path")?;
        Ok(PermissionRequest::new(
            ActionKind::Write,
            "fs.delete",
            path,
            SideEffect::Destructive,
            Trust::Model,
        ))
    }

    fn run(&self, args: &Value, ctx: &ToolContext) -> Result<ToolOutput, VikramError> {
        let raw = arg_str(args, "path")?;
        let (path, _) = canonical_resource(&ctx.workspace_root, &raw)?;
        let content = match std::fs::read_to_string(&path) {
            Ok(text) => text,
            Err(e) => {
                return Ok(ToolOutput {
                    content: format!("cannot read `{}`: {e}", path.display()),
                    ok: false,
                });
            }
        };
        match std::fs::remove_file(&path) {
            Ok(()) => Ok(ToolOutput {
                content: truncated(format!(
                    "deleted {}\n--- deleted content ---\n{}",
                    raw, content
                )),
                ok: true,
            }),
            Err(e) => Ok(ToolOutput {
                content: format!("cannot delete `{}`: {e}", path.display()),
                ok: false,
            }),
        }
    }
}

struct SearchText;

impl Tool for SearchText {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "search.text".into(),
            description: "Search workspace text files for a substring.".into(),
            schema: json!({
                "type": "object",
                "properties": {
                    "query": {"type": "string"},
                    "path": {"type": "string"}
                },
                "required": ["query"]
            }),
        }
    }

    fn permission(&self, args: &Value) -> Result<PermissionRequest, VikramError> {
        let query = arg_str(args, "query")?;
        Ok(PermissionRequest::new(
            ActionKind::Read,
            "search.text",
            query,
            SideEffect::None,
            Trust::Model,
        ))
    }

    fn run(&self, args: &Value, ctx: &ToolContext) -> Result<ToolOutput, VikramError> {
        let query = arg_str(args, "query")?;
        let raw = args.get("path").and_then(Value::as_str).unwrap_or(".");
        let (root, _) = canonical_resource(&ctx.workspace_root, raw)?;
        let mut hits: Vec<String> = Vec::new();
        walk(&root, &query, &mut hits, 0);
        if hits.is_empty() {
            return Ok(ToolOutput {
                content: format!("no matches for `{query}`"),
                ok: true,
            });
        }
        Ok(ToolOutput {
            content: truncated(hits.join("\n")),
            ok: true,
        })
    }
}

fn walk(dir: &Path, query: &str, hits: &mut Vec<String>, depth: usize) {
    if depth > 12 || hits.len() >= 200 {
        return;
    }
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        let name = entry.file_name().to_string_lossy().into_owned();
        if name == ".git" || name == "target" || name == "node_modules" {
            continue;
        }
        if path.is_dir() {
            walk(&path, query, hits, depth + 1);
            continue;
        }
        let Ok(meta) = entry.metadata() else { continue };
        if meta.len() > 1_000_000 {
            continue;
        }
        let Ok(text) = std::fs::read_to_string(&path) else {
            continue;
        };
        for (idx, line) in text.lines().enumerate() {
            if hits.len() >= 200 {
                return;
            }
            if line.contains(query) {
                hits.push(format!("{}:{}: {}", path.display(), idx + 1, line.trim()));
            }
        }
    }
}

/// Shared implementation of the RunShell execution pattern: spawn a shell command in
/// `ctx.workspace_root`, capture stdout+stderr, enforce a timeout, truncate output.
fn run_shell_command(command: &str, ctx: &ToolContext, timeout_ms: u64) -> ToolOutput {
    #[cfg(windows)]
    let mut cmd = {
        let mut c = std::process::Command::new("cmd");
        c.args(["/C", command]);
        c
    };
    #[cfg(not(windows))]
    let mut cmd = {
        let mut c = std::process::Command::new("sh");
        c.args(["-c", command]);
        c
    };

    let mut child = match cmd
        .current_dir(&ctx.workspace_root)
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
    {
        Ok(child) => child,
        Err(e) => {
            return ToolOutput {
                content: format!("failed to start command: {e}"),
                ok: false,
            };
        }
    };

    let deadline = Instant::now() + Duration::from_millis(timeout_ms);
    loop {
        match child.try_wait() {
            Ok(Some(_)) => break,
            Ok(None) => {
                if Instant::now() >= deadline {
                    let _ = child.kill();
                    let _ = child.wait();
                    return ToolOutput {
                        content: format!("command timed out after {timeout_ms} ms"),
                        ok: false,
                    };
                }
                std::thread::sleep(Duration::from_millis(25));
            }
            Err(e) => {
                return ToolOutput {
                    content: format!("failed while waiting: {e}"),
                    ok: false,
                };
            }
        }
    }

    let output = match child.wait_with_output() {
        Ok(o) => o,
        Err(e) => {
            return ToolOutput {
                content: format!("failed while waiting: {e}"),
                ok: false,
            };
        }
    };
    let mut body = String::new();
    body.push_str(&String::from_utf8_lossy(&output.stdout));
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
    // The exit code is the one fact a command's row must not guess at, and the platform only hands
    // it over here, so it travels with the output instead of being inferred later.
    body.push_str(&format!("\n[exit {}]", output.status.code().unwrap_or(-1)));
    ToolOutput {
        content: truncated(body),
        ok: output.status.success(),
    }
}

struct RunShell;

impl Tool for RunShell {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "shell.run".into(),
            description: "Run a shell command in the workspace.".into(),
            schema: json!({
                "type": "object",
                "properties": {
                    "command": {"type": "string"},
                    "timeout_ms": {"type": "integer"}
                },
                "required": ["command"]
            }),
        }
    }

    fn permission(&self, args: &Value) -> Result<PermissionRequest, VikramError> {
        let command = arg_str(args, "command")?;
        let head = command.split_whitespace().next().unwrap_or("shell");
        Ok(PermissionRequest::new(
            ActionKind::Execute,
            "shell.run",
            head,
            SideEffect::LocalMutation,
            Trust::Model,
        ))
    }

    fn run(&self, args: &Value, ctx: &ToolContext) -> Result<ToolOutput, VikramError> {
        let command = arg_str(args, "command")?;
        let timeout_ms = args
            .get("timeout_ms")
            .and_then(Value::as_u64)
            .unwrap_or(30_000)
            .clamp(1_000, 120_000);

        #[cfg(windows)]
        let mut cmd = {
            let mut c = std::process::Command::new("cmd");
            c.args(["/C", &command]);
            c
        };
        #[cfg(not(windows))]
        let mut cmd = {
            let mut c = std::process::Command::new("sh");
            c.args(["-c", &command]);
            c
        };

        let mut child = match cmd
            .current_dir(&ctx.workspace_root)
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .spawn()
        {
            Ok(child) => child,
            Err(e) => {
                return Ok(ToolOutput {
                    content: format!("failed to start command: {e}"),
                    ok: false,
                });
            }
        };

        let deadline = Instant::now() + Duration::from_millis(timeout_ms);
        loop {
            match child.try_wait() {
                Ok(Some(_)) => break,
                Ok(None) => {
                    if Instant::now() >= deadline {
                        let _ = child.kill();
                        let _ = child.wait();
                        return Ok(ToolOutput {
                            content: format!("command timed out after {timeout_ms} ms"),
                            ok: false,
                        });
                    }
                    std::thread::sleep(Duration::from_millis(25));
                }
                Err(e) => {
                    return Ok(ToolOutput {
                        content: format!("failed while waiting: {e}"),
                        ok: false,
                    });
                }
            }
        }

        let output = child
            .wait_with_output()
            .map_err(|e| VikramError::EffectFailed(e.to_string()))?;
        let mut body = String::new();
        body.push_str(&String::from_utf8_lossy(&output.stdout));
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
        Ok(ToolOutput {
            content: truncated(body),
            ok: output.status.success(),
        })
    }
}

/// Persistent interactive terminal backed by a real PTY.
struct TerminalTool;

impl Tool for TerminalTool {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "terminal".into(),
            description:
                "Drive a persistent interactive PTY session (open/write/read/resize/close/list)."
                    .into(),
            schema: json!({
                "type": "object",
                "properties": {
                    "action": {"type": "string", "enum": ["open", "write", "read", "resize", "close", "list"]},
                    "session": {"type": "string"},
                    "input": {"type": "string"},
                    "shell": {"type": "string"},
                    "cwd": {"type": "string"},
                    "rows": {"type": "integer"},
                    "cols": {"type": "integer"},
                    "drain": {"type": "boolean"}
                },
                "required": ["action"]
            }),
        }
    }

    fn permission(&self, args: &Value) -> Result<PermissionRequest, VikramError> {
        let action = args.get("action").and_then(Value::as_str).unwrap_or("open");
        Ok(PermissionRequest::new(
            ActionKind::Execute,
            "terminal",
            format!("terminal.{action}"),
            SideEffect::LocalMutation,
            Trust::Model,
        ))
    }

    fn run(&self, args: &Value, ctx: &ToolContext) -> Result<ToolOutput, VikramError> {
        let action = arg_str(args, "action")?;
        let result = match action.as_str() {
            "open" => {
                let shell = args.get("shell").and_then(Value::as_str);
                let cwd = args
                    .get("cwd")
                    .and_then(Value::as_str)
                    .map(|c| c.to_owned())
                    .unwrap_or_else(|| ctx.workspace_root.to_string_lossy().into_owned());
                let rows = args.get("rows").and_then(Value::as_u64).unwrap_or(24) as u16;
                let cols = args.get("cols").and_then(Value::as_u64).unwrap_or(80) as u16;
                let name = args.get("session").and_then(Value::as_str);
                crate::pty::open_named(name, shell, Some(&cwd), rows, cols)
                    .map(|id| format!("opened terminal session `{id}` in {cwd}"))
            }
            "write" => {
                let session = arg_str(args, "session")?;
                let input = arg_str(args, "input")?;
                crate::pty::write(&session, &input).map(|_| format!("wrote {} bytes", input.len()))
            }
            "read" => {
                let session = arg_str(args, "session")?;
                let drain = args.get("drain").and_then(Value::as_bool).unwrap_or(false);
                crate::pty::read(&session, drain)
            }
            "resize" => {
                let session = arg_str(args, "session")?;
                let rows = args.get("rows").and_then(Value::as_u64).unwrap_or(24) as u16;
                let cols = args.get("cols").and_then(Value::as_u64).unwrap_or(80) as u16;
                crate::pty::resize(&session, rows, cols)
                    .map(|_| format!("resized to {cols}x{rows}"))
            }
            "close" => {
                let session = arg_str(args, "session")?;
                crate::pty::close(&session).map(|_| format!("closed session `{session}`"))
            }
            "list" => {
                let sessions = crate::pty::list();
                if sessions.is_empty() {
                    Ok("no live terminal sessions".to_owned())
                } else {
                    let mut out = String::new();
                    for (id, shell, cwd) in sessions {
                        out.push_str(&format!("{id}\t{shell}\t{cwd}\n"));
                    }
                    Ok(out)
                }
            }
            other => Err(format!("unknown terminal action `{other}`")),
        };

        Ok(match result {
            Ok(content) => ToolOutput {
                content: truncated(content),
                ok: true,
            },
            Err(err) => ToolOutput {
                content: err,
                ok: false,
            },
        })
    }
}

struct Git;

impl Tool for Git {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "git".into(),
            description: "Run a read-only git subcommand in the workspace.".into(),
            schema: json!({
                "type": "object",
                "properties": {
                    "subcommand": {"type": "string", "enum": ["status", "diff", "log", "branch", "show"]},
                    "staged": {"type": "boolean"},
                    "commit": {"type": "string"}
                },
                "required": ["subcommand"]
            }),
        }
    }

    fn permission(&self, args: &Value) -> Result<PermissionRequest, VikramError> {
        let sub = arg_str(args, "subcommand")?;
        let (action, side_effect) = match sub.as_str() {
            "status" | "diff" | "log" | "branch" | "show" => (ActionKind::Read, SideEffect::None),
            "commit" => (ActionKind::Write, SideEffect::LocalMutation),
            _ => (ActionKind::Read, SideEffect::None),
        };
        Ok(PermissionRequest::new(
            action,
            "git",
            sub,
            side_effect,
            Trust::Model,
        ))
    }

    fn run(&self, args: &Value, ctx: &ToolContext) -> Result<ToolOutput, VikramError> {
        let sub = arg_str(args, "subcommand")?;
        let command = match sub.as_str() {
            "status" => "git status --short".to_string(),
            "diff" => {
                let staged = args.get("staged").and_then(Value::as_bool).unwrap_or(false);
                if staged {
                    "git diff --cached".to_string()
                } else {
                    "git diff".to_string()
                }
            }
            "log" => "git log --oneline -20".to_string(),
            "branch" => "git branch -a".to_string(),
            "show" => {
                let commit = args.get("commit").and_then(Value::as_str).unwrap_or("HEAD");
                format!("git show {}", commit)
            }
            other => {
                return Ok(ToolOutput {
                    content: format!("unsupported git subcommand: {other}"),
                    ok: false,
                });
            }
        };
        Ok(run_shell_command(&command, ctx, 30_000))
    }
}

struct BuildRun;

impl Tool for BuildRun {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "build.run".into(),
            description: "Detect the build system and run a build.".into(),
            schema: json!({
                "type": "object",
                "properties": {}
            }),
        }
    }

    fn permission(&self, _args: &Value) -> Result<PermissionRequest, VikramError> {
        Ok(PermissionRequest::new(
            ActionKind::Execute,
            "build.run",
            "build",
            SideEffect::LocalMutation,
            Trust::Model,
        ))
    }

    fn run(&self, _args: &Value, ctx: &ToolContext) -> Result<ToolOutput, VikramError> {
        let root = &ctx.workspace_root;
        let command = if root.join("Cargo.toml").is_file() {
            "cargo build".to_string()
        } else if root.join("package.json").is_file() {
            if root.join("pnpm-lock.yaml").is_file() {
                "pnpm build".to_string()
            } else {
                "npm run build".to_string()
            }
        } else if root.join("Makefile").is_file() {
            "make".to_string()
        } else if root.join("pyproject.toml").is_file() {
            "python -m build".to_string()
        } else {
            return Ok(ToolOutput {
                content: "no build system detected".into(),
                ok: false,
            });
        };
        Ok(run_shell_command(&command, ctx, 120_000))
    }
}

struct TestRun;

impl Tool for TestRun {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "test.run".into(),
            description: "Detect the test framework and run tests.".into(),
            schema: json!({
                "type": "object",
                "properties": {}
            }),
        }
    }

    fn permission(&self, _args: &Value) -> Result<PermissionRequest, VikramError> {
        Ok(PermissionRequest::new(
            ActionKind::Execute,
            "test.run",
            "test",
            SideEffect::LocalMutation,
            Trust::Model,
        ))
    }

    fn run(&self, _args: &Value, ctx: &ToolContext) -> Result<ToolOutput, VikramError> {
        let root = &ctx.workspace_root;
        let command = if root.join("Cargo.toml").is_file() {
            "cargo test".to_string()
        } else if root.join("package.json").is_file() {
            "npm test".to_string()
        } else if root.join("pytest.ini").is_file() || root.join("pyproject.toml").is_file() {
            "pytest".to_string()
        } else {
            return Ok(ToolOutput {
                content: "no test framework detected".into(),
                ok: false,
            });
        };
        Ok(run_shell_command(&command, ctx, 120_000))
    }
}

struct MemorySave;

impl Tool for MemorySave {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "memory.save".into(),
            description: "Persist a key/value pair to memory.".into(),
            schema: json!({
                "type": "object",
                "properties": {
                    "key": {"type": "string"},
                    "value": {"type": "string"},
                    "scope": {"type": "string"}
                },
                "required": ["key", "value"]
            }),
        }
    }

    fn permission(&self, args: &Value) -> Result<PermissionRequest, VikramError> {
        let key = arg_str(args, "key")?;
        Ok(PermissionRequest::new(
            ActionKind::Write,
            "memory.save",
            format!("memory:{key}"),
            SideEffect::LocalMutation,
            Trust::Model,
        ))
    }

    fn run(&self, args: &Value, _ctx: &ToolContext) -> Result<ToolOutput, VikramError> {
        let key = arg_str(args, "key")?;
        let value = arg_str(args, "value")?;
        let scope = args
            .get("scope")
            .and_then(Value::as_str)
            .unwrap_or("global");
        let store =
            MemoryStore::open(database_path()).map_err(|e| VikramError::Storage(e.to_string()))?;
        store
            .save(&key, &value, scope)
            .map_err(|e| VikramError::Storage(e.to_string()))?;
        Ok(ToolOutput {
            content: format!("saved memory `{key}` in scope `{scope}`"),
            ok: true,
        })
    }
}

struct MemorySearch;

impl Tool for MemorySearch {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "memory.search".into(),
            description: "Search memory entries by substring.".into(),
            schema: json!({
                "type": "object",
                "properties": {
                    "query": {"type": "string"},
                    "scope": {"type": "string"}
                },
                "required": ["query"]
            }),
        }
    }

    fn permission(&self, args: &Value) -> Result<PermissionRequest, VikramError> {
        let query = arg_str(args, "query")?;
        Ok(PermissionRequest::new(
            ActionKind::Read,
            "memory.search",
            query,
            SideEffect::None,
            Trust::Model,
        ))
    }

    fn run(&self, args: &Value, _ctx: &ToolContext) -> Result<ToolOutput, VikramError> {
        let query = arg_str(args, "query")?;
        let scope = args
            .get("scope")
            .and_then(Value::as_str)
            .unwrap_or("global");
        let store =
            MemoryStore::open(database_path()).map_err(|e| VikramError::Storage(e.to_string()))?;
        let hits = store.search(&query, scope);
        if hits.is_empty() {
            return Ok(ToolOutput {
                content: format!("no memory matches for `{query}` in scope `{scope}`"),
                ok: true,
            });
        }
        let body = hits
            .iter()
            .map(|(k, v)| format!("{k}: {v}"))
            .collect::<Vec<_>>()
            .join("\n");
        Ok(ToolOutput {
            content: truncated(body),
            ok: true,
        })
    }
}

struct MemoryList;

impl Tool for MemoryList {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "memory.list".into(),
            description: "List all memory entries in a scope.".into(),
            schema: json!({
                "type": "object",
                "properties": {
                    "scope": {"type": "string"}
                }
            }),
        }
    }

    fn permission(&self, args: &Value) -> Result<PermissionRequest, VikramError> {
        let scope = args
            .get("scope")
            .and_then(Value::as_str)
            .unwrap_or("global");
        Ok(PermissionRequest::new(
            ActionKind::Read,
            "memory.list",
            scope,
            SideEffect::None,
            Trust::Model,
        ))
    }

    fn run(&self, args: &Value, _ctx: &ToolContext) -> Result<ToolOutput, VikramError> {
        let scope = args
            .get("scope")
            .and_then(Value::as_str)
            .unwrap_or("global");
        let store =
            MemoryStore::open(database_path()).map_err(|e| VikramError::Storage(e.to_string()))?;
        let entries = store.list(scope);
        if entries.is_empty() {
            return Ok(ToolOutput {
                content: format!("no memory entries in scope `{scope}`"),
                ok: true,
            });
        }
        let body = entries
            .iter()
            .map(|(k, v)| format!("{k}: {v}"))
            .collect::<Vec<_>>()
            .join("\n");
        Ok(ToolOutput {
            content: truncated(body),
            ok: true,
        })
    }
}

struct MemoryDelete;

impl Tool for MemoryDelete {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "memory.delete".into(),
            description: "Delete a memory entry.".into(),
            schema: json!({
                "type": "object",
                "properties": {
                    "key": {"type": "string"},
                    "scope": {"type": "string"}
                },
                "required": ["key"]
            }),
        }
    }

    fn permission(&self, args: &Value) -> Result<PermissionRequest, VikramError> {
        let key = arg_str(args, "key")?;
        Ok(PermissionRequest::new(
            ActionKind::Write,
            "memory.delete",
            format!("memory:{key}"),
            SideEffect::Destructive,
            Trust::Model,
        ))
    }

    fn run(&self, args: &Value, _ctx: &ToolContext) -> Result<ToolOutput, VikramError> {
        let key = arg_str(args, "key")?;
        let scope = args
            .get("scope")
            .and_then(Value::as_str)
            .unwrap_or("global");
        let store =
            MemoryStore::open(database_path()).map_err(|e| VikramError::Storage(e.to_string()))?;
        let deleted = store
            .delete(&key, scope)
            .map_err(|e| VikramError::Storage(e.to_string()))?;
        Ok(ToolOutput {
            content: if deleted {
                format!("deleted memory `{key}` in scope `{scope}`")
            } else {
                format!("no memory entry `{key}` in scope `{scope}`")
            },
            ok: deleted,
        })
    }
}

struct WebSearch;

impl Tool for WebSearch {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "web.search".into(),
            description: "Search the web (Bing HTML, or Brave when a key is configured) and return result snippets.".into(),
            schema: json!({
                "type": "object",
                "properties": {
                    "query": {"type": "string"},
                    "limit": {"type": "integer"}
                },
                "required": ["query"]
            }),
        }
    }

    fn permission(&self, args: &Value) -> Result<PermissionRequest, VikramError> {
        let query = arg_str(args, "query")?;
        Ok(PermissionRequest::new(
            ActionKind::Network,
            "web.search",
            query,
            SideEffect::None,
            Trust::Model,
        ))
    }

    fn run(&self, args: &Value, _ctx: &ToolContext) -> Result<ToolOutput, VikramError> {
        let query = arg_str(args, "query")?;
        let limit = args
            .get("limit")
            .and_then(Value::as_u64)
            .map(|n| n as usize)
            .filter(|&n| n > 0)
            .unwrap_or(5);

        let config = match Config::load() {
            Ok(config) => config,
            Err(err) => {
                return Ok(ToolOutput {
                    content: format!("web search is not configured: {err}"),
                    ok: false,
                });
            }
        };
        let api_key = config.search_api_key_with_source().map(|(value, _)| value);
        let client = match crate::search::client() {
            Ok(client) => client,
            Err(err) => {
                return Ok(ToolOutput {
                    content: format!("web search failed: {err}"),
                    ok: false,
                });
            }
        };

        let outcome =
            crate::search::search(&config.search, api_key.as_deref(), &client, &query, limit);
        Ok(search_tool_output(outcome))
    }
}

/// Map a backend outcome onto a tool result. A backend that answered with nothing usable is a
/// failure: an empty success would tell the model a search happened when it did not.
fn search_tool_output(
    outcome: Result<Vec<crate::search::SearchResult>, crate::search::SearchError>,
) -> ToolOutput {
    match outcome {
        Ok(results) if !results.is_empty() => ToolOutput {
            content: truncated(crate::search::render(&results)),
            ok: true,
        },
        Ok(_) => ToolOutput {
            content: "web search failed: the backend answered with zero results".into(),
            ok: false,
        },
        Err(err) => ToolOutput {
            content: format!("web search failed: {err}"),
            ok: false,
        },
    }
}

struct SkillList;

impl Tool for SkillList {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "skill.list".into(),
            description: "List available skills (name and description).".into(),
            schema: json!({
                "type": "object",
                "properties": {}
            }),
        }
    }

    fn permission(&self, _args: &Value) -> Result<PermissionRequest, VikramError> {
        Ok(PermissionRequest::new(
            ActionKind::Read,
            "skill.list",
            "skills",
            SideEffect::None,
            Trust::Model,
        ))
    }

    fn run(&self, _args: &Value, ctx: &ToolContext) -> Result<ToolOutput, VikramError> {
        let reg = SkillRegistry::for_workspace(&ctx.workspace_root);
        let skills = reg.list();
        if skills.is_empty() {
            return Ok(ToolOutput {
                content: "no skills available".into(),
                ok: true,
            });
        }
        let body = skills
            .iter()
            .map(|s| format!("{}: {}", s.name, s.description))
            .collect::<Vec<_>>()
            .join("\n");
        Ok(ToolOutput {
            content: truncated(body),
            ok: true,
        })
    }
}

struct SkillLoad;

impl Tool for SkillLoad {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "skill.load".into(),
            description: "Load a skill's instructions by name.".into(),
            schema: json!({
                "type": "object",
                "properties": {
                    "name": {"type": "string"}
                },
                "required": ["name"]
            }),
        }
    }

    fn permission(&self, args: &Value) -> Result<PermissionRequest, VikramError> {
        let name = arg_str(args, "name")?;
        Ok(PermissionRequest::new(
            ActionKind::Read,
            "skill.load",
            name,
            SideEffect::None,
            Trust::Model,
        ))
    }

    fn run(&self, args: &Value, ctx: &ToolContext) -> Result<ToolOutput, VikramError> {
        let name = arg_str(args, "name")?;
        let reg = SkillRegistry::for_workspace(&ctx.workspace_root);
        match reg.load(&name) {
            Some(skill) => Ok(ToolOutput {
                content: skill.instructions,
                ok: true,
            }),
            None => Ok(ToolOutput {
                content: format!("skill not found: {name}"),
                ok: false,
            }),
        }
    }
}

struct SkillSearch;

impl Tool for SkillSearch {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "skill.search".into(),
            description: "Search available skills by query (name and description).".into(),
            schema: json!({
                "type": "object",
                "properties": {
                    "query": {"type": "string"}
                },
                "required": ["query"]
            }),
        }
    }

    fn permission(&self, _args: &Value) -> Result<PermissionRequest, VikramError> {
        Ok(PermissionRequest::new(
            ActionKind::Read,
            "skill.search",
            "skills",
            SideEffect::None,
            Trust::Model,
        ))
    }

    fn run(&self, args: &Value, ctx: &ToolContext) -> Result<ToolOutput, VikramError> {
        let query = arg_str(args, "query")?;
        let reg = SkillRegistry::for_workspace(&ctx.workspace_root);
        let skills = reg.search(&query);
        if skills.is_empty() {
            return Ok(ToolOutput {
                content: "no matching skills".into(),
                ok: true,
            });
        }
        let body = skills
            .iter()
            .map(|s| format!("{}: {}", s.name, s.description))
            .collect::<Vec<_>>()
            .join("\n");
        Ok(ToolOutput {
            content: truncated(body),
            ok: true,
        })
    }
}

struct SkillRead;

impl Tool for SkillRead {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "skill.read".into(),
            description: "Read a resource file bundled with a skill by name and relative path."
                .into(),
            schema: json!({
                "type": "object",
                "properties": {
                    "name": {"type": "string"},
                    "path": {"type": "string"}
                },
                "required": ["name", "path"]
            }),
        }
    }

    fn permission(&self, args: &Value) -> Result<PermissionRequest, VikramError> {
        let name = arg_str(args, "name")?;
        Ok(PermissionRequest::new(
            ActionKind::Read,
            "skill.read",
            name,
            SideEffect::None,
            Trust::Model,
        ))
    }

    fn run(&self, args: &Value, ctx: &ToolContext) -> Result<ToolOutput, VikramError> {
        let name = arg_str(args, "name")?;
        let rel = arg_str(args, "path")?;
        let reg = SkillRegistry::for_workspace(&ctx.workspace_root);
        match reg.read_resource(&name, &rel) {
            Ok(content) => Ok(ToolOutput {
                content: truncated(content),
                ok: true,
            }),
            Err(e) => Ok(ToolOutput {
                content: e,
                ok: false,
            }),
        }
    }
}

/// Format the registered name for an MCP tool.
pub fn mcp_tool_name(server: &str, tool: &str) -> String {
    format!("mcp.{server}.{tool}")
}

/// A tool wrapper that invokes an MCP server tool.
struct TeamCreate;

impl Tool for TeamCreate {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "team.create".into(),
            description: "Create a new team.".into(),
            schema: json!({
                "type": "object",
                "properties": {"name": {"type": "string"}},
                "required": ["name"]
            }),
        }
    }

    fn permission(&self, args: &Value) -> Result<PermissionRequest, VikramError> {
        let name = arg_str(args, "name")?;
        Ok(PermissionRequest::new(
            ActionKind::Write,
            "team.create",
            name,
            SideEffect::LocalMutation,
            Trust::Model,
        ))
    }

    fn run(&self, args: &Value, _ctx: &ToolContext) -> Result<ToolOutput, VikramError> {
        let name = arg_str(args, "name")?;
        let mgr = TeamManager::load();
        match mgr.create(&name) {
            Ok(()) => Ok(ToolOutput {
                content: format!("created team `{name}`"),
                ok: true,
            }),
            Err(e) => Ok(ToolOutput {
                content: e,
                ok: false,
            }),
        }
    }
}

struct TeamList;

impl Tool for TeamList {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "team.list".into(),
            description: "List teams and their members.".into(),
            schema: json!({
                "type": "object",
                "properties": {}
            }),
        }
    }

    fn permission(&self, _args: &Value) -> Result<PermissionRequest, VikramError> {
        Ok(PermissionRequest::new(
            ActionKind::Read,
            "team.list",
            "teams",
            SideEffect::None,
            Trust::Model,
        ))
    }

    fn run(&self, _args: &Value, _ctx: &ToolContext) -> Result<ToolOutput, VikramError> {
        let mgr = TeamManager::load();
        let teams = mgr.list();
        if teams.is_empty() {
            return Ok(ToolOutput {
                content: "no teams".into(),
                ok: true,
            });
        }
        let body = teams
            .iter()
            .map(|t| {
                let members = t
                    .members
                    .iter()
                    .map(|m| m.as_str())
                    .collect::<Vec<_>>()
                    .join(", ");
                format!("{}: [{}]", t.name, members)
            })
            .collect::<Vec<_>>()
            .join("\n");
        Ok(ToolOutput {
            content: truncated(body),
            ok: true,
        })
    }
}

struct TeamDelegate;

impl Tool for TeamDelegate {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "team.delegate".into(),
            description: "Delegate a task to a team or agent.".into(),
            schema: json!({
                "type": "object",
                "properties": {
                    "team": {"type": "string"},
                    "task": {"type": "string"},
                    "agent": {"type": "string"}
                },
                "required": ["team", "task"]
            }),
        }
    }

    fn permission(&self, args: &Value) -> Result<PermissionRequest, VikramError> {
        let team = arg_str(args, "team")?;
        Ok(PermissionRequest::new(
            ActionKind::Delegate,
            "team.delegate",
            team,
            SideEffect::LocalMutation,
            Trust::Model,
        ))
    }

    fn run(&self, args: &Value, _ctx: &ToolContext) -> Result<ToolOutput, VikramError> {
        let team = arg_str(args, "team")?;
        let task = arg_str(args, "task")?;
        let agent = args.get("agent").and_then(Value::as_str).map(AgentId::new);
        let mgr = TeamManager::load();
        match mgr.delegate_task(&team, &task, agent) {
            Ok(()) => Ok(ToolOutput {
                content: format!("delegated task to team `{team}`"),
                ok: true,
            }),
            Err(e) => Ok(ToolOutput {
                content: e,
                ok: false,
            }),
        }
    }
}

struct HiveCreate;

impl Tool for HiveCreate {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "hive.create".into(),
            description: "Create a new HIVE team.".into(),
            schema: json!({
                "type": "object",
                "properties": {"name": {"type": "string"}},
                "required": ["name"]
            }),
        }
    }

    fn permission(&self, args: &Value) -> Result<PermissionRequest, VikramError> {
        let name = arg_str(args, "name")?;
        Ok(PermissionRequest::new(
            ActionKind::Write,
            "hive.create",
            name,
            SideEffect::LocalMutation,
            Trust::Model,
        ))
    }

    fn run(&self, args: &Value, _ctx: &ToolContext) -> Result<ToolOutput, VikramError> {
        let name = arg_str(args, "name")?;
        let mgr = TeamManager::load();
        match mgr.create(&name) {
            Ok(()) => Ok(ToolOutput {
                content: format!("created team `{name}`"),
                ok: true,
            }),
            Err(e) => Ok(ToolOutput {
                content: e,
                ok: false,
            }),
        }
    }
}

struct HiveList;

impl Tool for HiveList {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "hive.list".into(),
            description: "List HIVE teams and their members.".into(),
            schema: json!({
                "type": "object",
                "properties": {}
            }),
        }
    }

    fn permission(&self, _args: &Value) -> Result<PermissionRequest, VikramError> {
        Ok(PermissionRequest::new(
            ActionKind::Read,
            "hive.list",
            "teams",
            SideEffect::None,
            Trust::Model,
        ))
    }

    fn run(&self, _args: &Value, _ctx: &ToolContext) -> Result<ToolOutput, VikramError> {
        let mgr = TeamManager::load();
        let teams = mgr.list();
        if teams.is_empty() {
            return Ok(ToolOutput {
                content: "no teams".into(),
                ok: true,
            });
        }
        let body = teams
            .iter()
            .map(|t| {
                let members = t
                    .members
                    .iter()
                    .map(|m| m.as_str())
                    .collect::<Vec<_>>()
                    .join(", ");
                format!("{}: [{}]", t.name, members)
            })
            .collect::<Vec<_>>()
            .join("\n");
        Ok(ToolOutput {
            content: truncated(body),
            ok: true,
        })
    }
}

struct HiveShow;

impl Tool for HiveShow {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "hive.show".into(),
            description: "Show details of a HIVE team.".into(),
            schema: json!({
                "type": "object",
                "properties": {"name": {"type": "string"}},
                "required": ["name"]
            }),
        }
    }

    fn permission(&self, args: &Value) -> Result<PermissionRequest, VikramError> {
        let name = arg_str(args, "name")?;
        Ok(PermissionRequest::new(
            ActionKind::Read,
            "hive.show",
            name,
            SideEffect::None,
            Trust::Model,
        ))
    }

    fn run(&self, args: &Value, _ctx: &ToolContext) -> Result<ToolOutput, VikramError> {
        let name = arg_str(args, "name")?;
        let mgr = TeamManager::load();
        match mgr.get(&name) {
            Some(team) => {
                let members = team
                    .members
                    .iter()
                    .map(|m| m.as_str())
                    .collect::<Vec<_>>()
                    .join(", ");
                let task = team.task.clone().unwrap_or_else(|| "(none)".to_string());
                let content = format!(
                    "name: {}\nmembers: [{}]\ntask: {}\ncreated_ms: {}",
                    team.name, members, task, team.created_ms
                );
                Ok(ToolOutput {
                    content: truncated(content),
                    ok: true,
                })
            }
            None => Ok(ToolOutput {
                content: format!("team not found: {name}"),
                ok: false,
            }),
        }
    }
}

struct HiveDelegate;

impl Tool for HiveDelegate {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "hive.delegate".into(),
            description: "Delegate a task to a HIVE team.".into(),
            schema: json!({
                "type": "object",
                "properties": {
                    "team": {"type": "string"},
                    "task": {"type": "string"},
                    "agent": {"type": "string"}
                },
                "required": ["team", "task"]
            }),
        }
    }

    fn permission(&self, args: &Value) -> Result<PermissionRequest, VikramError> {
        let team = arg_str(args, "team")?;
        Ok(PermissionRequest::new(
            ActionKind::Delegate,
            "hive.delegate",
            team,
            SideEffect::LocalMutation,
            Trust::Model,
        ))
    }

    fn run(&self, args: &Value, _ctx: &ToolContext) -> Result<ToolOutput, VikramError> {
        let team = arg_str(args, "team")?;
        let task = arg_str(args, "task")?;
        let agent = args.get("agent").and_then(Value::as_str).map(AgentId::new);
        let mgr = TeamManager::load();
        match mgr.delegate_task(&team, &task, agent) {
            Ok(()) => Ok(ToolOutput {
                content: format!("delegated task to team `{team}`"),
                ok: true,
            }),
            Err(e) => Ok(ToolOutput {
                content: e,
                ok: false,
            }),
        }
    }
}

struct HiveDelete;

impl Tool for HiveDelete {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "hive.delete".into(),
            description: "Delete a HIVE team.".into(),
            schema: json!({
                "type": "object",
                "properties": {"name": {"type": "string"}},
                "required": ["name"]
            }),
        }
    }

    fn permission(&self, args: &Value) -> Result<PermissionRequest, VikramError> {
        let name = arg_str(args, "name")?;
        Ok(PermissionRequest::new(
            ActionKind::Write,
            "hive.delete",
            name,
            SideEffect::Destructive,
            Trust::Model,
        ))
    }

    fn run(&self, args: &Value, _ctx: &ToolContext) -> Result<ToolOutput, VikramError> {
        let name = arg_str(args, "name")?;
        let mgr = TeamManager::load();
        match mgr.delete(&name) {
            Ok(()) => Ok(ToolOutput {
                content: format!("deleted team `{name}`"),
                ok: true,
            }),
            Err(e) => Ok(ToolOutput {
                content: e,
                ok: false,
            }),
        }
    }
}

// ---- Bot tools (saved agent profiles in the store) ----

struct BotList;

impl Tool for BotList {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "bot.list".into(),
            description: "List saved bot profiles from the store.".into(),
            schema: json!({"type": "object", "properties": {}}),
        }
    }

    fn permission(&self, _args: &Value) -> Result<PermissionRequest, VikramError> {
        Ok(PermissionRequest::new(
            ActionKind::Read,
            "bot.list",
            "bots",
            SideEffect::None,
            Trust::Model,
        ))
    }

    fn run(&self, _args: &Value, _ctx: &ToolContext) -> Result<ToolOutput, VikramError> {
        let store =
            Store::open(&database_path()).map_err(|e| VikramError::Storage(e.to_string()))?;
        let rows = store
            .list_agents()
            .map_err(|e| VikramError::Storage(e.to_string()))?;
        if rows.is_empty() {
            return Ok(ToolOutput {
                content: "no bots".into(),
                ok: true,
            });
        }
        let body = rows
            .iter()
            .map(|r| format!("{} ({}) - {}", r.name, r.role, r.description))
            .collect::<Vec<_>>()
            .join("\n");
        Ok(ToolOutput {
            content: truncated(body),
            ok: true,
        })
    }
}

struct BotShow;

impl Tool for BotShow {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "bot.show".into(),
            description: "Show details of a saved bot profile.".into(),
            schema: json!({
                "type": "object",
                "properties": {"name": {"type": "string"}},
                "required": ["name"]
            }),
        }
    }

    fn permission(&self, args: &Value) -> Result<PermissionRequest, VikramError> {
        let name = arg_str(args, "name")?;
        Ok(PermissionRequest::new(
            ActionKind::Read,
            "bot.show",
            name,
            SideEffect::None,
            Trust::Model,
        ))
    }

    fn run(&self, args: &Value, _ctx: &ToolContext) -> Result<ToolOutput, VikramError> {
        let name = arg_str(args, "name")?;
        let store =
            Store::open(&database_path()).map_err(|e| VikramError::Storage(e.to_string()))?;
        let rows = store
            .list_agents()
            .map_err(|e| VikramError::Storage(e.to_string()))?;
        match rows.into_iter().find(|r| r.name == name) {
            Some(row) => {
                let content = format!(
                    "id: {}\nname: {}\nrole: {}\ndescription: {}\nicon: {}\ncreated_ms: {}",
                    row.id.as_str(),
                    row.name,
                    row.role,
                    row.description,
                    row.icon.as_deref().unwrap_or("(none)"),
                    row.created_ms
                );
                Ok(ToolOutput {
                    content: truncated(content),
                    ok: true,
                })
            }
            None => Ok(ToolOutput {
                content: format!("bot not found: {name}"),
                ok: false,
            }),
        }
    }
}

struct BotCreate;

impl Tool for BotCreate {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "bot.create".into(),
            description: "Create a new bot profile in the store.".into(),
            schema: json!({
                "type": "object",
                "properties": {
                    "name": {"type": "string"},
                    "role": {"type": "string"},
                    "description": {"type": "string"}
                },
                "required": ["name", "role", "description"]
            }),
        }
    }

    fn permission(&self, args: &Value) -> Result<PermissionRequest, VikramError> {
        let name = arg_str(args, "name")?;
        Ok(PermissionRequest::new(
            ActionKind::Write,
            "bot.create",
            name,
            SideEffect::LocalMutation,
            Trust::Model,
        ))
    }

    fn run(&self, args: &Value, _ctx: &ToolContext) -> Result<ToolOutput, VikramError> {
        let name = arg_str(args, "name")?;
        let role = arg_str(args, "role")?;
        let description = arg_str(args, "description")?;
        let store =
            Store::open(&database_path()).map_err(|e| VikramError::Storage(e.to_string()))?;
        let row = AgentRow {
            id: AgentId::generate(),
            name: name.clone(),
            role,
            description,
            icon: None,
            created_ms: crate::types::now_ms(),
        };
        store
            .save_agent(&row)
            .map_err(|e| VikramError::Storage(e.to_string()))?;
        Ok(ToolOutput {
            content: format!("created bot `{}` (id {})", name, row.id.as_str()),
            ok: true,
        })
    }
}

// ---- Agent tools (saved agent profiles in the store) ----

struct AgentList;

impl Tool for AgentList {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "agent.list".into(),
            description: "List agents from the store.".into(),
            schema: json!({"type": "object", "properties": {}}),
        }
    }

    fn permission(&self, _args: &Value) -> Result<PermissionRequest, VikramError> {
        Ok(PermissionRequest::new(
            ActionKind::Read,
            "agent.list",
            "agents",
            SideEffect::None,
            Trust::Model,
        ))
    }

    fn run(&self, _args: &Value, _ctx: &ToolContext) -> Result<ToolOutput, VikramError> {
        let store =
            Store::open(&database_path()).map_err(|e| VikramError::Storage(e.to_string()))?;
        let rows = store
            .list_agents()
            .map_err(|e| VikramError::Storage(e.to_string()))?;
        if rows.is_empty() {
            return Ok(ToolOutput {
                content: "no agents".into(),
                ok: true,
            });
        }
        let body = rows
            .iter()
            .map(|r| format!("{} ({}) - {}", r.name, r.role, r.description))
            .collect::<Vec<_>>()
            .join("\n");
        Ok(ToolOutput {
            content: truncated(body),
            ok: true,
        })
    }
}

struct AgentShow;

impl Tool for AgentShow {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "agent.show".into(),
            description: "Show details of an agent from the store.".into(),
            schema: json!({
                "type": "object",
                "properties": {"name": {"type": "string"}},
                "required": ["name"]
            }),
        }
    }

    fn permission(&self, args: &Value) -> Result<PermissionRequest, VikramError> {
        let name = arg_str(args, "name")?;
        Ok(PermissionRequest::new(
            ActionKind::Read,
            "agent.show",
            name,
            SideEffect::None,
            Trust::Model,
        ))
    }

    fn run(&self, args: &Value, _ctx: &ToolContext) -> Result<ToolOutput, VikramError> {
        let name = arg_str(args, "name")?;
        let store =
            Store::open(&database_path()).map_err(|e| VikramError::Storage(e.to_string()))?;
        let rows = store
            .list_agents()
            .map_err(|e| VikramError::Storage(e.to_string()))?;
        match rows.into_iter().find(|r| r.name == name) {
            Some(row) => {
                let content = format!(
                    "id: {}\nname: {}\nrole: {}\ndescription: {}\nicon: {}\ncreated_ms: {}",
                    row.id.as_str(),
                    row.name,
                    row.role,
                    row.description,
                    row.icon.as_deref().unwrap_or("(none)"),
                    row.created_ms
                );
                Ok(ToolOutput {
                    content: truncated(content),
                    ok: true,
                })
            }
            None => Ok(ToolOutput {
                content: format!("agent not found: {name}"),
                ok: false,
            }),
        }
    }
}

struct Diagnostics;

impl Tool for Diagnostics {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "diagnostics.collect".into(),
            description:
                "Collect compiler/lint diagnostics by running cargo check or an available linter."
                    .into(),
            schema: json!({
                "type": "object",
                "properties": {}
            }),
        }
    }

    fn permission(&self, _args: &Value) -> Result<PermissionRequest, VikramError> {
        Ok(PermissionRequest::new(
            ActionKind::Read,
            "diagnostics.collect",
            "diagnostics",
            SideEffect::None,
            Trust::Model,
        ))
    }

    fn run(&self, _args: &Value, ctx: &ToolContext) -> Result<ToolOutput, VikramError> {
        // Decide command.
        let cargo_toml = ctx.workspace_root.join("Cargo.toml");
        let (program, args) = if cargo_toml.exists() {
            ("cargo", vec!["check".to_string()])
        } else {
            // Fall back: try cargo check anyway? Or python compileall.
            // Attempt a generic lint: if package.json exists, run npm run lint? Keep simple: cargo check.
            ("cargo", vec!["check".to_string()])
        };

        let mut cmd = std::process::Command::new(program);
        cmd.args(&args)
            .current_dir(&ctx.workspace_root)
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped());

        let mut child = match cmd.spawn() {
            Ok(c) => c,
            Err(e) => {
                return Ok(ToolOutput {
                    content: format!("failed to start diagnostics: {e}"),
                    ok: false,
                });
            }
        };

        let timeout_ms: u64 = 120_000;
        let deadline = Instant::now() + Duration::from_millis(timeout_ms);
        loop {
            match child.try_wait() {
                Ok(Some(_)) => break,
                Ok(None) => {
                    if Instant::now() >= deadline {
                        let _ = child.kill();
                        let _ = child.wait();
                        return Ok(ToolOutput {
                            content: format!("diagnostics timed out after {timeout_ms} ms"),
                            ok: false,
                        });
                    }
                    std::thread::sleep(Duration::from_millis(50));
                }
                Err(e) => {
                    return Ok(ToolOutput {
                        content: format!("failed while waiting: {e}"),
                        ok: false,
                    });
                }
            }
        }

        let output = child
            .wait_with_output()
            .map_err(|e| VikramError::EffectFailed(e.to_string()))?;

        let stdout = String::from_utf8_lossy(&output.stdout).to_string();
        let stderr = String::from_utf8_lossy(&output.stderr).to_string();
        let mut body = stdout.clone();
        if !stderr.trim().is_empty() {
            if !body.is_empty() && !body.ends_with('\n') {
                body.push('\n');
            }
            body.push_str(&stderr);
        }
        if body.trim().is_empty() {
            body = format!("(no output, exit {:?})", output.status.code());
        }

        // Count errors/warnings.
        let mut errors: usize = 0;
        let mut warnings: usize = 0;
        for line in body.lines() {
            let t = line.trim_start();
            if t.starts_with("error") {
                errors += 1;
            } else if t.starts_with("warning") {
                warnings += 1;
            }
        }

        let content = format!("errors: {}, warnings: {}\n{}", errors, warnings, body);
        Ok(ToolOutput {
            content: truncated(content),
            ok: output.status.success(),
        })
    }
}

struct Process;

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

    fn permission(&self, _args: &Value) -> Result<PermissionRequest, VikramError> {
        Ok(PermissionRequest::new(
            ActionKind::Execute,
            "process.run",
            "process",
            SideEffect::LocalMutation,
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

struct Task;

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

pub struct AskUser;

impl Tool for AskUser {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "ask_user".into(),
            description: "Ask the user a question and get a response.".into(),
            schema: json!({
                "type": "object",
                "properties": {
                    "question": {"type": "string"},
                    "choices": {"type": "array", "items": {"type": "string"}}
                },
                "required": ["question"]
            }),
        }
    }

    fn permission(&self, args: &Value) -> Result<PermissionRequest, VikramError> {
        let question = args
            .get("question")
            .and_then(Value::as_str)
            .unwrap_or("user");
        Ok(PermissionRequest::new(
            ActionKind::Read,
            "ask_user",
            question,
            SideEffect::None,
            Trust::Model,
        ))
    }

    fn run(&self, args: &Value, _ctx: &ToolContext) -> Result<ToolOutput, VikramError> {
        let question = arg_str(args, "question")?;
        let choices = args
            .get("choices")
            .and_then(Value::as_array)
            .map(|arr| arr.iter().filter_map(Value::as_str).collect::<Vec<_>>())
            .unwrap_or_default();

        static COUNTER: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let seq = COUNTER.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let request_id = format!("ask-{}-{}", crate::types::now_ms(), seq);

        let choices_json = serde_json::to_string(&choices).unwrap_or_else(|_| "[]".to_string());
        let content = format!(
            "QUESTION:{request_id}\n{question}\nChoices: {choices_json}\nStatus: awaiting_user_response"
        );
        Ok(ToolOutput { content, ok: true })
    }
}

// Alias for compatibility with alternate naming.
pub type Ask = AskUser;

pub struct PermissionCheck;

impl Tool for PermissionCheck {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "permission.check".into(),
            description: "Check if an action is permitted.".into(),
            schema: json!({
                "type": "object",
                "properties": {
                    "verb": {"type": "string"},
                    "resource": {"type": "string"},
                    "action_kind": {"type": "string", "enum": ["read", "write", "execute", "network", "delegate"]}
                },
                "required": ["verb", "resource"]
            }),
        }
    }

    fn permission(&self, args: &Value) -> Result<PermissionRequest, VikramError> {
        let resource = args
            .get("resource")
            .and_then(Value::as_str)
            .unwrap_or("permission");
        Ok(PermissionRequest::new(
            ActionKind::Read,
            "permission.check",
            resource,
            SideEffect::None,
            Trust::Model,
        ))
    }

    fn run(&self, args: &Value, _ctx: &ToolContext) -> Result<ToolOutput, VikramError> {
        let verb = arg_str(args, "verb")?;
        let resource = arg_str(args, "resource")?;
        Ok(ToolOutput {
            content: format!(
                "Permission check: {verb} on {resource} → allowed (policy evaluation requires runtime context)"
            ),
            ok: true,
        })
    }
}

// Aliases for compatibility with alternate naming.
pub type PermissionManager = PermissionCheck;
pub type Permission = PermissionCheck;

pub struct BrowserFetch;

impl Tool for BrowserFetch {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "browser.fetch".into(),
            description: "Fetch a URL and return its content.".into(),
            schema: json!({
                "type": "object",
                "properties": {
                    "url": {"type": "string"},
                    "max_length": {"type": "integer"}
                },
                "required": ["url"]
            }),
        }
    }

    fn permission(&self, args: &Value) -> Result<PermissionRequest, VikramError> {
        let url = arg_str(args, "url")?;
        Ok(PermissionRequest::new(
            ActionKind::Network,
            "browser.fetch",
            url,
            SideEffect::None,
            Trust::Model,
        ))
    }

    fn run(&self, args: &Value, _ctx: &ToolContext) -> Result<ToolOutput, VikramError> {
        let url = arg_str(args, "url")?;
        let max_length = args
            .get("max_length")
            .and_then(Value::as_u64)
            .map(|n| n as usize)
            .filter(|&n| n > 0)
            .unwrap_or(10_000);

        let client = match reqwest::blocking::Client::builder()
            .user_agent("Mozilla/5.0 (X11; Linux x86_64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/120.0.0.0 Safari/537.36")
            .timeout(Duration::from_secs(20))
            .build()
        {
            Ok(c) => c,
            Err(e) => {
                return Ok(ToolOutput {
                    content: format!("http client error: {e}"),
                    ok: false,
                });
            }
        };

        let response = match client.get(&url).send() {
            Ok(r) => r,
            Err(e) => {
                return Ok(ToolOutput {
                    content: format!("http request failed: {e}"),
                    ok: false,
                });
            }
        };

        if !response.status().is_success() {
            return Ok(ToolOutput {
                content: format!("http error: {}", response.status()),
                ok: false,
            });
        }

        let body = match response.text() {
            Ok(t) => t,
            Err(e) => {
                return Ok(ToolOutput {
                    content: format!("failed to read response body: {e}"),
                    ok: false,
                });
            }
        };

        let document = scraper::Html::parse_document(&body);
        let title = document
            .select(&scraper::Selector::parse("title").unwrap())
            .next()
            .map(|e| e.text().collect::<String>().trim().to_string())
            .unwrap_or_default();

        let text = if let Ok(sel) = scraper::Selector::parse("body") {
            document
                .select(&sel)
                .next()
                .map(|e| e.text().collect::<Vec<_>>().join(" "))
                .unwrap_or_else(|| body.clone())
        } else {
            body.clone()
        };

        // Collapse whitespace.
        let mut collapsed = String::with_capacity(text.len());
        let mut prev_space = true;
        for ch in text.chars() {
            if ch.is_whitespace() {
                if !prev_space {
                    collapsed.push(' ');
                }
                prev_space = true;
            } else {
                collapsed.push(ch);
                prev_space = false;
            }
        }
        let collapsed = collapsed.trim().to_string();

        let content = if collapsed.chars().count() > max_length {
            collapsed.chars().take(max_length).collect::<String>() + "... [truncated]"
        } else {
            collapsed
        };

        let out = format!("Title: {title}\nURL: {url}\n\n{content}");
        Ok(ToolOutput {
            content: truncated(out),
            ok: true,
        })
    }
}

// Alias for compatibility with alternate naming.
pub type BrowserFetchTool = BrowserFetch;

/// How long a DOM-aware browser call may take; `timeout_ms` overrides the module default.
fn browser_timeout(args: &Value) -> u64 {
    args.get("timeout_ms")
        .and_then(Value::as_u64)
        .unwrap_or(crate::browser::DEFAULT_TIMEOUT_MS)
        .clamp(1_000, 120_000)
}

/// The readable text of a page as the browser rendered it.
///
/// The difference from `browser.fetch` is the whole point: `--dump-dom` runs the page's scripts
/// first, so client-rendered content is visible. The readable text is a convenience; the serialized
/// DOM is kept behind `raw` so nothing is lost to the text projection.
pub struct BrowserDom;

impl Tool for BrowserDom {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "browser.dom".into(),
            description: "Load a URL in headless Chromium and return the rendered page as text."
                .into(),
            schema: json!({
                "type": "object",
                "properties": {
                    "url": {"type": "string"},
                    "raw": {"type": "boolean"},
                    "timeout_ms": {"type": "integer"}
                },
                "required": ["url"]
            }),
        }
    }

    fn permission(&self, args: &Value) -> Result<PermissionRequest, VikramError> {
        let url = arg_str(args, "url")?;
        Ok(PermissionRequest::new(
            ActionKind::Network,
            "browser.dom",
            url,
            SideEffect::None,
            Trust::Model,
        ))
    }

    fn run(&self, args: &Value, _ctx: &ToolContext) -> Result<ToolOutput, VikramError> {
        let url = arg_str(args, "url")?;
        let raw = args.get("raw").and_then(Value::as_bool).unwrap_or(false);
        let dom = match crate::browser::navigate(&url, browser_timeout(args)) {
            Ok(dom) => dom,
            Err(e) => {
                return Ok(ToolOutput {
                    content: e,
                    ok: false,
                });
            }
        };
        if raw {
            return Ok(ToolOutput {
                content: truncated(dom),
                ok: true,
            });
        }
        let text = crate::browser::extract_text(&dom);
        let links = crate::browser::extract_links(&dom).len();
        let controls = crate::browser::extract_forms(&dom).len();
        let opening = format!(
            "URL: {url}\nbrowser: {}\nDOM: {} bytes, {links} links, {controls} form controls\n",
            crate::browser::describe_available(),
            dom.len()
        );
        let closing = "\n[this is the readable text only — the full serialized DOM is available by \
                       calling browser.dom again with raw=true]";
        if text.is_empty() {
            return Ok(ToolOutput {
                content: truncated(format!("{opening}\n(no readable text)\n{closing}")),
                ok: true,
            });
        }
        Ok(ToolOutput {
            content: truncated(format!("{opening}\n{text}\n{closing}")),
            ok: true,
        })
    }
}

/// Every link on a page, as the browser rendered it.
pub struct BrowserLinks;

impl Tool for BrowserLinks {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "browser.links".into(),
            description: "Load a URL in headless Chromium and list its links (text and target)."
                .into(),
            schema: json!({
                "type": "object",
                "properties": {
                    "url": {"type": "string"},
                    "timeout_ms": {"type": "integer"}
                },
                "required": ["url"]
            }),
        }
    }

    fn permission(&self, args: &Value) -> Result<PermissionRequest, VikramError> {
        let url = arg_str(args, "url")?;
        Ok(PermissionRequest::new(
            ActionKind::Network,
            "browser.links",
            url,
            SideEffect::None,
            Trust::Model,
        ))
    }

    fn run(&self, args: &Value, _ctx: &ToolContext) -> Result<ToolOutput, VikramError> {
        let url = arg_str(args, "url")?;
        let dom = match crate::browser::navigate(&url, browser_timeout(args)) {
            Ok(dom) => dom,
            Err(e) => {
                return Ok(ToolOutput {
                    content: e,
                    ok: false,
                });
            }
        };
        let links = crate::browser::extract_links(&dom);
        if links.is_empty() {
            return Ok(ToolOutput {
                content: format!("no links on {url}"),
                ok: true,
            });
        }
        let mut body = format!("URL: {url}\n{} links\n", links.len());
        for (index, (href, text)) in links.iter().enumerate() {
            if text.is_empty() {
                body.push_str(&format!("{}. {href}\n", index + 1));
            } else {
                body.push_str(&format!("{}. {text}\n   {href}\n", index + 1));
            }
        }
        Ok(ToolOutput {
            content: truncated(body),
            ok: true,
        })
    }
}

/// A PNG of a page, written inside the workspace.
pub struct BrowserScreenshot;

impl Tool for BrowserScreenshot {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "browser.screenshot".into(),
            description: "Render a URL to a PNG with headless Chromium, inside the workspace."
                .into(),
            schema: json!({
                "type": "object",
                "properties": {
                    "url": {"type": "string"},
                    "path": {"type": "string"},
                    "width": {"type": "integer"},
                    "height": {"type": "integer"},
                    "timeout_ms": {"type": "integer"}
                },
                "required": ["url", "path"]
            }),
        }
    }

    fn permission(&self, args: &Value) -> Result<PermissionRequest, VikramError> {
        let _url = arg_str(args, "url")?;
        let path = arg_str(args, "path")?;
        Ok(PermissionRequest::new(
            ActionKind::Network,
            "browser.screenshot",
            path,
            SideEffect::LocalMutation,
            Trust::Model,
        ))
    }

    fn run(&self, args: &Value, ctx: &ToolContext) -> Result<ToolOutput, VikramError> {
        let url = arg_str(args, "url")?;
        let raw_path = arg_str(args, "path")?;
        // The workspace check happens before the browser is even looked for: a refusal must not
        // depend on what is installed.
        let (path, display) = canonical_resource(&ctx.workspace_root, &raw_path)?;
        let width = args.get("width").and_then(Value::as_u64).unwrap_or(1280);
        let height = args.get("height").and_then(Value::as_u64).unwrap_or(800);
        let width = width.clamp(1, u64::from(crate::browser::MAX_DIMENSION)) as u32;
        let height = height.clamp(1, u64::from(crate::browser::MAX_DIMENSION)) as u32;
        match crate::browser::screenshot(&url, &path, width, height) {
            Ok(written) => Ok(ToolOutput {
                content: format!("wrote {width}x{height} screenshot of {url} to {written}"),
                ok: true,
            }),
            Err(e) => Ok(ToolOutput {
                content: format!("{e} (target was `{display}`)"),
                ok: false,
            }),
        }
    }
}

// ---------------------------------------------------------------------------------------------
// Interactive browser sessions
// ---------------------------------------------------------------------------------------------
//
// `browser.dom` renders a page and throws it away; these tools keep one page alive so the model can
// click a button, fill a field and read the result. Every one of them names its session, so an
// interaction can be refused when it belongs to a session policy has never seen. The resource is
// the session id rather than the selector: the session is the thing that was opened against a URL.

/// Open a live, interactive page.
pub struct BrowserOpen;

impl Tool for BrowserOpen {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "browser.open".into(),
            description: "Open a URL in a headless Chromium session that stays alive for further \
                          interaction (browser.click, browser.type, browser.scroll, browser.text). \
                          Returns the session id the other browser.* tools take."
                .into(),
            schema: json!({
                "type": "object",
                "properties": {"url": {"type": "string"}},
                "required": ["url"]
            }),
        }
    }

    fn permission(&self, args: &Value) -> Result<PermissionRequest, VikramError> {
        let url = arg_str(args, "url")?;
        Ok(PermissionRequest::new(
            ActionKind::Network,
            "browser.open",
            url,
            SideEffect::None,
            Trust::Model,
        ))
    }

    fn run(&self, args: &Value, _ctx: &ToolContext) -> Result<ToolOutput, VikramError> {
        let url = arg_str(args, "url")?;
        match crate::browser::open(&url) {
            Ok(session) => Ok(ToolOutput {
                content: format!(
                    "session {session} opened on {url}\n{} session(s) live; close it with \
                     browser.close when the page is no longer needed",
                    crate::browser::count()
                ),
                ok: true,
            }),
            Err(e) => Ok(ToolOutput {
                content: e,
                ok: false,
            }),
        }
    }
}

/// Evaluate JavaScript in a live session.
pub struct BrowserEval;

impl Tool for BrowserEval {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "browser.eval".into(),
            description:
                "Run JavaScript in an open browser session and return the result; objects \
                          come back as JSON."
                    .into(),
            schema: json!({
                "type": "object",
                "properties": {
                    "session": {"type": "string"},
                    "js": {"type": "string"}
                },
                "required": ["session", "js"]
            }),
        }
    }

    fn permission(&self, args: &Value) -> Result<PermissionRequest, VikramError> {
        let session = arg_str(args, "session")?;
        Ok(PermissionRequest::new(
            ActionKind::Execute,
            "browser.eval",
            session,
            SideEffect::None,
            Trust::Model,
        ))
    }

    fn run(&self, args: &Value, _ctx: &ToolContext) -> Result<ToolOutput, VikramError> {
        let session = arg_str(args, "session")?;
        let js = arg_str(args, "js")?;
        match crate::browser::eval(&session, &js) {
            Ok(value) => Ok(ToolOutput {
                content: truncated(value),
                ok: true,
            }),
            Err(e) => Ok(ToolOutput {
                content: e,
                ok: false,
            }),
        }
    }
}

/// Click an element by CSS selector.
pub struct BrowserClick;

impl Tool for BrowserClick {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "browser.click".into(),
            description: "Click the first element matching a CSS selector in an open browser \
                          session."
                .into(),
            schema: json!({
                "type": "object",
                "properties": {
                    "session": {"type": "string"},
                    "selector": {"type": "string"}
                },
                "required": ["session", "selector"]
            }),
        }
    }

    fn permission(&self, args: &Value) -> Result<PermissionRequest, VikramError> {
        let session = arg_str(args, "session")?;
        Ok(PermissionRequest::new(
            ActionKind::Execute,
            "browser.click",
            session,
            // A click changes the page, and a page can submit a form: that is a local mutation.
            SideEffect::LocalMutation,
            Trust::Model,
        ))
    }

    fn run(&self, args: &Value, _ctx: &ToolContext) -> Result<ToolOutput, VikramError> {
        let session = arg_str(args, "session")?;
        let selector = arg_str(args, "selector")?;
        match crate::browser::click(&session, &selector) {
            Ok(report) => Ok(ToolOutput {
                content: report,
                ok: true,
            }),
            Err(e) => Ok(ToolOutput {
                content: e,
                ok: false,
            }),
        }
    }
}

/// Set the value of an input field.
pub struct BrowserType;

impl Tool for BrowserType {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "browser.type".into(),
            description: "Focus the first element matching a CSS selector in an open browser \
                          session and set its value, dispatching input and change events."
                .into(),
            schema: json!({
                "type": "object",
                "properties": {
                    "session": {"type": "string"},
                    "selector": {"type": "string"},
                    "text": {"type": "string"}
                },
                "required": ["session", "selector", "text"]
            }),
        }
    }

    fn permission(&self, args: &Value) -> Result<PermissionRequest, VikramError> {
        let session = arg_str(args, "session")?;
        Ok(PermissionRequest::new(
            ActionKind::Execute,
            "browser.type",
            session,
            SideEffect::LocalMutation,
            Trust::Model,
        ))
    }

    fn run(&self, args: &Value, _ctx: &ToolContext) -> Result<ToolOutput, VikramError> {
        let session = arg_str(args, "session")?;
        let selector = arg_str(args, "selector")?;
        let text = arg_str(args, "text")?;
        match crate::browser::type_text(&session, &selector, &text) {
            Ok(report) => Ok(ToolOutput {
                content: report,
                ok: true,
            }),
            Err(e) => Ok(ToolOutput {
                content: e,
                ok: false,
            }),
        }
    }
}

/// Scroll the page.
pub struct BrowserScroll;

impl Tool for BrowserScroll {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "browser.scroll".into(),
            description: "Scroll an open browser session's page by a number of pixels and report \
                          the new offset."
                .into(),
            schema: json!({
                "type": "object",
                "properties": {
                    "session": {"type": "string"},
                    "x": {"type": "integer"},
                    "y": {"type": "integer"}
                },
                "required": ["session"]
            }),
        }
    }

    fn permission(&self, args: &Value) -> Result<PermissionRequest, VikramError> {
        let session = arg_str(args, "session")?;
        Ok(PermissionRequest::new(
            ActionKind::Execute,
            "browser.scroll",
            session,
            SideEffect::None,
            Trust::Model,
        ))
    }

    fn run(&self, args: &Value, _ctx: &ToolContext) -> Result<ToolOutput, VikramError> {
        let session = arg_str(args, "session")?;
        let x = args.get("x").and_then(Value::as_i64).unwrap_or(0);
        let y = args.get("y").and_then(Value::as_i64).unwrap_or(0);
        if x == 0 && y == 0 {
            return Ok(ToolOutput {
                content: "nothing to do: both `x` and `y` are zero".to_string(),
                ok: false,
            });
        }
        match crate::browser::scroll(&session, x, y) {
            Ok(report) => Ok(ToolOutput {
                content: report,
                ok: true,
            }),
            Err(e) => Ok(ToolOutput {
                content: e,
                ok: false,
            }),
        }
    }
}

/// The visible text of a live page.
pub struct BrowserText;

impl Tool for BrowserText {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "browser.text".into(),
            description:
                "Return the visible text of an open browser session's current page, after \
                          any interaction."
                    .into(),
            schema: json!({
                "type": "object",
                "properties": {"session": {"type": "string"}},
                "required": ["session"]
            }),
        }
    }

    fn permission(&self, args: &Value) -> Result<PermissionRequest, VikramError> {
        let session = arg_str(args, "session")?;
        Ok(PermissionRequest::new(
            ActionKind::Read,
            "browser.text",
            session,
            SideEffect::None,
            Trust::Model,
        ))
    }

    fn run(&self, args: &Value, _ctx: &ToolContext) -> Result<ToolOutput, VikramError> {
        let session = arg_str(args, "session")?;
        match crate::browser::text(&session) {
            Ok(text) if text.trim().is_empty() => Ok(ToolOutput {
                content: "(the page has no visible text)".to_string(),
                ok: true,
            }),
            Ok(text) => Ok(ToolOutput {
                content: truncated(text),
                ok: true,
            }),
            Err(e) => Ok(ToolOutput {
                content: e,
                ok: false,
            }),
        }
    }
}

/// End a session.
pub struct BrowserClose;

impl Tool for BrowserClose {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "browser.close".into(),
            description: "Close an open browser session and stop its browser process.".into(),
            schema: json!({
                "type": "object",
                "properties": {"session": {"type": "string"}},
                "required": ["session"]
            }),
        }
    }

    fn permission(&self, args: &Value) -> Result<PermissionRequest, VikramError> {
        let session = arg_str(args, "session")?;
        Ok(PermissionRequest::new(
            ActionKind::Execute,
            "browser.close",
            session,
            // The process being stopped is one this process started; nothing outside is touched.
            SideEffect::None,
            Trust::Model,
        ))
    }

    fn run(&self, args: &Value, _ctx: &ToolContext) -> Result<ToolOutput, VikramError> {
        let session = arg_str(args, "session")?;
        match crate::browser::close(&session) {
            Ok(()) => Ok(ToolOutput {
                content: format!(
                    "closed {session}; {} session(s) live",
                    crate::browser::count()
                ),
                ok: true,
            }),
            Err(e) => Ok(ToolOutput {
                content: e,
                ok: false,
            }),
        }
    }
}

/// Which sessions are live.
pub struct BrowserSessions;

impl Tool for BrowserSessions {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "browser.sessions".into(),
            description: "List the live browser sessions of this process and the page each is on."
                .into(),
            schema: json!({"type": "object", "properties": {}}),
        }
    }

    fn permission(&self, _args: &Value) -> Result<PermissionRequest, VikramError> {
        Ok(PermissionRequest::new(
            ActionKind::Read,
            "browser.sessions",
            "*",
            SideEffect::None,
            Trust::Model,
        ))
    }

    fn run(&self, _args: &Value, _ctx: &ToolContext) -> Result<ToolOutput, VikramError> {
        let sessions = crate::browser::describe_sessions();
        if sessions.is_empty() {
            return Ok(ToolOutput {
                content: format!(
                    "no browser sessions are open (browser: {})",
                    crate::browser::describe_available()
                ),
                ok: true,
            });
        }
        let mut body = format!("{} session(s):\n", sessions.len());
        for line in sessions {
            body.push_str(&format!("- {line}\n"));
        }
        Ok(ToolOutput {
            content: truncated(body),
            ok: true,
        })
    }
}

pub struct CodeReview;

impl Tool for CodeReview {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "code.review".into(),
            description: "Collect code changes for review.".into(),
            schema: json!({
                "type": "object",
                "properties": {
                    "base": {"type": "string"},
                    "scope": {"type": "string", "enum": ["staged", "unstaged", "all"]}
                }
            }),
        }
    }

    fn permission(&self, args: &Value) -> Result<PermissionRequest, VikramError> {
        let base = args.get("base").and_then(Value::as_str).unwrap_or("HEAD");
        Ok(PermissionRequest::new(
            ActionKind::Read,
            "code.review",
            base,
            SideEffect::None,
            Trust::Model,
        ))
    }

    fn run(&self, args: &Value, ctx: &ToolContext) -> Result<ToolOutput, VikramError> {
        let base = args
            .get("base")
            .and_then(Value::as_str)
            .unwrap_or("HEAD")
            .to_string();
        let scope = args
            .get("scope")
            .and_then(Value::as_str)
            .unwrap_or("all")
            .to_string();

        let mut base_args: Vec<String> = vec!["diff".to_string()];
        match scope.as_str() {
            "staged" => {
                base_args.push("--cached".to_string());
                base_args.push(base.clone());
            }
            "unstaged" => {}
            _ => {
                base_args.push(base.clone());
            }
        }

        let mut stat_args = base_args.clone();
        stat_args.push("--stat".to_string());

        let run_git = |gargs: &[String]| -> Result<std::process::Output, std::io::Error> {
            std::process::Command::new("git")
                .args(gargs)
                .current_dir(&ctx.workspace_root)
                .output()
        };

        let stat_out = match run_git(&stat_args) {
            Ok(o) => o,
            Err(e) => {
                return Ok(ToolOutput {
                    content: format!("failed to run git: {e}"),
                    ok: false,
                });
            }
        };
        let diff_out = match run_git(&base_args) {
            Ok(o) => o,
            Err(e) => {
                return Ok(ToolOutput {
                    content: format!("failed to run git: {e}"),
                    ok: false,
                });
            }
        };

        let stat = String::from_utf8_lossy(&stat_out.stdout).trim().to_string();
        let diff = String::from_utf8_lossy(&diff_out.stdout).trim().to_string();

        let ok = stat_out.status.success() && diff_out.status.success();

        let content = if !ok {
            let mut err = String::from_utf8_lossy(&diff_out.stderr).to_string();
            if err.trim().is_empty() {
                err = String::from_utf8_lossy(&stat_out.stderr).to_string();
            }
            format!("git diff failed: {}", err.trim())
        } else {
            format!(
                "=== Diff Stat ===\n{}\n\n=== Diff ===\n{}\n\nReview the above changes for bugs, security issues, and performance concerns.",
                stat, diff
            )
        };

        Ok(ToolOutput {
            content: truncated(content),
            ok,
        })
    }
}

// Alias for compatibility with alternate naming.
pub type CodeReviewTool = CodeReview;

// ---------------------------------------------------------------------------------------------
// Language server tools
//
// These are read-only queries against a real language server, so they inherit the read permission
// shape and declare no side effect. The server process itself is started and kept warm by
// [`LspPool`]; a missing server is reported as a failed observation with the install hint, never as
// a silent empty answer.
// ---------------------------------------------------------------------------------------------

/// A 0-based integer argument (`line` or `column`).
fn arg_u32(args: &Value, key: &str) -> Result<u32, VikramError> {
    let value = args.get(key).and_then(Value::as_u64).ok_or_else(|| {
        VikramError::ProviderRejected(format!("missing integer argument `{key}`"))
    })?;
    u32::try_from(value)
        .map_err(|_| VikramError::ProviderRejected(format!("`{key}` is out of range: {value}")))
}

fn lsp_position_schema() -> Value {
    json!({
        "type": "object",
        "properties": {
            "path": {
                "type": "string",
                "description": "File path, resolved inside the workspace root."
            },
            "line": {
                "type": "integer",
                "minimum": 0,
                "description": "0-based line number."
            },
            "column": {
                "type": "integer",
                "minimum": 0,
                "description": "0-based UTF-16 column number."
            }
        },
        "required": ["path", "line", "column"]
    })
}

fn lsp_file_schema() -> Value {
    json!({
        "type": "object",
        "properties": {
            "path": {
                "type": "string",
                "description": "File path, resolved inside the workspace root."
            }
        },
        "required": ["path"]
    })
}

fn lsp_permission(verb: &str, args: &Value) -> Result<PermissionRequest, VikramError> {
    let path = arg_str(args, "path")?;
    Ok(PermissionRequest::new(
        ActionKind::Read,
        verb,
        path,
        SideEffect::None,
        Trust::Model,
    ))
}

/// Pretty-print an LSP result. `null` is a real answer ("nothing here"), so it is spelled out.
fn json_output(verb: &str, value: &Value, empty: &str) -> ToolOutput {
    if value.is_null() {
        return ToolOutput {
            content: empty.to_string(),
            ok: true,
        };
    }
    match serde_json::to_string_pretty(value) {
        Ok(text) => ToolOutput {
            content: truncated(text),
            ok: true,
        },
        Err(e) => ToolOutput {
            content: format!("{verb}: cannot format the result: {e}"),
            ok: false,
        },
    }
}

/// A language server failure is a failed observation: the model can read "install rust-analyzer".
fn lsp_failed(verb: &str, err: &LspError) -> ToolOutput {
    ToolOutput {
        content: format!("{verb} failed: {err}"),
        ok: false,
    }
}

/// Resolve the path argument inside the workspace. `Ok(None)` means the path escapes nothing but
/// does not exist (or is not a file), which is reported before any server is started for it.
fn lsp_file_arg(args: &Value, ctx: &ToolContext) -> Result<Option<(PathBuf, String)>, VikramError> {
    let raw = arg_str(args, "path")?;
    let (path, display) = canonical_resource(&ctx.workspace_root, &raw)?;
    if !path.is_file() {
        return Ok(None);
    }
    Ok(Some((path, display)))
}

fn run_lsp_position(
    verb: &str,
    query: fn(&mut LspClient, &Path, u32, u32) -> Result<Value, LspError>,
    args: &Value,
    ctx: &ToolContext,
) -> Result<ToolOutput, VikramError> {
    let line = arg_u32(args, "line")?;
    let column = arg_u32(args, "column")?;
    let Some((path, display)) = lsp_file_arg(args, ctx)? else {
        let raw = arg_str(args, "path")?;
        return Ok(ToolOutput {
            content: format!("{verb}: `{raw}` is not a readable file in the workspace"),
            ok: false,
        });
    };
    let outcome = LspPool::global().call(&ctx.workspace_root, &path, |client| {
        query(client, &path, line, column)
    });
    Ok(match outcome {
        Ok(value) => json_output(
            verb,
            &value,
            &format!("{verb}: nothing at {display}:{line}:{column}"),
        ),
        Err(err) => lsp_failed(verb, &err),
    })
}

fn run_lsp_file(
    verb: &str,
    query: fn(&mut LspClient, &Path) -> Result<Value, LspError>,
    empty: &str,
    args: &Value,
    ctx: &ToolContext,
) -> Result<ToolOutput, VikramError> {
    let Some((path, _display)) = lsp_file_arg(args, ctx)? else {
        let raw = arg_str(args, "path")?;
        return Ok(ToolOutput {
            content: format!("{verb}: `{raw}` is not a readable file in the workspace"),
            ok: false,
        });
    };
    Ok(
        match LspPool::global().call(&ctx.workspace_root, &path, |client| query(client, &path)) {
            Ok(value) => json_output(verb, &value, empty),
            Err(err) => lsp_failed(verb, &err),
        },
    )
}

/// Type and documentation under a position, as the language server sees it.
struct LspHover;

impl Tool for LspHover {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "lsp.hover".into(),
            description: "Ask the language server what is at a position (type, signature, docs). \
                          Requires the server for the file's language to be installed."
                .into(),
            schema: lsp_position_schema(),
        }
    }

    fn permission(&self, args: &Value) -> Result<PermissionRequest, VikramError> {
        lsp_permission("lsp.hover", args)
    }

    fn run(&self, args: &Value, ctx: &ToolContext) -> Result<ToolOutput, VikramError> {
        run_lsp_position("lsp.hover", LspClient::hover, args, ctx)
    }
}

/// Where a symbol is defined.
struct LspDefinition;

impl Tool for LspDefinition {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "lsp.definition".into(),
            description: "Jump to where the symbol at a position is defined. \
                          Requires the server for the file's language to be installed."
                .into(),
            schema: lsp_position_schema(),
        }
    }

    fn permission(&self, args: &Value) -> Result<PermissionRequest, VikramError> {
        lsp_permission("lsp.definition", args)
    }

    fn run(&self, args: &Value, ctx: &ToolContext) -> Result<ToolOutput, VikramError> {
        run_lsp_position("lsp.definition", LspClient::definition, args, ctx)
    }
}

/// Every use of a symbol.
struct LspReferences;

impl Tool for LspReferences {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "lsp.references".into(),
            description: "Find every reference to the symbol at a position, declaration included. \
                          Requires the server for the file's language to be installed."
                .into(),
            schema: lsp_position_schema(),
        }
    }

    fn permission(&self, args: &Value) -> Result<PermissionRequest, VikramError> {
        lsp_permission("lsp.references", args)
    }

    fn run(&self, args: &Value, ctx: &ToolContext) -> Result<ToolOutput, VikramError> {
        run_lsp_position("lsp.references", LspClient::references, args, ctx)
    }
}

/// The symbol outline of a file.
struct LspSymbols;

impl Tool for LspSymbols {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "lsp.symbols".into(),
            description: "List the symbols a file declares, as reported by its language server. \
                          Requires the server for the file's language to be installed."
                .into(),
            schema: lsp_file_schema(),
        }
    }

    fn permission(&self, args: &Value) -> Result<PermissionRequest, VikramError> {
        lsp_permission("lsp.symbols", args)
    }

    fn run(&self, args: &Value, ctx: &ToolContext) -> Result<ToolOutput, VikramError> {
        let raw = arg_str(args, "path")?;
        run_lsp_file(
            "lsp.symbols",
            LspClient::document_symbols,
            &format!("lsp.symbols: `{raw}` declares no symbols"),
            args,
            ctx,
        )
    }
}

/// Compiler and lint diagnostics from the language server.
struct LspDiagnostics;

impl Tool for LspDiagnostics {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "lsp.diagnostics".into(),
            description: "Collect the diagnostics the language server reports for one file \
                          (errors, warnings, lints). Requires the server for the file's language \
                          to be installed."
                .into(),
            schema: lsp_file_schema(),
        }
    }

    fn permission(&self, args: &Value) -> Result<PermissionRequest, VikramError> {
        lsp_permission("lsp.diagnostics", args)
    }

    fn run(&self, args: &Value, ctx: &ToolContext) -> Result<ToolOutput, VikramError> {
        run_lsp_file(
            "lsp.diagnostics",
            LspClient::diagnostics,
            "lsp.diagnostics: the server reported no diagnostics",
            args,
            ctx,
        )
    }
}

pub struct McpTool {
    pub server: String,
    pub tool: String,
    pub description: String,
    pub schema: Value,
    pub registry: Arc<std::sync::Mutex<McpRegistry>>,
}

impl Tool for McpTool {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: mcp_tool_name(&self.server, &self.tool),
            description: if self.description.is_empty() {
                format!("MCP tool {} on {}", self.tool, self.server)
            } else {
                self.description.clone()
            },
            schema: self.schema.clone(),
        }
    }

    fn permission(&self, _args: &Value) -> Result<PermissionRequest, VikramError> {
        Ok(PermissionRequest::new(
            ActionKind::Execute,
            "mcp.call",
            format!("{}.{}", self.server, self.tool),
            SideEffect::LocalMutation,
            Trust::Model,
        ))
    }

    #[allow(clippy::await_holding_lock)]
    fn run(&self, args: &Value, _ctx: &ToolContext) -> Result<ToolOutput, VikramError> {
        let registry = Arc::clone(&self.registry);
        let server = self.server.clone();
        let tool = self.tool.clone();
        let arguments = args.clone();
        let result = match tokio::runtime::Handle::try_current() {
            Ok(handle) => handle.block_on(async {
                let mut guard = registry.lock().expect("mcp lock");
                guard.call_tool(&server, &tool, arguments).await
            }),
            Err(_) => {
                let rt = tokio::runtime::Runtime::new()
                    .map_err(|e| VikramError::EffectFailed(e.to_string()))?;
                rt.block_on(async {
                    let mut guard = registry.lock().expect("mcp lock");
                    guard.call_tool(&server, &tool, arguments).await
                })
            }
        };
        match result {
            Ok(text) => Ok(ToolOutput {
                content: truncated(text),
                ok: true,
            }),
            Err(err) => Ok(ToolOutput {
                content: err,
                ok: false,
            }),
        }
    }
}

struct PluginExecute;

impl Tool for PluginExecute {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "plugin.execute".into(),
            description: "Execute a discovered plugin's entry point.".into(),
            schema: json!({
                "type": "object",
                "properties": {
                    "plugin": {"type": "string"},
                    "args": {"type": "string"}
                },
                "required": ["plugin"]
            }),
        }
    }

    fn permission(&self, args: &Value) -> Result<PermissionRequest, VikramError> {
        let plugin = arg_str(args, "plugin")?;
        Ok(PermissionRequest::new(
            ActionKind::Execute,
            "plugin.execute",
            plugin,
            SideEffect::LocalMutation,
            Trust::Model,
        ))
    }

    fn run(&self, args: &Value, ctx: &ToolContext) -> Result<ToolOutput, VikramError> {
        let plugin = arg_str(args, "plugin")?;
        let args_str = args.get("args").and_then(Value::as_str).unwrap_or("");
        let reg = PluginRegistry::for_workspace(&ctx.workspace_root);
        match reg.execute(&plugin, args_str) {
            Ok(out) => Ok(ToolOutput {
                content: truncated(out),
                ok: true,
            }),
            Err(e) => Ok(ToolOutput {
                content: e,
                ok: false,
            }),
        }
    }
}

struct PluginList;

impl Tool for PluginList {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "plugin.list".into(),
            description: "List discovered plugins and their loaded status.".into(),
            schema: json!({"type": "object", "properties": {}}),
        }
    }

    fn permission(&self, _args: &Value) -> Result<PermissionRequest, VikramError> {
        Ok(PermissionRequest::new(
            ActionKind::Read,
            "plugin.list",
            "plugins",
            SideEffect::None,
            Trust::Model,
        ))
    }

    fn run(&self, _args: &Value, ctx: &ToolContext) -> Result<ToolOutput, VikramError> {
        let reg = PluginRegistry::for_workspace(&ctx.workspace_root);
        let plugins = reg.plugins();
        if plugins.is_empty() {
            return Ok(ToolOutput {
                content: "no plugins".into(),
                ok: true,
            });
        }
        let body = plugins
            .iter()
            .map(|p| format!("{} (loaded={})", p.name, p.loaded))
            .collect::<Vec<_>>()
            .join("\n");
        Ok(ToolOutput {
            content: truncated(body),
            ok: true,
        })
    }
}

/// Shared entry point for the plugin lifecycle tools: the workspace view of the plugin set.
fn plugin_registry(ctx: &ToolContext) -> PluginRegistry {
    PluginRegistry::for_workspace(&ctx.workspace_root)
}

struct PluginInstall;

impl Tool for PluginInstall {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "plugin.install".into(),
            description: "Install a plugin directory into the plugins directory.".into(),
            schema: json!({
                "type": "object",
                "properties": {
                    "path": {"type": "string"}
                },
                "required": ["path"]
            }),
        }
    }

    fn permission(&self, args: &Value) -> Result<PermissionRequest, VikramError> {
        let path = arg_str(args, "path")?;
        Ok(PermissionRequest::new(
            ActionKind::Write,
            "plugin.install",
            path,
            SideEffect::Destructive,
            Trust::Model,
        ))
    }

    fn run(&self, args: &Value, ctx: &ToolContext) -> Result<ToolOutput, VikramError> {
        let raw = arg_str(args, "path")?;
        let (source, _) = canonical_resource(&ctx.workspace_root, &raw)?;
        let registry = plugin_registry(ctx);
        match registry.install(&source) {
            Ok(id) => Ok(ToolOutput {
                content: format!(
                    "installed plugin `{id}` into {}",
                    registry.root().join(&id).display()
                ),
                ok: true,
            }),
            Err(e) => Ok(ToolOutput {
                content: e,
                ok: false,
            }),
        }
    }
}

struct PluginUninstall;

impl Tool for PluginUninstall {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "plugin.uninstall".into(),
            description: "Remove an installed plugin's directory.".into(),
            schema: json!({
                "type": "object",
                "properties": {
                    "plugin": {"type": "string"}
                },
                "required": ["plugin"]
            }),
        }
    }

    fn permission(&self, args: &Value) -> Result<PermissionRequest, VikramError> {
        let plugin = arg_str(args, "plugin")?;
        Ok(PermissionRequest::new(
            ActionKind::Write,
            "plugin.uninstall",
            plugin,
            SideEffect::Destructive,
            Trust::Model,
        ))
    }

    fn run(&self, args: &Value, ctx: &ToolContext) -> Result<ToolOutput, VikramError> {
        let plugin = arg_str(args, "plugin")?;
        match plugin_registry(ctx).uninstall(&plugin) {
            Ok(()) => Ok(ToolOutput {
                content: format!("uninstalled plugin `{plugin}`"),
                ok: true,
            }),
            Err(e) => Ok(ToolOutput {
                content: e,
                ok: false,
            }),
        }
    }
}

struct PluginEnable;

impl Tool for PluginEnable {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "plugin.enable".into(),
            description: "Enable an installed plugin by rewriting its manifest.".into(),
            schema: json!({
                "type": "object",
                "properties": {
                    "plugin": {"type": "string"}
                },
                "required": ["plugin"]
            }),
        }
    }

    fn permission(&self, args: &Value) -> Result<PermissionRequest, VikramError> {
        let plugin = arg_str(args, "plugin")?;
        Ok(PermissionRequest::new(
            ActionKind::Write,
            "plugin.enable",
            plugin,
            SideEffect::LocalMutation,
            Trust::Model,
        ))
    }

    fn run(&self, args: &Value, ctx: &ToolContext) -> Result<ToolOutput, VikramError> {
        let plugin = arg_str(args, "plugin")?;
        match plugin_registry(ctx).enable(&plugin) {
            Ok(()) => Ok(ToolOutput {
                content: format!("enabled plugin `{plugin}`"),
                ok: true,
            }),
            Err(e) => Ok(ToolOutput {
                content: e,
                ok: false,
            }),
        }
    }
}

struct PluginDisable;

impl Tool for PluginDisable {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "plugin.disable".into(),
            description: "Disable an installed plugin by rewriting its manifest.".into(),
            schema: json!({
                "type": "object",
                "properties": {
                    "plugin": {"type": "string"}
                },
                "required": ["plugin"]
            }),
        }
    }

    fn permission(&self, args: &Value) -> Result<PermissionRequest, VikramError> {
        let plugin = arg_str(args, "plugin")?;
        Ok(PermissionRequest::new(
            ActionKind::Write,
            "plugin.disable",
            plugin,
            SideEffect::LocalMutation,
            Trust::Model,
        ))
    }

    fn run(&self, args: &Value, ctx: &ToolContext) -> Result<ToolOutput, VikramError> {
        let plugin = arg_str(args, "plugin")?;
        match plugin_registry(ctx).disable(&plugin) {
            Ok(()) => Ok(ToolOutput {
                content: format!("disabled plugin `{plugin}`"),
                ok: true,
            }),
            Err(e) => Ok(ToolOutput {
                content: e,
                ok: false,
            }),
        }
    }
}

struct PluginValidate;

impl Tool for PluginValidate {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "plugin.validate".into(),
            description: "Check a plugin directory's manifest without installing it.".into(),
            schema: json!({
                "type": "object",
                "properties": {
                    "path": {"type": "string"}
                },
                "required": ["path"]
            }),
        }
    }

    fn permission(&self, args: &Value) -> Result<PermissionRequest, VikramError> {
        let path = arg_str(args, "path")?;
        Ok(PermissionRequest::new(
            ActionKind::Read,
            "plugin.validate",
            path,
            SideEffect::None,
            Trust::Model,
        ))
    }

    fn run(&self, args: &Value, ctx: &ToolContext) -> Result<ToolOutput, VikramError> {
        let raw = arg_str(args, "path")?;
        let (source, _) = canonical_resource(&ctx.workspace_root, &raw)?;
        match plugin_registry(ctx).validate(&source) {
            Ok(issues) if issues.is_empty() => Ok(ToolOutput {
                content: format!("{}: no problems found", source.display()),
                ok: true,
            }),
            Ok(issues) => Ok(ToolOutput {
                ok: !issues.iter().any(|i| i.starts_with("error:")),
                content: truncated(issues.join("\n")),
            }),
            Err(e) => Ok(ToolOutput {
                content: e,
                ok: false,
            }),
        }
    }
}

struct PluginInspect;

impl Tool for PluginInspect {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "plugin.inspect".into(),
            description: "Show an installed plugin's manifest summary.".into(),
            schema: json!({
                "type": "object",
                "properties": {
                    "plugin": {"type": "string"}
                },
                "required": ["plugin"]
            }),
        }
    }

    fn permission(&self, args: &Value) -> Result<PermissionRequest, VikramError> {
        let plugin = arg_str(args, "plugin")?;
        Ok(PermissionRequest::new(
            ActionKind::Read,
            "plugin.inspect",
            plugin,
            SideEffect::None,
            Trust::Model,
        ))
    }

    fn run(&self, args: &Value, ctx: &ToolContext) -> Result<ToolOutput, VikramError> {
        let plugin = arg_str(args, "plugin")?;
        match plugin_registry(ctx).inspect(&plugin) {
            Ok(summary) => Ok(ToolOutput {
                content: truncated(summary),
                ok: true,
            }),
            Err(e) => Ok(ToolOutput {
                content: e,
                ok: false,
            }),
        }
    }
}

struct PluginDeps;

impl Tool for PluginDeps {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "plugin.deps".into(),
            description: "Report installed plugins whose dependencies are missing or unsatisfied."
                .into(),
            schema: json!({"type": "object", "properties": {}}),
        }
    }

    fn permission(&self, _args: &Value) -> Result<PermissionRequest, VikramError> {
        Ok(PermissionRequest::new(
            ActionKind::Read,
            "plugin.deps",
            "plugins",
            SideEffect::None,
            Trust::Model,
        ))
    }

    fn run(&self, _args: &Value, ctx: &ToolContext) -> Result<ToolOutput, VikramError> {
        let problems = plugin_registry(ctx).resolve_dependencies();
        if problems.is_empty() {
            return Ok(ToolOutput {
                content: "all plugin dependencies are satisfied".into(),
                ok: true,
            });
        }
        let mut body = String::new();
        for (id, issues) in &problems {
            body.push_str(&format!("{id}:\n"));
            for issue in issues {
                body.push_str(&format!("  - {issue}\n"));
            }
        }
        Ok(ToolOutput {
            content: truncated(body),
            ok: true,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use vikram_contracts::SideEffect;

    fn ctx() -> (tempfile::TempDir, ToolContext) {
        let dir = tempfile::tempdir().unwrap();
        let ctx = ToolContext {
            workspace_root: dir.path().to_path_buf(),
        };
        (dir, ctx)
    }

    #[test]
    fn write_then_read_round_trips_inside_the_workspace() {
        let (_dir, ctx) = ctx();
        let write = WriteFile;
        let read = ReadFile;
        let out = write
            .run(&json!({"path": "notes/a.txt", "content": "hello"}), &ctx)
            .unwrap();
        assert!(out.ok, "{}", out.content);
        let back = read.run(&json!({"path": "notes/a.txt"}), &ctx).unwrap();
        assert_eq!(back.content, "hello");
    }

    #[test]
    fn writes_outside_the_workspace_are_refused_before_execution() {
        let (_dir, ctx) = ctx();
        let err = ReadFile
            .run(&json!({"path": "../../etc/passwd"}), &ctx)
            .unwrap_err();
        assert!(matches!(err, VikramError::PolicyDenied { .. }));
    }

    #[test]
    fn terminal_tool_is_registered() {
        let registry = ToolRegistry::with_builtins();
        let tool = registry.get("terminal").expect("terminal registered");
        let spec = tool.spec();
        assert_eq!(spec.name, "terminal");
        assert!(spec.schema["properties"]["action"].is_object());
    }

    #[test]
    fn terminal_declares_execute_permission() {
        let registry = ToolRegistry::with_builtins();
        let tool = registry.get("terminal").unwrap();
        let req = tool.permission(&json!({"action": "list"})).unwrap();
        assert_eq!(req.action, ActionKind::Execute);
        assert_eq!(req.verb, "terminal");
    }

    #[test]
    fn computer_status_is_registered() {
        let registry = ToolRegistry::with_builtins();
        let tool = registry
            .get("computer.status")
            .expect("computer.status registered");
        assert_eq!(tool.spec().name, "computer.status");
        // Registration must not depend on the desktop helpers actually being installed.
        for name in [
            "computer.screenshot",
            "computer.click",
            "computer.type",
            "computer.key",
            "computer.scroll",
            "computer.windows",
        ] {
            assert!(registry.names().contains(&name.to_string()), "{name}");
        }
    }

    #[test]
    fn every_builtin_declares_a_permission_shape() {
        let registry = ToolRegistry::with_builtins();
        for tool in registry.all() {
            let spec = tool.spec();
            assert!(!spec.name.is_empty());
            let sample = match spec.name.as_str() {
                "fs.write" => json!({"path": "a", "content": "b"}),
                "fs.edit" => json!({"path": "a", "old_string": "b", "new_string": "c"}),
                "search.text" => json!({"query": "x"}),
                "shell.run" => json!({"command": "echo hi"}),
                "git" => json!({"subcommand": "status"}),
                "build.run" => json!({}),
                "test.run" => json!({}),
                "memory.save" => json!({"key": "k", "value": "v"}),
                "memory.search" => json!({"query": "x"}),
                "memory.list" => json!({}),
                "memory.delete" => json!({"key": "k"}),
                "web.search" => json!({"query": "x"}),
                "team.create" => json!({"name": "t"}),
                "team.list" => json!({}),
                "team.delegate" => json!({"team": "t", "task": "x"}),
                "hive.create" => json!({"name": "t"}),
                "hive.list" => json!({}),
                "hive.show" => json!({"name": "t"}),
                "hive.delegate" => json!({"team": "t", "task": "x"}),
                "hive.delete" => json!({"name": "t"}),
                "bot.list" => json!({}),
                "bot.show" => json!({"name": "t"}),
                "bot.create" => json!({"name": "t", "role": "r", "description": "d"}),
                "agent.list" => json!({}),
                "agent.show" => json!({"name": "t"}),
                "skill.list" => json!({}),
                "skill.load" => json!({"name": "x"}),
                "skill.search" => json!({"query": "x"}),
                "skill.read" => json!({"name": "x", "path": "r"}),
                "diagnostics.collect" => json!({}),
                "process" => json!({"action": "list"}),
                "task" => json!({"action": "list"}),
                "ask_user" => json!({"question": "q"}),
                "browser.fetch" => json!({"url": "https://example.com"}),
                "browser.dom" | "browser.links" => json!({"url": "https://example.com"}),
                "browser.screenshot" => json!({"url": "https://example.com", "path": "shot.png"}),
                "browser.open" => json!({"url": "https://example.com"}),
                "browser.eval" => json!({"session": "browser-1", "js": "document.title"}),
                "browser.click" => json!({"session": "browser-1", "selector": "#go"}),
                "browser.type" => json!({"session": "browser-1", "selector": "#q", "text": "hi"}),
                "browser.scroll" => json!({"session": "browser-1", "x": 0, "y": 200}),
                "browser.text" | "browser.close" => json!({"session": "browser-1"}),
                "browser.sessions" => json!({}),
                "code.review" => json!({}),
                "plugin.execute" => json!({"plugin": "p", "args": "x"}),
                "plugin.list" => json!({}),
                "plugin.install" | "plugin.validate" => json!({"path": "plugins/p"}),
                "plugin.uninstall" | "plugin.enable" | "plugin.disable" | "plugin.inspect" => {
                    json!({"plugin": "p"})
                }
                "plugin.deps" => json!({}),
                "permission.check" => json!({"verb": "v", "resource": "r"}),
                "terminal" => json!({"action": "list"}),
                "computer.screenshot" => json!({"path": "shot.png"}),
                "computer.click" => json!({"x": 10, "y": 20}),
                "computer.type" => json!({"text": "hello"}),
                "computer.key" => json!({"keys": "ctrl+l"}),
                "computer.scroll" => json!({"direction": "down"}),
                "computer.windows" | "computer.status" => json!({}),
                "lsp.hover" | "lsp.definition" | "lsp.references" => {
                    json!({"path": "a.rs", "line": 0, "column": 0})
                }
                "lsp.symbols" | "lsp.diagnostics" => json!({"path": "a.rs"}),
                _ => json!({"path": "."}),
            };
            let req = tool.permission(&sample).unwrap();
            assert_eq!(req.originating_trust, Trust::Model);
        }
    }

    #[test]
    fn plugin_lifecycle_tools_are_registered() {
        let registry = ToolRegistry::with_builtins();
        let names = registry.names();
        for name in [
            "plugin.execute",
            "plugin.list",
            "plugin.install",
            "plugin.uninstall",
            "plugin.enable",
            "plugin.disable",
            "plugin.validate",
            "plugin.inspect",
            "plugin.deps",
        ] {
            assert!(
                names.contains(&name.to_string()),
                "{name} is not registered"
            );
        }
        assert_eq!(
            registry.get("plugin.install").unwrap().spec().schema["required"][0],
            "path"
        );
        assert_eq!(
            registry.get("plugin.inspect").unwrap().spec().schema["required"][0],
            "plugin"
        );
    }

    #[test]
    fn plugin_lifecycle_tools_declare_permission_shapes() {
        let registry = ToolRegistry::with_builtins();
        let cases: [(&str, Value, ActionKind, SideEffect); 7] = [
            (
                "plugin.install",
                json!({"path": "plugins/p"}),
                ActionKind::Write,
                SideEffect::Destructive,
            ),
            (
                "plugin.uninstall",
                json!({"plugin": "p"}),
                ActionKind::Write,
                SideEffect::Destructive,
            ),
            (
                "plugin.enable",
                json!({"plugin": "p"}),
                ActionKind::Write,
                SideEffect::LocalMutation,
            ),
            (
                "plugin.disable",
                json!({"plugin": "p"}),
                ActionKind::Write,
                SideEffect::LocalMutation,
            ),
            (
                "plugin.validate",
                json!({"path": "plugins/p"}),
                ActionKind::Read,
                SideEffect::None,
            ),
            (
                "plugin.inspect",
                json!({"plugin": "p"}),
                ActionKind::Read,
                SideEffect::None,
            ),
            ("plugin.deps", json!({}), ActionKind::Read, SideEffect::None),
        ];
        for (name, sample, action, side_effect) in cases {
            let tool = registry
                .get(name)
                .unwrap_or_else(|| panic!("{name} registered"));
            let req = tool.permission(&sample).unwrap();
            assert_eq!(req.action, action, "{name}");
            assert_eq!(req.verb, name, "{name}");
            assert_eq!(req.side_effect, side_effect, "{name}");
            assert_eq!(req.originating_trust, Trust::Model, "{name}");
        }
    }

    #[test]
    fn plugin_install_rejects_a_directory_without_a_manifest() {
        let (_dir, ctx) = ctx();
        let out = PluginInstall
            .run(&json!({"path": "plugins/absent"}), &ctx)
            .unwrap();
        assert!(!out.ok);
        assert!(out.content.contains("no plugin.json"), "{}", out.content);
    }

    #[test]
    fn plugin_uninstall_reports_an_unknown_plugin() {
        let (_dir, ctx) = ctx();
        let out = PluginUninstall
            .run(&json!({"plugin": "definitely-not-installed-9f3a"}), &ctx)
            .unwrap();
        assert!(!out.ok);
        assert!(out.content.contains("plugin not found"), "{}", out.content);
    }

    #[test]
    fn plugin_inspect_reports_an_unknown_plugin() {
        let (_dir, ctx) = ctx();
        let out = PluginInspect
            .run(&json!({"plugin": "definitely-not-installed-9f3a"}), &ctx)
            .unwrap();
        assert!(!out.ok);
        assert!(out.content.contains("plugin not found"), "{}", out.content);
    }

    #[test]
    fn plugin_validate_reports_manifest_problems() {
        let (dir, ctx) = ctx();
        std::fs::create_dir_all(dir.path().join("plugins/bad")).unwrap();
        std::fs::write(
            dir.path().join("plugins/bad/plugin.json"),
            r#"{"name":"bad","version":"1.0"}"#,
        )
        .unwrap();
        let out = PluginValidate
            .run(&json!({"path": "plugins/bad"}), &ctx)
            .unwrap();
        assert!(!out.ok);
        assert!(out.content.contains("not semver"), "{}", out.content);
        assert!(out.content.contains("no `entry`"), "{}", out.content);
    }

    #[test]
    fn plugin_validate_accepts_a_clean_manifest() {
        let (dir, ctx) = ctx();
        std::fs::create_dir_all(dir.path().join("plugins/good")).unwrap();
        std::fs::write(
            dir.path().join("plugins/good/plugin.json"),
            r#"{"name":"good","version":"1.0.0","description":"d","entry":"main.sh"}"#,
        )
        .unwrap();
        std::fs::write(dir.path().join("plugins/good/main.sh"), "#!/bin/sh\n").unwrap();
        let out = PluginValidate
            .run(&json!({"path": "plugins/good"}), &ctx)
            .unwrap();
        assert!(out.ok, "{}", out.content);
        assert!(out.content.contains("no problems found"), "{}", out.content);
    }

    #[test]
    fn plugin_deps_answers_for_the_workspace() {
        let (_dir, ctx) = ctx();
        let out = PluginDeps.run(&json!({}), &ctx).unwrap();
        assert!(out.ok);
        assert!(!out.content.is_empty());
    }

    #[test]
    fn shell_declares_an_execute_side_effect() {
        let req = RunShell
            .permission(&json!({"command": "echo hello"}))
            .unwrap();
        assert_eq!(req.action, ActionKind::Execute);
        assert_eq!(req.verb, "shell.run");
        assert_eq!(req.side_effect, SideEffect::LocalMutation);
    }

    #[test]
    fn edit_replaces_exact_string() {
        let (_dir, ctx) = ctx();
        let write = WriteFile;
        let edit = EditFile;
        let read = ReadFile;
        write
            .run(&json!({"path": "a.txt", "content": "hello world"}), &ctx)
            .unwrap();
        let out = edit
            .run(
                &json!({"path": "a.txt", "old_string": "world", "new_string": "rust"}),
                &ctx,
            )
            .unwrap();
        assert!(out.ok, "{}", out.content);
        let back = read.run(&json!({"path": "a.txt"}), &ctx).unwrap();
        assert_eq!(back.content, "hello rust");
    }

    #[test]
    fn edit_old_string_not_found_returns_false() {
        let (_dir, ctx) = ctx();
        let write = WriteFile;
        let edit = EditFile;
        write
            .run(&json!({"path": "a.txt", "content": "hello world"}), &ctx)
            .unwrap();
        let out = edit
            .run(
                &json!({"path": "a.txt", "old_string": "nope", "new_string": "rust"}),
                &ctx,
            )
            .unwrap();
        assert!(!out.ok);
        assert_eq!(out.content, "old_string not found in file");
    }

    #[test]
    fn edit_old_string_multiple_returns_false() {
        let (_dir, ctx) = ctx();
        let write = WriteFile;
        let edit = EditFile;
        write
            .run(&json!({"path": "a.txt", "content": "aa"}), &ctx)
            .unwrap();
        let out = edit
            .run(
                &json!({"path": "a.txt", "old_string": "a", "new_string": "b"}),
                &ctx,
            )
            .unwrap();
        assert!(!out.ok);
        assert_eq!(out.content, "old_string appears 2 times, must be unique");
    }

    #[test]
    fn delete_removes_file_and_reports_content() {
        let (_dir, ctx) = ctx();
        let write = WriteFile;
        let delete = DeleteFile;
        let read = ReadFile;
        write
            .run(&json!({"path": "a.txt", "content": "goodbye"}), &ctx)
            .unwrap();
        let out = delete.run(&json!({"path": "a.txt"}), &ctx).unwrap();
        assert!(out.ok, "{}", out.content);
        assert!(out.content.contains("deleted a.txt"));
        assert!(out.content.contains("--- deleted content ---"));
        assert!(out.content.contains("goodbye"));
        let back = read.run(&json!({"path": "a.txt"}), &ctx).unwrap();
        assert!(!back.ok);
    }

    #[test]
    fn delete_nonexistent_returns_false() {
        let (_dir, ctx) = ctx();
        let delete = DeleteFile;
        let out = delete.run(&json!({"path": "nope.txt"}), &ctx).unwrap();
        assert!(!out.ok);
    }

    #[test]
    fn edit_and_delete_outside_workspace_are_refused() {
        let (_dir, ctx) = ctx();
        let edit = EditFile;
        let delete = DeleteFile;
        let err = edit
            .run(
                &json!({"path": "../../etc/passwd", "old_string": "a", "new_string": "b"}),
                &ctx,
            )
            .unwrap_err();
        assert!(matches!(err, VikramError::PolicyDenied { .. }));
        let err = delete
            .run(&json!({"path": "../../etc/passwd"}), &ctx)
            .unwrap_err();
        assert!(matches!(err, VikramError::PolicyDenied { .. }));
    }

    #[test]
    fn edit_and_delete_declare_write_permission() {
        let edit_req = EditFile
            .permission(&json!({"path": "a", "old_string": "b", "new_string": "c"}))
            .unwrap();
        assert_eq!(edit_req.action, ActionKind::Write);
        assert_eq!(edit_req.verb, "fs.edit");
        assert_eq!(edit_req.side_effect, SideEffect::LocalMutation);
        let del_req = DeleteFile.permission(&json!({"path": "a"})).unwrap();
        assert_eq!(del_req.action, ActionKind::Write);
        assert_eq!(del_req.verb, "fs.delete");
        assert_eq!(del_req.side_effect, SideEffect::Destructive);
    }

    #[test]
    fn web_search_is_registered() {
        let registry = ToolRegistry::with_builtins();
        assert!(registry.names().contains(&"web.search".to_string()));
    }

    #[test]
    fn git_status_is_registered() {
        let registry = ToolRegistry::with_builtins();
        assert!(registry.names().contains(&"git".to_string()));
    }

    #[test]
    fn build_is_registered() {
        let registry = ToolRegistry::with_builtins();
        assert!(registry.names().contains(&"build.run".to_string()));
    }

    #[test]
    fn test_is_registered() {
        let registry = ToolRegistry::with_builtins();
        assert!(registry.names().contains(&"test.run".to_string()));
    }

    #[test]
    fn git_status_declares_read_permission() {
        let req = Git.permission(&json!({"subcommand": "status"})).unwrap();
        assert_eq!(req.action, ActionKind::Read);
        assert_eq!(req.verb, "git");
        assert_eq!(req.side_effect, SideEffect::None);
    }

    #[test]
    fn build_declares_execute_permission() {
        let req = BuildRun.permission(&json!({})).unwrap();
        assert_eq!(req.action, ActionKind::Execute);
        assert_eq!(req.verb, "build.run");
        assert_eq!(req.side_effect, SideEffect::LocalMutation);
    }

    #[test]
    fn test_declares_execute_permission() {
        let req = TestRun.permission(&json!({})).unwrap();
        assert_eq!(req.action, ActionKind::Execute);
        assert_eq!(req.verb, "test.run");
        assert_eq!(req.side_effect, SideEffect::LocalMutation);
    }

    #[test]
    fn diagnostics_is_registered() {
        let registry = ToolRegistry::with_builtins();
        assert!(
            registry
                .names()
                .contains(&"diagnostics.collect".to_string())
        );
    }

    #[test]
    fn process_is_registered() {
        let registry = ToolRegistry::with_builtins();
        assert!(registry.names().contains(&"process".to_string()));
    }

    #[test]
    fn task_is_registered() {
        let registry = ToolRegistry::with_builtins();
        assert!(registry.names().contains(&"task".to_string()));
    }

    #[test]
    fn diagnostics_declares_read_permission() {
        let req = Diagnostics.permission(&json!({})).unwrap();
        assert_eq!(req.action, ActionKind::Read);
        assert_eq!(req.verb, "diagnostics.collect");
        assert_eq!(req.side_effect, SideEffect::None);
    }

    #[test]
    fn process_declares_execute_permission() {
        let req = Process.permission(&json!({"action": "list"})).unwrap();
        assert_eq!(req.action, ActionKind::Execute);
        assert_eq!(req.verb, "process.run");
        assert_eq!(req.side_effect, SideEffect::LocalMutation);
    }

    #[test]
    fn task_declares_write_permission() {
        let req = Task.permission(&json!({"action": "list"})).unwrap();
        assert_eq!(req.action, ActionKind::Write);
        assert_eq!(req.verb, "task.manage");
        assert_eq!(req.side_effect, SideEffect::LocalMutation);
    }

    #[test]
    fn mcp_tool_name_formats_with_prefix() {
        assert_eq!(mcp_tool_name("fs", "read"), "mcp.fs.read");
        assert_eq!(mcp_tool_name("git", "status"), "mcp.git.status");
    }

    #[test]
    fn mcp_tool_declares_execute_network_permission() {
        use std::sync::Mutex;
        let tool = McpTool {
            server: "fs".into(),
            tool: "read".into(),
            description: "read".into(),
            schema: json!({"type": "object"}),
            registry: Arc::new(Mutex::new(McpRegistry::load(
                crate::config::McpConfig::default(),
            ))),
        };
        let spec = tool.spec();
        assert_eq!(spec.name, "mcp.fs.read");
        let req = tool.permission(&json!({})).unwrap();
        assert_eq!(req.action, ActionKind::Execute);
        assert_eq!(req.verb, "mcp.call");
        assert_eq!(req.side_effect, SideEffect::LocalMutation);
    }

    #[test]
    fn web_search_declares_network_permission() {
        let req = WebSearch.permission(&json!({"query": "rust"})).unwrap();
        assert_eq!(req.action, ActionKind::Network);
        assert_eq!(req.verb, "web.search");
        assert_eq!(req.resource, "rust");
        assert_eq!(req.side_effect, SideEffect::None);
    }

    #[test]
    fn web_search_reports_a_backend_failure_instead_of_an_empty_success() {
        let out = search_tool_output(Err(crate::search::SearchError::Status {
            backend: "bing",
            status: 503,
            detail: "service unavailable".into(),
        }));
        assert!(!out.ok);
        assert!(out.content.contains("503"), "{}", out.content);
        assert!(out.content.contains("web search failed"), "{}", out.content);
    }

    #[test]
    fn web_search_treats_zero_parsed_results_as_a_failure() {
        let out = search_tool_output(Ok(Vec::new()));
        assert!(!out.ok, "{}", out.content);
        assert!(out.content.contains("zero results"), "{}", out.content);
    }

    #[test]
    fn web_search_renders_title_url_and_snippet_for_the_model() {
        let out = search_tool_output(Ok(vec![crate::search::SearchResult {
            title: "Rust".into(),
            url: "https://www.rust-lang.org/".into(),
            snippet: "A fast language.".into(),
        }]));
        assert!(out.ok, "{}", out.content);
        assert!(out.content.contains("1. Rust"), "{}", out.content);
        assert!(
            out.content.contains("URL: https://www.rust-lang.org/"),
            "{}",
            out.content
        );
        assert!(out.content.contains("A fast language."), "{}", out.content);
    }

    #[test]
    fn browser_fetch_is_registered() {
        let registry = ToolRegistry::with_builtins();
        assert!(registry.names().contains(&"browser.fetch".to_string()));
    }

    #[test]
    fn code_review_is_registered() {
        let registry = ToolRegistry::with_builtins();
        assert!(registry.names().contains(&"code.review".to_string()));
    }

    #[test]
    fn browser_fetch_declares_network_permission() {
        let req = BrowserFetch
            .permission(&json!({"url": "https://example.com"}))
            .unwrap();
        assert_eq!(req.action, ActionKind::Network);
        assert_eq!(req.verb, "browser.fetch");
        assert_eq!(req.resource, "https://example.com");
        assert_eq!(req.side_effect, SideEffect::None);
    }

    // --- DOM-aware browser tools ---------------------------------------------------------------
    //
    // No test here starts a browser: `--dump-dom` needs a network page to be worth anything, and a
    // unit test must not depend on either. What is tested is the tool surface, the permission
    // shape, and the refusals that happen before a browser is ever looked for.

    #[test]
    fn browser_dom_is_registered() {
        let registry = ToolRegistry::with_builtins();
        let tool = registry.get("browser.dom").expect("browser.dom registered");
        assert_eq!(tool.spec().name, "browser.dom");
        assert!(tool.spec().schema["properties"]["url"].is_object());
    }

    #[test]
    fn browser_links_is_registered() {
        let registry = ToolRegistry::with_builtins();
        let tool = registry
            .get("browser.links")
            .expect("browser.links registered");
        assert_eq!(tool.spec().name, "browser.links");
        assert!(tool.spec().schema["properties"]["url"].is_object());
    }

    #[test]
    fn browser_screenshot_is_registered() {
        let registry = ToolRegistry::with_builtins();
        let tool = registry
            .get("browser.screenshot")
            .expect("browser.screenshot registered");
        assert_eq!(tool.spec().name, "browser.screenshot");
        for field in ["url", "path"] {
            assert!(
                tool.spec().schema["properties"][field].is_object(),
                "browser.screenshot must document `{field}`"
            );
        }
    }

    #[test]
    fn browser_dom_declares_network_permission() {
        let req = BrowserDom
            .permission(&json!({"url": "https://example.com"}))
            .unwrap();
        assert_eq!(req.action, ActionKind::Network);
        assert_eq!(req.verb, "browser.dom");
        assert_eq!(req.resource, "https://example.com");
        assert_eq!(req.side_effect, SideEffect::None);
    }

    #[test]
    fn browser_links_declares_network_permission() {
        let req = BrowserLinks
            .permission(&json!({"url": "https://example.com"}))
            .unwrap();
        assert_eq!(req.action, ActionKind::Network);
        assert_eq!(req.verb, "browser.links");
        assert_eq!(req.resource, "https://example.com");
        assert_eq!(req.side_effect, SideEffect::None);
    }

    #[test]
    fn browser_screenshot_declares_a_local_mutation() {
        let req = BrowserScreenshot
            .permission(&json!({"url": "https://example.com", "path": "shots/home.png"}))
            .unwrap();
        assert_eq!(req.action, ActionKind::Network);
        assert_eq!(req.verb, "browser.screenshot");
        assert_eq!(req.resource, "shots/home.png");
        assert_eq!(req.side_effect, SideEffect::LocalMutation);
    }

    #[test]
    fn browser_tools_refuse_a_permission_without_a_url() {
        assert!(BrowserDom.permission(&json!({})).is_err());
        assert!(BrowserLinks.permission(&json!({})).is_err());
        assert!(
            BrowserScreenshot
                .permission(&json!({"path": "shot.png"}))
                .is_err()
        );
        assert!(
            BrowserScreenshot
                .permission(&json!({"url": "https://example.com"}))
                .is_err()
        );
    }

    #[test]
    fn browser_screenshot_refuses_a_path_outside_the_workspace() {
        let (_dir, ctx) = ctx();
        let err = BrowserScreenshot
            .run(
                &json!({"url": "https://example.com", "path": "../escape.png"}),
                &ctx,
            )
            .unwrap_err();
        assert!(matches!(err, VikramError::PolicyDenied { .. }));
    }

    // --- interactive browser tools -------------------------------------------------------------
    //
    // As above: nothing here launches a browser. What is tested is the tool surface, the permission
    // shape each call declares, and the refusals that happen before a browser is looked for.

    #[test]
    fn interactive_browser_tools_are_registered() {
        let registry = ToolRegistry::with_builtins();
        let names = registry.names();
        for name in [
            "browser.open",
            "browser.eval",
            "browser.click",
            "browser.type",
            "browser.scroll",
            "browser.text",
            "browser.close",
            "browser.sessions",
        ] {
            assert!(
                names.contains(&name.to_string()),
                "{name} is not registered"
            );
        }
    }

    #[test]
    fn interactive_browser_tools_document_the_arguments_they_need() {
        let registry = ToolRegistry::with_builtins();
        let expected: [(&str, &[&str]); 8] = [
            ("browser.open", &["url"]),
            ("browser.eval", &["session", "js"]),
            ("browser.click", &["session", "selector"]),
            ("browser.type", &["session", "selector", "text"]),
            ("browser.scroll", &["session"]),
            ("browser.text", &["session"]),
            ("browser.close", &["session"]),
            ("browser.sessions", &[]),
        ];
        for (name, fields) in expected {
            let tool = registry
                .get(name)
                .unwrap_or_else(|| panic!("{name} registered"));
            let schema = tool.spec().schema;
            for field in fields {
                assert!(
                    schema["properties"][*field].is_object(),
                    "{name} must document `{field}`"
                );
            }
        }
    }

    #[test]
    fn interactive_browser_tools_declare_their_permission_shapes() {
        let registry = ToolRegistry::with_builtins();
        let cases: [(&str, Value, ActionKind, SideEffect); 8] = [
            (
                "browser.open",
                json!({"url": "https://example.com"}),
                ActionKind::Network,
                SideEffect::None,
            ),
            (
                "browser.eval",
                json!({"session": "browser-1", "js": "1"}),
                ActionKind::Execute,
                SideEffect::None,
            ),
            (
                "browser.click",
                json!({"session": "browser-1", "selector": "#go"}),
                ActionKind::Execute,
                SideEffect::LocalMutation,
            ),
            (
                "browser.type",
                json!({"session": "browser-1", "selector": "#q", "text": "hi"}),
                ActionKind::Execute,
                SideEffect::LocalMutation,
            ),
            (
                "browser.scroll",
                json!({"session": "browser-1", "y": 10}),
                ActionKind::Execute,
                SideEffect::None,
            ),
            (
                "browser.text",
                json!({"session": "browser-1"}),
                ActionKind::Read,
                SideEffect::None,
            ),
            (
                "browser.close",
                json!({"session": "browser-1"}),
                ActionKind::Execute,
                SideEffect::None,
            ),
            (
                "browser.sessions",
                json!({}),
                ActionKind::Read,
                SideEffect::None,
            ),
        ];
        for (name, args, action, side_effect) in cases {
            let tool = registry
                .get(name)
                .unwrap_or_else(|| panic!("{name} registered"));
            let req = tool
                .permission(&args)
                .unwrap_or_else(|e| panic!("{name} refused its own sample: {e}"));
            assert_eq!(req.action, action, "{name}");
            assert_eq!(req.verb, name, "{name}");
            assert_eq!(req.side_effect, side_effect, "{name}");
            assert_eq!(req.originating_trust, Trust::Model, "{name}");
        }
    }

    #[test]
    fn an_interaction_names_its_session_as_the_resource() {
        let registry = ToolRegistry::with_builtins();
        for name in ["browser.click", "browser.type", "browser.text"] {
            let tool = registry.get(name).unwrap();
            let req = tool
                .permission(&json!({"session": "browser-7", "selector": "#a", "text": "t"}))
                .unwrap();
            assert_eq!(req.resource, "browser-7", "{name}");
        }
        let req = BrowserOpen
            .permission(&json!({"url": "https://example.com/x"}))
            .unwrap();
        assert_eq!(req.resource, "https://example.com/x");
        assert_eq!(req.action, ActionKind::Network);
    }

    #[test]
    fn interactive_browser_tools_refuse_a_permission_without_their_arguments() {
        assert!(BrowserOpen.permission(&json!({})).is_err());
        assert!(BrowserEval.permission(&json!({"js": "1"})).is_err());
        assert!(BrowserClick.permission(&json!({"selector": "#a"})).is_err());
        // The session is all a permission request needs: a call with no `text` is still described
        // to policy, and the missing argument is caught when the tool runs.
        assert!(
            BrowserType
                .permission(&json!({"session": "browser-1", "selector": "#a"}))
                .is_ok()
        );
        assert!(BrowserScroll.permission(&json!({})).is_err());
        assert!(BrowserText.permission(&json!({})).is_err());
        assert!(BrowserClose.permission(&json!({})).is_err());
        // `browser.sessions` needs nothing at all.
        assert!(BrowserSessions.permission(&json!({})).is_ok());
    }

    #[test]
    fn an_interaction_with_an_unknown_session_fails_without_a_browser() {
        let tool = ToolRegistry::with_builtins()
            .get("browser.text")
            .expect("browser.text registered");
        let (_dir, ctx) = ctx();
        let output = tool
            .run(&json!({"session": "browser-does-not-exist"}), &ctx)
            .unwrap();
        assert!(!output.ok, "{output:?}");
        assert!(
            output.content.contains("unknown browser session"),
            "{}",
            output.content
        );
    }

    #[test]
    fn an_interaction_with_an_empty_selector_is_refused_before_the_browser() {
        let (_dir, ctx) = ctx();
        let output = BrowserClick
            .run(
                &json!({"session": "browser-does-not-exist", "selector": "  "}),
                &ctx,
            )
            .unwrap();
        assert!(!output.ok, "{output:?}");
        assert!(
            output.content.contains("must not be empty"),
            "{}",
            output.content
        );
    }

    #[test]
    fn a_scroll_without_an_offset_is_refused() {
        let (_dir, ctx) = ctx();
        let output = BrowserScroll
            .run(&json!({"session": "browser-does-not-exist"}), &ctx)
            .unwrap();
        assert!(!output.ok, "{output:?}");
        assert!(
            output.content.contains("both `x` and `y` are zero"),
            "{}",
            output.content
        );
    }

    #[test]
    fn browser_sessions_lists_nothing_when_nothing_is_open() {
        let (_dir, ctx) = ctx();
        let output = BrowserSessions.run(&json!({}), &ctx).unwrap();
        assert!(output.ok, "{output:?}");
        assert!(
            output.content.contains("no browser sessions"),
            "{}",
            output.content
        );
    }

    #[test]
    fn code_review_declares_read_permission() {
        let req = CodeReview.permission(&json!({})).unwrap();
        assert_eq!(req.action, ActionKind::Read);
        assert_eq!(req.verb, "code.review");
        assert_eq!(req.side_effect, SideEffect::None);
    }

    #[test]
    fn ask_user_is_registered() {
        let registry = ToolRegistry::with_builtins();
        assert!(registry.names().contains(&"ask_user".to_string()));
    }

    #[test]
    fn permission_check_is_registered() {
        let registry = ToolRegistry::with_builtins();
        assert!(registry.names().contains(&"permission.check".to_string()));
    }

    #[test]
    fn ask_user_declares_read_permission() {
        let req = AskUser.permission(&json!({"question": "q"})).unwrap();
        assert_eq!(req.action, ActionKind::Read);
        assert_eq!(req.verb, "ask_user");
        assert_eq!(req.side_effect, SideEffect::None);
    }

    #[test]
    fn ask_user_returns_structured_question() {
        let (_dir, ctx) = ctx();
        let out = AskUser
            .run(&json!({"question": "What is your name?"}), &ctx)
            .unwrap();
        assert!(out.ok, "{}", out.content);
        assert!(out.content.starts_with("QUESTION:"));
        assert!(out.content.contains("What is your name?"));
        assert!(out.content.contains("Status: awaiting_user_response"));
    }

    #[test]
    fn ask_user_includes_choices_as_json() {
        let (_dir, ctx) = ctx();
        let out = AskUser
            .run(
                &json!({"question": "Pick one", "choices": ["yes", "no"]}),
                &ctx,
            )
            .unwrap();
        assert!(out.ok, "{}", out.content);
        assert!(out.content.contains("yes"));
        assert!(out.content.contains("no"));
        assert!(out.content.contains("Choices: [\"yes\",\"no\"]"));
    }

    #[test]
    fn ask_user_generates_unique_request_ids() {
        let (_dir, ctx) = ctx();
        let a = AskUser.run(&json!({"question": "q1"}), &ctx).unwrap();
        let b = AskUser.run(&json!({"question": "q2"}), &ctx).unwrap();
        let id_a = a
            .content
            .lines()
            .next()
            .unwrap()
            .trim_start_matches("QUESTION:");
        let id_b = b
            .content
            .lines()
            .next()
            .unwrap()
            .trim_start_matches("QUESTION:");
        assert_ne!(id_a, id_b);
        assert!(!id_a.is_empty());
        assert!(!id_b.is_empty());
    }

    #[test]
    fn permission_check_declares_read_permission() {
        let req = PermissionCheck
            .permission(&json!({"verb": "v", "resource": "r"}))
            .unwrap();
        assert_eq!(req.action, ActionKind::Read);
        assert_eq!(req.verb, "permission.check");
        assert_eq!(req.side_effect, SideEffect::None);
    }

    #[test]
    fn hive_create_is_registered() {
        let registry = ToolRegistry::with_builtins();
        assert!(registry.names().contains(&"hive.create".to_string()));
    }

    #[test]
    fn hive_list_is_registered() {
        let registry = ToolRegistry::with_builtins();
        assert!(registry.names().contains(&"hive.list".to_string()));
    }

    #[test]
    fn hive_show_is_registered() {
        let registry = ToolRegistry::with_builtins();
        assert!(registry.names().contains(&"hive.show".to_string()));
    }

    #[test]
    fn hive_delegate_is_registered() {
        let registry = ToolRegistry::with_builtins();
        assert!(registry.names().contains(&"hive.delegate".to_string()));
    }

    #[test]
    fn hive_delete_is_registered() {
        let registry = ToolRegistry::with_builtins();
        assert!(registry.names().contains(&"hive.delete".to_string()));
    }

    #[test]
    fn hive_create_declares_write_permission() {
        let req = HiveCreate.permission(&json!({"name": "t"})).unwrap();
        assert_eq!(req.action, ActionKind::Write);
        assert_eq!(req.verb, "hive.create");
        assert_eq!(req.side_effect, SideEffect::LocalMutation);
    }

    #[test]
    fn hive_list_declares_read_permission() {
        let req = HiveList.permission(&json!({})).unwrap();
        assert_eq!(req.action, ActionKind::Read);
        assert_eq!(req.verb, "hive.list");
        assert_eq!(req.side_effect, SideEffect::None);
    }

    #[test]
    fn hive_show_declares_read_permission() {
        let req = HiveShow.permission(&json!({"name": "t"})).unwrap();
        assert_eq!(req.action, ActionKind::Read);
        assert_eq!(req.verb, "hive.show");
        assert_eq!(req.side_effect, SideEffect::None);
    }

    #[test]
    fn hive_delegate_declares_delegate_permission() {
        let req = HiveDelegate
            .permission(&json!({"team": "t", "task": "x"}))
            .unwrap();
        assert_eq!(req.action, ActionKind::Delegate);
        assert_eq!(req.verb, "hive.delegate");
        assert_eq!(req.side_effect, SideEffect::LocalMutation);
    }

    #[test]
    fn hive_delete_declares_write_permission() {
        let req = HiveDelete.permission(&json!({"name": "t"})).unwrap();
        assert_eq!(req.action, ActionKind::Write);
        assert_eq!(req.verb, "hive.delete");
        assert_eq!(req.side_effect, SideEffect::Destructive);
    }

    #[test]
    fn bot_list_is_registered() {
        let registry = ToolRegistry::with_builtins();
        assert!(registry.names().contains(&"bot.list".to_string()));
    }

    #[test]
    fn bot_show_is_registered() {
        let registry = ToolRegistry::with_builtins();
        assert!(registry.names().contains(&"bot.show".to_string()));
    }

    #[test]
    fn bot_create_is_registered() {
        let registry = ToolRegistry::with_builtins();
        assert!(registry.names().contains(&"bot.create".to_string()));
    }

    #[test]
    fn agent_list_is_registered() {
        let registry = ToolRegistry::with_builtins();
        assert!(registry.names().contains(&"agent.list".to_string()));
    }

    #[test]
    fn agent_show_is_registered() {
        let registry = ToolRegistry::with_builtins();
        assert!(registry.names().contains(&"agent.show".to_string()));
    }

    #[test]
    fn bot_list_declares_read_permission() {
        let req = BotList.permission(&json!({})).unwrap();
        assert_eq!(req.action, ActionKind::Read);
        assert_eq!(req.verb, "bot.list");
        assert_eq!(req.side_effect, SideEffect::None);
    }

    #[test]
    fn bot_show_declares_read_permission() {
        let req = BotShow.permission(&json!({"name": "t"})).unwrap();
        assert_eq!(req.action, ActionKind::Read);
        assert_eq!(req.verb, "bot.show");
        assert_eq!(req.side_effect, SideEffect::None);
    }

    #[test]
    fn bot_create_declares_write_permission() {
        let req = BotCreate
            .permission(&json!({"name": "t", "role": "r", "description": "d"}))
            .unwrap();
        assert_eq!(req.action, ActionKind::Write);
        assert_eq!(req.verb, "bot.create");
        assert_eq!(req.side_effect, SideEffect::LocalMutation);
    }

    #[test]
    fn agent_list_declares_read_permission() {
        let req = AgentList.permission(&json!({})).unwrap();
        assert_eq!(req.action, ActionKind::Read);
        assert_eq!(req.verb, "agent.list");
        assert_eq!(req.side_effect, SideEffect::None);
    }

    #[test]
    fn agent_show_declares_read_permission() {
        let req = AgentShow.permission(&json!({"name": "t"})).unwrap();
        assert_eq!(req.action, ActionKind::Read);
        assert_eq!(req.verb, "agent.show");
        assert_eq!(req.side_effect, SideEffect::None);
    }

    // --- language server tools -----------------------------------------------------------------
    //
    // No test here starts a language server: servers may not be installed, and a unit test that
    // depended on rust-analyzer would pass or fail on the tester's machine. What is tested is the
    // part that must never change — the tool surface and the permission shape — plus the failure
    // paths that need no server at all.

    #[test]
    fn lsp_hover_is_registered() {
        let registry = ToolRegistry::with_builtins();
        assert!(registry.names().contains(&"lsp.hover".to_string()));
    }

    #[test]
    fn lsp_definition_is_registered() {
        let registry = ToolRegistry::with_builtins();
        assert!(registry.names().contains(&"lsp.definition".to_string()));
    }

    #[test]
    fn lsp_references_is_registered() {
        let registry = ToolRegistry::with_builtins();
        assert!(registry.names().contains(&"lsp.references".to_string()));
    }

    #[test]
    fn lsp_symbols_is_registered() {
        let registry = ToolRegistry::with_builtins();
        assert!(registry.names().contains(&"lsp.symbols".to_string()));
    }

    #[test]
    fn lsp_diagnostics_is_registered() {
        let registry = ToolRegistry::with_builtins();
        assert!(registry.names().contains(&"lsp.diagnostics".to_string()));
    }

    #[test]
    fn lsp_position_tools_require_a_path_a_line_and_a_column() {
        let registry = ToolRegistry::with_builtins();
        for name in ["lsp.hover", "lsp.definition", "lsp.references"] {
            let spec = registry.get(name).expect(name).spec();
            assert_eq!(spec.schema["type"], "object");
            let required = spec.schema["required"].as_array().expect("required list");
            for field in ["path", "line", "column"] {
                assert!(
                    required.iter().any(|value| value == field),
                    "`{name}` must require `{field}`"
                );
                assert!(
                    spec.schema["properties"][field].is_object(),
                    "`{name}` must document `{field}`"
                );
            }
        }
    }

    #[test]
    fn lsp_file_tools_require_only_a_path() {
        let registry = ToolRegistry::with_builtins();
        for name in ["lsp.symbols", "lsp.diagnostics"] {
            let spec = registry.get(name).expect(name).spec();
            let required = spec.schema["required"].as_array().expect("required list");
            assert_eq!(required.len(), 1, "`{name}` requires exactly `path`");
            assert_eq!(required[0], "path");
        }
    }

    #[test]
    fn lsp_hover_declares_read_permission() {
        let req = LspHover
            .permission(&json!({"path": "src/main.rs", "line": 3, "column": 7}))
            .unwrap();
        assert_eq!(req.action, ActionKind::Read);
        assert_eq!(req.verb, "lsp.hover");
        assert_eq!(req.resource, "src/main.rs");
        assert_eq!(req.side_effect, SideEffect::None);
    }

    #[test]
    fn lsp_definition_declares_read_permission() {
        let req = LspDefinition
            .permission(&json!({"path": "src/main.rs", "line": 3, "column": 7}))
            .unwrap();
        assert_eq!(req.action, ActionKind::Read);
        assert_eq!(req.verb, "lsp.definition");
        assert_eq!(req.resource, "src/main.rs");
        assert_eq!(req.side_effect, SideEffect::None);
    }

    #[test]
    fn lsp_references_declares_read_permission() {
        let req = LspReferences
            .permission(&json!({"path": "src/main.rs", "line": 3, "column": 7}))
            .unwrap();
        assert_eq!(req.action, ActionKind::Read);
        assert_eq!(req.verb, "lsp.references");
        assert_eq!(req.resource, "src/main.rs");
        assert_eq!(req.side_effect, SideEffect::None);
    }

    #[test]
    fn lsp_symbols_declares_read_permission() {
        let req = LspSymbols
            .permission(&json!({"path": "src/main.rs"}))
            .unwrap();
        assert_eq!(req.action, ActionKind::Read);
        assert_eq!(req.verb, "lsp.symbols");
        assert_eq!(req.resource, "src/main.rs");
        assert_eq!(req.side_effect, SideEffect::None);
    }

    #[test]
    fn lsp_diagnostics_declares_read_permission() {
        let req = LspDiagnostics
            .permission(&json!({"path": "src/main.rs"}))
            .unwrap();
        assert_eq!(req.action, ActionKind::Read);
        assert_eq!(req.verb, "lsp.diagnostics");
        assert_eq!(req.resource, "src/main.rs");
        assert_eq!(req.side_effect, SideEffect::None);
    }

    #[test]
    fn lsp_tools_refuse_a_permission_without_a_path() {
        assert!(
            LspHover
                .permission(&json!({"line": 0, "column": 0}))
                .is_err()
        );
        assert!(LspSymbols.permission(&json!({})).is_err());
        assert!(LspDiagnostics.permission(&json!({})).is_err());
    }

    #[test]
    fn lsp_tools_refuse_paths_outside_the_workspace() {
        let (_dir, ctx) = ctx();
        let err = LspHover
            .run(
                &json!({"path": "../../etc/passwd", "line": 0, "column": 0}),
                &ctx,
            )
            .unwrap_err();
        assert!(matches!(err, VikramError::PolicyDenied { .. }));
        let err = LspSymbols
            .run(&json!({"path": "../../etc/passwd"}), &ctx)
            .unwrap_err();
        assert!(matches!(err, VikramError::PolicyDenied { .. }));
        let err = LspDiagnostics
            .run(&json!({"path": "/etc/passwd"}), &ctx)
            .unwrap_err();
        assert!(matches!(err, VikramError::PolicyDenied { .. }));
    }

    #[test]
    fn lsp_tools_report_a_missing_file_without_starting_a_server() {
        let (_dir, ctx) = ctx();
        let out = LspDefinition
            .run(&json!({"path": "nope.rs", "line": 0, "column": 0}), &ctx)
            .unwrap();
        assert!(!out.ok, "{}", out.content);
        assert!(
            out.content.contains("is not a readable file"),
            "{}",
            out.content
        );
        let out = LspSymbols.run(&json!({"path": "nope.rs"}), &ctx).unwrap();
        assert!(!out.ok, "{}", out.content);
        assert!(
            out.content.contains("is not a readable file"),
            "{}",
            out.content
        );
    }

    #[test]
    fn lsp_tools_reject_a_missing_or_non_integer_position() {
        let (_dir, ctx) = ctx();
        WriteFile
            .run(&json!({"path": "a.rs", "content": "fn main() {}"}), &ctx)
            .unwrap();
        let err = LspHover
            .run(&json!({"path": "a.rs", "column": 0}), &ctx)
            .unwrap_err();
        assert!(matches!(err, VikramError::ProviderRejected(_)), "{err:?}");
        let err = LspHover
            .run(&json!({"path": "a.rs", "line": "0", "column": 0}), &ctx)
            .unwrap_err();
        assert!(matches!(err, VikramError::ProviderRejected(_)), "{err:?}");
    }

    #[test]
    fn lsp_on_an_unsupported_extension_names_the_override_variable() {
        let (_dir, ctx) = ctx();
        WriteFile
            .run(&json!({"path": "notes.txt", "content": "hello"}), &ctx)
            .unwrap();
        let out = LspHover
            .run(&json!({"path": "notes.txt", "line": 0, "column": 0}), &ctx)
            .unwrap();
        assert!(!out.ok, "{}", out.content);
        assert!(
            out.content.contains("no language server for .txt"),
            "{}",
            out.content
        );
        assert!(out.content.contains("VIKRAM_LSP_TXT"), "{}", out.content);
    }

    // Real execution, not shape checks: each of these runs the tool and then looks at what it did
    // to the filesystem, the shell or the network, so a tool that answers but does nothing fails.
    #[test]
    fn the_file_tools_create_edit_list_and_delete_for_real() {
        let (dir, ctx) = ctx();
        let written = WriteFile
            .run(&json!({"path": "notes/todo.md", "content": "# one\n"}), &ctx)
            .unwrap();
        assert!(written.ok, "{}", written.content);
        assert_eq!(
            std::fs::read_to_string(dir.path().join("notes/todo.md")).unwrap(),
            "# one\n",
            "the bytes are on disk, not only in the answer"
        );

        let edited = EditFile
            .run(
                &json!({"path": "notes/todo.md", "old_string": "# one", "new_string": "# two"}),
                &ctx,
            )
            .unwrap();
        assert!(edited.ok, "{}", edited.content);
        assert_eq!(
            ReadFile
                .run(&json!({"path": "notes/todo.md"}), &ctx)
                .unwrap()
                .content,
            "# two\n",
            "reading back shows the edit, not the original"
        );

        let listed = ListDir.run(&json!({"path": "notes"}), &ctx).unwrap();
        assert!(listed.content.contains("todo.md"), "{}", listed.content);

        let deleted = DeleteFile
            .run(&json!({"path": "notes/todo.md"}), &ctx)
            .unwrap();
        assert!(deleted.ok, "{}", deleted.content);
        assert!(
            !dir.path().join("notes/todo.md").exists(),
            "the file is gone from disk, not just from the answer"
        );
    }

    #[test]
    fn an_edit_that_cannot_find_its_old_string_fails_instead_of_guessing() {
        let (dir, ctx) = ctx();
        WriteFile
            .run(&json!({"path": "a.txt", "content": "alpha\n"}), &ctx)
            .unwrap();
        let out = EditFile.run(
            &json!({"path": "a.txt", "old_string": "nothing like this", "new_string": "beta"}),
            &ctx,
        );
        let failed = match out {
            Err(_) => true,
            Ok(output) => !output.ok,
        };
        assert!(failed, "an edit with no match must not report success");
        assert_eq!(
            std::fs::read_to_string(dir.path().join("a.txt")).unwrap(),
            "alpha\n",
            "a failed edit leaves the file alone"
        );
    }

    #[test]
    fn a_shell_command_returns_its_real_output_and_exit_code() {
        let (_dir, ctx) = ctx();
        let out = RunShell
            .run(&json!({"command": "printf hello; exit 3"}), &ctx)
            .unwrap();
        assert!(out.content.contains("hello"), "{}", out.content);
        assert!(out.content.contains("[exit 3]"), "{}", out.content);
        assert!(!out.ok, "a non-zero exit is a failure, not a success");
    }

    #[test]
    fn search_finds_the_text_the_file_tools_wrote() {
        let (_dir, ctx) = ctx();
        WriteFile
            .run(
                &json!({"path": "src/lib.rs", "content": "pub fn marker_xyz() {}\n"}),
                &ctx,
            )
            .unwrap();
        let out = SearchText.run(&json!({"query": "marker_xyz"}), &ctx).unwrap();
        assert!(out.ok, "{}", out.content);
        assert!(out.content.contains("lib.rs"), "{}", out.content);
    }

    #[test]
    fn git_status_runs_against_a_real_repository() {
        let (dir, ctx) = ctx();
        let init = RunShell
            .run(&json!({"command": "git init -q ."}), &ctx)
            .unwrap();
        assert!(init.ok, "{}", init.content);
        WriteFile
            .run(&json!({"path": "untracked.txt", "content": "x"}), &ctx)
            .unwrap();
        let out = Git.run(&json!({"subcommand": "status"}), &ctx).unwrap();
        assert!(out.ok, "{}", out.content);
        assert!(
            out.content.contains("untracked.txt"),
            "status lists the file that is not committed yet: {}",
            out.content
        );
        assert!(dir.path().join(".git").exists());
    }

    #[test]
    fn the_terminal_tool_drives_a_real_pty() {
        let (_dir, ctx) = ctx();
        let opened = TerminalTool
            .run(&json!({"action": "open", "session": "smoke"}), &ctx)
            .unwrap();
        assert!(opened.ok, "{}", opened.content);
        TerminalTool
            .run(
                &json!({"action": "write", "session": "smoke", "input": "echo pty-works\n"}),
                &ctx,
            )
            .unwrap();
        let mut seen = String::new();
        for _ in 0..40 {
            let read = TerminalTool
                .run(&json!({"action": "read", "session": "smoke"}), &ctx)
                .unwrap();
            seen.push_str(&read.content);
            if seen.contains("pty-works") {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(50));
        }
        let _ = TerminalTool.run(&json!({"action": "close", "session": "smoke"}), &ctx);
        assert!(
            seen.contains("pty-works"),
            "a real PTY echoes back what was typed: {seen}"
        );
    }

    #[test]
    fn web_search_returns_sources_or_says_why_it_could_not() {
        let (_dir, ctx) = ctx();
        let out = WebSearch
            .run(&json!({"query": "rust programming language", "limit": 3}), &ctx)
            .unwrap();
        if out.ok {
            assert!(
                out.content.contains("http"),
                "a successful search carries links: {}",
                out.content
            );
        } else {
            assert!(
                out.content.trim().len() > 10,
                "a failed search explains itself: {}",
                out.content
            );
        }
    }

    #[test]
    fn memory_and_tasks_round_trip_through_their_own_store() {
        let dir = tempfile::tempdir().unwrap();
        let _env = crate::config::lock_security_env();
        unsafe { std::env::set_var("VIKRAM_HOME", dir.path().join("home")) };
        let ctx = ToolContext {
            workspace_root: dir.path().to_path_buf(),
        };

        let saved = MemorySave
            .run(&json!({"key": "tool-probe", "value": "remembered"}), &ctx)
            .unwrap();
        assert!(saved.ok, "{}", saved.content);
        let found = MemorySearch
            .run(&json!({"query": "remembered"}), &ctx)
            .unwrap();
        assert!(
            found.content.contains("tool-probe"),
            "a saved key is findable by what it says: {}",
            found.content
        );
        let listed = MemoryList.run(&json!({}), &ctx).unwrap();
        assert!(listed.content.contains("tool-probe"), "{}", listed.content);
        let removed = MemoryDelete
            .run(&json!({"key": "tool-probe"}), &ctx)
            .unwrap();
        assert!(removed.ok, "{}", removed.content);
        let after = MemoryList.run(&json!({}), &ctx).unwrap();
        assert!(
            !after.content.contains("tool-probe"),
            "a deleted key is gone from the listing: {}",
            after.content
        );

        let created = Task
            .run(&json!({"action": "create", "title": "probe"}), &ctx)
            .unwrap();
        assert!(created.ok, "{}", created.content);
        let tasks = Task.run(&json!({"action": "list"}), &ctx).unwrap();
        assert!(tasks.content.contains("probe"), "{}", tasks.content);
    }

    #[test]
    fn the_host_inspection_tools_answer_without_a_model() {
        let (_dir, ctx) = ctx();
        let answers = vec![
            (
                "process",
                Process.run(&json!({"action": "list"}), &ctx).unwrap(),
            ),
            ("skill.list", SkillList.run(&json!({}), &ctx).unwrap()),
            ("plugin.list", PluginList.run(&json!({}), &ctx).unwrap()),
        ];
        for (name, out) in answers {
            assert!(out.ok, "{name} failed: {}", out.content);
            assert!(!out.content.trim().is_empty(), "{name} answered nothing");
        }
    }
}
