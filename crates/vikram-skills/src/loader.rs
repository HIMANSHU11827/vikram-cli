//! Progressive loading with context budgets.
//!
//! Stage A is the catalog (name + description only). Stage B is this module: the instructions,
//! capped so one skill cannot eat the window. Stage C is a single resource read on demand.
//! Every stage has a byte budget, and going over is a truncation with a marker, not a silent
//! cut that looks like the end of the file.

use std::fs;
use std::path::Path;

use crate::builtin;
use crate::discovery;
use crate::error::{Result, SkillError};
use crate::frontmatter::Frontmatter;
use crate::metadata::SkillMetadata;
use crate::paths;
use crate::skill::{Resource, ResourceKind, Scope, Skill, SkillSummary};

/// How deep a resource walk goes below a skill directory. `references/a/b/c.md` is plenty.
const RESOURCE_DEPTH: usize = 4;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Budget {
    /// Bytes of catalog listing (stage A) that may be rendered at once.
    pub catalog_bytes: usize,
    /// Bytes of one skill's instructions (stage B).
    pub instruction_bytes: usize,
    /// Bytes of one skill's instructions plus its listed resources.
    pub total_skill_bytes: usize,
    /// Bytes of a single resource read at stage C.
    pub resource_bytes: usize,
    pub max_resources: usize,
}

impl Default for Budget {
    fn default() -> Self {
        Budget {
            catalog_bytes: 16_000,
            instruction_bytes: 24_000,
            total_skill_bytes: 48_000,
            resource_bytes: 12_000,
            max_resources: 32,
        }
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct BudgetReport {
    pub catalog_bytes: usize,
    pub instruction_bytes: usize,
    pub resource_bytes: usize,
    pub total_bytes: usize,
    pub truncated: bool,
    pub omitted: usize,
}

#[derive(Clone, Debug)]
pub struct LoadedSkill {
    pub skill: Skill,
    /// Instructions as they may be placed in a prompt: possibly truncated.
    pub instructions: String,
    pub truncated: bool,
    pub instructions_bytes: usize,
    /// Resources that exist but were not listed because of the budget.
    pub omitted: Vec<String>,
}

#[derive(Clone, Debug)]
pub struct Loader {
    pub budget: Budget,
}

impl Loader {
    pub fn new(budget: Budget) -> Loader {
        Loader { budget }
    }

    /// Stage B. Reads `SKILL.md`, parses the metadata, enumerates resources, applies budgets.
    pub fn load(&self, summary: &SkillSummary) -> Result<LoadedSkill> {
        let name = summary.name.as_str().to_string();
        let text = match summary.scope {
            Scope::Builtin => match builtin::find(&name) {
                Some(skill) => skill.content.to_string(),
                None => return Err(SkillError::NotFound { name, sources: 1 }),
            },
            _ => {
                let file = discovery::find_skill_file(&summary.dir).ok_or_else(|| {
                    SkillError::NotFound {
                        name: name.clone(),
                        sources: 1,
                    }
                })?;
                fs::read_to_string(&file).map_err(|err| SkillError::io(&file, &err))?
            }
        };

        let skill_file = summary.dir.join("SKILL.md");
        let frontmatter = Frontmatter::parse_at(&text, &skill_file)?;
        let (metadata, _warnings) = SkillMetadata::from_frontmatter(&frontmatter, &name)?;

        let mut all_resources = Vec::new();
        if summary.scope != Scope::Builtin {
            walk_resources(&summary.dir, &summary.dir, 0, &mut all_resources)?;
            all_resources.sort_by(|a, b| a.rel.cmp(&b.rel));
        }

        let notice = all_resources
            .iter()
            .find(|r| r.kind == ResourceKind::Reference)
            .map(|r| r.rel.clone())
            .unwrap_or_else(|| "references/<file>".to_string());
        let (instructions, truncated) =
            truncate_lines(&frontmatter.body, self.budget.instruction_bytes, &notice);

        let mut listed: Vec<Resource> = Vec::new();
        let mut omitted: Vec<String> = Vec::new();
        let mut used = 0usize;
        for resource in all_resources {
            let size = resource.bytes as usize;
            if listed.len() >= self.budget.max_resources
                || used.saturating_add(size) > self.budget.resource_bytes
            {
                omitted.push(resource.rel);
                continue;
            }
            used += size;
            listed.push(resource);
        }
        let mut total = instructions.len();
        for resource in &listed {
            total = total.saturating_add(listing_cost(resource));
        }
        while total > self.budget.total_skill_bytes && !listed.is_empty() {
            if let Some(dropped) = listed.pop() {
                total = total.saturating_sub(listing_cost(&dropped));
                omitted.push(dropped.rel);
            }
        }
        omitted.sort();

        Ok(LoadedSkill {
            skill: Skill {
                summary: summary.clone(),
                metadata,
                instructions: frontmatter.body.clone(),
                frontmatter,
                resources: listed,
            },
            instructions_bytes: instructions.len(),
            instructions,
            truncated,
            omitted,
        })
    }

    /// Stage C. A reference, template or asset inside the skill directory.
    pub fn read_resource(&self, skill: &Skill, rel: &str) -> Result<String> {
        if ResourceKind::of(rel) == ResourceKind::Script {
            return Err(SkillError::UntrustedScript {
                name: skill.summary.name.as_str().to_string(),
                level: skill.summary.trust,
            });
        }
        self.read_text(skill, rel, self.budget.resource_bytes)
    }

    /// Stage C for a script. Refused outright at trust levels that do not allow scripts; the
    /// content is never executed here regardless.
    pub fn read_script(&self, skill: &Skill, rel: &str) -> Result<String> {
        if !skill.summary.trust.allows_scripts() {
            return Err(SkillError::UntrustedScript {
                name: skill.summary.name.as_str().to_string(),
                level: skill.summary.trust,
            });
        }
        self.read_text(skill, rel, self.budget.instruction_bytes)
    }

    fn read_text(&self, skill: &Skill, rel: &str, limit: usize) -> Result<String> {
        let name = skill.summary.name.as_str().to_string();
        if skill.summary.scope == Scope::Builtin {
            return Err(SkillError::MissingResource {
                name,
                rel: rel.to_string(),
            });
        }
        let path = paths::resolve_checked(&skill.summary.dir, rel, false)?;
        if !path.is_file() {
            return Err(SkillError::MissingResource {
                name,
                rel: rel.to_string(),
            });
        }
        let text = fs::read_to_string(&path).map_err(|err| SkillError::io(&path, &err))?;
        if text.len() > limit {
            return Err(SkillError::TooLarge {
                what: format!("resource `{rel}` of `{name}`"),
                size: text.len(),
                limit,
            });
        }
        Ok(text)
    }

    pub fn report(&self, loaded: &LoadedSkill) -> BudgetReport {
        let resource_bytes: usize = loaded.skill.resources.iter().map(listing_cost).sum();
        BudgetReport {
            catalog_bytes: self.budget.catalog_bytes,
            instruction_bytes: loaded.instructions_bytes,
            resource_bytes,
            total_bytes: loaded.instructions_bytes.saturating_add(resource_bytes),
            truncated: loaded.truncated || !loaded.omitted.is_empty(),
            omitted: loaded.omitted.len(),
        }
    }

    /// Stage A listing: one line per skill, capped at the catalog budget.
    pub fn catalog_listing(&self, summaries: &[&SkillSummary]) -> (String, bool) {
        let mut out = String::new();
        let mut truncated = false;
        for summary in summaries {
            let line = format!(
                "- {}: {} [{}]\n",
                summary.name.as_str(),
                summary.description,
                summary.scope.label()
            );
            if out.len() + line.len() > self.budget.catalog_bytes {
                truncated = true;
                break;
            }
            out.push_str(&line);
        }
        if truncated {
            out.push_str(&format!(
                "… (catalog truncated at {} bytes)\n",
                self.budget.catalog_bytes
            ));
        }
        (out, truncated)
    }
}

/// What one listed resource costs against the total stage-B budget: its byte size plus the path
/// printed in its listing line. Stage B does not read resource content, so the listing text is
/// the honest cost rather than the file.
fn listing_cost(resource: &Resource) -> usize {
    (resource.bytes as usize).saturating_add(resource.rel.len())
}

fn relative(root: &Path, path: &Path) -> String {
    let rel = path.strip_prefix(root).unwrap_or(path);
    rel.components()
        .filter_map(|c| match c {
            std::path::Component::Normal(part) => Some(part.to_string_lossy().into_owned()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("/")
}

fn walk_resources(root: &Path, dir: &Path, depth: usize, out: &mut Vec<Resource>) -> Result<()> {
    for path in paths::sorted_entries(dir)? {
        let file = match path.file_name() {
            Some(name) => name.to_string_lossy().into_owned(),
            None => continue,
        };
        if paths::default_excludes().iter().any(|x| x == &file) {
            continue;
        }
        let meta = fs::symlink_metadata(&path).map_err(|err| SkillError::io(&path, &err))?;
        let kind = meta.file_type();
        if kind.is_symlink() {
            continue;
        }
        if kind.is_dir() {
            if depth < RESOURCE_DEPTH {
                walk_resources(root, &path, depth + 1, out)?;
            }
            continue;
        }
        if !kind.is_file() {
            continue;
        }
        if file.eq_ignore_ascii_case("SKILL.md") {
            continue;
        }
        let rel = relative(root, &path);
        out.push(Resource {
            kind: ResourceKind::of(&rel),
            rel,
            bytes: meta.len(),
        });
    }
    Ok(())
}

fn truncate_lines(text: &str, limit: usize, notice: &str) -> (String, bool) {
    if text.len() <= limit {
        return (text.to_string(), false);
    }
    let mut end = limit.min(text.len());
    while end > 0 && !text.is_char_boundary(end) {
        end -= 1;
    }
    let cut = text
        .get(..end)
        .and_then(|slice| slice.rfind('\n'))
        .map(|index| index + 1)
        .unwrap_or(end);
    let mut out = text.get(..cut).unwrap_or("").trim_end().to_string();
    out.push('\n');
    out.push_str(&format!(
        "… (truncated at {limit} bytes; read {notice} for the rest)"
    ));
    (out, true)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::skill::{Compat, TrustLevel};
    use std::path::PathBuf;

    fn summary(dir: &Path, name: &str, trust: TrustLevel) -> SkillSummary {
        SkillSummary {
            name: crate::skill::SkillId::new(name).expect("id"),
            description: "d".to_string(),
            dir: dir.to_path_buf(),
            scope: Scope::User,
            compat: Compat::Native,
            trust,
            enabled: true,
            valid: true,
            version: None,
            categories: Vec::new(),
            tags: Vec::new(),
        }
    }

    fn write_skill(dir: &Path, body: &str) {
        fs::create_dir_all(dir).expect("dirs");
        fs::write(
            dir.join("SKILL.md"),
            format!("---\nname: alpha\ndescription: d\n---\n{body}"),
        )
        .expect("write");
    }

    #[test]
    fn loads_instructions_and_metadata() {
        let tmp = tempfile::tempdir().expect("temp");
        let dir = tmp.path().join("alpha");
        write_skill(&dir, "# Alpha\n\nDo the thing.\n");
        let loader = Loader::new(Budget::default());
        let loaded = loader
            .load(&summary(&dir, "alpha", TrustLevel::UserTrusted))
            .expect("load");
        assert_eq!(loaded.skill.metadata.name, "alpha");
        assert!(loaded.instructions.contains("Do the thing."));
        assert!(!loaded.truncated);
        assert_eq!(loaded.instructions_bytes, loaded.instructions.len());
        assert!(loaded.omitted.is_empty());
    }

    #[test]
    fn enumerates_resources_sorted_and_without_skill_file() {
        let tmp = tempfile::tempdir().expect("temp");
        let dir = tmp.path().join("alpha");
        write_skill(&dir, "body");
        fs::create_dir_all(dir.join("references")).expect("dirs");
        fs::create_dir_all(dir.join("scripts")).expect("dirs");
        fs::write(dir.join("references/b.md"), "b").expect("write");
        fs::write(dir.join("references/a.md"), "a").expect("write");
        fs::write(dir.join("scripts/run.sh"), "echo hi").expect("write");

        let loader = Loader::new(Budget::default());
        let loaded = loader
            .load(&summary(&dir, "alpha", TrustLevel::UserTrusted))
            .expect("load");
        let rels: Vec<&str> = loaded
            .skill
            .resources
            .iter()
            .map(|r| r.rel.as_str())
            .collect();
        assert_eq!(
            rels,
            vec!["references/a.md", "references/b.md", "scripts/run.sh"]
        );
        let kinds: Vec<ResourceKind> = loaded.skill.resources.iter().map(|r| r.kind).collect();
        assert_eq!(
            kinds,
            vec![
                ResourceKind::Reference,
                ResourceKind::Reference,
                ResourceKind::Script
            ]
        );
    }

    #[test]
    fn instruction_budget_truncates_on_a_line_boundary_with_a_marker() {
        let tmp = tempfile::tempdir().expect("temp");
        let dir = tmp.path().join("alpha");
        let body: String = (0..200)
            .map(|i| format!("line {i} of the body\n"))
            .collect();
        write_skill(&dir, &body);
        let loader = Loader::new(Budget {
            instruction_bytes: 120,
            ..Budget::default()
        });
        let loaded = loader
            .load(&summary(&dir, "alpha", TrustLevel::UserTrusted))
            .expect("load");
        assert!(loaded.truncated);
        assert!(loaded.instructions.len() <= 200);
        assert!(loaded.instructions.contains("(truncated at 120 bytes"));
        assert!(loaded.instructions.contains("for the rest)"));
        assert!(!loaded.instructions.contains("line 199"));
        // Whole lines only: every line before the marker is a complete body line.
        for line in loaded.instructions.lines().take(3) {
            assert!(line.starts_with("line "), "{line}");
        }
        assert_eq!(loaded.skill.instructions, body);
    }

    #[test]
    fn resource_count_and_byte_budgets_move_extras_to_omitted() {
        let tmp = tempfile::tempdir().expect("temp");
        let dir = tmp.path().join("alpha");
        write_skill(&dir, "body");
        for i in 0..5 {
            fs::write(dir.join(format!("r{i}.md")), "0123456789").expect("write");
        }
        let loader = Loader::new(Budget {
            max_resources: 2,
            ..Budget::default()
        });
        let loaded = loader
            .load(&summary(&dir, "alpha", TrustLevel::UserTrusted))
            .expect("load");
        assert_eq!(loaded.skill.resources.len(), 2);
        assert_eq!(loaded.omitted.len(), 3);
        assert_eq!(loaded.omitted[0], "r2.md");

        let loader = Loader::new(Budget {
            resource_bytes: 25,
            ..Budget::default()
        });
        let loaded = loader
            .load(&summary(&dir, "alpha", TrustLevel::UserTrusted))
            .expect("load");
        assert_eq!(loaded.skill.resources.len(), 2);
        assert_eq!(loaded.omitted.len(), 3);
    }

    #[test]
    fn total_skill_budget_drops_resources_from_the_end() {
        let tmp = tempfile::tempdir().expect("temp");
        let dir = tmp.path().join("alpha");
        write_skill(&dir, "0123456789");
        for name in ["a.md", "b.md", "c.md"] {
            fs::write(dir.join(name), "0123456789").expect("write");
        }
        let loader = Loader::new(Budget {
            instruction_bytes: 1000,
            total_skill_bytes: 40,
            ..Budget::default()
        });
        let loaded = loader
            .load(&summary(&dir, "alpha", TrustLevel::UserTrusted))
            .expect("load");
        assert!(loaded.skill.resources.len() < 3);
        assert!(!loaded.omitted.is_empty());
        let report = loader.report(&loaded);
        assert!(report.total_bytes <= 40);
        assert!(report.truncated);
        assert_eq!(report.omitted, loaded.omitted.len());
    }

    #[test]
    fn read_resource_returns_content_and_refuses_escapes() {
        let tmp = tempfile::tempdir().expect("temp");
        let dir = tmp.path().join("alpha");
        write_skill(&dir, "body");
        fs::create_dir_all(dir.join("references")).expect("dirs");
        fs::write(dir.join("references/api.md"), "# API\n").expect("write");
        std::fs::write(tmp.path().join("secret.txt"), "secret").expect("write");

        let loader = Loader::new(Budget::default());
        let loaded = loader
            .load(&summary(&dir, "alpha", TrustLevel::UserTrusted))
            .expect("load");
        assert_eq!(
            loader
                .read_resource(&loaded.skill, "references/api.md")
                .expect("read"),
            "# API\n"
        );
        let escaped = loader
            .read_resource(&loaded.skill, "../secret.txt")
            .expect_err("escape");
        assert!(matches!(escaped, SkillError::PathEscape { .. }));
        let missing = loader
            .read_resource(&loaded.skill, "references/nope.md")
            .expect_err("missing");
        assert!(matches!(missing, SkillError::MissingResource { .. }));
    }

    #[test]
    fn read_resource_refuses_scripts_and_untrusted_scripts_are_refused_too() {
        let tmp = tempfile::tempdir().expect("temp");
        let dir = tmp.path().join("alpha");
        write_skill(&dir, "body");
        fs::create_dir_all(dir.join("scripts")).expect("dirs");
        fs::write(dir.join("scripts/run.sh"), "echo hi").expect("write");

        let loader = Loader::new(Budget::default());
        let trusted = loader
            .load(&summary(&dir, "alpha", TrustLevel::UserTrusted))
            .expect("load");
        assert!(matches!(
            loader.read_resource(&trusted.skill, "scripts/run.sh"),
            Err(SkillError::UntrustedScript { .. })
        ));
        assert_eq!(
            loader
                .read_script(&trusted.skill, "scripts/run.sh")
                .expect("script"),
            "echo hi"
        );

        let untrusted = loader
            .load(&summary(&dir, "alpha", TrustLevel::Community))
            .expect("load");
        assert!(matches!(
            loader.read_script(&untrusted.skill, "scripts/run.sh"),
            Err(SkillError::UntrustedScript { .. })
        ));
    }

    #[test]
    fn a_resource_over_the_byte_budget_is_an_error_not_a_truncation() {
        let tmp = tempfile::tempdir().expect("temp");
        let dir = tmp.path().join("alpha");
        write_skill(&dir, "body");
        fs::create_dir_all(dir.join("references")).expect("dirs");
        fs::write(dir.join("references/big.md"), "x".repeat(500)).expect("write");
        let loader = Loader::new(Budget {
            resource_bytes: 100,
            ..Budget::default()
        });
        let loaded = loader
            .load(&summary(&dir, "alpha", TrustLevel::UserTrusted))
            .expect("load");
        let err = loader
            .read_resource(&loaded.skill, "references/big.md")
            .expect_err("too large");
        assert!(matches!(err, SkillError::TooLarge { limit: 100, .. }));
    }

    #[test]
    fn builtins_load_from_the_embedded_copy() {
        let tmp = tempfile::tempdir().expect("temp");
        let loader = Loader::new(Budget::default());
        let builtin = SkillSummary {
            name: crate::skill::SkillId::new("skill-author").expect("id"),
            description: "d".to_string(),
            dir: builtin::root().join("skill-author"),
            scope: Scope::Builtin,
            compat: Compat::Native,
            trust: TrustLevel::Builtin,
            enabled: true,
            valid: true,
            version: None,
            categories: Vec::new(),
            tags: Vec::new(),
        };
        let loaded = loader.load(&builtin).expect("builtin");
        assert!(loaded.instructions.contains("# Skill author"));
        assert!(loaded.skill.resources.is_empty());
        assert!(matches!(
            loader.read_resource(&loaded.skill, "references/x.md"),
            Err(SkillError::MissingResource { .. })
        ));
        assert!(tmp.path().exists());
    }

    #[test]
    fn a_missing_skill_directory_is_not_found() {
        let tmp = tempfile::tempdir().expect("temp");
        let loader = Loader::new(Budget::default());
        let err = loader
            .load(&summary(
                &tmp.path().join("ghost"),
                "ghost",
                TrustLevel::UserTrusted,
            ))
            .expect_err("missing");
        assert!(matches!(err, SkillError::NotFound { .. }));
    }

    #[test]
    fn catalog_listing_respects_its_own_budget() {
        let loader = Loader::new(Budget {
            catalog_bytes: 25,
            ..Budget::default()
        });
        let dir = PathBuf::from("/x");
        let a = summary(&dir, "alpha", TrustLevel::UserTrusted);
        let b = summary(&dir, "beta", TrustLevel::UserTrusted);
        let (listing, truncated) = loader.catalog_listing(&[&a, &b]);
        assert!(truncated);
        assert!(listing.contains("alpha"));
        assert!(!listing.contains("beta"));
        assert!(listing.contains("catalog truncated at 25 bytes"));
    }
}
