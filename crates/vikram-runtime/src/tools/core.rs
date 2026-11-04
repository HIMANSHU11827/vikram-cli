use serde_json::{Value, json};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use vikram_contracts::{ActionKind, PermissionRequest, SideEffect, Trust, VikramError};

use crate::config::resolve_within;

use crate::tools::*;

/// Output larger than this is truncated with an explicit marker rather than silently cut.
pub const MAX_TOOL_OUTPUT: usize = 20_000;

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
            .finish()
    }
}

impl ToolContext {
    /// The on-by-default context: sandbox resolved from the caller's settings, no spool, no runtime.
    pub fn new(workspace_root: PathBuf, settings: &crate::config::SandboxSettings) -> Self {
        let sandbox = if settings.enabled {
            crate::sandbox::Sandbox::detect(
                settings.allow_network,
                settings.read_only_paths.clone(),
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
        registry.register(Box::new(BuildRun));
        registry.register(Box::new(TestRun));
        registry.register(Box::new(VerifyRun));
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

pub(crate) fn truncated(mut text: String) -> String {
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
