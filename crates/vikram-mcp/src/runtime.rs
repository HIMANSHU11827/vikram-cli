//! Private MCP Runtime — manage third-party MCP server processes.

use crate::{McpError, Result};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::HashMap;
use std::path::PathBuf;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::{Child, ChildStdin, ChildStdout, Command};

/// Status of a managed server.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum ServerStatus {
    Stopped,
    Starting,
    Running,
    Stopping,
    Failed(String),
}

/// A managed third-party MCP server instance.
pub struct ServerInstance {
    pub name: String,
    pub command: String,
    pub args: Vec<String>,
    pub env: HashMap<String, String>,
    pub status: ServerStatus,
    pub pid: Option<u32>,
    pub started_at: Option<u64>,
    child: Option<Child>,
    stdin: Option<ChildStdin>,
    stdout: Option<BufReader<ChildStdout>>,
}

impl ServerInstance {
    pub fn new(
        name: String,
        command: String,
        args: Vec<String>,
        env: HashMap<String, String>,
    ) -> Self {
        Self {
            name,
            command,
            args,
            env,
            status: ServerStatus::Stopped,
            pid: None,
            started_at: None,
            child: None,
            stdin: None,
            stdout: None,
        }
    }

    pub async fn start(&mut self) -> Result<()> {
        if self.status == ServerStatus::Running {
            return Err(McpError::AlreadyRunning {
                name: self.name.clone(),
            });
        }
        self.status = ServerStatus::Starting;
        let mut cmd = Command::new(&self.command);
        cmd.args(&self.args)
            .envs(&self.env)
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::inherit())
            .kill_on_drop(true);

        match cmd.spawn() {
            Ok(mut child) => {
                self.pid = child.id();
                self.started_at = Some(
                    std::time::SystemTime::now()
                        .duration_since(std::time::UNIX_EPOCH)
                        .unwrap_or_default()
                        .as_secs(),
                );
                self.stdin = child.stdin.take();
                self.stdout = child.stdout.take().map(BufReader::new);
                self.child = Some(child);
                self.status = ServerStatus::Running;
                Ok(())
            }
            Err(e) => {
                self.status = ServerStatus::Failed(e.to_string());
                Err(McpError::Spawn(e.to_string()))
            }
        }
    }

    /// Send one newline-delimited JSON-RPC message over the child's stdin.
    pub async fn send(&mut self, message: &Value) -> Result<()> {
        let stdin = self
            .stdin
            .as_mut()
            .ok_or_else(|| McpError::Spawn(format!("{} is not running", self.name)))?;
        let mut out = serde_json::to_string(message).map_err(|e| McpError::Spawn(e.to_string()))?;
        out.push('\n');
        stdin
            .write_all(out.as_bytes())
            .await
            .map_err(|e| McpError::Spawn(e.to_string()))?;
        stdin
            .flush()
            .await
            .map_err(|e| McpError::Spawn(e.to_string()))
    }

    /// Read the next newline-delimited JSON-RPC message from the child's stdout.
    pub async fn read_message(&mut self) -> Result<Value> {
        let stdout = self
            .stdout
            .as_mut()
            .ok_or_else(|| McpError::Spawn(format!("{} is not running", self.name)))?;
        let mut line = String::new();
        stdout
            .read_line(&mut line)
            .await
            .map_err(|e| McpError::Spawn(e.to_string()))?;
        if line.is_empty() {
            self.status = ServerStatus::Stopped;
            return Err(McpError::Spawn(format!("{} closed its output", self.name)));
        }
        serde_json::from_str(line.trim())
            .map_err(|e| McpError::Spawn(format!("{} sent invalid JSON: {e}", self.name)))
    }

    /// One request/response exchange over the pipes.
    pub async fn send_and_receive(&mut self, request: Value, id: &Value) -> Result<Value> {
        self.send(&request).await?;
        loop {
            let msg = self.read_message().await?;
            if msg.get("id") == Some(id) {
                return Ok(msg);
            }
            // notifications and interleaved messages are skipped; responses match by id
        }
    }

    /// Cheap liveness probe: did the process exit without us noticing?
    pub async fn health_check(&mut self) -> Result<bool> {
        let alive = match self.child.as_mut() {
            Some(child) => child
                .try_wait()
                .map_err(|e| McpError::Io(e.to_string()))?
                .is_none(),
            None => false,
        };
        if !alive && self.status == ServerStatus::Running {
            self.status = ServerStatus::Stopped;
            self.stdin = None;
            self.stdout = None;
        }
        Ok(alive)
    }

    pub async fn stop(&mut self) -> Result<()> {
        if let Some(ref mut child) = self.child {
            let _ = child.kill().await;
        }
        self.child = None;
        self.stdin = None;
        self.stdout = None;
        self.pid = None;
        self.status = ServerStatus::Stopped;
        Ok(())
    }

    pub async fn restart(&mut self) -> Result<()> {
        self.stop().await?;
        self.start().await
    }
}

/// Private MCP Runtime Manager.
pub struct PrivateRuntime {
    servers: HashMap<String, ServerInstance>,
    #[allow(dead_code)]
    base_dir: PathBuf,
}

impl PrivateRuntime {
    pub fn new(base_dir: PathBuf) -> Self {
        Self {
            servers: HashMap::new(),
            base_dir,
        }
    }

    pub fn register(&mut self, instance: ServerInstance) {
        self.servers.insert(instance.name.clone(), instance);
    }

    pub fn get(&self, name: &str) -> Option<&ServerInstance> {
        self.servers.get(name)
    }

    pub fn get_mut(&mut self, name: &str) -> Option<&mut ServerInstance> {
        self.servers.get_mut(name)
    }

    pub fn list(&self) -> Vec<&ServerInstance> {
        self.servers.values().collect()
    }

    pub fn remove(&mut self, name: &str) -> Result<()> {
        if let Some(mut inst) = self.servers.remove(name)
            && inst.status == ServerStatus::Running
        {
            futures_stop(&mut inst);
        }
        Ok(())
    }

    pub fn is_running(&self, name: &str) -> bool {
        self.servers
            .get(name)
            .map(|s| s.status == ServerStatus::Running)
            .unwrap_or(false)
    }
}

fn futures_stop(inst: &mut ServerInstance) {
    // Synchronous stop for remove() — best effort
    if let Some(ref mut child) = inst.child {
        let _ = child.start_kill();
    }
    inst.child = None;
    inst.stdin = None;
    inst.stdout = None;
    inst.pid = None;
    inst.status = ServerStatus::Stopped;
}

/// Config for a managed server (persisted to disk).
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ManagedServerConfig {
    pub name: String,
    pub command: String,
    pub args: Vec<String>,
    pub env: HashMap<String, String>,
    pub auto_start: bool,
}
