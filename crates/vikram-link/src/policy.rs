//! One question, one function: may this token do this thing to this target?
//!
//! Every endpoint asks [`authorize`] before it touches the runtime, and the WebSocket asks it once
//! per event before it writes one. That is the whole point of keeping it in one place: a route
//! that forgets to ask is visible here, not scattered through a router.

use crate::tier::{SessionOwner, Tier};
use std::fmt;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Action {
    /// Liveness. Answerable without a token, and by every tier that has one.
    Health,
    ListSessions,
    ReadEvents,
    Submit,
    ListApprovals,
    /// Answering an approval. `persist` is the difference between one decision and a durable rule.
    AnswerApproval {
        persist: bool,
    },
    ListTokens,
    ManageTokens,
    ReadAudit,
}

impl Action {
    pub fn as_str(&self) -> &'static str {
        match self {
            Action::Health => "health",
            Action::ListSessions => "sessions",
            Action::ReadEvents => "events",
            Action::Submit => "submit",
            Action::ListApprovals => "approvals",
            Action::AnswerApproval { persist: false } => "answer-approval",
            Action::AnswerApproval { persist: true } => "keep-approval",
            Action::ListTokens => "list-tokens",
            Action::ManageTokens => "manage-tokens",
            Action::ReadAudit => "audit",
        }
    }

    /// The phrase a 403 uses: "tier `bot:alpha` may not <this>".
    fn phrase(&self) -> &'static str {
        match self {
            Action::Health => "read health",
            Action::ListSessions => "list sessions",
            Action::ReadEvents => "read the event stream",
            Action::Submit => "submit a turn",
            Action::ListApprovals => "list approvals",
            Action::AnswerApproval { persist: false } => "answer an approval",
            Action::AnswerApproval { persist: true } => "keep an approval as a durable rule",
            Action::ListTokens => "list link tokens",
            Action::ManageTokens => "mint or revoke link tokens",
            Action::ReadAudit => "read the audit log",
        }
    }
}

/// What an action is aimed at. A session target carries who owns it, because that is the only
/// thing that decides whether a bot or team tier reaches it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Target {
    pub id: String,
    pub owner: SessionOwner,
}

/// A refusal. `Unauthorized` is a 401 (the caller has no usable token); `Forbidden` is a 403 that
/// names the tier, so a linked device knows exactly which tier it needs.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Denial {
    Unauthorized(String),
    Forbidden { tier: String, reason: String },
}

impl fmt::Display for Denial {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Denial::Unauthorized(reason) => f.write_str(reason),
            Denial::Forbidden { reason, .. } => f.write_str(reason),
        }
    }
}

fn forbidden(tier: &Tier, action: Action, detail: &str) -> Denial {
    Denial::Forbidden {
        tier: tier.to_string(),
        reason: format!("tier `{tier}` may not {}: {detail}", action.phrase()),
    }
}

/// The boundary. `is_owner` is whether this token is the one `admin` token that owns the runtime;
/// it only matters for keeping an approval, which writes a rule that outlives the decision.
pub fn authorize(
    tier: &Tier,
    is_owner: bool,
    action: Action,
    target: Option<&Target>,
) -> Result<(), Denial> {
    match action {
        Action::Health | Action::ListSessions | Action::ReadEvents => Ok(()),
        Action::Submit => {
            let Some(target) = target else {
                return Err(forbidden(tier, action, "no session was named"));
            };
            if tier.may_submit(&target.owner) {
                Ok(())
            } else if matches!(tier, Tier::Read) {
                Err(forbidden(
                    tier,
                    action,
                    "a read token receives events and submits nothing",
                ))
            } else {
                Err(forbidden(
                    tier,
                    action,
                    &format!(
                        "session `{}` belongs to the {} {}",
                        target.id,
                        target.owner.kind(),
                        target.owner.name().unwrap_or("operator")
                    ),
                ))
            }
        }
        Action::ListApprovals => match tier {
            Tier::Admin | Tier::Main => Ok(()),
            _ => Err(forbidden(
                tier,
                action,
                "approvals belong to the owner and the root assistant",
            )),
        },
        Action::AnswerApproval { persist } => match (tier, persist) {
            (Tier::Admin, true) if !is_owner => Err(forbidden(
                tier,
                action,
                "only the owner admin token may keep a durable rule",
            )),
            (Tier::Admin, _) | (Tier::Main, false) => Ok(()),
            (Tier::Main, true) => Err(forbidden(
                tier,
                action,
                "keeping a rule is an owner decision; answer without `persist` instead",
            )),
            _ => Err(forbidden(
                tier,
                action,
                "approvals belong to the owner and the root assistant",
            )),
        },
        Action::ListTokens | Action::ManageTokens | Action::ReadAudit => match tier {
            Tier::Admin => Ok(()),
            _ => Err(forbidden(
                tier,
                action,
                "token administration and the audit log are the admin tier's",
            )),
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn root() -> Target {
        Target {
            id: "ses_root".into(),
            owner: SessionOwner::Root,
        }
    }

    fn bot() -> Target {
        Target {
            id: "ses_bot".into(),
            owner: SessionOwner::Bot("alpha".into()),
        }
    }

    fn team() -> Target {
        Target {
            id: "ses_team".into(),
            owner: SessionOwner::Team("alpha".into()),
        }
    }

    fn can(tier: &str, action: Action, target: Option<&Target>) -> bool {
        let tier = Tier::parse(tier).unwrap();
        authorize(&tier, true, action, target).is_ok()
    }

    #[test]
    fn the_submit_matrix_is_the_tier_table() {
        let cases: [(&str, &Target, bool); 10] = [
            ("admin", &root(), true),
            ("main", &root(), true),
            ("bots", &bot(), true),
            ("bots", &root(), false),
            ("bot:alpha", &bot(), true),
            ("bot:beta", &bot(), false),
            ("team", &team(), true),
            ("team:alpha", &team(), true),
            ("team:beta", &team(), false),
            ("read", &root(), false),
        ];
        for (tier, target, expected) in cases {
            assert_eq!(
                can(tier, Action::Submit, Some(target)),
                expected,
                "`{tier}` submitting to a {} session",
                target.owner.kind()
            );
        }
        assert!(
            !can("read", Action::Submit, Some(&bot())),
            "read never writes"
        );
    }

    #[test]
    fn administration_is_admin_only() {
        for action in [
            Action::ListTokens,
            Action::ManageTokens,
            Action::ReadAudit,
            Action::AnswerApproval { persist: true },
        ] {
            assert!(can("admin", action, None), "admin may {}", action.as_str());
            for tier in ["main", "bots", "bot:alpha", "team", "team:alpha", "read"] {
                assert!(
                    !can(tier, action, None),
                    "`{tier}` may not {}",
                    action.as_str()
                );
            }
        }
    }

    #[test]
    fn keeping_a_rule_needs_the_owner_not_merely_admin() {
        let admin = Tier::parse("admin").unwrap();
        assert!(
            authorize(
                &admin,
                false,
                Action::AnswerApproval { persist: true },
                None
            )
            .is_err()
        );
        assert!(
            authorize(
                &admin,
                false,
                Action::AnswerApproval { persist: false },
                None
            )
            .is_ok()
        );
        assert!(authorize(&admin, true, Action::AnswerApproval { persist: true }, None).is_ok());
    }

    #[test]
    fn a_refusal_names_the_tier() {
        let tier = Tier::parse("bot:alpha").unwrap();
        let err = authorize(&tier, false, Action::Submit, Some(&root())).unwrap_err();
        match err {
            Denial::Forbidden { tier, reason } => {
                assert_eq!(tier, "bot:alpha");
                assert!(reason.contains("bot:alpha"));
                assert!(reason.contains("root"));
            }
            other => panic!("expected a forbidden refusal, got {other:?}"),
        }
    }

    #[test]
    fn every_tier_reads_events_and_sessions() {
        for tier in [
            "admin",
            "main",
            "bots",
            "bot:alpha",
            "team",
            "team:alpha",
            "read",
        ] {
            assert!(can(tier, Action::ListSessions, None));
            assert!(can(tier, Action::ReadEvents, None));
        }
    }
}
