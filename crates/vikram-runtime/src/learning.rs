//! Runtime-level self-improvement: learning review, proposals, approval, audit.
//!
//! Inspired by Hermes Agent's background review: after a meaningful turn, a background task
//! evaluates the conversation snapshot and proposes memory/skill changes. Changes are either
//! applied (auto mode) or staged for approval. Nothing modifies the live conversation or its
//! prompt cache. Learning never grants permissions.

use std::collections::{HashMap, HashSet};
use std::sync::Mutex;
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
