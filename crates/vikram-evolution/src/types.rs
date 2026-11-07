use serde::{Deserialize, Serialize};
use std::time::{SystemTime, UNIX_EPOCH};

/// Current wall-clock time in milliseconds since the Unix epoch.
///
/// The clock is expected to be after the epoch; a (practically impossible)
/// pre-epoch reading is clamped to 1 rather than silently returning 0, so
/// that 0 remains usable as an "unset" sentinel for callers that check it.
pub fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| (d.as_millis() as u64).max(1))
        .unwrap_or(1)
}

pub fn gen_id(prefix: &str) -> String {
    use std::sync::atomic::{AtomicU64, Ordering};
    static C: AtomicU64 = AtomicU64::new(0);
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| (d.as_nanos() as u64).max(1))
        .unwrap_or(1);
    let seq = C.fetch_add(1, Ordering::Relaxed);
    format!("{prefix}-{nanos:020}-{seq:06}")
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MemoryLayer {
    Working,
    Episodic,
    Semantic,
    Procedural,
    User,
    Project,
    Agent,
    Shared,
}

impl MemoryLayer {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Working => "working",
            Self::Episodic => "episodic",
            Self::Semantic => "semantic",
            Self::Procedural => "procedural",
            Self::User => "user",
            Self::Project => "project",
            Self::Agent => "agent",
            Self::Shared => "shared",
        }
    }
    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "working" => Some(Self::Working),
            "episodic" => Some(Self::Episodic),
            "semantic" => Some(Self::Semantic),
            "procedural" => Some(Self::Procedural),
            "user" => Some(Self::User),
            "project" => Some(Self::Project),
            "agent" => Some(Self::Agent),
            "shared" => Some(Self::Shared),
            _ => None,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum KnowledgeStatus {
    Confirmed,
    UserStated,
    Observed,
    Inferred,
    Hypothesis,
    Temporary,
}

impl KnowledgeStatus {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Confirmed => "confirmed",
            Self::UserStated => "user_stated",
            Self::Observed => "observed",
            Self::Inferred => "inferred",
            Self::Hypothesis => "hypothesis",
            Self::Temporary => "temporary",
        }
    }
    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "confirmed" => Some(Self::Confirmed),
            "user_stated" => Some(Self::UserStated),
            "observed" => Some(Self::Observed),
            "inferred" => Some(Self::Inferred),
            "hypothesis" => Some(Self::Hypothesis),
            "temporary" => Some(Self::Temporary),
            _ => None,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ExperienceOutcome {
    Success,
    Failure,
    Partial,
}

impl ExperienceOutcome {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Success => "success",
            Self::Failure => "failure",
            Self::Partial => "partial",
        }
    }
    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "success" => Some(Self::Success),
            "failure" => Some(Self::Failure),
            "partial" => Some(Self::Partial),
            _ => None,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Sensitivity {
    Public,
    Private,
    Secret,
}

impl Sensitivity {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Public => "public",
            Self::Private => "private",
            Self::Secret => "secret",
        }
    }
    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "public" => Some(Self::Public),
            "private" => Some(Self::Private),
            "secret" => Some(Self::Secret),
            _ => None,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ProposalKind {
    NoChange,
    AddMemory,
    UpdateMemory,
    CreateSkill,
    ImproveSkill,
    RecordIssue,
    ProposeWorkflow,
    ProposeOptimization,
}

impl ProposalKind {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::NoChange => "no_change",
            Self::AddMemory => "add_memory",
            Self::UpdateMemory => "update_memory",
            Self::CreateSkill => "create_skill",
            Self::ImproveSkill => "improve_skill",
            Self::RecordIssue => "record_issue",
            Self::ProposeWorkflow => "propose_workflow",
            Self::ProposeOptimization => "propose_optimization",
        }
    }
    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "no_change" => Some(Self::NoChange),
            "add_memory" => Some(Self::AddMemory),
            "update_memory" => Some(Self::UpdateMemory),
            "create_skill" => Some(Self::CreateSkill),
            "improve_skill" => Some(Self::ImproveSkill),
            "record_issue" => Some(Self::RecordIssue),
            "propose_workflow" => Some(Self::ProposeWorkflow),
            "propose_optimization" => Some(Self::ProposeOptimization),
            _ => None,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ProposalStatus {
    Pending,
    Approved,
    Rejected,
    Applied,
}

impl ProposalStatus {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Pending => "pending",
            Self::Approved => "approved",
            Self::Rejected => "rejected",
            Self::Applied => "applied",
        }
    }
    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "pending" => Some(Self::Pending),
            "approved" => Some(Self::Approved),
            "rejected" => Some(Self::Rejected),
            "applied" => Some(Self::Applied),
            _ => None,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CandidateStatus {
    Candidate,
    Promoted,
    Rejected,
    RolledBack,
}

impl CandidateStatus {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Candidate => "candidate",
            Self::Promoted => "promoted",
            Self::Rejected => "rejected",
            Self::RolledBack => "rolled_back",
        }
    }
    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "candidate" => Some(Self::Candidate),
            "promoted" => Some(Self::Promoted),
            "rejected" => Some(Self::Rejected),
            "rolled_back" => Some(Self::RolledBack),
            _ => None,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ReflectionJobStatus {
    Pending,
    Running,
    Done,
    Failed,
}

impl ReflectionJobStatus {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Pending => "pending",
            Self::Running => "running",
            Self::Done => "done",
            Self::Failed => "failed",
        }
    }
    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "pending" => Some(Self::Pending),
            "running" => Some(Self::Running),
            "done" => Some(Self::Done),
            "failed" => Some(Self::Failed),
            _ => None,
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Experience {
    pub id: String,
    pub timestamp_ms: u64,
    pub agent_id: String,
    pub project_id: Option<String>,
    pub task_category: String,
    pub user_request: String,
    pub outcome: ExperienceOutcome,
    pub tools_used: Vec<String>,
    pub errors: Vec<String>,
    pub corrections: Vec<String>,
    pub feedback: Option<String>,
    pub reusable: bool,
    pub sensitivity: Sensitivity,
    pub metadata: serde_json::Value,
}

/// A layered memory fact. `superseded_by` points at the newer entry that
/// replaced this one; superseded (and expired) entries are hidden from
/// recall/listings by the store, but remain readable by id for audit.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct MemoryEntry {
    pub id: String,
    pub layer: MemoryLayer,
    pub scope: String, // agent:<id> | project:<id> | user:<id> | global
    pub key: String,
    pub value: String,
    pub status: KnowledgeStatus,
    pub confidence: f32,
    pub source_experience: Option<String>,
    pub provenance: String,
    pub created_ms: u64,
    pub updated_ms: u64,
    pub expires_ms: Option<u64>,
    pub superseded_by: Option<String>,
}

impl MemoryEntry {
    pub fn is_superseded(&self) -> bool {
        self.superseded_by.is_some()
    }
    pub fn is_expired(&self, now_ms: u64) -> bool {
        self.expires_ms.is_some_and(|exp| exp <= now_ms)
    }
    pub fn is_live(&self, now_ms: u64) -> bool {
        !self.is_superseded() && !self.is_expired(now_ms)
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct LearningProposal {
    pub id: String,
    pub experience_id: Option<String>,
    pub agent_id: String,
    pub kind: ProposalKind,
    pub summary: String,
    pub details: serde_json::Value,
    pub confidence: f32,
    pub requires_approval: bool,
    pub created_ms: u64,
    pub status: ProposalStatus,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct SkillVersion {
    pub skill_name: String,
    pub version: u32,
    pub content: String,
    pub created_ms: u64,
    pub parent_version: Option<u32>,
    pub change_description: String,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct OptimizationCandidate {
    pub id: String,
    pub target: String, // skill:<name> | agent_instructions | tool_description:<tool> | workflow:<name>
    pub parent_version: Option<String>,
    pub change_description: String,
    pub content: String,
    pub created_ms: u64,
    pub status: CandidateStatus,
    /// Why a candidate was rejected, when `status == Rejected`. Persisted so
    /// a later reader (or UI) can explain the decision; see
    /// `OptimizationEngine::reject`.
    #[serde(default)]
    pub rejection_reason: Option<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct EvaluationResult {
    /// Primary key. Older rows (pre-migration) were keyed only by
    /// `(candidate_id, dataset)` and silently replaced each other.
    pub id: String,
    pub candidate_id: String,
    pub dataset: String,
    pub metrics: serde_json::Value, // {success, correctness, tool_efficiency, reliability, ...}
    pub score: f64,
    pub passed: bool,
    pub notes: String,
    pub created_ms: u64,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct HiveLesson {
    pub id: String,
    pub task_id: String,
    pub agents: Vec<String>,
    pub outcome: String,
    pub lesson: String,
    pub pattern: String, // collaboration pattern name
    pub created_ms: u64,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct LearningEvent {
    pub id: String,
    pub timestamp_ms: u64,
    pub agent_id: String,
    pub kind: String,
    pub summary: String,
    pub payload: serde_json::Value,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ReflectionJob {
    pub id: String,
    pub experience_id: String,
    pub agent_id: String,
    pub status: ReflectionJobStatus,
    pub attempts: u32,
    pub created_ms: u64,
    pub updated_ms: u64,
    pub error: Option<String>,
}
