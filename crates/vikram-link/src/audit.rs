//! One append-only line per remote action.
//!
//! The runtime journal is per session and a link action often belongs to no session at all —
//! minting a token, opening the event stream — so the link keeps its own journal beside the token
//! store: `$VIKRAM_HOME/link/audit.jsonl`, opened in append mode, one JSON object per line. The
//! secret is never an argument to [`AuditEntry`], which is the only structural guard that matters.

use serde::{Deserialize, Serialize};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

/// The token a failed authentication is recorded under. A caller with no usable token still gets a
/// line: an attempt to reach the runtime is worth knowing about.
pub const UNKNOWN_TOKEN: &str = "<unknown>";

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct AuditEntry {
    pub ts_ms: u64,
    pub token: String,
    pub tier: String,
    pub action: String,
    pub target: String,
    /// `ok`, `denied: …` or `error: …`.
    pub result: String,
}

impl AuditEntry {
    pub fn ok(
        ts_ms: u64,
        token: impl Into<String>,
        tier: impl Into<String>,
        action: impl Into<String>,
        target: impl Into<String>,
    ) -> Self {
        Self {
            ts_ms,
            token: token.into(),
            tier: tier.into(),
            action: action.into(),
            target: target.into(),
            result: "ok".into(),
        }
    }

    pub fn denied(mut self, reason: impl AsRef<str>) -> Self {
        self.result = format!("denied: {}", reason.as_ref());
        self
    }

    pub fn failed(mut self, reason: impl AsRef<str>) -> Self {
        self.result = format!("error: {}", reason.as_ref());
        self
    }
}

pub struct Audit {
    path: PathBuf,
    file: Mutex<std::fs::File>,
}

impl Audit {
    /// Open (or create) the log in append mode. Every writer shares one handle behind a mutex, so
    /// two connections cannot interleave half a line.
    pub fn open(path: impl Into<PathBuf>) -> Result<Audit, String> {
        let path = path.into();
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir)
                .map_err(|e| format!("cannot create {}: {e}", dir.display()))?;
            restrict(dir, 0o700);
        }
        let file = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&path)
            .map_err(|e| format!("cannot open the audit log at {}: {e}", path.display()))?;
        restrict(&path, 0o600);
        Ok(Audit {
            path,
            file: Mutex::new(file),
        })
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn record(&self, entry: &AuditEntry) -> Result<(), String> {
        let mut file = self
            .file
            .lock()
            .map_err(|_| "the audit log is locked".to_owned())?;
        let line = serde_json::to_string(entry)
            .map_err(|e| format!("cannot serialize an audit record: {e}"))?;
        writeln!(*file, "{line}").map_err(|e| format!("cannot append to the audit log: {e}"))?;
        file.flush()
            .map_err(|e| format!("cannot flush the audit log: {e}"))
    }

    /// The most recent `limit` records, oldest first. A line that will not parse is skipped rather
    /// than failing the read: a torn write must not hide every record after it.
    pub fn tail(&self, limit: usize) -> Result<Vec<AuditEntry>, String> {
        let text = match std::fs::read_to_string(&self.path) {
            Ok(text) => text,
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(err) => return Err(format!("cannot read the audit log: {err}")),
        };
        let mut entries: Vec<AuditEntry> = text
            .lines()
            .filter(|line| !line.trim().is_empty())
            .filter_map(|line| serde_json::from_str(line).ok())
            .collect();
        if entries.len() > limit {
            entries.drain(..entries.len() - limit);
        }
        Ok(entries)
    }
}

#[cfg(unix)]
fn restrict(path: &Path, mode: u32) {
    use std::os::unix::fs::PermissionsExt;
    let _ = std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode));
}

#[cfg(not(unix))]
fn restrict(_path: &Path, _mode: u32) {}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn records_append_and_the_tail_is_ordered() {
        let dir = tempfile::tempdir().unwrap();
        let audit = Audit::open(dir.path().join("link/audit.jsonl")).unwrap();
        for index in 0..3 {
            audit
                .record(&AuditEntry::ok(
                    index, "laptop", "admin", "sessions", "ses_1",
                ))
                .unwrap();
        }
        let tail = audit.tail(2).unwrap();
        assert_eq!(tail.len(), 2);
        assert_eq!(tail[0].ts_ms, 1);
        assert_eq!(tail[1].ts_ms, 2);
    }

    #[test]
    fn a_denial_is_a_line_too_and_no_secret_can_be_written() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("link/audit.jsonl");
        let audit = Audit::open(&path).unwrap();
        audit
            .record(
                &AuditEntry::ok(1, "phone", "read", "submit", "ses_1").denied("read is a mirror"),
            )
            .unwrap();
        let text = std::fs::read_to_string(&path).unwrap();
        assert!(text.contains("denied: read is a mirror"));
        assert!(!text.contains("vlink_"));
    }
}
