//! MCP Server — exposes approved VIKRAM capabilities via JSON-RPC 2.0 over STDIO.
//!
//! Supports: tools (list/call), resources (list/read), prompts (list/get).
//! Tool execution is delegated via a configurable `ToolExecutor`.

use serde_json::{Value, json};
use std::collections::HashMap;
use std::sync::Arc;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

/// Configuration for the MCP server.
#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
pub struct McpServerConfig {
    pub name: String,
    pub version: String,
    pub approved_tools: Vec<String>,
    pub auth_required: bool,
}

impl Default for McpServerConfig {
    fn default() -> Self {
        Self {
            name: "vikram".to_string(),
            version: env!("CARGO_PKG_VERSION").to_string(),
            approved_tools: Vec::new(),
            auth_required: false,
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
            running: false,
        }
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

    pub fn approved_tools(&self) -> &[String] {
        &self.config.approved_tools
    }

    pub fn is_running(&self) -> bool {
        self.running
    }

    /// Handle a single JSON-RPC request and return the response.
    pub fn handle_request(&self, request: &Value) -> Value {
        let id = request.get("id").cloned().unwrap_or(Value::Null);
        let method = request.get("method").and_then(Value::as_str).unwrap_or("");

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

                // Check approved_tools filter
                if !self.config.approved_tools.is_empty()
                    && !self.config.approved_tools.contains(&name.to_string())
                {
                    return json!({
                        "jsonrpc": "2.0",
                        "id": id,
                        "error": {"code": -32603, "message": format!("tool not approved: {name}")}
                    });
                }

                // Delegate to executor if available
                if let Some(executor) = &self.tool_executor {
                    match executor.execute(name, args) {
                        Ok(result) => {
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
                        Err(e) => {
                            json!({
                                "jsonrpc": "2.0",
                                "id": id,
                                "error": {"code": -32603, "message": e}
                            })
                        }
                    }
                } else {
                    json!({
                        "jsonrpc": "2.0",
                        "id": id,
                        "result": {
                            "content": [{"type": "text", "text": "tool invoked (no executor configured)"}]
                        }
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
    pub async fn run_stdio(&mut self) -> Result<(), String> {
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
                    let trimmed = line.trim();
                    if trimmed.is_empty() {
                        continue;
                    }
                    let request: Value = match serde_json::from_str(trimmed) {
                        Ok(v) => v,
                        Err(_) => {
                            let resp = json!({"jsonrpc": "2.0", "id": null, "error": {"code": -32700, "message": "parse error"}});
                            let mut out = resp.to_string();
                            out.push('\n');
                            let _ = stdout.write_all(out.as_bytes()).await;
                            let _ = stdout.flush().await;
                            continue;
                        }
                    };
                    let response = self.handle_request(&request);
                    if request.get("id").is_none() || request.get("id") == Some(&Value::Null) {
                        // notification — no response
                        if request
                            .get("method")
                            .and_then(Value::as_str)
                            .map(|m| m.starts_with("notification"))
                            .unwrap_or(false)
                        {
                            continue;
                        }
                    }
                    let mut out = response.to_string();
                    out.push('\n');
                    if let Err(e) = stdout.write_all(out.as_bytes()).await {
                        eprintln!("write error: {e}");
                        break;
                    }
                    let _ = stdout.flush().await;
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
}
