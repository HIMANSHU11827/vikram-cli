use serde_json::{Value, json};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use vikram_contracts::{
    ActionKind, PermissionMode, PermissionRequest, SideEffect, Trust, VikramError,
};

use crate::config::resolve_within;

use crate::tools::*;

/// Output larger than this is elided with an explicit marker rather than silently cut. The head
/// and the tail both survive — the head for ordering context, the tail because a build or a test
/// run puts its reason for existing at the end.
pub const MAX_TOOL_OUTPUT: usize = 20_000;
const TRUNCATION_HEAD: usize = 15_000;
const TRUNCATION_TAIL: usize = 4_000;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ToolOutput {
    pub content: String,
    pub ok: bool,
}

#[derive(Clone)]
pub struct ToolContext {
    pub workspace_root: PathBuf,
    /// OS-level isolation for commands this tool spawns. Policy still gates both.
    pub sandbox: crate::sandbox::Sandbox,
    /// Where this session's oversized tool results were spooled, and how `output.read` reaches them.
    pub spill: crate::spill::Spill,
    /// The live runtime, for tools whose subject is the running system rather than a file:
    /// delegation spawns sessions. Absent when a tool runs outside a session (a direct CLI call).
    pub(crate) runtime: Option<Arc<crate::runtime::RuntimeInner>>,
    /// The session the call is being made from, so a delegation can record its parent and depth.
    pub session: Option<vikram_contracts::SessionId>,
    /// The agent identity the call runs as, when the turn belongs to a background agent.
    pub agent: Option<String>,
    /// The bot whose session this call comes from, when it comes from one. A bot keeps its notes in
    /// its own memory scope, so knowing which bot is speaking is what stops two bots sharing a brain.
    pub bot: Option<String>,
    /// Cancellation handle for the turn. Blocking tools that honor it can stop early;
    /// the host still enforces cancellation by aborting the spawn_blocking task.
    pub cancel: Option<vikram_contracts::CancellationToken>,
}

impl Default for ToolContext {
    fn default() -> Self {
        Self {
            workspace_root: PathBuf::new(),
            sandbox: crate::sandbox::Sandbox::disabled("default"),
            spill: crate::spill::Spill::disabled(),
            runtime: None,
            session: None,
            agent: None,
            bot: None,
            cancel: None,
        }
    }
}

impl std::fmt::Debug for ToolContext {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("ToolContext")
            .field("workspace_root", &self.workspace_root)
            .field("sandbox", &self.sandbox)
            .field("spill", &self.spill)
            .field("runtime", &self.runtime.as_ref().map(|_| "runtime"))
            .field("session", &self.session)
            .field("agent", &self.agent)
            .field("bot", &self.bot)
            .field("cancel", &self.cancel.as_ref().map(|_| "token"))
            .finish()
    }
}

impl ToolContext {
    /// The on-by-default context: sandbox resolved from the caller's settings, no spool, no runtime.
    pub fn new(workspace_root: PathBuf, settings: &crate::config::SandboxSettings) -> Self {
        let sandbox = if settings.enabled {
            crate::sandbox::Sandbox::detect_with_profile(
                settings.profile,
                settings.allow_network,
                settings.read_only_paths.clone(),
                crate::sandbox::ResourceLimits::default(),
            )
        } else {
            crate::sandbox::Sandbox::disabled("disabled in config")
        };
        Self {
            workspace_root,
            sandbox,
            spill: crate::spill::Spill::disabled(),
            runtime: None,
            session: None,
            agent: None,
            bot: None,
            cancel: None,
        }
    }
}

/// Read back a slice of a tool result that was too large to stay in the transcript.
///
/// The digest the host journaled names the handle; this is the only way to get at the middle of an
/// elided result, and it can only address the spool belonging to the session making the call.
pub(crate) struct OutputRead;

impl Tool for OutputRead {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "output.read".into(),
            description: "Read a slice of a spooled tool result by its handle.".into(),
            schema: json!({
                "type": "object",
                "properties": {
                    "id": {"type": "string", "description": "the handle named in the elided-output marker"},
                    "offset": {"type": "integer", "description": "character offset to read from"},
                    "limit": {"type": "integer", "description": "characters to return"}
                },
                "required": ["id"]
            }),
        }
    }

    fn permission(&self, args: &Value) -> Result<PermissionRequest, VikramError> {
        let id = args
            .get("id")
            .and_then(Value::as_str)
            .unwrap_or("output")
            .to_owned();
        Ok(PermissionRequest::new(
            ActionKind::Read,
            "output.read",
            format!("spool/{id}"),
            SideEffect::None,
            Trust::Model,
        ))
    }

    fn run(&self, args: &Value, ctx: &ToolContext) -> Result<ToolOutput, VikramError> {
        let id = arg_str(args, "id")?;
        let offset = args.get("offset").and_then(Value::as_u64).unwrap_or(0) as usize;
        let limit = args
            .get("limit")
            .and_then(Value::as_u64)
            .unwrap_or(crate::spill::SPILL_READ_LIMIT as u64) as usize;
        let read = ctx.spill.read(&id, offset, limit);
        Ok(match read {
            Ok(content) => ToolOutput { content, ok: true },
            Err(error) => ToolOutput {
                content: error,
                ok: false,
            },
        })
    }
}

pub use crate::provider::ToolSpec;

pub trait Tool: Send + Sync {
    fn spec(&self) -> ToolSpec;
    /// How the host should describe this call to policy.
    fn permission(&self, args: &Value) -> Result<PermissionRequest, VikramError>;
    fn run(&self, args: &Value, ctx: &ToolContext) -> Result<ToolOutput, VikramError>;
    /// What the operator will see change if this call is approved. The default is no preview: a
    /// tool that cannot describe its effect opts out instead of showing a guess. The context is the
    /// same one `run` gets, because a preview of a path needs the workspace root to resolve it.
    fn preview(
        &self,
        _call: &vikram_contracts::ToolCall,
        _ctx: &ToolContext,
    ) -> Option<crate::types::ApprovalPreview> {
        None
    }
}

/// What a tool needs from the posture before its schema is worth sending.
///
/// This is the schema gate, never the permission gate. A hidden tool is still registered and still
/// callable; a shown tool still has to pass policy on every call. The gate exists because 103 schemas
/// on every request is context the transcript needs.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ToolGate {
    /// The strongest action the tool reports to policy. `Read` is the only action a read-only
    /// posture runs, which is what keeps writes and commands out of plan mode.
    pub action: ActionKind,
    /// The tool's own permission is `SideEffect::Destructive`: it exists to remove something.
    /// Tools that are only *sometimes* destructive because of their arguments (`shell.run`,
    /// `terminal`, `process`) are not flagged: the arguments decide, and policy gates each call.
    pub destructive: bool,
    /// The tool cannot succeed unless a human answers.
    pub interactive: bool,
}

const fn gate(action: ActionKind, destructive: bool, interactive: bool) -> ToolGate {
    ToolGate {
        action,
        destructive,
        interactive,
    }
}

/// The posture in force, plus whether its policy can let an irreversible call run.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ToolSurface {
    pub mode: PermissionMode,
    /// Widen this when the rule book already covers a destructive verb. The surface is a context
    /// budget, so a tool the operator allowed must not disappear from the model's list.
    pub destructive_allowed: bool,
}

impl ToolSurface {
    /// The posture's own answer. `plan` is read-only and `unattended` cannot ask, so neither can
    /// let a destructive call run; every other posture either asks a human or auto-approves.
    pub fn for_mode(mode: PermissionMode) -> Self {
        Self {
            mode,
            destructive_allowed: !matches!(mode, PermissionMode::Plan | PermissionMode::Unattended),
        }
    }

    pub fn admits(&self, gate: ToolGate) -> bool {
        if gate.interactive && self.mode == PermissionMode::Unattended {
            return false;
        }
        if gate.destructive && !self.destructive_allowed {
            return false;
        }
        if self.mode == PermissionMode::Plan {
            // Exactly the posture's own test for a read: `PermissionMode::resolve` refuses every
            // non-read in plan mode, so advertising one would spend a schema on a refusal.
            return gate.action == ActionKind::Read;
        }
        true
    }
}

/// The gate for one tool, keyed by the name the model calls. `None` for a family this table does
/// not know — MCP servers, the daemon's scheduler — which `gate_of` keeps visible everywhere.
///
/// The classification follows each tool's own `permission`: a tool that can report
/// `ActionKind::Write` is a write tool even when one subcommand only reads, because the schema is a
/// promise about what the tool can do. `update_plan` is the one exception: its permission is
/// `Write` with no side effect (it writes session state, not the world), and a read-only posture
/// still needs it to report progress.
fn gate_for(name: &str) -> Option<ToolGate> {
    Some(match name {
        // Reads and searches: every posture, including plan.
        "fs.read"
        | "fs.list"
        | "search.text"
        | "session.search"
        | "output.read"
        | "git"
        | "git.status"
        | "git.diff"
        | "git.log"
        | "git_status"
        | "git_diff"
        | "git_log"
        | "lsp.hover"
        | "lsp.definition"
        | "lsp.references"
        | "lsp.symbols"
        | "lsp.diagnostics"
        | "code_search"
        | "code.review"
        | "skill.list"
        | "skill.load"
        | "skill.search"
        | "skill.read"
        | "memory.search"
        | "memory.list"
        | "hive.list"
        | "hive.show"
        | "bot.list"
        | "bot.show"
        | "agent.list"
        | "agent.show"
        | "plugin.list"
        | "plugin.validate"
        | "plugin.inspect"
        | "plugin.deps"
        | "worker.collect"
        | "permission.check"
        | "browser.text"
        | "browser.sessions"
        | "computer.windows"
        | "computer.status"
        | "computer.ocr"
        | "computer.read_screen"
        | "computer.clipboard_read" => gate(ActionKind::Read, false, false),

        // The plan is how a read-only posture says what it intends to do, so it stays everywhere.
        "update_plan" => gate(ActionKind::Read, false, false),

        // Needs a human at the keyboard: an unattended run cannot answer, so the schema is dead.
        "ask_user" | "message.poll" | "message.card" | "message.draft" | "message.react" => {
            gate(ActionKind::Read, false, true)
        }

        // Network reads: allowed wherever the posture is not read-only.
        "web.search" | "browser.fetch" | "browser.dom" | "browser.links" | "browser.open"
        | "browser.screenshot" => gate(ActionKind::Network, false, false),

        // Writes inside the workspace or the runtime's own state.
        "fs.write"
        | "fs.edit"
        | "memory.save"
        | "memory.note"
        | "memory.consolidate"
        | "hive.create"
        | "bot.create"
        | "plugin.enable"
        | "plugin.disable"
        | "codeindex"
        | "task"
        | "computer.screenshot"
        | "computer.clipboard_write" => gate(ActionKind::Write, false, false),

        // Runs something: a command, a build, a browser page, a virtual keyboard.
        "shell.run"
        | "terminal"
        | "build.run"
        | "test.run"
        | "verify"
        | "process"
        | "diagnostics.collect"
        | "git_commit"
        | "git_branch"
        | "git_stash"
        | "git.commit"
        | "git.branch"
        | "git.stash"
        | "build_check"
        | "test_run"
        | "plugin.execute"
        | "browser.eval"
        | "browser.click"
        | "browser.type"
        | "browser.scroll"
        | "browser.close"
        | "computer.click"
        | "computer.type"
        | "computer.key"
        | "computer.scroll"
        | "computer.notify" => gate(ActionKind::Execute, false, false),

        "hive.delegate" | "spawn_worker" => gate(ActionKind::Delegate, false, false),

        // Irreversible: only where the policy can already let it run.
        "fs.delete" | "memory.delete" | "hive.delete" | "relation" | "plugin.install"
        | "plugin.uninstall" => gate(ActionKind::Write, true, false),

        _ => return None,
    })
}

/// The gate for a tool, including families this table does not know. An unknown family stays
/// visible in every posture: hiding a tool we cannot classify would take away a capability, and a
/// read-only posture refuses the call anyway.
pub fn gate_of(name: &str) -> ToolGate {
    gate_for(name).unwrap_or(gate(ActionKind::Read, false, false))
}

/// The schemas a request in `mode` carries.
pub fn schemas_for_mode(mode: PermissionMode, specs: Vec<ToolSpec>) -> Vec<ToolSpec> {
    schemas_for_surface(ToolSurface::for_mode(mode), specs)
}

/// The schemas a request carries under an explicit surface.
pub fn schemas_for_surface(surface: ToolSurface, specs: Vec<ToolSpec>) -> Vec<ToolSpec> {
    specs
        .into_iter()
        .filter(|spec| surface.admits(gate_of(&spec.name)))
        .collect()
}

/// Extra tool families that the host installs at startup.
///
/// The runtime cannot depend on the crates that own some families — the daemon owns the scheduler
/// and already depends on the runtime — so it offers one seam instead: a family registers a
/// factory once, and every registry this process builds calls it.
type Extension = fn(&ToolRegistry);

fn extensions() -> &'static std::sync::Mutex<Vec<Extension>> {
    static EXTENSIONS: std::sync::OnceLock<std::sync::Mutex<Vec<Extension>>> =
        std::sync::OnceLock::new();
    EXTENSIONS.get_or_init(Default::default)
}

/// Install a family. Calling it twice with the same factory is a no-op, so a host that installs
/// and a registry that is built later cannot produce two tools with one name.
pub fn install_extension(factory: Extension) {
    let mut list = extensions().lock().expect("extensions lock");
    if !list.iter().any(|known| *known as usize == factory as usize) {
        list.push(factory);
    }
}

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
        registry.register(Box::new(GitStatus));
        registry.register(Box::new(GitDiff));
        registry.register(Box::new(GitLog));
        registry.register(Box::new(GitCommit));
        registry.register(Box::new(GitBranch));
        registry.register(Box::new(GitStash));
        registry.register(Box::new(GitStatusTool));
        registry.register(Box::new(GitDiffTool));
        registry.register(Box::new(GitLogTool));
        registry.register(Box::new(GitCommitTool));
        registry.register(Box::new(GitBranchTool));
        registry.register(Box::new(GitStashTool));
        registry.register(Box::new(BuildRun));
        registry.register(Box::new(TestRun));
        registry.register(Box::new(VerifyRun));
        registry.register(Box::new(TestRunTool));
        registry.register(Box::new(BuildCheck));
        registry.register(Box::new(MemorySave));
        registry.register(Box::new(MemorySearch));
        registry.register(Box::new(MemoryList));
        registry.register(Box::new(MemoryDelete));
        registry.register(Box::new(MemoryNote));
        registry.register(Box::new(MemoryConsolidate));
        registry.register(Box::new(Relation));
        registry.register(Box::new(SessionSearch));
        registry.register(Box::new(WebSearch));
        registry.register(Box::new(HiveCreate));
        registry.register(Box::new(HiveList));
        registry.register(Box::new(HiveShow));
        registry.register(Box::new(HiveDelegate));
        registry.register(Box::new(HiveDelete));
        registry.register(Box::new(crate::orchestrator::SpawnWorker));
        registry.register(Box::new(crate::orchestrator::WorkerCollect));
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
        registry.register(Box::new(UpdatePlan));
        registry.register(Box::new(AskUser));
        registry.register(Box::new(MessagePoll));
        registry.register(Box::new(MessageCard));
        registry.register(Box::new(MessageDraft));
        registry.register(Box::new(MessageReact));
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
        registry.register(Box::new(CodeIndexTool));
        registry.register(Box::new(CodeSearch));
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
        registry.register(Box::new(OutputRead));
        crate::computer::register(&registry);
        for factory in extensions().lock().expect("extensions lock").iter() {
            factory(&registry);
        }
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

    /// The schemas to send with a request in `mode`. What the model is told exists — not what the
    /// host will run: every tool stays registered, and every call still passes policy.
    pub fn specs_for_mode(&self, mode: PermissionMode) -> Vec<ToolSpec> {
        schemas_for_mode(mode, self.specs())
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
        self.disabled
            .lock()
            .expect("disabled lock")
            .insert(name.to_owned());
        self.persist_disabled();
    }

    fn persist_disabled(&self) {
        let disabled: Vec<String> = self
            .disabled
            .lock()
            .expect("disabled lock")
            .iter()
            .cloned()
            .collect();
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

impl Default for ToolRegistry {
    fn default() -> Self {
        Self::new()
    }
}

pub(crate) fn truncated(text: String) -> String {
    if text.len() <= MAX_TOOL_OUTPUT {
        return text;
    }
    let head_cut = floor_boundary(&text, TRUNCATION_HEAD);
    let tail_start = ceil_boundary(&text, text.len() - TRUNCATION_TAIL);
    let dropped = tail_start - head_cut;
    let mut out = String::with_capacity(head_cut + (text.len() - tail_start) + 96);
    out.push_str(&text[..head_cut]);
    out.push_str(&format!(
        "\n… [{dropped} bytes elided — the middle was dropped; the tail follows]\n"
    ));
    out.push_str(&text[tail_start..]);
    out
}

/// The largest char boundary at or below `target`, so a slice cut never splits a character.
fn floor_boundary(text: &str, target: usize) -> usize {
    let mut cut = target.min(text.len());
    while cut > 0 && !text.is_char_boundary(cut) {
        cut -= 1;
    }
    cut
}

/// The smallest char boundary at or above `target`.
fn ceil_boundary(text: &str, target: usize) -> usize {
    let mut cut = target.min(text.len());
    while cut < text.len() && !text.is_char_boundary(cut) {
        cut += 1;
    }
    cut
}

pub(crate) fn arg_str(args: &Value, key: &str) -> Result<String, VikramError> {
    args.get(key)
        .and_then(Value::as_str)
        .map(str::to_owned)
        .ok_or_else(|| VikramError::ProviderRejected(format!("missing string argument `{key}`")))
}

pub(crate) fn canonical_resource(root: &Path, raw: &str) -> Result<(PathBuf, String), VikramError> {
    let resolved =
        resolve_within(root, Path::new(raw)).ok_or_else(|| VikramError::PolicyDenied {
            reason: format!("`{raw}` is outside the workspace root `{}`", root.display()),
        })?;
    let display = resolved.to_string_lossy().into_owned();
    Ok((resolved, display))
}

#[cfg(test)]
mod gate_tests {
    use super::*;

    fn spec(name: &str) -> ToolSpec {
        ToolSpec {
            name: name.to_owned(),
            description: String::new(),
            schema: json!({"type": "object"}),
        }
    }

    fn builtins() -> ToolRegistry {
        ToolRegistry::with_builtins()
    }

    /// Names advertised in `mode`, taken from every registered spec so a machine with tools
    /// disabled in `disabled_tools.json` cannot change the answer.
    fn names_in(mode: vikram_contracts::PermissionMode) -> Vec<String> {
        let all: Vec<ToolSpec> = builtins()
            .all()
            .into_iter()
            .map(|tool| tool.spec())
            .collect();
        schemas_for_mode(mode, all)
            .into_iter()
            .map(|spec| spec.name)
            .collect()
    }

    fn has(names: &[String], name: &str) -> bool {
        names.iter().any(|n| n == name)
    }

    #[test]
    fn read_only_mode_omits_write_and_execute_tools() {
        let names = names_in(vikram_contracts::PermissionMode::Plan);

        for hidden in ["fs.write", "fs.edit", "fs.delete", "shell.run", "terminal"] {
            assert!(
                !has(&names, hidden),
                "plan mode must not advertise `{hidden}`"
            );
        }
        for kept in ["fs.read", "fs.list", "search.text", "update_plan"] {
            assert!(has(&names, kept), "plan mode still needs `{kept}`");
        }
        assert!(
            names.len() < builtins().all().len(),
            "the gate actually removes something"
        );
    }

    #[test]
    fn edit_mode_includes_write_and_execute_tools() {
        let names = names_in(vikram_contracts::PermissionMode::AcceptEdits);

        for kept in [
            "fs.write",
            "fs.edit",
            "shell.run",
            "test.run",
            "update_plan",
        ] {
            assert!(has(&names, kept), "edit-ok must advertise `{kept}`");
        }
    }

    #[test]
    fn destructive_tools_need_a_policy_that_allows_them() {
        let specs = vec![spec("fs.read"), spec("fs.write"), spec("fs.delete")];

        let asked = schemas_for_mode(vikram_contracts::PermissionMode::Ask, specs.clone());
        assert!(asked.iter().any(|s| s.name == "fs.delete"));

        let held = schemas_for_surface(
            ToolSurface {
                mode: vikram_contracts::PermissionMode::Ask,
                destructive_allowed: false,
            },
            specs.clone(),
        );
        let names: Vec<&str> = held.iter().map(|s| s.name.as_str()).collect();
        assert!(!names.contains(&"fs.delete"), "a held policy hides deletes");
        assert!(
            names.contains(&"fs.write"),
            "it does not hide ordinary writes"
        );

        let unattended = names_in(vikram_contracts::PermissionMode::Unattended);
        assert!(!has(&unattended, "fs.delete"));
        assert!(
            has(&unattended, "fs.write"),
            "an allow rule can still cover a write"
        );
    }

    #[test]
    fn unattended_mode_hides_tools_that_need_a_human() {
        let names = names_in(vikram_contracts::PermissionMode::Unattended);
        for hidden in ["ask_user", "message.poll", "message.card"] {
            assert!(
                !has(&names, hidden),
                "nobody can answer `{hidden}` in an unattended run"
            );
        }
        assert!(has(&names, "update_plan"));
    }

    #[test]
    fn update_plan_is_advertised_in_every_mode() {
        for mode in vikram_contracts::PermissionMode::all() {
            let names = names_in(mode);
            assert!(
                has(&names, "update_plan"),
                "`{}` must keep the plan tool",
                mode.as_str()
            );
            assert!(
                has(&names, "fs.read"),
                "`{}` must keep the read tools",
                mode.as_str()
            );
        }
    }

    #[test]
    fn the_gated_surface_is_pinned_per_mode() {
        // Counted off every registered spec, so a machine with tools disabled in
        // `disabled_tools.json` cannot change the answer. A new tool, or a gate that stops biting,
        // moves one of these numbers and fails here on purpose.
        let all: Vec<ToolSpec> = builtins()
            .all()
            .into_iter()
            .map(|tool| tool.spec())
            .collect();
        let counts: Vec<(&str, usize)> = vikram_contracts::PermissionMode::all()
            .into_iter()
            .map(|mode| (mode.as_str(), schemas_for_mode(mode, all.clone()).len()))
            .collect();
        assert_eq!(
            counts,
            vec![
                ("ask", 103),
                ("edit-ok", 103),
                ("allow-list", 103),
                ("smart", 103),
                ("unattended", 92),
                ("yolo", 103),
                ("plan", 50),
            ]
        );
    }

    #[test]
    fn every_builtin_tool_is_classified_and_the_count_is_pinned() {
        let names = builtins().names();
        let unknown: Vec<&String> = names
            .iter()
            .filter(|name| gate_for(name).is_none())
            .collect();
        assert!(unknown.is_empty(), "unclassified tools: {unknown:?}");
        assert_eq!(
            names.len(),
            103,
            "the builtin tool surface changed; classify the new tool in `gate_for` and update this \
             number only when the change is deliberate"
        );
        for name in &names {
            assert!(
                gate_for(name).is_some(),
                "`{name}` has no gate; add it to `gate_for` so a mode cannot silently hide it"
            );
        }
    }
}
