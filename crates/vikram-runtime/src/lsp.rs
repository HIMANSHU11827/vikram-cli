//! A minimal, synchronous Language Server Protocol client over stdio.
//!
//! The host speaks to language servers out of process, exactly as it speaks to MCP servers: it
//! spawns a command, frames JSON-RPC messages with a `Content-Length` header, and never lets a
//! server's silence hang the caller. Every read is bounded by a timeout, and the child process is
//! killed when the client is dropped so a forgotten server cannot outlive the session.
//!
//! What this module deliberately does *not* do: it does not install language servers and it does
//! not pretend a server is present when it is not. A missing server is reported as
//! [`LspError::Missing`] with the name of what to install. A request that times out is retried
//! exactly once — the first call after a server starts usually lands while it is still indexing and
//! answers too late — while every other failure is returned as it arrived.

use serde_json::{Value, json};
use std::collections::HashMap;
use std::collections::hash_map::Entry;
use std::fmt;
use std::io::{BufRead, BufReader, Write};
use std::path::Path;
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::mpsc::{Receiver, RecvTimeoutError};
use std::sync::{Arc, LazyLock, Mutex, TryLockError};
use std::time::{Duration, Instant};

/// How many of the server's last stderr lines are kept for error messages.
const STDERR_LINES: usize = 20;

/// How long a dying server is given to finish writing its explanation to stderr.
const STDERR_WAIT: Duration = Duration::from_millis(200);

/// How long a request may wait for its response before the call is abandoned.
pub const DEFAULT_TIMEOUT: Duration = Duration::from_millis(10_000);

/// How long to wait before repeating a request that timed out.
///
/// Long enough for a cold server to finish the indexing pass that made the first attempt late,
/// short enough that a caller waiting on a genuinely unresponsive server is not held up twice over.
pub const RETRY_DELAY: Duration = Duration::from_millis(250);

/// How long [`LspClient::diagnostics`] waits for push notifications on servers that do not
/// implement pull diagnostics.
pub const DRAIN_TIMEOUT: Duration = Duration::from_millis(500);

/// Everything that can go wrong while talking to a language server.
#[derive(Debug)]
pub enum LspError {
    /// No server is known for the file's extension and no override was configured.
    Unsupported { ext: String },
    /// A server is known for the extension, but it is not installed.
    Missing { ext: String, install: String },
    /// The path carries no extension at all, so no server can be chosen.
    NoExtension { path: String },
    /// The process could not be started for a reason other than "not found".
    Spawn { command: String, reason: String },
    /// Reading or writing the pipe failed (including the server closing its end).
    Io(String),
    /// The bytes on the wire were not valid LSP framing or not valid JSON.
    Protocol(String),
    /// The server did not answer in time.
    Timeout { method: String, millis: u64 },
    /// The server answered with a JSON-RPC error.
    Server {
        method: String,
        code: i64,
        message: String,
    },
}

impl fmt::Display for LspError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Unsupported { ext } => write!(
                f,
                "no language server for .{ext} (set VIKRAM_LSP_{} to the server command)",
                ext.to_uppercase()
            ),
            Self::Missing { ext, install } => {
                write!(f, "no language server for .{ext} (install {install})")
            }
            Self::NoExtension { path } => {
                write!(
                    f,
                    "no language server for `{path}`: the file has no extension"
                )
            }
            Self::Spawn { command, reason } => {
                write!(f, "cannot start language server `{command}`: {reason}")
            }
            Self::Io(reason) => write!(f, "language server i/o failure: {reason}"),
            Self::Protocol(reason) => write!(f, "language server protocol error: {reason}"),
            Self::Timeout { method, millis } => write!(
                f,
                "language server did not answer `{method}` within {millis} ms"
            ),
            Self::Server {
                method,
                code,
                message,
            } => write!(f, "language server rejected `{method}` ({code}): {message}"),
        }
    }
}

impl std::error::Error for LspError {}

/// Whether a failed request is worth sending again, exactly once.
///
/// Only a timeout is. A timed-out request says nothing about the request itself — the server was
/// busy indexing, or not yet awake — and a second attempt with a fresh deadline usually lands. A
/// protocol error, an i/o failure or a server-side rejection is deterministic: repeating it would
/// only make the caller wait twice to be told the same thing.
fn should_retry(err: &LspError) -> bool {
    matches!(err, LspError::Timeout { .. })
}

/// A resolved language server command, plus what to install if it is absent.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ServerCommand {
    pub program: String,
    pub args: Vec<String>,
    /// The file extension this server was chosen for (`rs`, `py`, …).
    pub ext: String,
    /// The package or binary the user must install to provide this server.
    pub install: String,
}

impl ServerCommand {
    /// Parse a whitespace-separated command line such as `basedpyright-langserver --stdio`.
    ///
    /// Deliberately simpler than a shell: quotes and escapes are not interpreted. An override that
    /// needs them should be wrapped in a small script instead.
    pub fn parse(line: &str, ext: &str, install: &str) -> Option<Self> {
        let mut parts = line.split_whitespace();
        let program = parts.next()?.to_string();
        Some(Self {
            program,
            args: parts.map(str::to_owned).collect(),
            ext: ext.to_string(),
            install: install.to_string(),
        })
    }

    /// `program arg arg` — how this server is named in error messages and in the pool key.
    pub fn display(&self) -> String {
        if self.args.is_empty() {
            self.program.clone()
        } else {
            format!("{} {}", self.program, self.args.join(" "))
        }
    }
}

/// Default server per extension: `(extension, command line, install hint)`.
const DEFAULTS: &[(&str, &str, &str)] = &[
    ("rs", "rust-analyzer", "rust-analyzer"),
    ("py", "basedpyright-langserver --stdio", "basedpyright"),
    ("pyi", "basedpyright-langserver --stdio", "basedpyright"),
    (
        "ts",
        "typescript-language-server --stdio",
        "typescript-language-server",
    ),
    (
        "tsx",
        "typescript-language-server --stdio",
        "typescript-language-server",
    ),
    (
        "js",
        "typescript-language-server --stdio",
        "typescript-language-server",
    ),
    (
        "jsx",
        "typescript-language-server --stdio",
        "typescript-language-server",
    ),
    (
        "mjs",
        "typescript-language-server --stdio",
        "typescript-language-server",
    ),
    (
        "cjs",
        "typescript-language-server --stdio",
        "typescript-language-server",
    ),
    ("go", "gopls", "gopls"),
];

/// The lower-cased extension of a path, without the dot.
pub fn extension_of(path: &Path) -> Option<String> {
    path.extension()
        .map(|ext| ext.to_string_lossy().to_lowercase())
        .filter(|ext| !ext.is_empty())
}

/// The LSP `languageId` for a path. Unknown extensions are passed through unchanged, which is what
/// most servers expect for their own language.
pub fn language_id(path: &Path) -> String {
    match extension_of(path).as_deref() {
        Some("rs") => "rust".into(),
        Some("py" | "pyi") => "python".into(),
        Some("ts") => "typescript".into(),
        Some("tsx") => "typescriptreact".into(),
        Some("js" | "mjs" | "cjs") => "javascript".into(),
        Some("jsx") => "javascriptreact".into(),
        Some("go") => "go".into(),
        Some(other) => other.to_string(),
        None => "plaintext".into(),
    }
}

/// Resolve the server for a path using the process environment.
pub fn server_for_path(path: &Path) -> Result<ServerCommand, LspError> {
    resolve_server(path, |key| std::env::var(key).ok())
}

/// Resolve the server for a path, consulting `env` for `VIKRAM_LSP_<EXT>` overrides.
///
/// The lookup is injected so the mapping can be tested without mutating the process environment.
pub fn resolve_server(
    path: &Path,
    env: impl Fn(&str) -> Option<String>,
) -> Result<ServerCommand, LspError> {
    let ext = extension_of(path).ok_or_else(|| LspError::NoExtension {
        path: path.display().to_string(),
    })?;
    let env_key = format!("VIKRAM_LSP_{}", ext.to_uppercase());
    if let Some(line) = env(&env_key) {
        if let Some(command) =
            ServerCommand::parse(line.trim(), &ext, "the server named in the override")
        {
            return Ok(command);
        }
    }
    let Some((_, line, install)) = DEFAULTS.iter().find(|(known, _, _)| *known == ext) else {
        return Err(LspError::Unsupported { ext });
    };
    ServerCommand::parse(line, &ext, install).ok_or(LspError::Unsupported { ext })
}

/// How long to wait for a response; `VIKRAM_LSP_TIMEOUT_MS` overrides [`DEFAULT_TIMEOUT`].
pub fn timeout_from_env() -> Duration {
    std::env::var("VIKRAM_LSP_TIMEOUT_MS")
        .ok()
        .and_then(|raw| raw.trim().parse::<u64>().ok())
        .map(Duration::from_millis)
        .filter(|value| !value.is_zero())
        .unwrap_or(DEFAULT_TIMEOUT)
}

/// `file://` URI for a path, percent-encoding everything outside the unreserved set.
pub fn path_to_uri(path: &Path) -> String {
    let mut uri = String::from("file://");
    let text = path.to_string_lossy();
    for byte in text.as_bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'.' | b'_' | b'~' | b'/' | b':' => {
                uri.push(char::from(*byte));
            }
            other => uri.push_str(&format!("%{other:02X}")),
        }
    }
    uri
}

/// Encode one JSON-RPC message as an LSP frame: `Content-Length: N\r\n\r\n<json>`.
fn encode_frame(message: &Value) -> Result<Vec<u8>, LspError> {
    let body = serde_json::to_vec(message)
        .map_err(|e| LspError::Protocol(format!("cannot serialise a message: {e}")))?;
    let mut frame = format!("Content-Length: {}\r\n\r\n", body.len()).into_bytes();
    frame.extend_from_slice(&body);
    Ok(frame)
}

/// Read one framed message. `Ok(None)` means the peer closed the stream cleanly.
fn read_frame(reader: &mut impl BufRead) -> Result<Option<Value>, LspError> {
    let mut content_length: Option<usize> = None;
    let mut saw_header = false;
    for _ in 0..64 {
        let mut line = String::new();
        let read = reader
            .read_line(&mut line)
            .map_err(|e| LspError::Io(format!("cannot read from the language server: {e}")))?;
        if read == 0 {
            return if saw_header {
                Err(LspError::Protocol(
                    "the language server closed the stream mid-header".into(),
                ))
            } else {
                Ok(None)
            };
        }
        saw_header = true;
        let trimmed = line.trim_end_matches(['\r', '\n']);
        if trimmed.is_empty() {
            break;
        }
        if let Some(raw) = trimmed.strip_prefix("Content-Length:") {
            let length = raw.trim().parse::<usize>().map_err(|e| {
                LspError::Protocol(format!("bad `Content-Length: {raw}` header: {e}"))
            })?;
            content_length = Some(length);
        }
    }
    let Some(length) = content_length else {
        return Err(LspError::Protocol(
            "message without a Content-Length header".into(),
        ));
    };
    let mut body = vec![0u8; length];
    reader
        .read_exact(&mut body)
        .map_err(|e| LspError::Io(format!("cannot read a message body: {e}")))?;
    let value = serde_json::from_slice(&body)
        .map_err(|e| LspError::Protocol(format!("invalid JSON from the language server: {e}")))?;
    Ok(Some(value))
}

/// One language server process, driven synchronously.
///
/// A background thread owns the server's stdout and feeds parsed messages into a channel, so every
/// wait has a real deadline instead of hanging on a `read`. A second thread keeps the last few
/// lines of stderr, because a server that dies usually explains why on stderr and the caller
/// deserves that explanation instead of a bare "the pipe closed".
pub struct LspClient {
    server: ServerCommand,
    child: Child,
    stdin: ChildStdin,
    inbox: Receiver<Result<Value, String>>,
    stderr: Arc<Mutex<Vec<String>>>,
    next_id: i64,
    /// Documents handed to the server: URI → the exact text that was sent.
    opened: HashMap<String, String>,
    /// Monotonic document versions, so a re-open is always newer than the last one.
    versions: HashMap<String, i64>,
    /// Latest `textDocument/publishDiagnostics` payload per URI.
    published: HashMap<String, Vec<Value>>,
    /// Notifications received, newest last (bounded).
    notifications: Vec<Value>,
    capabilities: Value,
    pull_diagnostics: bool,
    timeout: Duration,
}

impl LspClient {
    /// Start a server and complete the `initialize` / `initialized` handshake.
    pub fn start(server: &ServerCommand, root: &Path) -> Result<Self, LspError> {
        Self::start_with_timeout(server, root, timeout_from_env())
    }

    /// Same as [`LspClient::start`] with an explicit per-request timeout.
    pub fn start_with_timeout(
        server: &ServerCommand,
        root: &Path,
        timeout: Duration,
    ) -> Result<Self, LspError> {
        let mut command = Command::new(&server.program);
        command
            .args(&server.args)
            .current_dir(root)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            // Drained on its own thread: a server that logs heavily must not fill a pipe we never
            // read, and a server that dies must still be able to say why.
            .stderr(Stdio::piped());
        let mut child = command.spawn().map_err(|e| match e.kind() {
            std::io::ErrorKind::NotFound => LspError::Missing {
                ext: server.ext.clone(),
                install: server.install.clone(),
            },
            _ => LspError::Spawn {
                command: server.display(),
                reason: e.to_string(),
            },
        })?;
        let spawn_failed = |reason: String| LspError::Spawn {
            command: server.display(),
            reason,
        };
        let stdin = child
            .stdin
            .take()
            .ok_or_else(|| spawn_failed("stdin was not captured".into()))?;
        let stdout = child
            .stdout
            .take()
            .ok_or_else(|| spawn_failed("stdout was not captured".into()))?;

        let stderr = child
            .stderr
            .take()
            .ok_or_else(|| spawn_failed("stderr was not captured".into()))?;

        let (sender, inbox) = std::sync::mpsc::channel();
        std::thread::Builder::new()
            .name(format!("lsp-reader-{}", server.program))
            .spawn(move || {
                let mut reader = BufReader::new(stdout);
                loop {
                    match read_frame(&mut reader) {
                        Ok(Some(message)) => {
                            if sender.send(Ok(message)).is_err() {
                                return;
                            }
                        }
                        Ok(None) => {
                            let _ =
                                sender.send(Err("the language server closed its stdout".into()));
                            return;
                        }
                        Err(e) => {
                            let _ = sender.send(Err(e.to_string()));
                            return;
                        }
                    }
                }
            })
            .map_err(|e| spawn_failed(format!("cannot start the reader thread: {e}")))?;

        // Keep the tail of stderr so a server that dies mid-handshake can explain itself.
        let tail: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
        let tail_for_thread = Arc::clone(&tail);
        std::thread::Builder::new()
            .name(format!("lsp-stderr-{}", server.program))
            .spawn(move || {
                for line in BufReader::new(stderr).lines() {
                    let Ok(line) = line else { return };
                    let mut lines = tail_for_thread
                        .lock()
                        .unwrap_or_else(|poisoned| poisoned.into_inner());
                    if lines.len() >= STDERR_LINES {
                        lines.remove(0);
                    }
                    lines.push(line);
                }
            })
            .map_err(|e| spawn_failed(format!("cannot start the stderr thread: {e}")))?;

        let mut client = Self {
            server: server.clone(),
            child,
            stdin,
            inbox,
            stderr: tail,
            next_id: 1,
            opened: HashMap::new(),
            versions: HashMap::new(),
            published: HashMap::new(),
            notifications: Vec::new(),
            capabilities: Value::Null,
            pull_diagnostics: false,
            timeout,
        };
        client.initialize(root)?;
        Ok(client)
    }

    /// `initialize` + `initialized`. Kept separate so a failed handshake drops the child.
    fn initialize(&mut self, root: &Path) -> Result<(), LspError> {
        let root_uri = path_to_uri(root);
        let params = json!({
            "processId": std::process::id(),
            "clientInfo": {"name": "vikram", "version": env!("CARGO_PKG_VERSION")},
            "rootUri": root_uri,
            "rootPath": root.to_string_lossy(),
            "workspaceFolders": [{"uri": root_uri, "name": root_name(root)}],
            "capabilities": {
                "textDocument": {
                    "hover": {"contentFormat": ["markdown", "plaintext"]},
                    "definition": {"linkSupport": false},
                    "references": {},
                    "documentSymbol": {"hierarchicalDocumentSymbolSupport": true},
                    "publishDiagnostics": {"relatedInformation": true},
                    "diagnostic": {}
                },
                "workspace": {"workspaceFolders": true, "configuration": true}
            }
        });
        let result = self.request_once("initialize", &params)?;
        self.capabilities = result.get("capabilities").cloned().unwrap_or(Value::Null);
        self.pull_diagnostics = self
            .capabilities
            .get("diagnosticProvider")
            .is_some_and(|provider| !provider.is_null());
        self.notify("initialized", json!({}))
    }

    /// The command that started this server.
    pub fn server(&self) -> &ServerCommand {
        &self.server
    }

    /// The `initialize` result's `capabilities`, as advertised by the server.
    pub fn capabilities(&self) -> &Value {
        &self.capabilities
    }

    /// Notifications received so far (bounded ring, newest last).
    pub fn notifications(&self) -> &[Value] {
        &self.notifications
    }

    /// Whether the server process is still running.
    ///
    /// A cheap `try_wait`, never a request: the pool asks this before handing a client out, so a
    /// server that crashed while nobody was looking is replaced instead of failing every call.
    fn is_alive(&mut self) -> bool {
        matches!(self.child.try_wait(), Ok(None))
    }

    /// Send a notification (a message with no id).
    fn notify(&mut self, method: &str, params: Value) -> Result<(), LspError> {
        self.send(&json!({"jsonrpc": "2.0", "method": method, "params": params}))
    }

    /// Send a request and wait for its response, or fail with [`LspError::Timeout`].
    ///
    /// A request that times out is repeated once, after [`RETRY_DELAY`] and with a fresh deadline:
    /// the first call against a cold server routinely lands while it is still indexing. Only a
    /// timeout is repeated (see `should_retry`), so a real failure is never masked by a retry.
    pub fn request(&mut self, method: &str, params: Value) -> Result<Value, LspError> {
        match self.request_once(method, &params) {
            Err(error) if should_retry(&error) => {
                std::thread::sleep(RETRY_DELAY);
                self.request_once(method, &params)
            }
            result => result,
        }
    }

    /// One attempt at a request: send it under a new id and wait up to `self.timeout`.
    ///
    /// The handshake deliberately goes through this rather than [`LspClient::request`]: sending
    /// `initialize` twice would be a protocol violation, not a second chance.
    fn request_once(&mut self, method: &str, params: &Value) -> Result<Value, LspError> {
        let id = self.next_id;
        self.next_id += 1;
        self.send(&json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params}))?;
        let deadline = Instant::now() + self.timeout;
        loop {
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                return Err(self.timed_out(method));
            }
            let message = match self.inbox.recv_timeout(remaining) {
                Ok(Ok(message)) => message,
                Ok(Err(reason)) => return Err(self.io_error(reason)),
                Err(RecvTimeoutError::Timeout) => return Err(self.timed_out(method)),
                Err(RecvTimeoutError::Disconnected) => {
                    return Err(self.io_error("the language server is no longer running".into()));
                }
            };
            if let Some(result) = self.handle(message, method, id)? {
                return Ok(result);
            }
        }
    }

    fn timed_out(&self, method: &str) -> LspError {
        LspError::Timeout {
            method: method.to_string(),
            millis: u64::try_from(self.timeout.as_millis()).unwrap_or(u64::MAX),
        }
    }

    /// An i/o failure, carrying whatever the server last said on stderr.
    ///
    /// Without this a server that refuses to start (a rustup shim without the component, a missing
    /// runtime, a bad config file) is indistinguishable from a crash: the caller only sees the
    /// pipe close.
    fn io_error(&self, reason: String) -> LspError {
        match self.stderr_tail() {
            Some(tail) => LspError::Io(format!("{reason}; the server said: {tail}")),
            None => LspError::Io(reason),
        }
    }

    /// The last stderr lines, waiting briefly for a dying server to flush them.
    fn stderr_tail(&self) -> Option<String> {
        let deadline = Instant::now() + STDERR_WAIT;
        loop {
            {
                let lines = self
                    .stderr
                    .lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner());
                if !lines.is_empty() {
                    return Some(lines.join(" | "));
                }
            }
            if Instant::now() >= deadline {
                return None;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
    }

    /// Classify one incoming message. Returns the result when it answers `wanted`.
    ///
    /// Server-initiated requests are answered immediately: a server waiting on us is a server that
    /// will never answer the request we are waiting on.
    fn handle(
        &mut self,
        message: Value,
        method: &str,
        wanted: i64,
    ) -> Result<Option<Value>, LspError> {
        if let Some(server_method) = message.get("method").and_then(Value::as_str) {
            let server_method = server_method.to_string();
            let params = message.get("params").cloned().unwrap_or(Value::Null);
            let id = message.get("id").cloned().filter(|id| !id.is_null());
            match id {
                Some(id) => {
                    let result = server_request_result(&server_method, &params);
                    self.send(&json!({"jsonrpc": "2.0", "id": id, "result": result}))?;
                }
                None => self.record(&server_method, params),
            }
            return Ok(None);
        }
        if message.get("id").and_then(Value::as_i64) != Some(wanted) {
            // A response to a request we already gave up on. Dropped on purpose.
            return Ok(None);
        }
        if let Some(error) = message.get("error") {
            let code = error.get("code").and_then(Value::as_i64).unwrap_or(0);
            let text = error
                .get("message")
                .and_then(Value::as_str)
                .unwrap_or("unspecified error")
                .to_string();
            return Err(LspError::Server {
                method: method.to_string(),
                code,
                message: text,
            });
        }
        Ok(Some(message.get("result").cloned().unwrap_or(Value::Null)))
    }

    /// Remember a notification. Diagnostics are indexed by URI so a later query can find them.
    fn record(&mut self, method: &str, params: Value) {
        if method == "textDocument/publishDiagnostics" {
            if let Some(uri) = params.get("uri").and_then(Value::as_str) {
                let diagnostics = params
                    .get("diagnostics")
                    .and_then(Value::as_array)
                    .cloned()
                    .unwrap_or_default();
                self.published.insert(uri.to_string(), diagnostics);
            }
        }
        if self.notifications.len() >= 128 {
            self.notifications.remove(0);
        }
        self.notifications
            .push(json!({"method": method, "params": params}));
    }

    /// Read and handle whatever arrived within `within`, ignoring timeouts.
    fn drain(&mut self, within: Duration) {
        let deadline = Instant::now() + within;
        loop {
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                return;
            }
            match self.inbox.recv_timeout(remaining) {
                Ok(Ok(message)) => {
                    if self.handle(message, "", -1).is_err() {
                        return;
                    }
                }
                Ok(Err(_)) | Err(_) => return,
            }
        }
    }

    fn send(&mut self, message: &Value) -> Result<(), LspError> {
        let frame = encode_frame(message)?;
        self.stdin
            .write_all(&frame)
            .and_then(|()| self.stdin.flush())
            .map_err(|e| LspError::Io(format!("cannot write to the language server: {e}")))
    }

    /// Send `textDocument/didOpen`. Re-opening a document that is already open closes it first,
    /// which is the one way to replace its text that every server accepts.
    pub fn open_document(&mut self, path: &Path, text: &str) -> Result<(), LspError> {
        let uri = path_to_uri(path);
        if self.opened.remove(&uri).is_some() {
            self.notify(
                "textDocument/didClose",
                json!({"textDocument": {"uri": uri}}),
            )?;
        }
        let version = {
            let version = self.versions.entry(uri.clone()).or_insert(0);
            *version += 1;
            *version
        };
        self.notify(
            "textDocument/didOpen",
            json!({"textDocument": {
                "uri": uri,
                "languageId": language_id(path),
                "version": version,
                "text": text
            }}),
        )?;
        self.opened.insert(uri, text.to_string());
        Ok(())
    }

    /// Send `textDocument/didClose` if the document is open.
    pub fn close_document(&mut self, path: &Path) -> Result<(), LspError> {
        let uri = path_to_uri(path);
        if self.opened.remove(&uri).is_some() {
            self.notify(
                "textDocument/didClose",
                json!({"textDocument": {"uri": uri}}),
            )?;
        }
        Ok(())
    }

    /// Make sure the server has the current on-disk text of `path`.
    pub fn ensure_open(&mut self, path: &Path) -> Result<(), LspError> {
        let text = std::fs::read_to_string(path)
            .map_err(|e| LspError::Io(format!("cannot read `{}`: {e}", path.display())))?;
        let uri = path_to_uri(path);
        if self.opened.get(&uri).is_some_and(|open| *open == text) {
            return Ok(());
        }
        self.open_document(path, &text)
    }

    fn at(&mut self, path: &Path, line: u32, column: u32) -> Result<Value, LspError> {
        self.ensure_open(path)?;
        Ok(json!({
            "textDocument": {"uri": path_to_uri(path)},
            "position": {"line": line, "character": column}
        }))
    }

    /// `textDocument/hover` at a 0-based line and UTF-16 column.
    pub fn hover(&mut self, path: &Path, line: u32, column: u32) -> Result<Value, LspError> {
        let params = self.at(path, line, column)?;
        self.request("textDocument/hover", params)
    }

    /// `textDocument/definition` at a 0-based line and UTF-16 column.
    pub fn definition(&mut self, path: &Path, line: u32, column: u32) -> Result<Value, LspError> {
        let params = self.at(path, line, column)?;
        self.request("textDocument/definition", params)
    }

    /// `textDocument/references`, including the declaration itself.
    pub fn references(&mut self, path: &Path, line: u32, column: u32) -> Result<Value, LspError> {
        let mut params = self.at(path, line, column)?;
        params["context"] = json!({"includeDeclaration": true});
        self.request("textDocument/references", params)
    }

    /// `textDocument/documentSymbol` for a whole file.
    pub fn document_symbols(&mut self, path: &Path) -> Result<Value, LspError> {
        self.ensure_open(path)?;
        self.request(
            "textDocument/documentSymbol",
            json!({"textDocument": {"uri": path_to_uri(path)}}),
        )
    }

    /// Diagnostics for a file: pull (`textDocument/diagnostic`) when the server advertises it,
    /// otherwise the pushed `textDocument/publishDiagnostics` notifications collected so far.
    ///
    /// Either way the answer names the mechanism that produced it, so the caller never has to
    /// guess whether "no diagnostics" means clean or "not implemented".
    pub fn diagnostics(&mut self, path: &Path) -> Result<Value, LspError> {
        self.ensure_open(path)?;
        let uri = path_to_uri(path);
        if self.pull_diagnostics {
            let pulled = self.request(
                "textDocument/diagnostic",
                json!({"textDocument": {"uri": uri}}),
            );
            match pulled {
                Ok(report) => {
                    let items = report
                        .get("items")
                        .and_then(Value::as_array)
                        .cloned()
                        .unwrap_or_default();
                    return Ok(json!({
                        "source": "textDocument/diagnostic",
                        "server": self.server.display(),
                        "uri": uri,
                        "count": items.len(),
                        "diagnostics": items,
                        "report": report
                    }));
                }
                // A server may advertise the capability and still refuse a single file.
                Err(LspError::Server { .. }) => {}
                Err(other) => return Err(other),
            }
        }
        self.drain(DRAIN_TIMEOUT);
        let items = self.published.get(&uri).cloned().unwrap_or_default();
        Ok(json!({
            "source": "textDocument/publishDiagnostics",
            "server": self.server.display(),
            "uri": uri,
            "count": items.len(),
            "diagnostics": items
        }))
    }
}

impl Drop for LspClient {
    fn drop(&mut self) {
        // Polite first, then certain: `shutdown`/`exit` are best-effort (a wedged server ignores
        // them), the kill is not.
        let id = self.next_id;
        let _ =
            self.send(&json!({"jsonrpc": "2.0", "id": id, "method": "shutdown", "params": null}));
        let _ = self.notify("exit", Value::Null);
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

fn root_name(root: &Path) -> String {
    root.file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_else(|| root.to_string_lossy().into_owned())
}

/// Answer a server-initiated request without pretending to know more than we do.
fn server_request_result(method: &str, params: &Value) -> Value {
    match method {
        // One `null` per requested section: "I have no configured value for any of these."
        "workspace/configuration" => {
            let count = params
                .get("items")
                .and_then(Value::as_array)
                .map(Vec::len)
                .unwrap_or(0);
            Value::Array(vec![Value::Null; count])
        }
        "workspace/workspaceFolders" => Value::Null,
        _ => Value::Null,
    }
}

/// The pool key for `server` under `root`: the two facts that decide which process is reused.
fn pool_key(server: &ServerCommand, root: &Path) -> String {
    format!("{}\u{0}{}", server.display(), root.display())
}

/// Clone the handle registered under `key`, holding the map lock only for the lookup.
fn shared_slot<V>(
    clients: &Mutex<HashMap<String, Arc<Mutex<V>>>>,
    key: &str,
) -> Option<Arc<Mutex<V>>> {
    clients
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .get(key)
        .map(Arc::clone)
}

/// Register `client` under `key`, or hand back the handle that another call registered first.
///
/// Two threads can race to start the same server. The loser keeps the winner's handle and drops its
/// own, so exactly one process per key stays reachable — and the orphan is killed on drop.
fn register<V>(
    clients: &Mutex<HashMap<String, Arc<Mutex<V>>>>,
    key: String,
    client: Arc<Mutex<V>>,
) -> Arc<Mutex<V>> {
    let mut clients = clients
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    match clients.entry(key) {
        Entry::Occupied(entry) => Arc::clone(entry.get()),
        Entry::Vacant(entry) => {
            entry.insert(Arc::clone(&client));
            client
        }
    }
}

/// Drop the entry for `key`. The server is killed when the last handle to it is dropped.
fn forget<V>(clients: &Mutex<HashMap<String, Arc<Mutex<V>>>>, key: &str) {
    clients
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .remove(key);
}

/// Live clients, one per (server, workspace root), shared by the tools.
///
/// Starting rust-analyzer costs seconds; holding one per project amortises that across tool calls.
/// Each client sits behind its *own* mutex, and the map lock is only ever held long enough to clone
/// a handle out of it: a slow request against Python cannot delay a request against Rust, because
/// the two never contend. Requests to the *same* language still queue, which is what the protocol
/// requires — one server is one peer, and its answers arrive on one stream.
pub struct LspPool {
    clients: Mutex<HashMap<String, Arc<Mutex<LspClient>>>>,
}

static POOL: LazyLock<LspPool> = LazyLock::new(LspPool::new);

impl Default for LspPool {
    fn default() -> Self {
        Self::new()
    }
}

impl LspPool {
    pub fn new() -> Self {
        Self {
            clients: Mutex::new(HashMap::new()),
        }
    }

    /// The process-wide pool used by the `lsp.*` tools.
    pub fn global() -> &'static Self {
        &POOL
    }

    /// Run `action` against the server for `file`, starting it if this pair has no live client.
    ///
    /// The map is locked only to hand out the handle; the caller then locks that one client, so
    /// requests for different languages run at the same time. A client whose transport has died or
    /// whose lock was poisoned is evicted, and the next call starts a fresh server instead of
    /// failing forever.
    pub fn call<T>(
        &self,
        root: &Path,
        file: &Path,
        action: impl FnOnce(&mut LspClient) -> Result<T, LspError>,
    ) -> Result<T, LspError> {
        let server = server_for_path(file)?;
        let root = root.canonicalize().unwrap_or_else(|_| root.to_path_buf());
        let key = pool_key(&server, &root);
        let client = self.get(&server, &root)?;
        // Recorded before locking: `get` already replaces a poisoned client, so this only catches a
        // panic that landed in the gap between the two, and such a client must not be reused.
        let poisoned = client.is_poisoned();
        let result = {
            let mut client = client
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            action(&mut client)
        };
        if poisoned || matches!(&result, Err(LspError::Io(_) | LspError::Protocol(_))) {
            forget(&self.clients, &key);
        }
        result
    }

    /// The live client for `server` under `root`, starting it if there is none.
    ///
    /// The returned handle is shared: locking it serialises the callers *of this language only*.
    /// A dead or poisoned client is dropped from the map here, so a server that died between calls
    /// is respawned by the call that discovers it rather than poisoning every later one.
    pub fn get(
        &self,
        server: &ServerCommand,
        root: &Path,
    ) -> Result<Arc<Mutex<LspClient>>, LspError> {
        let key = pool_key(server, root);
        if let Some(client) = shared_slot(&self.clients, &key) {
            // Decided without keeping the guard, so the handle can still be handed back below.
            let usable = match client.try_lock() {
                // Busy: another call is mid-request for this language. Hand back the same handle
                // and let this call queue behind it; every other language is unaffected.
                Err(TryLockError::WouldBlock) => true,
                // Poisoned by a panicking request: the client is unusable, so replace it.
                Err(TryLockError::Poisoned(_)) => false,
                Ok(mut guard) => guard.is_alive(),
            };
            if usable {
                return Ok(client);
            }
            // Dead or poisoned: evict, then start a fresh server for this key.
            forget(&self.clients, &key);
        }
        let started = LspClient::start(server, root)?;
        Ok(register(&self.clients, key, Arc::new(Mutex::new(started))))
    }

    /// The registered (server, root) keys, sorted — one per running server.
    pub fn keys(&self) -> Vec<String> {
        let mut keys: Vec<String> = self
            .clients
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .keys()
            .cloned()
            .collect();
        keys.sort();
        keys
    }

    /// Stop every server in this pool (each client kills its child on drop).
    ///
    /// Because handles are shared, a call already in flight keeps its server until it finishes; the
    /// process is killed as soon as the last handle to it is dropped. Nothing new is started from
    /// the map until the next call.
    pub fn shutdown(&self) {
        let mut clients = self
            .clients
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        clients.clear();
    }

    /// How many servers are currently running.
    pub fn len(&self) -> usize {
        self.clients
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .len()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    fn no_env(_: &str) -> Option<String> {
        None
    }

    #[test]
    fn a_frame_round_trips_through_the_reader() {
        let message = json!({"jsonrpc": "2.0", "id": 1, "result": {"ok": true}});
        let frame = encode_frame(&message).unwrap();
        let text = String::from_utf8(frame).unwrap();
        assert!(text.starts_with("Content-Length: "));
        assert!(text.contains("\r\n\r\n"));
        let mut reader = Cursor::new(text.into_bytes());
        assert_eq!(read_frame(&mut reader).unwrap(), Some(message));
        // End of stream, not an error.
        assert_eq!(read_frame(&mut reader).unwrap(), None);
    }

    #[test]
    fn a_message_without_a_content_length_header_is_a_protocol_error() {
        let mut reader = Cursor::new(b"X-Unknown: 1\r\n\r\n{}".to_vec());
        let err = read_frame(&mut reader).unwrap_err();
        assert!(matches!(err, LspError::Protocol(_)), "{err:?}");
        assert!(err.to_string().contains("Content-Length"));
    }

    #[test]
    fn a_truncated_body_is_an_io_error() {
        let mut reader = Cursor::new(b"Content-Length: 40\r\n\r\n{}".to_vec());
        let err = read_frame(&mut reader).unwrap_err();
        assert!(matches!(err, LspError::Io(_)), "{err:?}");
    }

    #[test]
    fn known_extensions_map_to_their_default_server() {
        let rust = resolve_server(Path::new("/w/src/main.rs"), no_env).unwrap();
        assert_eq!(rust.program, "rust-analyzer");
        assert!(rust.args.is_empty());
        assert_eq!(rust.ext, "rs");

        let python = resolve_server(Path::new("/w/app.py"), no_env).unwrap();
        assert_eq!(python.program, "basedpyright-langserver");
        assert_eq!(python.args, vec!["--stdio".to_string()]);

        let typescript = resolve_server(Path::new("/w/a.ts"), no_env).unwrap();
        assert_eq!(typescript.program, "typescript-language-server");
        assert_eq!(typescript.args, vec!["--stdio".to_string()]);
        assert_eq!(
            resolve_server(Path::new("/w/a.js"), no_env)
                .unwrap()
                .program,
            "typescript-language-server"
        );

        let go = resolve_server(Path::new("/w/main.go"), no_env).unwrap();
        assert_eq!(go.program, "gopls");
    }

    #[test]
    fn an_environment_override_replaces_the_default_server() {
        let override_env = |key: &str| match key {
            "VIKRAM_LSP_RS" => Some("my-rust-lsp --stdio".to_string()),
            _ => None,
        };
        let server = resolve_server(Path::new("/w/main.rs"), override_env).unwrap();
        assert_eq!(server.program, "my-rust-lsp");
        assert_eq!(server.args, vec!["--stdio".to_string()]);
        assert_eq!(server.ext, "rs");
        // Other extensions keep their defaults.
        let python = resolve_server(Path::new("/w/app.py"), override_env).unwrap();
        assert_eq!(python.program, "basedpyright-langserver");
    }

    #[test]
    fn an_unknown_extension_reports_the_override_variable() {
        let err = resolve_server(Path::new("/w/notes.txt"), no_env).unwrap_err();
        assert!(matches!(&err, LspError::Unsupported { ext } if ext == "txt"));
        let message = err.to_string();
        assert!(message.contains("no language server for .txt"), "{message}");
        assert!(message.contains("VIKRAM_LSP_TXT"), "{message}");
    }

    #[test]
    fn a_file_without_an_extension_is_reported() {
        let err = resolve_server(Path::new("/w/Makefile"), no_env).unwrap_err();
        assert!(matches!(err, LspError::NoExtension { .. }), "{err:?}");
        assert!(err.to_string().contains("no extension"));
    }

    #[test]
    fn a_missing_binary_is_reported_as_not_installed() {
        let server = ServerCommand {
            program: "vikram-no-such-language-server".into(),
            args: Vec::new(),
            ext: "rs".into(),
            install: "rust-analyzer".into(),
        };
        let dir = tempfile::tempdir().unwrap();
        let err =
            match LspClient::start_with_timeout(&server, dir.path(), Duration::from_millis(200)) {
                Err(e) => e,
                Ok(_) => panic!("expected error"),
            };
        assert!(matches!(err, LspError::Missing { .. }), "{err:?}");
        assert_eq!(
            err.to_string(),
            "no language server for .rs (install rust-analyzer)"
        );
    }

    #[test]
    fn a_server_that_dies_reports_what_it_wrote_to_stderr() {
        // Not a language server: a process that says why it is giving up and then exits, which is
        // exactly what a rustup shim or a misconfigured server does.
        let server = ServerCommand {
            program: "sh".into(),
            args: vec![
                "-c".into(),
                "echo \"error: no such component\" >&2; exit 1".into(),
            ],
            ext: "rs".into(),
            install: "rust-analyzer".into(),
        };
        let dir = tempfile::tempdir().unwrap();
        let err = LspClient::start_with_timeout(&server, dir.path(), Duration::from_millis(500))
            .err()
            .expect("a server that exits must fail the handshake");
        let message = err.to_string();
        assert!(
            message.contains("no such component"),
            "stderr must reach the caller: {message}"
        );
    }

    #[test]
    fn a_uri_percent_encodes_everything_but_path_separators() {
        assert_eq!(
            path_to_uri(Path::new("/tmp/a b/c#d.rs")),
            "file:///tmp/a%20b/c%23d.rs"
        );
        assert_eq!(
            path_to_uri(Path::new("/w/src/main.rs")),
            "file:///w/src/main.rs"
        );
    }

    #[test]
    fn language_ids_follow_the_extension() {
        assert_eq!(language_id(Path::new("a.rs")), "rust");
        assert_eq!(language_id(Path::new("a.py")), "python");
        assert_eq!(language_id(Path::new("a.ts")), "typescript");
        assert_eq!(language_id(Path::new("a.tsx")), "typescriptreact");
        assert_eq!(language_id(Path::new("a.js")), "javascript");
        assert_eq!(language_id(Path::new("a.go")), "go");
        assert_eq!(language_id(Path::new("a.zz")), "zz");
    }

    #[test]
    fn server_requests_are_answered_in_the_shape_that_was_asked_for() {
        let items = json!({"items": [{"section": "rust-analyzer"}, {"section": "editor"}]});
        let answer = server_request_result("workspace/configuration", &items);
        assert_eq!(answer, json!([null, null]));
        assert_eq!(
            server_request_result("window/workDoneProgress/create", &json!({})),
            Value::Null
        );
    }

    #[test]
    fn the_pool_starts_empty_and_stays_usable_after_a_shutdown() {
        let pool = LspPool::new();
        assert!(pool.is_empty());
        assert!(pool.keys().is_empty());
        pool.shutdown();
        assert_eq!(pool.len(), 0);
        assert!(pool.keys().is_empty());
    }

    /// A `ServerCommand` with no arguments, for key arithmetic that must not spawn anything.
    fn command(program: &str, ext: &str) -> ServerCommand {
        ServerCommand::parse(program, ext, program).expect("a bare program always parses")
    }

    /// The map-level mechanics of the pool, exercised without a language server.
    ///
    /// The real pool stores `Arc<Mutex<LspClient>>`, which cannot be built without spawning a
    /// process; the helpers behind it are generic, so the same behaviour can be tested on `u32`.
    #[test]
    fn different_keys_get_distinct_handles_and_one_key_gets_one_handle() {
        let clients: Mutex<HashMap<String, Arc<Mutex<u32>>>> = Mutex::new(HashMap::new());
        let rust = pool_key(&command("rust-analyzer", "rs"), Path::new("/w"));
        let python = pool_key(&command("basedpyright-langserver", "py"), Path::new("/w"));
        // Same server, different workspace root: a separate process, so a separate key.
        let rust_other_root = pool_key(&command("rust-analyzer", "rs"), Path::new("/other"));

        let rust_client = register(&clients, rust.clone(), Arc::new(Mutex::new(1)));
        let python_client = register(&clients, python.clone(), Arc::new(Mutex::new(2)));
        assert!(
            !Arc::ptr_eq(&rust_client, &python_client),
            "different keys must not share a handle: a slow Python request would block Rust"
        );

        // Looking the same key up again clones the handle instead of making a second client.
        let again = shared_slot(&clients, &rust).expect("the key is registered");
        assert!(Arc::ptr_eq(&rust_client, &again));
        assert_eq!(*again.lock().unwrap(), 1);

        // A spawn that lost the race is discarded in favour of the client already registered.
        let late = register(&clients, rust.clone(), Arc::new(Mutex::new(3)));
        assert!(Arc::ptr_eq(&rust_client, &late));
        assert_eq!(*rust_client.lock().unwrap(), 1, "the loser must not win");

        // Eviction forgets exactly one key, so the neighbours keep running.
        forget(&clients, &rust);
        assert!(shared_slot(&clients, &rust).is_none());
        assert!(shared_slot(&clients, &python).is_some());
        assert!(shared_slot(&clients, &rust_other_root).is_none());
    }

    #[test]
    fn a_pool_key_names_both_the_server_and_the_root() {
        let one = pool_key(&command("gopls", "go"), Path::new("/w"));
        let same = pool_key(&command("gopls", "go"), Path::new("/w"));
        assert_eq!(one, same);
        assert!(one.contains("gopls") && one.contains("/w"), "{one}");
        assert_ne!(
            one,
            pool_key(&command("gopls", "go"), Path::new("/other")),
            "the workspace root is part of the key"
        );
        assert_ne!(
            one,
            pool_key(
                &command("typescript-language-server", "ts"),
                Path::new("/w")
            ),
            "the server is part of the key"
        );
    }

    #[test]
    fn only_a_timeout_is_worth_retrying() {
        assert!(should_retry(&LspError::Timeout {
            method: "textDocument/hover".into(),
            millis: 10,
        }));
        // Everything else is deterministic: repeating it only doubles the wait.
        for error in [
            LspError::Unsupported { ext: "txt".into() },
            LspError::Missing {
                ext: "rs".into(),
                install: "rust-analyzer".into(),
            },
            LspError::NoExtension {
                path: "Makefile".into(),
            },
            LspError::Spawn {
                command: "gopls".into(),
                reason: "boom".into(),
            },
            LspError::Io("the pipe closed".into()),
            LspError::Protocol("bad frame".into()),
            LspError::Server {
                method: "textDocument/hover".into(),
                code: -32601,
                message: "method not found".into(),
            },
        ] {
            assert!(!should_retry(&error), "{error:?} must not be retried");
        }
    }

    #[test]
    fn the_pool_can_be_shared_across_threads() {
        fn assert_send_sync<T: Send + Sync>() {}
        // The pool is a process-global: a client that could not cross threads would not compile.
        assert_send_sync::<LspPool>();
    }
}
