//! Runtime value types shared with the terminal interface.
//!
//! Nothing in here is a rendering concern: these are observations the runtime actually has. Fields
//! that cannot be known are `Option`, so an unavailable metric is omitted instead of invented.

use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::path::PathBuf;
use vikram_contracts::{
    AgentId, ApprovalId, Message, PermissionMode, SessionId, TeamId, ToolCallId, TurnId,
};
use vikram_kernel::TokenMeter;

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum RuntimeStatus {
    Initializing,
    Active,
    Idle,
    Stopping,
    Error(String),
}

impl RuntimeStatus {
    pub fn label(&self) -> &'static str {
        match self {
            Self::Initializing => "Runtime initializing",
            Self::Active => "Runtime active",
            Self::Idle => "Runtime idle",
            Self::Stopping => "Runtime stopping",
            Self::Error(_) => "Runtime error",
        }
    }

    pub fn is_error(&self) -> bool {
        matches!(self, Self::Error(_))
    }
}

/// Every value is sourced from the runtime. `None` means "not known", never zero-by-default.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct Metrics {
    pub model: Option<String>,
    pub provider: Option<String>,
    pub context_used: u64,
    pub context_window: Option<u64>,
    pub turn_elapsed_ms: Option<u64>,
    pub elapsed_ms: u64,
    pub tokens_per_sec: Option<f64>,
    pub background_tasks: usize,
    pub mcp_connected: usize,
    pub mcp_configured: usize,
    pub plugins_loaded: usize,
    pub plugins_installed: usize,
    pub subagents_active: usize,
    /// Skills discovered from the configured roots (metadata only, bodies loaded on demand).
    #[serde(default)]
    pub skills_discovered: usize,
    /// Skills whose bodies have been loaded into context this turn.
    #[serde(default)]
    pub skills_loaded: usize,
    /// Whether the active model/provider has thinking/reasoning enabled. Drives Thinking vs Working labels.
    pub thinking: bool,
}

impl Metrics {
    pub fn context_ratio(&self) -> Option<f32> {
        match (self.context_window, self.context_used) {
            (Some(window), used) if window > 0 => {
                Some((used as f32 / window as f32).clamp(0.0, 1.0))
            }
            _ => None,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum AgentState {
    Idle,
    Running,
    Thinking,
    Waiting,
    Completed,
    Failed,
    Paused,
    Stopped,
}

impl AgentState {
    pub fn label(self) -> &'static str {
        match self {
            Self::Idle => "Idle",
            Self::Running => "Working",
            Self::Thinking => "Thinking",
            Self::Waiting => "Waiting",
            Self::Completed => "Completed",
            Self::Failed => "Failed",
            Self::Paused => "Paused",
            Self::Stopped => "Stopped",
        }
    }

    pub fn is_active(self) -> bool {
        matches!(self, Self::Running | Self::Thinking | Self::Waiting)
    }

    /// Filled dot for active agents, hollow for the rest. Rendering picks the glyph for the
    /// terminal's Unicode support separately.
    pub fn is_filled(self) -> bool {
        self.is_active()
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct AgentProfile {
    pub id: AgentId,
    pub name: String,
    pub role: String,
    pub description: String,
    pub icon: Option<String>,
    pub created_ms: u64,
}

#[derive(Clone, Debug, PartialEq)]
pub struct AgentSnapshot {
    pub profile: AgentProfile,
    pub state: AgentState,
    pub current_task: Option<String>,
    /// Only ever set when the runtime genuinely tracks progress.
    pub progress: Option<f32>,
    pub session: Option<SessionId>,
    pub unread: usize,
}

/// A HIVE team as the runtime knows it: a user-named group of saved agent profiles sharing one
/// dedicated conversation. `working` is derived from that conversation's live turn state, and
/// `paused` is an explicit operator request that overrides it. Neither is ever guessed.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct TeamSnapshot {
    pub id: TeamId,
    pub name: String,
    pub members: Vec<AgentId>,
    pub session: Option<SessionId>,
    pub paused: bool,
    pub working: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum TaskState {
    Running,
    Completed,
    Failed,
    Cancelled,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct BackgroundTask {
    pub id: String,
    pub name: String,
    pub session: SessionId,
    pub state: TaskState,
    pub started_ms: u64,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum TurnState {
    Idle,
    Streaming,
    Tooling,
    WaitingApproval,
    Cancelled,
    Failed,
}

#[derive(Clone, Debug, PartialEq)]
pub struct SessionSnapshot {
    pub id: SessionId,
    pub title: String,
    pub cwd: PathBuf,
    pub model: Option<String>,
    pub messages: Vec<Message>,
    pub turn: TurnState,
    pub tokens: TokenMeter,
    pub created_ms: u64,
    pub updated_ms: u64,
    pub agent: Option<AgentId>,
    pub queued: usize,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ApprovalPrompt {
    pub approval: ApprovalId,
    pub session: SessionId,
    pub verb: String,
    pub resource: String,
    pub reason: String,
    pub amendment: Option<String>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum NoticeLevel {
    Info,
    Warn,
    Error,
}

#[derive(Clone, Debug, PartialEq)]
pub struct StateSnapshot {
    pub status: RuntimeStatus,
    pub metrics: Metrics,
    pub sessions: Vec<SessionSnapshot>,
    pub agents: Vec<AgentSnapshot>,
    pub teams: Vec<TeamSnapshot>,
    pub tasks: Vec<BackgroundTask>,
    pub approvals: Vec<ApprovalPrompt>,
    /// Skill names discovered from the configured roots (metadata only).
    pub skills: Vec<String>,
    /// The workspace root the runtime confines tools to; used by the TUI to resolve `@` paths.
    pub workspace_root: PathBuf,
    /// The posture in force, so an interface labels what it is actually running under.
    pub mode: PermissionMode,
}

/// The 11 approved activity types. Names are authoritative — use `label()` everywhere.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum ActivityKind {
    Wrote,
    Edited,
    Read,
    Deleted,
    Ran,
    Terminal,
    WebSearch,
    Delegated,
    Skill,
    Mcp,
    Plugin,
}

impl ActivityKind {
    pub fn label(&self) -> &'static str {
        match self {
            Self::Wrote => "Wrote",
            Self::Edited => "Edited",
            Self::Read => "Read",
            Self::Deleted => "Deleted",
            Self::Ran => "Ran",
            Self::Terminal => "Terminal",
            Self::WebSearch => "Web Search",
            Self::Delegated => "Delegated",
            Self::Skill => "Skill",
            Self::Mcp => "MCP",
            Self::Plugin => "Plugin",
        }
    }
}

/// A structured activity event emitted by the runtime. Captures what an agent actually did.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ActivityEvent {
    pub id: String,
    pub kind: ActivityKind,
    pub agent: Option<AgentId>,
    pub parent_agent: Option<AgentId>,
    pub session: SessionId,
    pub turn: Option<TurnId>,
    pub call: Option<ToolCallId>,
    pub summary: String,
    pub resource: Option<String>,
    pub content: Option<String>,
    pub detail: Option<String>,
    pub exit_code: Option<i32>,
    pub ok: bool,
    pub duration_ms: Option<u64>,
    pub metadata: Option<Value>,
    pub seq: u64,
}

#[derive(Clone, Debug, PartialEq)]
pub enum RuntimeEvent {
    Status(RuntimeStatus),
    Metrics(Metrics),
    SessionCreated(SessionSnapshot),
    SessionUpdated(SessionSnapshot),
    SessionClosed(SessionId),
    UserMessage {
        session: SessionId,
        turn: TurnId,
        text: String,
    },
    StreamStarted {
        session: SessionId,
        turn: TurnId,
    },
    StreamDelta {
        session: SessionId,
        turn: TurnId,
        text: String,
    },
    StreamThinkingDelta {
        session: SessionId,
        turn: TurnId,
        text: String,
    },
    /// A full assistant message committed to the journal (text and/or tool calls).
    MessageCommitted {
        session: SessionId,
        message: Message,
    },
    ToolStarted {
        session: SessionId,
        call: ToolCallId,
        name: String,
        summary: String,
        /// What the call is aimed at, taken from the tool's own permission request.
        resource: String,
        /// The arguments as JSON text, so a row can say what it was asked to do.
        arguments: String,
        /// The directory the tool runs in. A command means nothing without it.
        cwd: String,
    },
    ToolFinished {
        session: SessionId,
        call: ToolCallId,
        name: String,
        ok: bool,
        summary: String,
        /// The tool's output, which is what the row opens into.
        content: String,
        resource: String,
        arguments: String,
        cwd: String,
        duration_ms: u64,
    },
    TurnFinished {
        session: SessionId,
        turn: TurnId,
        state: TurnState,
    },
    AgentChanged(AgentSnapshot),
    AgentRemoved(AgentId),
    TeamChanged(TeamSnapshot),
    TeamRemoved(TeamId),
    TaskChanged(BackgroundTask),
    ApprovalRequested(ApprovalPrompt),
    /// A posture change, with what it replaced. The rules never changed; only what this runtime does
    /// with a prompt did.
    ModeChanged {
        session: SessionId,
        previous: PermissionMode,
        mode: PermissionMode,
    },
    ApprovalResolved {
        approval: ApprovalId,
        allowed: bool,
    },
    UserQuestion {
        session: SessionId,
        question: String,
        choices: Vec<String>,
        request_id: String,
    },
    /// The operator answered a pending [`RuntimeEvent::UserQuestion`]; the turn loop resumes.
    UserAnswerResolved {
        request_id: String,
        answer: String,
    },
    McpChanged {
        configured: usize,
        connected: usize,
        tools: usize,
    },
    PluginsChanged {
        installed: usize,
        loaded: usize,
    },
    SkillsChanged {
        discovered: usize,
        loaded: usize,
    },
    /// What the configured endpoint says it serves, read from its own `/models` listing. An endpoint
    /// that will not answer arrives as `error` rather than as an empty list that looks like a reply.
    ModelsListed {
        models: Vec<String>,
        error: Option<String>,
    },
    Notice {
        level: NoticeLevel,
        text: String,
    },
    Error {
        text: String,
    },
    Shutdown,
    Activity(ActivityEvent),
}

pub fn now_ms() -> u64 {
    use std::time::{SystemTime, UNIX_EPOCH};
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}
