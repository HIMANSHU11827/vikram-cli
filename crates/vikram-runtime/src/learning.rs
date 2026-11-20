//! Runtime-level self-improvement: learning review, proposals, approval, audit.
//!
//! Inspired by Hermes Agent's background review: after a meaningful turn, a background task
//! evaluates the conversation snapshot and proposes memory/skill changes. Changes are either
//! applied (auto mode) or staged for approval. Nothing modifies the live conversation or its
//! prompt cache. Learning never grants permissions.
//!
//! # Single source of truth (unified learning)
//!
//! There is exactly one durable learning store: `vikram-evolution`'s SQLite
//! [`vikram_evolution::EvolutionStore`] (`evolution.db`). This module's
//! [`LearningManager`] (JSON `learning.json`) is **only the approval
//! surface** — the operator-facing queue of staged proposals, their
//! policies, and the audit log. It intentionally does *not* keep a second
//! copy of experiences, layered memories, reflection jobs, candidates, or
//! hive lessons: `on_turn_end` / `record_and_reflect` delegate all of
//! that to `vikram-evolution`, and `proposal_from_evolution` maps
//! evolution proposals onto the surface. Migrating the surface away
//! entirely would require the UI to read SQLite directly; until then,
//! this split is documented and enforced: JSON → approvals + audit only,
//! SQLite → everything durable.
//!
//! The evolution engine is invoked on boot (recovery + drain, see
//! `Runtime::boot`) and on every tool-bearing turn finish
//! (`on_turn_end`, called from `turn::finish`), so
//! `RuntimeHandle::evolution()` is no longer dead — the turn-end path
//! locks `RuntimeInner::evolution` on every qualifying turn.

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
    #[serde(default = "default_policy_str")]
    pub policy_instruction: String,
    #[serde(default = "default_policy_str")]
    pub policy_identity: String,
}

fn default_policy_str() -> String {
    "approval".to_owned()
}

/// Recover a poisoned lock instead of panicking: one panic while a mutex is held must not take
/// down every later call into the manager.
trait RecoverPoisoned<T> {
    fn recover(self) -> T;
}

impl<T> RecoverPoisoned<T> for std::sync::LockResult<T> {
    fn recover(self) -> T {
        self.unwrap_or_else(|e| {
            eprintln!("vikram: learning mutex poisoned; recovering");
            e.into_inner()
        })
    }
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
    /// Non-empty when the state file could not be read/parsed, so the failure is visible to
    /// callers that check `load_error` instead of silently starting from scratch.
    load_error: Mutex<Option<String>>,
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
            load_error: Mutex::new(None),
        };
        if let Some(p) = mgr.path.lock().recover().clone() {
            match std::fs::read_to_string(&p) {
                Ok(data) => match serde_json::from_str::<LearningState>(&data) {
                    Ok(state) => {
                        *mgr.proposals.lock().recover() = state.proposals;
                        *mgr.audit.lock().recover() = state.audit;
                        *mgr.policy_memory.lock().recover() =
                            LearningPolicy::parse(&state.policy_memory);
                        *mgr.policy_skill.lock().recover() =
                            LearningPolicy::parse(&state.policy_skill);
                        *mgr.policy_instruction.lock().recover() =
                            LearningPolicy::parse(&state.policy_instruction);
                        *mgr.policy_identity.lock().recover() =
                            LearningPolicy::parse(&state.policy_identity);
                    }
                    Err(e) => {
                        *mgr.load_error.lock().recover() = Some(format!(
                            "learning state at {} is corrupt; starting empty: {e}",
                            p.display()
                        ));
                        eprintln!(
                            "vikram: {}",
                            mgr.load_error.lock().recover().as_deref().unwrap_or("")
                        );
                    }
                },
                Err(e) if e.kind() != std::io::ErrorKind::NotFound => {
                    *mgr.load_error.lock().recover() =
                        Some(format!("learning state at {} unreadable: {e}", p.display()));
                    eprintln!(
                        "vikram: {}",
                        mgr.load_error.lock().recover().as_deref().unwrap_or("")
                    );
                }
                Err(_) => {}
            }
        }
        mgr
    }

    pub fn save(&self) {
        if let Err(e) = self.try_save() {
            eprintln!("vikram: learning state save failed: {e}");
        }
    }

    pub fn try_save(&self) -> Result<(), String> {
        let path = self.path.lock().recover().clone();
        let Some(path) = path else {
            return Ok(());
        };
        let state = LearningState {
            proposals: self.proposals.lock().recover().clone(),
            audit: self.audit.lock().recover().clone(),
            policy_memory: self.policy_memory.lock().recover().as_str().to_owned(),
            policy_skill: self.policy_skill.lock().recover().as_str().to_owned(),
            policy_instruction: self.policy_instruction.lock().recover().as_str().to_owned(),
            policy_identity: self.policy_identity.lock().recover().as_str().to_owned(),
        };
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).map_err(|e| e.to_string())?;
        }
        let json = serde_json::to_string_pretty(&state).map_err(|e| e.to_string())?;
        std::fs::write(&path, json).map_err(|e| e.to_string())
    }

    pub fn load_error(&self) -> Option<String> {
        self.load_error.lock().recover().clone()
    }

    pub fn policy_instruction(&self) -> LearningPolicy {
        *self.policy_instruction.lock().recover()
    }
    pub fn policy_identity(&self) -> LearningPolicy {
        *self.policy_identity.lock().recover()
    }

    pub fn path(&self) -> Option<std::path::PathBuf> {
        self.path.lock().recover().clone()
    }

    pub fn policy_memory(&self) -> LearningPolicy {
        *self.policy_memory.lock().recover()
    }
    pub fn policy_skill(&self) -> LearningPolicy {
        *self.policy_skill.lock().recover()
    }
    pub fn set_policy_memory(&self, p: LearningPolicy) {
        *self.policy_memory.lock().recover() = p;
    }
    pub fn set_policy_skill(&self, p: LearningPolicy) {
        *self.policy_skill.lock().recover() = p;
    }
    pub fn set_policy_instruction(&self, p: LearningPolicy) {
        *self.policy_instruction.lock().recover() = p;
    }
    pub fn set_policy_identity(&self, p: LearningPolicy) {
        *self.policy_identity.lock().recover() = p;
    }

    /// Record that a skill was viewed in this session (read-before-write guard).
    pub fn mark_skill_viewed(&self, session: &SessionId, name: &str) {
        self.viewed_skills
            .lock()
            .recover()
            .entry(session.clone())
            .or_default()
            .insert(name.to_owned());
    }

    pub fn skill_was_viewed(&self, session: &SessionId, name: &str) -> bool {
        self.viewed_skills
            .lock()
            .recover()
            .get(session)
            .map(|s| s.contains(name))
            .unwrap_or(false)
    }

    pub fn stage_proposal(&self, proposal: LearningProposal) {
        self.proposals.lock().recover().push(proposal);
    }

    pub fn pending_proposals(&self) -> Vec<LearningProposal> {
        self.proposals
            .lock()
            .recover()
            .iter()
            .filter(|p| p.status == "pending")
            .cloned()
            .collect()
    }

    pub fn all_proposals(&self) -> Vec<LearningProposal> {
        self.proposals.lock().recover().clone()
    }

    pub fn find_proposal(&self, id: &str) -> Option<LearningProposal> {
        self.proposals
            .lock()
            .recover()
            .iter()
            .find(|p| p.id == id)
            .cloned()
    }

    pub fn set_proposal_status(&self, id: &str, status: &str) {
        if let Some(p) = self
            .proposals
            .lock()
            .recover()
            .iter_mut()
            .find(|p| p.id == id)
        {
            p.status = status.to_owned();
        }
    }

    pub fn audit(&self, entry: LearningAuditEntry) {
        self.audit.lock().recover().push(entry);
    }

    pub fn audit_log(&self) -> Vec<LearningAuditEntry> {
        self.audit.lock().recover().clone()
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
    let (kind, action) = match proposal.kind {
        vikram_evolution::ProposalKind::AddMemory => ("memory", "add"),
        vikram_evolution::ProposalKind::UpdateMemory => ("memory", "replace"),
        vikram_evolution::ProposalKind::CreateSkill => ("skill", "create"),
        vikram_evolution::ProposalKind::ImproveSkill => ("skill", "patch"),
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
        let mut turns = self.turns.lock().recover();
        let count = turns.entry(session.clone()).or_insert(0);
        *count += 1;
        *count
    }
}

/// Whether this turn earns a model-driven review, which spends tokens. It earns one only on a
/// tool-bearing turn at the configured interval, and never when the operator has switched both
/// learning policies off or left the interval at zero.
pub fn review_is_due(turns: u64, had_tools: bool, every: u64, policies_enabled: bool) -> bool {
    policies_enabled && every > 0 && had_tools && turns.is_multiple_of(every)
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
    let policies_enabled = !(inner.learning.surface.policy_memory() == LearningPolicy::Disabled
        && inner.learning.surface.policy_skill() == LearningPolicy::Disabled);
    if !policies_enabled {
        return;
    }
    let turns = inner.learning.surface.note_turn(&session.id);
    let corrections = detect_corrections(request);
    if growth.had_tools()
        || !growth.errors.is_empty()
        || outcome == TurnState::Failed
        || !corrections.is_empty()
    {
        record_and_reflect(inner, session, request, outcome, growth, &corrections);
    }
    let nudge_due = inner.memory_nudge_due(session);
    if review_is_due(
        turns,
        growth.had_tools(),
        inner
            .config
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .learning_review_every,
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
    corrections: &[String],
) {
    use crate::types::TurnState;
    let failed = outcome == TurnState::Failed;
    let experience = vikram_evolution::Experience {
        id: String::new(),
        timestamp_ms: 0,
        agent_id: session
            .bot()
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
            vikram_evolution::ExperienceOutcome::Failure
        } else if growth.errors.is_empty() {
            vikram_evolution::ExperienceOutcome::Success
        } else {
            vikram_evolution::ExperienceOutcome::Partial
        },
        tools_used: growth.tools.clone(),
        errors: growth.errors.clone(),
        corrections: corrections.to_vec(),
        feedback: None,
        reusable: !failed && growth.errors.is_empty() && growth.tools.len() >= 2,
        sensitivity: vikram_evolution::Sensitivity::Private,
        metadata: serde_json::json!({"session": session.id.as_str()}),
    };
    let proposals = {
        let engine = inner.learning.evolution.lock().recover();
        let Ok(recorded) = engine.experience.record(experience) else {
            return;
        };
        let auto_memory = inner.learning.surface.policy_memory() != LearningPolicy::Disabled;
        match engine.reflection.run_reflection(&recorded.id, auto_memory) {
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
        "memory" => inner.learning.surface.policy_memory(),
        "skill" => inner.learning.surface.policy_skill(),
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
        let scope = scope_of(&proposal.target, &proposal.session);
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
        if inner
            .learning
            .surface
            .dedup_memory(&contents, &proposal.content)
        {
            audit(inner, &proposal, "skipped", "already known");
            return;
        }
    }
    if policy != LearningPolicy::Auto {
        proposal.status = "pending".to_owned();
        inner.learning.surface.stage_proposal(proposal.clone());
        audit(inner, &proposal, "staged", &proposal.reason.clone());
        inner.learning.surface.save();
        return;
    }
    let outcome = if proposal.kind == "memory" {
        inner
            .memory
            .save(
                &proposal.key,
                &proposal.content,
                scope_of(&proposal.target, &proposal.session),
            )
            .map(|_| format!("auto-applied{}", tail(&proposal.reason)))
            .unwrap_or_else(|err| format!("could not save memory: {err}"))
    } else {
        apply_skill_proposal(&proposal)
            .map(|_| format!("auto-applied{}", tail(&proposal.reason)))
            .unwrap_or_else(|err| format!("could not apply skill: {err}"))
    };
    let applied = outcome.starts_with("auto-applied");
    audit(
        inner,
        &proposal,
        if applied { "applied" } else { "failed" },
        &outcome,
    );
    inner.learning.surface.save();
}

/// Memory writes land in the session's own scope: automatic recall only reads `global` +
/// the session id, and the shared store has no user dimension — a literal `user`/`memory`
/// scope would be both invisible to recall and readable by every other session.
pub fn scope_of(target: &str, session: &SessionId) -> String {
    let _ = target;
    session.as_str().to_owned()
}

/// Applies one staged skill proposal through the skills [`vikram_skills::Manager`], honoring
/// `action`: "patch"/"improve"/"replace" rewrite an existing skill's body while keeping its
/// frontmatter (a patch naming a skill that does not exist yet falls back to create, since
/// the proposal content is still the best available draft); every other action creates.
/// The manager's error is returned verbatim so callers can audit exactly what refused.
pub fn apply_skill_proposal(proposal: &LearningProposal) -> Result<(), String> {
    apply_skill_with(&vikram_skills::Manager::new(), proposal)
}

fn apply_skill_with(
    manager: &vikram_skills::Manager,
    proposal: &LearningProposal,
) -> Result<(), String> {
    let name = {
        let target = proposal.target.trim();
        if target.is_empty() {
            "learned-skill"
        } else {
            target
        }
    };
    let create = || {
        let reason = proposal.reason.trim();
        let description = if reason.is_empty() {
            name.to_owned()
        } else {
            reason
                .chars()
                .take(vikram_skills::DESCRIPTION_MAX)
                .collect::<String>()
        };
        manager
            .create(name, &description, &proposal.content)
            .map(|_| ())
            .map_err(|err| err.to_string())
    };
    match proposal.action.as_str() {
        "patch" | "improve" | "replace" => match manager.edit(name, &proposal.content) {
            Ok(()) => Ok(()),
            Err(vikram_skills::SkillError::NotFound { .. }) => create(),
            Err(err) => Err(err.to_string()),
        },
        _ => create(),
    }
}

/// How much of a correcting user message is kept as a correction note: the note points at
/// what the user said, it is not a transcript copy.
pub const MAX_CORRECTION_LEN: usize = 500;

/// Phrases that only appear when the user is pushing back on what just happened. The list is
/// deliberately narrow — a false positive records an ordinary request as a correction, and
/// under an auto policy that writes it into memory unsupervised.
const CORRECTION_STARTS: &[&str] = &[
    "no,",
    "no.",
    "nope",
    "wrong,",
    "wrong.",
    "incorrect",
    "not quite",
    "that's wrong",
    "thats wrong",
    "actually,",
    "actually ",
    "i meant",
    "you should have",
];

const CORRECTION_ANYWHERE: &[&str] = &[
    ", not ",
    " instead",
    " rather than",
    " should have ",
    " you should have ",
    " i meant ",
];

/// Correction detection for the growth pipeline. The turn's user request is the only signal
/// available without touching the turn loop: when it reads as a correction, the bounded text
/// itself becomes the note. This is a heuristic, not a classifier — it misses corrections
/// phrased without these markers and can fire on a request that merely mentions "instead".
/// Both failure modes are why the note goes through the same approval policy as every other
/// proposal rather than being written directly.
pub(crate) fn detect_corrections(request: &str) -> Vec<String> {
    let text = request.trim();
    if text.chars().count() < 8 {
        return Vec::new();
    }
    let lower = text.to_lowercase();
    let hit = CORRECTION_STARTS.iter().any(|s| lower.starts_with(s))
        || CORRECTION_ANYWHERE.iter().any(|s| lower.contains(s));
    if !hit {
        return Vec::new();
    }
    vec![text.chars().take(MAX_CORRECTION_LEN).collect()]
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
    inner.learning.surface.audit(LearningAuditEntry {
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
        kind: vikram_evolution::ProposalKind,
        details: serde_json::Value,
    ) -> vikram_evolution::LearningProposal {
        vikram_evolution::LearningProposal {
            id: "prop-1".to_owned(),
            experience_id: Some("exp-1".to_owned()),
            agent_id: "agent".to_owned(),
            kind,
            summary: "summary".to_owned(),
            details,
            confidence: 0.7,
            requires_approval: true,
            created_ms: 1,
            status: vikram_evolution::ProposalStatus::Pending,
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
                vikram_evolution::ProposalKind::AddMemory,
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
                vikram_evolution::ProposalKind::CreateSkill,
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
                &evolution_proposal(
                    vikram_evolution::ProposalKind::RecordIssue,
                    serde_json::json!({"errors": ["boom"]})
                ),
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

    fn surface_proposal(kind: &str, action: &str, target: &str) -> LearningProposal {
        LearningProposal {
            id: "lrn-test".to_owned(),
            session: session(),
            kind: kind.to_owned(),
            target: target.to_owned(),
            action: action.to_owned(),
            key: "k".to_owned(),
            content: "body content".to_owned(),
            reason: "because".to_owned(),
            status: "pending".to_owned(),
            created_ms: 1,
            before: None,
            after: None,
        }
    }

    #[test]
    fn correction_detection_fires_on_real_pushback() {
        for text in [
            "no, use pnpm not npm",
            "No. the config lives in src/",
            "actually, the endpoint is /v2",
            "that's wrong — it should have used the cache",
            "run the build instead",
            "I meant the other file, not this one",
        ] {
            let found = detect_corrections(text);
            assert_eq!(found.len(), 1, "{text:?} should read as a correction");
            assert_eq!(found[0], text.trim());
        }
    }

    #[test]
    fn correction_detection_ignores_ordinary_requests() {
        for text in [
            "please refactor the parser",
            "add a test for the login flow",
            "no",
            "what does this function do?",
            "the build finished successfully",
        ] {
            assert!(
                detect_corrections(text).is_empty(),
                "{text:?} must not be recorded as a correction"
            );
        }
    }

    #[test]
    fn correction_notes_are_bounded() {
        let long = format!("no, {}", "x".repeat(5_000));
        let found = detect_corrections(&long);
        assert_eq!(found.len(), 1);
        assert!(found[0].chars().count() <= MAX_CORRECTION_LEN);
    }

    #[test]
    fn approved_memory_scope_is_one_recall_reads() {
        let s = session();
        let scope = scope_of("user", &s);
        assert_eq!(scope, s.as_str());
        assert_ne!(scope, "user");
        assert_ne!(scope, "memory");
        // Recall reads exactly `global` + the session id (runtime.rs retrieved_memory).
        let scopes = ["global".to_owned(), s.as_str().to_owned()];
        assert!(scopes.contains(&scope));
    }

    #[test]
    fn skill_patch_updates_the_existing_skill_body() {
        let tmp = tempfile::tempdir().unwrap();
        let manager = vikram_skills::Manager::at(tmp.path().join("skills"));
        manager
            .create("audit-flow", "audit desc", "old body")
            .unwrap();

        let mut proposal = surface_proposal("skill", "patch", "audit-flow");
        proposal.content = "new body".to_owned();
        apply_skill_with(&manager, &proposal).unwrap();

        let doc = std::fs::read_to_string(tmp.path().join("skills/audit-flow/SKILL.md")).unwrap();
        assert!(doc.contains("new body"));
        assert!(!doc.contains("old body"));
        // edit keeps the frontmatter the create wrote.
        assert!(doc.contains("description: audit desc"));
        assert!(doc.contains("name: audit-flow"));
    }

    #[test]
    fn skill_patch_for_a_missing_skill_creates_it() {
        let tmp = tempfile::tempdir().unwrap();
        let manager = vikram_skills::Manager::at(tmp.path().join("skills"));
        let proposal = surface_proposal("skill", "improve", "learned-workflow");
        apply_skill_with(&manager, &proposal).unwrap();
        assert!(
            tmp.path()
                .join("skills/learned-workflow/SKILL.md")
                .is_file()
        );
    }

    #[test]
    fn skill_create_still_creates() {
        let tmp = tempfile::tempdir().unwrap();
        let manager = vikram_skills::Manager::at(tmp.path().join("skills"));
        let proposal = surface_proposal("skill", "create", "fresh-skill");
        apply_skill_with(&manager, &proposal).unwrap();
        assert!(tmp.path().join("skills/fresh-skill/SKILL.md").is_file());
        // A second create of the same name is refused, not silently overwritten.
        assert!(apply_skill_with(&manager, &proposal).is_err());
    }

    #[test]
    fn a_poisoned_lock_recovers_instead_of_panicking() {
        let mgr = LearningManager::new();
        let poisoned = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let _guard = mgr.proposals.lock().unwrap();
            panic!("poison the proposals mutex");
        }));
        assert!(poisoned.is_err());
        assert!(mgr.pending_proposals().is_empty());
        assert!(mgr.all_proposals().is_empty());
    }
}
