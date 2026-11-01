//! Builtin skills are compiled in, not read from disk.
//!
//! `include_str!` keeps them available in an installed binary that has no crate directory next
//! to it, and keeps discovery honest: it never has to write into a cache directory to make a
//! builtin visible.

use std::path::PathBuf;

pub struct BuiltinSkill {
    pub name: &'static str,
    pub content: &'static str,
}

const SKILLS: &[BuiltinSkill] = &[
    BuiltinSkill {
        name: "skill-author",
        content: include_str!("../builtin/skill-author/SKILL.md"),
    },
    BuiltinSkill {
        name: "skill-audit",
        content: include_str!("../builtin/skill-audit/SKILL.md"),
    },
];

pub fn catalog() -> &'static [BuiltinSkill] {
    SKILLS
}

/// The virtual root builtin summaries point at. It does not exist on disk on purpose: the
/// loader resolves builtin content from this module.
pub fn root() -> PathBuf {
    PathBuf::from("<builtin>")
}

pub fn find(name: &str) -> Option<&'static BuiltinSkill> {
    SKILLS.iter().find(|skill| skill.name == name)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::metadata::SkillMetadata;

    #[test]
    fn every_builtin_parses_and_validates() {
        for skill in catalog() {
            let fm = crate::frontmatter::Frontmatter::parse(skill.content)
                .unwrap_or_else(|e| panic!("{}: {e}", skill.name));
            let (meta, warnings) = SkillMetadata::from_frontmatter(&fm, skill.name)
                .unwrap_or_else(|e| panic!("{}: {e}", skill.name));
            assert_eq!(meta.name, skill.name);
            assert!(!meta.description.is_empty());
            assert!(
                warnings.is_empty(),
                "{} produced warnings: {warnings:?}",
                skill.name
            );
            assert!(SkillMetadata::validate_name(skill.name).is_ok());
        }
    }

    #[test]
    fn builtins_are_addressable_by_name() {
        assert!(find("skill-author").is_some());
        assert!(find("nope").is_none());
        assert!(root().ends_with("<builtin>"));
    }
}
