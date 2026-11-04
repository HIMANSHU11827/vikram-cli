//! Unified terminal execution backends: local, docker, ssh, and (later) cloud.
//!
//! Every backend implements `TerminalBackend`. The runtime selects one via config
//! (`terminal.backend`) and never bypasses the permission/sandbox layer.

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::path::PathBuf;
use thiserror::Error;

#[derive(Debug, Error)]
pub enum TerminalError {
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
    #[error("backend unavailable: {0}")]
    Unavailable(String),
    #[error("not found: {0}")]
    NotFound(String),
    #[error("protocol: {0}")]
    Protocol(String),
    #[error("{0}")]
    Other(String),
}

pub type Result<T> = std::result::Result<T, TerminalError>;

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct TerminalSpec {
    pub id: String,
    pub backend: String,
    pub cwd: Option<PathBuf>,
    pub env: BTreeMap<String, String>,
    pub cols: u16,
    pub rows: u16,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ExecResult {
    pub exit_code: Option<i32>,
    pub stdout: String,
    pub stderr: String,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub enum TerminalStatus {
    Running,
    Exited(i32),
    Disconnected,
    Closed,
}

#[async_trait]
pub trait TerminalBackend: Send + Sync {
    fn name(&self) -> &'static str;
    async fn create(&self, spec: TerminalSpec) -> Result<String>;
    async fn exec(&self, id: &str, command: &str, timeout_ms: u64) -> Result<ExecResult>;
    async fn write_stdin(&self, id: &str, data: &[u8]) -> Result<()>;
    async fn read_output(&self, id: &str) -> Result<String>;
    async fn resize(&self, id: &str, cols: u16, rows: u16) -> Result<()>;
    async fn interrupt(&self, id: &str) -> Result<()>;
    async fn status(&self, id: &str) -> Result<TerminalStatus>;
    async fn close(&self, id: &str) -> Result<()>;
    /// Reconnect to a still-live terminal after a client disconnect. Returns false if the
    /// process has already exited (distinguish reconnect from recovery).
    async fn reconnect(&self, id: &str) -> Result<bool>;
}

// ---------- Local backend ----------

pub struct LocalBackend;

#[async_trait]
impl TerminalBackend for LocalBackend {
    fn name(&self) -> &'static str {
        "local"
    }
    async fn create(&self, spec: TerminalSpec) -> Result<String> {
        // Local terminals are tracked by the PTY subsystem in vikram-runtime; this backend
        // records metadata only and delegates execution to the host shell.
        Ok(spec.id)
    }
    async fn exec(&self, _id: &str, command: &str, timeout_ms: u64) -> Result<ExecResult> {
        use tokio::process::Command as TCommand;
        use tokio::time::{Duration, timeout};
        let mut cmd = TCommand::new("sh");
        cmd.arg("-c").arg(command);
        let fut = cmd.output();
        match timeout(Duration::from_millis(timeout_ms.max(1)), fut).await {
            Ok(Ok(out)) => Ok(ExecResult {
                exit_code: out.status.code(),
                stdout: String::from_utf8_lossy(&out.stdout).to_string(),
                stderr: String::from_utf8_lossy(&out.stderr).to_string(),
            }),
            Ok(Err(e)) => Err(TerminalError::Io(e)),
            Err(_) => Err(TerminalError::Other("timeout".into())),
        }
    }
    async fn write_stdin(&self, _id: &str, _data: &[u8]) -> Result<()> {
        Err(TerminalError::Other(
            "write_stdin not supported by exec backend; use PTY".into(),
        ))
    }
    async fn read_output(&self, _id: &str) -> Result<String> {
        Ok(String::new())
    }
    async fn resize(&self, _id: &str, _cols: u16, _rows: u16) -> Result<()> {
        Ok(())
    }
    async fn interrupt(&self, _id: &str) -> Result<()> {
        Ok(())
    }
    async fn status(&self, _id: &str) -> Result<TerminalStatus> {
        Ok(TerminalStatus::Running)
    }
    async fn close(&self, _id: &str) -> Result<()> {
        Ok(())
    }
    async fn reconnect(&self, _id: &str) -> Result<bool> {
        Ok(false)
    }
}

// ---------- Docker backend ----------

pub struct DockerBackend {
    pub image: String,
}

#[async_trait]
impl TerminalBackend for DockerBackend {
    fn name(&self) -> &'static str {
        "docker"
    }
    async fn create(&self, spec: TerminalSpec) -> Result<String> {
        // Verify docker is available.
        let out = tokio::process::Command::new("docker")
            .args(["--version"])
            .output()
            .await
            .map_err(TerminalError::Io)?;
        if !out.status.success() {
            return Err(TerminalError::Unavailable("docker not available".into()));
        }
        Ok(spec.id)
    }
    async fn exec(&self, _id: &str, command: &str, timeout_ms: u64) -> Result<ExecResult> {
        use tokio::time::{Duration, timeout};
        let mut cmd = tokio::process::Command::new("docker");
        cmd.args(["run", "--rm", &self.image, "sh", "-c", command]);
        let fut = cmd.output();
        match timeout(Duration::from_millis(timeout_ms.max(1)), fut).await {
            Ok(Ok(out)) => Ok(ExecResult {
                exit_code: out.status.code(),
                stdout: String::from_utf8_lossy(&out.stdout).to_string(),
                stderr: String::from_utf8_lossy(&out.stderr).to_string(),
            }),
            Ok(Err(e)) => Err(TerminalError::Io(e)),
            Err(_) => Err(TerminalError::Other("timeout".into())),
        }
    }
    async fn write_stdin(&self, _id: &str, _data: &[u8]) -> Result<()> {
        Err(TerminalError::Other("not supported".into()))
    }
    async fn read_output(&self, _id: &str) -> Result<String> {
        Ok(String::new())
    }
    async fn resize(&self, _id: &str, _cols: u16, _rows: u16) -> Result<()> {
        Ok(())
    }
    async fn interrupt(&self, _id: &str) -> Result<()> {
        Ok(())
    }
    async fn status(&self, _id: &str) -> Result<TerminalStatus> {
        Ok(TerminalStatus::Exited(0))
    }
    async fn close(&self, _id: &str) -> Result<()> {
        Ok(())
    }
    async fn reconnect(&self, _id: &str) -> Result<bool> {
        Ok(false)
    }
}

// ---------- SSH backend ----------

pub struct SshBackend {
    pub host: String,
    pub user: Option<String>,
}

#[async_trait]
impl TerminalBackend for SshBackend {
    fn name(&self) -> &'static str {
        "ssh"
    }
    async fn create(&self, spec: TerminalSpec) -> Result<String> {
        // Verify ssh is available.
        let out = tokio::process::Command::new("ssh")
            .arg("-V")
            .output()
            .await
            .map_err(TerminalError::Io)?;
        // ssh -V writes to stderr and exits 0 or 255 depending; just check binary exists.
        if out.status.code().is_none() {
            return Err(TerminalError::Unavailable("ssh not available".into()));
        }
        Ok(spec.id)
    }
    async fn exec(&self, _id: &str, command: &str, timeout_ms: u64) -> Result<ExecResult> {
        use tokio::time::{Duration, timeout};
        let target = match &self.user {
            Some(u) => format!("{u}@{}", self.host),
            None => self.host.clone(),
        };
        let mut cmd = tokio::process::Command::new("ssh");
        cmd.args([&target, command]);
        let fut = cmd.output();
        match timeout(Duration::from_millis(timeout_ms.max(1)), fut).await {
            Ok(Ok(out)) => Ok(ExecResult {
                exit_code: out.status.code(),
                stdout: String::from_utf8_lossy(&out.stdout).to_string(),
                stderr: String::from_utf8_lossy(&out.stderr).to_string(),
            }),
            Ok(Err(e)) => Err(TerminalError::Io(e)),
            Err(_) => Err(TerminalError::Other("timeout".into())),
        }
    }
    async fn write_stdin(&self, _id: &str, _data: &[u8]) -> Result<()> {
        Err(TerminalError::Other("not supported".into()))
    }
    async fn read_output(&self, _id: &str) -> Result<String> {
        Ok(String::new())
    }
    async fn resize(&self, _id: &str, _cols: u16, _rows: u16) -> Result<()> {
        Ok(())
    }
    async fn interrupt(&self, _id: &str) -> Result<()> {
        Ok(())
    }
    async fn status(&self, _id: &str) -> Result<TerminalStatus> {
        Ok(TerminalStatus::Running)
    }
    async fn close(&self, _id: &str) -> Result<()> {
        Ok(())
    }
    async fn reconnect(&self, _id: &str) -> Result<bool> {
        Ok(false)
    }
}

// ---------- Registry ----------

pub struct TerminalRegistry {
    backends: BTreeMap<String, Box<dyn TerminalBackend>>,
}

impl TerminalRegistry {
    pub fn new() -> Self {
        Self {
            backends: BTreeMap::new(),
        }
    }
    pub fn register(&mut self, backend: Box<dyn TerminalBackend>) {
        self.backends.insert(backend.name().to_string(), backend);
    }
    pub fn get(&self, name: &str) -> Option<&dyn TerminalBackend> {
        self.backends.get(name).map(|b| b.as_ref())
    }
    pub fn names(&self) -> Vec<String> {
        self.backends.keys().cloned().collect()
    }
    pub fn with_defaults() -> Self {
        let mut r = Self::new();
        r.register(Box::new(LocalBackend));
        r.register(Box::new(DockerBackend {
            image: "ubuntu:22.04".into(),
        }));
        r
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[tokio::test]
    async fn local_exec() {
        let b = LocalBackend;
        let r = b.exec("x", "echo hi", 5000).await.unwrap();
        assert_eq!(r.exit_code, Some(0));
        assert!(r.stdout.contains("hi"));
    }
    #[test]
    fn registry_defaults() {
        let r = TerminalRegistry::with_defaults();
        assert!(r.get("local").is_some());
        assert!(r.get("docker").is_some());
    }
}
