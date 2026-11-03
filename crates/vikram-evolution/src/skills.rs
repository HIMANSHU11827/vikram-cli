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

    fn skill_dir(&self, name: &str) -> PathBuf {
        // Guard against path traversal: reject names with separators or "..".
        let safe = name.replace(['/', '\\'], "");
        self.skills_root.join(safe)
    }
    fn skill_file(&self, name: &str) -> PathBuf {
        self.skill_dir(name).join("SKILL.md")
    }

    pub fn discover(&self) -> Vec<String> {
        let mut out = Vec::new();
        if let Ok(rd) = fs::read_dir(&self.skills_root) {
            for e in rd.flatten() {
                if e.path().is_dir() && e.path().join("SKILL.md").exists() {
                    out.push(e.file_name().to_string_lossy().to_string());
                }
            }
        }
        out
    }

    pub fn read_skill(&self, name: &str) -> Result<String> {
        let p = self.skill_file(name);
        fs::read_to_string(&p).map_err(|e| EvolutionError::Io(e.to_string()))
    }

    pub fn create_skill(
        &self,
        name: &str,
        content: &str,
        change_desc: &str,
    ) -> Result<SkillVersion> {
        crate::redact::scan_injection(content)?;
        let dir = self.skill_dir(name);
        fs::create_dir_all(&dir).map_err(|e| EvolutionError::Io(e.to_string()))?;
        let versions_dir = dir.join(".versions");
        fs::create_dir_all(&versions_dir).ok();
        let v = SkillVersion {
            skill_name: name.to_string(),
            version: 1,
            content: content.to_string(),
            created_ms: now_ms(),
            parent_version: None,
            change_description: change_desc.to_string(),
        };
        fs::write(self.skill_file(name), content).map_err(|e| EvolutionError::Io(e.to_string()))?;
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
        let current = self.current_version(name).unwrap_or(0);
        let next = current + 1;
        let dir = self.skill_dir(name);
        let versions_dir = dir.join(".versions");
        fs::create_dir_all(&versions_dir).ok();
        if let Ok(old) = self.read_skill(name) {
            let backup = versions_dir.join(format!("v{}.md", current.max(1)));
            let _ = fs::write(&backup, old);
        }
        let v = SkillVersion {
            skill_name: name.to_string(),
            version: next,
            content: new_content.to_string(),
            created_ms: now_ms(),
            parent_version: if current == 0 { None } else { Some(current) },
            change_description: change_desc.to_string(),
        };
        fs::write(self.skill_file(name), new_content)
            .map_err(|e| EvolutionError::Io(e.to_string()))?;
        self.store.save_skill_version(&v)?;
        Ok(v)
    }

    pub fn rollback(&self, name: &str, version: u32) -> Result<()> {
        let versions = self.store.list_skill_versions(name)?;
        let v = versions
            .into_iter()
            .find(|x| x.version == version)
            .ok_or_else(|| EvolutionError::NotFound(format!("version {version}")))?;
        fs::write(self.skill_file(name), &v.content)
            .map_err(|e| EvolutionError::Io(e.to_string()))?;
        Ok(())
    }

    pub fn list_versions(&self, name: &str) -> Result<Vec<SkillVersion>> {
        self.store.list_skill_versions(name)
    }

    fn current_version(&self, name: &str) -> Option<u32> {
        self.store
            .list_skill_versions(name)
            .ok()
            .and_then(|v| v.iter().map(|x| x.version).max())
    }
}
