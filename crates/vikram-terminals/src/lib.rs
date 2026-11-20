//! Unified terminal execution backends: local, docker, ssh, and (later) cloud.
//!
//! EXPERIMENTAL — not wired into the runtime. `create` records the [`TerminalSpec`] (cwd, env)
//! and `exec` applies it where the backend allows; no process is kept alive between calls, so
//! `status` reports `Disconnected` rather than inventing a live session. `cols`/`rows` only take
//! effect on a real PTY, which these backends do not allocate. Do not rely on this crate until
//! it is connected to `terminal.backend` config and the permission layer.

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::Mutex;
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

/// A command line handed to the host shell: `cmd /C` on Windows, `$SHELL -c` elsewhere.
fn shell_command(command: &str) -> tokio::process::Command {
    #[cfg(windows)]
    {
        let comspec = std::env::var("COMSPEC")
            .ok()
            .map(|s| s.trim().to_owned())
            .filter(|s| !s.is_empty())
            .unwrap_or_else(|| "cmd".to_owned());
        let mut cmd = tokio::process::Command::new(comspec);
        cmd.args(["/C", command]);
        cmd
    }
    #[cfg(not(windows))]
    {
        let shell = std::env::var("SHELL")
            .ok()
            .map(|s| s.trim().to_owned())
            .filter(|s| !s.is_empty())
            .unwrap_or_else(|| "sh".to_owned());
        let mut cmd = tokio::process::Command::new(shell);
        cmd.args(["-c", command]);
        cmd
    }
}

/// Specs handed to `create`, kept so `exec` can honor cwd/env for the same terminal id.
#[derive(Default)]
struct SpecBook {
    specs: Mutex<BTreeMap<String, TerminalSpec>>,
}

impl SpecBook {
    fn insert(&self, spec: &TerminalSpec) {
        if let Ok(mut specs) = self.specs.lock() {
            specs.insert(spec.id.clone(), spec.clone());
        }
    }
    fn get(&self, id: &str) -> Option<TerminalSpec> {
        self.specs
            .lock()
            .ok()
            .and_then(|specs| specs.get(id).cloned())
    }
    fn contains(&self, id: &str) -> bool {
        self.specs.lock().is_ok_and(|specs| specs.contains_key(id))
    }
}

/// POSIX single-quote escaping for strings embedded in a remote shell command.
fn sh_quote(text: &str) -> String {
    format!("'{}'", text.replace('\'', "'\\''"))
}

pub struct LocalBackend {
    specs: SpecBook,
}

impl LocalBackend {
    pub fn new() -> Self {
        Self {
            specs: SpecBook::default(),
        }
    }
}

impl Default for LocalBackend {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl TerminalBackend for LocalBackend {
    fn name(&self) -> &'static str {
        "local"
    }
    async fn create(&self, spec: TerminalSpec) -> Result<String> {
        // Local terminals are tracked by the PTY subsystem in vikram-runtime; this backend
        // records metadata only and delegates execution to the host shell.
        self.specs.insert(&spec);
        Ok(spec.id)
    }
    async fn exec(&self, id: &str, command: &str, timeout_ms: u64) -> Result<ExecResult> {
        use tokio::time::{Duration, timeout};
        let mut cmd = shell_command(command);
        if let Some(spec) = self.specs.get(id) {
            if let Some(cwd) = &spec.cwd {
                cmd.current_dir(cwd);
            }
            cmd.envs(spec.env.iter());
        }
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
    async fn status(&self, id: &str) -> Result<TerminalStatus> {
        // Exec-per-call: nothing stays alive between calls, so a created id has no live
        // process to report and an unknown id was never created.
        if self.specs.contains(id) {
            Ok(TerminalStatus::Disconnected)
        } else {
            Err(TerminalError::NotFound(id.to_string()))
        }
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
    specs: SpecBook,
}

impl DockerBackend {
    pub fn new(image: impl Into<String>) -> Self {
        Self {
            image: image.into(),
            specs: SpecBook::default(),
        }
    }
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
        self.specs.insert(&spec);
        Ok(spec.id)
    }
    async fn exec(&self, id: &str, command: &str, timeout_ms: u64) -> Result<ExecResult> {
        use tokio::time::{Duration, timeout};
        let mut cmd = tokio::process::Command::new("docker");
        cmd.args(["run", "--rm"]);
        if let Some(spec) = self.specs.get(id) {
            if let Some(cwd) = &spec.cwd {
                cmd.arg("-w").arg(cwd.to_string_lossy().as_ref());
            }
            for (key, value) in &spec.env {
                cmd.arg("-e").arg(format!("{key}={value}"));
            }
        }
        cmd.args([&self.image, "sh", "-c", command]);
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
    async fn status(&self, id: &str) -> Result<TerminalStatus> {
        // `docker run --rm` exits with each exec; no container outlives a call.
        if self.specs.contains(id) {
            Ok(TerminalStatus::Exited(0))
        } else {
            Err(TerminalError::NotFound(id.to_string()))
        }
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
    specs: SpecBook,
}

impl SshBackend {
    pub fn new(host: impl Into<String>, user: Option<String>) -> Self {
        Self {
            host: host.into(),
            user,
            specs: SpecBook::default(),
        }
    }
}

#[async_trait]
impl TerminalBackend for SshBackend {
    fn name(&self) -> &'static str {
        "ssh"
    }
    async fn create(&self, spec: TerminalSpec) -> Result<String> {
        // Verify ssh is available; `-V` prints to stderr and exits 0 on success.
        let out = tokio::process::Command::new("ssh")
            .arg("-V")
            .output()
            .await
            .map_err(TerminalError::Io)?;
        if !out.status.success() {
            return Err(TerminalError::Unavailable("ssh not available".into()));
        }
        self.specs.insert(&spec);
        Ok(spec.id)
    }
    async fn exec(&self, id: &str, command: &str, timeout_ms: u64) -> Result<ExecResult> {
        use tokio::time::{Duration, timeout};
        let target = match &self.user {
            Some(u) => format!("{u}@{}", self.host),
            None => self.host.clone(),
        };
        let mut remote = String::new();
        if let Some(spec) = self.specs.get(id) {
            if let Some(cwd) = &spec.cwd {
                remote.push_str(&format!("cd {} && ", sh_quote(&cwd.to_string_lossy())));
            }
            if !spec.env.is_empty() {
                let assigns = spec
                    .env
                    .iter()
                    .map(|(k, v)| sh_quote(&format!("{k}={v}")))
                    .collect::<Vec<_>>()
                    .join(" ");
                remote.push_str(&format!("env {assigns} "));
            }
        }
        // The command keeps full shell syntax inside `sh -c` so `env` cannot swallow
        // leading assignments or builtins in the caller's command line.
        remote.push_str("sh -c ");
        remote.push_str(&sh_quote(command));
        let mut cmd = tokio::process::Command::new("ssh");
        cmd.args([&target, &remote]);
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
    async fn status(&self, id: &str) -> Result<TerminalStatus> {
        // One ssh invocation per exec; there is no persistent remote session to query.
        if self.specs.contains(id) {
            Ok(TerminalStatus::Disconnected)
        } else {
            Err(TerminalError::NotFound(id.to_string()))
        }
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
        r.register(Box::new(LocalBackend::new()));
        r.register(Box::new(DockerBackend::new("ubuntu:22.04")));
        r
    }
}

impl Default for TerminalRegistry {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[tokio::test]
    async fn local_exec() {
        let b = LocalBackend::new();
        let r = b.exec("x", "echo hi", 5000).await.unwrap();
        assert_eq!(r.exit_code, Some(0));
        assert!(r.stdout.contains("hi"));
    }

    fn spec(id: &str, cwd: Option<PathBuf>) -> TerminalSpec {
        let mut env = BTreeMap::new();
        env.insert("VIKRAM_TEST_VAR".to_string(), "set-by-spec".to_string());
        TerminalSpec {
            id: id.to_string(),
            backend: "local".into(),
            cwd,
            env,
            cols: 80,
            rows: 24,
        }
    }

    #[tokio::test]
    async fn local_exec_honors_spec_cwd_and_env() {
        let b = LocalBackend::new();
        let dir = std::env::temp_dir().join(format!("vikram-terminals-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let id = b.create(spec("t1", Some(dir.clone()))).await.unwrap();
        let r = b
            .exec(&id, "pwd && printf %s \"$VIKRAM_TEST_VAR\"", 5000)
            .await
            .unwrap();
        assert_eq!(r.exit_code, Some(0));
        let pwd = std::fs::canonicalize(&dir).unwrap();
        assert!(
            r.stdout.contains(&pwd.to_string_lossy().to_string())
                || r.stdout.contains(&dir.to_string_lossy().to_string()),
            "stdout was: {}",
            r.stdout
        );
        assert!(r.stdout.contains("set-by-spec"), "stdout was: {}", r.stdout);
        std::fs::remove_dir_all(&dir).ok();
    }

    #[tokio::test]
    async fn local_status_reflects_created_ids() {
        let b = LocalBackend::new();
        assert!(matches!(
            b.status("never-created").await,
            Err(TerminalError::NotFound(_))
        ));
        let id = b.create(spec("t2", None)).await.unwrap();
        assert_eq!(b.status(&id).await.unwrap(), TerminalStatus::Disconnected);
    }

    #[test]
    fn sh_quote_escapes_single_quotes() {
        assert_eq!(sh_quote("it's"), "'it'\\''s'");
        assert_eq!(sh_quote("plain"), "'plain'");
    }
    #[test]
    fn registry_defaults() {
        let r = TerminalRegistry::with_defaults();
        assert!(r.get("local").is_some());
        assert!(r.get("docker").is_some());
    }
}
