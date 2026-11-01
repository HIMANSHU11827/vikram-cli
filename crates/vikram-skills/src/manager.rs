//! Mutations on the user skills directory.
//!
//! Two invariants hold for every method here:
//!
//! * A skill is either fully written or untouched. Work happens in a sibling staging directory
//!   and is renamed into place, so an interrupted `create` never leaves a half-written skill
//!   that discovery would then report as invalid.
//! * A name is a validated [`SkillId`] before it is joined onto the root. `create("../etc")` is
//!   a name error, not a path traversal.
//!
//! `delete` moves into `<root>/.vikram/trash` instead of removing: the trash is the undo the
//! user did not ask for but will want once.

use std::fs;
use std::path::{Path, PathBuf};

use crate::discovery::{Diagnostic, DiagnosticLevel};
use crate::error::{Result, SkillError};
use crate::frontmatter::{self, Frontmatter, Yaml};
use crate::metadata::{DESCRIPTION_MAX, SkillMetadata};
use crate::paths;
use crate::skill::SkillId;

const SKILL_FILE: &str = "SKILL.md";
const DISABLED_FILE: &str = "disabled.json";
/// Marker written by [`Manager::export`], stripped again by [`Manager::import`].
const EXPORT_FILE: &str = "EXPORT.json";

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Manager {
    /// The skills root. Defaults to [`paths::user_skills_dir`].
    pub root: PathBuf,
}

impl Default for Manager {
    fn default() -> Self {
        Manager::new()
    }
}

impl Manager {
    pub fn new() -> Manager {
        Manager::at(paths::user_skills_dir())
    }

    pub fn at(root: PathBuf) -> Manager {
        Manager { root }
    }

    /// Crate-private state directory. `root/.vikram` rather than the global
    /// [`paths::state_dir`], so an isolated manager never touches the real home directory.
    pub(crate) fn state_dir(&self) -> PathBuf {
        self.root.join(".vikram")
    }

    pub(crate) fn disabled_path(&self) -> PathBuf {
        self.state_dir().join(DISABLED_FILE)
    }

    /// Creates `root/<name>/SKILL.md`. `body` is the markdown after the frontmatter.
    pub fn create(&self, name: &str, description: &str, body: &str) -> Result<PathBuf> {
        let (id, dir) = self.dir_for(name)?;
        self.reject_existing(&id, &dir)?;

        let description = description.trim();
        if description.is_empty() {
            return Err(SkillError::MissingField {
                path: dir.join(SKILL_FILE),
                field: "description",
            });
        }
        if description.len() > DESCRIPTION_MAX {
            return Err(SkillError::TooLarge {
                what: "description".to_string(),
                size: description.len(),
                limit: DESCRIPTION_MAX,
            });
        }

        let metadata = SkillMetadata::new(id.as_str(), description);
        let document = frontmatter::render_document(&metadata.to_frontmatter(body), body);
        self.check_document(&document, &dir.join(SKILL_FILE), id.as_str())?;

        let staging = self.stage(id.as_str(), |staging| {
            fs::create_dir_all(staging).map_err(|err| SkillError::io(staging, &err))?;
            paths::atomic_write(&staging.join(SKILL_FILE), document.as_bytes())
        })?;
        self.commit(&staging, &dir)?;
        Ok(dir)
    }

    /// Copies an existing directory into the root, renaming the frontmatter `name` to match its
    /// new directory. Resources are copied too; symlinks and excluded build directories are not.
    pub fn create_from(&self, name: &str, source_dir: &Path) -> Result<PathBuf> {
        let (id, dir) = self.dir_for(name)?;
        self.reject_existing(&id, &dir)?;
        if !source_dir.is_dir() {
            return Err(SkillError::Io {
                path: source_dir.to_path_buf(),
                reason: "not a directory".to_string(),
            });
        }
        let source = SkillSource::locate(source_dir)?;
        let renamed = {
            let text = fs::read_to_string(&source.file)
                .map_err(|err| SkillError::io(&source.file, &err))?;
            let fm = Frontmatter::parse_at(&text, &source.file)?;
            let document = frontmatter::render_document(
                &fm.with("name", Yaml::Str(id.as_str().to_string())),
                &fm.body,
            );
            self.check_document(&document, &source.file, id.as_str())?;
            document
        };

        let staging = self.stage(id.as_str(), |staging| {
            paths::copy_dir(source_dir, staging, &paths::default_excludes())?;
            paths::atomic_write(&staging.join(source.rel.as_str()), renamed.as_bytes())
        })?;
        self.commit(&staging, &dir)?;
        Ok(dir)
    }

    /// Replaces the markdown body, leaving the frontmatter byte-for-byte as it was.
    pub fn edit(&self, name: &str, body: &str) -> Result<()> {
        let (id, dir) = self.dir_for(name)?;
        let source = SkillSource::locate(&dir)?;
        let text =
            fs::read_to_string(&source.file).map_err(|err| SkillError::io(&source.file, &err))?;
        let fm = Frontmatter::parse_at(&text, &source.file)?;
        let document = frontmatter::render_document(&fm, body);
        self.check_document(&document, &source.file, id.as_str())?;
        paths::atomic_write(&source.file, document.as_bytes())
    }

    /// Sets one top-level frontmatter key. A change that would make the skill unloadable (an
    /// empty description, an invalid name) is refused before anything is written.
    pub fn set_metadata(&self, name: &str, key: &str, value: Yaml) -> Result<()> {
        let (id, dir) = self.dir_for(name)?;
        let source = SkillSource::locate(&dir)?;
        let text =
            fs::read_to_string(&source.file).map_err(|err| SkillError::io(&source.file, &err))?;
        let fm = Frontmatter::parse_at(&text, &source.file)?;
        let document = frontmatter::render_document(&fm.with(key, value), &fm.body);
        self.check_document(&document, &source.file, id.as_str())?;
        paths::atomic_write(&source.file, document.as_bytes())
    }

    /// Moves the skill into `<state>/trash/<name>-<ts>`. Never deletes outright.
    pub fn delete(&self, name: &str) -> Result<()> {
        let (id, dir) = self.dir_for(name)?;
        if !dir.is_dir() {
            return Err(SkillError::NotFound {
                name: id.into_string(),
                sources: 1,
            });
        }
        let trash = self.state_dir().join("trash");
        fs::create_dir_all(&trash).map_err(|err| SkillError::io(&trash, &err))?;
        let mut target = trash.join(format!("{}-{}", id.as_str(), paths::now_ms()));
        if target.exists() {
            target = paths::unique_path(&trash, id.as_str());
        }
        paths::move_dir(&dir, &target)
    }

    pub fn enable(&self, name: &str) -> Result<()> {
        self.set_disabled(name, false)
    }

    pub fn disable(&self, name: &str) -> Result<()> {
        self.set_disabled(name, true)
    }

    /// Copies a skill under a new name. The directory name is the identity, so the frontmatter
    /// `name` is rewritten rather than left stale.
    pub fn duplicate(&self, name: &str, as_name: &str) -> Result<PathBuf> {
        let (_, dir) = self.dir_for(name)?;
        let (new_id, new_dir) = self.dir_for(as_name)?;
        self.reject_existing(&new_id, &new_dir)?;
        let source = SkillSource::locate(&dir)?;
        let text =
            fs::read_to_string(&source.file).map_err(|err| SkillError::io(&source.file, &err))?;
        let fm = Frontmatter::parse_at(&text, &source.file)?;
        let document = frontmatter::render_document(
            &fm.with("name", Yaml::Str(new_id.as_str().to_string())),
            &fm.body,
        );
        self.check_document(&document, &source.file, new_id.as_str())?;

        let staging = self.stage(new_id.as_str(), |staging| {
            paths::copy_dir(&dir, staging, &paths::default_excludes())?;
            paths::atomic_write(&staging.join(source.rel.as_str()), document.as_bytes())
        })?;
        self.commit(&staging, &new_dir)?;
        Ok(new_dir)
    }

    /// Writes a directory archive: the skill's files plus `EXPORT.json` describing what was
    /// exported. No zip: the contract allows no archive dependency, and a directory is easier
    /// to inspect than a blob.
    ///
    /// `dest` is the skill directory itself unless it already exists as a directory, in which
    /// case the skill lands at `dest/<name>`.
    pub fn export(&self, name: &str, dest: &Path) -> Result<PathBuf> {
        let (id, dir) = self.dir_for(name)?;
        if !dir.is_dir() {
            return Err(SkillError::NotFound {
                name: id.into_string(),
                sources: 1,
            });
        }
        let target = if dest.is_dir() {
            dest.join(id.as_str())
        } else {
            dest.to_path_buf()
        };
        self.reject_existing(&id, &target)?;

        let hash = crate::history::hash_dir(&dir)?;
        let version = read_version(&dir);
        let manifest = serde_json::json!({
            "name": id.as_str(),
            "version": version,
            "content_hash": hash,
            "exported_ms": paths::now_ms(),
            "source": dir.display().to_string(),
        });
        let bytes = serde_json::to_vec_pretty(&manifest).map_err(|err| SkillError::Io {
            path: target.join(EXPORT_FILE),
            reason: err.to_string(),
        })?;

        let parent = match target.parent() {
            Some(parent) if !parent.as_os_str().is_empty() => parent.to_path_buf(),
            _ => PathBuf::from("."),
        };
        fs::create_dir_all(&parent).map_err(|err| SkillError::io(&parent, &err))?;
        let staging = self.stage_in(&parent, "export", |staging| {
            let mut excludes = paths::default_excludes();
            excludes.push(EXPORT_FILE.to_string());
            paths::copy_dir(&dir, staging, &excludes)?;
            paths::atomic_write(&staging.join(EXPORT_FILE), &bytes)
        })?;
        self.commit(&staging, &target)?;
        Ok(target)
    }

    /// Installs a directory (an export, a checkout, a hand-written skill) under its declared
    /// name, or under its directory name when the frontmatter has none.
    pub fn import(&self, src: &Path) -> Result<SkillId> {
        if !src.is_dir() {
            return Err(SkillError::Io {
                path: src.to_path_buf(),
                reason: "not a directory".to_string(),
            });
        }
        let source = SkillSource::locate(src)?;
        let text =
            fs::read_to_string(&source.file).map_err(|err| SkillError::io(&source.file, &err))?;
        let fm = Frontmatter::parse_at(&text, &source.file)?;

        let declared = fm
            .get("name")
            .and_then(Yaml::as_str)
            .map(str::trim)
            .filter(|name| !name.is_empty());
        let name = match declared {
            Some(name) => name.to_string(),
            None => src
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .ok_or_else(|| SkillError::Io {
                    path: src.to_path_buf(),
                    reason: "source directory has no name".to_string(),
                })?,
        };
        let id = SkillId::new(&name)?;
        let dir = paths::resolve_within(&self.root, id.as_str())?;
        self.reject_existing(&id, &dir)?;

        let document = frontmatter::render_document(
            &fm.with("name", Yaml::Str(id.as_str().to_string())),
            &fm.body,
        );
        self.check_document(&document, &source.file, id.as_str())?;

        let staging = self.stage(id.as_str(), |staging| {
            let mut excludes = paths::default_excludes();
            excludes.push(EXPORT_FILE.to_string());
            paths::copy_dir(src, staging, &excludes)?;
            paths::atomic_write(&staging.join(source.rel.as_str()), document.as_bytes())
        })?;
        self.commit(&staging, &dir)?;
        Ok(id)
    }

    /// Everything wrong with one directory, in the order it was found. A skill that cannot even
    /// be read is an `Err`; a skill that reads but is broken is an `Error` diagnostic, because
    /// the caller usually wants to show all problems at once.
    pub fn validate(&self, dir: &Path) -> Result<Vec<Diagnostic>> {
        if !dir.is_dir() {
            return Err(SkillError::Io {
                path: dir.to_path_buf(),
                reason: "not a directory".to_string(),
            });
        }
        let mut out = Vec::new();
        let dir_name = dir
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_default();
        let source = match SkillSource::locate(dir) {
            Ok(source) => source,
            Err(_) => {
                out.push(Diagnostic::new(
                    DiagnosticLevel::Error,
                    dir,
                    "no `SKILL.md` (or `skill.md`) in this directory",
                ));
                return Ok(out);
            }
        };
        let text =
            fs::read_to_string(&source.file).map_err(|err| SkillError::io(&source.file, &err))?;
        let fm = match Frontmatter::parse_at(&text, &source.file) {
            Ok(fm) => fm,
            Err(err) => {
                out.push(Diagnostic::new(
                    DiagnosticLevel::Error,
                    &source.file,
                    err.to_string(),
                ));
                return Ok(out);
            }
        };
        match SkillMetadata::from_frontmatter(&fm, &dir_name) {
            Ok((metadata, warnings)) => {
                for warning in warnings {
                    out.push(Diagnostic::new(
                        DiagnosticLevel::Warning,
                        &source.file,
                        warning,
                    ));
                }
                out.push(Diagnostic::new(
                    DiagnosticLevel::Info,
                    &source.file,
                    format!("skill `{}` is valid", metadata.name),
                ));
                for missing in missing_resources(&metadata, dir) {
                    out.push(Diagnostic::new(
                        DiagnosticLevel::Warning,
                        &source.file,
                        format!("declared test `{missing}` does not exist"),
                    ));
                }
            }
            Err(err) => out.push(Diagnostic::new(
                DiagnosticLevel::Error,
                &source.file,
                err.to_string(),
            )),
        }
        Ok(out)
    }

    fn set_disabled(&self, name: &str, disabled: bool) -> Result<()> {
        let id = SkillId::new(name)?;
        let path = self.disabled_path();
        let mut names: Vec<String> = paths::read_json(&path).unwrap_or_default();
        names.retain(|entry| SkillId::new(entry).is_ok());
        names.sort();
        names.dedup();
        if disabled {
            if !names.iter().any(|entry| entry == id.as_str()) {
                names.push(id.as_str().to_string());
                names.sort();
            }
        } else {
            names.retain(|entry| entry != id.as_str());
        }
        let bytes = serde_json::to_vec_pretty(&names).map_err(|err| SkillError::Io {
            path: path.clone(),
            reason: err.to_string(),
        })?;
        paths::atomic_write(&path, &bytes)
    }

    fn dir_for(&self, name: &str) -> Result<(SkillId, PathBuf)> {
        let id = SkillId::new(name)?;
        let dir = paths::resolve_within(&self.root, id.as_str())?;
        Ok((id, dir))
    }

    fn reject_existing(&self, id: &SkillId, dir: &Path) -> Result<()> {
        if dir.exists() {
            return Err(SkillError::AlreadyExists {
                name: id.as_str().to_string(),
                path: dir.to_path_buf(),
            });
        }
        Ok(())
    }

    /// Builds a staging directory in the root and hands it to `build`. Any failure removes it,
    /// so a rejected mutation leaves no debris for discovery to trip over.
    fn stage(&self, prefix: &str, build: impl FnOnce(&Path) -> Result<()>) -> Result<PathBuf> {
        fs::create_dir_all(&self.root).map_err(|err| SkillError::io(&self.root, &err))?;
        self.stage_in(&self.root, prefix, build)
    }

    fn stage_in(
        &self,
        parent: &Path,
        prefix: &str,
        build: impl FnOnce(&Path) -> Result<()>,
    ) -> Result<PathBuf> {
        let staging = paths::unique_path(parent, prefix);
        match build(&staging) {
            Ok(()) => Ok(staging),
            Err(err) => {
                paths::remove_path(&staging);
                Err(err)
            }
        }
    }

    fn commit(&self, staging: &Path, target: &Path) -> Result<()> {
        match fs::rename(staging, target) {
            Ok(()) => Ok(()),
            Err(err) => {
                let collision = target.exists();
                paths::remove_path(staging);
                if collision {
                    let name = target
                        .file_name()
                        .map(|n| n.to_string_lossy().into_owned())
                        .unwrap_or_default();
                    Err(SkillError::AlreadyExists {
                        name,
                        path: target.to_path_buf(),
                    })
                } else {
                    Err(SkillError::io(target, &err))
                }
            }
        }
    }

    pub(crate) fn check_document(&self, document: &str, path: &Path, dir_name: &str) -> Result<()> {
        let fm = Frontmatter::parse_at(document, path)?;
        SkillMetadata::from_frontmatter(&fm, dir_name)?;
        Ok(())
    }
}

/// A `SKILL.md` and its path relative to the skill directory.
pub(crate) struct SkillSource {
    pub(crate) file: PathBuf,
    pub(crate) rel: String,
}

impl SkillSource {
    pub(crate) fn locate(dir: &Path) -> Result<SkillSource> {
        let name = dir
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_default();
        for candidate in [SKILL_FILE, "skill.md"] {
            let file = dir.join(candidate);
            if file.is_file() {
                return Ok(SkillSource {
                    file,
                    rel: candidate.to_string(),
                });
            }
        }
        Err(SkillError::NotFound { name, sources: 1 })
    }
}

pub(crate) fn read_version(dir: &Path) -> Option<String> {
    let source = SkillSource::locate(dir).ok()?;
    let text = fs::read_to_string(&source.file).ok()?;
    let fm = Frontmatter::parse(&text).ok()?;
    let dir_name = dir.file_name()?.to_string_lossy().into_owned();
    SkillMetadata::from_frontmatter(&fm, &dir_name)
        .ok()
        .and_then(|(metadata, _)| metadata.vikram.version)
}

fn missing_resources(metadata: &SkillMetadata, dir: &Path) -> Vec<String> {
    metadata
        .vikram
        .tests
        .iter()
        .filter(|rel| {
            paths::resolve_checked(dir, rel, false)
                .map(|p| !p.exists())
                .unwrap_or(true)
        })
        .cloned()
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::discovery::{Catalog, DiscoveryOptions};
    use crate::trust::TrustPolicy;

    fn doc(name: &str, description: &str, body: &str) -> String {
        format!("---\nname: {name}\ndescription: {description}\n---\n{body}")
    }

    fn write_skill(dir: &Path, name: &str, text: &str) {
        let skill = dir.join(name);
        fs::create_dir_all(&skill).expect("dirs");
        fs::write(skill.join(SKILL_FILE), text).expect("write");
    }

    fn temp_manager() -> (tempfile::TempDir, Manager) {
        let tmp = tempfile::tempdir().expect("temp");
        let manager = Manager::at(tmp.path().join("skills"));
        (tmp, manager)
    }

    #[test]
    fn new_points_at_the_user_skills_directory() {
        assert_eq!(Manager::new().root, paths::user_skills_dir());
        assert_eq!(Manager::default().root, paths::user_skills_dir());
    }

    #[test]
    fn create_writes_frontmatter_and_body() {
        let (_tmp, manager) = temp_manager();
        let dir = manager
            .create("alpha", "Does alpha things", "# Alpha\n\nBody.\n")
            .expect("create");
        let text = fs::read_to_string(dir.join(SKILL_FILE)).expect("read");
        assert!(text.starts_with("---\nname: alpha\ndescription: Does alpha things\n---\n"));
        assert!(text.ends_with("# Alpha\n\nBody.\n"));
        assert!(dir.ends_with("skills/alpha"));
    }

    #[test]
    fn create_rejects_invalid_names_before_touching_the_filesystem() {
        let (_tmp, manager) = temp_manager();
        for name in ["", "Alpha", "../escape", "a/b", "a b", "a--b"] {
            let err = manager.create(name, "desc", "body").expect_err(name);
            assert!(
                matches!(
                    err,
                    SkillError::InvalidName { .. } | SkillError::PathEscape { .. }
                ),
                "{name}: {err:?}"
            );
        }
        assert!(!manager.root.exists());
    }

    #[test]
    fn create_rejects_an_empty_or_oversized_description() {
        let (_tmp, manager) = temp_manager();
        let err = manager.create("alpha", "   ", "body").expect_err("empty");
        assert!(matches!(
            err,
            SkillError::MissingField {
                field: "description",
                ..
            }
        ));

        let long = "x".repeat(DESCRIPTION_MAX + 1);
        let err = manager.create("alpha", &long, "body").expect_err("long");
        assert!(matches!(err, SkillError::TooLarge { .. }));
        assert!(!manager.root.join("alpha").exists());
    }

    #[test]
    fn create_refuses_to_overwrite_and_leaves_the_original_intact() {
        let (_tmp, manager) = temp_manager();
        manager.create("alpha", "first", "one").expect("first");
        let err = manager
            .create("alpha", "second", "two")
            .expect_err("collision");
        assert!(matches!(err, SkillError::AlreadyExists { .. }));
        let text = fs::read_to_string(manager.root.join("alpha/SKILL.md")).expect("read");
        assert!(text.contains("description: first"));
        assert!(text.contains("one"));
    }

    #[test]
    fn create_leaves_no_staging_directories_behind() {
        let (_tmp, manager) = temp_manager();
        manager.create("alpha", "desc", "body").expect("create");
        let leftovers: Vec<String> = fs::read_dir(&manager.root)
            .expect("root")
            .filter_map(|e| e.ok())
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .filter(|n| n.contains("vikram-tmp"))
            .collect();
        assert!(leftovers.is_empty(), "{leftovers:?}");
    }

    #[test]
    fn create_from_copies_resources_and_renames_the_frontmatter() {
        let (tmp, manager) = temp_manager();
        let src = tmp.path().join("source/other-name");
        fs::create_dir_all(src.join("references")).expect("dirs");
        fs::write(
            src.join("SKILL.md"),
            doc("other-name", "Copied", "# Body\n"),
        )
        .expect("write");
        fs::write(src.join("references/api.md"), "docs").expect("write");

        let dir = manager.create_from("alpha", &src).expect("create_from");
        assert!(dir.join("references/api.md").is_file());
        let text = fs::read_to_string(dir.join("SKILL.md")).expect("read");
        assert!(text.contains("name: alpha"));
        assert!(!text.contains("other-name"));
        assert!(text.ends_with("# Body\n"));
    }

    #[test]
    fn create_from_refuses_a_directory_without_a_skill_file() {
        let (tmp, manager) = temp_manager();
        let src = tmp.path().join("empty");
        fs::create_dir_all(&src).expect("dirs");
        let err = manager
            .create_from("alpha", &src)
            .expect_err("no skill file");
        assert!(matches!(err, SkillError::NotFound { .. }));
        assert!(!manager.root.join("alpha").exists());
    }

    #[test]
    fn edit_replaces_the_body_and_keeps_the_frontmatter() {
        let (_tmp, manager) = temp_manager();
        let dir = manager.create("alpha", "desc", "old body").expect("create");
        manager.edit("alpha", "new body").expect("edit");
        let text = fs::read_to_string(dir.join(SKILL_FILE)).expect("read");
        assert!(text.contains("description: desc"));
        assert!(text.ends_with("new body"));
        assert!(!text.contains("old body"));
    }

    #[test]
    fn edit_reports_a_missing_skill() {
        let (_tmp, manager) = temp_manager();
        fs::create_dir_all(&manager.root).expect("root");
        let err = manager.edit("ghost", "body").expect_err("missing");
        assert!(matches!(err, SkillError::NotFound { .. }));
    }

    #[test]
    fn set_metadata_persists_a_new_key() {
        let (_tmp, manager) = temp_manager();
        let dir = manager.create("alpha", "desc", "body").expect("create");
        manager
            .set_metadata("alpha", "license", Yaml::Str("MIT".to_string()))
            .expect("set");
        let text = fs::read_to_string(dir.join(SKILL_FILE)).expect("read");
        assert!(text.contains("license: MIT"));

        let fm = Frontmatter::parse(&text).expect("parse");
        assert_eq!(fm.get("license").and_then(Yaml::as_str), Some("MIT"));
        assert_eq!(fm.body, "body");
    }

    #[test]
    fn set_metadata_refuses_to_break_the_skill() {
        let (_tmp, manager) = temp_manager();
        let dir = manager.create("alpha", "desc", "body").expect("create");
        let err = manager
            .set_metadata("alpha", "description", Yaml::Str(String::new()))
            .expect_err("empty description");
        assert!(matches!(err, SkillError::MissingField { .. }));
        let text = fs::read_to_string(dir.join(SKILL_FILE)).expect("read");
        assert!(text.contains("description: desc"));
    }

    #[test]
    fn delete_moves_the_skill_to_the_trash() {
        let (_tmp, manager) = temp_manager();
        let dir = manager.create("alpha", "desc", "body").expect("create");
        manager.delete("alpha").expect("delete");
        assert!(!dir.exists());

        let trash = manager.state_dir().join("trash");
        let moved: Vec<PathBuf> = fs::read_dir(&trash)
            .expect("trash")
            .filter_map(|e| e.ok())
            .map(|e| e.path())
            .collect();
        assert_eq!(moved.len(), 1);
        assert!(
            moved[0]
                .file_name()
                .map(|n| n.to_string_lossy().starts_with("alpha-"))
                .unwrap_or(false)
        );
        assert!(moved[0].join(SKILL_FILE).is_file());
    }

    #[test]
    fn delete_reports_a_missing_skill() {
        let (_tmp, manager) = temp_manager();
        fs::create_dir_all(&manager.root).expect("root");
        let err = manager.delete("ghost").expect_err("missing");
        assert!(matches!(err, SkillError::NotFound { .. }));
    }

    #[test]
    fn disable_then_enable_persists_and_filters_discovery() {
        let (_tmp, manager) = temp_manager();
        manager.create("alpha", "desc", "body").expect("create");

        manager.disable("alpha").expect("disable");
        manager
            .disable("alpha")
            .expect("disable twice is idempotent");
        let stored: Vec<String> =
            paths::read_json(&manager.disabled_path()).expect("disabled.json");
        assert_eq!(stored, vec!["alpha".to_string()]);

        let opts = DiscoveryOptions::roots_only()
            .with_root(
                crate::skill::Scope::User,
                crate::skill::Compat::Native,
                manager.root.clone(),
            )
            .with_state_dir(manager.state_dir())
            .with_trust(TrustPolicy::default());
        let catalog = Catalog::discover(&opts);
        assert!(!catalog.resolve("alpha").expect("found").enabled);
        assert!(
            catalog
                .disabled()
                .contains(&SkillId::new("alpha").expect("id"))
        );

        manager.enable("alpha").expect("enable");
        let stored: Vec<String> =
            paths::read_json(&manager.disabled_path()).expect("disabled.json");
        assert!(stored.is_empty());
        let catalog = Catalog::discover(&opts);
        assert!(catalog.resolve("alpha").expect("found").enabled);
    }

    #[test]
    fn enable_and_disable_reject_invalid_names() {
        let (_tmp, manager) = temp_manager();
        assert!(matches!(
            manager.disable("Bad Name").expect_err("invalid"),
            SkillError::InvalidName { .. }
        ));
        assert!(matches!(
            manager.enable("../x").expect_err("invalid"),
            SkillError::InvalidName { .. }
        ));
    }

    #[test]
    fn duplicate_copies_content_under_the_new_identity() {
        let (_tmp, manager) = temp_manager();
        manager.create("alpha", "desc", "body").expect("create");
        let dir = manager.duplicate("alpha", "beta").expect("duplicate");
        assert!(dir.ends_with("beta"));
        let text = fs::read_to_string(dir.join(SKILL_FILE)).expect("read");
        assert!(text.contains("name: beta"));
        assert!(manager.root.join("alpha/SKILL.md").is_file());
        assert!(matches!(
            manager.duplicate("alpha", "beta").expect_err("collision"),
            SkillError::AlreadyExists { .. }
        ));
    }

    #[test]
    fn export_writes_a_directory_archive_with_a_manifest() {
        let (tmp, manager) = temp_manager();
        manager.create("alpha", "desc", "body").expect("create");
        let dest = tmp.path().join("out/alpha");
        let target = manager.export("alpha", &dest).expect("export");
        assert_eq!(target, dest);
        assert!(target.join(SKILL_FILE).is_file());

        let manifest: serde_json::Value =
            serde_json::from_str(&fs::read_to_string(target.join(EXPORT_FILE)).expect("read"))
                .expect("json");
        assert_eq!(manifest["name"], "alpha");
        assert_eq!(manifest["content_hash"].as_str().map(str::len), Some(16));
    }

    #[test]
    fn export_into_an_existing_directory_uses_the_skill_name() {
        let (tmp, manager) = temp_manager();
        manager.create("alpha", "desc", "body").expect("create");
        let dest = tmp.path().join("out");
        fs::create_dir_all(&dest).expect("dirs");
        let target = manager.export("alpha", &dest).expect("export");
        assert_eq!(target, dest.join("alpha"));
        assert!(target.join(SKILL_FILE).is_file());
    }

    #[test]
    fn import_round_trips_an_export() {
        let (tmp, manager) = temp_manager();
        let original = manager.create("alpha", "desc", "body").expect("create");
        let original_hash = crate::history::hash_dir(&original).expect("hash");
        let archive = manager
            .export("alpha", &tmp.path().join("archive"))
            .expect("export");
        manager.delete("alpha").expect("delete");

        let name = manager.import(&archive).expect("import");
        assert_eq!(name.as_str(), "alpha");
        let reimported = manager.root.join("alpha");
        assert!(reimported.join(SKILL_FILE).is_file());
        assert!(!reimported.join(EXPORT_FILE).exists());
        assert_eq!(
            crate::history::hash_dir(&reimported).expect("hash"),
            original_hash
        );
    }

    #[test]
    fn import_uses_the_directory_name_when_the_frontmatter_has_none() {
        let (tmp, manager) = temp_manager();
        let src = tmp.path().join("incoming/gamma");
        fs::create_dir_all(&src).expect("dirs");
        fs::write(
            src.join(SKILL_FILE),
            "---\ndescription: no name here\n---\nbody",
        )
        .expect("write");
        let name = manager.import(&src).expect("import");
        assert_eq!(name.as_str(), "gamma");
        let text = fs::read_to_string(manager.root.join("gamma/SKILL.md")).expect("read");
        assert!(text.contains("name: gamma"));
    }

    #[test]
    fn import_refuses_an_existing_name_and_a_missing_skill_file() {
        let (tmp, manager) = temp_manager();
        manager.create("alpha", "desc", "body").expect("create");
        let src = tmp.path().join("incoming/alpha");
        fs::create_dir_all(&src).expect("dirs");
        fs::write(src.join(SKILL_FILE), doc("alpha", "again", "body")).expect("write");
        assert!(matches!(
            manager.import(&src).expect_err("collision"),
            SkillError::AlreadyExists { .. }
        ));

        let empty = tmp.path().join("incoming/none");
        fs::create_dir_all(&empty).expect("dirs");
        assert!(matches!(
            manager.import(&empty).expect_err("no skill file"),
            SkillError::NotFound { .. }
        ));
    }

    #[test]
    fn validate_reports_a_valid_skill_as_info() {
        let (_tmp, manager) = temp_manager();
        let dir = manager.create("alpha", "desc", "body").expect("create");
        let diagnostics = manager.validate(&dir).expect("validate");
        assert_eq!(diagnostics.len(), 1);
        assert_eq!(diagnostics[0].level, DiagnosticLevel::Info);
        assert!(diagnostics[0].message.contains("alpha"));
    }

    #[test]
    fn validate_reports_broken_frontmatter_as_an_error() {
        let (tmp, manager) = temp_manager();
        let dir = tmp.path().join("skills/broken");
        write_skill(
            &tmp.path().join("skills"),
            "broken",
            "---\nname: broken\na: 1\na: 2\n---\n",
        );
        let diagnostics = manager.validate(&dir).expect("validate");
        assert!(
            diagnostics
                .iter()
                .any(|d| d.level == DiagnosticLevel::Error && d.message.contains("duplicate"))
        );
    }

    #[test]
    fn validate_warns_when_the_name_does_not_match_the_directory() {
        let (tmp, manager) = temp_manager();
        let dir = tmp.path().join("skills/alpha");
        write_skill(
            &tmp.path().join("skills"),
            "alpha",
            &doc("other", "desc", "body"),
        );
        let diagnostics = manager.validate(&dir).expect("validate");
        assert!(
            diagnostics.iter().any(
                |d| d.level == DiagnosticLevel::Warning && d.message.contains("does not match")
            )
        );
    }

    #[test]
    fn validate_reports_a_directory_without_a_skill_file() {
        let (tmp, manager) = temp_manager();
        let dir = tmp.path().join("skills/empty");
        fs::create_dir_all(&dir).expect("dirs");
        let diagnostics = manager.validate(&dir).expect("validate");
        assert_eq!(diagnostics.len(), 1);
        assert_eq!(diagnostics[0].level, DiagnosticLevel::Error);
    }

    #[test]
    fn validate_rejects_a_path_that_is_not_a_directory() {
        let (tmp, manager) = temp_manager();
        let file = tmp.path().join("file.txt");
        fs::write(&file, "x").expect("write");
        let err = manager.validate(&file).expect_err("not a dir");
        assert!(matches!(err, SkillError::Io { .. }));
    }

    #[test]
    fn names_cannot_escape_the_root_through_mutations() {
        let (tmp, manager) = temp_manager();
        for call in [
            manager.delete("../outside").map(|_| ()),
            manager.edit("../outside", "body"),
            manager.disable("../outside"),
            manager.duplicate("alpha", "../outside").map(|_| ()),
            manager.create_from("../outside", tmp.path()).map(|_| ()),
        ] {
            let err = call.expect_err("escape");
            assert!(
                matches!(
                    err,
                    SkillError::InvalidName { .. } | SkillError::PathEscape { .. }
                ),
                "{err:?}"
            );
        }
        assert!(!tmp.path().join("outside").exists());
    }
}
