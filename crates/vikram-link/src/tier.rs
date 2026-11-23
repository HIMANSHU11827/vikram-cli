//! What a link token is allowed to be.
//!
//! A tier is a stored string, so the spelling is part of the contract: the canonical forms below
//! are what a token file carries and what a refusal prints. The four names this server shipped
//! with before the rename (`full`, `vikram`, `hive`, `observe`) are still accepted on the way in —
//! a token file written yesterday must keep working — but they are never written back out.

use serde::Deserialize;
use serde::de::{self, Deserializer};
use serde::ser::{Serialize, Serializer};
use std::fmt;

/// The canonical spellings, in the order the documentation lists them. `bot:<name>` and
/// `team:<name>` are the scoped forms: the `:` introduces the one entity the token reaches.
pub const TIERS: [&str; 7] = [
    "admin",
    "main",
    "bots",
    "bot:<name>",
    "team",
    "team:<name>",
    "read",
];

/// The list every tier error quotes, so a typo is answered with the whole menu.
pub const TIER_LIST: &str = "admin, main, bots, bot:<name>, team, team:<name>, read";

/// The tier names this server used before the rename. Accepted when parsing, never stored.
pub const ALIASES: [(&str, &str); 4] = [
    ("full", "admin"),
    ("vikram", "main"),
    ("hive", "team"),
    ("observe", "read"),
];

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Tier {
    /// Everything, including config, daemon and token administration. The owner tier.
    Admin,
    /// The root assistant: every session, turns, approvals — but no administration.
    Main,
    /// Every bot entity. Not the root session, and no tool of its own.
    Bots,
    /// Exactly one bot, by name.
    Bot(String),
    /// Every team (hive) agent.
    Team,
    /// Exactly one team, by name.
    TeamOf(String),
    /// Read-only mirror: receives events, submits nothing, answers nothing.
    Read,
}

/// A tier that could not be read, with the reason an operator can act on.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TierError(pub String);

impl fmt::Display for TierError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for TierError {}

fn unknown(raw: &str) -> TierError {
    TierError(format!(
        "unknown tier `{raw}`; valid tiers are {TIER_LIST} (the old names full, vikram, hive and observe are still accepted)"
    ))
}

fn scoped(kind: &str, raw: &str) -> Result<String, TierError> {
    let name = raw.trim();
    let valid = !name.is_empty()
        && name
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || matches!(c, '-' | '_' | '.'));
    if !valid {
        return Err(TierError(format!(
            "tier `{kind}:` needs a lowercase name of letters, digits, `-`, `_` or `.`; got `{name}` — valid tiers are {TIER_LIST}"
        )));
    }
    Ok(name.to_owned())
}

impl Tier {
    /// Read a tier from a flag, a token file or a request. Case-insensitive; the aliases map onto
    /// their canonical tier so nothing written before the rename is stranded.
    pub fn parse(raw: &str) -> Result<Tier, TierError> {
        let text = raw.trim();
        if text.is_empty() {
            return Err(unknown(raw));
        }
        let lower = text.to_ascii_lowercase();
        let tier = match lower.as_str() {
            "admin" | "full" => Tier::Admin,
            "main" | "vikram" => Tier::Main,
            "bots" => Tier::Bots,
            "team" | "hive" => Tier::Team,
            "read" | "observe" => Tier::Read,
            other => {
                if let Some(rest) = other.strip_prefix("bot:") {
                    Tier::Bot(scoped("bot", rest)?)
                } else if let Some(rest) = other
                    .strip_prefix("team:")
                    .or_else(|| other.strip_prefix("hive:"))
                {
                    Tier::TeamOf(scoped("team", rest)?)
                } else {
                    return Err(unknown(text));
                }
            }
        };
        Ok(tier)
    }

    /// The canonical string: what the token file stores and what a 403 names.
    pub fn as_str(&self) -> &str {
        match self {
            Tier::Admin => "admin",
            Tier::Main => "main",
            Tier::Bots => "bots",
            Tier::Bot(_) => "bot",
            Tier::Team => "team",
            Tier::TeamOf(_) => "team",
            Tier::Read => "read",
        }
    }

    /// The name of the one entity a scoped tier reaches, if it is scoped.
    pub fn scope(&self) -> Option<&str> {
        match self {
            Tier::Bot(name) | Tier::TeamOf(name) => Some(name),
            _ => None,
        }
    }

    /// Whether this tier may see a session at all. Reading is the wider question: a `read` token
    /// sees every session, while a bot token sees only bot sessions.
    pub fn reaches(&self, owner: &SessionOwner) -> bool {
        match (self, owner) {
            (Tier::Admin | Tier::Main | Tier::Read, _) => true,
            (Tier::Bots, SessionOwner::Bot(_)) => true,
            (Tier::Bot(name), SessionOwner::Bot(owner)) => name == owner,
            (Tier::Team, SessionOwner::Team(_)) => true,
            (Tier::TeamOf(name), SessionOwner::Team(owner)) => name == owner,
            _ => false,
        }
    }

    /// Whether this tier may put a turn into that session. Reading is not writing: `read` sees
    /// everything and may submit nowhere, and a bot tier never reaches the root session.
    pub fn may_submit(&self, owner: &SessionOwner) -> bool {
        !matches!(self, Tier::Read) && self.reaches(owner)
    }
}

impl fmt::Display for Tier {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Tier::Bot(name) => write!(f, "bot:{name}"),
            Tier::TeamOf(name) => write!(f, "team:{name}"),
            other => f.write_str(other.as_str()),
        }
    }
}

impl Serialize for Tier {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(&self.to_string())
    }
}

impl<'de> Deserialize<'de> for Tier {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let raw = String::deserialize(deserializer)?;
        Tier::parse(&raw).map_err(de::Error::custom)
    }
}

/// Who a session belongs to. A session with no bot and no team is the operator's own: the root
/// session, which a bot or team token must never reach.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SessionOwner {
    Root,
    Bot(String),
    Team(String),
}

impl SessionOwner {
    pub fn kind(&self) -> &'static str {
        match self {
            SessionOwner::Root => "root",
            SessionOwner::Bot(_) => "bot",
            SessionOwner::Team(_) => "team",
        }
    }

    pub fn name(&self) -> Option<&str> {
        match self {
            SessionOwner::Root => None,
            SessionOwner::Bot(name) | SessionOwner::Team(name) => Some(name),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_canonical_tier_round_trips() {
        for text in [
            "admin",
            "main",
            "bots",
            "bot:alpha",
            "team",
            "team:alpha",
            "read",
        ] {
            let tier = Tier::parse(text).unwrap_or_else(|e| panic!("{text}: {e}"));
            assert_eq!(
                tier.to_string(),
                text,
                "the stored spelling is the canonical one"
            );
        }
    }

    #[test]
    fn the_old_names_are_aliases_and_are_never_stored() {
        for (old, new) in ALIASES {
            let tier = Tier::parse(old).unwrap();
            assert_eq!(tier.to_string(), new, "`{old}` must read as `{new}`");
        }
        assert_eq!(Tier::parse("bot:alpha").unwrap(), Tier::Bot("alpha".into()));
        assert_eq!(
            Tier::parse("hive:alpha").unwrap(),
            Tier::TeamOf("alpha".into())
        );
        assert_eq!(Tier::parse("full").unwrap(), Tier::Admin);
        assert_eq!(Tier::parse("observe").unwrap(), Tier::Read);
        assert_eq!(
            Tier::parse("HIVE:Alpha").unwrap(),
            Tier::TeamOf("alpha".into())
        );
    }

    #[test]
    fn an_unknown_tier_names_all_seven() {
        let err = Tier::parse("superuser").unwrap_err().to_string();
        for tier in TIERS {
            assert!(err.contains(tier), "`{err}` is missing `{tier}`");
        }
        assert!(err.contains("superuser"));
    }

    #[test]
    fn a_scoped_tier_needs_a_name() {
        assert!(Tier::parse("bot:").is_err());
        assert!(Tier::parse("team:").is_err());
        assert!(Tier::parse("bot:Al pha").is_err());
        assert!(Tier::parse("bot:a/b").is_err());
    }

    #[test]
    fn bot_and_team_tiers_do_not_reach_the_root_session() {
        let root = SessionOwner::Root;
        for tier in [
            Tier::Bots,
            Tier::Bot("alpha".into()),
            Tier::Team,
            Tier::TeamOf("alpha".into()),
        ] {
            assert!(
                !tier.reaches(&root),
                "`{tier}` must not reach the root session"
            );
            assert!(!tier.may_submit(&root));
        }
        assert!(Tier::Read.reaches(&root));
        assert!(
            !Tier::Read.may_submit(&root),
            "read is a mirror, not a writer"
        );
    }

    #[test]
    fn scoped_tiers_reach_exactly_one_entity() {
        let alpha = SessionOwner::Bot("alpha".into());
        let beta = SessionOwner::Bot("beta".into());
        assert!(Tier::Bot("alpha".into()).reaches(&alpha));
        assert!(!Tier::Bot("alpha".into()).reaches(&beta));
        assert!(Tier::Bots.reaches(&beta));
        let team = SessionOwner::Team("alpha".into());
        assert!(Tier::TeamOf("alpha".into()).reaches(&team));
        assert!(!Tier::TeamOf("beta".into()).reaches(&team));
        assert!(!Tier::Bots.reaches(&team), "a bot tier is not a team tier");
    }
}
