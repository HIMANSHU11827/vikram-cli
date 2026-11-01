//! Private MCP Runtime — manage third-party MCP server processes.

use crate::{McpError, Result};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::PathBuf;
use tokio::process::{Child, Command};

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
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .kill_on_drop(true);

        match cmd.spawn() {
            Ok(child) => {
                self.pid = child.id();
                self.started_at = Some(
                    std::time::SystemTime::now()
                        .duration_since(std::time::UNIX_EPOCH)
                        .unwrap_or_default()
                        .as_secs(),
                );
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

    pub async fn stop(&mut self) -> Result<()> {
        if let Some(ref mut child) = self.child {
            let _ = child.kill().await;
        }
        self.child = None;
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
