//! The one MCP client: server discovery, persistent connections and tool invocation.
//!
//! Connected means a server answered `initialize`. Nothing here reports a server as connected
//! because it is configured, and one failing server never stops the others. The runtime owns
//! configuration; this crate owns the wire.

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::collections::{BTreeMap, HashMap};
use std::time::Duration;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::{Child, ChildStdin, ChildStdout};

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ServerSpec {
    pub name: String,
    pub command: String,
    #[serde(default)]
    pub args: Vec<String>,
    #[serde(default)]
    pub env: BTreeMap<String, String>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ServerState {
    pub name: String,
    pub command: String,
    pub connected: bool,
    pub tools: usize,
    pub resources: usize,
    pub prompts: usize,
    pub error: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ToolInfo {
    pub server: String,
    pub name: String,
    pub description: String,
    pub input_schema: Value,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ResourceInfo {
    pub server: String,
    pub uri: String,
    pub name: String,
    pub description: String,
    pub mime_type: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct PromptInfo {
    pub server: String,
    pub name: String,
    pub description: String,
}

#[derive(Default)]
pub struct McpRegistry {
    configured: Vec<ServerSpec>,
    states: Vec<ServerState>,
    connections: HashMap<String, Connection>,
}

struct Connection {
    child: Child,
    stdin: ChildStdin,
    lines: tokio::io::Lines<BufReader<ChildStdout>>,
    tools: Vec<ToolInfo>,
    resources: Vec<ResourceInfo>,
    prompts: Vec<PromptInfo>,
    next_id: u64,
}

impl McpRegistry {
    pub fn load(servers: Vec<ServerSpec>) -> Self {
        let states = servers
            .iter()
            .map(|server| ServerState {
                name: server.name.clone(),
                command: server.command.clone(),
                connected: false,
                tools: 0,
                resources: 0,
                prompts: 0,
                error: None,
            })
            .collect();
        Self {
            configured: servers,
            states,
            connections: HashMap::new(),
        }
    }

    pub fn configured_count(&self) -> usize {
        self.configured.len()
    }

    pub fn connected_count(&self) -> usize {
        self.states.iter().filter(|s| s.connected).count()
    }

    pub fn tools_count(&self) -> usize {
        self.states.iter().map(|s| s.tools).sum()
    }

    pub fn servers(&self) -> &[ServerState] {
        &self.states
    }

    pub fn tool_defs(&self) -> Vec<ToolInfo> {
        self.connections
            .values()
            .flat_map(|c| c.tools.iter().cloned())
            .collect()
    }

    pub fn server_tools(&self, name: &str) -> Option<Vec<ToolInfo>> {
        self.connections.get(name).map(|c| c.tools.clone())
    }

    pub fn resources_count(&self) -> usize {
        self.connections.values().map(|c| c.resources.len()).sum()
    }

    pub fn prompts_count(&self) -> usize {
        self.connections.values().map(|c| c.prompts.len()).sum()
    }

    pub fn all_resources(&self) -> Vec<ResourceInfo> {
        self.connections
            .values()
            .flat_map(|c| c.resources.iter().cloned())
            .collect()
    }

    pub fn all_prompts(&self) -> Vec<PromptInfo> {
        self.connections
            .values()
            .flat_map(|c| c.prompts.iter().cloned())
            .collect()
    }

    pub async fn read_resource(
        &mut self,
        server_name: &str,
        uri: &str,
    ) -> std::result::Result<String, String> {
        let conn = self
            .connections
            .get_mut(server_name)
            .ok_or_else(|| format!("server `{server_name}` not connected"))?;
        let id = conn.next_id;
        conn.next_id += 1;

        let request = json!({
            "jsonrpc": "2.0",
            "id": id,
            "method": "resources/read",
            "params": {"uri": uri}
        });
        write_line(&mut conn.stdin, &request).await?;

        for _ in 0..50 {
            let line = read_line(&mut conn.lines, "resources/read").await?;
            let value: Value = match serde_json::from_str(&line) {
                Ok(v) => v,
                Err(_) => continue,
            };
            if value.get("id").and_then(Value::as_u64) != Some(id) {
                continue;
            }
            if let Some(err) = value.get("error") {
                return Err(format!("resource read error: {err}"));
            }
            let result = value.get("result").cloned().unwrap_or(Value::Null);
            if let Some(contents) = result.get("contents").and_then(Value::as_array) {
                let mut out = String::new();
                for c in contents {
                    if let Some(text) = c.get("text").and_then(Value::as_str) {
                        if !out.is_empty() {
                            out.push('\n');
                        }
                        out.push_str(text);
                    }
                }
                if out.is_empty() {
                    out = result.to_string();
                }
                return Ok(out);
            }
            return Ok(result.to_string());
        }
        Err("no response to resources/read".into())
    }

    pub async fn get_prompt(
        &mut self,
        server_name: &str,
        prompt_name: &str,
        arguments: Value,
    ) -> std::result::Result<String, String> {
        let conn = self
            .connections
            .get_mut(server_name)
            .ok_or_else(|| format!("server `{server_name}` not connected"))?;
        let id = conn.next_id;
        conn.next_id += 1;

        let request = json!({
            "jsonrpc": "2.0",
            "id": id,
            "method": "prompts/get",
            "params": {
                "name": prompt_name,
                "arguments": arguments,
            }
        });
        write_line(&mut conn.stdin, &request).await?;

        for _ in 0..50 {
            let line = read_line(&mut conn.lines, "prompts/get").await?;
            let value: Value = match serde_json::from_str(&line) {
                Ok(v) => v,
                Err(_) => continue,
            };
            if value.get("id").and_then(Value::as_u64) != Some(id) {
                continue;
            }
            if let Some(err) = value.get("error") {
                return Err(format!("prompt get error: {err}"));
            }
            let result = value.get("result").cloned().unwrap_or(Value::Null);
            return Ok(result.to_string());
        }
        Err("no response to prompts/get".into())
    }

    pub fn take(&mut self) -> Self {
        Self {
            configured: std::mem::take(&mut self.configured),
            states: std::mem::take(&mut self.states),
            connections: std::mem::take(&mut self.connections),
        }
    }

    pub async fn connect_all(mut self) -> (Self, std::result::Result<(), String>) {
        let servers = self.configured.clone();
        let mut states = Vec::with_capacity(servers.len());
        let mut failures: Vec<String> = Vec::new();
        for server in &servers {
            match establish(server).await {
                Ok((child, stdin, lines, tools, resources, prompts)) => {
                    let count = tools.len();
                    let rcount = resources.len();
                    let pcount = prompts.len();
                    self.connections.insert(
                        server.name.clone(),
                        Connection {
                            child,
                            stdin,
                            lines,
                            tools,
                            resources,
                            prompts,
                            next_id: 10,
                        },
                    );
                    states.push(ServerState {
                        name: server.name.clone(),
                        command: server.command.clone(),
                        connected: true,
                        tools: count,
                        resources: rcount,
                        prompts: pcount,
                        error: None,
                    });
                }
                Err(err) => {
                    failures.push(format!("{}: {err}", server.name));
                    states.push(ServerState {
                        name: server.name.clone(),
                        command: server.command.clone(),
                        connected: false,
                        tools: 0,
                        resources: 0,
                        prompts: 0,
                        error: Some(err),
                    });
                }
            }
        }
        self.states = states;
        let result = if failures.is_empty() {
            Ok(())
        } else {
            Err(format!(
                "{} of {} configured MCP servers failed to connect: {}",
                failures.len(),
                servers.len(),
                failures.join("; ")
            ))
        };
        (self, result)
    }

    pub async fn connect(&mut self, server: &ServerSpec) -> std::result::Result<usize, String> {
        if self.connections.contains_key(&server.name) {
            let _ = self.disconnect(&server.name).await;
        }
        match establish(server).await {
            Ok((child, stdin, lines, tools, resources, prompts)) => {
                let count = tools.len();
                let rcount = resources.len();
                let pcount = prompts.len();
                self.connections.insert(
                    server.name.clone(),
                    Connection {
                        child,
                        stdin,
                        lines,
                        tools,
                        resources,
                        prompts,
                        next_id: 10,
                    },
                );
                if let Some(state) = self.states.iter_mut().find(|s| s.name == server.name) {
                    state.command = server.command.clone();
                    state.connected = true;
                    state.tools = count;
                    state.resources = rcount;
                    state.prompts = pcount;
                    state.error = None;
                } else {
                    self.states.push(ServerState {
                        name: server.name.clone(),
                        command: server.command.clone(),
                        connected: true,
                        tools: count,
                        resources: rcount,
                        prompts: pcount,
                        error: None,
                    });
                }
                if !self.configured.iter().any(|s| s.name == server.name) {
                    self.configured.push(server.clone());
                }
                Ok(count)
            }
            Err(err) => {
                if let Some(state) = self.states.iter_mut().find(|s| s.name == server.name) {
                    state.connected = false;
                    state.tools = 0;
                    state.error = Some(err.clone());
                } else {
                    self.states.push(ServerState {
                        name: server.name.clone(),
                        command: server.command.clone(),
                        connected: false,
                        tools: 0,
                        resources: 0,
                        prompts: 0,
                        error: Some(err.clone()),
                    });
                }
                Err(err)
            }
        }
    }

    pub async fn connect_by_name(&mut self, name: &str) -> std::result::Result<usize, String> {
        let server = self
            .configured
            .iter()
            .find(|s| s.name == name)
            .cloned()
            .ok_or_else(|| format!("server `{name}` not configured"))?;
        self.connect(&server).await
    }

    pub async fn disconnect(&mut self, name: &str) -> std::result::Result<(), String> {
        if let Some(mut conn) = self.connections.remove(name) {
            let _ = conn.child.kill().await;
        }
        if let Some(state) = self.states.iter_mut().find(|s| s.name == name) {
            state.connected = false;
            state.tools = 0;
            state.error = None;
        }
        Ok(())
    }

    pub async fn call_tool(
        &mut self,
        server_name: &str,
        tool_name: &str,
        arguments: Value,
    ) -> std::result::Result<String, String> {
        let conn = self
            .connections
            .get_mut(server_name)
            .ok_or_else(|| format!("server `{server_name}` not connected"))?;
        let id = conn.next_id;
        conn.next_id += 1;

        let request = json!({
            "jsonrpc": "2.0",
            "id": id,
            "method": "tools/call",
            "params": {
                "name": tool_name,
                "arguments": arguments,
            }
        });
        write_line(&mut conn.stdin, &request).await?;

        for _ in 0..50 {
            let line = read_line(&mut conn.lines, "tools/call").await?;
            let value: Value = match serde_json::from_str(&line) {
                Ok(v) => v,
                Err(_) => continue,
            };
            if value.get("id").and_then(Value::as_u64) != Some(id) {
                continue;
            }
            if let Some(err) = value.get("error") {
                return Err(format!("tool call error: {err}"));
            }
            let result = value.get("result").cloned().unwrap_or(Value::Null);
            if let Some(content) = result.get("content").and_then(Value::as_array) {
                let mut out = String::new();
                for c in content {
                    if let Some(text) = c.get("text").and_then(Value::as_str) {
                        if !out.is_empty() {
                            out.push('\n');
                        }
                        out.push_str(text);
                    }
                }
                if out.is_empty() {
                    out = result.to_string();
                }
                return Ok(out);
            }
            return Ok(result.to_string());
        }
        Err("no response to tools/call".into())
    }
}

async fn establish(
    server: &ServerSpec,
) -> std::result::Result<
    (
        Child,
        ChildStdin,
        tokio::io::Lines<BufReader<ChildStdout>>,
        Vec<ToolInfo>,
        Vec<ResourceInfo>,
        Vec<PromptInfo>,
    ),
    String,
> {
    let mut command = tokio::process::Command::new(&server.command);
    command
        .args(&server.args)
        .envs(&server.env)
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null())
        .kill_on_drop(true);

    let mut child = command
        .spawn()
        .map_err(|err| format!("spawn `{}`: {err}", server.command))?;

    let mut stdin = child.stdin.take().ok_or("no stdin")?;
    let stdout = child.stdout.take().ok_or("no stdout")?;
    let mut lines = BufReader::new(stdout).lines();

    let initialize = json!({
        "jsonrpc": "2.0",
        "id": 1,
        "method": "initialize",
        "params": {
            "protocolVersion": "2024-11-05",
            "capabilities": {},
            "clientInfo": {"name": "vikram", "version": env!("CARGO_PKG_VERSION")}
        }
    });

    write_line(&mut stdin, &initialize).await?;
    let init_line = read_line(&mut lines, "initialize").await?;
    let init_value: Value = serde_json::from_str(&init_line)
        .map_err(|err| format!("initialize response was not JSON: {err}"))?;
    if init_value.get("error").is_some() {
        let _ = child.kill().await;
        return Err(format!("initialize rejected: {}", init_value["error"]));
    }

    write_line(
        &mut stdin,
        &json!({"jsonrpc": "2.0", "method": "notifications/initialized"}),
    )
    .await?;
    write_line(
        &mut stdin,
        &json!({"jsonrpc": "2.0", "id": 2, "method": "tools/list"}),
    )
    .await?;

    let mut tools = Vec::new();
    for _ in 0..10 {
        let line = read_line(&mut lines, "tools/list").await?;
        let value: Value = match serde_json::from_str(&line) {
            Ok(value) => value,
            Err(_) => continue,
        };
        if value.get("id").and_then(Value::as_u64) == Some(2) {
            if let Some(arr) = value
                .get("result")
                .and_then(|r| r.get("tools"))
                .and_then(Value::as_array)
            {
                for t in arr {
                    let name = t
                        .get("name")
                        .and_then(Value::as_str)
                        .unwrap_or("")
                        .to_string();
                    if name.is_empty() {
                        continue;
                    }
                    let description = t
                        .get("description")
                        .and_then(Value::as_str)
                        .unwrap_or("")
                        .to_string();
                    let input_schema = t
                        .get("inputSchema")
                        .cloned()
                        .unwrap_or_else(|| json!({"type": "object"}));
                    tools.push(ToolInfo {
                        server: server.name.clone(),
                        name,
                        description,
                        input_schema,
                    });
                }
            }
            break;
        }
    }

    write_line(
        &mut stdin,
        &json!({"jsonrpc": "2.0", "id": 3, "method": "resources/list"}),
    )
    .await?;
    let mut resources = Vec::new();
    for _ in 0..10 {
        let line = read_line(&mut lines, "resources/list").await?;
        let value: Value = match serde_json::from_str(&line) {
            Ok(value) => value,
            Err(_) => continue,
        };
        if value.get("id").and_then(Value::as_u64) == Some(3) {
            if let Some(arr) = value
                .get("result")
                .and_then(|r| r.get("resources"))
                .and_then(Value::as_array)
            {
                for r in arr {
                    let uri = r
                        .get("uri")
                        .and_then(Value::as_str)
                        .unwrap_or("")
                        .to_string();
                    if uri.is_empty() {
                        continue;
                    }
                    let name = r
                        .get("name")
                        .and_then(Value::as_str)
                        .unwrap_or("")
                        .to_string();
                    let description = r
                        .get("description")
                        .and_then(Value::as_str)
                        .unwrap_or("")
                        .to_string();
                    let mime_type = r.get("mimeType").and_then(Value::as_str).map(String::from);
                    resources.push(ResourceInfo {
                        server: server.name.clone(),
                        uri,
                        name,
                        description,
                        mime_type,
                    });
                }
            }
            break;
        }
    }

    write_line(
        &mut stdin,
        &json!({"jsonrpc": "2.0", "id": 4, "method": "prompts/list"}),
    )
    .await?;
    let mut prompts = Vec::new();
    for _ in 0..10 {
        let line = read_line(&mut lines, "prompts/list").await?;
        let value: Value = match serde_json::from_str(&line) {
            Ok(value) => value,
            Err(_) => continue,
        };
        if value.get("id").and_then(Value::as_u64) == Some(4) {
            if let Some(arr) = value
                .get("result")
                .and_then(|r| r.get("prompts"))
                .and_then(Value::as_array)
            {
                for p in arr {
                    let name = p
                        .get("name")
                        .and_then(Value::as_str)
                        .unwrap_or("")
                        .to_string();
                    if name.is_empty() {
                        continue;
                    }
                    let description = p
                        .get("description")
                        .and_then(Value::as_str)
                        .unwrap_or("")
                        .to_string();
                    prompts.push(PromptInfo {
                        server: server.name.clone(),
                        name,
                        description,
                    });
                }
            }
            break;
        }
    }

    Ok((child, stdin, lines, tools, resources, prompts))
}

async fn write_line(stdin: &mut ChildStdin, value: &Value) -> std::result::Result<(), String> {
    let mut line = value.to_string();
    line.push('\n');
    stdin
        .write_all(line.as_bytes())
        .await
        .map_err(|err| format!("write failed: {err}"))?;
    stdin
        .flush()
        .await
        .map_err(|err| format!("flush failed: {err}"))
}

async fn read_line(
    lines: &mut tokio::io::Lines<BufReader<ChildStdout>>,
    what: &str,
) -> std::result::Result<String, String> {
    match tokio::time::timeout(Duration::from_secs(5), lines.next_line()).await {
        Ok(Ok(Some(line))) => Ok(line),
        Ok(Ok(None)) => Err(format!("server closed before {what}")),
        Ok(Err(err)) => Err(format!("{what} read failed: {err}")),
        Err(_) => Err(format!("{what} timed out")),
    }
}

/// Parse a `tools/list` response line. Exposed for tests.
pub fn parse_tools_response(line: &str) -> Option<usize> {
    let value: Value = serde_json::from_str(line).ok()?;
    value
        .get("result")
        .and_then(|result| result.get("tools"))
        .and_then(Value::as_array)
        .map(Vec::len)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn server(name: &str) -> ServerSpec {
        ServerSpec {
            name: name.into(),
            command: "mcp-server".into(),
            args: Vec::new(),
            env: Default::default(),
        }
    }

    #[test]
    fn configuring_servers_does_not_report_them_as_connected() {
        let registry = McpRegistry::load(vec![server("fs"), server("git")]);
        assert_eq!(registry.configured_count(), 2);
        assert_eq!(registry.connected_count(), 0);
        assert_eq!(registry.tools_count(), 0);
    }

    #[tokio::test]
    async fn connect_all_reports_servers_that_never_answered() {
        let registry = McpRegistry::load(vec![server("fs"), server("git")]);
        let (registry, result) = registry.connect_all().await;
        let err = result.expect_err("a server that never connected is not a success");
        assert!(err.contains("2 of 2"), "names how many failed: {err}");
        assert!(
            err.contains("fs") && err.contains("git"),
            "names which: {err}"
        );
        assert_eq!(registry.connected_count(), 0);
        assert!(
            registry
                .servers()
                .iter()
                .all(|state| !state.connected && state.error.is_some()),
            "every server carries its own failure"
        );
    }

    #[tokio::test]
    async fn connect_all_with_nothing_configured_is_a_clean_success() {
        let registry = McpRegistry::load(Vec::new());
        let (registry, result) = registry.connect_all().await;
        assert!(result.is_ok(), "no servers configured is not a failure");
        assert_eq!(registry.configured_count(), 0);
    }

    #[test]
    fn tools_list_response_counts_real_entries() {
        let line = r#"{"jsonrpc":"2.0","id":2,"result":{"tools":[{"name":"a"},{"name":"b"}]}}"#;
        assert_eq!(parse_tools_response(line), Some(2));
        assert_eq!(parse_tools_response("not json"), None);
        assert_eq!(parse_tools_response(r#"{"id":2}"#), None);
    }

    #[test]
    fn empty_registry_has_no_tool_defs() {
        let registry = McpRegistry::load(Vec::new());
        assert!(registry.tool_defs().is_empty());
        assert!(registry.server_tools("nope").is_none());
    }
}
