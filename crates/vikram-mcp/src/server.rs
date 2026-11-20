//! MCP Server — exposes approved VIKRAM capabilities via JSON-RPC 2.0 over STDIO.
//!
//! Supports: tools (list/call), resources (list/read), prompts (list/get).
//! Tool execution is delegated via a configurable `ToolExecutor`.

use serde_json::{Value, json};
use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

/// Upper bound for one STDIO request line. Anything larger is rejected
/// with a JSON-RPC error instead of being parsed.
const MAX_REQUEST_LINE_BYTES: usize = 1024 * 1024;

/// How long a `tools/call` may occupy the blocking pool before the server
/// stops waiting for it. The executor thread is not killed — `spawn_blocking`
/// tasks cannot be aborted — but the request loop stays responsive.
const TOOL_CALL_TIMEOUT: Duration = Duration::from_secs(60);

fn constant_time_eq(a: &str, b: &str) -> bool {
    let (a, b) = (a.as_bytes(), b.as_bytes());
    if a.len() != b.len() {
        return false;
    }
    let mut diff = 0u8;
    for (x, y) in a.iter().zip(b.iter()) {
        diff |= x ^ y;
    }
    diff == 0
}

/// Configuration for the MCP server.
///
/// Auth: when `auth_required` is true, every request (including `initialize`)
/// must carry a matching `auth_token` (top-level or in `params`) or it is
/// rejected with JSON-RPC error `-32001`. A missing or empty `auth_token`
/// while `auth_required` is set means no client can authenticate — the server
/// refuses it at construction via [`McpServerConfig::validate`]; there is no
/// unauthenticated fallback.
///
/// Tool allowlist: `approved_tools` is an explicit allowlist with NO implicit
/// "empty means all" default. A tool may only be called when its name appears
/// in this list; an empty list therefore exposes nothing, and unlisted tools
/// are rejected with "tool not approved". `tools/list` also filters to the
/// allowlist so unapproved tools are not even advertised.
#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
pub struct McpServerConfig {
    pub name: String,
    pub version: String,
    pub approved_tools: Vec<String>,
    pub auth_required: bool,
    /// Shared secret required when `auth_required` is set. Sent as `auth_token` in requests.
    #[serde(default)]
    pub auth_token: Option<String>,
}

impl McpServerConfig {
    /// Reject configurations that can never operate safely: `auth_required`
    /// without a non-empty `auth_token` would lock out every client (or, worse,
    /// invite a "no token configured means allow" fallback — which this server
    /// deliberately does not have).
    pub fn validate(&self) -> Result<(), String> {
        if self.auth_required {
            match self.auth_token.as_deref() {
                Some(token) if !token.is_empty() => {}
                _ => {
                    return Err("auth_required is set but auth_token is missing or empty; \
                         refusing to start an MCP server no client can authenticate to"
                        .to_string());
                }
            }
        }
        Ok(())
    }

    /// Whether `name` may be called. There is no implicit allow-all: an empty
    /// `approved_tools` list approves nothing.
    pub fn is_tool_approved(&self, name: &str) -> bool {
        self.approved_tools.iter().any(|t| t == name)
    }
}

impl Default for McpServerConfig {
    fn default() -> Self {
        Self {
            name: "vikram".to_string(),
            version: env!("CARGO_PKG_VERSION").to_string(),
            approved_tools: Vec::new(),
            auth_required: false,
            auth_token: None,
        }
    }
}

/// A tool exposed by the MCP server.
#[derive(Clone, Debug)]
pub struct ExposedTool {
    pub name: String,
    pub description: String,
    pub input_schema: Value,
}

/// A resource exposed by the MCP server.
#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
pub struct ExposedResource {
    pub uri: String,
    pub name: String,
    pub description: String,
    pub mime_type: Option<String>,
}

/// Content returned when reading a resource.
#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
pub struct ResourceContent {
    pub uri: String,
    pub mime_type: Option<String>,
    pub text: Option<String>,
    pub blob: Option<String>, // base64-encoded
}

/// A prompt exposed by the MCP server.
#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
pub struct ExposedPrompt {
    pub name: String,
    pub description: String,
    pub arguments: Vec<PromptArgument>,
}

#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
pub struct PromptArgument {
    pub name: String,
    pub description: String,
    pub required: bool,
}

/// Content returned when getting a prompt.
#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
pub struct PromptContent {
    pub description: String,
    pub messages: Vec<PromptMessage>,
}

#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
pub struct PromptMessage {
    pub role: String,
    pub content: Value,
}

/// Trait for executing tools on behalf of the MCP server.
pub trait ToolExecutor: Send + Sync {
    fn execute(&self, name: &str, arguments: Value) -> Result<Value, String>;
}

/// Trait for providing resources.
pub trait ResourceProvider: Send + Sync {
    fn list(&self) -> Vec<ExposedResource>;
    fn read(&self, uri: &str) -> Result<ResourceContent, String>;
}

/// Trait for providing prompts.
pub trait PromptProvider: Send + Sync {
    fn list(&self) -> Vec<ExposedPrompt>;
    fn get(&self, name: &str, arguments: Value) -> Result<PromptContent, String>;
}

/// MCP Server state.
pub struct McpServer {
    config: McpServerConfig,
    tools: HashMap<String, ExposedTool>,
    resources: HashMap<String, ExposedResource>,
    prompts: HashMap<String, ExposedPrompt>,
    tool_executor: Option<Arc<dyn ToolExecutor>>,
    resource_provider: Option<Arc<dyn ResourceProvider>>,
    prompt_provider: Option<Arc<dyn PromptProvider>>,
    tool_call_timeout: Duration,
    running: bool,
}

impl McpServer {
    pub fn new(config: McpServerConfig) -> Self {
        Self {
            config,
            tools: HashMap::new(),
            resources: HashMap::new(),
            prompts: HashMap::new(),
            tool_executor: None,
            resource_provider: None,
            prompt_provider: None,
            tool_call_timeout: TOOL_CALL_TIMEOUT,
            running: false,
        }
    }

    /// Like [`McpServer::new`], but fails fast on an invalid configuration
    /// (see [`McpServerConfig::validate`]) instead of only rejecting requests
    /// at runtime.
    pub fn try_new(config: McpServerConfig) -> Result<Self, String> {
        config.validate()?;
        Ok(Self::new(config))
    }

    pub fn config(&self) -> &McpServerConfig {
        &self.config
    }

    pub fn register_tool(&mut self, tool: ExposedTool) {
        self.tools.insert(tool.name.clone(), tool);
    }

    pub fn unregister_tool(&mut self, name: &str) {
        self.tools.remove(name);
    }

    pub fn register_resource(&mut self, resource: ExposedResource) {
        self.resources.insert(resource.uri.clone(), resource);
    }

    pub fn register_prompt(&mut self, prompt: ExposedPrompt) {
        self.prompts.insert(prompt.name.clone(), prompt);
    }

    pub fn set_tool_executor(&mut self, executor: Arc<dyn ToolExecutor>) {
        self.tool_executor = Some(executor);
    }

    pub fn set_resource_provider(&mut self, provider: Arc<dyn ResourceProvider>) {
        self.resource_provider = Some(provider);
    }

    pub fn set_prompt_provider(&mut self, provider: Arc<dyn PromptProvider>) {
        self.prompt_provider = Some(provider);
    }

    /// Override the `tools/call` execution timeout (default 60s). Mostly
    /// for tests; production callers should leave the default.
    pub fn set_tool_call_timeout(&mut self, timeout: Duration) {
        self.tool_call_timeout = timeout;
    }

    pub fn approved_tools(&self) -> &[String] {
        &self.config.approved_tools
    }

    /// A request is authenticated when `auth_required` is off, or when it carries
    /// `auth_token` (top level or in `params`) matching the configured secret.
    fn authenticated(&self, request: &Value) -> bool {
        let Some(expected) = self.config.auth_token.as_deref() else {
            return false;
        };
        let supplied = request
            .get("auth_token")
            .and_then(Value::as_str)
            .or_else(|| {
                request
                    .get("params")
                    .and_then(|p| p.get("auth_token"))
                    .and_then(Value::as_str)
            });
        supplied.is_some_and(|s| constant_time_eq(s, expected))
    }

    pub fn is_running(&self) -> bool {
        self.running
    }

    /// Handle a single JSON-RPC request and return the response.
    ///
    /// Async because `tools/call` runs the synchronous [`ToolExecutor`] on
    /// the blocking pool under [`Self::tool_call_timeout`], so one slow tool
    /// cannot stall the STDIO request loop.
    pub async fn handle_request(&self, request: &Value) -> Value {
        let id = request.get("id").cloned().unwrap_or(Value::Null);
        let method = request.get("method").and_then(Value::as_str).unwrap_or("");

        if self.config.auth_required && !self.authenticated(request) {
            return json!({
                "jsonrpc": "2.0",
                "id": id,
                "error": {"code": -32001, "message": "unauthenticated: provide auth_token"}
            });
        }

        match method {
            "initialize" => {
                json!({
                    "jsonrpc": "2.0",
                    "id": id,
                    "result": {
                        "protocolVersion": "2024-11-05",
                        "capabilities": {
                            "tools": {"listChanged": true},
                            "resources": {"subscribe": false, "listChanged": true},
                            "prompts": {"listChanged": true}
                        },
                        "serverInfo": {
                            "name": self.config.name,
                            "version": self.config.version
                        }
                    }
                })
            }
            "notifications/initialized" => {
                json!({"jsonrpc": "2.0", "result": {}})
            }
            "tools/list" => {
                let tools: Vec<Value> = self
                    .tools
                    .values()
                    .filter(|t| self.config.is_tool_approved(&t.name))
                    .map(|t| {
                        json!({
                            "name": t.name,
                            "description": t.description,
                            "inputSchema": t.input_schema
                        })
                    })
                    .collect();
                json!({
                    "jsonrpc": "2.0",
                    "id": id,
                    "result": {"tools": tools}
                })
            }
            "tools/call" => {
                let name = request
                    .get("params")
                    .and_then(|p| p.get("name"))
                    .and_then(Value::as_str)
                    .unwrap_or("");
                let args = request
                    .get("params")
                    .and_then(|p| p.get("arguments"))
                    .cloned()
                    .unwrap_or(Value::Null);

                if !self.tools.contains_key(name) {
                    return json!({
                        "jsonrpc": "2.0",
                        "id": id,
                        "error": {"code": -32601, "message": format!("tool not found: {name}")}
                    });
                }

                if !self.config.is_tool_approved(name) {
                    return json!({
                        "jsonrpc": "2.0",
                        "id": id,
                        "error": {"code": -32603, "message": format!("tool not approved: {name}")}
                    });
                }

                // Delegate to executor if available. The executor is
                // synchronous, so it runs on the blocking pool; a timeout
                // stops us waiting but cannot kill the thread — the tool
                // may still finish in the background.
                if let Some(executor) = self.tool_executor.clone() {
                    let tool_name = name.to_string();
                    let call =
                        tokio::task::spawn_blocking(move || executor.execute(&tool_name, args));
                    match tokio::time::timeout(self.tool_call_timeout, call).await {
                        Ok(Ok(Ok(result))) => {
                            let text = if result.is_string() {
                                result.as_str().unwrap_or("").to_string()
                            } else {
                                result.to_string()
                            };
                            json!({
                                "jsonrpc": "2.0",
                                "id": id,
                                "result": {
                                    "content": [{"type": "text", "text": text}]
                                }
                            })
                        }
                        Ok(Ok(Err(e))) => {
                            json!({
                                "jsonrpc": "2.0",
                                "id": id,
                                "error": {"code": -32603, "message": e}
                            })
                        }
                        Ok(Err(e)) => {
                            json!({
                                "jsonrpc": "2.0",
                                "id": id,
                                "error": {"code": -32603, "message": format!("tool executor failed: {e}")}
                            })
                        }
                        Err(_) => {
                            json!({
                                "jsonrpc": "2.0",
                                "id": id,
                                "error": {"code": -32603, "message": format!("tool `{name}` timed out after {:?}", self.tool_call_timeout)}
                            })
                        }
                    }
                } else {
                    // No executor means the call cannot run — a stub "success" would tell the
                    // client work happened that never did.
                    json!({
                        "jsonrpc": "2.0",
                        "id": id,
                        "error": {"code": -32601, "message": "no tool executor configured"}
                    })
                }
            }
            "resources/list" => {
                let resources: Vec<Value> = if let Some(provider) = &self.resource_provider {
                    provider
                        .list()
                        .iter()
                        .map(|r| {
                            let mut obj = json!({
                                "uri": r.uri,
                                "name": r.name,
                                "description": r.description
                            });
                            if let Some(mt) = &r.mime_type {
                                obj["mimeType"] = json!(mt);
                            }
                            obj
                        })
                        .collect()
                } else {
                    self.resources
                        .values()
                        .map(|r| {
                            let mut obj = json!({
                                "uri": r.uri,
                                "name": r.name,
                                "description": r.description
                            });
                            if let Some(mt) = &r.mime_type {
                                obj["mimeType"] = json!(mt);
                            }
                            obj
                        })
                        .collect()
                };
                json!({
                    "jsonrpc": "2.0",
                    "id": id,
                    "result": {"resources": resources}
                })
            }
            "resources/read" => {
                let uri = request
                    .get("params")
                    .and_then(|p| p.get("uri"))
                    .and_then(Value::as_str)
                    .unwrap_or("");

                let content = if let Some(provider) = &self.resource_provider {
                    provider.read(uri)
                } else if let Some(r) = self.resources.get(uri) {
                    Ok(ResourceContent {
                        uri: r.uri.clone(),
                        mime_type: r.mime_type.clone(),
                        text: Some(format!("Resource: {}", r.name)),
                        blob: None,
                    })
                } else {
                    Err(format!("resource not found: {uri}"))
                };

                match content {
                    Ok(c) => {
                        let mut contents = json!([{
                            "uri": c.uri,
                        }]);
                        if let Some(mt) = &c.mime_type {
                            contents[0]["mimeType"] = json!(mt);
                        }
                        if let Some(text) = &c.text {
                            contents[0]["text"] = json!(text);
                        }
                        if let Some(blob) = &c.blob {
                            contents[0]["blob"] = json!(blob);
                        }
                        json!({
                            "jsonrpc": "2.0",
                            "id": id,
                            "result": {"contents": contents}
                        })
                    }
                    Err(e) => {
                        json!({
                            "jsonrpc": "2.0",
                            "id": id,
                            "error": {"code": -32602, "message": e}
                        })
                    }
                }
            }
            "prompts/list" => {
                let prompts: Vec<Value> = if let Some(provider) = &self.prompt_provider {
                    provider
                        .list()
                        .iter()
                        .map(|p| {
                            let args: Vec<Value> = p
                                .arguments
                                .iter()
                                .map(|a| {
                                    json!({
                                        "name": a.name,
                                        "description": a.description,
                                        "required": a.required
                                    })
                                })
                                .collect();
                            json!({
                                "name": p.name,
                                "description": p.description,
                                "arguments": args
                            })
                        })
                        .collect()
                } else {
                    self.prompts
                        .values()
                        .map(|p| {
                            let args: Vec<Value> = p
                                .arguments
                                .iter()
                                .map(|a| {
                                    json!({
                                        "name": a.name,
                                        "description": a.description,
                                        "required": a.required
                                    })
                                })
                                .collect();
                            json!({
                                "name": p.name,
                                "description": p.description,
                                "arguments": args
                            })
                        })
                        .collect()
                };
                json!({
                    "jsonrpc": "2.0",
                    "id": id,
                    "result": {"prompts": prompts}
                })
            }
            "prompts/get" => {
                let name = request
                    .get("params")
                    .and_then(|p| p.get("name"))
                    .and_then(Value::as_str)
                    .unwrap_or("");
                let args = request
                    .get("params")
                    .and_then(|p| p.get("arguments"))
                    .cloned()
                    .unwrap_or(Value::Null);

                let result = if let Some(provider) = &self.prompt_provider {
                    provider.get(name, args)
                } else if let Some(p) = self.prompts.get(name) {
                    Ok(PromptContent {
                        description: p.description.clone(),
                        messages: vec![PromptMessage {
                            role: "user".to_string(),
                            content: json!({"type": "text", "text": format!("Prompt: {}", p.name)}),
                        }],
                    })
                } else {
                    Err(format!("prompt not found: {name}"))
                };

                match result {
                    Ok(pc) => {
                        let messages: Vec<Value> = pc
                            .messages
                            .iter()
                            .map(|m| {
                                json!({
                                    "role": m.role,
                                    "content": m.content
                                })
                            })
                            .collect();
                        json!({
                            "jsonrpc": "2.0",
                            "id": id,
                            "result": {
                                "description": pc.description,
                                "messages": messages
                            }
                        })
                    }
                    Err(e) => {
                        json!({
                            "jsonrpc": "2.0",
                            "id": id,
                            "error": {"code": -32602, "message": e}
                        })
                    }
                }
            }
            _ => {
                json!({
                    "jsonrpc": "2.0",
                    "id": id,
                    "error": {"code": -32601, "message": format!("method not found: {method}")}
                })
            }
        }
    }

    /// Run the MCP server over STDIO (blocking async).
    /// Fails first if the configuration is invalid (see
    /// [`McpServerConfig::validate`]) so `auth_required` without a token is a
    /// startup error, not a silent per-request rejection loop.
    pub async fn run_stdio(&mut self) -> Result<(), String> {
        self.config.validate()?;
        self.running = true;
        let stdin = tokio::io::stdin();
        let mut stdout = tokio::io::stdout();
        let mut reader = BufReader::new(stdin);
        let mut line = String::new();

        loop {
            line.clear();
            match reader.read_line(&mut line).await {
                Ok(0) => break,
                Ok(_) => {
                    if let Some(response) = self.response_for_line(&line).await {
                        let mut out = response.to_string();
                        out.push('\n');
                        if let Err(e) = stdout.write_all(out.as_bytes()).await {
                            eprintln!("write error: {e}");
                            break;
                        }
                        let _ = stdout.flush().await;
                    }
                }
                Err(e) => {
                    eprintln!("read error: {e}");
                    break;
                }
            }
        }
        self.running = false;
        Ok(())
    }

    /// Dispatch one parsed JSON-RPC message.
    ///
    /// Per JSON-RPC 2.0 only messages carrying an `id` are requests that
    /// get a response; anything else is a notification (or a stray
    /// response) and is dropped without side effects — in particular a
    /// `tools/call` without an `id` must not execute.
    async fn handle_message(&self, request: &Value) -> Option<Value> {
        if request.get("id").is_none_or(Value::is_null) {
            return None;
        }
        Some(self.handle_request(request).await)
    }

    /// Turn one raw STDIO line into at most one JSON-RPC response.
    ///
    /// Lines over [`MAX_REQUEST_LINE_BYTES`] are rejected without parsing;
    /// blank lines and notifications produce no response. Kept separate
    /// from [`Self::run_stdio`] so the protocol decisions are testable
    /// without a real stdin/stdout pair.
    async fn response_for_line(&self, line: &str) -> Option<Value> {
        if line.len() > MAX_REQUEST_LINE_BYTES {
            return Some(json!({
                "jsonrpc": "2.0",
                "id": null,
                "error": {"code": -32600, "message": "request line exceeds 1 MiB limit"}
            }));
        }
        let trimmed = line.trim();
        if trimmed.is_empty() {
            return None;
        }
        let request: Value = match serde_json::from_str(trimmed) {
            Ok(v) => v,
            Err(_) => {
                return Some(json!({
                    "jsonrpc": "2.0",
                    "id": null,
                    "error": {"code": -32700, "message": "parse error"}
                }));
            }
        };
        self.handle_message(&request).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    struct CountingExecutor {
        calls: Arc<AtomicUsize>,
        sleep: Duration,
    }

    impl ToolExecutor for CountingExecutor {
        fn execute(&self, _name: &str, _arguments: Value) -> Result<Value, String> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            if !self.sleep.is_zero() {
                std::thread::sleep(self.sleep);
            }
            Ok(json!("done"))
        }
    }

    fn server_with_tool() -> (McpServer, Arc<AtomicUsize>) {
        let config = McpServerConfig {
            approved_tools: vec!["t".to_string()],
            ..McpServerConfig::default()
        };
        let mut server = McpServer::new(config);
        server.register_tool(ExposedTool {
            name: "t".to_string(),
            description: "d".to_string(),
            input_schema: json!({"type": "object"}),
        });
        let calls = Arc::new(AtomicUsize::new(0));
        server.set_tool_executor(Arc::new(CountingExecutor {
            calls: calls.clone(),
            sleep: Duration::ZERO,
        }));
        (server, calls)
    }

    fn tool_call(id: Value) -> Value {
        json!({
            "jsonrpc": "2.0",
            "id": id,
            "method": "tools/call",
            "params": {"name": "t", "arguments": {}}
        })
    }

    #[tokio::test]
    async fn id_less_tools_call_is_a_notification_and_never_executes() {
        let (server, calls) = server_with_tool();

        // No `id` member at all.
        let mut req = tool_call(Value::Null);
        req.as_object_mut().unwrap().remove("id");
        assert!(server.handle_message(&req).await.is_none());
        // Explicit `"id": null`.
        assert!(
            server
                .handle_message(&tool_call(Value::Null))
                .await
                .is_none()
        );
        assert_eq!(calls.load(Ordering::SeqCst), 0);

        // A real request still executes and responds.
        let resp = server
            .handle_message(&tool_call(json!(7)))
            .await
            .expect("request with id gets a response");
        assert_eq!(resp["id"], 7);
        assert_eq!(calls.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn slow_tool_times_out_without_stalling_the_loop() {
        let (mut server, calls) = server_with_tool();
        server.set_tool_executor(Arc::new(CountingExecutor {
            calls: calls.clone(),
            sleep: Duration::from_millis(500),
        }));
        server.set_tool_call_timeout(Duration::from_millis(50));

        let started = std::time::Instant::now();
        let resp = server.handle_request(&tool_call(json!(1))).await;
        assert!(
            started.elapsed() < Duration::from_millis(400),
            "timeout must return promptly, not wait for the tool"
        );
        assert_eq!(resp["error"]["code"], -32603);
        assert!(
            resp["error"]["message"]
                .as_str()
                .unwrap_or("")
                .contains("timed out")
        );
    }

    #[tokio::test]
    async fn fast_tool_still_returns_its_result() {
        let (server, calls) = server_with_tool();
        let resp = server.handle_request(&tool_call(json!(1))).await;
        assert_eq!(resp["result"]["content"][0]["text"], "done");
        assert_eq!(calls.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn oversized_lines_are_rejected_and_blank_lines_ignored() {
        let (server, _calls) = server_with_tool();
        let big = format!(
            "{{\"jsonrpc\":\"2.0\",\"id\":1,\"method\":\"tools/list\",\"pad\":\"{}\"}}",
            "x".repeat(MAX_REQUEST_LINE_BYTES)
        );
        let resp = server
            .response_for_line(&big)
            .await
            .expect("oversized line gets an error response");
        assert_eq!(resp["error"]["code"], -32600);

        assert!(server.response_for_line("   \n").await.is_none());
        let resp = server
            .response_for_line("not json")
            .await
            .expect("parse error still responds");
        assert_eq!(resp["error"]["code"], -32700);
    }
}
