use crate::error::{EvolutionError, Result};
use crate::store::EvolutionStore;
use crate::types::{SkillVersion, now_ms};
use std::fs;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};

/// Disambiguates temp files for [`SkillEvolution::write_skill_file`] so
/// concurrent writers to different skills (or retries) never collide.
static SKILL_TMP_SEQ: AtomicU64 = AtomicU64::new(0);

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

    /// Record `v` in the version table, then materialize `SKILL.md`.
    ///
    /// The DB row is written first because it carries the full content —
    /// it is the durable record of intent and `SKILL.md` is derived from
    /// it. If materialization fails the intent row is removed so the two
    /// never disagree after a *handled* error. A hard crash between the
    /// two leaves the DB ahead, which is the recoverable direction: the
    /// row holds the exact content needed to rewrite `SKILL.md`, whereas
    /// the old file-first order could leave file content missing from the
    /// version history permanently.
    fn commit_version(&self, v: &SkillVersion) -> Result<()> {
        self.store.save_skill_version(v)?;
        if let Err(e) = self.write_skill_file(&v.skill_name, &v.content) {
            // Best-effort compensation; if even this fails the DB stays
            // ahead — still recoverable, since the row has the content.
            let _ = self.store.delete_skill_version(&v.skill_name, v.version);
            return Err(e);
        }
        Ok(())
    }

    /// Write `content` as the skill's `SKILL.md`.
    ///
    /// `skills_root` is the same directory `vikram-skills` discovery scans, so the file
    /// must carry the frontmatter discovery requires (`name` + `description`) or the skill
    /// is written but never loads. Content that already opens with a frontmatter block is
    /// written verbatim — the caller owns its validity; bare content is wrapped with a
    /// generated header.
    ///
    /// Goes through a sibling temp file + rename so a failed write never
    /// leaves a truncated `SKILL.md` behind (rename is atomic on the same
    /// filesystem), which is what makes [`Self::commit_version`]'s
    /// compensation honest.
    fn write_skill_file(&self, name: &str, content: &str) -> Result<()> {
        let dir = self.skill_dir(name)?;
        fs::create_dir_all(&dir).map_err(|e| EvolutionError::Io(e.to_string()))?;
        let target = dir.join("SKILL.md");
        let document = skill_document(name, content);
        let seq = SKILL_TMP_SEQ.fetch_add(1, Ordering::Relaxed);
        let tmp = dir.join(format!(".SKILL.md.tmp-{}-{seq}", std::process::id()));
        fs::write(&tmp, document).map_err(|e| EvolutionError::Io(e.to_string()))?;
        if let Err(e) = fs::rename(&tmp, &target) {
            let _ = fs::remove_file(&tmp);
            return Err(EvolutionError::Io(e.to_string()));
        }
        Ok(())
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
        self.commit_version(&v)?;
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
        self.commit_version(&v)?;
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
        if current != version {
            let restored = SkillVersion {
                skill_name: name.to_string(),
                version: current + 1,
                content: target.content.clone(),
                created_ms: now_ms(),
                parent_version: Some(current),
                change_description: format!("rollback to v{version}"),
            };
            // Records the rollback as a new head version and materializes
            // SKILL.md with that version's content in one commit.
            self.commit_version(&restored)?;
            Ok(restored)
        } else {
            self.write_skill_file(name, &target.content)?;
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

/// Wrap bare skill content in the frontmatter block discovery requires. Content that already
/// opens with `---` is returned unchanged: it is a full document and its frontmatter is the
/// caller's responsibility.
fn skill_document(name: &str, content: &str) -> String {
    let trimmed = content.trim_start();
    if trimmed.starts_with("---\n") || trimmed.starts_with("---\r\n") {
        return content.to_string();
    }
    let description = content
        .lines()
        .map(str::trim)
        .find(|line| !line.is_empty() && !line.starts_with('#'))
        .unwrap_or(name);
    let description: String = description
        .chars()
        .filter(|c| !c.is_control())
        .take(200)
        .collect();
    let description = if description.trim().is_empty() {
        format!("{name} workflow")
    } else {
        // Double-quoted YAML scalar: safe for any single-line text after escaping.
        format!(
            "\"{}\"",
            description.replace('\\', "\\\\").replace('"', "\\\"")
        )
    };
    format!("---\nname: {name}\ndescription: {description}\n---\n{content}")
}

/// Reject names that could escape `skills_root` — or that skill discovery would refuse anyway:
/// the rules mirror `vikram-skills`' `SkillMetadata::validate_name` (lowercase ASCII
/// alphanumerics and `-`, starting alphanumeric, no `--`, at most 64 bytes) so a skill
/// evolution accepts is one discovery can actually load.
pub fn validate_skill_name(name: &str) -> Result<()> {
    let name = name.trim();
    if name.is_empty() {
        return Err(EvolutionError::Validation(
            "skill name must not be empty".to_string(),
        ));
    }
    let invalid = |reason: &str| {
        Err(EvolutionError::Validation(format!(
            "invalid skill name {name:?}: {reason}"
        )))
    };
    if name.len() > 64 {
        return invalid("longer than 64 bytes");
    }
    let first = name.chars().next().unwrap_or(' ');
    if !(first.is_ascii_lowercase() || first.is_ascii_digit()) {
        return invalid("must start with a lowercase ASCII letter or a digit");
    }
    if name.ends_with('-') {
        return invalid("must not end with '-'");
    }
    if name.contains("--") {
        return invalid("must not contain '--'");
    }
    if !name
        .chars()
        .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-')
    {
        return invalid("only lowercase letters, digits and '-' are allowed");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn engine() -> (SkillEvolution, tempfile::TempDir) {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("skills");
        fs::create_dir_all(&root).unwrap();
        (
            SkillEvolution::new(EvolutionStore::open_in_memory().unwrap(), root),
            tmp,
        )
    }

    #[test]
    fn create_improve_rollback_roundtrip() {
        let (engine, _tmp) = engine();
        let v1 = engine.create_skill("demo", "step1", "init").unwrap();
        assert_eq!(v1.version, 1);
        assert_eq!(
            engine.read_skill("demo").unwrap(),
            skill_document("demo", "step1")
        );

        let v2 = engine
            .improve_skill("demo", "step1\nstep2", "add step")
            .unwrap();
        assert_eq!(v2.version, 2);
        assert_eq!(
            engine.read_skill("demo").unwrap(),
            skill_document("demo", "step1\nstep2")
        );

        let restored = engine.rollback("demo", 1).unwrap();
        assert_eq!(restored.version, 3);
        assert_eq!(
            engine.read_skill("demo").unwrap(),
            skill_document("demo", "step1")
        );
        assert_eq!(engine.list_versions("demo").unwrap().len(), 3);
    }

    #[test]
    fn written_skill_carries_discoverable_frontmatter() {
        let (engine, _tmp) = engine();
        engine
            .create_skill("audit-flow", "run the audit checklist", "init")
            .unwrap();
        let doc = engine.read_skill("audit-flow").unwrap();
        assert!(doc.starts_with("---\nname: audit-flow\ndescription: "));
        assert!(doc.contains("\n---\nrun the audit checklist"));
        // The description line must be a single valid YAML scalar.
        let fm = doc.split("\n---\n").next().unwrap();
        assert!(fm.contains("description: \"run the audit checklist\""));
    }

    #[test]
    fn frontmatter_content_is_written_verbatim() {
        let full = "---\nname: demo\ndescription: caller-owned\n---\nbody\n";
        assert_eq!(skill_document("demo", full), full);
        // Leading whitespace before the fence still counts as a document.
        assert_eq!(
            skill_document("demo", &format!("  \n{full}")),
            format!("  \n{full}")
        );
    }

    #[test]
    fn skill_names_match_what_discovery_accepts() {
        for ok in ["demo", "audit-flow", "v2", "a1-b2"] {
            assert!(validate_skill_name(ok).is_ok(), "{ok} should be valid");
        }
        for bad in [
            "",
            "  ",
            "../escape",
            "a/b",
            "My_Skill",
            "UPPER",
            "a--b",
            "-lead",
            "trail-",
            "with space",
            "dot.name",
        ] {
            assert!(
                validate_skill_name(bad).is_err(),
                "{bad:?} must be rejected before it reaches disk"
            );
        }
        assert!(validate_skill_name(&"a".repeat(65)).is_err());
    }

    #[test]
    fn failed_file_write_leaves_no_version_row() {
        let (engine, _tmp) = engine();
        // A directory where SKILL.md should be makes the atomic rename
        // fail *after* the version row is recorded — the compensation must
        // remove that row so DB and disk never disagree.
        let blocked = engine.skills_root().join("blocked");
        fs::create_dir_all(blocked.join("SKILL.md")).unwrap();

        assert!(engine.create_skill("blocked", "content", "init").is_err());
        assert!(
            engine.list_versions("blocked").unwrap().is_empty(),
            "failed materialization must not leave a version row"
        );

        // Same for improve_skill on an existing skill.
        engine.create_skill("demo", "v1", "init").unwrap();
        let demo_md = engine.skills_root().join("demo").join("SKILL.md");
        fs::remove_file(&demo_md).unwrap();
        fs::create_dir_all(&demo_md).unwrap();
        assert!(engine.improve_skill("demo", "v2", "bump").is_err());
        assert_eq!(
            engine
                .list_versions("demo")
                .unwrap()
                .iter()
                .map(|v| v.version)
                .collect::<Vec<_>>(),
            vec![1],
            "only the original version survives a failed write"
        );
    }
}
