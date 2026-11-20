//! Agent Client Protocol (ACP) — client and server over JSON-RPC 2.0 on stdio.
//!
//! EXPERIMENTAL — no crate in the workspace depends on this yet. Known gaps before use:
//! `new_session` cannot pass `mcpServers`, and agent→client requests are answered with
//! `-32601` rather than actually handled.
//!
//! ACP connects a *client* (IDE/host) to an *agent* (Vikram or another compatible agent).
//! It is distinct from MCP (agent↔tools) and A2A (agent↔agent).

use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::io::{BufRead, BufReader, Write};
use std::process::{Child, Command, Stdio};
use std::sync::mpsc::{Receiver, RecvTimeoutError};
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

// ---------- ACP Client ----------

pub struct AcpClient {
    child: Option<Child>,
    stdin: Option<std::process::ChildStdin>,
    rx: Option<Receiver<String>>,
    request_timeout: Duration,
    next_id: i64,
}

impl AcpClient {
    pub fn new() -> Self {
        Self {
            child: None,
            stdin: None,
            rx: None,
            request_timeout: Duration::from_secs(300),
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
        })
    }

    pub fn set_request_timeout(&mut self, timeout: Duration) {
        self.request_timeout = timeout;
    }

    fn alloc_id(&mut self) -> i64 {
        let id = self.next_id;
        self.next_id += 1;
        id
    }

    /// Handle one inbound line from the agent. Returns `Some` when it is the
    /// response to `our_id`; agent→client requests get a `-32601` reply.
    fn handle_inbound<W: Write>(
        out: &mut W,
        line: &str,
        our_id: i64,
    ) -> Result<Option<JsonRpcResponse>> {
        let value: Value = match serde_json::from_str(line) {
            Ok(v) => v,
            Err(_) => return Ok(None),
        };
        if value.get("method").is_some() {
            // Agent→client request or notification, not a response to us.
            if let Ok(req) = serde_json::from_value::<JsonRpcRequest>(value.clone()) {
                let resp = JsonRpcResponse {
                    jsonrpc: "2.0".into(),
                    id: req.id,
                    result: None,
                    error: Some(JsonRpcError {
                        code: -32601,
                        message: "Method not found".into(),
                        data: None,
                    }),
                };
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
            if let Some(resp) = Self::handle_inbound(stdin, &line, id)? {
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

    #[test]
    fn handle_inbound_matches_response_by_id() {
        let mut out = Vec::new();
        let resp = AcpClient::handle_inbound(
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
        let resp =
            AcpClient::handle_inbound(&mut out, r#"{"jsonrpc":"2.0","id":9,"result":{}}"#, 7)
                .unwrap();
        assert!(resp.is_none());
        assert!(out.is_empty());
    }

    #[test]
    fn handle_inbound_answers_agent_request() {
        let mut out = Vec::new();
        let resp = AcpClient::handle_inbound(
            &mut out,
            r#"{"jsonrpc":"2.0","id":"agent-1","method":"session/request_permission","params":{}}"#,
            1,
        )
        .unwrap();
        assert!(resp.is_none());
        let written: Value = serde_json::from_slice(&out).unwrap();
        assert_eq!(written["id"], serde_json::json!("agent-1"));
        assert_eq!(written["error"]["code"], serde_json::json!(-32601));
    }

    #[test]
    fn handle_inbound_ignores_agent_notification() {
        let mut out = Vec::new();
        let resp = AcpClient::handle_inbound(
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
             printf '%s\\n' '{{\"jsonrpc\":\"2.0\",\"id\":\"agent-1\",\"method\":\"fs/read_text_file\",\"params\":{{}}}}'\n\
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
