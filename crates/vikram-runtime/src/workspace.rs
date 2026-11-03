//! Session workspaces, project association, artifact indexing and file checkpoints.
//!
//! The store stays authoritative for metadata; this module owns the filesystem side: lazily
//! creating a session's own workspace, enforcing that a session only touches its own workspace or
//! its associated project, indexing generated files as artifacts, and keeping pre-change copies so
//! a session can undo its own edits.

use std::fs;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};

use vikram_contracts::{AgentId, SessionId};
use vikram_store::{ArtifactRow, FileChangeRow, ProjectRow, Store};

use crate::error::{Result, RuntimeError};
use crate::types::now_ms;

/// Files larger than this are indexed but not checkpointed (recovery would be too expensive).
const CHECKPOINT_MAX_BYTES: u64 = 5 * 1024 * 1024;

pub struct WorkspaceManager {
    store: Store,
    home: PathBuf,
}

impl std::fmt::Debug for WorkspaceManager {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("WorkspaceManager").finish_non_exhaustive()
    }
}

impl WorkspaceManager {
    pub fn new(store: Store, home: PathBuf) -> Self {
        Self { store, home }
    }

    pub fn store(&self) -> &Store {
        &self.store
    }
    pub fn home(&self) -> &Path {
        &self.home
    }

    fn sessions_root(&self) -> PathBuf {
        self.home.join("sessions")
    }
    fn session_root(&self, session: &SessionId) -> PathBuf {
        self.sessions_root().join(session.as_str())
    }

    /// The lazily created workspace for a session. Creating it is the only side effect; a
    /// text-only conversation that never needs files never triggers it.
    pub fn ensure_session_workspace(&self, session: &SessionId) -> Result<PathBuf> {
        let path = self.session_root(session).join("workspace");
        fs::create_dir_all(&path)?;
        self.store
            .set_session_workspace(session, "session", Some(&path), None, now_ms())?;
        Ok(path)
    }

    pub fn session_workspace(&self, session: &SessionId) -> Option<PathBuf> {
        self.store
            .get_session_workspace(session)
            .ok()
            .flatten()
            .and_then(|w| w.workspace_path)
    }

    /// Which roots this session may touch: its own workspace plus its project when one is set.
    pub fn allowed_roots(&self, session: &SessionId) -> Vec<PathBuf> {
        let mut roots = Vec::new();
        if let Some(ws) = self.session_workspace(session) {
            roots.push(ws);
        } else {
            roots.push(self.session_root(session).join("workspace"));
        }
        if let Ok(Some(project)) = self.active_project(session) {
            roots.push(project.path);
        }
        roots
    }

    /// Resolve a session-relative path against the session's active root (project when set,
    /// otherwise the session workspace), and verify the result stays inside the allowed roots.
    pub fn resolve(&self, session: &SessionId, path: &Path) -> Result<PathBuf> {
        let roots = self.allowed_roots(session);
        let resolved = if path.is_absolute() {
            path.to_path_buf()
        } else {
            roots
                .first()
                .cloned()
                .unwrap_or_else(|| PathBuf::from("."))
                .join(path)
        };
        self.verify_within(session, &resolved)?;
        Ok(resolved)
    }

    /// True when `path` is inside one of the session's allowed roots. Symlinks are resolved first,
    /// so a link pointing outside the boundary is not an escape hatch.
    pub fn is_within(&self, session: &SessionId, path: &Path) -> bool {
        self.verify_within(session, path).is_ok()
    }

    fn verify_within(&self, session: &SessionId, path: &Path) -> Result<()> {
        let roots = self.allowed_roots(session);
        let canon = canonicalize_or_parent(path);
        for root in &roots {
            let root_canon = canonicalize_or_parent(root);
            if canon.starts_with(&root_canon) {
                return Ok(());
            }
        }
        Err(RuntimeError::Denied(format!(
            "path `{}` is outside session {}'s workspace boundary",
            path.display(),
            session.as_str()
        )))
    }

    // ---- projects --------------------------------------------------------

    pub fn select_project(&self, session: &SessionId, path: &Path) -> Result<ProjectRow> {
        let canon = fs::canonicalize(path)
            .map_err(|e| RuntimeError::Io(format!("project path unreadable: {e}")))?;
        if !canon.is_dir() {
            return Err(RuntimeError::InvalidState(format!(
                "project path is not a directory: {}",
                canon.display()
            )));
        }
        let now = now_ms();
        let project = match self.store.find_project_by_path(&canon)? {
            Some(mut row) => {
                row.status = "active".to_string();
                self.store.set_project_status(&row.id, "active", now)?;
                row
            }
            None => {
                let name = canon
                    .file_name()
                    .map(|n| n.to_string_lossy().to_string())
                    .unwrap_or_else(|| "project".to_string());
                self.store.create_project(&name, &canon, now)?
            }
        };
        self.store
            .set_session_project(session, Some(&project.id), now)?;
        self.store
            .set_session_workspace(session, "project", None, Some(&project.id), now)?;
        Ok(project)
    }

    pub fn disconnect_project(&self, session: &SessionId) -> Result<()> {
        let now = now_ms();
        self.store.set_session_project(session, None, now)?;
        self.store
            .set_session_workspace(session, "session", None, None, now)?;
        self.ensure_session_workspace(session)?;
        Ok(())
    }

    pub fn active_project(&self, session: &SessionId) -> Result<Option<ProjectRow>> {
        let row = self
            .store
            .get_session(session)?
            .ok_or_else(|| RuntimeError::SessionNotFound(session.clone()))?;
        match row.project_id {
            Some(id) => Ok(self.store.get_project(&id)?),
            None => Ok(None),
        }
    }

    pub fn list_projects(&self) -> Result<Vec<ProjectRow>> {
        Ok(self.store.list_projects()?)
    }

    /// Mark projects whose directories disappeared as `missing` (never deleted).
    pub fn refresh_project_status(&self) -> Result<usize> {
        let now = now_ms();
        let mut changed = 0usize;
        for p in self.store.list_projects()? {
            let exists = p.path.is_dir();
            let status = if exists { "active" } else { "missing" };
            if status != p.status {
                self.store.set_project_status(&p.id, status, now)?;
                changed += 1;
            }
        }
        Ok(changed)
    }

    // ---- artifacts -------------------------------------------------------

    pub fn register_artifact(
        &self,
        session: &SessionId,
        agent: Option<&AgentId>,
        path: &Path,
        source: Option<&str>,
    ) -> Result<ArtifactRow> {
        let now = now_ms();
        let canon = canonicalize_or_parent(path);
        let meta = fs::metadata(&canon);
        let size = meta.as_ref().map(|m| m.len()).unwrap_or(0);
        let checksum = meta
            .as_ref()
            .ok()
            .filter(|m| m.is_file() && m.len() <= CHECKPOINT_MAX_BYTES)
            .and_then(|_| checksum_of_file(&canon).ok());
        let kind = classify_file(&canon).to_string();
        let mime = guess_mime(&canon);
        let row = ArtifactRow {
            id: format!("art-{now}-{}", short_random()),
            session: session.clone(),
            project_id: self.active_project(session).ok().flatten().map(|p| p.id),
            agent: agent.cloned(),
            name: canon
                .file_name()
                .map(|n| n.to_string_lossy().to_string())
                .unwrap_or_default(),
            path: canon.clone(),
            kind,
            mime,
            size,
            checksum,
            source: source.map(str::to_owned),
            created_ms: now,
            updated_ms: now,
        };
        self.store.register_artifact(&row)?;
        Ok(row)
    }

    pub fn list_artifacts(&self, session: Option<&SessionId>) -> Result<Vec<ArtifactRow>> {
        Ok(self.store.list_artifacts(session)?)
    }

    /// Drop an artifact record only. The file itself is never deleted by this call.
    pub fn remove_artifact(&self, id: &str) -> Result<bool> {
        Ok(self.store.remove_artifact(id)?)
    }

    // ---- file changes & checkpoints --------------------------------------

    /// Record a change and take a checkpoint of the original content when it existed.
    /// `before` is the content before the operation, when the caller has it.
    pub fn record_file_change(
        &self,
        session: &SessionId,
        agent: Option<&AgentId>,
        path: &Path,
        operation: &str,
        before: Option<&[u8]>,
    ) -> Result<FileChangeRow> {
        let now = now_ms();
        let canon = canonicalize_or_parent(path);
        let (checkpoint_path, checksum_before) = match before {
            Some(bytes) if !bytes.is_empty() && bytes.len() as u64 <= CHECKPOINT_MAX_BYTES => {
                let cp = self.checkpoint_dir(session)?.join(format!(
                    "{}-{}",
                    now,
                    canon
                        .file_name()
                        .map(|n| n.to_string_lossy().to_string())
                        .unwrap_or_else(|| "file".into())
                ));
                fs::write(&cp, bytes)?;
                (Some(cp), Some(checksum_bytes(bytes)))
            }
            _ => (None, None),
        };
        let checksum_after = if operation == "delete" {
            None
        } else {
            fs::metadata(&canon)
                .ok()
                .filter(|m| m.is_file())
                .and_then(|_| checksum_of_file(&canon).ok())
        };
        let size = fs::metadata(&canon).ok().map(|m| m.len());
        let row = FileChangeRow {
            id: format!("chg-{now}-{}", short_random()),
            session: session.clone(),
            agent: agent.cloned(),
            path: canon.clone(),
            operation: operation.to_string(),
            checksum_before,
            checksum_after,
            size,
            checkpoint_path,
            ts_ms: now,
        };
        self.store.record_file_change(&row)?;
        Ok(row)
    }

    pub fn file_changes(&self, session: &SessionId) -> Result<Vec<FileChangeRow>> {
        Ok(self.store.list_file_changes(session)?)
    }

    /// Restore the most recent checkpointed content for `path` inside this session.
    pub fn restore_file(&self, session: &SessionId, path: &Path) -> Result<()> {
        let canon = canonicalize_or_parent(path);
        let changes = self.store.list_file_changes(session)?;
        let target = changes
            .iter()
            .rev()
            .find(|c| c.path == canon && c.checkpoint_path.is_some())
            .ok_or_else(|| {
                RuntimeError::InvalidState(format!("no checkpoint for {}", canon.display()))
            })?;
        let cp = target.checkpoint_path.as_ref().unwrap();
        let bytes = fs::read(cp)?;
        if let Some(parent) = canon.parent() {
            fs::create_dir_all(parent)?;
        }
        let mut f = fs::File::create(&canon)?;
        f.write_all(&bytes)?;
        Ok(())
    }

    fn checkpoint_dir(&self, session: &SessionId) -> Result<PathBuf> {
        let dir = self.session_root(session).join("checkpoints");
        fs::create_dir_all(&dir)?;
        Ok(dir)
    }
}

fn canonicalize_or_parent(path: &Path) -> PathBuf {
    match fs::canonicalize(path) {
        Ok(p) => p,
        Err(_) => {
            let parent = path
                .parent()
                .map(|p| p.to_path_buf())
                .unwrap_or_else(|| PathBuf::from("."));
            let base = fs::canonicalize(&parent).unwrap_or(parent);
            base.join(path.file_name().unwrap_or_default())
        }
    }
}

fn checksum_bytes(bytes: &[u8]) -> String {
    let mut h: u64 = 0xcbf29ce484222325;
    for b in bytes {
        h ^= *b as u64;
        h = h.wrapping_mul(0x100000001b3);
    }
    format!("{h:016x}")
}

fn checksum_of_file(path: &Path) -> std::io::Result<String> {
    let mut f = fs::File::open(path)?;
    let mut h: u64 = 0xcbf29ce484222325;
    let mut buf = [0u8; 65536];
    loop {
        let n = f.read(&mut buf)?;
        if n == 0 {
            break;
        }
        for b in &buf[..n] {
            h ^= *b as u64;
            h = h.wrapping_mul(0x100000001b3);
        }
    }
    Ok(format!("{h:016x}"))
}

fn short_random() -> String {
    use std::sync::atomic::{AtomicU64, Ordering};
    static C: AtomicU64 = AtomicU64::new(0);
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos() as u64)
        .unwrap_or(0);
    format!(
        "{:x}",
        nanos.wrapping_add(C.fetch_add(1, Ordering::Relaxed))
    )
}

fn classify_file(path: &Path) -> &'static str {
    match path.extension().and_then(|e| e.to_str()) {
        Some("png" | "jpg" | "jpeg" | "gif" | "webp" | "svg" | "bmp") => "image",
        Some("pdf") => "pdf",
        Some("md" | "txt" | "rst") => "document",
        Some("rs" | "py" | "js" | "ts" | "go" | "c" | "cpp" | "h" | "java" | "rb" | "sh") => {
            "source"
        }
        Some("json" | "yaml" | "yml" | "toml" | "csv") => "data",
        _ => "other",
    }
}

fn guess_mime(path: &Path) -> Option<String> {
    let ext = path.extension().and_then(|e| e.to_str())?.to_lowercase();
    let mime = match ext.as_str() {
        "png" => "image/png",
        "jpg" | "jpeg" => "image/jpeg",
        "gif" => "image/gif",
        "webp" => "image/webp",
        "svg" => "image/svg+xml",
        "pdf" => "application/pdf",
        "md" | "txt" => "text/plain",
        "json" => "application/json",
        "yaml" | "yml" => "text/yaml",
        "toml" => "text/toml",
        "html" | "htm" => "text/html",
        "css" => "text/css",
        "js" => "text/javascript",
        "ts" => "text/typescript",
        "rs" => "text/rust",
        "py" => "text/x-python",
        _ => return None,
    };
    Some(mime.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    #[test]
    fn classify_known_kinds() {
        assert_eq!(classify_file(Path::new("a.png")), "image");
        assert_eq!(classify_file(Path::new("a.pdf")), "pdf");
        assert_eq!(classify_file(Path::new("a.rs")), "source");
        assert_eq!(classify_file(Path::new("a.bin")), "other");
    }

    #[test]
    fn checksum_is_stable() {
        assert_eq!(checksum_bytes(b"hello"), checksum_bytes(b"hello"));
        assert_ne!(checksum_bytes(b"hello"), checksum_bytes(b"world"));
    }

    #[test]
    fn workspace_is_created_lazily_and_recorded() {
        let tmp = tempdir().unwrap();
        let store = Store::open_in_memory().unwrap();
        let mgr = WorkspaceManager::new(store.clone(), tmp.path().to_path_buf());
        let now = now_ms();
        let session = store.create_session("s", tmp.path(), None, now).unwrap();
        let ws = mgr.ensure_session_workspace(&session.id).unwrap();
        assert!(ws.is_dir());
        let info = mgr
            .store
            .get_session_workspace(&session.id)
            .unwrap()
            .unwrap();
        assert_eq!(info.mode, "session");
    }

    #[test]
    fn project_select_disconnect_round_trip() {
        let tmp = tempdir().unwrap();
        let store = Store::open_in_memory().unwrap();
        let mgr = WorkspaceManager::new(store.clone(), tmp.path().to_path_buf());
        let now = now_ms();
        let session = store.create_session("s", tmp.path(), None, now).unwrap();
        let proj_dir = tmp.path().join("myproj");
        fs::create_dir_all(&proj_dir).unwrap();
        let project = mgr.select_project(&session.id, &proj_dir).unwrap();
        assert_eq!(project.status, "active");
        assert_eq!(
            mgr.active_project(&session.id).unwrap().unwrap().id,
            project.id
        );
        mgr.disconnect_project(&session.id).unwrap();
        assert!(mgr.active_project(&session.id).unwrap().is_none());
    }

    #[test]
    fn boundary_blocks_escape_and_symlink() {
        let tmp = tempdir().unwrap();
        let store = Store::open_in_memory().unwrap();
        let mgr = WorkspaceManager::new(store.clone(), tmp.path().to_path_buf());
        let now = now_ms();
        let session = store.create_session("s", tmp.path(), None, now).unwrap();
        let ws = mgr.ensure_session_workspace(&session.id).unwrap();
        // inside is fine
        assert!(mgr.is_within(&session.id, &ws.join("calc.py")));
        // outside is refused
        assert!(!mgr.is_within(&session.id, &tmp.path().join("other").join("x.txt")));
        // symlink escape is refused
        #[cfg(unix)]
        {
            let outside = tmp.path().join("outside.txt");
            fs::write(&outside, "x").unwrap();
            let link = ws.join("link.txt");
            std::os::unix::fs::symlink(&outside, &link).unwrap();
            assert!(!mgr.is_within(&session.id, &link));
        }
    }

    #[test]
    fn artifact_registration_and_removal() {
        let tmp = tempdir().unwrap();
        let store = Store::open_in_memory().unwrap();
        let mgr = WorkspaceManager::new(store.clone(), tmp.path().to_path_buf());
        let now = now_ms();
        let session = store.create_session("s", tmp.path(), None, now).unwrap();
        let ws = mgr.ensure_session_workspace(&session.id).unwrap();
        let file = ws.join("chart.png");
        fs::write(&file, b"\x89PNG fake").unwrap();
        let art = mgr
            .register_artifact(&session.id, None, &file, Some("tool:image.generate"))
            .unwrap();
        assert_eq!(art.kind, "image");
        let listed = mgr.list_artifacts(Some(&session.id)).unwrap();
        assert_eq!(listed.len(), 1);
        assert!(mgr.remove_artifact(&art.id).unwrap());
        assert!(mgr.list_artifacts(Some(&session.id)).unwrap().is_empty());
        // the physical file survives metadata removal
        assert!(file.exists());
    }

    #[test]
    fn file_change_checkpoint_restore() {
        let tmp = tempdir().unwrap();
        let store = Store::open_in_memory().unwrap();
        let mgr = WorkspaceManager::new(store.clone(), tmp.path().to_path_buf());
        let now = now_ms();
        let session = store.create_session("s", tmp.path(), None, now).unwrap();
        let ws = mgr.ensure_session_workspace(&session.id).unwrap();
        let file = ws.join("notes.md");
        fs::write(&file, "v1").unwrap();
        mgr.record_file_change(&session.id, None, &file, "modify", Some(b"v1"))
            .unwrap();
        fs::write(&file, "v2 broken").unwrap();
        mgr.restore_file(&session.id, &file).unwrap();
        assert_eq!(fs::read_to_string(&file).unwrap(), "v1");
    }

    #[test]
    fn restore_without_checkpoint_errors() {
        let tmp = tempdir().unwrap();
        let store = Store::open_in_memory().unwrap();
        let mgr = WorkspaceManager::new(store.clone(), tmp.path().to_path_buf());
        let now = now_ms();
        let session = store.create_session("s", tmp.path(), None, now).unwrap();
        mgr.ensure_session_workspace(&session.id).unwrap();
        assert!(
            mgr.restore_file(&session.id, Path::new("nope.txt"))
                .is_err()
        );
    }
}
