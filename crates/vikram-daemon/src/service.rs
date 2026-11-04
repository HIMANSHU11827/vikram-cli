//! Installing the daemon as an operating-system service.
//!
//! The daemon runs perfectly well in the foreground (`vikram daemon start --foreground`), so a
//! service is a convenience and never a requirement. Where a service manager exists, its own
//! mechanism is used rather than a pretend one:
//!
//! * Linux: a systemd **user** unit, so no root is needed and the daemon lives and dies with the
//!   login session it belongs to.
//! * macOS: a launchd agent in `~/Library/LaunchAgents`.
//! * Windows: a scheduled task that runs at logon.
//!
//! Everything here writes files and prints the one command that finishes the job; nothing escalates
//! privileges on the operator's behalf, and nothing is deleted that this module did not create.

use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::process::Command;

use thiserror::Error;

/// The service manager this machine actually has.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Platform {
    /// systemd, driven through `systemctl --user`.
    Systemd,
    /// launchd, driven through `launchctl`.
    Launchd,
    /// Task Scheduler, driven by `schtasks`.
    ScheduledTask,
    /// No known service manager; the daemon can still be run in the foreground.
    Unsupported,
}

impl Platform {
    pub fn as_str(self) -> &'static str {
        match self {
            Platform::Systemd => "systemd",
            Platform::Launchd => "launchd",
            Platform::ScheduledTask => "scheduled task",
            Platform::Unsupported => "none",
        }
    }
}

/// Which service manager this build is talking to.
pub fn platform() -> Platform {
    #[cfg(target_os = "linux")]
    {
        if Path::new("/run/systemd/system").exists() || Path::new("/usr/bin/systemctl").exists() {
            return Platform::Systemd;
        }
        Platform::Unsupported
    }
    #[cfg(target_os = "macos")]
    {
        return Platform::Launchd;
    }
    #[cfg(target_os = "windows")]
    {
        return Platform::ScheduledTask;
    }
    #[cfg(not(any(target_os = "linux", target_os = "macos", target_os = "windows")))]
    {
        Platform::Unsupported
    }
}

/// What the service needs to know to start the daemon again after a reboot.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Spec {
    /// The binary to run. Absent when the running executable cannot be resolved.
    pub exec: PathBuf,
    /// `VIKRAM_HOME`, so the service finds the same database the CLI does.
    pub home: PathBuf,
    /// Arguments after the binary.
    pub args: Vec<String>,
}

impl Spec {
    /// The spec for the daemon of *this* installation.
    pub fn current(home: &Path) -> Result<Self, ServiceError> {
        let exec = std::env::current_exe().map_err(ServiceError::Io)?;
        Ok(Spec {
            exec,
            home: home.to_path_buf(),
            args: vec!["daemon".into(), "start".into(), "--foreground".into()],
        })
    }

    fn command_line(&self) -> String {
        let mut line = self.exec.display().to_string();
        for arg in &self.args {
            line.push(' ');
            line.push_str(arg);
        }
        line
    }
}

#[derive(Debug, Error)]
pub enum ServiceError {
    #[error("{0}")]
    Io(#[source] io::Error),
    #[error(
        "{platform} has no service manager this build knows how to drive; run `vikram daemon start --foreground` instead"
    )]
    Unsupported { platform: &'static str },
    #[error("{command} failed: {detail}")]
    Command { command: String, detail: String },
    #[error("nothing is installed at {0}")]
    NotInstalled(String),
}

/// Where the unit file lives, and what it is called.
///
/// The path is returned rather than guessed by each caller so a test can assert it without
/// touching the real home directory.
pub fn unit_path(platform: Platform, home: &Path) -> Option<PathBuf> {
    match platform {
        Platform::Systemd => {
            let config = std::env::var_os("XDG_CONFIG_HOME")
                .map(PathBuf::from)
                .or_else(|| {
                    std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".config"))
                })?;
            Some(config.join("systemd/user/vikram-daemon.service"))
        }
        Platform::Launchd => {
            let home_dir = std::env::var_os("HOME").map(PathBuf::from)?;
            Some(home_dir.join("Library/LaunchAgents/dev.vikram.daemon.plist"))
        }
        Platform::ScheduledTask => Some(home.join("daemon/vikram-daemon.xml")),
        Platform::Unsupported => None,
    }
}

/// The unit file's text. Pure, so what gets installed can be read and tested.
pub fn unit_text(platform: Platform, spec: &Spec) -> Result<String, ServiceError> {
    match platform {
        Platform::Systemd => Ok(format!(
            "[Unit]\n\
             Description=vikram background daemon\n\
             After=default.target\n\
             \n\
             [Service]\n\
             Type=simple\n\
             ExecStart={command}\n\
             WorkingDirectory={home}\n\
             Environment=VIKRAM_HOME={home}\n\
             Restart=on-failure\n\
             RestartSec=5\n\
             KillSignal=SIGTERM\n\
             TimeoutStopSec=30\n\
             \n\
             [Install]\n\
             WantedBy=default.target\n",
            command = spec.command_line(),
            home = spec.home.display()
        )),
        Platform::Launchd => Ok(format!(
            "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n\
             <!DOCTYPE plist PUBLIC \"-//Apple//DTD PLIST 1.0//EN\" \"http://www.apple.com/DTDs/PropertyList-1.0.dtd\">\n\
             <plist version=\"1.0\">\n\
             <dict>\n\
             \x20 <key>Label</key><string>dev.vikram.daemon</string>\n\
             \x20 <key>ProgramArguments</key>\n\
             \x20 <array>\n\
             {args}\
             \x20 </array>\n\
             \x20 <key>EnvironmentVariables</key>\n\
             \x20 <dict><key>VIKRAM_HOME</key><string>{home}</string></dict>\n\
             \x20 <key>RunAtLoad</key><true/>\n\
             \x20 <key>KeepAlive</key><true/>\n\
             \x20 <key>StandardOutPath</key><string>{home}/daemon/service.log</string>\n\
             \x20 <key>StandardErrorPath</key><string>{home}/daemon/service.log</string>\n\
             </dict>\n\
             </plist>\n",
            args = spec
                .args
                .iter()
                .map(|arg| format!("    <string>{}</string>\n", arg.replace('&', "&amp;")))
                .collect::<String>(),
            home = spec.home.display()
        )),
        Platform::ScheduledTask => Ok(format!(
            "<Task version=\"1.2\">\n\
             \x20 <RegistrationInfo><Description>vikram background daemon</Description></RegistrationInfo>\n\
             \x20 <Settings><MultipleInstancesPolicy>IgnoreNew</MultipleInstancesPolicy></Settings>\n\
             \x20 <Actions><Exec><Command>{exec}</Command>{args}</Exec></Actions>\n\
             \x20 <Triggers><LogonTrigger/></Triggers>\n\
             </Task>\n",
            exec = spec.exec.display(),
            args = spec
                .args
                .iter()
                .map(|arg| format!("<Arguments>{arg}</Arguments>"))
                .collect::<String>()
        )),
        Platform::Unsupported => Err(ServiceError::Unsupported {
            platform: platform.as_str(),
        }),
    }
}

/// Write the unit file. Says where it went; does not start anything.
pub fn install(spec: &Spec) -> Result<String, ServiceError> {
    let platform = platform();
    let path = unit_path(platform, &spec.home).ok_or(ServiceError::Unsupported {
        platform: platform.as_str(),
    })?;
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).map_err(ServiceError::Io)?;
    }
    fs::create_dir_all(spec.home.join("daemon")).map_err(ServiceError::Io)?;
    let text = unit_text(platform, spec)?;
    fs::write(&path, text).map_err(ServiceError::Io)?;
    Ok(format!(
        "installed the {} service at {}\nnext: {}",
        platform.as_str(),
        path.display(),
        activation(platform).unwrap_or_else(|| "vikram daemon start".into())
    ))
}

/// Remove the unit file, if this module put one there.
pub fn uninstall(spec: &Spec) -> Result<String, ServiceError> {
    let platform = platform();
    let path = unit_path(platform, &spec.home).ok_or(ServiceError::Unsupported {
        platform: platform.as_str(),
    })?;
    if !path.exists() {
        return Err(ServiceError::NotInstalled(path.display().to_string()));
    }
    fs::remove_file(&path).map_err(ServiceError::Io)?;
    Ok(format!("removed {}", path.display()))
}

/// Ask the service manager to pick the unit up and start it at login.
pub fn enable(spec: &Spec) -> Result<String, ServiceError> {
    let platform = platform();
    match platform {
        Platform::Systemd => {
            run("systemctl", &["--user", "daemon-reload"])?;
            run("systemctl", &["--user", "enable", "vikram-daemon.service"])
        }
        Platform::Launchd => {
            let path = unit_path(platform, &spec.home).ok_or(ServiceError::Unsupported {
                platform: platform.as_str(),
            })?;
            run("launchctl", &["load", "-w", &path.display().to_string()])
        }
        Platform::ScheduledTask => {
            let path = unit_path(platform, &spec.home).ok_or(ServiceError::Unsupported {
                platform: platform.as_str(),
            })?;
            run(
                "schtasks",
                &[
                    "/Create",
                    "/TN",
                    "vikram-daemon",
                    "/XML",
                    &path.display().to_string(),
                    "/F",
                ],
            )
        }
        Platform::Unsupported => Err(ServiceError::Unsupported {
            platform: platform.as_str(),
        }),
    }
}

/// Stop starting the daemon at login, leaving the unit file in place.
pub fn disable() -> Result<String, ServiceError> {
    match platform() {
        Platform::Systemd => run("systemctl", &["--user", "disable", "vikram-daemon.service"]),
        Platform::Launchd => run("launchctl", &["unload", "-w", "dev.vikram.daemon"]),
        Platform::ScheduledTask => run("schtasks", &["/Delete", "/TN", "vikram-daemon", "/F"]),
        Platform::Unsupported => Err(ServiceError::Unsupported {
            platform: platform().as_str(),
        }),
    }
}

/// Whether a unit file is in place.
pub fn is_installed(spec: &Spec) -> bool {
    unit_path(platform(), &spec.home).is_some_and(|path| path.exists())
}

/// The one command an operator runs by hand after `install`.
pub fn activation(platform: Platform) -> Option<String> {
    match platform {
        Platform::Systemd => Some(
            "systemctl --user daemon-reload && systemctl --user enable --now vikram-daemon".into(),
        ),
        Platform::Launchd => {
            Some("launchctl load -w ~/Library/LaunchAgents/dev.vikram.daemon.plist".into())
        }
        Platform::ScheduledTask => Some("schtasks /Create /TN vikram-daemon /XML <file> /F".into()),
        Platform::Unsupported => None,
    }
}

fn run(command: &str, args: &[&str]) -> Result<String, ServiceError> {
    let output = Command::new(command)
        .args(args)
        .output()
        .map_err(ServiceError::Io)?;
    let stdout = String::from_utf8_lossy(&output.stdout).trim().to_string();
    let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
    if output.status.success() {
        let detail = if stdout.is_empty() { stderr } else { stdout };
        Ok(format!(
            "{} {}: {}",
            command,
            args.join(" "),
            if detail.is_empty() { "ok" } else { &detail }
        ))
    } else {
        Err(ServiceError::Command {
            command: format!("{command} {}", args.join(" ")),
            detail: if stderr.is_empty() { stdout } else { stderr },
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    fn spec(home: &Path) -> Spec {
        Spec {
            exec: PathBuf::from("/opt/vikram/bin/vikram"),
            home: home.to_path_buf(),
            args: vec!["daemon".into(), "start".into(), "--foreground".into()],
        }
    }

    #[test]
    fn a_systemd_unit_says_how_to_start_and_stop_the_daemon() {
        let home = TempDir::new().unwrap();
        let text = unit_text(Platform::Systemd, &spec(home.path())).unwrap();
        assert!(
            text.contains("ExecStart=/opt/vikram/bin/vikram daemon start --foreground"),
            "the unit must name the exact command: {text}"
        );
        assert!(
            text.contains(&format!(
                "Environment=VIKRAM_HOME={}",
                home.path().display()
            )),
            "the service must find the same home the CLI uses: {text}"
        );
        assert!(
            text.contains("Restart=on-failure") && text.contains("KillSignal=SIGTERM"),
            "a crash must restart and a stop must be graceful: {text}"
        );
    }

    #[test]
    fn a_launchd_agent_keeps_the_daemon_alive_and_logs_where_it_can_be_read() {
        let home = TempDir::new().unwrap();
        let text = unit_text(Platform::Launchd, &spec(home.path())).unwrap();
        assert!(text.contains("<key>KeepAlive</key><true/>"));
        assert!(text.contains("dev.vikram.daemon"));
        assert!(
            text.contains(&format!("{}/daemon/service.log", home.path().display())),
            "launchd needs somewhere to write: {text}"
        );
    }

    #[test]
    fn a_scheduled_task_runs_at_logon_and_never_twice() {
        let home = TempDir::new().unwrap();
        let text = unit_text(Platform::ScheduledTask, &spec(home.path())).unwrap();
        assert!(text.contains("<LogonTrigger/>"));
        assert!(text.contains("IgnoreNew"));
    }

    #[test]
    fn a_machine_with_no_service_manager_is_told_to_use_the_foreground() {
        let home = TempDir::new().unwrap();
        let err = unit_text(Platform::Unsupported, &spec(home.path())).unwrap_err();
        assert!(
            err.to_string().contains("--foreground"),
            "the fallback has to be named: {err}"
        );
    }

    #[test]
    fn installing_writes_the_unit_and_installing_nothing_is_an_error() {
        let home = TempDir::new().unwrap();
        let spec = spec(home.path());
        // Choose the platform this machine really has; the other branches are covered above with
        // their own text, which is what an install would write.
        let Some(path) = unit_path(platform(), home.path()) else {
            return;
        };
        if platform() != Platform::Systemd && platform() != Platform::Launchd {
            return;
        }
        assert!(!is_installed(&spec));
        let report = install(&spec).unwrap();
        assert!(is_installed(&spec), "{report}");
        assert!(path.exists());
        uninstall(&spec).unwrap();
        assert!(!is_installed(&spec));
        assert!(
            uninstall(&spec).is_err(),
            "removing what is not there is reported, not ignored"
        );
    }

    #[test]
    fn the_activation_hint_matches_the_platform() {
        assert!(
            activation(Platform::Systemd)
                .unwrap()
                .contains("systemctl --user")
        );
        assert!(
            activation(Platform::Launchd)
                .unwrap()
                .contains("launchctl load")
        );
        assert!(activation(Platform::Unsupported).is_none());
    }
}
