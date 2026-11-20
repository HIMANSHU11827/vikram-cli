//! `.tar.gz` module archives: detection and extraction for the install commands.
//!
//! `vikram module pack` writes a deterministic tarball holding one top-level module
//! directory. The installers extract it with the same `tar` binary `pack` shells out to,
//! then run their normal directory path, so validation is identical either way. The
//! extracted directory is owned by a temp dir that is removed on every path, including
//! errors.

use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::Command;

/// True when `path` is an existing file that looks like a tar archive: a `.tar.gz`, `.tgz`,
/// or `.tar` name, or gzip/tar magic bytes. Anything else is not second-guessed — callers
/// fall through to their directory handling or their own honest error.
pub fn looks_like_archive(path: &Path) -> bool {
    if !path.is_file() {
        return false;
    }
    let name = path
        .file_name()
        .map(|n| n.to_string_lossy().to_lowercase())
        .unwrap_or_default();
    if name.ends_with(".tar.gz") || name.ends_with(".tgz") || name.ends_with(".tar") {
        return true;
    }
    let mut head = [0u8; 262];
    let Ok(mut file) = std::fs::File::open(path) else {
        return false;
    };
    let read = file.read(&mut head).unwrap_or(0);
    // gzip magic, or a tar header's `ustar` magic at offset 257.
    (read >= 2 && head[..2] == [0x1f, 0x8b]) || (read >= 262 && &head[257..262] == b"ustar")
}

/// An extracted module directory plus the temp dir that owns it; dropping the value
/// removes the temp dir, on success and on every error path.
#[derive(Debug)]
pub struct ExtractedModule {
    _tmp: tempfile::TempDir,
    root: PathBuf,
}

impl ExtractedModule {
    pub fn path(&self) -> &Path {
        &self.root
    }
}

/// Extract `archive` into a fresh temp dir and return its single top-level directory —
/// the module directory `vikram module pack` archived. `tar -xf` auto-detects the gzip
/// wrapper on read, so `.tar.gz`, `.tgz`, and plain `.tar` all work.
pub fn extract_module_dir(archive: &Path) -> Result<ExtractedModule, String> {
    if !archive.is_file() {
        return Err(format!("not an archive file: {}", archive.display()));
    }
    let tmp =
        tempfile::tempdir().map_err(|e| format!("cannot create a temp dir for extraction: {e}"))?;
    let output = Command::new("tar")
        .arg("-xf")
        .arg(archive)
        .arg("-C")
        .arg(tmp.path())
        .output()
        .map_err(|e| {
            if e.kind() == std::io::ErrorKind::NotFound {
                "extracting needs the `tar` executable, which was not found".to_owned()
            } else {
                format!("cannot run tar: {e}")
            }
        })?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        let detail = stderr
            .lines()
            .map(str::trim)
            .find(|line| !line.is_empty())
            .unwrap_or("tar failed");
        return Err(format!("cannot extract {}: {detail}", archive.display()));
    }
    let mut dirs = Vec::new();
    let entries = std::fs::read_dir(tmp.path())
        .map_err(|e| format!("cannot read the extracted archive: {e}"))?;
    for entry in entries.flatten() {
        if entry.file_type().map(|kind| kind.is_dir()).unwrap_or(false) {
            dirs.push(entry.path());
        }
    }
    match dirs.as_slice() {
        [root] => Ok(ExtractedModule {
            _tmp: tmp,
            root: root.clone(),
        }),
        [] => Err(format!(
            "{} does not contain a module directory",
            archive.display()
        )),
        _ => Err(format!(
            "{} contains {} top-level directories; expected one module directory",
            archive.display(),
            dirs.len()
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tar_gz(dir: &Path, out: &Path) {
        let name = dir.file_name().unwrap();
        let parent = dir.parent().unwrap();
        let status = Command::new("tar")
            .args(["-czf"])
            .arg(out)
            .arg("-C")
            .arg(parent)
            .arg(name)
            .status()
            .unwrap();
        assert!(status.success());
    }

    #[test]
    fn detection_covers_names_magic_and_rejects_the_rest() {
        let tmp = tempfile::tempdir().unwrap();
        let gz = tmp.path().join("mod.tar.gz");
        std::fs::write(&gz, b"not really gzip").unwrap();
        assert!(looks_like_archive(&gz));
        let tgz = tmp.path().join("mod.tgz");
        std::fs::write(&tgz, b"x").unwrap();
        assert!(looks_like_archive(&tgz));
        // gzip magic with an unrelated name still counts.
        let magic = tmp.path().join("blob");
        std::fs::write(&magic, [0x1f, 0x8b, 0x08, 0x00]).unwrap();
        assert!(looks_like_archive(&magic));
        let plain = tmp.path().join("notes.txt");
        std::fs::write(&plain, b"hello").unwrap();
        assert!(!looks_like_archive(&plain));
        assert!(!looks_like_archive(&tmp.path().join("missing.tar.gz")));
        assert!(!looks_like_archive(tmp.path()));
    }

    #[test]
    fn extraction_returns_the_single_module_dir() {
        if Command::new("tar").arg("--version").output().is_err() {
            return;
        }
        let tmp = tempfile::tempdir().unwrap();
        let module = tmp.path().join("pack-me");
        std::fs::create_dir_all(&module).unwrap();
        std::fs::write(module.join("plugin.json"), "{}").unwrap();
        let archive = tmp.path().join("pack-me.tar.gz");
        tar_gz(&module, &archive);
        let extracted = extract_module_dir(&archive).unwrap();
        assert!(extracted.path().join("plugin.json").is_file());
        assert_eq!(
            extracted.path().file_name().unwrap(),
            std::ffi::OsStr::new("pack-me")
        );
    }

    #[test]
    fn extraction_refuses_a_corrupt_archive() {
        let tmp = tempfile::tempdir().unwrap();
        let archive = tmp.path().join("bad.tar.gz");
        std::fs::write(&archive, b"this is not a tarball").unwrap();
        let err = extract_module_dir(&archive).unwrap_err();
        assert!(err.contains("cannot extract"), "{err}");
    }

    #[test]
    fn extraction_refuses_multiple_top_level_dirs() {
        if Command::new("tar").arg("--version").output().is_err() {
            return;
        }
        let tmp = tempfile::tempdir().unwrap();
        let staging = tmp.path().join("staging");
        std::fs::create_dir_all(staging.join("one")).unwrap();
        std::fs::create_dir_all(staging.join("two")).unwrap();
        let archive = tmp.path().join("multi.tar.gz");
        let status = Command::new("tar")
            .args(["-czf"])
            .arg(&archive)
            .arg("-C")
            .arg(&staging)
            .args(["one", "two"])
            .status()
            .unwrap();
        assert!(status.success());
        let err = extract_module_dir(&archive).unwrap_err();
        assert!(err.contains("top-level directories"), "{err}");
    }
}
