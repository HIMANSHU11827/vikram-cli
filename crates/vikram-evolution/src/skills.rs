use crate::error::{EvolutionError, Result};
use crate::store::EvolutionStore;
use crate::types::{SkillVersion, now_ms};
use std::fs;
use std::path::PathBuf;

pub struct SkillEvolution {
    store: EvolutionStore,
    skills_root: PathBuf,
}

impl SkillEvolution {
    pub fn new(store: EvolutionStore, skills_root: PathBuf) -> Self {
        Self { store, skills_root }
    }

    pub fn store(&self) -> &EvolutionStore {
        &self.store
    }
    pub fn skills_root(&self) -> &std::path::Path {
        &self.skills_root
    }

    /// Validate a skill name and resolve its directory.
    ///
    /// Rejects (rather than silently stripping) any name containing path
    /// separators, `.`/`..` components, NUL, or that is empty — stripping
    /// could map two distinct names to the same directory and overwrite
    /// an unrelated skill.
    fn skill_dir(&self, name: &str) -> Result<PathBuf> {
        validate_skill_name(name)?;
        Ok(self.skills_root.join(name))
    }
    fn skill_file(&self, name: &str) -> Result<PathBuf> {
        Ok(self.skill_dir(name)?.join("SKILL.md"))
    }

    pub fn discover(&self) -> Vec<String> {
        let mut out = Vec::new();
        if let Ok(rd) = fs::read_dir(&self.skills_root) {
            for e in rd.flatten() {
                let name = e.file_name().to_string_lossy().to_string();
                if validate_skill_name(&name).is_ok()
                    && e.path().is_dir()
                    && e.path().join("SKILL.md").exists()
                {
                    out.push(name);
                }
            }
        }
        out.sort();
        out
    }

    pub fn read_skill(&self, name: &str) -> Result<String> {
        let p = self.skill_file(name)?;
        fs::read_to_string(&p).map_err(|e| EvolutionError::Io(e.to_string()))
    }

    pub fn create_skill(
        &self,
        name: &str,
        content: &str,
        change_desc: &str,
    ) -> Result<SkillVersion> {
        crate::redact::scan_injection(content)?;
        // Version consistency with `improve_skill`: the next version is
        // always `max(existing) + 1`. Creating over an existing skill
        // therefore yields v2, v3, … rather than silently resetting to v1
        // while the on-disk content is replaced.
        let current = self.current_version(name)?;
        let next = current.unwrap_or(0) + 1;
        let dir = self.skill_dir(name)?;
        fs::create_dir_all(&dir).map_err(|e| EvolutionError::Io(e.to_string()))?;
        let versions_dir = dir.join(".versions");
        fs::create_dir_all(&versions_dir).map_err(|e| EvolutionError::Io(e.to_string()))?;
        if next > 1 {
            // Preserve the overwritten content as the previous version.
            if let Ok(old) = self.read_skill(name) {
                let backup = versions_dir.join(format!("v{}.md", next - 1));
                fs::write(&backup, old).map_err(|e| EvolutionError::Io(e.to_string()))?;
            }
        }
        let v = SkillVersion {
            skill_name: name.to_string(),
            version: next,
            content: content.to_string(),
            created_ms: now_ms(),
            parent_version: if next == 1 { None } else { Some(next - 1) },
            change_description: change_desc.to_string(),
        };
        fs::write(self.skill_file(name)?, content)
            .map_err(|e| EvolutionError::Io(e.to_string()))?;
        self.store.save_skill_version(&v)?;
        Ok(v)
    }

    pub fn improve_skill(
        &self,
        name: &str,
        new_content: &str,
        change_desc: &str,
    ) -> Result<SkillVersion> {
        crate::redact::scan_injection(new_content)?;
        let current = self.current_version(name)?.unwrap_or(0);
        let next = current + 1;
        let dir = self.skill_dir(name)?;
        let versions_dir = dir.join(".versions");
        fs::create_dir_all(&versions_dir).map_err(|e| EvolutionError::Io(e.to_string()))?;
        if let Ok(old) = self.read_skill(name) {
            let backup = versions_dir.join(format!("v{}.md", current.max(1)));
            fs::write(&backup, old).map_err(|e| EvolutionError::Io(e.to_string()))?;
        }
        let v = SkillVersion {
            skill_name: name.to_string(),
            version: next,
            content: new_content.to_string(),
            created_ms: now_ms(),
            parent_version: if current == 0 { None } else { Some(current) },
            change_description: change_desc.to_string(),
        };
        fs::write(self.skill_file(name)?, new_content)
            .map_err(|e| EvolutionError::Io(e.to_string()))?;
        self.store.save_skill_version(&v)?;
        Ok(v)
    }

    /// Roll back a skill to a previous version.
    ///
    /// Restores both the on-disk `SKILL.md` and records the rollback as a
    /// new version (content copied from the target version, parent = the
    /// version being rolled back from), so history is append-only and
    /// `list_versions` shows the rollback. Previously it only rewrote the
    /// file and left the version history claiming the newer content was
    /// current.
    pub fn rollback(&self, name: &str, version: u32) -> Result<SkillVersion> {
        // Fail closed when the store cannot tell us the history: a silent
        // `unwrap_or(0)` here used to restart versioning at 1.
        let versions = self.store.list_skill_versions(name)?;
        let target = versions
            .iter()
            .find(|x| x.version == version)
            .cloned()
            .ok_or_else(|| EvolutionError::NotFound(format!("version {version}")))?;
        let current = versions.iter().map(|x| x.version).max().unwrap_or(version);
        fs::write(self.skill_file(name)?, &target.content)
            .map_err(|e| EvolutionError::Io(e.to_string()))?;
        if current != version {
            let restored = SkillVersion {
                skill_name: name.to_string(),
                version: current + 1,
                content: target.content.clone(),
                created_ms: now_ms(),
                parent_version: Some(current),
                change_description: format!("rollback to v{version}"),
            };
            self.store.save_skill_version(&restored)?;
            // Keep the on-disk file in sync with the new head version.
            fs::write(self.skill_file(name)?, &restored.content)
                .map_err(|e| EvolutionError::Io(e.to_string()))?;
            Ok(restored)
        } else {
            Ok(target)
        }
    }

    pub fn list_versions(&self, name: &str) -> Result<Vec<SkillVersion>> {
        let _ = self.skill_dir(name)?; // validate name even with no versions
        self.store.list_skill_versions(name)
    }

    fn current_version(&self, name: &str) -> Result<Option<u32>> {
        let _ = self.skill_dir(name)?; // validate
        let versions = self.store.list_skill_versions(name)?;
        Ok(versions.iter().map(|x| x.version).max())
    }
}

/// Reject names that could escape `skills_root` (or collide after
/// sanitisation). Names must be a single, non-empty path component.
pub fn validate_skill_name(name: &str) -> Result<()> {
    let name = name.trim();
    if name.is_empty() {
        return Err(EvolutionError::Validation(
            "skill name must not be empty".to_string(),
        ));
    }
    if name.contains('/')
        || name.contains('\\')
        || name.contains('\0')
        || name == "."
        || name == ".."
        || name.contains("..")
    {
        return Err(EvolutionError::Security(format!(
            "invalid skill name (path traversal): {name:?}"
        )));
    }
    Ok(())
}
