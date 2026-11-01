//! The permission vocabulary shared by the policy evaluator, the kernel, and any replacement for
//! either. Kept in contracts so a substituted policy implementation cannot invent new fields that
//! the kernel would silently ignore.

use crate::trust::Trust;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ActionKind {
    Read,
    Write,
    Execute,
    Network,
    Delegate,
    Spend,
    Admin,
}

/// What running the action would do to the world outside the runtime.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SideEffect {
    None,
    LocalMutation,
    ExternalChange,
    Destructive,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PermissionRequest {
    pub action: ActionKind,
    /// Tool verb, e.g. `shell.run` or `fs.write`. Matched by rules, never by the model.
    pub verb: String,
    /// Canonicalised resource: absolute path, URL host, or agent id.
    pub resource: String,
    pub side_effect: SideEffect,
    /// Provenance of the content that produced this request.
    pub originating_trust: Trust,
}

impl PermissionRequest {
    pub fn new(
        action: ActionKind,
        verb: impl Into<String>,
        resource: impl Into<String>,
        side_effect: SideEffect,
        originating_trust: Trust,
    ) -> Self {
        Self {
            action,
            verb: verb.into(),
            resource: resource.into(),
            side_effect,
            originating_trust,
        }
    }
}

/// What a rule says. Separate from [`Decision`] because a rule is configuration and a decision is
/// the answer for one specific request.
///
/// The payloads are named fields, not newtypes: an internally tagged enum cannot carry a bare
/// string, so a `deny` rule written as one would not survive the file it is saved in.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "outcome", rename_all = "snake_case")]
pub enum RuleOutcome {
    Allow,
    Deny { reason: String },
    Ask { reason: String },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "decision", rename_all = "snake_case")]
pub enum Decision {
    Allow,
    Deny {
        reason: String,
    },
    RequireApproval {
        reason: String,
        /// A machine-proposed rule amendment the user can accept instead of re-explaining.
        amendment: Option<String>,
    },
}

impl Decision {
    pub fn is_allowed(&self) -> bool {
        matches!(self, Decision::Allow)
    }
}

/// The posture a runtime takes toward prompts. Six postures, chosen to match Claude Code's set;
/// VIKRAM uses its own short names because every other runtime surveyed spells the same shape
/// differently (Gemini `auto_edit`, goose `approve`, OpenHands `ConfirmRisky`).
///
/// A mode is not a permission list. Rules are evaluated inside every mode, deny-first, and the
/// mandatory floor applies underneath all of them — see `docs/research/security/`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PermissionMode {
    /// Reads run; anything gated prompts.
    #[default]
    Ask,
    /// [`Self::Ask`] plus workspace writes auto-approved.
    AcceptEdits,
    /// Read-only. Mutations are refused, not deferred.
    Plan,
    /// Every action is risk-tiered deterministically before it runs.
    Smart,
    /// Anything that would prompt is denied, and the model is told not to retry.
    Unattended,
    /// Every prompt is auto-approved. The floor still applies.
    Yolo,
    /// Only what the operator saved in the allow-list runs quietly. Built-in conveniences — reads,
    /// workspace writes — stop being automatic, so this is stricter than [`Self::Ask`].
    AllowList,
}

/// What a posture needs to know about the world: the root writes are confined to, and whether the
/// operator saved a rule that covers this exact request.
#[derive(Debug, Clone, Copy, Default)]
pub struct Posture<'a> {
    pub workspace_root: Option<&'a str>,
    /// True when an operator-authored allow rule — config or the saved allow-list — matches the
    /// request. A replacement evaluator that cannot answer gets `false`, which only ever costs a
    /// prompt.
    pub listed: bool,
}

impl PermissionMode {
    /// The canonical spelling, which is also what the config file and the journal store.
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Ask => "ask",
            Self::AcceptEdits => "edit-ok",
            Self::Plan => "plan",
            Self::Smart => "smart",
            Self::Unattended => "unattended",
            Self::Yolo => "yolo",
            Self::AllowList => "allow-list",
        }
    }

    /// Every posture, in the order an interface should list them.
    pub fn all() -> [PermissionMode; 7] {
        [
            Self::Ask,
            Self::AcceptEdits,
            Self::Plan,
            Self::Smart,
            Self::Unattended,
            Self::Yolo,
            Self::AllowList,
        ]
    }

    /// Accepts our names and the upstream ones people already know, ignoring case and separators.
    pub fn parse(name: &str) -> Option<Self> {
        let key: String = name
            .chars()
            .filter(|c| !c.is_whitespace() && *c != '-' && *c != '_')
            .flat_map(|c| c.to_lowercase())
            .collect();
        Some(match key.as_str() {
            "ask" | "manual" | "default" => Self::Ask,
            "editok" | "acceptedits" | "autoedit" | "edit" => Self::AcceptEdits,
            "plan" => Self::Plan,
            "smart" | "auto" => Self::Smart,
            "unattended" | "dontask" | "headless" | "noinput" => Self::Unattended,
            "yolo" | "bypass" | "bypasspermissions" => Self::Yolo,
            "allowlist" | "list" | "saved" => Self::AllowList,
            _ => return None,
        })
    }

    /// Postures that can let something run without a human reading the prompt.
    pub fn auto_approves(&self) -> bool {
        matches!(self, Self::AcceptEdits | Self::Smart | Self::Yolo)
    }

    /// The most permissive posture, and the one an operator most wants to forbid centrally.
    pub fn is_bypass(&self) -> bool {
        *self == Self::Yolo
    }

    /// Apply the posture to what the evaluator returned.
    ///
    /// Two ordering rules make this safe to call anywhere:
    /// * `Deny` is final. No mode converts a refusal into permission, so `yolo` means "stop
    ///   prompting", never "grant everything".
    /// * Only `RequireApproval` can change, except in [`Self::Plan`] and [`Self::AllowList`], which
    ///   additionally tighten what the rules allowed — a posture is enforced, not suggested.
    ///
    /// Auto-approval is per request. It never mints a grant or a durable rule, which is what keeps
    /// "an agent cannot grant itself permissions by generating output" true in every mode: the only
    /// path to durable authority is `ApprovalTicket`, and a ticket from policy auto-approval is
    /// refused for untrusted content in the kernel's broker.
    pub fn resolve(
        self,
        decision: Decision,
        request: &PermissionRequest,
        posture: Posture<'_>,
    ) -> Decision {
        let root = posture.workspace_root;

        if self == Self::Plan && !Self::reads_only(request) {
            return Decision::Deny {
                reason: format!(
                    "plan mode is read-only; `{}` would {}.",
                    request.verb,
                    effect_name(request.side_effect)
                ),
            };
        }

        if self == Self::AllowList {
            // Nothing runs quietly unless a rule the operator wrote says it may.
            return match decision {
                Decision::Allow if !posture.listed => Decision::RequireApproval {
                    reason: format!(
                        "allow-list mode: `{}` on `{}` is not on the list",
                        request.verb, request.resource
                    ),
                    amendment: Some(crate::amendment_for(request)),
                },
                other => other,
            };
        }

        let Decision::RequireApproval { reason, .. } = &decision else {
            return decision;
        };

        match self {
            Self::Ask | Self::Plan | Self::AllowList => decision,
            Self::Unattended => Decision::Deny {
                reason: format!(
                    "unattended: `{}` was not asked ({reason}); do not retry it",
                    request.verb
                ),
            },
            Self::Yolo => Decision::Allow,
            Self::AcceptEdits => {
                if Self::auto_allows_edit(request, root) {
                    Decision::Allow
                } else {
                    decision
                }
            }
            Self::Smart => {
                if Self::auto_allows_tiered(request, root) {
                    Decision::Allow
                } else {
                    decision
                }
            }
        }
    }

    /// True when the mode turned a prompt into permission, which the caller journals so a replay
    /// can tell a human approval apart from a posture that never asked.
    pub fn auto_resolved(before: &Decision, after: &Decision) -> bool {
        matches!(before, Decision::RequireApproval { .. }) && matches!(after, Decision::Allow)
    }

    fn reads_only(request: &PermissionRequest) -> bool {
        request.action == ActionKind::Read && request.side_effect == SideEffect::None
    }

    fn auto_allows_edit(request: &PermissionRequest, root: Option<&str>) -> bool {
        request.action == ActionKind::Write
            && request.side_effect == SideEffect::LocalMutation
            && within_root(&request.resource, root)
    }

    /// The deterministic risk tiers. Deliberately not a model call: a classifier invoked on
    /// model-authored requests would put the model on its own authority path.
    fn auto_allows_tiered(request: &PermissionRequest, root: Option<&str>) -> bool {
        if request.side_effect == SideEffect::Destructive {
            return false;
        }
        match request.action {
            ActionKind::Read => request.side_effect == SideEffect::None,
            ActionKind::Write => Self::auto_allows_edit(request, root),
            ActionKind::Execute => request.side_effect == SideEffect::LocalMutation,
            ActionKind::Network | ActionKind::Delegate | ActionKind::Spend | ActionKind::Admin => {
                false
            }
        }
    }
}

impl std::fmt::Display for PermissionMode {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// A resource is inside the root only as a path segment, never as a string prefix that happens to
/// match — `/work/proj-evil` does not start with `/work/proj`.
fn within_root(resource: &str, root: Option<&str>) -> bool {
    match root {
        None => false,
        Some(root) => resource == root || resource.starts_with(&format!("{root}/")),
    }
}

/// The rule text that would stop this exact shape from ever asking again, in the form the policy
/// crate parses back into a rule. Deliberately narrower than the request: a command saves its first
/// two words, a file saves its directory, and anything else saves the exact resource — so accepting
/// an amendment can never hand over more authority than the prompt described.
pub fn amendment_for(request: &PermissionRequest) -> String {
    let scope = match request.action {
        ActionKind::Execute => {
            let head = request
                .resource
                .split_whitespace()
                .take(2)
                .collect::<Vec<_>>()
                .join(" ");
            format!("{head} *")
        }
        ActionKind::Read | ActionKind::Write => match request.resource.rfind('/') {
            Some(idx) if idx > 0 => format!("{}/**", &request.resource[..idx]),
            _ => request.resource.clone(),
        },
        _ => request.resource.clone(),
    };
    format!(
        "allow {} {} resource={}",
        request.action, request.verb, scope
    )
}

impl std::fmt::Display for ActionKind {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let word = match self {
            ActionKind::Read => "read",
            ActionKind::Write => "write",
            ActionKind::Execute => "execute",
            ActionKind::Network => "network",
            ActionKind::Delegate => "delegate",
            ActionKind::Spend => "spend",
            ActionKind::Admin => "admin",
        };
        f.write_str(word)
    }
}

fn effect_name(effect: SideEffect) -> &'static str {
    match effect {
        SideEffect::None => "stay inside the runtime",
        SideEffect::LocalMutation => "change this machine",
        SideEffect::ExternalChange => "reach outside this machine",
        SideEffect::Destructive => "destroy data",
    }
}

/// The seam the kernel calls. The kernel depends on this trait and not on any evaluator, which is
/// what lets the whole policy implementation be replaced without touching the execution loop.
pub trait PermissionPolicy: Send + Sync {
    fn contract(&self) -> crate::version::ContractDecl;

    /// `depth` is how far this request has been delegated from the root agent.
    fn evaluate_at_depth(&self, request: &PermissionRequest, depth: u32) -> Decision;

    fn evaluate(&self, request: &PermissionRequest) -> Decision {
        self.evaluate_at_depth(request, 0)
    }

    /// Whether a rule the operator wrote — config or the saved allow-list — permits this request.
    /// [`PermissionMode::AllowList`] reads it, and the default is the conservative answer: an
    /// evaluator that cannot tell gets a prompt instead of silent permission.
    fn is_specially_allowed(&self, _request: &PermissionRequest) -> bool {
        false
    }

    /// The directory an auto-approved edit must stay inside. `None` is the conservative answer: a
    /// posture that depends on scope has nothing to be in, so it asks.
    fn workspace_root(&self) -> Option<String> {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn decisions_round_trip_through_the_wire_format() {
        let d = Decision::RequireApproval {
            reason: "writes outside workspace".into(),
            amendment: Some("allow fs write resource=/tmp/**".into()),
        };
        let json = serde_json::to_string(&d).unwrap();
        assert_eq!(serde_json::from_str::<Decision>(&json).unwrap(), d);
    }

    #[test]
    fn only_allow_counts_as_allowed() {
        assert!(Decision::Allow.is_allowed());
        assert!(
            !Decision::Deny {
                reason: "no".into()
            }
            .is_allowed()
        );
    }

    fn gated(action: ActionKind, resource: &str, side_effect: SideEffect) -> PermissionRequest {
        PermissionRequest::new(action, verb(action), resource, side_effect, Trust::Model)
    }

    fn verb(action: ActionKind) -> &'static str {
        match action {
            ActionKind::Read => "fs.read",
            ActionKind::Write => "fs.write",
            ActionKind::Execute => "shell.run",
            ActionKind::Network => "web.fetch",
            ActionKind::Delegate => "agent.spawn",
            ActionKind::Spend => "api.purchase",
            ActionKind::Admin => "grant.scope",
        }
    }

    fn prompt() -> Decision {
        Decision::RequireApproval {
            reason: "commands run with your authority".into(),
            amendment: None,
        }
    }

    fn posture(root: Option<&str>) -> Posture<'_> {
        Posture {
            workspace_root: root,
            listed: false,
        }
    }

    fn listed(root: Option<&str>) -> Posture<'_> {
        Posture {
            workspace_root: root,
            listed: true,
        }
    }

    #[test]
    fn no_mode_converts_a_deny_into_permission() {
        let denied = Decision::Deny {
            reason: "no rule permits this".into(),
        };
        for mode in PermissionMode::all() {
            let after = mode.resolve(
                denied.clone(),
                &gated(ActionKind::Write, "/work/a.txt", SideEffect::LocalMutation),
                posture(Some("/work")),
            );
            assert!(
                matches!(after, Decision::Deny { .. }),
                "{mode} must leave a refusal refused"
            );
        }
    }

    #[test]
    fn yolo_only_stops_prompting_and_leaves_an_allowed_action_alone() {
        let req = gated(ActionKind::Execute, "/work", SideEffect::LocalMutation);
        let after = PermissionMode::Yolo.resolve(prompt(), &req, posture(Some("/work")));
        assert!(after.is_allowed());
        assert!(PermissionMode::auto_resolved(&prompt(), &after));
        let already = PermissionMode::Yolo.resolve(Decision::Allow, &req, posture(Some("/work")));
        assert!(!PermissionMode::auto_resolved(&Decision::Allow, &already));
    }

    #[test]
    fn plan_refuses_a_write_that_the_rules_would_have_allowed() {
        // Enforcement, not prompting: a posture cannot be argued out of by a permissive evaluator.
        let req = gated(ActionKind::Write, "/work/a.txt", SideEffect::LocalMutation);
        match PermissionMode::Plan.resolve(Decision::Allow, &req, posture(Some("/work"))) {
            Decision::Deny { reason } => assert!(reason.contains("plan mode is read-only"), "{reason}"),
            other => panic!("plan must refuse mutations, got {other:?}"),
        }
        let read = gated(ActionKind::Read, "/work/a.txt", SideEffect::None);
        assert!(
            matches!(
                PermissionMode::Plan.resolve(prompt(), &read, posture(Some("/work"))),
                Decision::RequireApproval { .. }
            ),
            "plan refuses mutations; it does not invent a refusal for a read"
        );
    }

    #[test]
    fn allow_list_prompts_for_an_ordinary_action_even_when_the_rules_allow_it() {
        // The built-in conveniences (reads, workspace writes) are not permission in this posture;
        // only a rule the operator wrote is. The prompt carries the amendment that would list it.
        let req = gated(ActionKind::Read, "/work/a.txt", SideEffect::None);
        match PermissionMode::AllowList.resolve(Decision::Allow, &req, posture(Some("/work"))) {
            Decision::RequireApproval { reason, amendment } => {
                assert!(reason.contains("not on the list"), "{reason}");
                assert_eq!(amendment.as_deref(), Some(crate::amendment_for(&req).as_str()));
            }
            other => panic!("an unlisted action must ask, got {other:?}"),
        }
    }

    #[test]
    fn allow_list_runs_a_listed_action_quietly_and_still_obeyes_a_deny() {
        let req = gated(ActionKind::Execute, "cargo test", SideEffect::LocalMutation);
        let after =
            PermissionMode::AllowList.resolve(Decision::Allow, &req, listed(Some("/work")));
        assert!(after.is_allowed());
        assert!(
            !PermissionMode::auto_resolved(&Decision::Allow, &after),
            "a listed action is permission, not an auto-approval"
        );
        let refused = PermissionMode::AllowList.resolve(
            Decision::Deny {
                reason: "the floor refuses this".into(),
            },
            &req,
            listed(Some("/work")),
        );
        assert!(
            matches!(refused, Decision::Deny { .. }),
            "the list cannot outvote a refusal"
        );
    }

    #[test]
    fn unattended_denies_without_asking_and_tells_the_model_not_to_retry() {
        let req = gated(ActionKind::Network, "https://example.com", SideEffect::ExternalChange);
        match PermissionMode::Unattended.resolve(prompt(), &req, posture(Some("/work"))) {
            Decision::Deny { reason } => {
                assert!(reason.contains("was not asked"), "{reason}");
                assert!(reason.contains("do not retry"), "{reason}");
            }
            other => panic!("unattended must deny, got {other:?}"),
        }
    }

    #[test]
    fn edit_ok_covers_workspace_writes_and_nothing_else() {
        let inside = gated(ActionKind::Write, "/work/a.txt", SideEffect::LocalMutation);
        assert!(
            PermissionMode::AcceptEdits
                .resolve(prompt(), &inside, posture(Some("/work")))
                .is_allowed()
        );
        for req in [
            gated(
                ActionKind::Write,
                "/home/u/.ssh/authorized_keys",
                SideEffect::LocalMutation,
            ),
            gated(ActionKind::Write, "/work/a.txt", SideEffect::Destructive),
            gated(ActionKind::Execute, "/work", SideEffect::LocalMutation),
            gated(ActionKind::Network, "https://example.com", SideEffect::ExternalChange),
        ] {
            let after = PermissionMode::AcceptEdits.resolve(prompt(), &req, posture(Some("/work")));
            assert!(
                matches!(after, Decision::RequireApproval { .. }),
                "`{}` must still ask",
                req.verb
            );
        }
    }

    #[test]
    fn smart_tiers_by_action_and_effect_without_a_model_call() {
        let cases = [
            (
                gated(ActionKind::Read, "/etc/hosts", SideEffect::None),
                true,
                "reads are free",
            ),
            (
                gated(ActionKind::Write, "/work/a.txt", SideEffect::LocalMutation),
                true,
                "workspace writes are the point",
            ),
            (
                gated(ActionKind::Execute, "/work", SideEffect::LocalMutation),
                true,
                "local commands run, tiered",
            ),
            (
                gated(ActionKind::Write, "/work/a.txt", SideEffect::Destructive),
                false,
                "destruction always asks",
            ),
            (
                gated(ActionKind::Network, "https://example.com", SideEffect::ExternalChange),
                false,
                "the network leaves the machine",
            ),
            (
                gated(ActionKind::Delegate, "agent-child", SideEffect::None),
                false,
                "spawning authority is not a local edit",
            ),
        ];
        for (req, expect_auto, why) in cases {
            let after = PermissionMode::Smart.resolve(prompt(), &req, posture(Some("/work")));
            assert_eq!(after.is_allowed(), expect_auto, "{why} (verb {})", req.verb);
        }
    }

    #[test]
    fn a_root_that_merely_looks_like_a_prefix_is_not_the_workspace() {
        let req = gated(
            ActionKind::Write,
            "/work/proj-evil/a.txt",
            SideEffect::LocalMutation,
        );
        assert!(
            !PermissionMode::AcceptEdits
                .resolve(prompt(), &req, posture(Some("/work/proj")))
                .is_allowed()
        );
    }

    #[test]
    fn every_upstream_spelling_reaches_the_same_mode() {
        let expected = [
            ("ask", PermissionMode::Ask),
            ("Manual", PermissionMode::Ask),
            ("default", PermissionMode::Ask),
            ("acceptEdits", PermissionMode::AcceptEdits),
            ("auto_edit", PermissionMode::AcceptEdits),
            ("edit-ok", PermissionMode::AcceptEdits),
            ("plan", PermissionMode::Plan),
            ("smart", PermissionMode::Smart),
            ("auto", PermissionMode::Smart),
            ("dontAsk", PermissionMode::Unattended),
            ("unattended", PermissionMode::Unattended),
            ("yolo", PermissionMode::Yolo),
            ("bypassPermissions", PermissionMode::Yolo),
        ];
        for (name, mode) in expected {
            assert_eq!(PermissionMode::parse(name), Some(mode), "{name}");
            assert_eq!(mode.as_str(), PermissionMode::parse(name).unwrap().as_str());
        }
        assert_eq!(PermissionMode::parse("everything-allowed"), None);
    }

    #[test]
    fn modes_round_trip_through_the_config_format() {
        for mode in PermissionMode::all() {
            let json = serde_json::to_string(&mode).unwrap();
            assert_eq!(serde_json::from_str::<PermissionMode>(&json).unwrap(), mode);
        }
        assert_eq!(PermissionMode::default(), PermissionMode::Ask);
    }

    #[test]
    fn only_the_auto_approving_postures_are_classified_as_such() {
        assert!(PermissionMode::AcceptEdits.auto_approves());
        assert!(PermissionMode::Smart.auto_approves());
        assert!(PermissionMode::Yolo.auto_approves());
        assert!(!PermissionMode::Ask.auto_approves());
        assert!(!PermissionMode::Plan.auto_approves());
        // Nothing that never grants permission can be "always allow", including the deny posture.
        assert!(!PermissionMode::Unattended.auto_approves());
        assert!(PermissionMode::Yolo.is_bypass());
        assert!(!PermissionMode::Smart.is_bypass());
    }
}
