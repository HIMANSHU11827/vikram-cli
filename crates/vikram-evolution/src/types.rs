use serde::{Deserialize, Serialize};
use std::time::{SystemTime, UNIX_EPOCH};

pub fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

pub fn gen_id(prefix: &str) -> String {
    use std::sync::atomic::{AtomicU64, Ordering};
    static C: AtomicU64 = AtomicU64::new(0);
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos() as u64)
        .unwrap_or(0);
    let seq = C.fetch_add(1, Ordering::Relaxed);
    format!("{prefix}-{nanos:020}-{seq:06}")
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
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

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Experience {
    pub id: String,
    pub timestamp_ms: u64,
    pub agent_id: String,
    pub project_id: Option<String>,
    pub task_category: String,
    pub user_request: String,
    pub outcome: String, // success|failure|partial
    pub tools_used: Vec<String>,
    pub errors: Vec<String>,
    pub corrections: Vec<String>,
    pub feedback: Option<String>,
    pub reusable: bool,
    pub sensitivity: String, // public|private|secret
    pub metadata: serde_json::Value,
}

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

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct LearningProposal {
    pub id: String,
    pub experience_id: Option<String>,
    pub agent_id: String,
    pub kind: String, // no_change | add_memory | update_memory | create_skill | improve_skill | record_issue | propose_workflow | propose_optimization
    pub summary: String,
    pub details: serde_json::Value,
    pub confidence: f32,
    pub requires_approval: bool,
    pub created_ms: u64,
    pub status: String, // pending | approved | rejected | applied
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
    pub status: String, // candidate | promoted | rejected | rolled_back
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct EvaluationResult {
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
    pub status: String, // pending | running | done | failed
    pub attempts: u32,
    pub created_ms: u64,
    pub updated_ms: u64,
    pub error: Option<String>,
}
