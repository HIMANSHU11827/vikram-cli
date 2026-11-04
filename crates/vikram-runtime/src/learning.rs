//! Runtime-level self-improvement: learning review, proposals, approval, audit.
//!
//! Inspired by Hermes Agent's background review: after a meaningful turn, a background task
//! evaluates the conversation snapshot and proposes memory/skill changes. Changes are either
//! applied (auto mode) or staged for approval. Nothing modifies the live conversation or its
//! prompt cache. Learning never grants permissions.

use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex};
use std::time::SystemTime;

use serde::{Deserialize, Serialize};
use vikram_contracts::SessionId;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum LearningPolicy {
    Auto,
    ApprovalRequired,
    SuggestOnly,
    Disabled,
}

impl LearningPolicy {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Auto => "auto",
            Self::ApprovalRequired => "approval",
            Self::SuggestOnly => "suggest",
            Self::Disabled => "disabled",
        }
    }
    pub fn parse(value: &str) -> Self {
        match value.to_lowercase().as_str() {
            "auto" => Self::Auto,
            "approval" | "approval-required" | "approval_required" => Self::ApprovalRequired,
            "suggest" | "suggest-only" | "suggest_only" => Self::SuggestOnly,
            "disabled" | "off" => Self::Disabled,
            _ => Self::ApprovalRequired,
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct LearningProposal {
    pub id: String,
    pub session: SessionId,
    pub kind: String,   // "memory" | "skill"
    pub target: String, // "user" | "memory" | skill name
    pub action: String, // "add" | "replace" | "patch" | "create"
    pub key: String,
    pub content: String,
    pub reason: String,
    pub status: String, // "pending" | "approved" | "rejected" | "applied"
    pub created_ms: u64,
    pub before: Option<String>,
    pub after: Option<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct LearningAuditEntry {
    pub ts_ms: u64,
    pub session: SessionId,
    pub kind: String,
    pub action: String,
    pub target: String,
    pub outcome: String, // "applied" | "staged" | "rejected" | "skipped" | "error"
    pub detail: String,
}

#[derive(Serialize, Deserialize, Default)]
pub struct LearningState {
    #[serde(default)]
    pub proposals: Vec<LearningProposal>,
    #[serde(default)]
    pub audit: Vec<LearningAuditEntry>,
    #[serde(default = "default_policy_str")]
    pub policy_memory: String,
    #[serde(default = "default_policy_str")]
    pub policy_skill: String,
}

fn default_policy_str() -> String {
    "approval".to_owned()
}

pub struct LearningManager {
    proposals: Mutex<Vec<LearningProposal>>,
    audit: Mutex<Vec<LearningAuditEntry>>,
    /// Skills viewed during a session (read-before-write guard).
    viewed_skills: Mutex<HashMap<SessionId, HashSet<String>>>,
    policy_memory: Mutex<LearningPolicy>,
    policy_skill: Mutex<LearningPolicy>,
    policy_instruction: Mutex<LearningPolicy>,
    policy_identity: Mutex<LearningPolicy>,
    path: Mutex<Option<std::path::PathBuf>>,
    /// Finished turns per session, for the review gate. Not persisted: it bounds this run's spend.
    turns: Mutex<HashMap<SessionId, u64>>,
}

impl Default for LearningManager {
    fn default() -> Self {
        Self::new()
    }
}

impl LearningManager {
    pub fn new() -> Self {
        Self::open(None)
    }

    pub fn open(path: Option<std::path::PathBuf>) -> Self {
        let mgr = Self {
            proposals: Mutex::new(Vec::new()),
            audit: Mutex::new(Vec::new()),
            viewed_skills: Mutex::new(HashMap::new()),
            policy_memory: Mutex::new(LearningPolicy::ApprovalRequired),
            policy_skill: Mutex::new(LearningPolicy::ApprovalRequired),
            policy_instruction: Mutex::new(LearningPolicy::ApprovalRequired),
            policy_identity: Mutex::new(LearningPolicy::ApprovalRequired),
            path: Mutex::new(path),
            turns: Mutex::new(HashMap::new()),
        };
        if let Some(p) = mgr.path.lock().unwrap().clone() {
            if let Ok(data) = std::fs::read_to_string(&p) {
                if let Ok(state) = serde_json::from_str::<LearningState>(&data) {
                    *mgr.proposals.lock().unwrap() = state.proposals;
                    *mgr.audit.lock().unwrap() = state.audit;
                    *mgr.policy_memory.lock().unwrap() =
                        LearningPolicy::parse(&state.policy_memory);
                    *mgr.policy_skill.lock().unwrap() = LearningPolicy::parse(&state.policy_skill);
                }
            }
        }
        mgr
    }

    pub fn save(&self) {
        let path = self.path.lock().unwrap().clone();
        let Some(path) = path else {
            return;
        };
        let state = LearningState {
            proposals: self.proposals.lock().unwrap().clone(),
            audit: self.audit.lock().unwrap().clone(),
            policy_memory: self.policy_memory.lock().unwrap().as_str().to_owned(),
            policy_skill: self.policy_skill.lock().unwrap().as_str().to_owned(),
        };
        if let Some(parent) = path.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        if let Ok(json) = serde_json::to_string_pretty(&state) {
            let _ = std::fs::write(&path, json);
        }
    }

    pub fn path(&self) -> Option<std::path::PathBuf> {
        self.path.lock().unwrap().clone()
    }

    pub fn policy_memory(&self) -> LearningPolicy {
        *self.policy_memory.lock().unwrap()
    }
    pub fn policy_skill(&self) -> LearningPolicy {
        *self.policy_skill.lock().unwrap()
    }
    pub fn set_policy_memory(&self, p: LearningPolicy) {
        *self.policy_memory.lock().unwrap() = p;
    }
    pub fn set_policy_skill(&self, p: LearningPolicy) {
        *self.policy_skill.lock().unwrap() = p;
    }
    pub fn set_policy_instruction(&self, p: LearningPolicy) {
        *self.policy_instruction.lock().unwrap() = p;
    }
    pub fn set_policy_identity(&self, p: LearningPolicy) {
        *self.policy_identity.lock().unwrap() = p;
    }

    /// Record that a skill was viewed in this session (read-before-write guard).
    pub fn mark_skill_viewed(&self, session: &SessionId, name: &str) {
        self.viewed_skills
            .lock()
            .unwrap()
            .entry(session.clone())
            .or_default()
            .insert(name.to_owned());
    }

    pub fn skill_was_viewed(&self, session: &SessionId, name: &str) -> bool {
        self.viewed_skills
            .lock()
            .unwrap()
            .get(session)
            .map(|s| s.contains(name))
            .unwrap_or(false)
    }

    pub fn stage_proposal(&self, proposal: LearningProposal) {
        self.proposals.lock().unwrap().push(proposal);
    }

    pub fn pending_proposals(&self) -> Vec<LearningProposal> {
        self.proposals
            .lock()
            .unwrap()
            .iter()
            .filter(|p| p.status == "pending")
            .cloned()
            .collect()
    }

    pub fn all_proposals(&self) -> Vec<LearningProposal> {
        self.proposals.lock().unwrap().clone()
    }

    pub fn find_proposal(&self, id: &str) -> Option<LearningProposal> {
        self.proposals
            .lock()
            .unwrap()
            .iter()
            .find(|p| p.id == id)
            .cloned()
    }

    pub fn set_proposal_status(&self, id: &str, status: &str) {
        if let Some(p) = self
            .proposals
            .lock()
            .unwrap()
            .iter_mut()
            .find(|p| p.id == id)
        {
            p.status = status.to_owned();
        }
    }

    pub fn audit(&self, entry: LearningAuditEntry) {
        self.audit.lock().unwrap().push(entry);
    }

    pub fn audit_log(&self) -> Vec<LearningAuditEntry> {
        self.audit.lock().unwrap().clone()
    }

    pub fn dedup_memory(&self, existing: &[String], candidate: &str) -> bool {
        // Returns true if candidate is a duplicate of an existing entry.
        let cand = candidate.trim().to_lowercase();
        existing.iter().any(|e| e.trim().to_lowercase() == cand)
    }

    pub fn review_prompt(&self) -> &'static str {
        REVIEW_PROMPT
    }
}

const REVIEW_PROMPT: &str = r#"Review the conversation above and consider saving durable, reusable knowledge.

Memory routing:
- USER: who the user is — persona, preferences, communication style, expectations.
- MEMORY: facts about the environment — tool quirks, project conventions, paths, endpoints.

Skills: reusable procedures for a class of task. Create or patch a skill only when a non-trivial, reusable workflow emerged. Never create narrow one-session skills or duplicate existing ones. Patch existing skills before creating new ones. Read a skill before patching it.

Do NOT save: transient errors that resolved, environment-dependent failures, negative claims about tools, one-off task narratives, unresolved failures.

If nothing is worth saving, say 'Nothing to save.' and stop.

Output a JSON array of proposals (or [] if nothing). Each proposal:
{"kind":"memory"|"skill","target":"user"|"memory"|"<skill name>","action":"add"|"replace"|"patch"|"create","key":"...","content":"...","reason":"..."}

Only output the JSON array, no prose."#;

pub fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

/// What one turn did, as the growth pipeline sees it: which tools it reached for and which of
/// them came back failing. The turn loop feeds it, so a turn that never ran a tool produces an
/// empty record rather than a guess.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct TurnGrowth {
    pub tools: Vec<String>,
    pub errors: Vec<String>,
}

/// A failure note points at a result; it does not copy it.
pub const MAX_GROWTH_ERROR_LEN: usize = 200;

impl TurnGrowth {
    /// The turn ran this tool. A tool used twice is one tool used.
    pub fn record_call(&mut self, name: &str) {
        let name = name.to_owned();
        if !self.tools.contains(&name) {
            self.tools.push(name);
        }
    }

    /// A tool came back failing. The note names the tool and quotes as much of the result as a
    /// learning record is worth; a failure log is a pointer, not a copy.
    pub fn record_failure(&mut self, name: &str, content: &str) {
        let line = format!("{name}: {}", content.trim());
        if line.chars().count() > MAX_GROWTH_ERROR_LEN {
            self.errors
                .push(line.chars().take(MAX_GROWTH_ERROR_LEN).collect());
        } else {
            self.errors.push(line);
        }
    }

    pub fn had_tools(&self) -> bool {
        !self.tools.is_empty()
    }
}

/// What a reflection proposal is worth on the approval surface. Only kinds the surface can act on
/// are mapped; anything else stays in the evolution store, because a decision that applies nothing
/// when approved is worse than no decision.
pub fn proposal_from_evolution(
    proposal: &vikram_evolution::LearningProposal,
    session: &SessionId,
) -> Option<LearningProposal> {
    let (kind, action) = match proposal.kind.as_str() {
        "add_memory" => ("memory", "add"),
        "update_memory" => ("memory", "replace"),
        "create_skill" => ("skill", "create"),
        "improve_skill" => ("skill", "patch"),
        _ => return None,
    };
    let (target, content) = if kind == "memory" {
        let layer = proposal
            .details
            .get("layer")
            .and_then(|v| v.as_str())
            .unwrap_or("memory");
        let content = detail_text(&proposal.details);
        if content.trim().is_empty() {
            return None;
        }
        (
            if layer == "user" {
                "user".to_owned()
            } else {
                "memory".to_owned()
            },
            content,
        )
    } else {
        let category = proposal
            .details
            .get("category")
            .and_then(|v| v.as_str())
            .unwrap_or("learned");
        let tools = proposal
            .details
            .get("tools")
            .and_then(|v| v.as_array())
            .map(|arr| {
                arr.iter()
                    .filter_map(|v| v.as_str())
                    .collect::<Vec<_>>()
                    .join(", ")
            })
            .unwrap_or_default();
        let content = format!("{}\n\nTools: {tools}", proposal.summary);
        (format!("{category}-workflow"), content)
    };
    Some(LearningProposal {
        id: new_proposal_id(),
        session: session.clone(),
        kind: kind.to_owned(),
        target,
        action: action.to_owned(),
        key: format!("reflection-{}", proposal.id),
        content: content.clone(),
        reason: proposal.summary.clone(),
        status: "pending".to_owned(),
        created_ms: now_ms(),
        before: None,
        after: Some(content),
    })
}

fn detail_text(details: &serde_json::Value) -> String {
    for key in ["corrections", "content", "value"] {
        match details.get(key) {
            Some(serde_json::Value::Array(items)) => {
                let joined = items
                    .iter()
                    .filter_map(|v| v.as_str())
                    .collect::<Vec<_>>()
                    .join("; ");
                if !joined.trim().is_empty() {
                    return joined;
                }
            }
            Some(serde_json::Value::String(text)) if !text.trim().is_empty() => {
                return text.clone();
            }
            _ => {}
        }
    }
    String::new()
}

/// How many tool-bearing turns have finished in this session. Process-local: the review gate is
/// about this run's spend, not a durable ledger.
impl LearningManager {
    pub fn note_turn(&self, session: &SessionId) -> u64 {
        let mut turns = self.turns.lock().expect("turn count lock");
        let count = turns.entry(session.clone()).or_insert(0);
        *count += 1;
        *count
    }
}

/// Whether this turn earns a model-driven review, which spends tokens. It earns one only on a
/// tool-bearing turn at the configured interval, and never when the operator has switched both
/// learning policies off or left the interval at zero.
pub fn review_is_due(turns: u64, had_tools: bool, every: u64, policies_enabled: bool) -> bool {
    policies_enabled && every > 0 && had_tools && turns % every == 0
}

/// The text a review is asked about: the transcript, bounded, oldest first dropped.
pub fn transcript_snapshot(messages: &[vikram_contracts::Message], budget: usize) -> String {
    let mut lines = Vec::new();
    let mut total = 0usize;
    for message in messages.iter().rev() {
        let body = message.text_body();
        if body.trim().is_empty() {
            continue;
        }
        let line = format!("{:?}: {}", message.role, body);
        total += line.len();
        lines.push(line);
        if total > budget {
            break;
        }
    }
    lines.reverse();
    lines.join("\n")
}

pub(crate) fn new_proposal_id() -> String {
    static SEQUENCE: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let seq = SEQUENCE.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    format!("lrn-{}-{seq}", now_ms())
}

/// How much transcript a background review is shown.
pub const REVIEW_SNAPSHOT_BUDGET: usize = 12_000;

/// The one growth pipeline, run when a turn ends.
///
/// The free half runs every turn: what happened is recorded as an experience, reflected on with no
/// model in the loop, and whatever the approval surface can act on goes through the same policy
/// gate a model's proposal goes through. The paid half is a background model review of the
/// transcript, and only when [`review_is_due`] says the operator asked for one. Neither half can
/// grant, widen or reinterpret a permission, and both halves are off when both learning policies
/// are.
pub(crate) fn on_turn_end(
    inner: &Arc<crate::runtime::RuntimeInner>,
    session: &crate::session::SessionRuntime,
    request: &str,
    outcome: crate::types::TurnState,
    growth: &TurnGrowth,
) {
    use crate::types::TurnState;
    if outcome == TurnState::Cancelled {
        return;
    }
    // Both policies off is the operator's switch for the whole feature, not just its output.
    let policies_enabled = !(inner.learning.policy_memory() == LearningPolicy::Disabled
        && inner.learning.policy_skill() == LearningPolicy::Disabled);
    if !policies_enabled {
        return;
    }
    let turns = inner.learning.note_turn(&session.id);
    if growth.had_tools() || !growth.errors.is_empty() || outcome == TurnState::Failed {
        record_and_reflect(inner, session, request, outcome, growth);
    }
    let nudge_due = inner.memory_nudge_due(session);
    if review_is_due(
        turns,
        growth.had_tools(),
        inner.config.learning_review_every,
        policies_enabled,
    ) || (policies_enabled && nudge_due)
    {
        let snapshot = transcript_snapshot(&session.messages(), REVIEW_SNAPSHOT_BUDGET);
        inner.spawn_learning_review(session.id.clone(), snapshot);
    }
}

fn record_and_reflect(
    inner: &Arc<crate::runtime::RuntimeInner>,
    session: &crate::session::SessionRuntime,
    request: &str,
    outcome: crate::types::TurnState,
    growth: &TurnGrowth,
) {
    use crate::types::TurnState;
    let failed = outcome == TurnState::Failed;
    let experience = vikram_evolution::Experience {
        id: String::new(),
        timestamp_ms: 0,
        agent_id: session
            .agent()
            .map(|id| id.to_string())
            .unwrap_or_else(|| "main".to_owned()),
        project_id: session.project_id().map(|id| id.to_string()),
        task_category: growth
            .tools
            .first()
            .cloned()
            .unwrap_or_else(|| "conversation".to_owned()),
        user_request: request.chars().take(2_000).collect(),
        outcome: if failed {
            "failure".to_owned()
        } else if growth.errors.is_empty() {
            "success".to_owned()
        } else {
            "partial".to_owned()
        },
        tools_used: growth.tools.clone(),
        errors: growth.errors.clone(),
        corrections: Vec::new(),
        feedback: None,
        reusable: !failed && growth.errors.is_empty() && growth.tools.len() >= 2,
        sensitivity: "private".to_owned(),
        metadata: serde_json::json!({"session": session.id.as_str()}),
    };
    let proposals = {
        let engine = inner.evolution.lock().expect("evolution lock");
        let Ok(recorded) = engine.experience.record(experience) else {
            return;
        };
        match engine.reflection.run_reflection(&recorded.id) {
            Ok(proposals) => proposals,
            Err(_) => return,
        }
    };
    for proposal in proposals {
        if let Some(mapped) = proposal_from_evolution(&proposal, &session.id) {
            apply_or_stage(inner, mapped);
        }
    }
}

/// One gate for every proposal source. The policy for the proposal's kind decides whether the
/// change happens now, waits for the operator, or is dropped; a kind the surface cannot act on and
/// a disabled policy both drop it rather than offering a decision that does nothing.
pub(crate) fn apply_or_stage(
    inner: &Arc<crate::runtime::RuntimeInner>,
    mut proposal: LearningProposal,
) {
    let scope_kind = proposal.kind.clone();
    let policy = match scope_kind.as_str() {
        "memory" => inner.learning.policy_memory(),
        "skill" => inner.learning.policy_skill(),
        other => {
            audit(
                inner,
                &proposal,
                "skipped",
                &format!("the approval surface cannot apply a {other} proposal"),
            );
            return;
        }
    };
    if policy == LearningPolicy::Disabled {
        audit(inner, &proposal, "skipped", "policy disabled");
        return;
    }
    if proposal.kind == "memory" {
        let scope = scope_of(&proposal.target);
        let existing = match inner.memory.list(scope) {
            Ok(existing) => existing,
            Err(err) => {
                audit(
                    inner,
                    &proposal,
                    "failed",
                    &format!("could not read existing memory: {err}"),
                );
                return;
            }
        };
        let contents: Vec<String> = existing.into_iter().map(|(_, value)| value).collect();
        if inner.learning.dedup_memory(&contents, &proposal.content) {
            audit(inner, &proposal, "skipped", "already known");
            return;
        }
    }
    if policy != LearningPolicy::Auto {
        proposal.status = "pending".to_owned();
        inner.learning.stage_proposal(proposal.clone());
        audit(inner, &proposal, "staged", &proposal.reason.clone());
        inner.learning.save();
        return;
    }
    let outcome = if proposal.kind == "memory" {
        inner
            .memory
            .save(&proposal.key, &proposal.content, scope_of(&proposal.target))
            .map(|_| format!("auto-applied{}", tail(&proposal.reason)))
            .unwrap_or_else(|err| format!("could not save memory: {err}"))
    } else {
        let name = if proposal.target.is_empty() {
            "learned-skill"
        } else {
            &proposal.target
        };
        vikram_skills::Manager::new()
            .create(name, &proposal.reason, &proposal.content)
            .map(|_| format!("auto-applied{}", tail(&proposal.reason)))
            .unwrap_or_else(|err| format!("could not create skill: {err}"))
    };
    let applied = outcome.starts_with("auto-applied");
    audit(
        inner,
        &proposal,
        if applied { "applied" } else { "failed" },
        &outcome,
    );
    inner.learning.save();
}

fn scope_of(target: &str) -> &'static str {
    if target == "user" { "user" } else { "memory" }
}

fn tail(reason: &str) -> String {
    if reason.trim().is_empty() {
        String::new()
    } else {
        format!(": {}", reason.trim())
    }
}

fn audit(
    inner: &Arc<crate::runtime::RuntimeInner>,
    proposal: &LearningProposal,
    outcome: &str,
    detail: &str,
) {
    inner.learning.audit(LearningAuditEntry {
        ts_ms: now_ms(),
        session: proposal.session.clone(),
        kind: proposal.kind.clone(),
        action: proposal.action.clone(),
        target: proposal.target.clone(),
        outcome: outcome.to_owned(),
        detail: detail.to_owned(),
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    fn evolution_proposal(
        kind: &str,
        details: serde_json::Value,
    ) -> vikram_evolution::LearningProposal {
        vikram_evolution::LearningProposal {
            id: "prop-1".to_owned(),
            experience_id: Some("exp-1".to_owned()),
            agent_id: "agent".to_owned(),
            kind: kind.to_owned(),
            summary: "summary".to_owned(),
            details,
            confidence: 0.7,
            requires_approval: true,
            created_ms: 1,
            status: "pending".to_owned(),
        }
    }

    fn session() -> SessionId {
        SessionId::generate()
    }

    #[test]
    fn a_tool_bearing_turn_on_the_interval_earns_a_paid_review() {
        assert!(review_is_due(5, true, 5, true));
    }

    #[test]
    fn a_chat_only_turn_never_earns_a_paid_review() {
        assert!(!review_is_due(5, false, 5, true));
    }

    #[test]
    fn a_zero_interval_means_the_operator_kept_the_review_off() {
        assert!(!review_is_due(5, true, 0, true));
    }

    #[test]
    fn a_review_is_earned_on_the_interval_not_every_turn_after_it() {
        assert!(!review_is_due(6, true, 5, true));
        assert!(review_is_due(10, true, 5, true));
    }

    #[test]
    fn no_paid_review_when_every_learning_policy_is_disabled() {
        assert!(!review_is_due(5, true, 5, false));
    }

    #[test]
    fn growth_names_the_tools_a_turn_used_and_the_ones_that_failed() {
        let mut growth = TurnGrowth::default();
        growth.record_call("fs.read");
        growth.record_call("shell.run");
        growth.record_failure("shell.run", "  exit status 2  ");
        assert_eq!(
            growth.tools,
            vec!["fs.read".to_owned(), "shell.run".to_owned()]
        );
        assert_eq!(growth.errors, vec!["shell.run: exit status 2".to_owned()]);
        assert!(growth.had_tools());
    }

    #[test]
    fn a_turn_that_touched_no_tool_is_not_growth() {
        let growth = TurnGrowth::default();
        assert!(!growth.had_tools());
        assert!(growth.errors.is_empty());
    }

    #[test]
    fn growth_lists_each_tool_once_however_often_the_turn_used_it() {
        let mut growth = TurnGrowth::default();
        growth.record_call("fs.read");
        growth.record_call("fs.read");
        assert_eq!(growth.tools, vec!["fs.read".to_owned()]);
    }

    #[test]
    fn a_long_tool_failure_is_bounded_before_it_is_stored() {
        let mut growth = TurnGrowth::default();
        growth.record_failure("shell.run", &"x".repeat(5_000));
        let errors = growth.errors;
        assert_eq!(errors.len(), 1);
        assert!(
            errors[0].chars().count() <= MAX_GROWTH_ERROR_LEN,
            "a failure log entry is a note, not the whole dump"
        );
    }

    #[test]
    fn a_correction_proposal_lands_on_the_memory_policy() {
        let staged = proposal_from_evolution(
            &evolution_proposal(
                "add_memory",
                serde_json::json!({"layer": "user", "corrections": ["use pnpm, not npm"]}),
            ),
            &session(),
        )
        .expect("a memory proposal has a place on the approval surface");
        assert_eq!(staged.kind, "memory");
        assert_eq!(staged.target, "user");
        assert_eq!(staged.content, "use pnpm, not npm");
    }

    #[test]
    fn a_skill_proposal_lands_on_the_skill_policy() {
        let staged = proposal_from_evolution(
            &evolution_proposal(
                "create_skill",
                serde_json::json!({"tools": ["fs.read", "shell.run"], "category": "audit"}),
            ),
            &session(),
        )
        .expect("a skill proposal has a place on the approval surface");
        assert_eq!(staged.kind, "skill");
        assert_eq!(staged.action, "create");
        assert!(staged.content.contains("fs.read"));
    }

    #[test]
    fn an_issue_proposal_has_no_home_on_the_approval_surface() {
        assert!(
            proposal_from_evolution(
                &evolution_proposal("record_issue", serde_json::json!({"errors": ["boom"]})),
                &session(),
            )
            .is_none(),
            "the approval surface cannot apply an issue record, so it must not offer one"
        );
    }

    #[test]
    fn turn_counting_is_per_session() {
        let mgr = LearningManager::new();
        let a = session();
        let b = session();
        assert_eq!(mgr.note_turn(&a), 1);
        assert_eq!(mgr.note_turn(&a), 2);
        assert_eq!(mgr.note_turn(&b), 1);
    }
}
