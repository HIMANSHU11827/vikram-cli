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
    pub status: String, // "pending" | "approved" | "rejected" | "applied" | "failed"
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

/// What the `verify` tool concluded when it ran during a turn. Verification is stronger
/// evidence than tool count alone: a green build+test means the workflow produced a working
/// artifact, a red one means it did not — even when every individual tool call "succeeded".
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Verification {
    Passed,
    Failed {
        step: Option<String>,
        kind: Option<String>,
        /// Bounded tail of the failing step's real output, when the verify report carried one.
        detail: Option<String>,
    },
}

/// What one turn did, as the growth pipeline sees it: which tools it reached for and which of
/// them came back failing. The turn loop feeds it, so a turn that never ran a tool produces an
/// empty record rather than a guess.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct TurnGrowth {
    pub tools: Vec<String>,
    pub errors: Vec<String>,
    /// The `verify` tool's verdict this turn; `None` when verification never ran.
    pub verification: Option<Verification>,
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

    /// The `verify` tool finished. Its output leads with a `verify: PASS` / `verify: FAIL
    /// step=… kind=…` verdict line (see `tools::verify`), so the verdict survives truncation and
    /// spooling; content without that line falls back to the JSON report's `ok`, then to the
    /// call's own `ok`. A failure also lands in `errors` as a bounded note, so reflection sees
    /// "the work finished but did not verify" distinctly from a tool error. The recorded verdict
    /// is `ok && reported`: a plugin that flips the call's `ok` cannot make a failed verification
    /// pass, and a withheld pass is still a failure the turn saw.
    pub fn record_verification(&mut self, ok: bool, content: &str) {
        let (reported, step, kind, detail) = verification_signal(content);
        let passed = ok && reported.unwrap_or(true);
        let detail = detail
            .map(|d| bounded_tail(&d, MAX_VERIFICATION_DETAIL))
            .filter(|d| !d.is_empty());
        let location = match (&step, &kind) {
            (Some(step), Some(kind)) => format!(" {step}:{kind}"),
            (Some(step), None) => format!(" {step}"),
            (None, Some(kind)) => format!(" {kind}"),
            (None, None) => String::new(),
        };
        self.verification = Some(if passed {
            Verification::Passed
        } else {
            Verification::Failed {
                step,
                kind,
                detail: detail.clone(),
            }
        });
        if !passed {
            if !location.is_empty() || reported == Some(false) {
                let mut note = format!("verify: FAIL{location}");
                if let Some(detail) = &detail {
                    let flat = detail.split_whitespace().collect::<Vec<_>>().join(" ");
                    let room = MAX_GROWTH_ERROR_LEN.saturating_sub(note.chars().count() + 3);
                    if room > 0 && !flat.is_empty() {
                        note.push_str(" — ");
                        note.push_str(&bounded_tail(&flat, room));
                    }
                }
                if note.chars().count() > MAX_GROWTH_ERROR_LEN {
                    note = note.chars().take(MAX_GROWTH_ERROR_LEN).collect();
                }
                self.errors.push(note);
            } else {
                self.record_failure("verify", content);
            }
        }
    }

    pub fn had_tools(&self) -> bool {
        !self.tools.is_empty()
    }
}

/// How much of the failing step's output a growth record keeps. The tail is what matters:
/// the last lines of a compiler or test run carry the error.
pub const MAX_VERIFICATION_DETAIL: usize = 400;

/// The last `max` chars of `text`, trimmed — the end of a check's output is where the
/// failure is reported.
fn bounded_tail(text: &str, max: usize) -> String {
    let trimmed = text.trim();
    let chars: Vec<char> = trimmed.chars().collect();
    if chars.len() <= max {
        trimmed.to_owned()
    } else {
        chars[chars.len() - max..].iter().collect()
    }
}

/// Read the `verify` tool's verdict out of its output: the leading verdict line first, then the
/// JSON report's `ok`/`failure`/`failure_output` fields, else `None` (caller falls back to the
/// call's `ok`). The report may be truncated mid-JSON, so `failure_output` is also recovered by
/// a tolerant string-field scan when a full parse fails.
fn verification_signal(
    content: &str,
) -> (Option<bool>, Option<String>, Option<String>, Option<String>) {
    let verdict = content.lines().next().unwrap_or_default();
    if verdict == "verify: PASS" {
        return (Some(true), None, None, None);
    }
    if let Some(detail) = verdict.strip_prefix("verify: FAIL") {
        return (
            Some(false),
            verdict_field(detail, "step"),
            verdict_field(detail, "kind"),
            json_string_field(content, "failure_output"),
        );
    }
    if let Ok(value) = serde_json::from_str::<serde_json::Value>(content)
        && let Some(reported) = value.get("ok").and_then(serde_json::Value::as_bool)
    {
        let (step, kind) = value
            .get("failure")
            .filter(|f| !f.is_null())
            .map(|f| {
                (
                    f.get("step")
                        .and_then(serde_json::Value::as_str)
                        .map(str::to_owned),
                    f.get("kind")
                        .and_then(serde_json::Value::as_str)
                        .map(str::to_owned),
                )
            })
            .unwrap_or((None, None));
        let output = value
            .get("failure_output")
            .and_then(serde_json::Value::as_str)
            .map(str::to_owned);
        return (Some(reported), step, kind, output);
    }
    (None, None, None, None)
}

/// Pull a `"key": "..."` string field out of possibly-truncated JSON: locate the key, then scan
/// the string literal honouring backslash escapes. Returns `None` when the field is absent or
/// the literal is unterminated and unparseable.
fn json_string_field(content: &str, key: &str) -> Option<String> {
    let needle = format!("\"{key}\"");
    let start = content.find(&needle)? + needle.len();
    let rest = content[start..]
        .trim_start()
        .strip_prefix(':')?
        .trim_start();
    let rest = rest.strip_prefix('"')?;
    let mut raw = String::new();
    let mut escaped = false;
    let mut closed = false;
    for c in rest.chars() {
        if escaped {
            raw.push('\\');
            raw.push(c);
            escaped = false;
            continue;
        }
        match c {
            '\\' => escaped = true,
            '"' => {
                closed = true;
                break;
            }
            _ => raw.push(c),
        }
    }
    // A trailing `\` with no following char was a truncation artifact; it was never pushed.
    let candidate = format!("\"{raw}\"");
    if closed {
        serde_json::from_str::<String>(&candidate).ok()
    } else {
        // Truncated mid-string: unescape what survived rather than drop the signal entirely.
        serde_json::from_str::<String>(&candidate).ok().or_else(|| {
            Some(
                raw.replace("\\n", "\n")
                    .replace("\\t", "\t")
                    .replace("\\\"", "\""),
            )
        })
    }
}

/// Pull `key=value` out of the `verify: FAIL …` detail text.
fn verdict_field(detail: &str, key: &str) -> Option<String> {
    detail
        .split_whitespace()
        .filter_map(|token| token.split_once('='))
        .find(|(k, _)| *k == key)
        .map(|(_, v)| v.to_owned())
        .filter(|v| !v.is_empty())
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
            match layer {
                "user" => "user".to_owned(),
                "project" => "project".to_owned(),
                _ => "memory".to_owned(),
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
    // Keep the session's shared-scope binding current so recall sees `user:`/`project:`
    // shelves even when both learning policies are off — the binding is recall plumbing,
    // not a learning write.
    bind_session_memory_scopes(inner, session);
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
        outcome: experience_outcome(outcome, growth),
        tools_used: growth.tools.clone(),
        errors: growth.errors.clone(),
        corrections: corrections.to_vec(),
        feedback: None,
        reusable: experience_reusable(growth, outcome),
        sensitivity: vikram_evolution::Sensitivity::Private,
        metadata: experience_metadata(&session.id, growth, request, corrections),
    };
    let (proposals, repeated_failure) = {
        let engine = inner.learning.evolution.lock().recover();
        let Ok(recorded) = engine.experience.record(experience) else {
            return;
        };
        let auto_memory = inner.learning.surface.policy_memory() != LearningPolicy::Disabled;
        let proposals = match engine.reflection.run_reflection(&recorded.id, auto_memory) {
            Ok(proposals) => proposals,
            Err(_) => return,
        };
        // The signature is read back from the *stored* metadata, so it is compared against
        // past rows in exactly the form they were sanitized into.
        let repeated = metadata_verify_signature(&recorded.metadata)
            .map(|signature| {
                (
                    signature.clone(),
                    count_verify_failures(&engine, &recorded, &signature),
                )
            })
            .filter(|(_, count)| *count >= REPEATED_VERIFY_FAILURES);
        (proposals, repeated)
    };
    for proposal in proposals {
        if let Some(mapped) = proposal_from_evolution(&proposal, &session.id) {
            apply_or_stage(inner, mapped);
        }
    }
    if let Some((signature, count)) = repeated_failure {
        stage_repeated_verify_failure(inner, session, growth, &signature, count);
    }
}

/// How many recent experiences sharing one failed-verification signature make it a recurring
/// pattern worth surfacing, rather than a transient the turn's `RecordIssue` already noted.
const REPEATED_VERIFY_FAILURES: usize = 3;
/// How far back the repetition scan looks, in the agent's most recent experiences.
const VERIFY_FAILURE_SCAN: usize = 25;
/// Longest step/kind fragment kept in a signature; longer names are truncated, not dropped.
const VERIFY_SIGNATURE_PART_MAX: usize = 40;

/// The stable name of a failed-verification signature: which check failed and how. Step and
/// kind are bounded and whitespace-free so a verbose or hostile verifier cannot grow the
/// memory key or the proposal content unboundedly.
fn verify_signature_parts(step: Option<&str>, kind: Option<&str>) -> String {
    let part = |value: Option<&str>| {
        value
            .map(|v| {
                v.split_whitespace()
                    .collect::<Vec<_>>()
                    .join("_")
                    .chars()
                    .take(VERIFY_SIGNATURE_PART_MAX)
                    .collect::<String>()
            })
            .filter(|v| !v.is_empty())
            .unwrap_or_else(|| "-".to_owned())
    };
    format!("verify:{}:{}", part(step), part(kind))
}

/// The failed-verification signature an experience carries in its metadata, or `None` when
/// verification never ran or passed. This is the same shape `experience_metadata` writes.
fn metadata_verify_signature(metadata: &serde_json::Value) -> Option<String> {
    let report = metadata.get("verification")?;
    if report.get("passed").and_then(serde_json::Value::as_bool) != Some(false) {
        return None;
    }
    Some(verify_signature_parts(
        report.get("step").and_then(serde_json::Value::as_str),
        report.get("kind").and_then(serde_json::Value::as_str),
    ))
}

/// How many of the agent's recent experiences failed verification with the same signature.
/// Scoped to the recorded experience's project: the same check failing in two different
/// projects is two problems, not one pattern. The just-recorded experience is included in
/// the count, so `REPEATED_VERIFY_FAILURES` means "this is at least the Nth occurrence".
fn count_verify_failures(
    engine: &vikram_evolution::EvolutionEngine,
    recorded: &vikram_evolution::Experience,
    signature: &str,
) -> usize {
    engine
        .experience
        .list(Some(recorded.agent_id.as_str()), VERIFY_FAILURE_SCAN)
        .map(|experiences| {
            experiences
                .iter()
                .filter(|exp| {
                    exp.project_id == recorded.project_id
                        && metadata_verify_signature(&exp.metadata).as_deref() == Some(signature)
                })
                .count()
        })
        .unwrap_or(0)
}

/// A verification signature that keeps failing is project knowledge worth recalling — but
/// staged once: the content is deliberately count-free so memory dedup collapses repeats,
/// and the pending-proposal guard keeps an unapproved first proposal from being re-staged
/// every turn. The count and the latest output tail live in `reason`, which is shown to the
/// operator but never deduplicated against.
fn stage_repeated_verify_failure(
    inner: &Arc<crate::runtime::RuntimeInner>,
    session: &crate::session::SessionRuntime,
    growth: &TurnGrowth,
    signature: &str,
    count: usize,
) {
    if inner.learning.surface.policy_memory() == LearningPolicy::Disabled {
        return;
    }
    let key = format!("verify-failure-{signature}");
    let already_pending = inner
        .learning
        .surface
        .all_proposals()
        .iter()
        .any(|p| p.key == key && p.status == "pending");
    if already_pending {
        return;
    }
    let content = format!(
        "Verification keeps failing the same way: `{signature}`. \
         Check the project's verify report before changing the code that step covers."
    );
    let detail = match &growth.verification {
        Some(Verification::Failed {
            detail: Some(detail),
            ..
        }) => {
            let flat = detail.split_whitespace().collect::<Vec<_>>().join(" ");
            format!(" Latest output: {}", bounded_tail(&flat, 200))
        }
        _ => String::new(),
    };
    let proposal = LearningProposal {
        id: new_proposal_id(),
        session: session.id.clone(),
        kind: "memory".to_owned(),
        target: "project".to_owned(),
        action: "add".to_owned(),
        key,
        content: content.clone(),
        reason: format!("same verification failure seen {count} times recently.{detail}"),
        status: "pending".to_owned(),
        created_ms: now_ms(),
        before: None,
        after: Some(content),
    };
    apply_or_stage(inner, proposal);
}

// `Experience` has no dedicated verification field and the evolution crate is not ours to
// extend, so the verdict rides in `metadata` — free-form JSON the store already round-trips.
// The same channel carries config-shaped signals (`config.model`, `config.context_window`,
// ...) when the user's own words steer a presettable field, so `vikram presets suggest` has
// a real producer instead of scanning for keys nothing writes.
fn experience_metadata(
    session: &SessionId,
    growth: &TurnGrowth,
    request: &str,
    corrections: &[String],
) -> serde_json::Value {
    let mut metadata = serde_json::json!({"session": session.as_str()});
    if let Some(verification) = &growth.verification {
        metadata["verification"] = match verification {
            Verification::Passed => serde_json::json!({"ran": true, "passed": true}),
            Verification::Failed { step, kind, detail } => {
                let mut report = serde_json::json!({
                    "ran": true,
                    "passed": false,
                    "step": step,
                    "kind": kind,
                });
                if let Some(detail) = detail {
                    report["detail"] = serde_json::json!(detail);
                }
                report
            }
        };
    }
    let mut texts = Vec::with_capacity(corrections.len() + 1);
    texts.push(request.to_owned());
    texts.extend(corrections.iter().cloned());
    let signals = config_signals(&texts);
    if !signals.is_empty() {
        metadata["config"] = serde_json::Value::Object(signals);
    }
    metadata
}

/// Model-name prefixes a user instruction can point at. A token only counts when it also
/// carries a digit (or a `/`-qualified suffix that does), so "use the cache" or "run
/// node-20" never reads as a model.
const MODEL_PREFIXES: &[&str] = &[
    "gpt", "claude", "gemini", "llama", "mistral", "qwen", "deepseek", "grok", "phi", "command",
    "o1", "o3", "o4",
];

/// Words that may sit between an intent verb and the model name: "switch to gpt-4o",
/// "set the model to claude-sonnet-4".
const MODEL_INTENT: &[&str] = &[
    "use",
    "using",
    "switch",
    "switched",
    "switching",
    "try",
    "tried",
    "run",
    "call",
    "with",
    "on",
];

/// Negations that void a model signal. `ADJACENT` covers "not gpt-4o"; `BEFORE_VERB`
/// covers "don't use gpt-4o" / "do not use gpt-4o" — but deliberately not bare "no",
/// which is also the correction marker in "no, use gpt-4o".
const NEGATION_ADJACENT: &[&str] = &["not", "no", "never", "none", "cannot", "cant"];
const NEGATION_BEFORE_VERB: &[&str] = &[
    "not",
    "never",
    "don't",
    "dont",
    "doesn't",
    "doesnt",
    "didn't",
    "didnt",
    "shouldn't",
    "shouldnt",
    "mustn't",
    "mustnt",
    "won't",
    "wont",
    "cannot",
    "cant",
];

/// Provider names recognized when a correction says "<provider> provider" or names one
/// after the word "provider".
const PROVIDER_NAMES: &[&str] = &[
    "openai",
    "anthropic",
    "openrouter",
    "ollama",
    "groq",
    "together",
    "fireworks",
    "azure",
    "bedrock",
    "vertex",
    "xai",
    "google",
    "lmstudio",
    "local",
    "deepseek",
    "mistral",
    "perplexity",
    "cohere",
];

/// Filler allowed between a config keyword and its value: "model is gpt-4o",
/// "provider should be openrouter".
const VALUE_FILLER: &[&str] = &[
    "to", "is", "be", "the", "a", "an", "as", "for", "should", "use", "using", "model",
];

/// Words that may sit between "context" and a size: "context window of about 200k".
const CONTEXT_LINK: &[&str] = &[
    "window",
    "windows",
    "size",
    "of",
    "to",
    "the",
    "a",
    "an",
    "is",
    "be",
    "at",
    "around",
    "about",
    "roughly",
    "approximately",
    "approx",
    "token",
    "tokens",
    "ctx",
];

/// Words that can never be a config value, so "provider is down" stays silent.
const VALUE_STOPWORDS: &[&str] = &[
    "the",
    "a",
    "an",
    "to",
    "and",
    "or",
    "not",
    "no",
    "is",
    "was",
    "were",
    "be",
    "been",
    "use",
    "using",
    "used",
    "with",
    "for",
    "in",
    "on",
    "of",
    "that",
    "this",
    "it",
    "as",
    "at",
    "by",
    "from",
    "into",
    "please",
    "should",
    "would",
    "could",
    "will",
    "can",
    "do",
    "does",
    "did",
    "have",
    "has",
    "had",
    "if",
    "then",
    "than",
    "but",
    "so",
    "such",
    "yes",
    "new",
    "old",
    "same",
    "different",
    "another",
    "other",
    "first",
    "last",
    "next",
    "current",
    "default",
    "right",
    "wrong",
    "correct",
    "actual",
    "actually",
    "just",
    "only",
    "also",
    "too",
    "very",
    "much",
    "many",
    "more",
    "most",
    "some",
    "any",
    "each",
    "every",
    "all",
    "both",
    "own",
    "down",
    "up",
    "broken",
    "error",
    "failed",
    "failing",
    "working",
    "slow",
    "fast",
    "bad",
    "good",
    "off",
    "on",
];

fn strip_token(token: &str) -> &str {
    token.trim_matches(|c: char| !(c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.' | '/')))
}

fn word(token: &str) -> String {
    strip_token(token).to_ascii_lowercase()
}

fn prefix_boundary(lower: &str, prefix: &str) -> bool {
    lower.strip_prefix(prefix).is_some_and(|rest| {
        rest.is_empty()
            || rest.starts_with(|c: char| c.is_ascii_digit() || matches!(c, '-' | '_' | '.' | '/'))
    })
}

fn is_model_name(token: &str) -> bool {
    let t = strip_token(token);
    if let Some((_, suffix)) = t.rsplit_once('/')
        && is_model_name(suffix)
    {
        return true;
    }
    let lower = t.to_ascii_lowercase();
    t.len() <= 80
        && t.chars().any(|c| c.is_ascii_digit())
        && MODEL_PREFIXES.iter().any(|p| prefix_boundary(&lower, p))
}

fn is_provider_name(token: &str) -> bool {
    let t = strip_token(token);
    let lower = t.to_ascii_lowercase();
    !t.is_empty()
        && t.len() <= 40
        && t.chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'))
        && t.chars().any(|c| c.is_ascii_alphabetic())
        && !VALUE_STOPWORDS.contains(&lower.as_str())
}

fn is_provider_value(token: &str) -> bool {
    let t = strip_token(token);
    let lower = t.to_ascii_lowercase();
    PROVIDER_NAMES.contains(&lower.as_str())
        || (is_provider_name(t)
            && t.chars()
                .any(|c| c.is_ascii_digit() || matches!(c, '-' | '.' | '/')))
}

/// A context size in tokens: "200k", "128000", "1.5m" is not supported (digits only),
/// "200kb"/"2mb" accepted. Anything under 1024 is not a plausible window.
fn parse_context_size(token: &str) -> Option<u64> {
    let lower = strip_token(token).to_ascii_lowercase();
    let (digits, mult) = [
        ("kb", 1_000u64),
        ("mb", 1_000_000u64),
        ("gb", 1_000_000_000u64),
        ("k", 1_000u64),
        ("m", 1_000_000u64),
        ("g", 1_000_000_000u64),
    ]
    .iter()
    .find_map(|(suffix, mult)| lower.strip_suffix(suffix).map(|d| (d, *mult)))
    .unwrap_or((lower.as_str(), 1));
    let n: u64 = digits.parse().ok()?;
    let value = n.checked_mul(mult)?;
    (value >= 1024).then_some(value)
}

/// Whether the model-name token at `i` is negated: "not gpt-4o" or
/// "don't use gpt-4o" / "do not use gpt-4o". Bare "no" is not a negation here —
/// it is the correction marker in "no, use gpt-4o".
fn negated(words: &[String], i: usize) -> bool {
    if i >= 1 && NEGATION_ADJACENT.contains(&words[i - 1].as_str()) {
        return true;
    }
    i >= 2
        && MODEL_INTENT.contains(&words[i - 1].as_str())
        && NEGATION_BEFORE_VERB.contains(&words[i - 2].as_str())
}

/// Whether a model-name token at `i` follows an intent verb, possibly through
/// "to"/"model": "use gpt-4o", "switch to gpt-4o", "set model to gpt-4o".
fn model_intent(words: &[String], i: usize) -> bool {
    if i == 0 {
        return false;
    }
    let prev = words[i - 1].as_str();
    if MODEL_INTENT.contains(&prev) || prev == "model" {
        return true;
    }
    prev == "to"
        && i >= 2
        && (MODEL_INTENT.contains(&words[i - 2].as_str()) || words[i - 2] == "model")
}

/// First non-filler token after position `i`, if it satisfies `pred`. Filler is
/// skipped at most 3 deep so "model is not gpt-4o" ("not" is not filler) stays silent.
fn next_value<'a>(tokens: &[&'a str], i: usize, pred: impl Fn(&str) -> bool) -> Option<&'a str> {
    for t in tokens.get(i + 1..)?.iter().take(3) {
        let w = word(t);
        if VALUE_FILLER.contains(&w.as_str()) {
            continue;
        }
        return pred(t).then_some(*t);
    }
    None
}

fn plausible_value(value: &str) -> bool {
    let lower = value.to_ascii_lowercase();
    !VALUE_STOPWORDS.contains(&lower.as_str())
        && value
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.' | '/'))
}

fn insert_value_signal(
    signals: &mut serde_json::Map<String, serde_json::Value>,
    field: &str,
    raw: &str,
) {
    let value = strip_token(raw);
    if value.is_empty() || value.len() > 120 || !plausible_value(value) {
        return;
    }
    if field == "context_window" {
        if let Some(n) = parse_context_size(value) {
            signals
                .entry(field.to_owned())
                .or_insert_with(|| serde_json::json!(n));
        }
        return;
    }
    signals
        .entry(field.to_owned())
        .or_insert_with(|| serde_json::Value::String(value.to_owned()));
}

fn scan_config_text(text: &str, signals: &mut serde_json::Map<String, serde_json::Value>) {
    let tokens: Vec<&str> = text.split_whitespace().collect();
    let words: Vec<String> = tokens.iter().map(|t| word(t)).collect();
    for (i, raw) in tokens.iter().enumerate() {
        if let Some((key, value)) = raw.split_once('=') {
            let field = word(key).replace('-', "_");
            if matches!(
                field.as_str(),
                "model" | "provider" | "fallback_model" | "context_window"
            ) {
                insert_value_signal(signals, &field, value);
            }
            continue;
        }
        let w = words[i].as_str();
        match w {
            "model" | "models" => {
                if let Some(v) = next_value(&tokens, i, is_model_name) {
                    insert_value_signal(signals, "model", v);
                }
            }
            "provider" | "providers" => {
                if let Some(v) = next_value(&tokens, i, is_provider_value) {
                    insert_value_signal(signals, "provider", v);
                }
            }
            "fallback" | "fallback_model" | "fallback-model" => {
                if negated(&words, i) {
                    continue;
                }
                if let Some(v) =
                    next_value(&tokens, i, |t| is_model_name(t) || is_provider_value(t))
                {
                    insert_value_signal(signals, "fallback_model", v);
                }
            }
            "context" | "context_window" | "context-window" => {
                let mut found = None;
                for t in tokens.get(i + 1..).unwrap_or(&[]).iter().take(5) {
                    if let Some(n) = parse_context_size(t) {
                        found = Some(n);
                        break;
                    }
                    if !CONTEXT_LINK.contains(&word(t).as_str()) {
                        break;
                    }
                }
                if found.is_none()
                    && i > 0
                    // "don't use 200k context" is a negation, not a size request.
                    && !(i >= 3
                        && MODEL_INTENT.contains(&words[i - 2].as_str())
                        && NEGATION_BEFORE_VERB.contains(&words[i - 3].as_str()))
                    && let Some(t) = tokens.get(i - 1)
                {
                    found = parse_context_size(t);
                }
                if let Some(n) = found {
                    signals
                        .entry("context_window".to_owned())
                        .or_insert_with(|| serde_json::json!(n));
                }
            }
            _ => {
                if PROVIDER_NAMES.contains(&w)
                    && tokens.get(i + 1).is_some_and(|t| word(t) == "provider")
                    && !negated(&words, i)
                {
                    insert_value_signal(signals, "provider", raw);
                }
                if is_model_name(raw) && model_intent(&words, i) && !negated(&words, i) {
                    insert_value_signal(signals, "model", raw);
                }
            }
        }
    }
}

/// Config-shaped signals (`model`, `provider`, `fallback_model`, `context_window`) a
/// piece of user text carries, as a JSON object `vikram presets suggest` can pick up
/// from experience metadata. Only unambiguous intents emit: a model-name token after
/// a use/switch verb, a config keyword followed by a plausible value, or an explicit
/// `field=value` pair. Anything else stays silent.
pub fn config_signals(texts: &[String]) -> serde_json::Map<String, serde_json::Value> {
    let mut signals = serde_json::Map::new();
    for text in texts {
        scan_config_text(text, &mut signals);
    }
    signals
}

/// The coarse outcome the experience records: a failed turn is a failure, a turn whose tools
/// all returned ok but whose verification went red is partial — "the work finished but did not
/// verify" is not a success — and anything else with clean errors is a success.
fn experience_outcome(
    outcome: crate::types::TurnState,
    growth: &TurnGrowth,
) -> vikram_evolution::ExperienceOutcome {
    use crate::types::TurnState;
    if outcome == TurnState::Failed {
        vikram_evolution::ExperienceOutcome::Failure
    } else if growth.errors.is_empty() {
        vikram_evolution::ExperienceOutcome::Success
    } else {
        vikram_evolution::ExperienceOutcome::Partial
    }
}

/// Whether the turn is reusable workflow evidence. Two clean tools was the old bar; a passed
/// `verify` is stronger evidence than a second tool call, so a verified turn with any tool use
/// qualifies even below the count. A failed verification keeps `errors` non-empty, so it can
/// never be reusable.
fn experience_reusable(growth: &TurnGrowth, outcome: crate::types::TurnState) -> bool {
    use crate::types::TurnState;
    let verified = matches!(growth.verification, Some(Verification::Passed));
    outcome != TurnState::Failed
        && growth.errors.is_empty()
        && (growth.tools.len() >= 2 || (verified && !growth.tools.is_empty()))
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
    // Resolved once so the dedup read and the eventual write can never disagree about the
    // shelf: `user`/`project` targets land on the shared scopes recall is bound to read.
    let scope = memory_write_scope(inner, &proposal);
    if proposal.kind == "memory" {
        let existing = match inner.memory.list(&scope) {
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
        if proposal.kind == "skill" {
            request_skill_approval(inner, &proposal);
        }
        inner.learning.surface.stage_proposal(proposal.clone());
        audit(inner, &proposal, "staged", &proposal.reason.clone());
        inner.learning.surface.save();
        return;
    }
    let outcome = if proposal.kind == "memory" {
        inner
            .memory
            .save(&proposal.key, &proposal.content, &scope)
            .map(|_| format!("auto-applied{}", tail(&proposal.reason)))
            .unwrap_or_else(|err| format!("could not save memory: {err}"))
    } else {
        let gate = inner
            .config
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .skill_proposal_gate;
        apply_skill_proposal_gated(&proposal, gate)
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

/// A staged skill proposal is a pending capability change, so it also lands on the
/// autonomy approval queue where `vikram evolution approvals` lists it and
/// `approve`/`reject` record a durable decision. This is a second record of the same
/// pending change, not a second executor: the learning proposal remains the gate that
/// performs the write (`vikram learning approve`), while the autonomy request is the
/// capability-change ledger — rejecting it tells the operator the change should not
/// proceed even though the staged proposal still exists. One request per proposal:
/// a pending request already naming this proposal id is reused, not duplicated.
/// Auto-policy skill proposals skip this entirely — the operator already chose to let
/// the runtime apply them without a per-change decision.
fn request_skill_approval(inner: &Arc<crate::runtime::RuntimeInner>, proposal: &LearningProposal) {
    let mut engine = inner
        .learning
        .evolution
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    let already_pending = engine.autonomy.get_pending_approvals().iter().any(|r| {
        r.status == vikram_evolution::autonomy::ApprovalStatus::Pending
            && r.reason.contains(&proposal.id)
    });
    if already_pending {
        return;
    }
    // Create and patch both rewrite what the agent can do, so both are ModifySkill;
    // the proposal's action is already named in the description.
    let action = vikram_evolution::ActionType::ModifySkill;
    let description = format!(
        "skill {} {}: {}",
        proposal.action,
        proposal.target,
        proposal.reason.trim()
    );
    let reason = format!(
        "learning proposal {} staged under the skill {} policy",
        proposal.id,
        inner.learning.surface.policy_skill().as_str()
    );
    if let Err(err) = engine
        .autonomy
        .request_approval(action, &description, &reason)
    {
        eprintln!("vikram: could not record autonomy approval for skill proposal: {err}");
    }
}

/// Memory writes land in the session's own scope: automatic recall only reads `global` +
/// the session id, and the shared store has no user dimension — a literal `user`/`memory`
/// scope would be both invisible to recall and readable by every other session.
///
/// Kept for callers that only hold a session id (the CLI approve path, benches). The
/// runtime's own apply path resolves the wider `user:`/`project:` shelves through
/// [`memory_write_scope`], which has the session and store context this signature lacks.
pub fn scope_of(target: &str, session: &SessionId) -> String {
    let _ = target;
    session.as_str().to_owned()
}

/// FNV-1a over the workspace path: a stable, dependency-free slug for the `project:`
/// shelf of sessions that share a working directory but have no project row.
fn fnv1a(bytes: &[u8]) -> u64 {
    let mut h = 0xcbf29ce484222325u64;
    for b in bytes {
        h ^= *b as u64;
        h = h.wrapping_mul(0x100000001b3);
    }
    h
}

/// The `project:<slug>` shelf for a session: its project row's id when one is attached
/// (the store reuses one row per directory, so sessions on the same project share it),
/// else a hash of the session's working directory so sessions on the same workspace
/// share a shelf without leaking the absolute path into the scope name.
fn project_memory_scope(session: &crate::session::SessionRuntime) -> Option<String> {
    let slug = match session.project_id() {
        Some(id) => id.as_str().to_owned(),
        None => {
            let cwd = session.cwd().to_string_lossy().into_owned();
            if cwd.trim().is_empty() {
                return None;
            }
            format!("ws-{:016x}", fnv1a(cwd.as_bytes()))
        }
    };
    crate::memory::project_scope(&slug)
}

/// The shared shelves a session may read and write: `user:<id>` from the runtime's active
/// identity (the same account `create_session` stamps on new sessions) and
/// `project:<slug>` from [`project_memory_scope`]. Either is `None` when the session has
/// no resolvable dimension — never guessed, so a write can only land where recall was
/// bound to look.
fn shared_memory_scopes(
    inner: &Arc<crate::runtime::RuntimeInner>,
    session: &crate::session::SessionRuntime,
) -> (Option<String>, Option<String>) {
    let user = inner
        .store
        .get_active_identity()
        .ok()
        .and_then(|identity| identity.user)
        .and_then(|id| crate::memory::user_scope(id.as_str()));
    (user, project_memory_scope(session))
}

/// Where a memory proposal's write lands. `user`/`project` targets go to the shared
/// `user:<id>`/`project:<slug>` shelves when the session resolves them; every other
/// target — and any shared scope that cannot be resolved — stays in the session's own
/// scope, the narrowest shelf recall already reads.
fn memory_write_scope(
    inner: &Arc<crate::runtime::RuntimeInner>,
    proposal: &LearningProposal,
) -> String {
    let session_scope = proposal.session.as_str().to_owned();
    if proposal.kind != "memory" {
        return session_scope;
    }
    let Some(session) = inner.session(&proposal.session) else {
        return session_scope;
    };
    let (user_scope, project_scope) = shared_memory_scopes(inner, &session);
    match proposal.target.as_str() {
        "user" => user_scope.unwrap_or(session_scope),
        "project" => project_scope.unwrap_or(session_scope),
        _ => session_scope,
    }
}

/// Tell the memory store which shared shelves this session may recall. Runs every turn so
/// a project switch or identity change is picked up; the store skips the write when the
/// binding is unchanged, and a session with neither dimension clears any stale binding.
fn bind_session_memory_scopes(
    inner: &Arc<crate::runtime::RuntimeInner>,
    session: &crate::session::SessionRuntime,
) {
    let (user, project) = shared_memory_scopes(inner, session);
    if let Err(err) =
        inner
            .memory
            .bind_session_scopes(session.id.as_str(), user.as_deref(), project.as_deref())
    {
        eprintln!(
            "vikram: could not bind memory scopes for {}: {err}",
            session.id
        );
    }
}

/// Applies one staged skill proposal through the skills [`vikram_skills::Manager`], honoring
/// `action`: "patch"/"improve"/"replace" rewrite an existing skill's body while keeping its
/// frontmatter (a patch naming a skill that does not exist yet falls back to create, since
/// the proposal content is still the best available draft); every other action creates.
/// The manager's error is returned verbatim so callers can audit exactly what refused.
pub fn apply_skill_proposal(proposal: &LearningProposal) -> Result<(), String> {
    apply_skill_proposal_gated(proposal, true)
}

/// [`apply_skill_proposal`] with the structural admission gate switchable. `gate: false`
/// is the pre-gate behavior, kept for operators who set `skill_proposal_gate: false`;
/// the manager's own write-time validation still applies either way.
pub fn apply_skill_proposal_gated(proposal: &LearningProposal, gate: bool) -> Result<(), String> {
    apply_skill_with(&vikram_skills::Manager::new(), proposal, gate)
}

fn apply_skill_with(
    manager: &vikram_skills::Manager,
    proposal: &LearningProposal,
    gate: bool,
) -> Result<(), String> {
    if gate {
        skill_gate(manager, proposal)
            .map_err(|err| format!("skill gate rejected the proposal: {err}"))?;
    }
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

/// Structural admission gate for skill proposals: renders the SKILL.md the manager would
/// write and runs it through the skills crate's own frontmatter/metadata validation —
/// the same check `Manager::create`/`edit` apply before touching the filesystem —
/// without writing anything. A patch must also find its target on disk and still parse.
/// This is deliberately *structural*: it proves the artifact is loadable, not that the
/// skill is any good. Behavioral gating (benchmarks, review) is future work; nothing
/// here runs a model.
fn skill_gate(manager: &vikram_skills::Manager, proposal: &LearningProposal) -> Result<(), String> {
    let name = {
        let target = proposal.target.trim();
        if target.is_empty() {
            "learned-skill"
        } else {
            target
        }
    };
    let id = vikram_skills::SkillId::new(name).map_err(|err| err.to_string())?;
    let dir = manager.root.join(id.as_str());
    let existing = ["SKILL.md", "skill.md"]
        .iter()
        .map(|file| dir.join(file))
        .find(|path| path.is_file());
    let patch = matches!(proposal.action.as_str(), "patch" | "improve" | "replace");
    let candidate = match (patch, existing) {
        (true, Some(path)) => {
            let text = std::fs::read_to_string(&path)
                .map_err(|err| format!("could not read {}: {err}", path.display()))?;
            let fm = vikram_skills::Frontmatter::parse(&text).map_err(|err| err.to_string())?;
            render_skill_document(&fm, &proposal.content)
        }
        _ => {
            let reason = proposal.reason.trim();
            let description = if reason.is_empty() {
                id.as_str().to_owned()
            } else {
                reason
                    .chars()
                    .take(vikram_skills::DESCRIPTION_MAX)
                    .collect::<String>()
            };
            let metadata = vikram_skills::SkillMetadata::new(id.as_str(), &description);
            let fm = metadata.to_frontmatter(&proposal.content);
            render_skill_document(&fm, &proposal.content)
        }
    };
    let parsed = vikram_skills::Frontmatter::parse(&candidate).map_err(|err| err.to_string())?;
    vikram_skills::SkillMetadata::from_frontmatter(&parsed, id.as_str())
        .map(|_| ())
        .map_err(|err| err.to_string())
}

/// `vikram_skills::frontmatter::render_document` is crate-private; this mirrors it so the
/// gate can validate the exact bytes the manager would write.
fn render_skill_document(fm: &vikram_skills::Frontmatter, body: &str) -> String {
    let yaml = fm.to_yaml();
    let mut document = String::from("---\n");
    document.push_str(&yaml);
    if !yaml.is_empty() && !yaml.ends_with('\n') {
        document.push('\n');
    }
    document.push_str("---\n");
    document.push_str(body);
    document
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

    fn signals_of(texts: &[&str]) -> serde_json::Map<String, serde_json::Value> {
        let owned: Vec<String> = texts.iter().map(|s| s.to_string()).collect();
        config_signals(&owned)
    }

    #[test]
    fn a_correction_naming_a_model_emits_a_config_signal() {
        let signals = signals_of(&["no, use gpt-4o instead of gpt-3.5"]);
        assert_eq!(signals["model"], "gpt-4o");
        assert!(signals.get("provider").is_none());
    }

    #[test]
    fn config_signals_cover_model_provider_fallback_and_context() {
        let signals = signals_of(&[
            "no, the model should be claude-sonnet-4",
            "actually, switch the provider to openrouter",
            "i meant fallback model to gpt-4o-mini",
            "no, the context window of 200k was too small",
        ]);
        assert_eq!(signals["model"], "claude-sonnet-4");
        assert_eq!(signals["provider"], "openrouter");
        assert_eq!(signals["fallback_model"], "gpt-4o-mini");
        assert_eq!(signals["context_window"], serde_json::json!(200000));
    }

    #[test]
    fn config_signals_read_key_value_and_bare_model_forms() {
        let signals = signals_of(&[
            "please set model=gpt-4o in the config",
            "use llama3.1-8b for this repo",
            "context_window=131072",
        ]);
        assert_eq!(signals["model"], "gpt-4o");
        assert_eq!(signals["context_window"], serde_json::json!(131072));
        // "llama3.1-8b" also matches, but the first observed value wins.
        assert!(signals.contains_key("model"));
    }

    #[test]
    fn config_signals_stay_silent_without_a_real_intent() {
        for text in [
            "no, use pnpm not npm",
            "actually, the endpoint is /v2",
            "that's wrong — it should have used the cache",
            "no, don't use gpt-4o for this",
            "the provider is down",
            "model is not gpt-4o",
            "what does gpt-4o cost?",
            "run node-20 tests",
            "no, increase the context a bit",
        ] {
            let signals = signals_of(&[text]);
            assert!(
                signals.is_empty(),
                "{text:?} must not emit config signals, got {signals:?}"
            );
        }
    }

    #[test]
    fn experience_metadata_carries_config_signals_for_preset_suggest() {
        let corrections = detect_corrections("no, use gpt-4o instead");
        let metadata = experience_metadata(
            &session(),
            &TurnGrowth::default(),
            "summarize this file",
            &corrections,
        );
        assert_eq!(metadata["config"]["model"], "gpt-4o");

        // The same correction in the request itself is also a signal.
        let metadata = experience_metadata(
            &session(),
            &TurnGrowth::default(),
            "use claude-sonnet-4 for the review",
            &[],
        );
        assert_eq!(metadata["config"]["model"], "claude-sonnet-4");
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
        apply_skill_with(&manager, &proposal, true).unwrap();

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
        apply_skill_with(&manager, &proposal, true).unwrap();
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
        apply_skill_with(&manager, &proposal, true).unwrap();
        assert!(tmp.path().join("skills/fresh-skill/SKILL.md").is_file());
        // A second create of the same name is refused, not silently overwritten.
        assert!(apply_skill_with(&manager, &proposal, true).is_err());
    }

    #[test]
    fn the_gate_rejects_a_proposal_whose_name_cannot_be_a_skill() {
        let tmp = tempfile::tempdir().unwrap();
        let manager = vikram_skills::Manager::at(tmp.path().join("skills"));
        let too_long = "x".repeat(vikram_skills::NAME_MAX + 1);
        for bad in ["Bad Name", "../escape", "double--dash", too_long.as_str()] {
            let proposal = surface_proposal("skill", "create", bad);
            let err = apply_skill_with(&manager, &proposal, true)
                .expect_err("the gate must refuse an invalid name");
            assert!(
                err.contains("skill gate rejected"),
                "rejection must be attributed to the gate, got: {err}"
            );
        }
        assert!(
            !tmp.path().join("skills").exists(),
            "rejected proposals must not leave a skill behind"
        );
    }

    #[test]
    fn an_empty_target_still_falls_back_to_a_valid_skill_name() {
        let tmp = tempfile::tempdir().unwrap();
        let manager = vikram_skills::Manager::at(tmp.path().join("skills"));
        let proposal = surface_proposal("skill", "create", "  ");
        apply_skill_with(&manager, &proposal, true).unwrap();
        assert!(tmp.path().join("skills/learned-skill/SKILL.md").is_file());
    }

    #[test]
    fn the_gate_rejects_a_patch_when_the_existing_skill_no_longer_parses() {
        let tmp = tempfile::tempdir().unwrap();
        let manager = vikram_skills::Manager::at(tmp.path().join("skills"));
        let skill_dir = tmp.path().join("skills/broken-skill");
        std::fs::create_dir_all(&skill_dir).unwrap();
        // Duplicate `name` keys make the frontmatter unparseable.
        std::fs::write(
            skill_dir.join("SKILL.md"),
            "---\nname: broken-skill\ndescription: d\nname: broken-skill\n---\nbody\n",
        )
        .unwrap();

        let proposal = surface_proposal("skill", "patch", "broken-skill");
        let err = apply_skill_with(&manager, &proposal, true)
            .expect_err("a patch onto an unloadable skill must be refused");
        assert!(err.contains("skill gate rejected"), "got: {err}");
        // The broken file is untouched: the gate rejected before any write.
        let doc = std::fs::read_to_string(skill_dir.join("SKILL.md")).unwrap();
        assert!(doc.contains("body"));
    }

    #[test]
    fn a_valid_patch_passes_the_gate_and_applies() {
        let tmp = tempfile::tempdir().unwrap();
        let manager = vikram_skills::Manager::at(tmp.path().join("skills"));
        manager
            .create("audit-flow", "audit desc", "old body")
            .unwrap();
        let mut proposal = surface_proposal("skill", "patch", "audit-flow");
        proposal.content = "new body".to_owned();
        apply_skill_with(&manager, &proposal, true).unwrap();
        let doc = std::fs::read_to_string(tmp.path().join("skills/audit-flow/SKILL.md")).unwrap();
        assert!(doc.contains("new body"));
    }

    #[test]
    fn disabling_the_gate_skips_the_preflight_but_not_the_managers_own_validation() {
        let tmp = tempfile::tempdir().unwrap();
        let manager = vikram_skills::Manager::at(tmp.path().join("skills"));
        let mut proposal = surface_proposal("skill", "create", "gate-off-skill");
        proposal.content = "body".to_owned();
        apply_skill_with(&manager, &proposal, false).unwrap();
        assert!(tmp.path().join("skills/gate-off-skill/SKILL.md").is_file());

        // Gate off does not mean garbage lands: the manager still refuses a bad name.
        let bad = surface_proposal("skill", "create", "Bad Name");
        let err = apply_skill_with(&manager, &bad, false).expect_err("manager still validates");
        assert!(!err.contains("skill gate rejected"), "got: {err}");
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

    fn verify_growth(content: &str, ok: bool) -> TurnGrowth {
        let mut growth = TurnGrowth::default();
        growth.record_call("verify");
        growth.record_verification(ok, content);
        growth
    }

    #[test]
    fn a_verify_pass_is_recorded_without_touching_errors() {
        let growth = verify_growth(
            "verify: PASS\n{\"ok\":true,\"attempts\":1,\"steps\":[]}",
            true,
        );
        assert_eq!(growth.verification, Some(Verification::Passed));
        assert!(growth.errors.is_empty());
    }

    #[test]
    fn a_verify_fail_names_the_step_and_kind_and_lands_in_errors() {
        let growth = verify_growth(
            "verify: FAIL step=test kind=test_failure\n{\"ok\":false}",
            false,
        );
        assert_eq!(
            growth.verification,
            Some(Verification::Failed {
                step: Some("test".to_owned()),
                kind: Some("test_failure".to_owned()),
                detail: None,
            })
        );
        assert_eq!(
            growth.errors,
            vec!["verify: FAIL test:test_failure".to_owned()]
        );
    }

    #[test]
    fn a_verify_fail_carries_the_failing_output_into_metadata() {
        let content = concat!(
            "verify: FAIL step=test kind=test_failure\n",
            "{\"ok\":false,\"failure\":{\"step\":\"test\",\"kind\":\"test_failure\"},",
            "\"failure_output\":\"running 3 tests\\ntest a ... ok\\nerror[E0308]: mismatched types\\nfailures: 1\"}"
        );
        let growth = verify_growth(content, false);
        let Some(Verification::Failed { step, kind, detail }) = &growth.verification else {
            panic!(
                "expected a failed verification, got {:?}",
                growth.verification
            );
        };
        assert_eq!(step.as_deref(), Some("test"));
        assert_eq!(kind.as_deref(), Some("test_failure"));
        let detail = detail.as_deref().expect("failure output should be kept");
        assert!(detail.contains("error[E0308]"), "{detail}");
        assert!(detail.contains("failures: 1"), "{detail}");

        let metadata = experience_metadata(&session(), &growth, "fix the tests", &[]);
        assert_eq!(metadata["verification"]["step"], "test");
        assert_eq!(metadata["verification"]["kind"], "test_failure");
        assert!(
            metadata["verification"]["detail"]
                .as_str()
                .unwrap_or_default()
                .contains("error[E0308]"),
            "{}",
            metadata["verification"]
        );

        let note = growth
            .errors
            .iter()
            .find(|e| e.starts_with("verify: FAIL"))
            .unwrap();
        assert!(note.contains("test:test_failure"), "{note}");
        assert!(note.contains("error[E0308]"), "{note}");
        assert!(note.chars().count() <= MAX_GROWTH_ERROR_LEN, "{note}");
    }

    #[test]
    fn verify_detail_survives_a_json_report_truncated_mid_string() {
        // The tool truncates its whole output; the failure_output literal can be cut before
        // its closing quote and before `steps` — the tail must still be recovered.
        let content = concat!(
            "verify: FAIL step=build kind=compile_error\n",
            "{\"ok\":false,\"attempts\":1,\"failure\":{\"step\":\"build\",\"kind\":\"compile_error\"},",
            "\"failure_output\":\"error[E0433]: failed to resolve: use of undeclared\\n   --> src/main.rs:4:5\\nerror: could not compile `app`"
        );
        let growth = verify_growth(content, false);
        let Some(Verification::Failed { detail, .. }) = &growth.verification else {
            panic!(
                "expected a failed verification, got {:?}",
                growth.verification
            );
        };
        let detail = detail
            .as_deref()
            .expect("truncated output should still be kept");
        assert!(detail.contains("could not compile"), "{detail}");
    }

    #[test]
    fn verify_detail_is_bounded_to_a_tail() {
        let long_output = format!("{}\nerror: the real failure", "x".repeat(5_000));
        let content = format!(
            "verify: FAIL step=test kind=unknown\n{}",
            serde_json::json!({"ok": false, "failure_output": long_output})
        );
        let growth = verify_growth(&content, false);
        let Some(Verification::Failed { detail, .. }) = &growth.verification else {
            panic!("expected a failed verification");
        };
        let detail = detail.as_deref().expect("detail kept");
        assert!(detail.chars().count() <= MAX_VERIFICATION_DETAIL);
        assert!(detail.contains("the real failure"));
        assert!(!detail.contains(&"x".repeat(1_000)));
    }

    #[test]
    fn verify_signal_falls_back_to_the_json_report() {
        let growth = verify_growth(
            "{\"ok\":false,\"failure\":{\"step\":\"build\",\"kind\":\"compile_error\"}}",
            false,
        );
        assert_eq!(
            growth.verification,
            Some(Verification::Failed {
                step: Some("build".to_owned()),
                kind: Some("compile_error".to_owned()),
                detail: None,
            })
        );
        assert_eq!(
            growth.errors,
            vec!["verify: FAIL build:compile_error".to_owned()]
        );
    }

    #[test]
    fn an_unparseable_verify_result_still_records_the_failure() {
        let growth = verify_growth("spawn failed: no such file", false);
        assert_eq!(
            growth.verification,
            Some(Verification::Failed {
                step: None,
                kind: None,
                detail: None,
            })
        );
        assert_eq!(
            growth.errors,
            vec!["verify: spawn failed: no such file".to_owned()]
        );
    }

    #[test]
    fn a_plugin_cannot_flip_a_failed_verdict_to_a_pass() {
        let growth = verify_growth("verify: FAIL step=test kind=timeout\n{}", true);
        assert!(matches!(
            growth.verification,
            Some(Verification::Failed { .. })
        ));
        assert!(!growth.errors.is_empty());
    }

    #[test]
    fn a_verify_fail_reads_as_partial_not_success() {
        let growth = verify_growth("verify: FAIL step=test kind=test_failure", false);
        assert_eq!(
            experience_outcome(crate::types::TurnState::Idle, &growth),
            vikram_evolution::ExperienceOutcome::Partial
        );
        assert!(!experience_reusable(&growth, crate::types::TurnState::Idle));
    }

    #[test]
    fn a_verified_pass_makes_reusable_below_the_two_tool_bar() {
        let growth = verify_growth("verify: PASS", true);
        assert_eq!(growth.tools, vec!["verify".to_owned()]);
        assert!(experience_reusable(&growth, crate::types::TurnState::Idle));
        assert_eq!(
            experience_outcome(crate::types::TurnState::Idle, &growth),
            vikram_evolution::ExperienceOutcome::Success
        );
    }

    #[test]
    fn verification_metadata_carries_the_verdict() {
        let failed = verify_growth("verify: FAIL step=test kind=lint_failure", false);
        let metadata = experience_metadata(&session(), &failed, "fix the build", &[]);
        assert_eq!(metadata["verification"]["passed"], false);
        assert_eq!(metadata["verification"]["step"], "test");
        assert_eq!(metadata["verification"]["kind"], "lint_failure");

        let clean = TurnGrowth::default();
        let metadata = experience_metadata(&session(), &clean, "hello", &[]);
        assert!(metadata.get("verification").is_none());
        assert!(metadata.get("config").is_none());
    }

    #[test]
    fn reflection_sees_a_verify_fail_as_an_issue_pattern() {
        use vikram_evolution::Reflector;
        let growth = verify_growth("verify: FAIL step=test kind=test_failure", false);
        let experience = vikram_evolution::Experience {
            id: "exp-verify".to_owned(),
            timestamp_ms: 0,
            agent_id: "main".to_owned(),
            project_id: None,
            task_category: "verify".to_owned(),
            user_request: "fix the build".to_owned(),
            outcome: experience_outcome(crate::types::TurnState::Idle, &growth),
            tools_used: growth.tools.clone(),
            errors: growth.errors.clone(),
            corrections: Vec::new(),
            feedback: None,
            reusable: experience_reusable(&growth, crate::types::TurnState::Idle),
            sensitivity: vikram_evolution::Sensitivity::Private,
            metadata: experience_metadata(&session(), &growth, "fix the build", &[]),
        };
        let proposals = vikram_evolution::HeuristicReflector
            .reflect(&experience)
            .unwrap();
        assert!(
            proposals
                .iter()
                .any(|p| p.kind == vikram_evolution::ProposalKind::RecordIssue),
            "a failed verification must surface as an issue pattern, got {proposals:?}"
        );
        assert!(!experience.reusable);
    }

    #[test]
    fn reflection_proposes_a_skill_for_a_verified_reusable_turn() {
        use vikram_evolution::Reflector;
        let growth = verify_growth("verify: PASS", true);
        let experience = vikram_evolution::Experience {
            id: "exp-verified".to_owned(),
            timestamp_ms: 0,
            agent_id: "main".to_owned(),
            project_id: None,
            task_category: "verify".to_owned(),
            user_request: "add the endpoint".to_owned(),
            outcome: experience_outcome(crate::types::TurnState::Idle, &growth),
            tools_used: growth.tools.clone(),
            errors: growth.errors.clone(),
            corrections: Vec::new(),
            feedback: None,
            reusable: experience_reusable(&growth, crate::types::TurnState::Idle),
            sensitivity: vikram_evolution::Sensitivity::Private,
            metadata: experience_metadata(&session(), &growth, "add the endpoint", &[]),
        };
        let proposals = vikram_evolution::HeuristicReflector
            .reflect(&experience)
            .unwrap();
        assert!(
            proposals
                .iter()
                .any(|p| p.kind == vikram_evolution::ProposalKind::CreateSkill),
            "a verified reusable turn should earn a skill candidate, got {proposals:?}"
        );
    }

    #[test]
    fn a_failed_verification_signature_is_stable_and_passes_have_none() {
        let growth = verify_growth("verify: FAIL step=test kind=test_failure", false);
        let metadata = experience_metadata(&session(), &growth, "fix tests", &[]);
        assert_eq!(
            metadata_verify_signature(&metadata).as_deref(),
            Some("verify:test:test_failure")
        );

        let passed = verify_growth("verify: PASS", true);
        let metadata = experience_metadata(&session(), &passed, "ok", &[]);
        assert!(metadata_verify_signature(&metadata).is_none());

        // A different failing step is a different signature: no false grouping.
        let other = verify_growth("verify: FAIL step=build kind=compile_error", false);
        let metadata = experience_metadata(&session(), &other, "fix build", &[]);
        assert_eq!(
            metadata_verify_signature(&metadata).as_deref(),
            Some("verify:build:compile_error")
        );
    }

    #[test]
    fn verify_signature_parts_normalize_whitespace_and_bound_length() {
        let long_kind = "k".repeat(200);
        let signature =
            verify_signature_parts(Some("cargo test --workspace"), Some(long_kind.as_str()));
        assert_eq!(
            signature,
            format!("verify:cargo_test_--workspace:{}", "k".repeat(40))
        );
        assert!(!signature.contains(' '));
        assert_eq!(verify_signature_parts(None, None), "verify:-:-");
    }

    #[tokio::test]
    async fn a_recurring_verify_failure_stages_one_project_memory_proposal() {
        let dir = tempfile::tempdir().unwrap();
        let (runtime, _guard) = test_runtime(dir.path()).await;
        let inner = runtime.handle().inner.clone();
        inner
            .learning
            .surface
            .set_policy_memory(LearningPolicy::ApprovalRequired);
        inner
            .learning
            .surface
            .set_policy_skill(LearningPolicy::Disabled);

        let session = inner.session_list().into_iter().next().unwrap();
        let growth = verify_growth("verify: FAIL step=test kind=test_failure", false);
        let turn = || {
            record_and_reflect(
                &inner,
                &session,
                "fix the tests",
                crate::types::TurnState::Idle,
                &growth,
                &[],
            )
        };
        let verify_failures = || {
            inner
                .learning
                .surface
                .pending_proposals()
                .into_iter()
                .filter(|p| p.key.starts_with("verify-failure-"))
                .collect::<Vec<_>>()
        };

        turn();
        turn();
        assert!(
            verify_failures().is_empty(),
            "two occurrences are still a transient, not a pattern"
        );

        turn();
        let staged = verify_failures();
        assert_eq!(staged.len(), 1, "the third identical failure surfaces once");
        assert_eq!(staged[0].kind, "memory");
        assert_eq!(staged[0].target, "project");
        assert!(staged[0].content.contains("verify:test:test_failure"));
        assert!(staged[0].reason.contains("3 times"));

        turn();
        assert_eq!(
            verify_failures().len(),
            1,
            "a pending proposal must not be re-staged every turn"
        );
    }

    /// Boot a real runtime against a temp home so `apply_or_stage` has a genuine
    /// `RuntimeInner` (evolution store + learning surface). The env guard is
    /// process-global, so it is held for the whole test.
    #[allow(
        clippy::await_holding_lock,
        reason = "isolated_home guards process-global env vars; releasing it at an await would let a concurrent test repoint VIKRAM_HOME mid-run"
    )]
    async fn test_runtime(
        dir: &std::path::Path,
    ) -> (crate::Runtime, std::sync::MutexGuard<'static, ()>) {
        let guard = crate::config::lock_security_env();
        unsafe { std::env::set_var("VIKRAM_HOME", dir.join("home")) };
        let config = crate::config::Config {
            model: "test-model".into(),
            workspace_root: Some(dir.to_path_buf()),
            ..Default::default()
        };
        let runtime = crate::Runtime::start(config).await.unwrap();
        (runtime, guard)
    }

    fn pending_autonomy_requests(
        inner: &Arc<crate::runtime::RuntimeInner>,
    ) -> Vec<vikram_evolution::autonomy::ApprovalRequest> {
        inner
            .learning
            .evolution
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .autonomy
            .get_pending_approvals()
            .iter()
            .filter(|r| r.status == vikram_evolution::autonomy::ApprovalStatus::Pending)
            .cloned()
            .collect()
    }

    #[tokio::test]
    async fn a_staged_skill_proposal_creates_an_autonomy_approval_request() {
        let dir = tempfile::tempdir().unwrap();
        let (runtime, _guard) = test_runtime(dir.path()).await;
        let inner = runtime.handle().inner.clone();
        inner
            .learning
            .surface
            .set_policy_skill(LearningPolicy::ApprovalRequired);
        inner
            .learning
            .surface
            .set_policy_memory(LearningPolicy::Disabled);

        let proposal = surface_proposal("skill", "create", "audit-flow");
        apply_or_stage(&inner, proposal.clone());

        let pending = pending_autonomy_requests(&inner);
        assert_eq!(pending.len(), 1, "one staged skill change, one request");
        assert_eq!(
            pending[0].action_type,
            vikram_evolution::ActionType::ModifySkill
        );
        assert_eq!(
            pending[0].risk_level,
            vikram_evolution::AutonomyLevel::ControlledDevelopment
        );
        assert!(
            pending[0].reason.contains(&proposal.id),
            "the request must name the learning proposal it records: {}",
            pending[0].reason
        );

        // The learning flow is untouched: the proposal is still the pending gate.
        assert_eq!(
            inner
                .learning
                .surface
                .find_proposal(&proposal.id)
                .unwrap()
                .status,
            "pending"
        );

        // Deciding it through the autonomy surface does not apply or drop the
        // learning proposal — the two records stay independent.
        inner
            .learning
            .evolution
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .autonomy
            .approve_request(&pending[0].id, "tester")
            .unwrap();
        assert_eq!(
            inner
                .learning
                .surface
                .find_proposal(&proposal.id)
                .unwrap()
                .status,
            "pending"
        );
        assert!(pending_autonomy_requests(&inner).is_empty());
    }

    #[tokio::test]
    async fn restaging_the_same_skill_proposal_does_not_duplicate_the_request() {
        let dir = tempfile::tempdir().unwrap();
        let (runtime, _guard) = test_runtime(dir.path()).await;
        let inner = runtime.handle().inner.clone();
        inner
            .learning
            .surface
            .set_policy_skill(LearningPolicy::ApprovalRequired);
        inner
            .learning
            .surface
            .set_policy_memory(LearningPolicy::Disabled);

        let proposal = surface_proposal("skill", "patch", "audit-flow");
        apply_or_stage(&inner, proposal.clone());
        apply_or_stage(&inner, proposal);

        assert_eq!(pending_autonomy_requests(&inner).len(), 1);
    }

    #[tokio::test]
    async fn non_skill_proposals_and_auto_policy_create_no_autonomy_requests() {
        let dir = tempfile::tempdir().unwrap();
        let (runtime, _guard) = test_runtime(dir.path()).await;
        let inner = runtime.handle().inner.clone();
        inner
            .learning
            .surface
            .set_policy_skill(LearningPolicy::ApprovalRequired);
        inner
            .learning
            .surface
            .set_policy_memory(LearningPolicy::ApprovalRequired);

        // A staged memory proposal is not a capability change: no autonomy request.
        apply_or_stage(&inner, surface_proposal("memory", "add", "user"));
        assert!(pending_autonomy_requests(&inner).is_empty());

        // Auto policy means the operator already chose unattended application:
        // staging never happens, so there is nothing to approve. (The apply may
        // fail on this test's empty skills root — either way no request exists.)
        inner
            .learning
            .surface
            .set_policy_skill(LearningPolicy::Auto);
        apply_or_stage(&inner, surface_proposal("skill", "create", "auto-skill"));
        assert!(pending_autonomy_requests(&inner).is_empty());
    }

    #[test]
    fn a_project_layer_proposal_maps_to_the_project_target() {
        let staged = proposal_from_evolution(
            &evolution_proposal(
                vikram_evolution::ProposalKind::AddMemory,
                serde_json::json!({"layer": "project", "content": "crate uses cargo-nextest"}),
            ),
            &session(),
        )
        .expect("a project-layer memory proposal maps onto the surface");
        assert_eq!(staged.kind, "memory");
        assert_eq!(staged.target, "project");
        assert_eq!(staged.content, "crate uses cargo-nextest");
    }

    #[tokio::test]
    async fn an_auto_applied_user_memory_lands_on_the_shared_user_scope() {
        let dir = tempfile::tempdir().unwrap();
        let (runtime, _guard) = test_runtime(dir.path()).await;
        let inner = runtime.handle().inner.clone();
        inner
            .learning
            .surface
            .set_policy_memory(LearningPolicy::Auto);

        let session = inner.session_list().into_iter().next().unwrap();
        let mut proposal = surface_proposal("memory", "add", "user");
        proposal.session = session.id.clone();
        proposal.key = "pref-1".to_owned();
        proposal.content = "prefers pnpm over npm".to_owned();
        apply_or_stage(&inner, proposal);

        let user = inner
            .store
            .get_active_identity()
            .unwrap()
            .user
            .expect("bootstrapped identity has a user");
        let user_scope = crate::memory::user_scope(user.as_str()).unwrap();
        assert_eq!(
            inner.memory.get("pref-1", &user_scope).unwrap(),
            Some("prefers pnpm over npm".to_owned()),
            "a user-target proposal writes the shared user shelf, not the session's"
        );
        assert_eq!(
            inner.memory.get("pref-1", session.id.as_str()).unwrap(),
            None,
            "nothing may be double-written into the session scope"
        );

        // Once the turn loop binds the session, recall of the session scope surfaces it.
        bind_session_memory_scopes(&inner, &session);
        let hits = inner
            .memory
            .recall_terms(
                &["pnpm".to_owned()],
                &["global".to_owned(), session.id.as_str().to_owned()],
                5,
            )
            .unwrap();
        assert!(
            hits.iter().any(|(scope, _, _)| scope == &user_scope),
            "bound recall must surface the user shelf: {hits:?}"
        );
    }

    #[tokio::test]
    async fn a_project_memory_lands_on_the_project_scope_and_stays_isolated() {
        let dir = tempfile::tempdir().unwrap();
        let (runtime, _guard) = test_runtime(dir.path()).await;
        let inner = runtime.handle().inner.clone();
        inner
            .learning
            .surface
            .set_policy_memory(LearningPolicy::Auto);

        let session = inner.session_list().into_iter().next().unwrap();
        let mut proposal = surface_proposal("memory", "add", "project");
        proposal.session = session.id.clone();
        proposal.key = "conv-1".to_owned();
        proposal.content = "this workspace uses cargo-nextest".to_owned();
        apply_or_stage(&inner, proposal);

        let expected = project_memory_scope(&session).expect("session has a workspace");
        assert_eq!(
            inner.memory.get("conv-1", &expected).unwrap(),
            Some("this workspace uses cargo-nextest".to_owned())
        );

        // A session on a different workspace resolves a different project shelf.
        let other_dir = tempfile::tempdir().unwrap();
        let other = crate::session::SessionRuntime::from_row(vikram_store::SessionRow {
            id: vikram_contracts::SessionId::generate(),
            title: "other".to_owned(),
            cwd: other_dir.path().to_path_buf(),
            model: None,
            bot: None,
            created_ms: 0,
            updated_ms: 0,
            status: "active".to_owned(),
            project_id: None,
            title_source: "generic".to_owned(),
            title_status: "ready".to_owned(),
            parent_session: None,
            forked_from: None,
        });
        assert_ne!(
            project_memory_scope(&other),
            Some(expected.clone()),
            "different workspaces must not share a project shelf"
        );
    }
}
