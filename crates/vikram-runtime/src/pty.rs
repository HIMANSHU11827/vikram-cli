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
    let pair = pty_system
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

    let child = pair
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
        Some(name) if reg.contains_key(name) => {
            return Err(format!("session `{name}` is already open"));
        }
        Some(name) => name.to_owned(),
        None => format!("term-{}-{}", std::process::id(), seq),
    };

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
}
