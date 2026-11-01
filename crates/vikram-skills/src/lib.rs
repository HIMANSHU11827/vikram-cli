//! Skill system for VIKRAM: discovery, progressive loading, activation, management,
//! bundles, installation and history.
//!
//! A skill is a directory containing `SKILL.md`. The file starts with a frontmatter block
//! delimited by `---` lines; the rest is markdown instructions. Nothing in this crate
//! executes skill content: it parses, validates, copies and describes it.
//!
//! The public surface is fixed by `docs/skills/skills-api-contract.md`. Everything else in
//! here is private on purpose, so the runtime/cli/tui cannot reach past that surface.

#![cfg_attr(
    not(test),
    deny(
        clippy::unwrap_used,
        clippy::expect_used,
        clippy::panic,
        clippy::todo
    )
)]

mod builtin;

pub mod activation;
pub mod bundle;
pub mod discovery;
pub mod error;
pub mod frontmatter;
pub mod history;
pub mod install;
pub mod loader;
pub mod manager;
pub mod metadata;
pub mod paths;
pub mod skill;
pub mod trust;

pub use activation::{
    Activation, ActivationSet, ActivationSource, Selection, SelectionInput, Selector,
};
pub use bundle::{Bundle, BundleIndex};
pub use discovery::{Catalog, CatalogStats, Diagnostic, DiagnosticLevel, DiscoveryOptions};
pub use error::{Result, SkillError};
pub use frontmatter::{Frontmatter, Yaml};
pub use history::{History, Manifest, hash_dir};
pub use install::{InstallOutcome, InstallSource, Installer};
pub use loader::{Budget, BudgetReport, LoadedSkill, Loader};
pub use manager::Manager;
pub use metadata::{ActivationPolicy, DESCRIPTION_MAX, NAME_MAX, SkillMetadata, VikramExtension};
pub use paths::{
    bundles_dir, disabled_path, history_dir, home, project_skill_dirs, provenance_path,
    resolve_within, state_dir, user_skill_dirs, user_skills_dir,
};
pub use skill::{Compat, Resource, ResourceKind, Scope, Skill, SkillId, SkillSummary, TrustLevel};
pub use trust::TrustPolicy;
