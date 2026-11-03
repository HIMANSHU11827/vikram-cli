//! OS-level process isolation for commands the host agrees to run.
//!
//! Policy decides *whether* a command may run; the sandbox shrinks what a running command can do
//! to the machine around it. The mechanism is picked per platform: bubblewrap on Linux, Seatbelt
//! on macOS, and a documented pass-through where the platform offers nothing usable (Windows
//! Job objects and Termux are process-level only for now).

use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use std::process::Command;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SandboxMechanism {
    /// Linux: isolate with bubblewrap.
    Bubblewrap,
    /// macOS: enforce with a generated Seatbelt profile (`sandbox-exec`).
    Seatbelt,
    /// No usable OS facility; commands run unconfined and the reason is carried on the context.
    PassThrough,
}

#[derive(Clone, Debug)]
pub struct Sandbox {
    mechanism: SandboxMechanism,
    /// Why a pass-through mechanism was chosen, when it was.
    passthrough_reason: Option<String>,
    allow_network: bool,
    extra_read_only: Vec<PathBuf>,
    profile_path: Option<PathBuf>,
}

impl Sandbox {
    /// Resolve the strongest mechanism the platform supports. Callers should surface
    /// `describe()` so an operator sees when isolation is not actually enforced.
    pub fn detect(allow_network: bool, extra_read_only: Vec<PathBuf>) -> Self {
        let (mechanism, passthrough_reason) = if cfg!(target_os = "linux") {
            if bwrap_available() {
                (SandboxMechanism::Bubblewrap, None)
            } else {
                (
                    SandboxMechanism::PassThrough,
                    Some(
                        "bubblewrap not installed; OS isolation inactive (install bubblewrap)"
                            .into(),
                    ),
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
                Some("platform offers no supported isolation mechanism; process-level only".into()),
            )
        };

        let profile_path = mechanism
            .eq(&SandboxMechanism::Seatbelt)
            .then(seatbelt_profile_file)
            .flatten();

        Self {
            mechanism,
            passthrough_reason,
            allow_network,
            extra_read_only,
            profile_path,
        }
    }

    /// A disabled sandbox: behavior is unchanged and the reason states why.
    pub fn disabled(reason: impl Into<String>) -> Self {
        Self {
            mechanism: SandboxMechanism::PassThrough,
            passthrough_reason: Some(reason.into()),
            allow_network: true,
            extra_read_only: Vec::new(),
            profile_path: None,
        }
    }

    pub fn mechanism(&self) -> SandboxMechanism {
        self.mechanism
    }

    pub fn reason(&self) -> Option<&str> {
        self.passthrough_reason.as_deref()
    }

    /// One line an operator can read to know what isolation actually applies.
    pub fn describe(&self) -> String {
        match self.mechanism {
            SandboxMechanism::Bubblewrap => {
                format!(
                    "bubblewrap (network {})",
                    if self.allow_network {
                        "shared"
                    } else {
                        "unshared"
                    }
                )
            }
            SandboxMechanism::Seatbelt => "Seatbelt profile".to_string(),
            SandboxMechanism::PassThrough => self
                .passthrough_reason
                .clone()
                .unwrap_or_else(|| "no isolation".into()),
        }
    }

    /// Wrap `cmd` so it runs inside the sandbox. The command's own arguments, environment and
    /// working directory are preserved; only how the OS sees it changes.
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
            SandboxMechanism::PassThrough => {}
        }
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
}

/// Replace the program of `cmd` with `wrapper`, preserving the original program, arguments,
/// environment and working directory (`std::env` inheritance is kept by a fresh `Command`).
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
            // Non-UTF-8 program names are not representable after wrapping; run unconfined instead
            // of mangling the command.
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
    // stdio and env were never touched on `previous` here (they are applied by the caller after
    // wrapping), so nothing else needs carrying.
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
    fn detect_names_the_edge_case_instead_of_lying() {
        let sandbox = Sandbox::detect(false, vec![]);
        let described = sandbox.describe();
        if sandbox.mechanism() == SandboxMechanism::PassThrough {
            assert!(
                sandbox.reason().is_some(),
                "pass-through must say why: {described}"
            );
        }
    }

    #[test]
    fn wrap_command_preserves_program_and_arguments() {
        let sandbox = Sandbox::disabled("");
        let mut cmd = Command::new("sh");
        cmd.args(["-c", "echo hi"]);
        sandbox.wrap_command(&mut cmd, Path::new("/tmp"));
        assert_eq!(cmd.get_program(), std::ffi::OsStr::new("sh"));
        assert_eq!(
            cmd.get_args().collect::<Vec<_>>(),
            vec![std::ffi::OsStr::new("-c"), std::ffi::OsStr::new("echo hi")]
        );
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn bubblewrap_wrap_replaces_program_with_bwrap() {
        if !bwrap_available() {
            return;
        }
        let sandbox = Sandbox::detect(false, vec![]);
        let mut cmd = Command::new("sh");
        cmd.args(["-c", "echo hi"]);
        sandbox.wrap_command(&mut cmd, Path::new("/tmp"));
        assert_eq!(cmd.get_program(), std::ffi::OsStr::new("bwrap"));
        let args: Vec<_> = cmd
            .get_args()
            .map(|a| a.to_string_lossy().into_owned())
            .collect();
        assert!(args.contains(&"--unshare-net".to_string()));
        assert!(
            args.ends_with(&["sh".to_string(), "-c".to_string(), "echo hi".to_string()]),
            "argv must survive the wrap: {args:?}"
        );
    }
}
