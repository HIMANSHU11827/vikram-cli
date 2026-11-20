//! Persistent PTY-backed terminal sessions.
//!
//! `shell.run` executes a one-shot command; this module keeps a real interactive pseudo-terminal
//! alive so the agent can drive long-running processes. Sessions are owned by the process (a
//! global registry), so a tool call can write to a session opened by an earlier call.

use portable_pty::{Child, CommandBuilder, MasterPty, PtySize, native_pty_system};
use std::collections::HashMap;
use std::io::{Read, Write};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, OnceLock};

/// Output retained per session. Older bytes are dropped so a chatty process cannot exhaust memory.
const MAX_BUFFER: usize = 256 * 1024;

/// Live sessions are capped: each one is a real shell with a PTY and a reader thread, and the
/// registry is process-global, so an unbounded `terminal open` loop would leak them all.
const MAX_SESSIONS: usize = 8;

/// Platform-aware default interactive shell.
fn default_shell() -> String {
    #[cfg(windows)]
    {
        if let Ok(comspec) = std::env::var("COMSPEC") {
            if !comspec.trim().is_empty() {
                return comspec;
            }
        }
        "powershell.exe".to_owned()
    }
    #[cfg(not(windows))]
    {
        "sh".to_owned()
    }
}

/// A shell that runs a sandbox wrapper command line.
///
/// Reached only when `Sandbox::wrap_command` rewrote the argv, which happens on Linux
/// (bubblewrap) and macOS (sandbox-exec). Windows has no enforcing wrapper, so the
/// `exec` form built by the caller is never produced there.
fn wrapper_command(script: &str) -> CommandBuilder {
    #[cfg(windows)]
    {
        let mut cmd = CommandBuilder::new("cmd");
        cmd.arg("/C");
        cmd.arg(script);
        cmd
    }
    #[cfg(not(windows))]
    {
        let mut cmd = CommandBuilder::new("sh");
        cmd.arg("-c");
        cmd.arg(script);
        cmd
    }
}

pub struct TerminalSession {
    pub id: String,
    pub shell: String,
    pub cwd: String,
    pub master: Box<dyn MasterPty + Send>,
    pub writer: Box<dyn Write + Send>,
    pub child: Box<dyn Child + Send + Sync>,
    pub output: Arc<Mutex<Vec<u8>>>,
}

impl std::fmt::Debug for TerminalSession {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TerminalSession")
            .field("id", &self.id)
            .field("shell", &self.shell)
            .field("cwd", &self.cwd)
            .finish()
    }
}

fn registry() -> &'static Mutex<HashMap<String, TerminalSession>> {
    static REGISTRY: OnceLock<Mutex<HashMap<String, TerminalSession>>> = OnceLock::new();
    REGISTRY.get_or_init(|| Mutex::new(HashMap::new()))
}

fn counter() -> &'static AtomicU64 {
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    &COUNTER
}

/// Open a PTY session under the given sandbox. When the sandbox is enforced
/// (bubblewrap/Seatbelt) we cannot inject it into a PTY child process portably via
/// `portable-pty`, so we wrap the *shell command* in the same wrapper the shell
/// tool uses: `bwrap -- <shell>` / `sandbox-exec -f <profile> -- <shell>`. When it
/// is not enforced we run on the host and surface the degradation in the spawn
/// result string count (logged by the caller via `Sandbox::enforcement_notice`).
pub fn open_sandboxed(
    sandbox: &crate::sandbox::Sandbox,
    workspace_root: &std::path::Path,
    name: Option<&str>,
    shell: Option<&str>,
    cwd: Option<&str>,
    rows: u16,
    cols: u16,
) -> Result<String, String> {
    // `cwd` is model-supplied and becomes the child process's working directory, so it gets the
    // same workspace containment the fs tools apply: anything resolving outside the root is
    // refused rather than silently chrooting the shell somewhere unintended.
    let cwd = match cwd {
        Some(raw) => {
            let resolved = crate::config::resolve_within(workspace_root, std::path::Path::new(raw))
                .ok_or_else(|| {
                    format!(
                        "cwd `{raw}` is outside the workspace `{}`",
                        workspace_root.display()
                    )
                })?;
            Some(resolved.to_string_lossy().into_owned())
        }
        None => None,
    };
    let cwd = cwd.as_deref();
    // Build the wrapped command line as a single shell string for the PTY.
    // portable-pty spawns the shell directly; wrapping via sh -c keeps the wrapper
    // args intact without a second exec machinery.
    let inner_shell = shell
        .map(|s| s.to_owned())
        .or_else(|| std::env::var("SHELL").ok())
        .filter(|s| !s.trim().is_empty())
        .unwrap_or_else(default_shell);
    // The sandbox's writable root is the real workspace, never the model-chosen
    // `cwd`: `cwd: "/"` would otherwise make the whole host filesystem writable
    // inside the wrapper. `cwd` is only the process working directory, applied
    // to the CommandBuilder below.
    // Reuse the sandbox wrapper on a throwaway Command to learn the wrapper argv.
    let mut probe = std::process::Command::new(&inner_shell);
    sandbox.wrap_command(&mut probe, workspace_root);
    let program = probe.get_program().to_string_lossy().into_owned();
    let args: Vec<String> = probe
        .get_args()
        .map(|a| a.to_string_lossy().into_owned())
        .collect();
    // If wrapped, the PTY must run the wrapper; encode as `sh -c 'exec ...'`.
    let (cmd_shell, _cmd_arg) = if program == inner_shell {
        (inner_shell.clone(), None)
    } else {
        // program is the wrapper (bwrap / sandbox-exec); re-run through sh -c.
        // rewrap already ended the argv with inner_shell — appending it again
        // produced `sh sh`, which dies instantly under an enforced sandbox.
        let mut parts = vec![program.clone()];
        parts.extend(args.clone());
        let joined = parts
            .iter()
            .map(|p| {
                if p.chars()
                    .all(|c| c.is_alphanumeric() || "-_/=.".contains(c))
                {
                    p.clone()
                } else {
                    format!("'{}'", p.replace('\'', "'\\''"))
                }
            })
            .collect::<Vec<_>>()
            .join(" ");
        ("sh".to_owned(), Some(format!("exec {joined}")))
    };
    // On Unix portable-pty spawns with no argv; passing args via the shell string is
    // the only portable route, so run `sh -c 'exec <wrapper> <shell>'` when wrapped.
    if let Some(arg) = _cmd_arg {
        let mut cmd = wrapper_command(&arg);
        if let Some(cwd) = cwd {
            cmd.cwd(cwd);
        }
        return open_named_cmd(name, cmd, cwd, rows, cols);
    }
    open_named(name, Some(&cmd_shell), cwd, rows, cols)
}

/// Open a new PTY session and return its id. The shell defaults to `$SHELL` or `sh`.
pub fn open(
    shell: Option<&str>,
    cwd: Option<&str>,
    rows: u16,
    cols: u16,
) -> Result<String, String> {
    open_named(None, shell, cwd, rows, cols)
}

/// The same terminal, under a name the caller chose. A session a model can name twice — open
/// `build`, then write to `build` — is the difference between a second call working and an
/// `unknown session` answer, so the name is taken at open time rather than parsed out of prose.
pub fn open_named(
    name: Option<&str>,
    shell: Option<&str>,
    cwd: Option<&str>,
    rows: u16,
    cols: u16,
) -> Result<String, String> {
    let pty_system = native_pty_system();
    let _pair = pty_system
        .openpty(PtySize {
            rows: rows.max(1),
            cols: cols.max(1),
            pixel_width: 0,
            pixel_height: 0,
        })
        .map_err(|e| format!("openpty: {e}"))?;

    let shell = shell
        .map(|s| s.to_owned())
        .or_else(|| std::env::var("SHELL").ok())
        .filter(|s| !s.trim().is_empty())
        .unwrap_or_else(default_shell);

    let mut cmd = CommandBuilder::new(&shell);
    cmd.env("TERM", "xterm-256color");
    if let Some(cwd) = cwd {
        cmd.cwd(cwd);
    }
    open_named_cmd(name, cmd, cwd, rows, cols)
}

fn open_named_cmd(
    name: Option<&str>,
    cmd: CommandBuilder,
    cwd: Option<&str>,
    rows: u16,
    cols: u16,
) -> Result<String, String> {
    // Opening a name that is already live is the same session, not a failure: a model that is told
    // "already open" answers by inventing a fresh name (`snake`, `snake-run`, `snake-play`) and
    // never gets back to the session it wanted. Handing back the live id is the idempotent answer.
    if let Some(name) = name.map(str::trim).filter(|n| !n.is_empty()) {
        let reg = registry()
            .lock()
            .map_err(|_| "terminal registry poisoned")?;
        if reg.contains_key(name) {
            return Ok(name.to_owned());
        }
    }

    let pty_system = native_pty_system();
    let pair = pty_system
        .openpty(PtySize {
            rows: rows.max(1),
            cols: cols.max(1),
            pixel_width: 0,
            pixel_height: 0,
        })
        .map_err(|e| format!("openpty: {e}"))?;
    let shell = "sh".to_owned();
    let mut cmd = cmd;
    cmd.env("TERM", "xterm-256color");

    let mut child = pair
        .slave
        .spawn_command(cmd)
        .map_err(|e| format!("spawn {shell}: {e}"))?;
    drop(pair.slave);

    let mut reader = pair
        .master
        .try_clone_reader()
        .map_err(|e| format!("reader: {e}"))?;
    let writer = pair
        .master
        .take_writer()
        .map_err(|e| format!("writer: {e}"))?;
    let master = pair.master;

    let output: Arc<Mutex<Vec<u8>>> = Arc::new(Mutex::new(Vec::new()));
    let sink = Arc::clone(&output);
    std::thread::spawn(move || {
        let mut buf = [0u8; 8192];
        loop {
            match reader.read(&mut buf) {
                Ok(0) => break,
                Ok(n) => {
                    if let Ok(mut out) = sink.lock() {
                        out.extend_from_slice(&buf[..n]);
                        if out.len() > MAX_BUFFER {
                            let drop = out.len() - MAX_BUFFER;
                            out.drain(0..drop);
                        }
                    }
                }
                Err(_) => break,
            }
        }
    });

    let seq = counter().fetch_add(1, Ordering::Relaxed);
    let cwd = cwd.map(|c| c.to_owned()).unwrap_or_else(|| ".".to_owned());
    let mut reg = registry().lock().expect("terminal registry");
    let id = match name.map(str::trim).filter(|n| !n.is_empty()) {
        Some(name) => name.to_owned(),
        None => format!("term-{}-{}", std::process::id(), seq),
    };
    // The name was taken between the check above and here. The session that already exists wins;
    // this one is stopped rather than dropped, so its shell does not outlive the call.
    if reg.contains_key(&id) {
        drop(reg);
        let _ = child.kill();
        let _ = child.wait();
        return Ok(id);
    }
    if reg.len() >= MAX_SESSIONS {
        drop(reg);
        let _ = child.kill();
        let _ = child.wait();
        return Err(format!(
            "terminal session limit reached ({MAX_SESSIONS}); close a session first"
        ));
    }

    reg.insert(
        id.clone(),
        TerminalSession {
            id: id.clone(),
            shell,
            cwd,
            master,
            writer,
            child,
            output,
        },
    );
    drop(reg);

    Ok(id)
}

/// Write raw input to a session.
pub fn write(id: &str, input: &str) -> Result<(), String> {
    let mut reg = registry()
        .lock()
        .map_err(|_| "terminal registry poisoned")?;
    let session = reg
        .get_mut(id)
        .ok_or_else(|| format!("unknown session `{id}`"))?;
    session
        .writer
        .write_all(input.as_bytes())
        .map_err(|e| format!("write: {e}"))?;
    session.writer.flush().map_err(|e| format!("flush: {e}"))?;
    Ok(())
}

/// Read buffered output. When `drain` is true, the buffer is cleared after reading.
pub fn read(id: &str, drain: bool) -> Result<String, String> {
    let reg = registry()
        .lock()
        .map_err(|_| "terminal registry poisoned")?;
    let session = reg
        .get(id)
        .ok_or_else(|| format!("unknown session `{id}`"))?;
    let mut out = session.output.lock().map_err(|_| "output poisoned")?;
    let text = String::from_utf8_lossy(&out).into_owned();
    if drain {
        out.clear();
    }
    Ok(text)
}

pub fn resize(id: &str, rows: u16, cols: u16) -> Result<(), String> {
    let mut reg = registry()
        .lock()
        .map_err(|_| "terminal registry poisoned")?;
    let session = reg
        .get_mut(id)
        .ok_or_else(|| format!("unknown session `{id}`"))?;
    session
        .master
        .resize(PtySize {
            rows: rows.max(1),
            cols: cols.max(1),
            pixel_width: 0,
            pixel_height: 0,
        })
        .map_err(|e| format!("resize: {e}"))?;
    Ok(())
}

pub fn close(id: &str) -> Result<(), String> {
    let mut reg = registry()
        .lock()
        .map_err(|_| "terminal registry poisoned")?;
    let mut session = reg
        .remove(id)
        .ok_or_else(|| format!("unknown session `{id}`"))?;
    // The PTY child called setsid at spawn, so its pid is also its process-group id: killing
    // the group takes down whatever the session spawned (`npm run dev` and its children), not
    // just the direct child. Same approach as `verification::kill_tree`.
    #[cfg(unix)]
    if let Some(pid) = session.child.process_id() {
        // SAFETY: kill(2) is always safe to call; a stale pgid just returns ESRCH.
        unsafe {
            libc::kill(-(pid as i32), libc::SIGKILL);
        }
    }
    let _ = session.child.kill();
    let _ = session.child.wait();
    Ok(())
}

/// List live session ids with their shell and working directory.
pub fn list() -> Vec<(String, String, String)> {
    let reg = match registry().lock() {
        Ok(reg) => reg,
        Err(_) => return Vec::new(),
    };
    reg.values()
        .map(|s| (s.id.clone(), s.shell.clone(), s.cwd.clone()))
        .collect()
}

/// Number of live sessions.
pub fn count() -> usize {
    registry().lock().map(|r| r.len()).unwrap_or(0)
}

/// Whether a session with this id is live right now.
pub fn is_open(id: &str) -> bool {
    registry()
        .lock()
        .map(|reg| reg.contains_key(id))
        .unwrap_or(false)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn open_write_read_close_round_trip() {
        let id = match open(Some("sh"), None, 24, 80) {
            Ok(id) => id,
            Err(_) => return, // no PTY support in this environment
        };
        let _ = write(&id, "echo vikram-pty-test\n");
        // Give the shell a moment to echo and execute.
        for _ in 0..40 {
            std::thread::sleep(std::time::Duration::from_millis(25));
            let out = read(&id, false).unwrap_or_default();
            if out.contains("vikram-pty-test") {
                break;
            }
        }
        let out = read(&id, false).unwrap_or_default();
        assert!(out.contains("vikram-pty-test"), "output was {out:?}");
        assert!(list().iter().any(|(sid, _, _)| sid == &id));
        close(&id).expect("close");
        assert!(!list().iter().any(|(sid, _, _)| sid == &id));
    }

    #[test]
    fn unknown_session_is_an_error() {
        assert!(write("no-such-session", "x").is_err());
        assert!(read("no-such-session", false).is_err());
        assert!(close("no-such-session").is_err());
    }

    #[test]
    fn cwd_outside_the_workspace_is_refused() {
        let dir = match std::env::temp_dir().canonicalize() {
            Ok(d) => d,
            Err(_) => return,
        };
        let sandbox = crate::sandbox::Sandbox::disabled("test");
        let err = open_sandboxed(&sandbox, &dir, None, Some("sh"), Some("/etc"), 24, 80)
            .expect_err("cwd outside the workspace must be refused");
        assert!(err.contains("outside the workspace"), "{err}");
        let err = open_sandboxed(&sandbox, &dir, None, Some("sh"), Some("../.."), 24, 80)
            .expect_err("cwd climbing out must be refused");
        assert!(err.contains("outside the workspace"), "{err}");
    }

    #[test]
    fn session_count_is_capped() {
        let sandbox = crate::sandbox::Sandbox::disabled("test");
        let root = std::env::temp_dir();
        let mut opened = Vec::new();
        for _ in 0..MAX_SESSIONS {
            match open_sandboxed(&sandbox, &root, None, Some("sh"), None, 24, 80) {
                Ok(id) => opened.push(id),
                Err(_) => return, // no PTY support in this environment
            }
        }
        let err = open_sandboxed(&sandbox, &root, None, Some("sh"), None, 24, 80)
            .expect_err("the session cap must refuse further opens");
        assert!(err.contains("session limit"), "{err}");
        for id in opened {
            let _ = close(&id);
        }
    }

    #[test]
    fn reopening_a_live_name_returns_the_same_session() {
        let name = "vikram-pty-idempotent";
        let first = match open_named(Some(name), Some("sh"), None, 24, 80) {
            Ok(id) => id,
            Err(_) => return, // no PTY support in this environment
        };
        let second = open_named(Some(name), Some("sh"), None, 24, 80)
            .expect("reopening a live name is not an error");
        assert_eq!(
            first, second,
            "the live session is handed back, not a new one"
        );
        close(&first).expect("close");
        assert!(!is_open(name), "nothing is left behind under that name");
    }
}
