//! Filesystem layout and the path-safety primitives every other module goes through.
//!
//! Two rules live here because breaking either is a security bug, not a style choice:
//! a relative path handed to us may never leave its root, and a mutation is either applied
//! completely or not at all (write a sibling, then rename).

use std::fs;
use std::path::{Component, Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use crate::error::{Result, SkillError};
use crate::skill::{Compat, Scope};

pub const HOME_ENV: &str = "VIKRAM_HOME";

/// `$VIKRAM_HOME`, else `$HOME/.vikram`, else `.vikram` under the current directory.
pub fn home() -> PathBuf {
    if let Some(value) = std::env::var_os(HOME_ENV) {
        let path = PathBuf::from(value);
        if !path.as_os_str().is_empty() {
            return path;
        }
    }
    if let Some(value) = std::env::var_os("HOME") {
        let path = PathBuf::from(value);
        if !path.as_os_str().is_empty() {
            return path.join(".vikram");
        }
    }
    // Windows fallback: USERPROFILE (mirrors vikram-runtime::config::vikram_home).
    if let Some(value) = std::env::var_os("USERPROFILE") {
        let path = PathBuf::from(value);
        if !path.as_os_str().is_empty() {
            return path.join(".vikram");
        }
    }
    PathBuf::from(".vikram")
}

pub fn user_skills_dir() -> PathBuf {
    home().join("skills")
}

pub fn bundles_dir() -> PathBuf {
    home().join("bundles")
}

pub fn state_dir() -> PathBuf {
    user_skills_dir().join(".vikram")
}

pub fn disabled_path() -> PathBuf {
    state_dir().join("disabled.json")
}

pub fn history_dir(name: &str) -> PathBuf {
    state_dir().join("history").join(name)
}

pub fn provenance_path() -> PathBuf {
    state_dir().join("provenance.json")
}

/// User-level roots in preference order. Compatibility directories are only scanned when the
/// caller asks for them; when it does not, their contents are not silently adopted either.
pub fn user_skill_dirs() -> Vec<(Scope, Compat, PathBuf)> {
    let root = user_skills_dir();
    vec![
        (Scope::User, Compat::Native, root.clone()),
        (Scope::User, Compat::AgentsSkills, root.join("agents")),
        (Scope::User, Compat::Claude, root.join("claude")),
        (Scope::User, Compat::OpenCode, root.join("opencode")),
        (Scope::User, Compat::Hermes, root.join("hermes")),
    ]
}

/// Project-level roots in preference order.
pub fn project_skill_dirs(workspace: &Path) -> Vec<(Scope, Compat, PathBuf)> {
    vec![
        (
            Scope::Project,
            Compat::Native,
            workspace.join(".vikram/skills"),
        ),
        (Scope::Project, Compat::Native, workspace.join("skills")),
        (
            Scope::Project,
            Compat::AgentsSkills,
            workspace.join(".agents/skills"),
        ),
        (
            Scope::Project,
            Compat::Claude,
            workspace.join(".claude/skills"),
        ),
        (
            Scope::Project,
            Compat::OpenCode,
            workspace.join(".opencode/skills"),
        ),
        (
            Scope::Project,
            Compat::Hermes,
            workspace.join(".hermes/skills"),
        ),
    ]
}

/// Directory names that are never descended into. `.vikram` is our own state directory, which
/// lives under the user skills root and must not be read back as skills.
pub fn default_excludes() -> Vec<String> {
    [
        ".git",
        ".hg",
        ".svn",
        "node_modules",
        "target",
        "__pycache__",
        ".venv",
        "venv",
        ".vikram",
        "trash",
        "staging",
    ]
    .iter()
    .map(|s| s.to_string())
    .collect()
}

/// Subdirectories of the user skills root that are separate sources in their own right, so the
/// parent scan must not claim their children at the native compat level.
pub fn compat_subdirs() -> Vec<String> {
    ["agents", "claude", "opencode", "hermes"]
        .iter()
        .map(|s| s.to_string())
        .collect()
}

/// Resolves `rel` under `root`, refusing anything that could leave it.
pub fn resolve_within(root: &Path, rel: &str) -> Result<PathBuf> {
    let escape = || SkillError::PathEscape {
        path: root.join(rel),
    };
    if rel.is_empty() {
        return Err(escape());
    }
    if rel.contains('\0') || rel.contains('\\') {
        return Err(escape());
    }
    let requested = Path::new(rel);
    if requested.is_absolute() {
        return Err(escape());
    }
    let mut out = root.to_path_buf();
    for component in requested.components() {
        match component {
            Component::Normal(part) => out.push(part),
            Component::CurDir => {}
            _ => return Err(escape()),
        }
    }
    if !out.starts_with(root) || out == root {
        return Err(escape());
    }
    Ok(out)
}

/// [`resolve_within`] plus a symlink walk. A symlink is a way to read something outside the
/// skill, so it is refused unless the caller opted in.
pub fn resolve_checked(root: &Path, rel: &str, follow_symlinks: bool) -> Result<PathBuf> {
    let path = resolve_within(root, rel)?;
    if follow_symlinks {
        return Ok(path);
    }
    let mut cursor = root.to_path_buf();
    for component in Path::new(rel).components() {
        let Component::Normal(part) = component else {
            continue;
        };
        cursor.push(part);
        match fs::symlink_metadata(&cursor) {
            Ok(meta) if meta.file_type().is_symlink() => {
                return Err(SkillError::Denied {
                    reason: format!("refusing to follow symlink {}", cursor.display()),
                });
            }
            Ok(_) => {}
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(path),
            Err(err) => return Err(SkillError::io(&cursor, &err)),
        }
    }
    Ok(path)
}

pub(crate) fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

/// A collision-free sibling path. Never used as a final location: it is the staging name that
/// gets renamed into place.
pub(crate) fn unique_path(parent: &Path, prefix: &str) -> PathBuf {
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let n = COUNTER.fetch_add(1, Ordering::Relaxed);
    parent.join(format!(
        ".{prefix}.vikram-tmp-{}-{}-{}",
        now_ms(),
        std::process::id(),
        n
    ))
}

/// Writes `bytes` to `path` via a sibling temp file and a rename.
pub(crate) fn atomic_write(path: &Path, bytes: &[u8]) -> Result<()> {
    let parent = path.parent().ok_or_else(|| SkillError::Io {
        path: path.to_path_buf(),
        reason: "path has no parent directory".to_string(),
    })?;
    fs::create_dir_all(parent).map_err(|e| SkillError::io(parent, &e))?;
    let stem = path
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| "file".to_string());
    let tmp = unique_path(parent, &stem);
    if let Err(err) = fs::write(&tmp, bytes) {
        let _ = fs::remove_file(&tmp);
        return Err(SkillError::io(&tmp, &err));
    }
    if let Err(err) = fs::rename(&tmp, path) {
        let _ = fs::remove_file(&tmp);
        return Err(SkillError::io(path, &err));
    }
    Ok(())
}

pub(crate) fn file_name(path: &Path) -> Result<String> {
    path.file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .ok_or_else(|| SkillError::Io {
            path: path.to_path_buf(),
            reason: "path has no file name".to_string(),
        })
}

pub(crate) fn sorted_entries(dir: &Path) -> Result<Vec<PathBuf>> {
    let reader = fs::read_dir(dir).map_err(|e| SkillError::io(dir, &e))?;
    let mut out = Vec::new();
    for entry in reader {
        let entry = entry.map_err(|e| SkillError::io(dir, &e))?;
        out.push(entry.path());
    }
    out.sort();
    Ok(out)
}

/// Recursive copy that never follows a symlink and skips excluded directory names.
pub(crate) fn copy_dir(src: &Path, dst: &Path, exclude: &[String]) -> Result<()> {
    fs::create_dir_all(dst).map_err(|e| SkillError::io(dst, &e))?;
    for path in sorted_entries(src)? {
        let name = file_name(&path)?;
        if exclude.iter().any(|x| x == &name) {
            continue;
        }
        let meta = fs::symlink_metadata(&path).map_err(|e| SkillError::io(&path, &e))?;
        let kind = meta.file_type();
        if kind.is_symlink() {
            return Err(SkillError::Denied {
                reason: format!("refusing to copy symlink {}", path.display()),
            });
        }
        let target = dst.join(&name);
        if kind.is_dir() {
            copy_dir(&path, &target, exclude)?;
        } else if kind.is_file() {
            fs::copy(&path, &target).map_err(|e| SkillError::io(&path, &e))?;
        }
    }
    Ok(())
}

/// Rename, falling back to copy+remove when the two paths live on different filesystems.
pub(crate) fn move_dir(src: &Path, dst: &Path) -> Result<()> {
    if fs::rename(src, dst).is_ok() {
        return Ok(());
    }
    copy_dir(src, dst, &[])?;
    fs::remove_dir_all(src).map_err(|e| SkillError::io(src, &e))
}

pub(crate) fn remove_path(path: &Path) {
    if fs::symlink_metadata(path).is_err() {
        return;
    }
    if path.is_dir() {
        let _ = fs::remove_dir_all(path);
    } else {
        let _ = fs::remove_file(path);
    }
}

pub(crate) fn read_json<T: serde::de::DeserializeOwned>(path: &Path) -> Option<T> {
    let text = fs::read_to_string(path).ok()?;
    serde_json::from_str(&text).ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn home_and_the_derived_directories_nest_correctly() {
        let h = home();
        assert_eq!(user_skills_dir(), h.join("skills"));
        assert_eq!(state_dir(), h.join("skills").join(".vikram"));
        assert_eq!(disabled_path(), state_dir().join("disabled.json"));
        assert_eq!(provenance_path(), state_dir().join("provenance.json"));
        assert_eq!(
            history_dir("alpha"),
            state_dir().join("history").join("alpha")
        );
    }

    #[test]
    fn project_roots_follow_the_documented_preference_order() {
        let ws = Path::new("/ws");
        let dirs = project_skill_dirs(ws);
        let names: Vec<String> = dirs
            .iter()
            .map(|(_, _, p)| p.display().to_string())
            .collect();
        assert_eq!(
            names,
            vec![
                "/ws/.vikram/skills",
                "/ws/skills",
                "/ws/.agents/skills",
                "/ws/.claude/skills",
                "/ws/.opencode/skills",
                "/ws/.hermes/skills",
            ]
        );
        assert_eq!(dirs[0].1, Compat::Native);
        assert_eq!(dirs[2].1, Compat::AgentsSkills);
        assert_eq!(dirs[3].1, Compat::Claude);
        assert_eq!(dirs[5].1, Compat::Hermes);
        assert!(dirs.iter().all(|(scope, _, _)| *scope == Scope::Project));
    }

    #[test]
    fn user_roots_include_the_compat_subdirectories() {
        let dirs = user_skill_dirs();
        assert_eq!(dirs.len(), 5);
        assert_eq!(dirs[0].1, Compat::Native);
        assert_eq!(dirs[1].2, user_skills_dir().join("agents"));
        assert_eq!(dirs[4].2, user_skills_dir().join("hermes"));
    }

    #[test]
    fn resolve_within_accepts_nested_relative_paths() {
        let root = Path::new("/skills/alpha");
        assert_eq!(
            resolve_within(root, "references/api.md").expect("ok"),
            root.join("references/api.md")
        );
        assert_eq!(
            resolve_within(root, "./a/./b.txt").expect("ok"),
            root.join("a/b.txt")
        );
    }

    #[test]
    fn resolve_within_rejects_escapes() {
        let root = Path::new("/skills/alpha");
        for rel in [
            "",
            "../etc/passwd",
            "a/../../etc/passwd",
            "/etc/passwd",
            "..",
            ".",
            "a\\b",
        ] {
            let err = resolve_within(root, rel).expect_err(rel);
            assert!(matches!(err, SkillError::PathEscape { .. }), "{rel}");
        }
    }

    #[cfg(unix)]
    #[test]
    fn resolve_checked_refuses_symlinks_unless_asked() {
        let dir = tempfile::tempdir().expect("temp");
        let root = dir.path().join("skill");
        fs::create_dir_all(root.join("references")).expect("dirs");
        fs::write(root.join("references/real.md"), "hi").expect("write");
        std::os::unix::fs::symlink("/etc", root.join("references/link")).expect("symlink");

        let err = resolve_checked(&root, "references/link", false).expect_err("symlink");
        assert!(matches!(err, SkillError::Denied { .. }));
        let allowed = resolve_checked(&root, "references/link", true).expect("opted in");
        assert!(allowed.ends_with("references/link"));
        assert!(resolve_checked(&root, "references/real.md", false).is_ok());
    }

    #[test]
    fn atomic_write_replaces_the_file_and_leaves_no_staging_files() {
        let dir = tempfile::tempdir().expect("temp");
        let path = dir.path().join("nested/SKILL.md");
        atomic_write(&path, b"first").expect("first write");
        assert_eq!(fs::read_to_string(&path).expect("read"), "first");
        atomic_write(&path, b"second").expect("second write");
        assert_eq!(fs::read_to_string(&path).expect("read"), "second");
        let leftovers: Vec<String> = fs::read_dir(dir.path().join("nested"))
            .expect("read dir")
            .filter_map(|e| e.ok())
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .filter(|n| n.contains("vikram-tmp"))
            .collect();
        assert!(leftovers.is_empty(), "{leftovers:?}");
    }

    #[test]
    fn copy_dir_copies_nested_files_and_skips_excluded_names() {
        let dir = tempfile::tempdir().expect("temp");
        let src = dir.path().join("src");
        fs::create_dir_all(src.join("references")).expect("dirs");
        fs::create_dir_all(src.join("node_modules")).expect("dirs");
        fs::write(src.join("SKILL.md"), "body").expect("write");
        fs::write(src.join("references/a.md"), "a").expect("write");
        fs::write(src.join("node_modules/x.js"), "x").expect("write");

        let dst = dir.path().join("dst");
        copy_dir(&src, &dst, &default_excludes()).expect("copy");
        assert!(dst.join("SKILL.md").is_file());
        assert!(dst.join("references/a.md").is_file());
        assert!(!dst.join("node_modules").exists());
    }

    #[cfg(unix)]
    #[test]
    fn copy_dir_refuses_symlinks() {
        let dir = tempfile::tempdir().expect("temp");
        let src = dir.path().join("src");
        fs::create_dir_all(&src).expect("dirs");
        std::os::unix::fs::symlink("/etc/passwd", src.join("passwd")).expect("symlink");
        let err = copy_dir(&src, &dir.path().join("dst"), &[]).expect_err("symlink copy");
        assert!(matches!(err, SkillError::Denied { .. }));
    }

    #[test]
    fn unique_paths_do_not_collide() {
        let dir = Path::new("/tmp");
        let a = unique_path(dir, "x");
        let b = unique_path(dir, "x");
        assert_ne!(a, b);
        assert_eq!(a.parent(), Some(dir));
    }
}
