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

use std::collections::HashMap;
use std::path::Path;
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

/// The capability envelope one session's calls are evaluated under.
///
/// A worker's scope is computed at spawn from the intersection of its parent's envelope and the
/// brief's asks, and that intersection is the whole security story: a child can never hold more
/// than its parent, so a denial the parent earned is a denial every descendant inherits. `None`
/// fields are the operator's own posture — unrestricted.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct CapabilityScope {
    /// Tool verbs this session may call, as exact verbs or family heads (`fs` covers `fs.write`).
    /// `None` means every registered tool.
    pub tools: Option<Vec<String>>,
    /// Absolute subtrees file actions may land in. `None` means the whole workspace.
    pub file_scope: Option<Vec<String>>,
}

impl CapabilityScope {
    pub fn unrestricted() -> Self {
        Self::default()
    }

    pub fn is_unrestricted(&self) -> bool {
        self.tools.is_none() && self.file_scope.is_none()
    }

    /// The scope a child receives for one brief. `None` on the parent means the operator's own
    /// posture, so the child gets exactly what the brief asked for; a scoped parent narrows every
    /// ask to what the parent already held — subtree-aware, so a brief naming a subdirectory of a
    /// granted tree keeps it, while a brief naming a parent of a granted tree gets nothing.
    pub fn narrowed(parent: &Self, brief_tools: &[String], brief_files: &[String]) -> Self {
        let tools = Some(match &parent.tools {
            None => brief_tools.to_vec(),
            Some(parent_tools) => brief_tools
                .iter()
                .filter(|tool| parent_tools.contains(tool))
                .cloned()
                .collect(),
        });
        let file_scope = Some(match &parent.file_scope {
            None => brief_files.to_vec(),
            Some(parent_files) => brief_files
                .iter()
                .filter(|file| parent_files.iter().any(|root| subtree_holds(root, file)))
                .cloned()
                .collect(),
        });
        Self { tools, file_scope }
    }

    /// Whether a verb survives the tool allowlist. An entry matches the exact verb or its dotted
    /// descendants — `fs.read` covers `fs.read.x`, never `fs.readx` — mirroring how rule verb
    /// prefixes are matched, so a family head (`fs`) and a full verb grant behave alike.
    pub fn allows_verb(&self, verb: &str) -> bool {
        self.tools.as_ref().is_none_or(|list| {
            list.iter().any(|allowed| {
                allowed == verb
                    || verb
                        .strip_prefix(allowed.as_str())
                        .is_some_and(|rest| rest.starts_with('.'))
            })
        })
    }

    /// Whether a path stays inside the file scope. Unresolved or absolute-elsewhere paths are
    /// outside by construction — the scope only ever narrows.
    pub fn contains_path(&self, path: &str) -> bool {
        self.file_scope
            .as_ref()
            .is_none_or(|roots| roots.iter().any(|root| subtree_holds(root, path)))
    }
}

/// Whether `path` sits inside the subtree rooted at `root`: the root itself, or anything strictly
/// beneath it. A lookalike prefix (`/ws/proj-evil` under `/ws/proj`) never qualifies.
fn subtree_holds(root: &str, path: &str) -> bool {
    path == root
        || path
            .strip_prefix(root)
            .is_some_and(|rest| rest.starts_with('/'))
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

    pub fn deny(action: ActionKind, resource: ResourcePattern, reason: impl Into<String>) -> Self {
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
        // An empty action list matches nothing: a rule that names no action never fires, instead
        // of silently becoming a wildcard.
        if self.actions.is_empty() || !self.actions.contains(&req.action) {
            return false;
        }
        if let Some(prefix) = &self.verb_prefix {
            // Segment-boundary match: `fs.read` covers `fs.read` and `fs.read.x`, never `fs.readx`.
            let verb_matches = req.verb == *prefix
                || req
                    .verb
                    .strip_prefix(prefix.as_str())
                    .is_some_and(|rest| rest.starts_with('.'));
            if !verb_matches {
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
/// costs a prompt — an allow rule that does not match is a rule that asks. Redirection,
/// substitution, and grouping operators split too: `cargo test > /etc/x` is two effects, not one
/// approved word, and `cargo test $(id)` is not the command that was saved.
pub fn command_segments(command: &str) -> Vec<&str> {
    let bytes = command.as_bytes();
    let mut segments = Vec::new();
    let mut start = 0;
    let mut i = 0;
    while i < bytes.len() {
        // `$(` and `((` are two-byte operators; every other delimiter is a single byte.
        let pair = i + 1 < bytes.len() && bytes[i + 1] == b'(' && matches!(bytes[i], b'$' | b'(');
        let single = matches!(
            bytes[i],
            b'&' | b'|' | b';' | b'\n' | b'>' | b'<' | b'`' | b'{' | b'}' | b'(' | b')'
        );
        if pair || single {
            let segment = command[start..i].trim();
            if !segment.is_empty() {
                segments.push(segment);
            }
            i += if pair { 2 } else { 1 };
            start = i;
        } else {
            i += 1;
        }
    }
    let segment = command[start..].trim();
    if !segment.is_empty() {
        segments.push(segment);
    }
    segments
}

/// Constraints no rule may relax.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MandatoryFloor {
    /// Mutations outside this root are always denied, whatever the rules say.
    pub workspace_root: Option<String>,
    /// Additional roots the host itself declares writable (VIKRAM's own session workspaces under
    /// its home). Fixed at boot from the runtime, never from rules or model content, so the floor
    /// stays deny-first: it widens nothing the operator did not already own.
    pub extra_write_roots: Vec<String>,
    /// Verbs that are never permitted from content the model or a tool produced.
    pub forbidden_from_untrusted: Vec<String>,
    /// How deep delegation may nest before it is refused outright.
    pub max_delegation_depth: u32,
}

impl Default for MandatoryFloor {
    fn default() -> Self {
        Self {
            workspace_root: None,
            extra_write_roots: Vec::new(),
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
            && let Some(path) = resource_path(req)
        {
            let root = root.trim_end_matches('/');
            // A root that trims to nothing ("/" or "") confines nothing: resolving it would
            // accept every path, so the floor fails closed and treats the request as outside.
            if root.is_empty() {
                return Some(format!(
                    "`{}` outside the workspace root `{root}` ({:?} {})",
                    req.verb, req.action, req.resource,
                ));
            }
            let base = if path.is_absolute() {
                path.clone()
            } else {
                std::path::Path::new(root).join(&path)
            };
            let resolved = resolve_symlinks(&base);
            let within = |root: &str| {
                let root = root.trim_end_matches('/');
                if root.is_empty() {
                    return false;
                }
                resolve_symlinks(std::path::Path::new(root)).is_some_and(|canonical_root| {
                    resolved.as_ref().is_some_and(|resolved| {
                        *resolved == canonical_root || resolved.starts_with(&canonical_root)
                    })
                })
            };
            if !(within(root) || self.extra_write_roots.iter().any(|extra| within(extra))) {
                return Some(format!(
                    "`{}` outside the workspace root `{root}` ({:?} {})",
                    req.verb, req.action, req.resource,
                ));
            }
        }

        None
    }
}

/// Resolve a path against a root and refuse escapes — the same semantics as the runtime's
/// `resolve_within`, duplicated here because this crate cannot depend on it.
fn resolve_within(root: &Path, candidate: &Path) -> Option<std::path::PathBuf> {
    let canonical_root = resolve_symlinks(root)?;
    let resolved = if candidate.is_absolute() {
        resolve_symlinks(candidate)?
    } else {
        resolve_symlinks(&canonical_root.join(candidate))?
    };
    (resolved == canonical_root || resolved.starts_with(&canonical_root)).then_some(resolved)
}

/// Resolve `path` one component at a time, canonicalizing every component that exists so symlink
/// targets — including the parent a `..` climbs to — are the real ones. A component that exists
/// but cannot be canonicalized refuses the whole path; a not-yet-existing tail is kept verbatim
/// because it cannot contain symlinks.
fn resolve_symlinks(path: &Path) -> Option<std::path::PathBuf> {
    let absolute = if path.is_absolute() {
        path.to_path_buf()
    } else {
        std::env::current_dir().ok()?.join(path)
    };
    let mut current = std::path::PathBuf::new();
    for component in absolute.components() {
        use std::path::Component;
        match component {
            Component::Prefix(prefix) => current.push(prefix.as_os_str()),
            Component::RootDir => current.push(component.as_os_str()),
            Component::CurDir => {}
            Component::ParentDir => {
                current.pop();
            }
            Component::Normal(name) => {
                current.push(name);
                match current.canonicalize() {
                    Ok(canonical) => current = canonical,
                    Err(_) => {
                        if current.symlink_metadata().is_ok() {
                            return None;
                        }
                    }
                }
            }
        }
    }
    Some(current)
}

/// The resource as a filesystem path, for actions whose side effects are confined to the
/// workspace. `Execute` counts when the command itself is an absolute path (a bare program name
/// like `cargo` is not a path and is not confinable this way); `Network` counts only for
/// `file://` — remote hosts are outside any filesystem root by definition and are governed by
/// network rules, not the floor.
fn resource_path(req: &PermissionRequest) -> Option<std::path::PathBuf> {
    match req.action {
        ActionKind::Read | ActionKind::Write => {
            let trimmed = req.resource.trim();
            (!trimmed.is_empty()).then(|| std::path::PathBuf::from(trimmed))
        }
        ActionKind::Execute => {
            let head = req.resource.split_whitespace().next()?;
            let path = std::path::PathBuf::from(head);
            path.is_absolute().then_some(path)
        }
        ActionKind::Network => req
            .resource
            .trim()
            .strip_prefix("file://")
            .map(std::path::PathBuf::from),
        _ => None,
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
    /// Per-tool permission overrides: tool name -> "allow"/"deny"/"ask". Checked before the rules.
    pub tool_permissions: HashMap<String, String>,
}

impl Policy {
    pub fn new(rules: Vec<Rule>, floor: MandatoryFloor) -> Self {
        Self {
            rules,
            defaults: Vec::new(),
            floor,
            tool_permissions: HashMap::new(),
        }
    }

    pub fn with_defaults(rules: Vec<Rule>, defaults: Vec<Rule>, floor: MandatoryFloor) -> Self {
        Self {
            rules,
            defaults,
            floor,
            tool_permissions: HashMap::new(),
        }
    }

    pub fn empty() -> Self {
        Self {
            rules: Vec::new(),
            defaults: Vec::new(),
            floor: MandatoryFloor::default(),
            tool_permissions: HashMap::new(),
        }
    }

    /// Attach per-tool permission overrides to this policy.
    pub fn with_tool_permissions(mut self, overrides: HashMap<String, String>) -> Self {
        self.tool_permissions = overrides;
        self
    }

    /// Alias for [`Self::with_tool_permissions`].
    pub fn with_overrides(self, overrides: HashMap<String, String>) -> Self {
        self.with_tool_permissions(overrides)
    }

    /// Alias for [`Self::with_tool_permissions`].
    pub fn with_permissions(self, overrides: HashMap<String, String>) -> Self {
        self.with_tool_permissions(overrides)
    }

    pub fn rules(&self) -> &[Rule] {
        &self.rules
    }

    pub fn defaults(&self) -> &[Rule] {
        &self.defaults
    }

    /// How deep delegation may nest. Callers that spawn a session check this before the gate does.
    pub fn max_delegation_depth(&self) -> u32 {
        self.floor.max_delegation_depth
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

        // Deny is final across every source: a saved rule, a built-in default, or a config
        // override. An allow from any of them cannot outvote a refusal.
        if self.tool_override(&req.verb).is_some_and(|v| v == "deny") {
            return Decision::Deny {
                reason: format!("tool `{}` is denied by config override", req.verb),
            };
        }

        let rules_decision = Self::scan(&self.rules, &req);
        let defaults_decision = Self::scan(&self.defaults, &req);
        for decision in rules_decision.iter().chain(defaults_decision.iter()) {
            if matches!(decision, Decision::Deny { .. }) {
                return decision.clone();
            }
        }

        // Overrides still outrank prompts and quiet allows, but never a deny.
        if let Some(override_value) = self.tool_override(&req.verb) {
            match override_value.as_str() {
                "allow" => return Decision::Allow,
                "ask" => {
                    return Decision::RequireApproval {
                        reason: format!("tool `{}` requires approval by config override", req.verb),
                        amendment: Some(vikram_contracts::amendment_for(&req)),
                    };
                }
                _ => {}
            }
        }

        // Operator rules decide before the defaults they were written against — an operator allow
        // must be able to end a default's prompt. Denies were already settled above, in both tiers.
        rules_decision
            .or(defaults_decision)
            .unwrap_or_else(|| Decision::Deny {
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
                        pending_ask =
                            Some((reason.clone(), Some(vikram_contracts::amendment_for(req))));
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
        if root.is_empty() || std::path::Path::new(&req.resource).is_absolute() {
            return req.clone();
        }
        if let Some(resolved) = resolve_within(
            std::path::Path::new(root),
            std::path::Path::new(&req.resource),
        ) {
            let mut out = req.clone();
            out.resource = resolved.to_string_lossy().into_owned();
            out
        } else {
            req.clone()
        }
    }
    /// `tool_permissions` is documented as "tool name -> allow/deny/ask", but requests carry a
    /// verb like `terminal.open` or `process.run`. Match the exact verb first, then the tool
    /// family (the segment before the first `.`) so `{"terminal": "deny"}` is not a dead rule.
    fn tool_override<'a>(&'a self, verb: &str) -> Option<&'a String> {
        self.tool_permissions.get(verb).or_else(|| {
            verb.split('.')
                .next()
                .and_then(|head| self.tool_permissions.get(head))
        })
    }

    /// Whether a rule the operator wrote permits this request. `allow-list` mode asks exactly this:
    /// a default that allows is not enough, the human has to have saved the shape.
    pub fn has_allow(&self, req: &PermissionRequest) -> bool {
        let req = self.normalize_request(req);
        if self.tool_override(&req.verb).is_some_and(|v| v == "allow") {
            return true;
        }
        self.rules
            .iter()
            .any(|rule| rule.outcome == RuleOutcome::Allow && rule.applies_to(&req))
    }
}

/// Whether a rule the human is about to keep would grant reach past `root`.
///
/// Checked when the rule is *persisted*, not only when it is used: the mandatory floor already
/// vetoes an out-of-workspace write at decision time, so an entry pointing outside the workspace is
/// dead weight until the root changes — and the day it changes, a file nobody re-read becomes the
/// permission. Only `allow` is a grant, and only file actions name a path; a command prefix and a
/// network target are checked by their own rules, not as paths.
pub fn allow_reaches_outside(rule: &Rule, root: &Path) -> bool {
    if rule.outcome != RuleOutcome::Allow {
        return false;
    }
    if !rule
        .actions
        .iter()
        .any(|a| matches!(a, ActionKind::Read | ActionKind::Write))
    {
        return false;
    }
    let value = match &rule.resource {
        ResourcePattern::Any => return true,
        ResourcePattern::Exact(s) | ResourcePattern::Subtree(s) | ResourcePattern::Prefix(s) => s,
    };
    let path = std::path::Path::new(value);
    // Absolute entries are checked as given; relative ones resolve against the root when used,
    // so only a climb — or a symlink — can carry them outside.
    resolve_within(root, path).is_none()
}

/// Parse the amendment string the runtime proposed back into a rule. Deliberately strict: anything/// that is not exactly the shape we emit is refused rather than guessed at, because this text is the
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
    fn the_floor_refuses_executing_an_absolute_binary_outside_the_root() {
        let policy = Policy::empty();
        let req = PermissionRequest::new(
            ActionKind::Execute,
            "exec",
            "/usr/bin/danger",
            vikram_contracts::SideEffect::LocalMutation,
            Trust::Model,
        );
        assert!(
            matches!(policy.evaluate(&req), Decision::Deny { .. }),
            "/usr/bin/danger is not a workspace path"
        );
    }

    #[test]
    fn the_floor_ignores_bare_command_names_for_execute() {
        let policy = Policy::empty();
        let req = PermissionRequest::new(
            ActionKind::Execute,
            "shell.run",
            "cargo",
            vikram_contracts::SideEffect::LocalMutation,
            Trust::Model,
        );
        let decision = policy.evaluate(&req);
        if let Decision::Deny { reason } = decision {
            assert!(
                !reason.contains("workspace root"),
                "a bare name is not a path the floor can confine: {reason}"
            );
        }
    }

    #[test]
    fn the_floor_refuses_file_urls_pointing_outside_the_root() {
        let policy = Policy::empty();
        let req = PermissionRequest::new(
            ActionKind::Network,
            "net.read",
            "file:///etc/passwd",
            vikram_contracts::SideEffect::LocalMutation,
            Trust::Model,
        );
        assert!(matches!(policy.evaluate(&req), Decision::Deny { .. }));
    }

    #[test]
    fn a_rule_with_no_actions_matches_nothing() {
        let rule = Rule {
            actions: Vec::new(),
            verb_prefix: None,
            resource: crate::ResourcePattern::Any,
            outcome: RuleOutcome::Allow,
        };
        let policy = Policy::new(vec![rule], MandatoryFloor::default());
        assert!(
            !matches!(
                policy.evaluate(&fs_write("/work/a.txt", Trust::User)),
                Decision::Allow
            ),
            "empty actions must not become a wildcard"
        );
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

    #[cfg(unix)]
    fn tempdir(tag: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "vikram-policy-{tag}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[cfg(unix)]
    #[test]
    fn the_floor_refuses_writes_that_escape_through_a_symlink() {
        let base = tempdir("symlink-floor");
        let root = base.join("ws");
        let outside = base.join("outside");
        std::fs::create_dir_all(&root).unwrap();
        std::fs::create_dir_all(&outside).unwrap();
        std::os::unix::fs::symlink(&outside, root.join("link")).unwrap();

        let floor = MandatoryFloor {
            workspace_root: Some(root.to_string_lossy().into_owned()),
            ..Default::default()
        };
        let policy = Policy::new(
            vec![Rule::allow(ActionKind::Write, ResourcePattern::Any)],
            floor,
        );

        let inside = root.join("ok.txt").to_string_lossy().into_owned();
        assert!(
            policy
                .evaluate(&fs_write(&inside, Trust::Model))
                .is_allowed()
        );

        let through_link = root.join("link/evil.txt").to_string_lossy().into_owned();
        assert!(matches!(
            policy.evaluate(&fs_write(&through_link, Trust::Model)),
            Decision::Deny { .. }
        ));

        let climbing = root.join("link/../evil.txt").to_string_lossy().into_owned();
        assert!(matches!(
            policy.evaluate(&fs_write(&climbing, Trust::Model)),
            Decision::Deny { .. }
        ));

        assert!(matches!(
            policy.evaluate(&fs_write("link/evil.txt", Trust::Model)),
            Decision::Deny { .. }
        ));

        std::fs::remove_dir_all(&base).unwrap();
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
        assert!(matches!(guarded.evaluate(&smuggled), Decision::Deny { .. }));
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
        assert_eq!(text, "allow execute shell.run resource=cargo *");
        let rule = parse_amendment(&text).expect("the runtime's own amendment parses");
        assert_eq!(rule.verb_prefix.as_deref(), Some("shell.run"));
        assert_eq!(rule.resource, ResourcePattern::Prefix("cargo".into()));
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
            assert!(
                parse_amendment(text).is_none(),
                "{text} must not become a rule"
            );
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

    #[test]
    fn extra_write_roots_are_the_only_way_outside_the_workspace_becomes_writable() {
        let floor = MandatoryFloor {
            workspace_root: Some("/work/proj".into()),
            extra_write_roots: vec!["/home/u/.vikram/sessions".into()],
            ..Default::default()
        };
        let policy = Policy::new(
            vec![Rule::allow(ActionKind::Write, ResourcePattern::Any)],
            floor,
        );
        assert!(
            policy
                .evaluate(&fs_write("/home/u/.vikram/sessions/note.md", Trust::Model))
                .is_allowed(),
            "a host-declared root is writable"
        );
        assert!(
            matches!(
                policy.evaluate(&fs_write("/home/u/.vikram/sessions-evil/x", Trust::Model)),
                Decision::Deny { .. }
            ),
            "a lookalike prefix of an extra root is not inside it"
        );
        assert!(matches!(
            policy.evaluate(&fs_write("/home/u/.vikram/config.json", Trust::Model)),
            Decision::Deny { .. }
        ));
    }

    #[test]
    fn a_traversal_that_climbs_out_of_the_root_is_refused() {
        let floor = MandatoryFloor {
            workspace_root: Some("/work/proj".into()),
            ..Default::default()
        };
        let policy = Policy::new(
            vec![Rule::allow(ActionKind::Write, ResourcePattern::Any)],
            floor,
        );
        assert!(
            matches!(
                policy.evaluate(&fs_write("/work/proj/../outside.txt", Trust::Model)),
                Decision::Deny { .. }
            ),
            "/work/proj/../outside.txt joins the root but climbs back out"
        );
        assert!(matches!(
            policy.evaluate(&fs_write("../outside.txt", Trust::Model)),
            Decision::Deny { .. }
        ));
    }

    #[test]
    fn a_relative_write_is_normalised_against_the_root_before_ruling() {
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
                .evaluate(&fs_write("src/main.rs", Trust::Model))
                .is_allowed(),
            "relative paths belong inside the workspace"
        );
    }

    #[test]
    fn a_tool_permission_override_allow_returns_allow() {
        let mut overrides = HashMap::new();
        overrides.insert("shell.run".to_owned(), "allow".to_owned());
        let policy = Policy::empty().with_tool_permissions(overrides);
        let req = PermissionRequest::new(
            ActionKind::Execute,
            "shell.run",
            "cargo publish",
            SideEffect::LocalMutation,
            Trust::Model,
        );
        assert!(policy.evaluate(&req).is_allowed());
    }

    #[test]
    fn a_tool_permission_override_deny_wins_over_an_allow_rule() {
        let mut overrides = HashMap::new();
        overrides.insert("shell.run".to_owned(), "deny".to_owned());
        let policy = Policy::with_defaults(
            vec![Rule::allow(ActionKind::Execute, ResourcePattern::Any)],
            Vec::new(),
            MandatoryFloor::default(),
        )
        .with_tool_permissions(overrides);
        let req = PermissionRequest::new(
            ActionKind::Execute,
            "shell.run",
            "cargo test",
            SideEffect::LocalMutation,
            Trust::Model,
        );
        assert!(matches!(policy.evaluate(&req), Decision::Deny { .. }));
    }

    #[test]
    fn a_tool_permission_override_ask_returns_approval() {
        let mut overrides = HashMap::new();
        overrides.insert("fs.write".to_owned(), "ask".to_owned());
        let policy = Policy::empty().with_tool_permissions(overrides);
        let req = PermissionRequest::new(
            ActionKind::Write,
            "fs.write",
            "/tmp/x",
            SideEffect::LocalMutation,
            Trust::Model,
        );
        assert!(matches!(
            policy.evaluate(&req),
            Decision::RequireApproval { .. }
        ));
    }

    #[test]
    fn a_tool_permission_override_cannot_widen_the_mandatory_floor() {
        let mut overrides = HashMap::new();
        overrides.insert("fs.write".to_owned(), "allow".to_owned());
        let floor = MandatoryFloor {
            workspace_root: Some("/work/proj".into()),
            ..Default::default()
        };
        let policy = Policy::new(Vec::new(), floor).with_tool_permissions(overrides);
        let req = PermissionRequest::new(
            ActionKind::Write,
            "fs.write",
            "/home/himanshu/.ssh/authorized_keys",
            SideEffect::LocalMutation,
            Trust::Model,
        );
        assert!(matches!(policy.evaluate(&req), Decision::Deny { .. }));
    }

    #[test]
    fn an_allow_rule_does_not_cover_redirection_substitution_or_grouping() {
        let policy = allowlist_policy();
        for command in [
            "cargo test > /tmp/x",
            "cargo test >> /etc/cron.d/evil",
            "cargo test < /etc/passwd",
            "cargo test $(id)",
            "cargo test `id`",
            "cargo test { rm -rf /; }",
            "cargo test ((id))",
        ] {
            assert!(
                matches!(
                    policy.evaluate(&shell(command)),
                    Decision::RequireApproval { .. }
                ),
                "`{command}` smuggles an effect past `cargo test *`"
            );
        }
        // The unsullied command still runs quietly.
        assert!(policy.evaluate(&shell("cargo test -p x")).is_allowed());
    }

    #[test]
    fn a_deny_in_either_rule_tier_outranks_an_allow_in_the_other() {
        let policy = Policy::with_defaults(
            vec![Rule::allow(ActionKind::Execute, ResourcePattern::Any)],
            vec![Rule::deny(
                ActionKind::Execute,
                ResourcePattern::Any,
                "defaults say no",
            )],
            MandatoryFloor::default(),
        );
        assert!(matches!(
            policy.evaluate(&shell("cargo test")),
            Decision::Deny { .. }
        ));
        // The same holds with the tiers swapped: an operator deny beats a default allow.
        let swapped = Policy::with_defaults(
            vec![Rule::deny(
                ActionKind::Execute,
                ResourcePattern::Any,
                "operator said no",
            )],
            vec![Rule::allow(ActionKind::Execute, ResourcePattern::Any)],
            MandatoryFloor::default(),
        );
        assert!(matches!(
            swapped.evaluate(&shell("cargo test")),
            Decision::Deny { .. }
        ));
        // And an operator ask still loses to a default deny.
        let asked = Policy::with_defaults(
            vec![Rule::ask(
                ActionKind::Execute,
                ResourcePattern::Any,
                "review first",
            )],
            vec![Rule::deny(
                ActionKind::Execute,
                ResourcePattern::Any,
                "defaults say no",
            )],
            MandatoryFloor::default(),
        );
        assert!(matches!(
            asked.evaluate(&shell("cargo test")),
            Decision::Deny { .. }
        ));
    }

    #[test]
    fn a_tool_override_allow_cannot_bypass_a_deny_in_either_tier() {
        let mut overrides = HashMap::new();
        overrides.insert("shell.run".to_owned(), "allow".to_owned());
        let saved_deny = Policy::with_defaults(
            vec![Rule::deny(
                ActionKind::Execute,
                ResourcePattern::Any,
                "operator said no",
            )],
            Vec::new(),
            MandatoryFloor::default(),
        )
        .with_tool_permissions(overrides.clone());
        assert!(matches!(
            saved_deny.evaluate(&shell("cargo test")),
            Decision::Deny { .. }
        ));
        let default_deny = Policy::with_defaults(
            Vec::new(),
            vec![Rule::deny(
                ActionKind::Execute,
                ResourcePattern::Any,
                "defaults say no",
            )],
            MandatoryFloor::default(),
        )
        .with_tool_permissions(overrides);
        assert!(matches!(
            default_deny.evaluate(&shell("cargo test")),
            Decision::Deny { .. }
        ));
    }

    #[test]
    fn an_empty_or_filesystem_root_workspace_fails_closed() {
        for root in ["", "/"] {
            let floor = MandatoryFloor {
                workspace_root: Some(root.into()),
                ..Default::default()
            };
            let policy = Policy::new(
                vec![Rule::allow(ActionKind::Write, ResourcePattern::Any)],
                floor,
            );
            assert!(
                matches!(
                    policy.evaluate(&fs_write("/work/a.txt", Trust::Model)),
                    Decision::Deny { .. }
                ),
                "workspace root `{root}` trims to nothing and must confine nothing"
            );
        }
    }

    #[test]
    fn tool_and_web_sourced_content_is_untrusted_by_the_floor() {
        let policy = Policy::new(
            vec![Rule::allow(ActionKind::Admin, ResourcePattern::Any)],
            MandatoryFloor::default(),
        );
        let req = PermissionRequest::new(
            ActionKind::Admin,
            "policy.set",
            "*",
            SideEffect::ExternalChange,
            Trust::External,
        );
        match policy.evaluate(&req) {
            Decision::Deny { reason } => {
                assert!(reason.contains("tool or web-sourced content"), "{reason}")
            }
            other => panic!("external content must never widen policy, got {other:?}"),
        }
    }

    #[test]
    fn an_unrestricted_parent_grants_exactly_what_the_brief_asks() {
        let brief_tools = vec!["fs.read".to_owned(), "fs.write".to_owned()];
        let brief_files = vec!["/ws/a".to_owned()];
        let scope =
            CapabilityScope::narrowed(&CapabilityScope::unrestricted(), &brief_tools, &brief_files);
        assert_eq!(scope.tools.as_deref(), Some(&brief_tools[..]));
        assert_eq!(scope.file_scope.as_deref(), Some(&brief_files[..]));
        assert!(!scope.is_unrestricted());
    }

    #[test]
    fn a_scoped_parent_narrows_the_brief_to_what_it_already_held() {
        let parent = CapabilityScope {
            tools: Some(vec!["fs.read".to_owned(), "fs.write".to_owned()]),
            file_scope: Some(vec!["/ws/a".to_owned()]),
        };
        let brief_tools = vec!["fs.write".to_owned(), "shell.run".to_owned()];
        let brief_files = vec![
            "/ws/a".to_owned(),
            "/ws/a/sub".to_owned(),
            // A parent of the granted tree, and an unrelated tree, get nothing.
            "/ws".to_owned(),
            "/ws/b".to_owned(),
        ];
        let child = CapabilityScope::narrowed(&parent, &brief_tools, &brief_files);
        assert_eq!(
            child.tools.as_deref(),
            Some(&["fs.write".to_owned()][..]),
            "`shell.run` was never the parent's to grant"
        );
        assert_eq!(
            child.file_scope.as_deref(),
            Some(&["/ws/a".to_owned(), "/ws/a/sub".to_owned()][..])
        );
        // The child never exceeds the parent, whatever the brief claimed.
        for file in child.file_scope.unwrap() {
            assert!(parent.contains_path(&file), "child gained `{file}`");
        }
    }

    #[test]
    fn a_verb_the_allowlist_never_named_is_refused() {
        let scope = CapabilityScope {
            tools: Some(vec!["fs.read".to_owned()]),
            file_scope: None,
        };
        assert!(scope.allows_verb("fs.read"));
        assert!(scope.allows_verb("fs.read.x"));
        assert!(!scope.allows_verb("fs.write"));
        assert!(!scope.allows_verb("fs.readx"));
    }

    #[test]
    fn a_family_head_entry_covers_every_tool_in_that_family() {
        let scope = CapabilityScope {
            tools: Some(vec!["fs".to_owned()]),
            file_scope: None,
        };
        assert!(scope.allows_verb("fs.read"));
        assert!(scope.allows_verb("fs.write"));
        assert!(!scope.allows_verb("shell.run"));
    }

    #[test]
    fn the_file_scope_admits_subtrees_and_refuses_lookalikes() {
        let scope = CapabilityScope {
            tools: None,
            file_scope: Some(vec!["/ws/a".to_owned()]),
        };
        assert!(scope.contains_path("/ws/a"));
        assert!(scope.contains_path("/ws/a/x/y.txt"));
        assert!(!scope.contains_path("/ws/a-evil/x"));
        assert!(!scope.contains_path("/ws/b"));
    }

    #[test]
    fn narrowing_survives_any_number_of_generations() {
        let mut scope = CapabilityScope::narrowed(
            &CapabilityScope::unrestricted(),
            &["fs.read".to_owned(), "fs.write".to_owned()],
            &["/ws/a".to_owned(), "/ws/b".to_owned()],
        );
        for _ in 0..8 {
            scope = CapabilityScope::narrowed(
                &scope,
                &[
                    "fs.read".to_owned(),
                    "fs.write".to_owned(),
                    "shell.run".to_owned(),
                ],
                &["/ws/a/x".to_owned(), "/ws/b".to_owned()],
            );
            assert!(scope.allows_verb("fs.read"));
            assert!(!scope.allows_verb("shell.run"));
            assert!(scope.contains_path("/ws/a/x"));
            assert!(scope.contains_path("/ws/b/y"));
            assert!(!scope.contains_path("/ws/a/y"));
        }
    }
}
