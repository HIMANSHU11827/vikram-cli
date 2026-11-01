//! Trust: who vouched for a skill directory, and how a declared permission turns into a policy
//! request. This module never decides anything — it classifies, and the kernel's policy
//! evaluates.

use std::path::Path;

use serde::{Deserialize, Serialize};
use vikram_contracts::policy::{ActionKind, PermissionRequest, SideEffect};
use vikram_contracts::trust::Trust;

use crate::error::{Result, SkillError};
use crate::paths;
use crate::skill::{Compat, Scope, SkillId, TrustLevel};

const TRUST_FILE: &str = "trust.json";

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct TrustPolicy {
    /// A workspace the user has said they trust. Untrusted projects still load, at Community.
    pub project_trusted: bool,
    /// Sources (paths or URLs) that have been verified once and stay verified.
    pub trusted_sources: Vec<String>,
    /// Names or paths that must never be trusted, whatever their scope.
    pub deny: Vec<String>,
}

impl TrustPolicy {
    /// `<state_dir>/trust.json`. A missing or unreadable file is the default policy, never an
    /// error: a broken trust file must not stop the tool from starting.
    pub fn load() -> TrustPolicy {
        TrustPolicy::load_in(&paths::state_dir())
    }

    pub fn load_in(state_dir: &Path) -> TrustPolicy {
        paths::read_json(&state_dir.join(TRUST_FILE)).unwrap_or_default()
    }

    pub fn save(&self) -> Result<()> {
        self.save_in(&paths::state_dir())
    }

    pub fn save_in(&self, state_dir: &Path) -> Result<()> {
        let path = state_dir.join(TRUST_FILE);
        let bytes = serde_json::to_vec_pretty(self).map_err(|err| SkillError::Io {
            path: path.clone(),
            reason: err.to_string(),
        })?;
        paths::atomic_write(&path, &bytes)
    }

    pub fn level_for(&self, scope: Scope, compat: Compat, dir: &Path) -> TrustLevel {
        if self.is_denied(dir) {
            return TrustLevel::Untrusted;
        }
        match scope {
            Scope::Builtin => TrustLevel::Builtin,
            // The user put it there; the layout convention does not lower that. A third-party
            // layout does, because nobody verified who wrote it.
            Scope::User => {
                if compat == Compat::Native {
                    TrustLevel::UserTrusted
                } else {
                    TrustLevel::Community
                }
            }
            Scope::Project => {
                if compat == Compat::Native && self.project_trusted {
                    TrustLevel::ProjectTrusted
                } else {
                    TrustLevel::Community
                }
            }
            Scope::Imported => {
                if self.source_trusted(dir) {
                    TrustLevel::Verified
                } else {
                    TrustLevel::Community
                }
            }
            Scope::External => TrustLevel::Community,
        }
    }

    fn is_denied(&self, dir: &Path) -> bool {
        let name = dir
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_default();
        let full = dir.to_string_lossy().into_owned();
        self.deny.iter().any(|entry| {
            let entry = entry.trim();
            !entry.is_empty()
                && (entry == name
                    || full == entry
                    || full.ends_with(&format!("/{entry}"))
                    || full.starts_with(entry))
        })
    }

    fn source_trusted(&self, dir: &Path) -> bool {
        let full = dir.to_string_lossy().into_owned();
        self.trusted_sources
            .iter()
            .map(|s| s.trim())
            .filter(|s| !s.is_empty())
            .any(|s| full == s || full.starts_with(&format!("{s}/")))
    }

    /// Parses `action:resource` (`fs.write:workspace`, `net.http:host`, `shell.run`, ...) into a
    /// policy request that originates at [`Trust::External`]: content a skill declares can
    /// never be the thing that authorises it.
    pub fn permission_request(name: &SkillId, perm: &str) -> Result<PermissionRequest> {
        let trimmed = perm.trim();
        if trimmed.is_empty() {
            return Err(SkillError::Unsupported {
                what: format!("empty permission declared by skill `{name}`"),
            });
        }
        let (verb, resource) = match trimmed.split_once(':') {
            Some((verb, rest)) => {
                let rest = rest.trim();
                (verb.trim(), if rest.is_empty() { "*" } else { rest })
            }
            None => (trimmed, "*"),
        };
        let (action, side_effect) = classify(verb).ok_or_else(|| SkillError::Unsupported {
            what: format!("permission `{perm}` declared by skill `{name}`"),
        })?;
        Ok(PermissionRequest::new(
            action,
            verb,
            resource,
            side_effect,
            Trust::External,
        ))
    }

    /// Every declared permission mapped to a request, in declaration order.
    pub fn permission_requests(name: &SkillId, perms: &[String]) -> Result<Vec<PermissionRequest>> {
        perms
            .iter()
            .map(|p| Self::permission_request(name, p))
            .collect()
    }
}

/// Verb → (action, side effect). Unknown `fs.*` verbs are treated as writes, the conservative
/// direction: a permission we do not understand must not look weaker than one we do.
fn classify(verb: &str) -> Option<(ActionKind, SideEffect)> {
    let lower = verb.to_ascii_lowercase();
    let pair = match lower.as_str() {
        "fs.read" | "fs.list" | "fs.stat" => (ActionKind::Read, SideEffect::None),
        "fs.write" | "fs.append" | "fs.delete" | "fs.move" => {
            (ActionKind::Write, SideEffect::LocalMutation)
        }
        "shell.run" | "shell.exec" | "process.spawn" => {
            (ActionKind::Execute, SideEffect::ExternalChange)
        }
        "net.http" | "net.fetch" => (ActionKind::Network, SideEffect::ExternalChange),
        "delegate" | "agent.delegate" => (ActionKind::Delegate, SideEffect::None),
        "admin" | "admin.configure" => (ActionKind::Admin, SideEffect::Destructive),
        "spend" | "spend.purchase" => (ActionKind::Spend, SideEffect::ExternalChange),
        other if other.starts_with("fs.") => (ActionKind::Write, SideEffect::LocalMutation),
        other if other.starts_with("shell.") => (ActionKind::Execute, SideEffect::ExternalChange),
        other if other.starts_with("net.") => (ActionKind::Network, SideEffect::ExternalChange),
        other if other.starts_with("delegate.") => (ActionKind::Delegate, SideEffect::None),
        other if other.starts_with("admin.") => (ActionKind::Admin, SideEffect::Destructive),
        other if other.starts_with("spend.") => (ActionKind::Spend, SideEffect::ExternalChange),
        _ => return None,
    };
    Some(pair)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::skill::TrustLevel;

    fn id(name: &str) -> SkillId {
        SkillId::new(name).expect("valid name")
    }

    #[test]
    fn missing_trust_file_is_the_default_policy() {
        let dir = tempfile::tempdir().expect("temp");
        let policy = TrustPolicy::load_in(dir.path());
        assert_eq!(policy, TrustPolicy::default());
        assert!(!policy.project_trusted);
        assert!(policy.trusted_sources.is_empty());
    }

    #[test]
    fn policy_round_trips_through_disk() {
        let dir = tempfile::tempdir().expect("temp");
        let policy = TrustPolicy {
            project_trusted: true,
            trusted_sources: vec!["https://example.com/x".into()],
            deny: vec!["evil".into()],
        };
        policy.save_in(dir.path()).expect("save");
        assert_eq!(TrustPolicy::load_in(dir.path()), policy);
    }

    #[test]
    fn a_corrupt_trust_file_falls_back_to_defaults() {
        let dir = tempfile::tempdir().expect("temp");
        std::fs::write(dir.path().join(TRUST_FILE), "{not json").expect("write");
        assert_eq!(TrustPolicy::load_in(dir.path()), TrustPolicy::default());
    }

    #[test]
    fn builtin_and_native_user_skills_are_trusted() {
        let policy = TrustPolicy::default();
        assert_eq!(
            policy.level_for(Scope::Builtin, Compat::Native, Path::new("/b/x")),
            TrustLevel::Builtin
        );
        assert_eq!(
            policy.level_for(Scope::User, Compat::Native, Path::new("/u/x")),
            TrustLevel::UserTrusted
        );
    }

    #[test]
    fn compat_layouts_and_untrusted_projects_drop_to_community() {
        let policy = TrustPolicy::default();
        assert_eq!(
            policy.level_for(Scope::User, Compat::Claude, Path::new("/u/x")),
            TrustLevel::Community
        );
        assert_eq!(
            policy.level_for(Scope::Project, Compat::Native, Path::new("/ws/x")),
            TrustLevel::Community
        );
        let trusted = TrustPolicy {
            project_trusted: true,
            ..TrustPolicy::default()
        };
        assert_eq!(
            trusted.level_for(Scope::Project, Compat::Native, Path::new("/ws/x")),
            TrustLevel::ProjectTrusted
        );
        assert_eq!(
            trusted.level_for(Scope::Project, Compat::Claude, Path::new("/ws/x")),
            TrustLevel::Community
        );
    }

    #[test]
    fn imported_skills_are_verified_only_from_a_trusted_source() {
        let policy = TrustPolicy {
            trusted_sources: vec!["/opt/vendor/skills".into()],
            ..TrustPolicy::default()
        };
        assert_eq!(
            policy.level_for(
                Scope::Imported,
                Compat::Native,
                Path::new("/opt/vendor/skills/a")
            ),
            TrustLevel::Verified
        );
        assert_eq!(
            policy.level_for(Scope::Imported, Compat::Native, Path::new("/tmp/skills/a")),
            TrustLevel::Community
        );
    }

    #[test]
    fn deny_wins_over_every_scope() {
        let policy = TrustPolicy {
            project_trusted: true,
            deny: vec!["evil".into(), "/opt/bad".into()],
            ..TrustPolicy::default()
        };
        assert_eq!(
            policy.level_for(Scope::Builtin, Compat::Native, Path::new("/b/evil")),
            TrustLevel::Untrusted
        );
        assert_eq!(
            policy.level_for(Scope::User, Compat::Native, Path::new("/opt/bad/x")),
            TrustLevel::Untrusted
        );
        assert_eq!(
            policy.level_for(Scope::User, Compat::Native, Path::new("/u/fine")),
            TrustLevel::UserTrusted
        );
    }

    #[test]
    fn permissions_map_to_kernel_requests() {
        let name = id("alpha");
        let cases = [
            ("fs.read:workspace", ActionKind::Read, SideEffect::None),
            (
                "fs.write:workspace",
                ActionKind::Write,
                SideEffect::LocalMutation,
            ),
            (
                "shell.run:make test",
                ActionKind::Execute,
                SideEffect::ExternalChange,
            ),
            (
                "net.http:example.com",
                ActionKind::Network,
                SideEffect::ExternalChange,
            ),
            ("delegate:helper", ActionKind::Delegate, SideEffect::None),
            ("admin:settings", ActionKind::Admin, SideEffect::Destructive),
        ];
        for (perm, action, effect) in cases {
            let request = TrustPolicy::permission_request(&name, perm).expect(perm);
            assert_eq!(request.action, action, "{perm}");
            assert_eq!(request.side_effect, effect, "{perm}");
            assert_eq!(request.originating_trust, Trust::External, "{perm}");
        }
    }

    #[test]
    fn a_permission_without_a_resource_gets_a_wildcard() {
        let request =
            TrustPolicy::permission_request(&id("a"), "net.http").expect("wildcard resource");
        assert_eq!(request.verb, "net.http");
        assert_eq!(request.resource, "*");
        assert_eq!(request.side_effect, SideEffect::ExternalChange);
    }

    #[test]
    fn unknown_permissions_are_unsupported_not_silently_allowed() {
        let err =
            TrustPolicy::permission_request(&id("a"), "telepathy:mind").expect_err("unsupported");
        assert!(matches!(err, SkillError::Unsupported { .. }));
        assert!(err.to_string().contains("telepathy:mind"));
        assert!(err.to_string().contains("skill `a`"));
        assert!(TrustPolicy::permission_request(&id("a"), "  ").is_err());
    }

    #[test]
    fn permission_requests_map_a_whole_declaration() {
        let requests = TrustPolicy::permission_requests(
            &id("a"),
            &["fs.read:workspace".to_string(), "shell.run".to_string()],
        )
        .expect("requests");
        assert_eq!(requests.len(), 2);
        assert_eq!(requests[1].resource, "*");
    }
}
