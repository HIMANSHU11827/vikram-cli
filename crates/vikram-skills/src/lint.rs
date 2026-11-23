//! Checks a skill against the budgets the loader enforces, so a skill written by the assistant is
//! judged before it is offered rather than silently truncated when it is used.
//!
//! The loader already refuses what it cannot fit. This says *why* first: a body that will be cut,
//! a resource nobody will ever be allowed to read, a description that reaches the catalog empty, a
//! skill that has grown into a shelf of references instead of one set of instructions.

use crate::loader::Budget;
use crate::skill::{Resource, ResourceKind, Skill};

pub const REFERENCE_SPRAWL: usize = 60;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Severity {
    Warning,
    Error,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum LintIssue {
    BlankDescription,
    BlankInstructions,
    InstructionsTooLong {
        bytes: usize,
        limit: usize,
    },
    InstructionsPlusResourcesTooLong {
        bytes: usize,
        limit: usize,
    },
    TooManyResources {
        count: usize,
        limit: usize,
    },
    ResourceTooLarge {
        rel: String,
        bytes: u64,
        limit: usize,
    },
    ReferenceSprawl {
        count: usize,
        limit: usize,
    },
}

impl LintIssue {
    pub fn severity(&self) -> Severity {
        match self {
            LintIssue::BlankDescription
            | LintIssue::BlankInstructions
            | LintIssue::ReferenceSprawl { .. } => Severity::Warning,
            LintIssue::InstructionsTooLong { .. }
            | LintIssue::InstructionsPlusResourcesTooLong { .. }
            | LintIssue::TooManyResources { .. }
            | LintIssue::ResourceTooLarge { .. } => Severity::Error,
        }
    }

    pub fn describe(&self) -> String {
        match self {
            LintIssue::BlankDescription => {
                "the description is empty, so the catalog line says nothing about it".to_string()
            }
            LintIssue::BlankInstructions => {
                "the body is empty, so only the description reaches the prompt".to_string()
            }
            LintIssue::InstructionsTooLong { bytes, limit } => {
                format!("the body is {bytes} bytes; it will be cut at {limit} when it is used")
            }
            LintIssue::InstructionsPlusResourcesTooLong { bytes, limit } => {
                format!("body plus resources come to {bytes} bytes; the skill budget is {limit}")
            }
            LintIssue::TooManyResources { count, limit } => {
                format!("{count} resources; at most {limit} are ever listed")
            }
            LintIssue::ResourceTooLarge { rel, bytes, limit } => {
                format!("`{rel}` is {bytes} bytes and cannot be read: the limit is {limit}")
            }
            LintIssue::ReferenceSprawl { count, limit } => {
                format!("{count} reference files; past {limit} this should be two skills, not one")
            }
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SkillLint {
    pub name: String,
    pub issues: Vec<LintIssue>,
}

impl SkillLint {
    pub fn is_clean(&self) -> bool {
        self.issues.is_empty()
    }

    pub fn errors(&self) -> usize {
        self.count(Severity::Error)
    }

    pub fn warnings(&self) -> usize {
        self.count(Severity::Warning)
    }

    fn count(&self, severity: Severity) -> usize {
        self.issues
            .iter()
            .filter(|issue| issue.severity() == severity)
            .count()
    }
}

pub fn lint(skill: &Skill, budget: &Budget) -> SkillLint {
    SkillLint {
        name: skill.summary.name.as_str().to_string(),
        issues: lint_parts(
            &skill.summary.description,
            &skill.instructions,
            &skill.resources,
            budget,
        ),
    }
}

pub fn lint_parts(
    description: &str,
    instructions: &str,
    resources: &[Resource],
    budget: &Budget,
) -> Vec<LintIssue> {
    let mut issues = Vec::new();
    if description.trim().is_empty() {
        issues.push(LintIssue::BlankDescription);
    }
    if instructions.trim().is_empty() {
        issues.push(LintIssue::BlankInstructions);
    }
    if instructions.len() > budget.instruction_bytes {
        issues.push(LintIssue::InstructionsTooLong {
            bytes: instructions.len(),
            limit: budget.instruction_bytes,
        });
    }
    let spent: usize = instructions
        .len()
        .saturating_add(resources.iter().map(listed_cost).sum());
    if spent > budget.total_skill_bytes {
        issues.push(LintIssue::InstructionsPlusResourcesTooLong {
            bytes: spent,
            limit: budget.total_skill_bytes,
        });
    }
    if resources.len() > budget.max_resources {
        issues.push(LintIssue::TooManyResources {
            count: resources.len(),
            limit: budget.max_resources,
        });
    }
    for resource in resources {
        if resource.bytes as usize > budget.resource_bytes {
            issues.push(LintIssue::ResourceTooLarge {
                rel: resource.rel.clone(),
                bytes: resource.bytes,
                limit: budget.resource_bytes,
            });
        }
    }
    let references = resources
        .iter()
        .filter(|resource| resource.kind == ResourceKind::Reference)
        .count();
    if references > REFERENCE_SPRAWL {
        issues.push(LintIssue::ReferenceSprawl {
            count: references,
            limit: REFERENCE_SPRAWL,
        });
    }
    issues
}

fn listed_cost(resource: &Resource) -> usize {
    (resource.bytes as usize).saturating_add(resource.rel.len())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::skill::{Compat, Scope, SkillId, SkillSource, SkillSummary, TrustLevel};
    use std::path::PathBuf;

    fn resource(rel: &str, kind: ResourceKind, bytes: u64) -> Resource {
        Resource {
            rel: rel.to_string(),
            kind,
            bytes,
        }
    }

    #[test]
    fn a_skill_within_every_budget_is_clean() {
        let issues = lint_parts(
            "Explains the release checklist",
            "# Release\n\nRun the script.\n",
            &[resource("scripts/release.sh", ResourceKind::Script, 400)],
            &Budget::default(),
        );
        assert!(issues.is_empty(), "{issues:?}");
    }

    #[test]
    fn an_unreadable_resource_is_an_error() {
        let issues = lint_parts(
            "d",
            "body",
            &[resource(
                "references/huge.md",
                ResourceKind::Reference,
                20_000,
            )],
            &Budget::default(),
        );
        assert_eq!(
            issues,
            vec![LintIssue::ResourceTooLarge {
                rel: "references/huge.md".to_string(),
                bytes: 20_000,
                limit: 12_000,
            }]
        );
        assert_eq!(issues[0].severity(), Severity::Error);
    }

    #[test]
    fn a_body_the_loader_would_cut_is_reported_before_it_is_offered() {
        let body = "x".repeat(30_000);
        let issues = lint_parts("d", &body, &[], &Budget::default());
        assert_eq!(
            issues,
            vec![LintIssue::InstructionsTooLong {
                bytes: 30_000,
                limit: 24_000,
            }]
        );
    }

    #[test]
    fn an_empty_description_and_body_are_warnings_not_errors() {
        let issues = lint_parts("  ", "\n", &[], &Budget::default());
        assert_eq!(
            issues,
            vec![LintIssue::BlankDescription, LintIssue::BlankInstructions]
        );
        assert_eq!(Severity::Warning, issues[0].severity());
    }

    #[test]
    fn a_reference_shelf_instead_of_a_skill_is_flagged() {
        let references: Vec<Resource> = (0..REFERENCE_SPRAWL + 1)
            .map(|i| resource(&format!("references/r{i}.md"), ResourceKind::Reference, 10))
            .collect();
        let issues = lint_parts(
            "d",
            "body",
            &references,
            &Budget {
                max_resources: 200,
                ..Budget::default()
            },
        );
        assert_eq!(
            issues,
            vec![LintIssue::ReferenceSprawl {
                count: REFERENCE_SPRAWL + 1,
                limit: REFERENCE_SPRAWL,
            }]
        );
    }

    #[test]
    fn the_report_counts_errors_and_warnings_separately() {
        let report = SkillLint {
            name: "alpha".to_string(),
            issues: vec![
                LintIssue::BlankDescription,
                LintIssue::ReferenceSprawl {
                    count: 61,
                    limit: REFERENCE_SPRAWL,
                },
                LintIssue::TooManyResources {
                    count: 40,
                    limit: 32,
                },
            ],
        };
        assert_eq!(report.errors(), 1);
        assert_eq!(report.warnings(), 2);
        assert!(!report.is_clean());
    }

    #[test]
    fn a_loaded_skill_carries_its_own_name_into_the_report() {
        let dir = std::env::temp_dir();
        let summary = SkillSummary {
            name: SkillId::new("alpha").expect("id"),
            description: String::new(),
            dir: dir.clone(),
            scope: Scope::User,
            compat: Compat::Native,
            trust: TrustLevel::UserTrusted,
            enabled: true,
            valid: true,
            version: None,
            categories: Vec::new(),
            tags: Vec::new(),
            source: SkillSource::User,
        };
        let skill = Skill {
            summary,
            metadata: crate::metadata::SkillMetadata::new("alpha", ""),
            instructions: "body".to_string(),
            frontmatter: crate::frontmatter::Frontmatter::default(),
            resources: Vec::new(),
        };
        let report = lint(&skill, &Budget::default());
        assert_eq!(report.name, "alpha");
        assert_eq!(report.issues, vec![LintIssue::BlankDescription]);
        assert_eq!(PathBuf::from(&skill.summary.dir), dir);
    }
}
