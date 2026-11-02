//! Permission evaluation.
//!
//! Three properties the rest of the runtime relies on:
//! 1. Deny-first. A `Deny` outranks every `Allow`, and no rule at all means no permission.
//! 2. The mandatory floor cannot be widened away. Rules are evaluated *underneath* it, so neither a
//!    user config, a plugin, nor a substituted policy implementation can authorise something the
//!    floor forbids.
//! 3. Two rule tiers, in this order: operator-authored rules (config and the saved allow-list),
//!    then the runtime's built-in defaults for whatever none of them matched. An allow-list entry
//!    has to be able to end a prompt, which it could not do as one rule among defaults that ask.

use vikram_contracts::POLICY_EVALUATOR;
use vikram_contracts::policy::{
    ActionKind, Decision, PermissionPolicy, PermissionRequest, RuleOutcome,
};
use vikram_contracts::trust::Trust;
use vikram_contracts::version::{ContractDecl, Versioned};

/// Resource patterns are deliberately tiny: an exact string, a `prefix/**` subtree, a `cmd *`
/// command prefix, or `*` for any. A real glob engine would be a larger surface than the decision it
/// needs to support.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ResourcePattern {
    Any,
    Exact(String),
    Subtree(String),
    /// A command prefix, matched on whole words: `cargo test` covers `cargo test -p x`, never
    /// `cargo test-evil` and never a bare `cargo`.
    Prefix(String),
}

impl ResourcePattern {
    pub fn parse(pattern: &str) -> Self {
        if pattern == "*" {
            Self::Any
        } else if let Some(prefix) = pattern.strip_suffix("/**") {
            Self::Subtree(prefix.to_owned())
        } else if let Some(prefix) = pattern.strip_suffix(" *") {
            Self::Prefix(prefix.trim_end().to_owned())
        } else {
            Self::Exact(pattern.to_owned())
        }
    }

    pub fn matches(&self, resource: &str) -> bool {
        match self {
            Self::Any => true,
            Self::Exact(expected) => resource == expected,
            Self::Subtree(prefix) => {
                resource == prefix
                    || resource
                        .strip_prefix(prefix)
                        .is_some_and(|rest| rest.starts_with('/'))
            }
            Self::Prefix(prefix) => {
                resource == *prefix
                    || resource
                        .strip_prefix(prefix)
                        .is_some_and(|rest| rest.starts_with(' '))
            }
        }
    }

    pub fn as_str(&self) -> &str {
        match self {
            Self::Any => "*",
            Self::Exact(s) => s,
            Self::Subtree(s) => s,
            Self::Prefix(s) => s,
        }
    }

    /// The pattern as it is written in a rules file, which is lossy for [`Self::Prefix`] unless we
    /// keep the trailing ` *` marker.
    pub fn to_wire(&self) -> String {
        match self {
            Self::Any => "*".to_owned(),
            Self::Exact(s) => s.clone(),
            Self::Subtree(s) => format!("{s}/**"),
            Self::Prefix(s) => format!("{s} *"),
        }
    }
}

impl serde::Serialize for ResourcePattern {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(&self.to_wire())
    }
}

impl<'de> serde::Deserialize<'de> for ResourcePattern {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let raw = String::deserialize(deserializer)?;
        Ok(Self::parse(&raw))
    }
}

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct Rule {
    pub actions: Vec<ActionKind>,
    pub verb_prefix: Option<String>,
    pub resource: ResourcePattern,
    pub outcome: RuleOutcome,
}

impl Rule {
    pub fn allow(action: ActionKind, resource: ResourcePattern) -> Self {
        Self {
            actions: vec![action],
            verb_prefix: None,
            resource,
            outcome: RuleOutcome::Allow,
        }
    }

    /// An allow-list entry the operator keeps across sessions: one verb, one command or path prefix.
    pub fn allow_scoped(
        action: ActionKind,
        verb_prefix: impl Into<String>,
        resource: ResourcePattern,
    ) -> Self {
        Self {
            actions: vec![action],
            verb_prefix: Some(verb_prefix.into()),
            resource,
            outcome: RuleOutcome::Allow,
        }
    }

    pub fn deny(
        action: ActionKind,
        resource: ResourcePattern,
        reason: impl Into<String>,
    ) -> Self {
        Self {
            actions: vec![action],
            verb_prefix: None,
            resource,
            outcome: RuleOutcome::Deny {
                reason: reason.into(),
            },
        }
    }

    pub fn ask(action: ActionKind, resource: ResourcePattern, reason: impl Into<String>) -> Self {
        Self {
            actions: vec![action],
            verb_prefix: None,
            resource,
            outcome: RuleOutcome::Ask {
                reason: reason.into(),
            },
        }
    }

    pub fn with_verb_prefix(mut self, prefix: impl Into<String>) -> Self {
        self.verb_prefix = Some(prefix.into());
        self
    }

    /// The rule in the same one-line grammar [`parse_amendment`] reads, so `list` prints exactly what
    /// `add` accepts.
    pub fn describe(&self) -> String {
        let outcome = match &self.outcome {
            RuleOutcome::Allow => "allow",
            RuleOutcome::Deny { .. } => "deny",
            RuleOutcome::Ask { .. } => "ask",
        };
        let actions: Vec<String> = self.actions.iter().map(|a| a.to_string()).collect();
        format!(
            "{outcome} {} {} resource={}",
            if actions.is_empty() {
                "*".to_owned()
            } else {
                actions.join(",")
            },
            self.verb_prefix.clone().unwrap_or_else(|| "*".to_owned()),
            self.resource.to_wire()
        )
    }

    fn applies_to(&self, req: &PermissionRequest) -> bool {
        if !self.actions.is_empty() && !self.actions.contains(&req.action) {
            return false;
        }
        if let Some(prefix) = &self.verb_prefix {
            if !req.verb.starts_with(prefix.as_str()) {
                return false;
            }
        }
        self.matches_resource(req)
    }

    /// An `allow` must cover every command in the request; a `deny` or `ask` needs only one of them.
    /// Without that asymmetry `cargo test && rm -rf /` would inherit the permission saved for
    /// `cargo test`, which is the oldest trick in this book.
    fn matches_resource(&self, req: &PermissionRequest) -> bool {
        if req.action != ActionKind::Execute {
            return self.resource.matches(&req.resource);
        }
        let segments = command_segments(&req.resource);
        if segments.len() < 2 {
            return self.resource.matches(&req.resource);
        }
        match &self.outcome {
            RuleOutcome::Allow => segments
                .iter()
                .all(|segment| self.resource.matches(segment)),
            RuleOutcome::Deny { .. } | RuleOutcome::Ask { .. } => segments
                .iter()
                .any(|segment| self.resource.matches(segment)),
        }
    }
}

/// The separate commands inside one shell string. Splitting is textual, not a shell parse: a
/// command that merely contains one of these characters is over-split, and over-splitting only ever
/// costs a prompt — an allow rule that does not match is a rule that asks.
pub fn command_segments(command: &str) -> Vec<&str> {
    command
        .split(['&', '|', ';', '\n'])
        .map(str::trim)
        .filter(|segment| !segment.is_empty())
        .collect()
}

/// Constraints no rule may relax.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MandatoryFloor {
    /// Mutations outside this root always need a human, whatever the rules say.
    pub workspace_root: Option<String>,
    /// Verbs that are never permitted from content the model or a tool produced.
    pub forbidden_from_untrusted: Vec<String>,
    /// How deep delegation may nest before it is refused outright.
    pub max_delegation_depth: u32,
}

impl Default for MandatoryFloor {
    fn default() -> Self {
        Self {
            workspace_root: None,
            forbidden_from_untrusted: vec!["grant.".into(), "policy.".into(), "self.".into()],
            max_delegation_depth: 3,
        }
    }
}

impl MandatoryFloor {
    /// Returns `Some(reason)` when the request is forbidden regardless of configuration.
    pub fn veto(&self, req: &PermissionRequest, depth: u32) -> Option<String> {
        if req.originating_trust.is_untrusted()
            && self
                .forbidden_from_untrusted
                .iter()
                .any(|prefix| req.verb.starts_with(prefix.as_str()))
        {
            return Some(format!(
                "`{}` cannot be authorised by {}-sourced content",
                req.verb,
                trust_name(req.originating_trust)
            ));
        }

        if req.action == ActionKind::Delegate && depth >= self.max_delegation_depth {
            return Some(format!(
                "delegation depth {depth} exceeds the limit of {}",
                self.max_delegation_depth
            ));
        }

        if req.side_effect != vikram_contracts::SideEffect::None
            && let Some(root) = &self.workspace_root
        {
            let root = root.trim_end_matches('/');
            let outside = req.action == ActionKind::Write
                && req.resource != root
                && !req.resource.starts_with(&format!("{root}/"));
            if outside {
                return Some(format!(
                    "`{}` writes outside the workspace root `{root}`",
                    req.verb
                ));
            }
        }

        None
    }
}

fn trust_name(trust: Trust) -> &'static str {
    match trust {
        Trust::Host => "host",
        Trust::User => "user",
        Trust::Model => "model",
        Trust::External => "tool or web",
    }
}

#[derive(Debug, Clone, Default)]
pub struct Policy {
    /// Operator-authored rules: config plus the saved allow-list. Consulted first, because an
    /// allow-list entry the human wrote is the answer to "may this run" and the built-in defaults
    /// exist only to describe what has not been decided yet.
    rules: Vec<Rule>,
    /// The runtime's posture for anything no operator rule matches.
    defaults: Vec<Rule>,
    floor: MandatoryFloor,
}

impl Policy {
    pub fn new(rules: Vec<Rule>, floor: MandatoryFloor) -> Self {
        Self {
            rules,
            defaults: Vec::new(),
            floor,
        }
    }

    pub fn with_defaults(rules: Vec<Rule>, defaults: Vec<Rule>, floor: MandatoryFloor) -> Self {
        Self {
            rules,
            defaults,
            floor,
        }
    }

    pub fn empty() -> Self {
        Self {
            rules: Vec::new(),
            defaults: Vec::new(),
            floor: MandatoryFloor::default(),
        }
    }

    pub fn rules(&self) -> &[Rule] {
        &self.rules
    }

    pub fn defaults(&self) -> &[Rule] {
        &self.defaults
    }

    pub fn evaluate(&self, req: &PermissionRequest) -> Decision {
        self.evaluate_at_depth(req, 0)
    }

    pub fn evaluate_at_depth(&self, req: &PermissionRequest, depth: u32) -> Decision {
        // Normalise a workspace-relative resource against the floor's root so callers that report a
        // relative path are evaluated against the same scope as absolute ones.
        let req = self.normalize_request(req);
        if let Some(reason) = self.floor.veto(&req, depth) {
            return Decision::Deny { reason };
        }

        let decided = Self::scan(&self.rules, &req).or_else(|| Self::scan(&self.defaults, &req));

        decided.unwrap_or_else(|| Decision::Deny {
            reason: format!("no rule permits `{}` on `{}`", req.verb, req.resource),
        })
    }

    /// One tier: deny returns immediately, an `ask` outranks an `allow`, and matching nothing is not
    /// a decision — the caller falls through to the next tier.
    fn scan(rules: &[Rule], req: &PermissionRequest) -> Option<Decision> {
        let mut pending_ask = None;
        let mut pending_allow = false;

        for rule in rules {
            if !rule.applies_to(req) {
                continue;
            }
            match &rule.outcome {
                RuleOutcome::Deny { reason } => {
                    return Some(Decision::Deny {
                        reason: reason.clone(),
                    });
                }
                RuleOutcome::Ask { reason } => {
                    if pending_ask.is_none() {
                        pending_ask = Some((
                            reason.clone(),
                            Some(vikram_contracts::amendment_for(req)),
                        ));
                    }
                }
                RuleOutcome::Allow => pending_allow = true,
            }
        }

        if let Some((reason, amendment)) = pending_ask {
            return Some(Decision::RequireApproval { reason, amendment });
        }
        if pending_allow {
            return Some(Decision::Allow);
        }
        None
    }

    /// Resolve a workspace-relative resource against the floor's root, mirroring `resolve_within`
    /// semantics for the policy layer. Non-file actions and already-absolute resources are kept.
    fn normalize_request(&self, req: &PermissionRequest) -> PermissionRequest {
        use vikram_contracts::ActionKind;
        if !matches!(req.action, ActionKind::Read | ActionKind::Write) {
            return req.clone();
        }
        let Some(root) = &self.floor.workspace_root else {
            return req.clone();
        };
        let root = root.trim_end_matches('/');
        if std::path::Path::new(&req.resource).is_absolute() {
            return req.clone();
        }
        let mut normalized = std::path::PathBuf::new();
        for component in std::path::Path::new(root)
            .join(&req.resource)
            .components()
        {
            use std::path::Component;
            match component {
                Component::ParentDir => {
                    normalized.pop();
                }
                Component::CurDir => {}
                other => normalized.push(other.as_os_str()),
            }
        }
        if normalized.starts_with(root) {
            let mut out = req.clone();
            out.resource = normalized.to_string_lossy().into_owned();
            out
        } else {
            req.clone()
        }
    }
    /// Whether a rule the operator wrote permits this request. `allow-list` mode asks exactly this:
    /// a default that allows is not enough, the human has to have saved the shape.
    pub fn has_allow(&self, req: &PermissionRequest) -> bool {
        let req = self.normalize_request(req);
        self.rules
            .iter()
            .any(|rule| rule.outcome == RuleOutcome::Allow && rule.applies_to(&req))
    }
}

/// Parse the amendment string the runtime proposed back into a rule. Deliberately strict: anything
/// that is not exactly the shape we emit is refused rather than guessed at, because this text is the
/// only thing standing between "allow `cargo test`" and "allow everything".
pub fn parse_amendment(text: &str) -> Option<Rule> {
    // The resource is last and may contain spaces, so it is taken as the remainder, not a word.
    let (head, resource) = text.trim().split_once(" resource=")?;
    let resource = resource.trim();
    if resource.is_empty() || resource.contains('\n') {
        return None;
    }
    let mut parts = head.split_whitespace();
    let outcome = match parts.next()? {
        "allow" => RuleOutcome::Allow,
        "deny" => RuleOutcome::Deny {
            reason: "saved rule".into(),
        },
        "ask" => RuleOutcome::Ask {
            reason: "saved rule".into(),
        },
        _ => return None,
    };
    let action = match parts.next()? {
        "read" => ActionKind::Read,
        "write" => ActionKind::Write,
        "execute" => ActionKind::Execute,
        "network" => ActionKind::Network,
        "delegate" => ActionKind::Delegate,
        "spend" => ActionKind::Spend,
        "admin" => ActionKind::Admin,
        _ => return None,
    };
    let verb = parts.next()?;
    if parts.next().is_some() {
        return None;
    }
    let pattern = ResourcePattern::parse(resource);
    if !is_canonical(resource, &pattern) {
        // A wildcard anywhere other than where our own proposal puts one is a hand-written rule
        // reaching past the shape of the approval it claims to come from.
        return None;
    }
    if let ResourcePattern::Prefix(prefix) | ResourcePattern::Subtree(prefix) = &pattern
        && command_segments(prefix).len() > 1
    {
        // A saved prefix carrying a separator would be a rule that matches two commands while
        // authorising only the first one it was derived from.
        return None;
    }
    if pattern == ResourcePattern::Any && action != ActionKind::Read {
        // A saved `*` for anything but reads is the whole verb, which no amendment we emit says.
        return None;
    }
    Some(Rule {
        actions: vec![action],
        verb_prefix: Some(verb.to_owned()),
        resource: pattern,
        outcome,
    })
}

/// Whether the text is exactly the spelling [`ResourcePattern::parse`] reads back, with no second
/// wildcard hiding inside the value.
fn is_canonical(resource: &str, pattern: &ResourcePattern) -> bool {
    if pattern.to_wire() != resource {
        return false;
    }
    let value = match pattern {
        ResourcePattern::Any => return true,
        ResourcePattern::Exact(s) | ResourcePattern::Subtree(s) | ResourcePattern::Prefix(s) => s,
    };
    !value.contains('*')
}

impl Versioned for Policy {
    fn contract(&self) -> ContractDecl {
        ContractDecl::new(POLICY_EVALUATOR, 1, 0)
    }
}

impl PermissionPolicy for Policy {
    fn contract(&self) -> ContractDecl {
        <Self as Versioned>::contract(self)
    }

    fn evaluate_at_depth(&self, request: &PermissionRequest, depth: u32) -> Decision {
        Policy::evaluate_at_depth(self, request, depth)
    }

    fn is_specially_allowed(&self, request: &PermissionRequest) -> bool {
        Policy::has_allow(self, request)
    }

    fn workspace_root(&self) -> Option<String> {
        self.floor.workspace_root.clone()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use vikram_contracts::{SideEffect, Trust};

    fn fs_write(resource: &str, trust: Trust) -> PermissionRequest {
        PermissionRequest::new(
            ActionKind::Write,
            "fs.write",
            resource,
            SideEffect::LocalMutation,
            trust,
        )
    }

    #[test]
    fn deny_outranks_a_later_allow() {
        let policy = Policy::new(
            vec![
                Rule::deny(
                    ActionKind::Write,
                    ResourcePattern::Subtree("/etc".into()),
                    "system paths are read-only",
                ),
                Rule::allow(ActionKind::Write, ResourcePattern::Any),
            ],
            MandatoryFloor::default(),
        );
        let decision = policy.evaluate(&fs_write("/etc/passwd", Trust::User));
        assert!(matches!(decision, Decision::Deny { .. }));
    }

    #[test]
    fn absence_of_a_rule_is_a_deny() {
        let policy = Policy::empty();
        assert!(
            matches!(
                policy.evaluate(&fs_write("/tmp/x", Trust::User)),
                Decision::Deny { .. }
            ),
            "default must be deny"
        );
    }

    #[test]
    fn ask_outranks_allow_because_approval_is_the_stricter_answer() {
        let policy = Policy::new(
            vec![
                Rule::allow(ActionKind::Write, ResourcePattern::Any),
                Rule::ask(
                    ActionKind::Write,
                    ResourcePattern::Subtree("/home/u".into()),
                    "home writes need a human",
                ),
            ],
            MandatoryFloor::default(),
        );
        let decision = policy.evaluate(&fs_write("/home/u/notes.md", Trust::User));
        assert!(matches!(decision, Decision::RequireApproval { .. }));
    }

    #[test]
    fn an_allow_rule_cannot_widen_the_mandatory_floor() {
        // Exactly the attack a malicious or merely careless plugin config would try.
        let policy = Policy::new(
            vec![Rule::allow(ActionKind::Admin, ResourcePattern::Any)],
            MandatoryFloor::default(),
        );
        let req = PermissionRequest::new(
            ActionKind::Admin,
            "grant.self",
            "*",
            SideEffect::ExternalChange,
            Trust::Model,
        );
        let decision = policy.evaluate(&req);
        match decision {
            Decision::Deny { reason } => assert!(reason.contains("model-sourced content")),
            other => panic!("model output must never grant permission, got {other:?}"),
        }
    }

    #[test]
    fn user_typed_requests_are_not_blocked_by_the_untrusted_verb_floor() {
        let policy = Policy::new(
            vec![Rule::allow(ActionKind::Admin, ResourcePattern::Any)],
            MandatoryFloor::default(),
        );
        let req = PermissionRequest::new(
            ActionKind::Admin,
            "grant.scope",
            "*",
            SideEffect::ExternalChange,
            Trust::User,
        );
        assert!(policy.evaluate(&req).is_allowed());
    }

    #[test]
    fn delegation_is_cut_off_at_the_floor_limit() {
        let policy = Policy::new(
            vec![Rule::allow(ActionKind::Delegate, ResourcePattern::Any)],
            MandatoryFloor::default(),
        );
        let req = PermissionRequest::new(
            ActionKind::Delegate,
            "agent.spawn",
            "agent-child",
            SideEffect::None,
            Trust::Model,
        );
        assert!(policy.evaluate_at_depth(&req, 2).is_allowed());
        assert!(matches!(
            policy.evaluate_at_depth(&req, 3),
            Decision::Deny { .. }
        ));
    }

    #[test]
    fn writes_outside_the_workspace_root_are_refused_even_when_allowed_by_config() {
        let floor = MandatoryFloor {
            workspace_root: Some("/work/proj".into()),
            ..Default::default()
        };
        let policy = Policy::new(
            vec![Rule::allow(ActionKind::Write, ResourcePattern::Any)],
            floor,
        );
        assert!(
            policy
                .evaluate(&fs_write("/work/proj/src/main.rs", Trust::Model))
                .is_allowed()
        );
        assert!(matches!(
            policy.evaluate(&fs_write(
                "/home/himanshu/.ssh/authorized_keys",
                Trust::Model
            )),
            Decision::Deny { .. }
        ));
        // A prefix that merely looks like the root must not pass.
        assert!(matches!(
            policy.evaluate(&fs_write("/work/proj-evil/x", Trust::Model)),
            Decision::Deny { .. }
        ));
    }

    #[test]
    fn subtree_patterns_match_paths_and_not_their_lookalikes() {
        let pattern = ResourcePattern::parse("/tmp/**");
        assert!(pattern.matches("/tmp/a/b"));
        assert!(pattern.matches("/tmp"));
        assert!(!pattern.matches("/tmpx/a"));
        assert!(!ResourcePattern::parse("/etc").matches("/etc/passwd"));
    }

    #[test]
    fn verb_prefixes_narrow_a_rule() {
        let policy = Policy::new(
            vec![Rule::allow(ActionKind::Read, ResourcePattern::Any).with_verb_prefix("fs.read")],
            MandatoryFloor::default(),
        );
        let fs = PermissionRequest::new(
            ActionKind::Read,
            "fs.read",
            "/x",
            SideEffect::None,
            Trust::Model,
        );
        let env = PermissionRequest::new(
            ActionKind::Read,
            "env.read",
            "/x",
            SideEffect::None,
            Trust::Model,
        );
        assert!(policy.evaluate(&fs).is_allowed());
        assert!(matches!(policy.evaluate(&env), Decision::Deny { .. }));
    }

    #[test]
    fn approval_requests_carry_a_machine_proposed_amendment() {
        let policy = Policy::new(
            vec![Rule::ask(
                ActionKind::Write,
                ResourcePattern::Subtree("/work".into()),
                "review mutation",
            )],
            MandatoryFloor::default(),
        );
        match policy.evaluate(&fs_write("/work/proj/a.txt", Trust::User)) {
            Decision::RequireApproval { amendment, .. } => {
                let amendment = amendment.expect("amendment should be proposed");
                assert!(
                    amendment.starts_with("allow write fs.write resource="),
                    "{amendment}"
                );
            }
            other => panic!("expected approval, got {other:?}"),
        }
    }

    #[test]
    fn policy_declares_the_contract_the_kernel_checks() {
        let policy = Policy::empty();
        let decl = <Policy as PermissionPolicy>::contract(&policy);
        assert_eq!(decl.id, POLICY_EVALUATOR);
        assert!(decl.satisfied_by(vikram_contracts::HOST_CONTRACT).is_ok());
    }

    fn shell(command: &str) -> PermissionRequest {
        PermissionRequest::new(
            ActionKind::Execute,
            "shell.run",
            command,
            SideEffect::LocalMutation,
            Trust::Model,
        )
    }

    fn allowlist_policy() -> Policy {
        Policy::with_defaults(
            vec![Rule::allow_scoped(
                ActionKind::Execute,
                "shell.run",
                ResourcePattern::parse("cargo test *"),
            )],
            vec![Rule::ask(
                ActionKind::Execute,
                ResourcePattern::Any,
                "commands run with your authority",
            )],
            MandatoryFloor::default(),
        )
    }

    #[test]
    fn a_saved_allow_rule_ends_the_prompts_default_without_widening_it() {
        let policy = allowlist_policy();
        assert!(
            policy
                .evaluate(&shell("cargo test -p vikram-policy"))
                .is_allowed(),
            "the point of an allow-list is that it does not ask again"
        );
        // A different command still gets the default posture, not the saved permission.
        assert!(matches!(
            policy.evaluate(&shell("cargo publish")),
            Decision::RequireApproval { .. }
        ));
    }

    #[test]
    fn a_saved_allow_rule_cannot_reach_past_its_own_words() {
        let policy = allowlist_policy();
        for command in ["cargo", "cargo test-evil", "npm test", "cat cargo test"] {
            assert!(
                !policy.evaluate(&shell(command)).is_allowed(),
                "`{command}` is not `cargo test`"
            );
        }
    }

    #[test]
    fn an_allow_rule_must_cover_every_command_in_a_compound_request() {
        let policy = allowlist_policy();
        let smuggled = shell("cargo test && rm -rf /");
        assert!(
            matches!(policy.evaluate(&smuggled), Decision::RequireApproval { .. }),
            "the second command was never approved"
        );
        // The same shape on the deny side is the opposite: one matching segment is enough.
        let guarded = Policy::with_defaults(
            vec![
                Rule::allow_scoped(
                    ActionKind::Execute,
                    "shell.run",
                    ResourcePattern::parse("cargo test *"),
                ),
                Rule::deny(
                    ActionKind::Execute,
                    ResourcePattern::parse("rm *"),
                    "never delete this way",
                ),
            ],
            Vec::new(),
            MandatoryFloor::default(),
        );
        assert!(matches!(
            guarded.evaluate(&smuggled),
            Decision::Deny { .. }
        ));
        assert!(guarded.evaluate(&shell("cargo test")).is_allowed());
    }

    #[test]
    fn an_allow_rule_still_cannot_widen_the_floor() {
        let floor = MandatoryFloor {
            workspace_root: Some("/work/proj".into()),
            ..Default::default()
        };
        let policy = Policy::with_defaults(
            vec![Rule::allow_scoped(
                ActionKind::Write,
                "fs.write",
                ResourcePattern::Any,
            )],
            Vec::new(),
            floor,
        );
        assert!(matches!(
            policy.evaluate(&fs_write("/work/outside.txt", Trust::Model)),
            Decision::Deny { .. }
        ));
    }

    #[test]
    fn an_amendment_parses_back_into_the_rule_it_proposes() {
        let policy = Policy::with_defaults(
            Vec::new(),
            vec![Rule::ask(
                ActionKind::Execute,
                ResourcePattern::Any,
                "commands run with your authority",
            )],
            MandatoryFloor::default(),
        );
        let Decision::RequireApproval {
            amendment: Some(text),
            ..
        } = policy.evaluate(&shell("cargo test -p vikram-policy"))
        else {
            panic!("an ask rule always proposes an amendment");
        };
        assert_eq!(text, "allow execute shell.run resource=cargo test *");
        let rule = parse_amendment(&text).expect("the runtime's own amendment parses");
        assert_eq!(rule.verb_prefix.as_deref(), Some("shell.run"));
        assert_eq!(rule.resource, ResourcePattern::Prefix("cargo test".into()));
        assert_eq!(rule.outcome, RuleOutcome::Allow);

        let writes = Policy::with_defaults(
            Vec::new(),
            vec![Rule::ask(
                ActionKind::Write,
                ResourcePattern::Subtree("/work".into()),
                "review mutation",
            )],
            MandatoryFloor::default(),
        );
        let Decision::RequireApproval {
            amendment: Some(text),
            ..
        } = writes.evaluate(&fs_write("/work/proj/a.txt", Trust::User))
        else {
            panic!("amendment");
        };
        assert_eq!(text, "allow write fs.write resource=/work/proj/**");
        assert_eq!(
            parse_amendment(&text).expect("parses").resource,
            ResourcePattern::Subtree("/work/proj".into())
        );
    }

    #[test]
    fn a_hand_written_amendment_that_outruns_its_shape_is_refused() {
        for text in [
            "allow execute shell.run resource=*",
            "allow admin grant.scope resource=*",
            "allow execute shell.run resource=cargo test && rm *",
            "allow execute shell.run",
            "everything is fine",
            "allow execute shell.run resource=/work/** extra=argument",
        ] {
            assert!(parse_amendment(text).is_none(), "{text} must not become a rule");
        }
    }

    #[test]
    fn rules_round_trip_through_the_file_they_are_saved_in() {
        let rules = vec![
            Rule::allow_scoped(
                ActionKind::Execute,
                "shell.run",
                ResourcePattern::Prefix("cargo test".into()),
            ),
            Rule::allow(ActionKind::Write, ResourcePattern::Subtree("/work".into())),
            Rule::allow(ActionKind::Read, ResourcePattern::Any),
            Rule::deny(
                ActionKind::Network,
                ResourcePattern::Exact("https://evil.example".into()),
                "operator said no",
            ),
        ];
        let json = serde_json::to_string(&rules).expect("serialises");
        assert_eq!(
            serde_json::from_str::<Vec<Rule>>(&json).expect("deserialises"),
            rules,
            "a rule must survive its own file: {json}"
        );
    }
}
