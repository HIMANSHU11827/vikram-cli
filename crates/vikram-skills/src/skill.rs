use std::borrow::Borrow;
use std::fmt;
use std::path::PathBuf;
use std::str::FromStr;

use serde::{Deserialize, Serialize};

use crate::error::{Result, SkillError};
use crate::frontmatter::Frontmatter;
use crate::metadata::SkillMetadata;

/// A validated skill name. The directory name is the identity, so this is also the only
/// thing that may be joined onto a skills root.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct SkillId(String);

impl SkillId {
    pub fn new(name: &str) -> Result<Self> {
        SkillMetadata::validate_name(name)?;
        Ok(Self(name.to_string()))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }

    pub fn into_string(self) -> String {
        self.0
    }
}

impl Borrow<str> for SkillId {
    fn borrow(&self) -> &str {
        &self.0
    }
}

impl AsRef<str> for SkillId {
    fn as_ref(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for SkillId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl FromStr for SkillId {
    type Err = SkillError;

    fn from_str(s: &str) -> Result<Self> {
        Self::new(s)
    }
}

impl TryFrom<&str> for SkillId {
    type Error = SkillError;

    fn try_from(value: &str) -> Result<Self> {
        Self::new(value)
    }
}

/// Where a skill came from. Precedence is the tie-breaker when two sources offer the same name.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Scope {
    Builtin,
    User,
    Project,
    Imported,
    External,
}

impl Scope {
    pub fn precedence(self) -> u8 {
        match self {
            Scope::Builtin => 50,
            Scope::User => 40,
            Scope::Project => 30,
            Scope::Imported => 20,
            Scope::External => 10,
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Scope::Builtin => "builtin",
            Scope::User => "user",
            Scope::Project => "project",
            Scope::Imported => "imported",
            Scope::External => "external",
        }
    }
}

/// Which layout convention a skill directory follows. The content is the same markdown; only
/// the directory it was found in differs.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Compat {
    Native,
    AgentsSkills,
    Claude,
    Codex,
    OpenCode,
    Hermes,
}

impl Compat {
    pub fn label(self) -> &'static str {
        match self {
            Compat::Native => "native",
            Compat::AgentsSkills => "agents",
            Compat::Claude => "claude",
            Compat::Codex => "codex",
            Compat::OpenCode => "opencode",
            Compat::Hermes => "hermes",
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TrustLevel {
    Builtin,
    Verified,
    UserTrusted,
    ProjectTrusted,
    Community,
    Untrusted,
}

impl TrustLevel {
    pub fn label(self) -> &'static str {
        match self {
            TrustLevel::Builtin => "builtin",
            TrustLevel::Verified => "verified",
            TrustLevel::UserTrusted => "user",
            TrustLevel::ProjectTrusted => "project",
            TrustLevel::Community => "community",
            TrustLevel::Untrusted => "untrusted",
        }
    }

    pub fn allows_scripts(self) -> bool {
        matches!(
            self,
            TrustLevel::Builtin | TrustLevel::Verified | TrustLevel::UserTrusted
        )
    }

    /// Activation is always permitted to be *proposed*; whether it happens is the policy's
    /// decision. Only content nobody vouches for is closed to activation outright.
    pub fn allows_activation(self) -> bool {
        !matches!(self, TrustLevel::Untrusted)
    }
}

/// The cheap stage-A record: enough to build a catalog entry and a prompt listing without
/// reading the instructions.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SkillSummary {
    pub name: SkillId,
    pub description: String,
    pub dir: PathBuf,
    pub scope: Scope,
    pub compat: Compat,
    pub trust: TrustLevel,
    pub enabled: bool,
    pub valid: bool,
    pub version: Option<String>,
    pub categories: Vec<String>,
    pub tags: Vec<String>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ResourceKind {
    Script,
    Reference,
    Asset,
    Template,
    AgentConfig,
    Other,
}

impl ResourceKind {
    /// Classification from the relative path. Extension-driven and deliberately dumb: a
    /// resource is never executed here, this only decides how a loader may present it.
    pub fn of(rel: &str) -> ResourceKind {
        let file = rel.rsplit('/').next().unwrap_or(rel);
        let lower = file.to_ascii_lowercase();
        let ext = lower.rsplit_once('.').map(|(_, e)| e).unwrap_or("");
        match ext {
            "sh" | "bash" | "zsh" | "fish" | "py" | "js" | "mjs" | "cjs" | "ts" | "rb" | "pl"
            | "ps1" | "lua" => ResourceKind::Script,
            "md" | "markdown" | "txt" | "rst" | "adoc" => ResourceKind::Reference,
            "png" | "jpg" | "jpeg" | "gif" | "svg" | "webp" | "ico" | "pdf" => ResourceKind::Asset,
            "tmpl" | "template" | "tpl" | "hbs" | "mustache" | "j2" => ResourceKind::Template,
            "agent" => ResourceKind::AgentConfig,
            _ => {
                if matches!(
                    lower.as_str(),
                    "agent.yaml" | "agent.json" | "agent.toml" | "agents.yaml" | "agents.json"
                ) {
                    ResourceKind::AgentConfig
                } else {
                    ResourceKind::Other
                }
            }
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Resource {
    /// Path relative to the skill directory, always `/` separated.
    pub rel: String,
    pub kind: ResourceKind,
    pub bytes: u64,
}

/// The full stage-B record: metadata plus instructions plus the resource inventory.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Skill {
    pub summary: SkillSummary,
    pub metadata: SkillMetadata,
    pub instructions: String,
    pub frontmatter: Frontmatter,
    pub resources: Vec<Resource>,
}

impl Skill {
    pub fn name(&self) -> &SkillId {
        &self.summary.name
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn skill_ids_reject_uppercase_and_spaces() {
        assert!(SkillId::new("code-review").is_ok());
        assert!(SkillId::new("Code-Review").is_err());
        assert!(SkillId::new("code review").is_err());
        assert!(SkillId::new("").is_err());
    }

    #[test]
    fn skill_ids_order_lexically_and_borrow_as_str() {
        let a = SkillId::new("alpha").expect("valid");
        let b = SkillId::new("beta").expect("valid");
        assert!(a < b);
        assert_eq!(a.as_str(), "alpha");
        let borrowed: &str = a.borrow();
        assert_eq!(borrowed, "alpha");
    }

    #[test]
    fn scope_precedence_matches_the_contract() {
        assert_eq!(Scope::Builtin.precedence(), 50);
        assert_eq!(Scope::User.precedence(), 40);
        assert_eq!(Scope::Project.precedence(), 30);
        assert_eq!(Scope::Imported.precedence(), 20);
        assert_eq!(Scope::External.precedence(), 10);
        assert!(Scope::Builtin.precedence() > Scope::Project.precedence());
    }

    #[test]
    fn only_the_trusted_tiers_may_run_scripts() {
        assert!(TrustLevel::Builtin.allows_scripts());
        assert!(TrustLevel::Verified.allows_scripts());
        assert!(TrustLevel::UserTrusted.allows_scripts());
        assert!(!TrustLevel::ProjectTrusted.allows_scripts());
        assert!(!TrustLevel::Community.allows_scripts());
        assert!(!TrustLevel::Untrusted.allows_scripts());
    }

    #[test]
    fn untrusted_is_the_only_level_closed_to_activation() {
        for level in [
            TrustLevel::Builtin,
            TrustLevel::Verified,
            TrustLevel::UserTrusted,
            TrustLevel::ProjectTrusted,
            TrustLevel::Community,
        ] {
            assert!(level.allows_activation(), "{level:?}");
        }
        assert!(!TrustLevel::Untrusted.allows_activation());
    }

    #[test]
    fn resource_kinds_come_from_the_extension() {
        assert_eq!(ResourceKind::of("scripts/run.sh"), ResourceKind::Script);
        assert_eq!(
            ResourceKind::of("references/api.md"),
            ResourceKind::Reference
        );
        assert_eq!(ResourceKind::of("assets/logo.png"), ResourceKind::Asset);
        assert_eq!(ResourceKind::of("tpl/output.tmpl"), ResourceKind::Template);
        assert_eq!(ResourceKind::of("agent.yaml"), ResourceKind::AgentConfig);
        assert_eq!(ResourceKind::of("data.bin"), ResourceKind::Other);
    }

    #[test]
    fn labels_are_stable_strings() {
        assert_eq!(Scope::User.label(), "user");
        assert_eq!(Compat::OpenCode.label(), "opencode");
        assert_eq!(TrustLevel::Untrusted.label(), "untrusted");
    }
}
