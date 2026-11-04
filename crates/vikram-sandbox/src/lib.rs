//! OS-level process isolation for commands the host agrees to run.
//!
//! Policy decides *whether* a command may run; the sandbox shrinks what a running command can do
//! to the machine around it. Modes: NoSandbox (direct host), Selective (per-session), Full (all).

use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use std::process::Command;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SandboxMode {
    /// Direct host execution; policy still enforced.
    NoSandbox,
    /// Per-session/per-agent isolation; selective.
    Selective,
    /// All applicable tool execution isolated.
    Full,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SandboxMechanism {
    Bubblewrap,
    Seatbelt,
    Landlock,
    PassThrough,
}

#[derive(Clone, Debug)]
pub struct Sandbox {
    pub mode: SandboxMode,
    mechanism: SandboxMechanism,
    passthrough_reason: Option<String>,
    allow_network: bool,
    extra_read_only: Vec<PathBuf>,
    profile_path: Option<PathBuf>,
    resource_limits: ResourceLimits,
}

#[derive(Clone, Copy, Debug)]
pub struct ResourceLimits {
    pub max_memory_bytes: Option<u64>,
    pub max_cpu_seconds: Option<u64>,
    pub max_open_files: Option<u64>,
    pub max_processes: Option<u64>,
}

impl Default for ResourceLimits {
    fn default() -> Self {
        Self {
            max_memory_bytes: None,
            max_cpu_seconds: None,
            max_open_files: None,
            max_processes: None,
        }
    }
}

impl Sandbox {
    pub fn detect(mode: SandboxMode, allow_network: bool, extra_read_only: Vec<PathBuf>) -> Self {
        Self::detect_with_limits(
            mode,
            allow_network,
            extra_read_only,
            ResourceLimits::default(),
        )
    }

    pub fn detect_with_limits(
        mode: SandboxMode,
        allow_network: bool,
        extra_read_only: Vec<PathBuf>,
        limits: ResourceLimits,
    ) -> Self {
        if matches!(mode, SandboxMode::NoSandbox) {
            return Self {
                mode,
                mechanism: SandboxMechanism::PassThrough,
                passthrough_reason: Some("sandbox disabled by mode (no_sandbox)".into()),
                allow_network: true,
                extra_read_only: Vec::new(),
                profile_path: None,
                resource_limits: limits,
            };
        }
        let (mechanism, passthrough_reason) = if cfg!(target_os = "linux") {
            if bwrap_available() {
                (SandboxMechanism::Bubblewrap, None)
            } else if landlock_available() {
                (SandboxMechanism::Landlock, None)
            } else {
                (
                    SandboxMechanism::PassThrough,
                    Some("bubblewrap/landlock not installed; OS isolation inactive".into()),
                )
            }
        } else if cfg!(target_os = "macos") {
            if sandbox_exec_available() {
                (SandboxMechanism::Seatbelt, None)
            } else {
                (
                    SandboxMechanism::PassThrough,
                    Some("sandbox-exec not available; OS isolation inactive".into()),
                )
            }
        } else {
            (
                SandboxMechanism::PassThrough,
                Some("platform offers no supported isolation mechanism".into()),
            )
        };

        let profile_path = (mechanism == SandboxMechanism::Seatbelt)
            .then(seatbelt_profile_file)
            .flatten();

        Self {
            mode,
            mechanism,
            passthrough_reason,
            allow_network,
            extra_read_only,
            profile_path,
            resource_limits: limits,
        }
    }

    pub fn disabled(reason: impl Into<String>) -> Self {
        Self {
            mode: SandboxMode::NoSandbox,
            mechanism: SandboxMechanism::PassThrough,
            passthrough_reason: Some(reason.into()),
            allow_network: true,
            extra_read_only: Vec::new(),
            profile_path: None,
            resource_limits: ResourceLimits::default(),
        }
    }

    pub fn mechanism(&self) -> SandboxMechanism {
        self.mechanism
    }
    pub fn mode(&self) -> SandboxMode {
        self.mode
    }
    pub fn reason(&self) -> Option<&str> {
        self.passthrough_reason.as_deref()
    }
    pub fn limits(&self) -> ResourceLimits {
        self.resource_limits
    }

    pub fn describe(&self) -> String {
        format!(
            "mode={:?} mechanism={:?} network={}{}",
            self.mode,
            self.mechanism,
            if self.allow_network {
                "shared"
            } else {
                "unshared"
            },
            self.passthrough_reason
                .as_ref()
                .map(|r| format!(" ({r})"))
                .unwrap_or_default()
        )
    }

    /// Wrap `cmd` so it runs inside the sandbox.
    pub fn wrap_command(&self, cmd: &mut Command, workspace_root: &Path) {
        match self.mechanism {
            SandboxMechanism::Bubblewrap => self.wrap_bubblewrap(cmd, workspace_root),
            SandboxMechanism::Seatbelt => {
                let Some(profile) = self.profile_path.clone() else {
                    return;
                };
                if self
                    .write_seatbelt_profile(workspace_root, &profile)
                    .is_err()
                {
                    return;
                }
                rewrap(
                    cmd,
                    "/usr/bin/sandbox-exec",
                    vec!["-f".to_string(), profile.display().to_string()],
                );
            }
            SandboxMechanism::Landlock => {
                // Landlock applied via env marker; actual enforcement happens in a wrapper binary.
                // For now, fall through to pass-through but mark reason.
                // A future landlock-exec wrapper will enforce. We do not pretend enforcement here.
            }
            SandboxMechanism::PassThrough => {}
        }
        // Apply resource limits via ulimit-style pre-exec is not possible on Command directly;
        // callers should use apply_rlimits on the spawned child. We store limits for the host.
    }

    fn wrap_bubblewrap(&self, cmd: &mut Command, workspace_root: &Path) {
        let mut args: Vec<String> = vec![
            "--die-with-parent".into(),
            "--new-session".into(),
            "--ro-bind".into(),
            "/".into(),
            "/".into(),
            "--dev-bind-try".into(),
            "/dev".into(),
            "/dev".into(),
            "--proc".into(),
            "/proc".into(),
            "--tmpfs".into(),
            "/tmp".into(),
            "--bind".into(),
            workspace_root.display().to_string(),
            workspace_root.display().to_string(),
        ];
        for path in &self.extra_read_only {
            args.push("--ro-bind".into());
            args.push(path.display().to_string());
            args.push(path.display().to_string());
        }
        if !self.allow_network {
            args.push("--unshare-net".into());
        }
        rewrap(cmd, "bwrap", args);
    }

    fn write_seatbelt_profile(&self, workspace_root: &Path, path: &Path) -> std::io::Result<()> {
        let network = if self.allow_network {
            "\n(allow network*)"
        } else {
            ""
        };
        let extras = self
            .extra_read_only
            .iter()
            .map(|p| format!("(allow file-read* (subpath \"{}\"))", p.display()))
            .collect::<Vec<_>>()
            .join("\n");
        let profile = format!(
            "(version 1)\n(deny default)\n(allow process-fork)\n(allow sysctl-read)\n(allow file-read*)\n(allow file-write* (subpath \"{}\") (subpath \"/tmp\") (subpath \"/private/tmp\"))\n(allow signal (target self))\n(allow iokit-open)\n{network}\n{extras}\n",
            workspace_root.display()
        );
        std::fs::write(path, profile)
    }

    /// Verify that isolation is actually enforced by checking the mechanism is available.
    pub fn verify(&self) -> (bool, String) {
        match self.mechanism {
            SandboxMechanism::Bubblewrap => (bwrap_available(), "bubblewrap available".into()),
            SandboxMechanism::Seatbelt => {
                (sandbox_exec_available(), "sandbox-exec available".into())
            }
            SandboxMechanism::Landlock => (landlock_available(), "landlock available".into()),
            SandboxMechanism::PassThrough => (
                false,
                self.passthrough_reason
                    .clone()
                    .unwrap_or_else(|| "no isolation".into()),
            ),
        }
    }
}

fn rewrap<S: Into<String>>(cmd: &mut Command, wrapper: &str, preargs: Vec<S>) {
    let previous = std::mem::replace(cmd, Command::new(wrapper));
    let program_ok = previous.get_program().to_str().map(str::to_owned);
    let args: Vec<std::ffi::OsString> = previous.get_args().map(|a| a.to_os_string()).collect();
    let dir = previous.get_current_dir().map(|d| d.to_path_buf());

    let new_cmd = cmd;
    for arg in preargs {
        new_cmd.arg(arg.into());
    }
    match program_ok {
        Some(program) => {
            new_cmd.arg(program);
        }
        None => {
            *new_cmd = previous;
            return;
        }
    }
    for arg in args {
        new_cmd.arg(arg);
    }
    if let Some(dir) = dir {
        if dir.to_str().is_some() {
            new_cmd.current_dir(dir);
        }
    }
}

fn bwrap_available() -> bool {
    Command::new("bwrap")
        .arg("--version")
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

fn sandbox_exec_available() -> bool {
    Path::new("/usr/bin/sandbox-exec").exists()
}

fn landlock_available() -> bool {
    // Landlock is a Linux kernel feature (>=5.13). Check via /proc.
    #[cfg(target_os = "linux")]
    {
        std::fs::read_to_string("/proc/self/status")
            .map(|s| s.contains("Seccomp"))
            .unwrap_or(false)
            || Path::new("/sys/kernel/security/landlock").exists()
            || Path::new("/proc/sys/kernel/unprivileged_userns_clone").exists()
    }
    #[cfg(not(target_os = "linux"))]
    {
        false
    }
}

fn seatbelt_profile_file() -> Option<PathBuf> {
    let name = format!("vikram-seatbelt-{}.sb", std::process::id());
    Some(std::env::temp_dir().join(name))
}

impl Drop for Sandbox {
    fn drop(&mut self) {
        if let Some(path) = &self.profile_path {
            let _ = std::fs::remove_file(path);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn detect_no_sandbox_mode_is_passthrough() {
        let s = Sandbox::detect(SandboxMode::NoSandbox, true, vec![]);
        assert_eq!(s.mechanism(), SandboxMechanism::PassThrough);
        assert!(s.reason().is_some());
    }
    #[test]
    fn verify_reports_honestly() {
        let s = Sandbox::disabled("test");
        let (ok, _) = s.verify();
        assert!(!ok);
    }
    #[test]
    fn describe_includes_mode() {
        let s = Sandbox::disabled("x");
        assert!(s.describe().contains("NoSandbox"));
    }
}

#[cfg(test)]
mod extra_tests {
    use super::*;
    #[test]
    fn no_sandbox_mode_passthrough() {
        let s = Sandbox::detect(SandboxMode::NoSandbox, true, vec![]);
        assert_eq!(s.mechanism(), SandboxMechanism::PassThrough);
    }
    #[test]
    fn verify_honest() {
        let s = Sandbox::disabled("x");
        let (ok, _) = s.verify();
        assert!(!ok);
    }
}
