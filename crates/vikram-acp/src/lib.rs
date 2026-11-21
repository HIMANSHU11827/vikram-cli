//! Agent Client Protocol (ACP) — client and server over JSON-RPC 2.0 on stdio.
//!
//! EXPERIMENTAL — no crate in the workspace depends on this yet. Known gaps before use:
//! `new_session` cannot pass `mcpServers`. `WorkspaceAgentRequestHandler` implements
//! `session/request_permission`, `fs/read_text_file`, and `fs/write_text_file` for real,
//! confined to a workspace root. The `terminal/*` methods stay `-32601` unless the handler is
//! built with [`WorkspaceAgentRequestHandler::with_terminal_support`]; those terminals are
//! real PTYs (portable-pty) and `cwd`-confined. ACP has no terminal-stdin method, so there is
//! no way to feed a running program input after `terminal/create`.
//!
//! ACP connects a *client* (IDE/host) to an *agent* (Vikram or another compatible agent).
//! It is distinct from MCP (agent↔tools) and A2A (agent↔agent).

use portable_pty::{Child as PtyChild, CommandBuilder, MasterPty, PtySize, native_pty_system};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::HashMap;
use std::io::{BufRead, BufReader, Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::{Receiver, RecvTimeoutError};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use thiserror::Error;

#[derive(Debug, Error)]
pub enum AcpError {
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
    #[error("json: {0}")]
    Json(#[from] serde_json::Error),
    #[error("protocol: {0}")]
    Protocol(String),
    #[error("not connected")]
    NotConnected,
}

pub type Result<T> = std::result::Result<T, AcpError>;

// ---------- JSON-RPC 2.0 ----------

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct JsonRpcRequest {
    pub jsonrpc: String,
    pub id: Value,
    pub method: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub params: Option<Value>,
}

impl JsonRpcRequest {
    pub fn new(id: i64, method: impl Into<String>, params: Option<Value>) -> Self {
        Self {
            jsonrpc: "2.0".into(),
            id: Value::Number(id.into()),
            method: method.into(),
            params,
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct JsonRpcResponse {
    pub jsonrpc: String,
    pub id: Value,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub result: Option<Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<JsonRpcError>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct JsonRpcError {
    pub code: i64,
    pub message: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub data: Option<Value>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct JsonRpcNotification {
    pub jsonrpc: String,
    pub method: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub params: Option<Value>,
}

// ---------- ACP types ----------

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct InitializeParams {
    #[serde(rename = "protocolVersion")]
    pub protocol_version: u32,
    #[serde(default, rename = "clientCapabilities")]
    pub client_capabilities: Value,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct InitializeResult {
    #[serde(rename = "protocolVersion")]
    pub protocol_version: u32,
    #[serde(default, rename = "agentCapabilities")]
    pub agent_capabilities: Value,
    #[serde(default, rename = "agentInfo")]
    pub agent_info: Option<Value>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct NewSessionParams {
    #[serde(rename = "cwd")]
    pub cwd: String,
    #[serde(default, rename = "mcpServers")]
    pub mcp_servers: Vec<Value>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct NewSessionResult {
    #[serde(rename = "sessionId")]
    pub session_id: String,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct PromptParams {
    #[serde(rename = "sessionId")]
    pub session_id: String,
    pub prompt: Vec<ContentBlock>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ContentBlock {
    #[serde(rename = "type")]
    pub kind: String,
    #[serde(default)]
    pub text: Option<String>,
}

impl ContentBlock {
    pub fn text(s: impl Into<String>) -> Self {
        Self {
            kind: "text".into(),
            text: Some(s.into()),
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct CancelParams {
    #[serde(rename = "sessionId")]
    pub session_id: String,
}

// ---------- agent→client request types ----------

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct PermissionOption {
    #[serde(rename = "optionId")]
    pub option_id: String,
    pub name: String,
    #[serde(default)]
    pub kind: Option<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct RequestPermissionParams {
    #[serde(rename = "sessionId")]
    pub session_id: String,
    #[serde(rename = "toolCall", default)]
    pub tool_call: Value,
    #[serde(default)]
    pub options: Vec<PermissionOption>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct PermissionOutcome {
    #[serde(rename = "outcome")]
    pub outcome: String,
    #[serde(default, rename = "optionId", skip_serializing_if = "Option::is_none")]
    pub option_id: Option<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct RequestPermissionResult {
    pub outcome: PermissionOutcome,
}

impl RequestPermissionResult {
    /// The agent asked and the answer was no — either a policy refusal or a dismissed prompt.
    /// Per the ACP spec this carries no `optionId`.
    pub fn cancelled() -> Self {
        Self {
            outcome: PermissionOutcome {
                outcome: "cancelled".into(),
                option_id: None,
            },
        }
    }

    /// The agent asked and one of its offered options was chosen.
    pub fn selected(option_id: impl Into<String>) -> Self {
        Self {
            outcome: PermissionOutcome {
                outcome: "selected".into(),
                option_id: Some(option_id.into()),
            },
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ReadTextFileParams {
    #[serde(rename = "sessionId")]
    pub session_id: String,
    pub path: String,
    #[serde(default)]
    pub line: Option<u64>,
    #[serde(default)]
    pub limit: Option<u64>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ReadTextFileResult {
    pub content: String,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct WriteTextFileParams {
    #[serde(rename = "sessionId")]
    pub session_id: String,
    pub path: String,
    pub content: String,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct TerminalEnvVariable {
    pub name: String,
    pub value: String,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct TerminalCreateParams {
    #[serde(rename = "sessionId")]
    pub session_id: String,
    pub command: String,
    #[serde(default)]
    pub cwd: Option<String>,
    #[serde(default)]
    pub env: Option<Vec<TerminalEnvVariable>>,
    #[serde(default, rename = "outputByteLimit")]
    pub output_byte_limit: Option<usize>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct TerminalCreateResult {
    #[serde(rename = "terminalId")]
    pub terminal_id: String,
}

/// `terminal/output`, `terminal/release`, `terminal/wait_for_exit`, and `terminal/kill`
/// all take the same `{sessionId, terminalId}` shape.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct TerminalIdParams {
    #[serde(rename = "sessionId")]
    pub session_id: String,
    #[serde(rename = "terminalId")]
    pub terminal_id: String,
}

/// Exit information shared by `terminal/output` (`exitStatus`) and `terminal/wait_for_exit`.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct TerminalExitStatus {
    #[serde(rename = "exitCode")]
    pub exit_code: Option<i32>,
    #[serde(default)]
    pub signal: Option<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct TerminalOutputResult {
    pub output: String,
    pub truncated: bool,
    #[serde(
        rename = "exitStatus",
        default,
        skip_serializing_if = "Option::is_none"
    )]
    pub exit_status: Option<TerminalExitStatus>,
}

/// Handles requests an ACP agent sends back to the client
/// (`session/request_permission`, `fs/read_text_file`, `fs/write_text_file`, `terminal/*`).
///
/// Every method defaults to a `-32601 Method not found` error, so implementers
/// only override what they support. There is no built-in filesystem or
/// permission policy — the caller decides what `read_text_file` /
/// `write_text_file` / `request_permission` are allowed to do.
pub trait AgentRequestHandler {
    fn request_permission(
        &self,
        _params: RequestPermissionParams,
    ) -> std::result::Result<RequestPermissionResult, JsonRpcError> {
        Err(method_not_found())
    }

    fn read_text_file(
        &self,
        _params: ReadTextFileParams,
    ) -> std::result::Result<ReadTextFileResult, JsonRpcError> {
        Err(method_not_found())
    }

    fn write_text_file(
        &self,
        _params: WriteTextFileParams,
    ) -> std::result::Result<(), JsonRpcError> {
        Err(method_not_found())
    }

    fn terminal_create(
        &self,
        _params: TerminalCreateParams,
    ) -> std::result::Result<TerminalCreateResult, JsonRpcError> {
        Err(method_not_found())
    }

    fn terminal_output(
        &self,
        _params: TerminalIdParams,
    ) -> std::result::Result<TerminalOutputResult, JsonRpcError> {
        Err(method_not_found())
    }

    fn terminal_release(&self, _params: TerminalIdParams) -> std::result::Result<(), JsonRpcError> {
        Err(method_not_found())
    }

    fn terminal_wait_for_exit(
        &self,
        _params: TerminalIdParams,
    ) -> std::result::Result<TerminalExitStatus, JsonRpcError> {
        Err(method_not_found())
    }

    fn terminal_kill(&self, _params: TerminalIdParams) -> std::result::Result<(), JsonRpcError> {
        Err(method_not_found())
    }
}

/// Default handler: every agent→client request gets `-32601`.
pub struct NoopAgentRequestHandler;

impl AgentRequestHandler for NoopAgentRequestHandler {}

/// Largest payload `fs/write_text_file` will put on disk in one call — mirrors the
/// runtime's `MAX_WRITE_BYTES` in `tools/fs.rs`.
const MAX_WRITE_BYTES: usize = 4 * 1024 * 1024;

/// Largest file `fs/read_text_file` will return. Reads are cheaper than writes but an
/// agent asking for a multi-gigabyte file is still a memory event, so the read is capped.
const MAX_READ_BYTES: u64 = 16 * 1024 * 1024;

/// JSON-RPC implementation-defined error for a request the handler understood but refused
/// on policy grounds (path outside the workspace, oversize payload).
const ERR_REQUEST_DENIED: i64 = -32001;

/// Live ACP terminals are capped: each one is a real child process with reader threads, and
/// the handler may live as long as the ACP session.
const MAX_TERMINALS: usize = 8;

/// Output retained per terminal. Older bytes are dropped so a chatty process cannot exhaust memory.
const MAX_TERMINAL_OUTPUT: usize = 1024 * 1024;

/// Per-call `terminal/output` byte limit when the agent does not send `outputByteLimit`.
const DEFAULT_OUTPUT_LIMIT: usize = 256 * 1024;

/// PTY window size for `terminal/create`; the ACP terminal API has no cols/rows fields.
const TERMINAL_PTY_COLS: u16 = 80;
const TERMINAL_PTY_ROWS: u16 = 24;

const TERMINAL_POLL_INTERVAL: Duration = Duration::from_millis(10);

fn request_denied(message: &str) -> JsonRpcError {
    JsonRpcError {
        code: ERR_REQUEST_DENIED,
        message: format!("Request denied: {message}"),
        data: None,
    }
}

/// Decide an agent's `session/request_permission` request. The ACP crate cannot reach the
/// runtime's policy engine, so the host installs a decider; the default refuses everything.
pub type PermissionDecider =
    Box<dyn Fn(&RequestPermissionParams) -> RequestPermissionResult + Send + Sync>;

/// A live `terminal/*` session: a command running under a real PTY. `_master` and `_writer`
/// are never read directly — they must simply stay alive: dropping the master hangs up the
/// pty and dropping the writer sends EOF to the slave. There is no ACP method that writes to
/// a terminal's stdin, so `_writer` is kept only to avoid that EOF.
struct TerminalState {
    _master: Box<dyn MasterPty + Send>,
    _writer: Box<dyn Write + Send>,
    child: Box<dyn PtyChild + Send + Sync>,
    output: Arc<Mutex<OutputBuffer>>,
    exit: Option<TerminalExitStatus>,
    /// Absolute offset of the next byte `terminal/output` has not yet returned.
    read_offset: u64,
    byte_limit: usize,
}

/// Ring of retained terminal output. `base` is the absolute offset of `data[0]`; bytes
/// drained off the front advance `base` so a reader can tell it missed output.
#[derive(Default)]
struct OutputBuffer {
    data: Vec<u8>,
    base: u64,
}

fn terminal_shell_command(command: &str) -> CommandBuilder {
    #[cfg(windows)]
    {
        let comspec = std::env::var("COMSPEC")
            .ok()
            .map(|s| s.trim().to_owned())
            .filter(|s| !s.is_empty())
            .unwrap_or_else(|| "cmd".to_owned());
        let mut cmd = CommandBuilder::new(comspec);
        cmd.arg("/C");
        cmd.arg(command);
        cmd
    }
    #[cfg(not(windows))]
    {
        let shell = std::env::var("SHELL")
            .ok()
            .map(|s| s.trim().to_owned())
            .filter(|s| !s.is_empty())
            .unwrap_or_else(|| "sh".to_owned());
        let mut cmd = CommandBuilder::new(shell);
        cmd.arg("-c");
        cmd.arg(command);
        cmd
    }
}

/// Drain a pty output stream into the shared buffer, dropping the oldest bytes past the cap.
fn pump_terminal_output<R: Read + Send + 'static>(mut reader: R, output: Arc<Mutex<OutputBuffer>>) {
    std::thread::spawn(move || {
        let mut chunk = [0u8; 8192];
        loop {
            match reader.read(&mut chunk) {
                Ok(0) | Err(_) => break,
                Ok(n) => {
                    if let Ok(mut out) = output.lock() {
                        out.data.extend_from_slice(&chunk[..n]);
                        if out.data.len() > MAX_TERMINAL_OUTPUT {
                            let excess = out.data.len() - MAX_TERMINAL_OUTPUT;
                            out.data.drain(..excess);
                            out.base += excess as u64;
                        }
                    }
                }
            }
        }
    });
}

/// portable-pty reports the signal name from `strsignal` (e.g. "Hangup", "Killed") rather
/// than the `SIG*` constant name; pass it through verbatim rather than guess a mapping.
fn pty_exit_status(status: portable_pty::ExitStatus) -> TerminalExitStatus {
    TerminalExitStatus {
        exit_code: if status.signal().is_some() {
            None
        } else {
            Some(status.exit_code() as i32)
        },
        signal: status.signal().map(str::to_string),
    }
}

/// A real [`AgentRequestHandler`] for hosts that want filesystem, permission, and terminal
/// requests answered instead of `-32601`.
///
/// `fs/read_text_file` and `fs/write_text_file` are confined to `workspace_root` with the
/// same symlink-aware resolution the runtime uses (`resolve_within` in
/// `vikram-runtime/src/config.rs`, duplicated here because this crate cannot depend on it).
/// `session/request_permission` is deny-first: without an installed
/// [`Self::with_permission_decider`] every request resolves to `cancelled`, which the agent
/// must treat as a refusal — never a fake approval. `terminal/*` is likewise deny-first:
/// without [`Self::with_terminal_support`] every terminal request gets `-32601`.
pub struct WorkspaceAgentRequestHandler {
    workspace_root: PathBuf,
    permission_decider: PermissionDecider,
    terminal_support: bool,
    terminals: Mutex<HashMap<String, Arc<Mutex<TerminalState>>>>,
    terminal_seq: AtomicU64,
}

impl WorkspaceAgentRequestHandler {
    /// `workspace_root` must exist and be resolvable; the canonical root is stored so every
    /// later confinement check compares against the real path, not a symlink spelling.
    pub fn new(workspace_root: impl AsRef<Path>) -> Result<Self> {
        let root = resolve_symlinks(workspace_root.as_ref())
            .ok_or_else(|| AcpError::Protocol("workspace root cannot be resolved".into()))?;
        Ok(Self {
            workspace_root: root,
            permission_decider: Box::new(|_| RequestPermissionResult::cancelled()),
            terminal_support: false,
            terminals: Mutex::new(HashMap::new()),
            terminal_seq: AtomicU64::new(0),
        })
    }

    /// Install the function that turns an agent permission request into a decision. The host
    /// wires its own policy (or an interactive prompt) here; whatever it returns is sent back
    /// verbatim, so a decider that cannot decide should return [`RequestPermissionResult::cancelled`].
    pub fn with_permission_decider(
        mut self,
        decider: impl Fn(&RequestPermissionParams) -> RequestPermissionResult + Send + Sync + 'static,
    ) -> Self {
        self.permission_decider = Box::new(decider);
        self
    }

    /// Enable the `terminal/*` agent→client methods. Off by default — enabling is the host's
    /// policy decision, since `terminal/create` runs an arbitrary command line. Terminals are
    /// real PTYs (default size 80x24; the ACP API carries no cols/rows), and `cwd` is confined
    /// to the workspace root.
    pub fn with_terminal_support(mut self) -> Self {
        self.terminal_support = true;
        self
    }

    fn confined_path(&self, raw: &str) -> std::result::Result<PathBuf, JsonRpcError> {
        resolve_within(&self.workspace_root, Path::new(raw))
            .ok_or_else(|| request_denied(&format!("`{raw}` is outside the workspace root")))
    }

    fn terminal_state(
        &self,
        terminal_id: &str,
    ) -> std::result::Result<Arc<Mutex<TerminalState>>, JsonRpcError> {
        self.terminals
            .lock()
            .map_err(|_| internal_error("terminal registry poisoned"))?
            .get(terminal_id)
            .cloned()
            .ok_or_else(|| invalid_params(&format!("unknown terminal `{terminal_id}`")))
    }

    fn reap_if_exited(state: &mut TerminalState) -> std::result::Result<(), JsonRpcError> {
        if state.exit.is_some() {
            return Ok(());
        }
        match state.child.try_wait() {
            Ok(Some(status)) => {
                state.exit = Some(pty_exit_status(status));
                Ok(())
            }
            Ok(None) => Ok(()),
            Err(e) => Err(internal_error(&format!("cannot poll terminal: {e}"))),
        }
    }
}

impl AgentRequestHandler for WorkspaceAgentRequestHandler {
    fn request_permission(
        &self,
        params: RequestPermissionParams,
    ) -> std::result::Result<RequestPermissionResult, JsonRpcError> {
        Ok((self.permission_decider)(&params))
    }

    fn read_text_file(
        &self,
        params: ReadTextFileParams,
    ) -> std::result::Result<ReadTextFileResult, JsonRpcError> {
        let path = self.confined_path(&params.path)?;
        let metadata = std::fs::metadata(&path)
            .map_err(|e| internal_error(&format!("cannot stat `{}`: {e}", path.display())))?;
        if !metadata.is_file() {
            return Err(internal_error(&format!(
                "`{}` is not a file",
                path.display()
            )));
        }
        if metadata.len() > MAX_READ_BYTES {
            return Err(request_denied(&format!(
                "`{}` is {} bytes, over the {MAX_READ_BYTES} byte read limit",
                params.path,
                metadata.len()
            )));
        }
        let content = std::fs::read_to_string(&path)
            .map_err(|e| internal_error(&format!("cannot read `{}`: {e}", path.display())))?;
        Ok(ReadTextFileResult {
            content: apply_line_window(&content, params.line, params.limit),
        })
    }

    fn write_text_file(
        &self,
        params: WriteTextFileParams,
    ) -> std::result::Result<(), JsonRpcError> {
        if params.content.len() > MAX_WRITE_BYTES {
            return Err(request_denied(&format!(
                "content is {} bytes, over the {MAX_WRITE_BYTES} byte write limit",
                params.content.len()
            )));
        }
        let path = self.confined_path(&params.path)?;
        if let Some(parent) = path.parent()
            && let Err(e) = std::fs::create_dir_all(parent)
        {
            return Err(internal_error(&format!(
                "cannot create `{}`: {e}",
                parent.display()
            )));
        }
        std::fs::write(&path, &params.content)
            .map_err(|e| internal_error(&format!("cannot write `{}`: {e}", path.display())))
    }

    fn terminal_create(
        &self,
        params: TerminalCreateParams,
    ) -> std::result::Result<TerminalCreateResult, JsonRpcError> {
        if !self.terminal_support {
            return Err(method_not_found());
        }
        if params.command.trim().is_empty() {
            return Err(invalid_params("`command` must not be empty"));
        }
        let cwd = match params.cwd.as_deref() {
            Some(raw) => self.confined_path(raw)?,
            None => self.workspace_root.clone(),
        };
        let byte_limit = params
            .output_byte_limit
            .unwrap_or(DEFAULT_OUTPUT_LIMIT)
            .clamp(1, MAX_TERMINAL_OUTPUT);

        {
            let terminals = self
                .terminals
                .lock()
                .map_err(|_| internal_error("terminal registry poisoned"))?;
            if terminals.len() >= MAX_TERMINALS {
                return Err(request_denied(&format!(
                    "terminal limit reached ({MAX_TERMINALS}); release one first"
                )));
            }
        }

        let mut cmd = terminal_shell_command(params.command.trim());
        cmd.env("TERM", "xterm-256color");
        cmd.cwd(cwd.as_path());
        for var in params.env.unwrap_or_default() {
            cmd.env(var.name, var.value);
        }

        let pair = native_pty_system()
            .openpty(PtySize {
                rows: TERMINAL_PTY_ROWS,
                cols: TERMINAL_PTY_COLS,
                pixel_width: 0,
                pixel_height: 0,
            })
            .map_err(|e| internal_error(&format!("cannot open terminal pty: {e}")))?;
        let child = pair
            .slave
            .spawn_command(cmd)
            .map_err(|e| internal_error(&format!("cannot spawn terminal: {e}")))?;
        drop(pair.slave);
        let reader = pair
            .master
            .try_clone_reader()
            .map_err(|e| internal_error(&format!("cannot read terminal pty: {e}")))?;
        let writer = pair
            .master
            .take_writer()
            .map_err(|e| internal_error(&format!("cannot write terminal pty: {e}")))?;

        let output = Arc::new(Mutex::new(OutputBuffer::default()));
        pump_terminal_output(reader, Arc::clone(&output));

        let id = format!(
            "term-{}-{}",
            std::process::id(),
            self.terminal_seq.fetch_add(1, Ordering::Relaxed)
        );
        let state = Arc::new(Mutex::new(TerminalState {
            _master: pair.master,
            _writer: writer,
            child,
            output,
            exit: None,
            read_offset: 0,
            byte_limit,
        }));
        self.terminals
            .lock()
            .map_err(|_| internal_error("terminal registry poisoned"))?
            .insert(id.clone(), state);
        Ok(TerminalCreateResult { terminal_id: id })
    }

    fn terminal_output(
        &self,
        params: TerminalIdParams,
    ) -> std::result::Result<TerminalOutputResult, JsonRpcError> {
        if !self.terminal_support {
            return Err(method_not_found());
        }
        let state = self.terminal_state(&params.terminal_id)?;
        let mut st = state
            .lock()
            .map_err(|_| internal_error("terminal state poisoned"))?;
        Self::reap_if_exited(&mut st)?;
        let (output, chunk_end, truncated) = {
            let buf = st
                .output
                .lock()
                .map_err(|_| internal_error("terminal output poisoned"))?;
            let end = buf.base + buf.data.len() as u64;
            let start = st.read_offset.max(buf.base);
            let mut truncated = st.read_offset < buf.base;
            let mut chunk_end = end;
            if chunk_end.saturating_sub(start) > st.byte_limit as u64 {
                chunk_end = start + st.byte_limit as u64;
                truncated = true;
            }
            let output = String::from_utf8_lossy(
                &buf.data[(start - buf.base) as usize..(chunk_end - buf.base) as usize],
            )
            .into_owned();
            (output, chunk_end, truncated)
        };
        st.read_offset = chunk_end;
        Ok(TerminalOutputResult {
            output,
            truncated,
            exit_status: st.exit.clone(),
        })
    }

    fn terminal_release(&self, params: TerminalIdParams) -> std::result::Result<(), JsonRpcError> {
        if !self.terminal_support {
            return Err(method_not_found());
        }
        let removed = self
            .terminals
            .lock()
            .map_err(|_| internal_error("terminal registry poisoned"))?
            .remove(&params.terminal_id);
        let Some(state) = removed else {
            return Err(invalid_params(&format!(
                "unknown terminal `{}`",
                params.terminal_id
            )));
        };
        if let Ok(mut st) = state.lock()
            && st.exit.is_none()
        {
            let _ = st.child.kill();
            let _ = st.child.wait();
        }
        Ok(())
    }

    fn terminal_wait_for_exit(
        &self,
        params: TerminalIdParams,
    ) -> std::result::Result<TerminalExitStatus, JsonRpcError> {
        if !self.terminal_support {
            return Err(method_not_found());
        }
        let state = self.terminal_state(&params.terminal_id)?;
        loop {
            let result = {
                let mut st = state
                    .lock()
                    .map_err(|_| internal_error("terminal state poisoned"))?;
                Self::reap_if_exited(&mut st)?;
                st.exit.clone()
            };
            if let Some(exit) = result {
                return Ok(exit);
            }
            std::thread::sleep(TERMINAL_POLL_INTERVAL);
        }
    }

    fn terminal_kill(&self, params: TerminalIdParams) -> std::result::Result<(), JsonRpcError> {
        if !self.terminal_support {
            return Err(method_not_found());
        }
        let state = self.terminal_state(&params.terminal_id)?;
        let mut st = state
            .lock()
            .map_err(|_| internal_error("terminal state poisoned"))?;
        if st.exit.is_none() {
            // kill() on an already-dead child errors harmlessly; wait() then reaps and
            // records the real exit status either way.
            let _ = st.child.kill();
            match st.child.wait() {
                Ok(status) => st.exit = Some(pty_exit_status(status)),
                Err(e) => return Err(internal_error(&format!("cannot reap terminal: {e}"))),
            }
        }
        Ok(())
    }
}

impl Drop for WorkspaceAgentRequestHandler {
    fn drop(&mut self) {
        // Dropping a std Child does not kill the process; reap every live terminal so a
        // dropped handler cannot leak shells.
        if let Ok(mut terminals) = self.terminals.lock() {
            for (_, state) in terminals.drain() {
                if let Ok(mut st) = state.lock()
                    && st.exit.is_none()
                {
                    let _ = st.child.kill();
                    let _ = st.child.wait();
                }
            }
        }
    }
}

/// `line` is 1-indexed per the ACP spec; `limit` caps the number of lines returned. Both
/// absent means the whole file.
fn apply_line_window(content: &str, line: Option<u64>, limit: Option<u64>) -> String {
    if line.is_none() && limit.is_none() {
        return content.to_string();
    }
    let start = line.unwrap_or(1).saturating_sub(1) as usize;
    let mut lines: Vec<&str> = content.lines().skip(start).collect();
    if let Some(limit) = limit {
        lines.truncate(limit as usize);
    }
    let mut out = lines.join("\n");
    if content.ends_with('\n') && !out.is_empty() {
        out.push('\n');
    }
    out
}

/// Resolve a path against a root and refuse escapes — same semantics as the runtime's
/// `resolve_within`, duplicated here because this crate cannot depend on it.
fn resolve_within(root: &Path, candidate: &Path) -> Option<PathBuf> {
    let canonical_root = resolve_symlinks(root)?;
    let resolved = if candidate.is_absolute() {
        resolve_symlinks(candidate)?
    } else {
        resolve_symlinks(&canonical_root.join(candidate))?
    };
    (resolved == canonical_root || resolved.starts_with(&canonical_root)).then_some(resolved)
}

/// Resolve `path` one component at a time, canonicalizing every component that exists so symlink
/// targets — including the parent a `..` climbs to — are the real ones. A component that exists
/// but cannot be canonicalized refuses the whole path; a not-yet-existing tail is kept verbatim
/// because it cannot contain symlinks.
fn resolve_symlinks(path: &Path) -> Option<PathBuf> {
    let absolute = if path.is_absolute() {
        path.to_path_buf()
    } else {
        std::env::current_dir().ok()?.join(path)
    };
    let mut current = PathBuf::new();
    for component in absolute.components() {
        use std::path::Component;
        match component {
            Component::Prefix(prefix) => current.push(prefix.as_os_str()),
            Component::RootDir => current.push(component.as_os_str()),
            Component::CurDir => {}
            Component::ParentDir => {
                current.pop();
            }
            Component::Normal(name) => {
                current.push(name);
                match current.canonicalize() {
                    Ok(canonical) => current = canonical,
                    Err(_) => {
                        if current.symlink_metadata().is_ok() {
                            return None;
                        }
                    }
                }
            }
        }
    }
    Some(current)
}

fn method_not_found() -> JsonRpcError {
    JsonRpcError {
        code: -32601,
        message: "Method not found".into(),
        data: None,
    }
}

fn invalid_params(message: &str) -> JsonRpcError {
    JsonRpcError {
        code: -32602,
        message: format!("Invalid params: {message}"),
        data: None,
    }
}

fn internal_error(message: &str) -> JsonRpcError {
    JsonRpcError {
        code: -32603,
        message: format!("Internal error: {message}"),
        data: None,
    }
}

fn decode_params<T: serde::de::DeserializeOwned>(
    req: &JsonRpcRequest,
) -> std::result::Result<T, JsonRpcError> {
    let value = req
        .params
        .clone()
        .ok_or_else(|| invalid_params("missing params"))?;
    serde_json::from_value(value).map_err(|e| invalid_params(&e.to_string()))
}

fn respond<T: Serialize>(
    res: std::result::Result<T, JsonRpcError>,
) -> (Option<Value>, Option<JsonRpcError>) {
    match res {
        Ok(v) => match serde_json::to_value(v) {
            Ok(value) => (Some(value), None),
            Err(e) => (None, Some(internal_error(&e.to_string()))),
        },
        Err(e) => (None, Some(e)),
    }
}

fn agent_request_response(
    handler: &dyn AgentRequestHandler,
    req: &JsonRpcRequest,
) -> JsonRpcResponse {
    let (result, error) = match req.method.as_str() {
        "session/request_permission" => respond(
            decode_params::<RequestPermissionParams>(req)
                .and_then(|p| handler.request_permission(p)),
        ),
        "fs/read_text_file" => respond(
            decode_params::<ReadTextFileParams>(req).and_then(|p| handler.read_text_file(p)),
        ),
        "fs/write_text_file" => respond(
            decode_params::<WriteTextFileParams>(req).and_then(|p| handler.write_text_file(p)),
        ),
        "terminal/create" => respond(
            decode_params::<TerminalCreateParams>(req).and_then(|p| handler.terminal_create(p)),
        ),
        "terminal/output" => {
            respond(decode_params::<TerminalIdParams>(req).and_then(|p| handler.terminal_output(p)))
        }
        "terminal/release" => respond(
            decode_params::<TerminalIdParams>(req).and_then(|p| handler.terminal_release(p)),
        ),
        "terminal/wait_for_exit" => respond(
            decode_params::<TerminalIdParams>(req).and_then(|p| handler.terminal_wait_for_exit(p)),
        ),
        "terminal/kill" => {
            respond(decode_params::<TerminalIdParams>(req).and_then(|p| handler.terminal_kill(p)))
        }
        _ => (None, Some(method_not_found())),
    };
    JsonRpcResponse {
        jsonrpc: "2.0".into(),
        id: req.id.clone(),
        result,
        error,
    }
}

// ---------- ACP Client ----------

pub struct AcpClient {
    child: Option<Child>,
    stdin: Option<std::process::ChildStdin>,
    rx: Option<Receiver<String>>,
    request_timeout: Duration,
    next_id: i64,
    agent_handler: Box<dyn AgentRequestHandler>,
}

impl AcpClient {
    pub fn new() -> Self {
        Self {
            child: None,
            stdin: None,
            rx: None,
            request_timeout: Duration::from_secs(300),
            next_id: 1,
            agent_handler: Box::new(NoopAgentRequestHandler),
        }
    }

    /// Spawn an agent process and connect over stdio.
    pub fn spawn(command: &str, args: &[&str]) -> Result<Self> {
        let mut child = Command::new(command)
            .args(args)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit())
            .spawn()?;
        let stdin = child.stdin.take();
        let (tx, rx) = std::sync::mpsc::channel();
        if let Some(stdout) = child.stdout.take() {
            std::thread::spawn(move || {
                let mut reader = BufReader::new(stdout);
                let mut line = String::new();
                loop {
                    line.clear();
                    match reader.read_line(&mut line) {
                        Ok(0) | Err(_) => break,
                        Ok(_) => {
                            if tx.send(line.trim_end().to_string()).is_err() {
                                break;
                            }
                        }
                    }
                }
            });
        }
        Ok(Self {
            child: Some(child),
            stdin,
            rx: Some(rx),
            request_timeout: Duration::from_secs(300),
            next_id: 1,
            agent_handler: Box::new(NoopAgentRequestHandler),
        })
    }

    pub fn set_request_timeout(&mut self, timeout: Duration) {
        self.request_timeout = timeout;
    }

    /// Install a handler for agent→client requests. Defaults to
    /// `NoopAgentRequestHandler` (`-32601` for every request).
    pub fn set_agent_request_handler<H: AgentRequestHandler + 'static>(&mut self, handler: H) {
        self.agent_handler = Box::new(handler);
    }

    fn alloc_id(&mut self) -> i64 {
        let id = self.next_id;
        self.next_id += 1;
        id
    }

    /// Handle one inbound line from the agent. Returns `Some` when it is the
    /// response to `our_id`; agent→client requests are answered by `handler`.
    fn handle_inbound<W: Write>(
        out: &mut W,
        line: &str,
        our_id: i64,
        handler: &dyn AgentRequestHandler,
    ) -> Result<Option<JsonRpcResponse>> {
        let value: Value = match serde_json::from_str(line) {
            Ok(v) => v,
            Err(_) => return Ok(None),
        };
        if value.get("method").is_some() {
            // Agent→client request or notification, not a response to us.
            if let Ok(req) = serde_json::from_value::<JsonRpcRequest>(value.clone()) {
                let resp = agent_request_response(handler, &req);
                writeln!(out, "{}", serde_json::to_string(&resp)?)?;
                out.flush()?;
            }
            return Ok(None);
        }
        if let Ok(resp) = serde_json::from_value::<JsonRpcResponse>(value)
            && resp.id == Value::Number(our_id.into())
        {
            return Ok(Some(resp));
        }
        Ok(None)
    }

    pub fn send_request(&mut self, method: &str, params: Option<Value>) -> Result<JsonRpcResponse> {
        let id = self.alloc_id();
        let req = JsonRpcRequest::new(id, method, params);
        let line = serde_json::to_string(&req)?;
        let stdin = self.stdin.as_mut().ok_or(AcpError::NotConnected)?;
        writeln!(stdin, "{line}")?;
        stdin.flush()?;

        let rx = self.rx.as_mut().ok_or(AcpError::NotConnected)?;
        let deadline = Instant::now() + self.request_timeout;
        loop {
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                return Err(AcpError::Protocol("request timeout".into()));
            }
            let line = match rx.recv_timeout(remaining) {
                Ok(line) => line,
                Err(RecvTimeoutError::Timeout) => {
                    return Err(AcpError::Protocol("request timeout".into()));
                }
                Err(RecvTimeoutError::Disconnected) => {
                    return Err(AcpError::Protocol("eof".into()));
                }
            };
            if line.trim().is_empty() {
                continue;
            }
            let stdin = self.stdin.as_mut().ok_or(AcpError::NotConnected)?;
            let handler = &*self.agent_handler;
            if let Some(resp) = Self::handle_inbound(stdin, &line, id, handler)? {
                return Ok(resp);
            }
        }
    }

    pub fn initialize(&mut self, client_caps: Value) -> Result<InitializeResult> {
        let params = serde_json::to_value(InitializeParams {
            protocol_version: 1,
            client_capabilities: client_caps,
        })?;
        let resp = self.send_request("initialize", Some(params))?;
        if let Some(err) = resp.error {
            return Err(AcpError::Protocol(err.message));
        }
        let result = resp
            .result
            .ok_or_else(|| AcpError::Protocol("no result".into()))?;
        serde_json::from_value(result).map_err(AcpError::Json)
    }

    pub fn new_session(&mut self, cwd: &str) -> Result<String> {
        let params = serde_json::to_value(NewSessionParams {
            cwd: cwd.to_string(),
            mcp_servers: vec![],
        })?;
        let resp = self.send_request("session/new", Some(params))?;
        if let Some(err) = resp.error {
            return Err(AcpError::Protocol(err.message));
        }
        let result = resp
            .result
            .ok_or_else(|| AcpError::Protocol("no result".into()))?;
        let r: NewSessionResult = serde_json::from_value(result)?;
        Ok(r.session_id)
    }

    pub fn prompt(&mut self, session_id: &str, text: &str) -> Result<Value> {
        let params = serde_json::to_value(PromptParams {
            session_id: session_id.to_string(),
            prompt: vec![ContentBlock::text(text)],
        })?;
        let resp = self.send_request("session/prompt", Some(params))?;
        if let Some(err) = resp.error {
            return Err(AcpError::Protocol(err.message));
        }
        Ok(resp.result.unwrap_or(Value::Null))
    }

    pub fn cancel(&mut self, session_id: &str) -> Result<()> {
        let params = serde_json::to_value(CancelParams {
            session_id: session_id.to_string(),
        })?;
        let _ = self.send_request("session/cancel", Some(params))?;
        Ok(())
    }

    pub fn shutdown(&mut self) {
        if let Some(mut child) = self.child.take() {
            let _ = child.kill();
            let _ = child.wait();
        }
        self.stdin = None;
        self.rx = None;
    }
}

impl Default for AcpClient {
    fn default() -> Self {
        Self::new()
    }
}

impl Drop for AcpClient {
    fn drop(&mut self) {
        self.shutdown();
    }
}

// ---------- ACP Server (minimal stdio) ----------

pub struct AcpServer {
    agent_name: String,
}

impl AcpServer {
    pub fn new(agent_name: impl Into<String>) -> Self {
        Self {
            agent_name: agent_name.into(),
        }
    }

    /// Run the server on stdin/stdout, dispatching requests to the provided handler.
    /// The handler receives (method, params) and returns (result, error).
    pub fn run<F>(&self, mut handler: F) -> Result<()>
    where
        F: FnMut(&str, Option<Value>) -> (Option<Value>, Option<JsonRpcError>),
    {
        let stdin = std::io::stdin();
        let mut stdout = std::io::stdout();
        let reader = BufReader::new(stdin);
        for line in reader.lines() {
            let line = line?;
            let trimmed = line.trim();
            if trimmed.is_empty() {
                continue;
            }
            Self::dispatch_line(trimmed, &mut handler, &mut stdout)?;
        }
        Ok(())
    }

    fn write_error(out: &mut dyn Write, id: Value, code: i64, message: &str) -> Result<()> {
        let resp = JsonRpcResponse {
            jsonrpc: "2.0".into(),
            id,
            result: None,
            error: Some(JsonRpcError {
                code,
                message: message.into(),
                data: None,
            }),
        };
        writeln!(out, "{}", serde_json::to_string(&resp)?)?;
        out.flush()?;
        Ok(())
    }

    fn dispatch_line<F>(line: &str, handler: &mut F, out: &mut dyn Write) -> Result<()>
    where
        F: FnMut(&str, Option<Value>) -> (Option<Value>, Option<JsonRpcError>),
    {
        let value: Value = match serde_json::from_str(line) {
            Ok(v) => v,
            Err(_) => {
                return Self::write_error(out, Value::Null, -32700, "Parse error");
            }
        };
        let req = match serde_json::from_value::<JsonRpcRequest>(value.clone()) {
            Ok(r) => r,
            Err(_) => {
                if serde_json::from_value::<JsonRpcNotification>(value.clone()).is_ok() {
                    return Ok(());
                }
                let id = value.get("id").cloned().unwrap_or(Value::Null);
                return Self::write_error(out, id, -32600, "Invalid Request");
            }
        };
        let (result, error) = handler(&req.method, req.params.clone());
        let resp = JsonRpcResponse {
            jsonrpc: "2.0".into(),
            id: req.id.clone(),
            result,
            error,
        };
        writeln!(out, "{}", serde_json::to_string(&resp)?)?;
        out.flush()?;
        Ok(())
    }

    pub fn agent_name(&self) -> &str {
        &self.agent_name
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn jsonrpc_roundtrip() {
        let req = JsonRpcRequest::new(1, "initialize", None);
        let s = serde_json::to_string(&req).unwrap();
        let back: JsonRpcRequest = serde_json::from_str(&s).unwrap();
        assert_eq!(back.method, "initialize");
        assert_eq!(back.id, serde_json::json!(1));
        assert_eq!(back.jsonrpc, "2.0");
    }
    #[test]
    fn content_block_text() {
        let c = ContentBlock::text("hi");
        assert_eq!(c.kind, "text");
        assert_eq!(c.text.as_deref(), Some("hi"));
    }
    #[test]
    fn initialize_params_roundtrip() {
        let p = InitializeParams {
            protocol_version: 1,
            client_capabilities: serde_json::json!({"fs": true}),
        };
        let v = serde_json::to_value(&p).unwrap();
        assert_eq!(v["protocolVersion"], 1);
        let back: InitializeParams = serde_json::from_value(v).unwrap();
        assert_eq!(back.protocol_version, 1);
    }
    #[test]
    fn new_session_result_roundtrip() {
        let r = NewSessionResult {
            session_id: "sess-42".into(),
        };
        let v = serde_json::to_value(&r).unwrap();
        assert_eq!(v["sessionId"], "sess-42");
        let back: NewSessionResult = serde_json::from_value(v).unwrap();
        assert_eq!(back.session_id, "sess-42");
    }
    #[test]
    fn prompt_params_camel_case() {
        let p = PromptParams {
            session_id: "s1".into(),
            prompt: vec![ContentBlock::text("hello")],
        };
        let v = serde_json::to_value(&p).unwrap();
        assert_eq!(v["sessionId"], "s1");
        assert_eq!(v["prompt"][0]["type"], "text");
        assert_eq!(v["prompt"][0]["text"], "hello");
    }
    #[test]
    fn cancel_params_camel_case() {
        let p = CancelParams {
            session_id: "s9".into(),
        };
        let v = serde_json::to_value(&p).unwrap();
        assert_eq!(v["sessionId"], "s9");
    }
    #[test]
    fn jsonrpc_response_error_shape() {
        let e = JsonRpcError {
            code: -32600,
            message: "bad".into(),
            data: Some(serde_json::json!({"x": 1})),
        };
        let resp = JsonRpcResponse {
            jsonrpc: "2.0".into(),
            id: serde_json::json!(7),
            result: None,
            error: Some(e),
        };
        let s = serde_json::to_string(&resp).unwrap();
        let back: JsonRpcResponse = serde_json::from_str(&s).unwrap();
        assert!(back.result.is_none());
        assert_eq!(back.error.unwrap().code, -32600);
    }
    #[test]
    fn acp_client_starts_at_id_one() {
        let c = AcpClient::new();
        assert_eq!(c.next_id, 1);
    }
    #[test]
    fn acp_server_agent_name() {
        let s = AcpServer::new("vikram");
        assert_eq!(s.agent_name(), "vikram");
    }
    #[test]
    fn acp_client_spawn_missing_binary_errors() {
        let res = AcpClient::spawn("/definitely/not/a/binary", &[]);
        assert!(res.is_err());
    }

    fn handle_inbound(
        out: &mut Vec<u8>,
        line: &str,
        our_id: i64,
    ) -> Result<Option<JsonRpcResponse>> {
        AcpClient::handle_inbound(out, line, our_id, &NoopAgentRequestHandler)
    }

    #[test]
    fn handle_inbound_matches_response_by_id() {
        let mut out = Vec::new();
        let resp = handle_inbound(
            &mut out,
            r#"{"jsonrpc":"2.0","id":7,"result":{"ok":true}}"#,
            7,
        )
        .unwrap();
        assert_eq!(resp.unwrap().id, serde_json::json!(7));
        assert!(out.is_empty());
    }

    #[test]
    fn handle_inbound_ignores_response_for_other_id() {
        let mut out = Vec::new();
        let resp = handle_inbound(&mut out, r#"{"jsonrpc":"2.0","id":9,"result":{}}"#, 7).unwrap();
        assert!(resp.is_none());
        assert!(out.is_empty());
    }

    #[test]
    fn handle_inbound_answers_unknown_agent_request() {
        let mut out = Vec::new();
        let resp = handle_inbound(
            &mut out,
            r#"{"jsonrpc":"2.0","id":"agent-1","method":"unknown/method","params":{}}"#,
            1,
        )
        .unwrap();
        assert!(resp.is_none());
        let written: Value = serde_json::from_slice(&out).unwrap();
        assert_eq!(written["id"], serde_json::json!("agent-1"));
        assert_eq!(written["error"]["code"], serde_json::json!(-32601));
    }

    #[test]
    fn handle_inbound_known_method_default_handler_is_method_not_found() {
        let mut out = Vec::new();
        let resp = handle_inbound(
            &mut out,
            r#"{"jsonrpc":"2.0","id":3,"method":"session/request_permission","params":{"sessionId":"s1","toolCall":{},"options":[]}}"#,
            1,
        )
        .unwrap();
        assert!(resp.is_none());
        let written: Value = serde_json::from_slice(&out).unwrap();
        assert_eq!(written["id"], serde_json::json!(3));
        assert_eq!(written["error"]["code"], serde_json::json!(-32601));
    }

    #[test]
    fn handle_inbound_known_method_bad_params_is_invalid_params() {
        let mut out = Vec::new();
        let resp = handle_inbound(
            &mut out,
            r#"{"jsonrpc":"2.0","id":4,"method":"fs/read_text_file","params":{}}"#,
            1,
        )
        .unwrap();
        assert!(resp.is_none());
        let written: Value = serde_json::from_slice(&out).unwrap();
        assert_eq!(written["id"], serde_json::json!(4));
        assert_eq!(written["error"]["code"], serde_json::json!(-32602));
    }

    struct TestAgentHandler;

    impl AgentRequestHandler for TestAgentHandler {
        fn request_permission(
            &self,
            params: RequestPermissionParams,
        ) -> std::result::Result<RequestPermissionResult, JsonRpcError> {
            assert_eq!(params.session_id, "s1");
            assert_eq!(params.options.len(), 1);
            Ok(RequestPermissionResult {
                outcome: PermissionOutcome {
                    outcome: "selected".into(),
                    option_id: Some("allow".into()),
                },
            })
        }

        fn read_text_file(
            &self,
            params: ReadTextFileParams,
        ) -> std::result::Result<ReadTextFileResult, JsonRpcError> {
            Ok(ReadTextFileResult {
                content: format!("contents of {}", params.path),
            })
        }

        fn write_text_file(
            &self,
            params: WriteTextFileParams,
        ) -> std::result::Result<(), JsonRpcError> {
            assert_eq!(params.content, "data");
            Ok(())
        }
    }

    fn handle_inbound_with(handler: &dyn AgentRequestHandler, line: &str) -> JsonRpcResponse {
        let mut out = Vec::new();
        AcpClient::handle_inbound(&mut out, line, 1, handler).unwrap();
        serde_json::from_slice(out.trim_ascii()).unwrap()
    }

    #[test]
    fn handle_inbound_dispatches_request_permission() {
        let resp = handle_inbound_with(
            &TestAgentHandler,
            r#"{"jsonrpc":"2.0","id":"p1","method":"session/request_permission","params":{"sessionId":"s1","toolCall":{"title":"rm"},"options":[{"optionId":"allow","name":"Allow","kind":"allow_once"}]}}"#,
        );
        assert!(resp.error.is_none());
        let result = resp.result.unwrap();
        assert_eq!(result["outcome"]["outcome"], "selected");
        assert_eq!(result["outcome"]["optionId"], "allow");
    }

    #[test]
    fn handle_inbound_dispatches_read_text_file() {
        let resp = handle_inbound_with(
            &TestAgentHandler,
            r#"{"jsonrpc":"2.0","id":5,"method":"fs/read_text_file","params":{"sessionId":"s1","path":"/tmp/x.txt"}}"#,
        );
        assert!(resp.error.is_none());
        assert_eq!(resp.result.unwrap()["content"], "contents of /tmp/x.txt");
    }

    #[test]
    fn handle_inbound_dispatches_write_text_file() {
        let resp = handle_inbound_with(
            &TestAgentHandler,
            r#"{"jsonrpc":"2.0","id":6,"method":"fs/write_text_file","params":{"sessionId":"s1","path":"/tmp/x.txt","content":"data"}}"#,
        );
        assert!(resp.error.is_none());
        // `()` serializes to `"result":null`, which round-trips as `None`.
        assert!(resp.result.is_none());
    }

    #[test]
    fn handle_inbound_ignores_agent_notification() {
        let mut out = Vec::new();
        let resp = handle_inbound(
            &mut out,
            r#"{"jsonrpc":"2.0","method":"session/update","params":{}}"#,
            1,
        )
        .unwrap();
        assert!(resp.is_none());
        assert!(out.is_empty());
    }

    #[cfg(unix)]
    #[test]
    fn client_send_request_survives_interleaved_agent_request() {
        let dir = std::env::temp_dir();
        let capture = dir.join(format!("vikram-acp-capture-{}", std::process::id()));
        let script = format!(
            "read -r _req\n\
             printf '%s\\n' '{{\"jsonrpc\":\"2.0\",\"id\":\"agent-1\",\"method\":\"fs/read_text_file\",\"params\":{{\"sessionId\":\"s\",\"path\":\"/tmp/x\"}}}}'\n\
             read -r _resp\n\
             printf '%s\\n' \"$_resp\" > {}\n\
             printf '%s\\n' '{{\"jsonrpc\":\"2.0\",\"id\":1,\"result\":{{\"ok\":true}}}}'\n",
            capture.display()
        );
        let mut client = AcpClient::spawn("sh", &["-c", script.as_str()]).unwrap();
        let resp = client.send_request("initialize", None).unwrap();
        assert_eq!(resp.result.unwrap()["ok"], serde_json::json!(true));
        let written: Value = serde_json::from_slice(&std::fs::read(&capture).unwrap()).unwrap();
        assert_eq!(written["id"], serde_json::json!("agent-1"));
        assert_eq!(written["error"]["code"], serde_json::json!(-32601));
        let _ = std::fs::remove_file(&capture);
    }

    #[cfg(unix)]
    #[test]
    fn client_send_request_times_out() {
        let mut client = AcpClient::spawn("sh", &["-c", "sleep 30"]).unwrap();
        client.set_request_timeout(Duration::from_millis(200));
        let err = client.send_request("initialize", None).unwrap_err();
        assert!(matches!(err, AcpError::Protocol(ref m) if m == "request timeout"));
    }

    fn dispatch(lines: &[&str]) -> String {
        let mut out: Vec<u8> = Vec::new();
        let mut handler = |_: &str, _: Option<Value>| (Some(serde_json::json!({})), None);
        for line in lines {
            AcpServer::dispatch_line(line, &mut handler, &mut out).unwrap();
        }
        String::from_utf8(out).unwrap()
    }

    #[test]
    fn server_responds_parse_error() {
        let out = dispatch(&["not json"]);
        let resp: Value = serde_json::from_str(out.trim()).unwrap();
        assert_eq!(resp["error"]["code"], serde_json::json!(-32700));
        assert_eq!(resp["id"], Value::Null);
    }

    #[test]
    fn server_responds_invalid_request() {
        let out = dispatch(&[r#"{"jsonrpc":"2.0","id":3,"params":{}}"#]);
        let resp: Value = serde_json::from_str(out.trim()).unwrap();
        assert_eq!(resp["error"]["code"], serde_json::json!(-32600));
        assert_eq!(resp["id"], serde_json::json!(3));
    }

    /// A unique scratch directory under the system temp dir; removed on drop.
    struct ScratchDir(PathBuf);

    impl ScratchDir {
        fn new(name: &str) -> Self {
            let dir = std::env::temp_dir().join(format!(
                "vikram-acp-{name}-{}-{}",
                std::process::id(),
                std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .unwrap()
                    .as_nanos()
            ));
            std::fs::create_dir_all(&dir).unwrap();
            Self(dir)
        }

        fn path(&self) -> &Path {
            &self.0
        }
    }

    impl Drop for ScratchDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    fn workspace_handler() -> (ScratchDir, WorkspaceAgentRequestHandler) {
        let dir = ScratchDir::new("ws");
        let handler = WorkspaceAgentRequestHandler::new(dir.path()).unwrap();
        (dir, handler)
    }

    #[test]
    fn workspace_handler_reads_file_inside_root() {
        let (dir, handler) = workspace_handler();
        std::fs::write(dir.path().join("a.txt"), "hello\nworld\n").unwrap();
        let resp = handle_inbound_with(
            &handler,
            r#"{"jsonrpc":"2.0","id":1,"method":"fs/read_text_file","params":{"sessionId":"s","path":"a.txt"}}"#,
        );
        assert!(resp.error.is_none());
        assert_eq!(resp.result.unwrap()["content"], "hello\nworld\n");
    }

    #[test]
    fn workspace_handler_read_honors_line_and_limit() {
        let (dir, handler) = workspace_handler();
        std::fs::write(dir.path().join("a.txt"), "one\ntwo\nthree\n").unwrap();
        let resp = handle_inbound_with(
            &handler,
            r#"{"jsonrpc":"2.0","id":1,"method":"fs/read_text_file","params":{"sessionId":"s","path":"a.txt","line":2,"limit":1}}"#,
        );
        assert!(resp.error.is_none());
        assert_eq!(resp.result.unwrap()["content"], "two\n");
    }

    #[test]
    fn workspace_handler_writes_file_inside_root() {
        let (dir, handler) = workspace_handler();
        let resp = handle_inbound_with(
            &handler,
            r#"{"jsonrpc":"2.0","id":1,"method":"fs/write_text_file","params":{"sessionId":"s","path":"sub/b.txt","content":"data"}}"#,
        );
        assert!(resp.error.is_none());
        assert_eq!(
            std::fs::read_to_string(dir.path().join("sub/b.txt")).unwrap(),
            "data"
        );
    }

    #[test]
    fn workspace_handler_refuses_read_outside_root() {
        let (dir, handler) = workspace_handler();
        let outside = dir.path().parent().unwrap().join("vikram-acp-outside.txt");
        std::fs::write(&outside, "secret").unwrap();
        for path in [
            "../vikram-acp-outside.txt".to_string(),
            outside.to_string_lossy().into_owned(),
        ] {
            let resp = handle_inbound_with(
                &handler,
                &format!(
                    r#"{{"jsonrpc":"2.0","id":1,"method":"fs/read_text_file","params":{{"sessionId":"s","path":{}}}}}"#,
                    serde_json::to_string(&path).unwrap()
                ),
            );
            assert_eq!(resp.error.unwrap().code, ERR_REQUEST_DENIED, "{path}");
        }
        let _ = std::fs::remove_file(&outside);
    }

    #[test]
    fn workspace_handler_refuses_write_outside_root() {
        let (dir, handler) = workspace_handler();
        let outside = dir.path().parent().unwrap().join("vikram-acp-escape.txt");
        let _ = std::fs::remove_file(&outside);
        let resp = handle_inbound_with(
            &handler,
            r#"{"jsonrpc":"2.0","id":1,"method":"fs/write_text_file","params":{"sessionId":"s","path":"../vikram-acp-escape.txt","content":"x"}}"#,
        );
        assert_eq!(resp.error.unwrap().code, ERR_REQUEST_DENIED);
        assert!(!outside.exists());
    }

    #[cfg(unix)]
    #[test]
    fn workspace_handler_refuses_symlink_escape() {
        let (dir, handler) = workspace_handler();
        let outside = ScratchDir::new("outside");
        std::fs::write(outside.path().join("secret.txt"), "secret").unwrap();
        std::os::unix::fs::symlink(outside.path(), dir.path().join("link")).unwrap();
        let resp = handle_inbound_with(
            &handler,
            r#"{"jsonrpc":"2.0","id":1,"method":"fs/read_text_file","params":{"sessionId":"s","path":"link/secret.txt"}}"#,
        );
        assert_eq!(resp.error.unwrap().code, ERR_REQUEST_DENIED);
    }

    #[test]
    fn workspace_handler_write_over_size_cap_is_denied() {
        let (dir, handler) = workspace_handler();
        let huge = "x".repeat(MAX_WRITE_BYTES + 1);
        let resp = handle_inbound_with(
            &handler,
            &format!(
                r#"{{"jsonrpc":"2.0","id":1,"method":"fs/write_text_file","params":{{"sessionId":"s","path":"big.txt","content":{}}}}}"#,
                serde_json::to_string(&huge).unwrap()
            ),
        );
        assert_eq!(resp.error.unwrap().code, ERR_REQUEST_DENIED);
        assert!(!dir.path().join("big.txt").exists());
    }

    #[test]
    fn workspace_handler_permission_defaults_to_cancelled() {
        let (_dir, handler) = workspace_handler();
        let resp = handle_inbound_with(
            &handler,
            r#"{"jsonrpc":"2.0","id":1,"method":"session/request_permission","params":{"sessionId":"s","toolCall":{"title":"rm"},"options":[{"optionId":"allow","name":"Allow","kind":"allow_once"}]}}"#,
        );
        assert!(resp.error.is_none());
        let result = resp.result.unwrap();
        assert_eq!(result["outcome"]["outcome"], "cancelled");
        assert!(result["outcome"].get("optionId").is_none());
    }

    #[test]
    fn workspace_handler_permission_uses_installed_decider() {
        let (_dir, base) = workspace_handler();
        let handler = base.with_permission_decider(|params| {
            assert_eq!(params.options.len(), 1);
            RequestPermissionResult::selected("allow")
        });
        let resp = handle_inbound_with(
            &handler,
            r#"{"jsonrpc":"2.0","id":1,"method":"session/request_permission","params":{"sessionId":"s","toolCall":{},"options":[{"optionId":"allow","name":"Allow","kind":"allow_once"}]}}"#,
        );
        assert!(resp.error.is_none());
        let result = resp.result.unwrap();
        assert_eq!(result["outcome"]["outcome"], "selected");
        assert_eq!(result["outcome"]["optionId"], "allow");
    }

    #[test]
    fn workspace_handler_still_rejects_terminal_methods() {
        let (_dir, handler) = workspace_handler();
        for (id, method, params) in [
            (
                1,
                "terminal/create",
                r#"{"sessionId":"s","command":"echo hi"}"#,
            ),
            (
                2,
                "terminal/output",
                r#"{"sessionId":"s","terminalId":"t"}"#,
            ),
            (
                3,
                "terminal/release",
                r#"{"sessionId":"s","terminalId":"t"}"#,
            ),
            (
                4,
                "terminal/wait_for_exit",
                r#"{"sessionId":"s","terminalId":"t"}"#,
            ),
            (5, "terminal/kill", r#"{"sessionId":"s","terminalId":"t"}"#),
        ] {
            let resp = handle_inbound_with(
                &handler,
                &format!(r#"{{"jsonrpc":"2.0","id":{id},"method":"{method}","params":{params}}}"#),
            );
            assert_eq!(resp.error.unwrap().code, -32601, "{method}");
        }
    }

    fn terminal_handler() -> (ScratchDir, WorkspaceAgentRequestHandler) {
        let (dir, handler) = workspace_handler();
        (dir, handler.with_terminal_support())
    }

    fn terminal_request(
        handler: &WorkspaceAgentRequestHandler,
        id: i64,
        method: &str,
        params: &str,
    ) -> JsonRpcResponse {
        handle_inbound_with(
            handler,
            &format!(r#"{{"jsonrpc":"2.0","id":{id},"method":"{method}","params":{params}}}"#),
        )
    }

    #[test]
    fn terminal_create_output_wait_release_round_trip() {
        let (_dir, handler) = terminal_handler();
        let resp = terminal_request(
            &handler,
            1,
            "terminal/create",
            r#"{"sessionId":"s","command":"echo vikram-acp-term","cwd":"."}"#,
        );
        assert!(resp.error.is_none(), "{:?}", resp.error);
        let terminal_id = resp.result.unwrap()["terminalId"]
            .as_str()
            .unwrap()
            .to_string();

        let mut output = String::new();
        let mut exit_status = None;
        for _ in 0..100 {
            let resp = terminal_request(
                &handler,
                2,
                "terminal/output",
                &format!(r#"{{"sessionId":"s","terminalId":"{terminal_id}"}}"#),
            );
            assert!(resp.error.is_none());
            let result = resp.result.unwrap();
            output.push_str(result["output"].as_str().unwrap());
            if let Some(exit) = result.get("exitStatus") {
                exit_status = Some(exit.clone());
                break;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        assert!(output.contains("vikram-acp-term"), "output was {output:?}");
        assert_eq!(exit_status.unwrap()["exitCode"], serde_json::json!(0));

        let resp = terminal_request(
            &handler,
            3,
            "terminal/wait_for_exit",
            &format!(r#"{{"sessionId":"s","terminalId":"{terminal_id}"}}"#),
        );
        assert!(resp.error.is_none());
        assert_eq!(resp.result.unwrap()["exitCode"], serde_json::json!(0));

        let resp = terminal_request(
            &handler,
            4,
            "terminal/release",
            &format!(r#"{{"sessionId":"s","terminalId":"{terminal_id}"}}"#),
        );
        assert!(resp.error.is_none());
        let resp = terminal_request(
            &handler,
            5,
            "terminal/output",
            &format!(r#"{{"sessionId":"s","terminalId":"{terminal_id}"}}"#),
        );
        assert_eq!(resp.error.unwrap().code, -32602);
    }

    #[cfg(unix)]
    #[test]
    fn terminal_kill_terminates_running_command() {
        let (_dir, handler) = terminal_handler();
        let resp = terminal_request(
            &handler,
            1,
            "terminal/create",
            r#"{"sessionId":"s","command":"sleep 30"}"#,
        );
        assert!(resp.error.is_none());
        let terminal_id = resp.result.unwrap()["terminalId"]
            .as_str()
            .unwrap()
            .to_string();

        let resp = terminal_request(
            &handler,
            2,
            "terminal/kill",
            &format!(r#"{{"sessionId":"s","terminalId":"{terminal_id}"}}"#),
        );
        assert!(resp.error.is_none());

        let resp = terminal_request(
            &handler,
            3,
            "terminal/wait_for_exit",
            &format!(r#"{{"sessionId":"s","terminalId":"{terminal_id}"}}"#),
        );
        assert!(resp.error.is_none());
        let result = resp.result.unwrap();
        assert_eq!(result["exitCode"], serde_json::Value::Null);
        // portable-pty reports the strsignal name (e.g. "Hangup"/"Killed"), not "SIGKILL".
        assert!(result["signal"].is_string());
    }

    /// Poll `terminal/output` until `needle` appears or the deadline passes; returns the
    /// accumulated output.
    fn poll_terminal_output(
        handler: &WorkspaceAgentRequestHandler,
        terminal_id: &str,
        needle: &str,
    ) -> String {
        let mut output = String::new();
        for _ in 0..200 {
            let resp = terminal_request(
                handler,
                2,
                "terminal/output",
                &format!(r#"{{"sessionId":"s","terminalId":"{terminal_id}"}}"#),
            );
            assert!(resp.error.is_none());
            let result = resp.result.unwrap();
            output.push_str(result["output"].as_str().unwrap());
            if output.contains(needle) || result.get("exitStatus").is_some() {
                break;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        output
    }

    #[cfg(unix)]
    #[test]
    fn terminal_create_runs_command_on_a_real_pty() {
        let (_dir, handler) = terminal_handler();
        let resp = terminal_request(
            &handler,
            1,
            "terminal/create",
            r#"{"sessionId":"s","command":"test -t 0 && test -t 1 && echo pty-ok"}"#,
        );
        assert!(resp.error.is_none(), "{:?}", resp.error);
        let terminal_id = resp.result.unwrap()["terminalId"]
            .as_str()
            .unwrap()
            .to_string();
        let output = poll_terminal_output(&handler, &terminal_id, "pty-ok");
        assert!(output.contains("pty-ok"), "output was {output:?}");
    }

    #[cfg(unix)]
    #[test]
    fn terminal_create_applies_pty_window_size() {
        let (_dir, handler) = terminal_handler();
        let resp = terminal_request(
            &handler,
            1,
            "terminal/create",
            r#"{"sessionId":"s","command":"stty size"}"#,
        );
        assert!(resp.error.is_none(), "{:?}", resp.error);
        let terminal_id = resp.result.unwrap()["terminalId"]
            .as_str()
            .unwrap()
            .to_string();
        let output = poll_terminal_output(&handler, &terminal_id, "24 80");
        assert!(output.contains("24 80"), "output was {output:?}");
    }

    #[cfg(unix)]
    #[test]
    fn terminal_pty_accepts_stdin_written_to_the_master() {
        // ACP has no terminal-stdin method, so this exercises the pty writer directly:
        // `cat` proves bytes written to the master come back through the slave.
        let (_dir, handler) = terminal_handler();
        let resp = terminal_request(
            &handler,
            1,
            "terminal/create",
            r#"{"sessionId":"s","command":"cat"}"#,
        );
        assert!(resp.error.is_none(), "{:?}", resp.error);
        let terminal_id = resp.result.unwrap()["terminalId"]
            .as_str()
            .unwrap()
            .to_string();
        {
            let state = handler
                .terminals
                .lock()
                .unwrap()
                .get(&terminal_id)
                .cloned()
                .unwrap();
            let mut st = state.lock().unwrap();
            st._writer.write_all(b"vikram-cat-marker\n").unwrap();
            st._writer.flush().unwrap();
        }
        let output = poll_terminal_output(&handler, &terminal_id, "vikram-cat-marker");
        assert!(
            output.contains("vikram-cat-marker"),
            "output was {output:?}"
        );
        terminal_request(
            &handler,
            3,
            "terminal/kill",
            &format!(r#"{{"sessionId":"s","terminalId":"{terminal_id}"}}"#),
        );
    }

    #[test]
    fn terminal_create_rejects_empty_command_and_bad_params() {
        let (_dir, handler) = terminal_handler();
        let resp = terminal_request(
            &handler,
            1,
            "terminal/create",
            r#"{"sessionId":"s","command":"   "}"#,
        );
        assert_eq!(resp.error.unwrap().code, -32602);
        let resp = terminal_request(&handler, 2, "terminal/create", r#"{"sessionId":"s"}"#);
        assert_eq!(resp.error.unwrap().code, -32602);
    }

    #[test]
    fn terminal_create_refuses_cwd_outside_workspace() {
        let (_dir, handler) = terminal_handler();
        let resp = terminal_request(
            &handler,
            1,
            "terminal/create",
            r#"{"sessionId":"s","command":"pwd","cwd":".."}"#,
        );
        assert_eq!(resp.error.unwrap().code, ERR_REQUEST_DENIED);
    }

    #[test]
    fn terminal_methods_reject_unknown_terminal_id() {
        let (_dir, handler) = terminal_handler();
        for (id, method) in [
            (1, "terminal/output"),
            (2, "terminal/release"),
            (3, "terminal/wait_for_exit"),
            (4, "terminal/kill"),
        ] {
            let resp = terminal_request(
                &handler,
                id,
                method,
                r#"{"sessionId":"s","terminalId":"nope"}"#,
            );
            assert_eq!(resp.error.unwrap().code, -32602, "{method}");
        }
    }

    #[test]
    fn server_dispatches_request_and_ignores_notification() {
        let out = dispatch(&[
            r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{}}"#,
            r#"{"jsonrpc":"2.0","method":"initialized","params":{}}"#,
        ]);
        let lines: Vec<&str> = out.lines().collect();
        assert_eq!(lines.len(), 1);
        let resp: Value = serde_json::from_str(lines[0]).unwrap();
        assert_eq!(resp["id"], serde_json::json!(1));
        assert!(resp["result"].is_object());
    }
}
