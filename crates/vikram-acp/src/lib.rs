//! Agent Client Protocol (ACP) — client and server over JSON-RPC 2.0 on stdio.
//!
//! ACP connects a *client* (IDE/host) to an *agent* (Vikram or another compatible agent).
//! It is distinct from MCP (agent↔tools) and A2A (agent↔agent).

use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::io::{BufRead, BufReader, Write};
use std::process::{Child, Command, Stdio};
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

// ---------- ACP Client ----------

pub struct AcpClient {
    child: Option<Child>,
    stdin: Option<std::process::ChildStdin>,
    stdout: Option<BufReader<std::process::ChildStdout>>,
    next_id: i64,
}

impl AcpClient {
    pub fn new() -> Self {
        Self {
            child: None,
            stdin: None,
            stdout: None,
            next_id: 1,
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
        let stdout = child.stdout.take().map(BufReader::new);
        Ok(Self {
            child: Some(child),
            stdin,
            stdout,
            next_id: 1,
        })
    }

    fn alloc_id(&mut self) -> i64 {
        let id = self.next_id;
        self.next_id += 1;
        id
    }

    pub fn send_request(&mut self, method: &str, params: Option<Value>) -> Result<JsonRpcResponse> {
        let id = self.alloc_id();
        let req = JsonRpcRequest::new(id, method, params);
        let line = serde_json::to_string(&req)?;
        let stdin = self.stdin.as_mut().ok_or(AcpError::NotConnected)?;
        writeln!(stdin, "{line}")?;
        stdin.flush()?;

        // Read responses until we get one matching our id.
        let reader = self.stdout.as_mut().ok_or(AcpError::NotConnected)?;
        loop {
            let mut buf = String::new();
            let n = reader.read_line(&mut buf)?;
            if n == 0 {
                return Err(AcpError::Protocol("eof".into()));
            }
            let trimmed = buf.trim();
            if trimmed.is_empty() {
                continue;
            }
            // Try response first.
            if let Ok(resp) = serde_json::from_str::<JsonRpcResponse>(trimmed) {
                if resp.id == Value::Number(id.into()) {
                    return Ok(resp);
                }
                // notification or other; continue
                continue;
            }
            // notification
            continue;
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
        self.stdout = None;
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
            // Parse as request.
            let req: JsonRpcRequest = match serde_json::from_str(trimmed) {
                Ok(r) => r,
                Err(_) => {
                    // Maybe a notification; try.
                    if let Ok(n) = serde_json::from_str::<JsonRpcNotification>(trimmed) {
                        // notifications have no response
                        let _ = n;
                        continue;
                    }
                    continue;
                }
            };
            let (result, error) = handler(&req.method, req.params.clone());
            let resp = JsonRpcResponse {
                jsonrpc: "2.0".into(),
                id: req.id.clone(),
                result,
                error,
            };
            let out = serde_json::to_string(&resp)?;
            writeln!(stdout, "{out}")?;
            stdout.flush()?;
        }
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
}
