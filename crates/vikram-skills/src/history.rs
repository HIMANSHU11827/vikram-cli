//! Install history: what a skill hashed to, where it came from, and how to go back.
//!
//! Hashing is deliberately boring: FNV-1a 64 over the sorted `(relative path, bytes)` pairs.
//! No dependency, no crypto claim — it answers "did this directory change?" for update and
//! rollback decisions, not "is this content authentic?".
//!
//! [`History::record`] takes `&self` and persists: two processes installing different skills
//! never share a mutable value, and the log on disk — not the in-memory copy — is what an
//! append reads first. `latest()` still answers from what was loaded, so call
//! [`History::load_in`] again to observe a record you just wrote.

use std::fs;
use std::path::{Component, Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::error::{Result, SkillError};
use crate::paths;
use crate::skill::SkillId;

/// How many manifests survive before the oldest is dropped.
pub const HISTORY_KEEP: usize = 20;

const HISTORY_FILE: &str = "history.json";
const SNAPSHOTS_DIR: &str = "snapshots";
const FNV_OFFSET: u64 = 0xcbf2_9ce4_8422_2325;
const FNV_PRIME: u64 = 0x0000_0100_0000_01b3;

/// One recorded state of one skill.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Manifest {
    pub name: SkillId,
    pub version: Option<String>,
    pub content_hash: String,
    pub source: Option<String>,
    pub rev: Option<String>,
    pub installed_ms: u64,
    /// `(relative path, FNV-1a hex of the bytes)`, sorted by path.
    pub files: Vec<(String, String)>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct History {
    pub name: SkillId,
    entries: Vec<Manifest>,
    /// The state directory this history belongs to. Private, so an isolated `Manager` never
    /// writes into the real `~/.vikram` behind the caller's back.
    state_dir: PathBuf,
}

impl History {
    /// `<state_dir>/history/<name>/history.json` under the real home.
    pub fn load(name: &SkillId) -> Result<History> {
        History::load_in(&paths::state_dir(), name)
    }

    /// Same, against an explicit state directory. A missing file is an empty history: a skill
    /// the user copied in by hand has no install record, and that is not an error.
    pub fn load_in(state_dir: &Path, name: &SkillId) -> Result<History> {
        let path = Self::history_path_in(state_dir, name);
        let empty = History {
            name: name.clone(),
            entries: Vec::new(),
            state_dir: state_dir.to_path_buf(),
        };
        if !path.exists() {
            return Ok(empty);
        }
        let text = fs::read_to_string(&path).map_err(|err| SkillError::io(&path, &err))?;
        let entries: Vec<Manifest> = serde_json::from_str(&text).map_err(|err| SkillError::Io {
            path: path.clone(),
            reason: format!("invalid history: {err}"),
        })?;
        Ok(History {
            name: name.clone(),
            entries,
            state_dir: state_dir.to_path_buf(),
        })
    }

    /// Oldest first. The last element is the newest state of the skill.
    pub fn entries(&self) -> &[Manifest] {
        &self.entries
    }

    /// Appends a manifest and persists, keeping the newest [`HISTORY_KEEP`] entries and
    /// dropping the snapshots that belong to the ones that fell off.
    ///
    /// The on-disk log is the source of truth, not `self`: two `Installer`s can record for the
    /// same skill, and the second must not overwrite the first's entry.
    pub fn record(&self, manifest: Manifest) -> Result<()> {
        let mut entries = Self::load_in(&self.state_dir, &self.name)?.entries;
        entries.push(manifest);
        let mut dropped: Vec<Manifest> = Vec::new();
        if entries.len() > HISTORY_KEEP {
            let excess = entries.len() - HISTORY_KEEP;
            dropped = entries.drain(..excess).collect();
        }
        self.write(&entries)?;
        for old in dropped {
            self.remove_snapshot(old.installed_ms);
        }
        Ok(())
    }

    pub fn latest(&self) -> Option<Manifest> {
        Self::load_in(&self.state_dir, &self.name)
            .ok()
            .and_then(|h| h.entries.into_iter().last())
    }

    /// The manifest before the newest one: what [`History::rollback`] would restore.
    pub fn previous(&self) -> Option<&Manifest> {
        self.entries.iter().rev().nth(1)
    }

    /// True when the directory no longer matches its newest recorded hash. A skill with no
    /// history has, by definition, not been modified since an install that never happened.
    pub fn detect_local_changes(&self, dir: &Path) -> Result<bool> {
        match self.latest() {
            Some(latest) => Ok(hash_dir(dir)? != latest.content_hash),
            None => Ok(false),
        }
    }

    /// Copies the current contents into this skill's snapshot area and returns the per-file
    /// hashes for a [`Manifest`]. The caller stamps the manifest; the snapshot is keyed by the
    /// same `at_ms` so rollback can find it again.
    pub fn snapshot(&self, dir: &Path, at_ms: u64) -> Result<Vec<(String, String)>> {
        if !dir.is_dir() {
            return Err(SkillError::Io {
                path: dir.to_path_buf(),
                reason: "not a directory".to_string(),
            });
        }
        let root = self.snapshot_root();
        fs::create_dir_all(&root).map_err(|err| SkillError::io(&root, &err))?;
        let target = free_snapshot_path(&root, at_ms);
        paths::copy_dir(dir, &target, &[])?;
        file_hashes(&target)
    }

    /// Restores the files from the previous manifest's snapshot.
    ///
    /// Refuses in two cases rather than guessing: there is no earlier snapshot, or the directory
    /// was edited by hand after the newest install. Restoring over local edits would silently
    /// throw away work that was never recorded.
    pub fn rollback(&self, dir: &Path) -> Result<Manifest> {
        let conflict = || SkillError::UpdateConflict {
            name: self.name.as_str().to_string(),
            path: dir.to_path_buf(),
        };
        let latest = self.latest().ok_or_else(conflict)?;
        let previous = self.previous().ok_or_else(conflict)?.clone();
        let snapshot = self
            .find_snapshot(previous.installed_ms)
            .ok_or_else(conflict)?;
        if hash_dir(dir)? != latest.content_hash {
            return Err(conflict());
        }
        for entry in paths::sorted_entries(dir)? {
            paths::remove_path(&entry);
        }
        paths::copy_dir(&snapshot, dir, &[])?;
        Ok(previous)
    }

    fn write(&self, entries: &[Manifest]) -> Result<()> {
        let path = self.history_path();
        let bytes = serde_json::to_vec_pretty(entries).map_err(|err| SkillError::Io {
            path: path.clone(),
            reason: err.to_string(),
        })?;
        paths::atomic_write(&path, &bytes)
    }

    fn history_path(&self) -> PathBuf {
        Self::history_path_in(&self.state_dir, &self.name)
    }

    fn snapshot_root(&self) -> PathBuf {
        self.state_dir
            .join("history")
            .join(self.name.as_str())
            .join(SNAPSHOTS_DIR)
    }

    fn find_snapshot(&self, at_ms: u64) -> Option<PathBuf> {
        let root = self.snapshot_root();
        let exact = at_ms.to_string();
        let prefixed = format!("{exact}-");
        paths::sorted_entries(&root).ok()?.into_iter().find(|path| {
            path.file_name()
                .map(|name| {
                    let name = name.to_string_lossy();
                    name == exact.as_str() || name.starts_with(&prefixed)
                })
                .unwrap_or(false)
        })
    }

    fn remove_snapshot(&self, at_ms: u64) {
        if let Some(path) = self.find_snapshot(at_ms) {
            paths::remove_path(&path);
        }
    }

    fn history_path_in(state_dir: &Path, name: &SkillId) -> PathBuf {
        state_dir
            .join("history")
            .join(name.as_str())
            .join(HISTORY_FILE)
    }
}

/// FNV-1a 64 over the sorted `(relative path, bytes)` pairs, as 16 lowercase hex digits.
///
/// A path separator byte and a terminator are mixed in so that `a` + `b` and `ab` + `` cannot
/// collide. Empty directories are not represented: the hash covers content, not scaffolding.
pub fn hash_dir(dir: &Path) -> Result<String> {
    if !dir.is_dir() {
        return Err(SkillError::Io {
            path: dir.to_path_buf(),
            reason: "not a directory".to_string(),
        });
    }
    let mut files = Vec::new();
    collect_files(dir, dir, &mut files)?;
    files.sort_by(|a, b| a.0.cmp(&b.0));
    let mut hash = FNV_OFFSET;
    for (rel, path) in &files {
        let bytes = fs::read(path).map_err(|err| SkillError::io(path, &err))?;
        hash = fnv1a(hash, rel.as_bytes());
        hash = fnv1a(hash, &[0]);
        hash = fnv1a(hash, &bytes);
        hash = fnv1a(hash, &[0xff]);
    }
    Ok(format!("{hash:016x}"))
}

fn fnv1a(mut hash: u64, bytes: &[u8]) -> u64 {
    for byte in bytes {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(FNV_PRIME);
    }
    hash
}

fn file_hashes(root: &Path) -> Result<Vec<(String, String)>> {
    let mut files = Vec::new();
    collect_files(root, root, &mut files)?;
    let mut out = Vec::new();
    for (rel, path) in files {
        let bytes = fs::read(&path).map_err(|err| SkillError::io(&path, &err))?;
        out.push((rel, format!("{:016x}", fnv1a(FNV_OFFSET, &bytes))));
    }
    out.sort();
    Ok(out)
}

/// Every regular file below `dir`, relative to `root`, `/`-separated. Symlinks are refused:
/// hashing a directory is a claim about what is in it, and a link points somewhere else.
fn collect_files(root: &Path, dir: &Path, out: &mut Vec<(String, PathBuf)>) -> Result<()> {
    for path in paths::sorted_entries(dir)? {
        let meta = fs::symlink_metadata(&path).map_err(|err| SkillError::io(&path, &err))?;
        let kind = meta.file_type();
        if kind.is_symlink() {
            return Err(SkillError::Denied {
                reason: format!("refusing to hash symlink {}", path.display()),
            });
        }
        if kind.is_dir() {
            collect_files(root, &path, out)?;
        } else if kind.is_file() {
            out.push((relative_slash(root, &path), path));
        }
    }
    Ok(())
}

fn relative_slash(root: &Path, path: &Path) -> String {
    let rel = path.strip_prefix(root).unwrap_or(path);
    let mut parts: Vec<String> = Vec::new();
    for component in rel.components() {
        if let Component::Normal(part) = component {
            parts.push(part.to_string_lossy().into_owned());
        }
    }
    parts.join("/")
}

fn free_snapshot_path(root: &Path, at_ms: u64) -> PathBuf {
    let exact = root.join(at_ms.to_string());
    if !exact.exists() {
        return exact;
    }
    for n in 1..1000u32 {
        let candidate = root.join(format!("{at_ms}-{n}"));
        if !candidate.exists() {
            return candidate;
        }
    }
    root.join(format!("{at_ms}-overflow"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn id(name: &str) -> SkillId {
        SkillId::new(name).expect("valid id")
    }

    fn manifest(name: &str, hash: &str, at: u64) -> Manifest {
        Manifest {
            name: id(name),
            version: Some("1.0.0".to_string()),
            content_hash: hash.to_string(),
            source: Some("https://example.test/repo".to_string()),
            rev: Some("abc123".to_string()),
            installed_ms: at,
            files: vec![("SKILL.md".to_string(), hash.to_string())],
        }
    }

    fn write_skill(dir: &Path, body: &str) {
        fs::create_dir_all(dir).expect("dirs");
        fs::write(dir.join("SKILL.md"), body).expect("write");
    }

    #[test]
    fn hashing_is_deterministic_and_ignores_creation_order() {
        let tmp = tempfile::tempdir().expect("temp");
        let one = tmp.path().join("one");
        fs::create_dir_all(one.join("references")).expect("dirs");
        fs::write(one.join("SKILL.md"), "body").expect("write");
        fs::write(one.join("references/a.md"), "a").expect("write");

        let two = tmp.path().join("two");
        fs::create_dir_all(two.join("references")).expect("dirs");
        fs::write(two.join("references/a.md"), "a").expect("write");
        fs::write(two.join("SKILL.md"), "body").expect("write");

        assert_eq!(hash_dir(&one).expect("h1"), hash_dir(&two).expect("h2"));
    }

    #[test]
    fn hashing_changes_when_content_changes() {
        let tmp = tempfile::tempdir().expect("temp");
        let dir = tmp.path().join("skill");
        write_skill(&dir, "one");
        let first = hash_dir(&dir).expect("hash");
        write_skill(&dir, "two");
        let second = hash_dir(&dir).expect("hash");
        assert_ne!(first, second);
        assert_eq!(first.len(), 16);
        assert!(first.chars().all(|c| c.is_ascii_hexdigit()));
    }

    #[test]
    fn hashing_separates_adjacent_paths() {
        let tmp = tempfile::tempdir().expect("temp");
        let a = tmp.path().join("a");
        fs::create_dir_all(&a).expect("dirs");
        fs::write(a.join("x"), "yz").expect("write");

        let b = tmp.path().join("b");
        fs::create_dir_all(&b).expect("dirs");
        fs::write(b.join("xy"), "z").expect("write");

        assert_ne!(hash_dir(&a).expect("a"), hash_dir(&b).expect("b"));
    }

    #[test]
    fn hashing_a_missing_directory_is_an_io_error_with_the_path() {
        let tmp = tempfile::tempdir().expect("temp");
        let missing = tmp.path().join("nope");
        let err = hash_dir(&missing).expect_err("missing");
        match err {
            SkillError::Io { path, .. } => assert_eq!(path, missing),
            other => panic!("unexpected {other:?}"),
        }
    }

    #[cfg(unix)]
    #[test]
    fn hashing_refuses_symlinks() {
        let tmp = tempfile::tempdir().expect("temp");
        let dir = tmp.path().join("skill");
        fs::create_dir_all(&dir).expect("dirs");
        std::os::unix::fs::symlink("/etc/passwd", dir.join("passwd")).expect("symlink");
        let err = hash_dir(&dir).expect_err("symlink");
        assert!(matches!(err, SkillError::Denied { .. }));
    }

    #[test]
    fn a_missing_history_file_loads_empty() {
        let tmp = tempfile::tempdir().expect("temp");
        let history = History::load_in(tmp.path(), &id("alpha")).expect("load");
        assert!(history.entries().is_empty());
        assert!(history.latest().is_none());
        assert!(history.previous().is_none());
    }

    #[test]
    fn recording_persists_and_keeps_the_newest_twenty() {
        let tmp = tempfile::tempdir().expect("temp");
        let history = History::load_in(tmp.path(), &id("alpha")).expect("load");
        for i in 0..25u64 {
            history
                .record(manifest("alpha", &format!("h{i}"), i))
                .expect("record");
        }
        let reloaded = History::load_in(tmp.path(), &id("alpha")).expect("reload");
        assert_eq!(reloaded.entries().len(), HISTORY_KEEP);
        assert_eq!(reloaded.entries()[0].installed_ms, 5);
        assert_eq!(reloaded.latest().map(|m| m.installed_ms), Some(24));
        assert_eq!(reloaded.previous().map(|m| m.installed_ms), Some(23));
    }

    #[test]
    fn history_round_trips_through_json() {
        let tmp = tempfile::tempdir().expect("temp");
        let history = History::load_in(tmp.path(), &id("alpha")).expect("load");
        let entry = manifest("alpha", "deadbeef", 7);
        history.record(entry.clone()).expect("record");
        let reloaded = History::load_in(tmp.path(), &id("alpha")).expect("reload");
        assert_eq!(reloaded.latest(), Some(entry));
    }

    #[test]
    fn a_corrupt_history_file_is_reported_with_its_path() {
        let tmp = tempfile::tempdir().expect("temp");
        let dir = tmp.path().join("history/alpha");
        fs::create_dir_all(&dir).expect("dirs");
        fs::write(dir.join(HISTORY_FILE), "{not json").expect("write");
        let err = History::load_in(tmp.path(), &id("alpha")).expect_err("corrupt");
        match err {
            SkillError::Io { path, reason } => {
                assert!(path.ends_with("history/alpha/history.json"));
                assert!(reason.contains("invalid history"), "{reason}");
            }
            other => panic!("unexpected {other:?}"),
        }
    }

    #[test]
    fn local_changes_are_detected_against_the_latest_hash() {
        let tmp = tempfile::tempdir().expect("temp");
        let dir = tmp.path().join("skills/alpha");
        write_skill(&dir, "v1");
        let history = History::load_in(tmp.path(), &id("alpha")).expect("load");
        let files = history.snapshot(&dir, 1_000).expect("snapshot");
        history
            .record(Manifest {
                name: id("alpha"),
                version: None,
                content_hash: hash_dir(&dir).expect("hash"),
                source: None,
                rev: None,
                installed_ms: 1_000,
                files,
            })
            .expect("record");

        let history = History::load_in(tmp.path(), &id("alpha")).expect("reload");
        assert!(!history.detect_local_changes(&dir).expect("clean"));

        write_skill(&dir, "v2");
        assert!(history.detect_local_changes(&dir).expect("dirty"));
    }

    #[test]
    fn a_skill_without_history_is_not_locally_modified() {
        let tmp = tempfile::tempdir().expect("temp");
        let dir = tmp.path().join("alpha");
        write_skill(&dir, "body");
        let history = History::load_in(tmp.path(), &id("alpha")).expect("load");
        assert!(!history.detect_local_changes(&dir).expect("no history"));
    }

    #[test]
    fn snapshot_copies_the_tree_and_reports_per_file_hashes() {
        let tmp = tempfile::tempdir().expect("temp");
        let dir = tmp.path().join("skills/alpha");
        write_skill(&dir, "v1");
        fs::create_dir_all(dir.join("references")).expect("dirs");
        fs::write(dir.join("references/api.md"), "docs").expect("write");

        let history = History::load_in(tmp.path(), &id("alpha")).expect("load");
        let files = history.snapshot(&dir, 1_000).expect("snapshot");
        let names: Vec<&str> = files.iter().map(|(rel, _)| rel.as_str()).collect();
        assert_eq!(names, vec!["SKILL.md", "references/api.md"]);

        let snapshot = history.find_snapshot(1_000).expect("snapshot dir");
        assert_eq!(
            fs::read_to_string(snapshot.join("references/api.md")).expect("read"),
            "docs"
        );
    }

    #[test]
    fn rollback_restores_the_previous_snapshot() {
        let tmp = tempfile::tempdir().expect("temp");
        let dir = tmp.path().join("skills/alpha");
        write_skill(&dir, "v1");
        let history = History::load_in(tmp.path(), &id("alpha")).expect("load");
        let files = history.snapshot(&dir, 1_000).expect("snapshot");
        let first_hash = hash_dir(&dir).expect("hash");
        history
            .record(Manifest {
                name: id("alpha"),
                version: None,
                content_hash: first_hash.clone(),
                source: None,
                rev: None,
                installed_ms: 1_000,
                files,
            })
            .expect("record");

        write_skill(&dir, "v2");
        let files = history.snapshot(&dir, 2_000).expect("snapshot");
        history
            .record(Manifest {
                name: id("alpha"),
                version: None,
                content_hash: hash_dir(&dir).expect("hash"),
                source: None,
                rev: None,
                installed_ms: 2_000,
                files,
            })
            .expect("record");

        let history = History::load_in(tmp.path(), &id("alpha")).expect("reload");
        let restored = history.rollback(&dir).expect("rollback");
        assert_eq!(restored.installed_ms, 1_000);
        assert_eq!(
            fs::read_to_string(dir.join("SKILL.md")).expect("read"),
            "v1"
        );
        assert_eq!(hash_dir(&dir).expect("hash"), first_hash);
    }

    #[test]
    fn rollback_without_a_previous_snapshot_is_a_conflict() {
        let tmp = tempfile::tempdir().expect("temp");
        let dir = tmp.path().join("skills/alpha");
        write_skill(&dir, "v1");
        let history = History::load_in(tmp.path(), &id("alpha")).expect("load");
        let files = history.snapshot(&dir, 1_000).expect("snapshot");
        history
            .record(Manifest {
                name: id("alpha"),
                version: None,
                content_hash: hash_dir(&dir).expect("hash"),
                source: None,
                rev: None,
                installed_ms: 1_000,
                files,
            })
            .expect("record");

        let history = History::load_in(tmp.path(), &id("alpha")).expect("reload");
        let err = history.rollback(&dir).expect_err("nothing to go back to");
        assert!(matches!(err, SkillError::UpdateConflict { .. }));
        assert_eq!(
            fs::read_to_string(dir.join("SKILL.md")).expect("read"),
            "v1"
        );
    }

    #[test]
    fn rollback_refuses_to_clobber_local_edits() {
        let tmp = tempfile::tempdir().expect("temp");
        let dir = tmp.path().join("skills/alpha");
        write_skill(&dir, "v1");
        let history = History::load_in(tmp.path(), &id("alpha")).expect("load");
        for (at, body) in [(1_000u64, "v1"), (2_000, "v2")] {
            write_skill(&dir, body);
            let files = history.snapshot(&dir, at).expect("snapshot");
            history
                .record(Manifest {
                    name: id("alpha"),
                    version: None,
                    content_hash: hash_dir(&dir).expect("hash"),
                    source: None,
                    rev: None,
                    installed_ms: at,
                    files,
                })
                .expect("record");
        }
        write_skill(&dir, "edited by hand");

        let history = History::load_in(tmp.path(), &id("alpha")).expect("reload");
        let err = history.rollback(&dir).expect_err("local edits");
        assert!(matches!(err, SkillError::UpdateConflict { .. }));
        assert_eq!(
            fs::read_to_string(dir.join("SKILL.md")).expect("read"),
            "edited by hand"
        );
    }

    #[test]
    fn two_snapshots_in_the_same_millisecond_do_not_collide() {
        let tmp = tempfile::tempdir().expect("temp");
        let dir = tmp.path().join("alpha");
        write_skill(&dir, "v1");
        let history = History::load_in(tmp.path(), &id("alpha")).expect("load");
        history.snapshot(&dir, 5).expect("first");
        history.snapshot(&dir, 5).expect("second");
        let root = history.snapshot_root();
        let names: Vec<String> = paths::sorted_entries(&root)
            .expect("entries")
            .into_iter()
            .map(|p| {
                p.file_name()
                    .unwrap_or_default()
                    .to_string_lossy()
                    .into_owned()
            })
            .collect();
        assert_eq!(names, vec!["5".to_string(), "5-1".to_string()]);
        assert!(history.find_snapshot(5).is_some());
    }
}
