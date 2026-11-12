//! Runtime value types shared with the terminal interface.
//!
//! Nothing in here is a rendering concern: these are observations the runtime actually has. Fields
//! that cannot be known are `Option`, so an unavailable metric is omitted instead of invented.

use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::path::PathBuf;
use vikram_contracts::TokenMeter;
use vikram_contracts::{
    ApprovalId, BotId, HiveRosterId, Message, PermissionMode, ProjectId, SessionId, ToolCallId,
    TurnId,
};

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
    /// True when `context_used` rests on provider-reported usage rather than the estimator.
    #[serde(default)]
    pub context_measured: bool,
    /// True when no source advertised a window and a conservative floor is being assumed.
    #[serde(default)]
    pub context_limit_assumed: bool,
    /// How many times transcripts have been folded across live sessions.
    #[serde(default)]
    pub context_compactions: u64,
    /// Tokens the budget still allows, after output reserve, overhead and safety margin.
    #[serde(default)]
    pub context_usable: u64,
    pub turn_elapsed_ms: Option<u64>,
    pub elapsed_ms: u64,
    pub tokens_per_sec: Option<f64>,
    pub background_tasks: usize,
    pub mcp_connected: usize,
    pub mcp_configured: usize,
    pub plugins_loaded: usize,
    pub plugins_installed: usize,
    pub bots_active: usize,
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

/// One labelled slice of a request, priced by the same counter that prices the transcript.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ContextPart {
    pub label: String,
    pub tokens: u64,
}

impl ContextPart {
    pub fn new(label: impl Into<String>, tokens: u64) -> Self {
        Self {
            label: label.into(),
            tokens,
        }
    }
}

/// Everything the interface needs to say what a context is made of and where the number came from.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ContextMeter {
    pub window: u64,
    pub usable: u64,
    pub used: u64,
    /// True when `used` rests on a provider-reported figure for the older part of the transcript.
    pub measured: bool,
    /// True when nothing advertised a window and a conservative floor is being assumed.
    pub limits_assumed: bool,
    /// Tokens held back for the reply.
    pub output_reserve: u64,
    /// The fixed cost of the system parts and tool schemas.
    pub overhead: u64,
    pub parts: Vec<ContextPart>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum BotState {
    Idle,
    Running,
    Thinking,
    Waiting,
    Completed,
    Failed,
    Paused,
    Stopped,
}

impl BotState {
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

    /// Filled dot for active bots, hollow for the rest. Rendering picks the glyph for the
    /// terminal's Unicode support separately.
    pub fn is_filled(self) -> bool {
        self.is_active()
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct BotProfile {
    pub id: BotId,
    pub name: String,
    pub role: String,
    pub description: String,
    pub icon: Option<String>,
    pub created_ms: u64,
}

#[derive(Clone, Debug, PartialEq)]
pub struct BotSnapshot {
    pub profile: BotProfile,
    pub state: BotState,
    pub current_task: Option<String>,
    /// Only ever set when the runtime genuinely tracks progress.
    pub progress: Option<f32>,
    pub session: Option<SessionId>,
    pub unread: usize,
}

/// A HIVE roster as the runtime knows it: a user-named group of saved agent profiles sharing one
/// dedicated conversation. `working` is derived from that conversation's live turn state, and
/// `paused` is an explicit operator request that overrides it. Neither is ever guessed.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct HiveRosterSnapshot {
    pub id: HiveRosterId,
    pub name: String,
    pub members: Vec<BotId>,
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
    pub bot: Option<BotId>,
    pub queued: usize,
    /// Every follow-up waiting behind the running turn, in the order it will run. A queued text is
    /// not a message yet — the runtime only commits it when its turn starts — so this is the only
    /// copy an interface can show while it waits.
    pub queue: Vec<String>,
    /// The steer instruction waiting to be picked up at the next safe point of the running turn.
    pub steer_text: Option<String>,
    /// The most recently queued follow-up, so an interface can preview what runs next.
    pub queue_text: Option<String>,
    pub project_id: Option<ProjectId>,
    pub status: String,
    /// `generic`, `auto` or `manual` — how this session got its name, so an interface can tell the
    /// operator which titles are still the placeholder and whether renaming would stick.
    pub title_source: String,
    /// `pending`, `ready` or `failed` for automatic naming. `pending` is what a `generic` title
    /// carries before the first meaningful message has been named.
    pub title_status: String,
}

/// One row of the session catalogue: every session the store knows about, whether or not this process
/// is holding it live. A tab is a view onto a transcript, so the list has to be built from both the
/// live set and the store — otherwise closing a tab would look like deleting the conversation.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SessionSummary {
    pub id: SessionId,
    pub title: String,
    pub model: Option<String>,
    pub messages: usize,
    /// True while the session is loaded and can be talked to without reopening it.
    pub open: bool,
    pub updated_ms: u64,
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
    pub bots: Vec<BotSnapshot>,
    pub rosters: Vec<HiveRosterSnapshot>,
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
    pub bot: Option<BotId>,
    pub parent_agent: Option<BotId>,
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
    BotChanged(BotSnapshot),
    AgentRemoved(BotId),
    HiveRosterChanged(HiveRosterSnapshot),
    TeamRemoved(HiveRosterId),
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
    /// The transcript was folded. Reported every time, because a silent compaction is the one an
    /// operator cannot trust the answer after.
    ContextCompacted {
        session: SessionId,
        folded: usize,
        kept: usize,
        before_tokens: u64,
        after_tokens: u64,
        /// Journal sequence the folded span reaches, which is where the originals still live.
        through_seq: u64,
    },
    /// A tool result was digested in place rather than folded into a summary.
    ContextPruned {
        session: SessionId,
        replaced: usize,
        saved_tokens: u64,
    },
    /// What actually went into the request that is about to be sent, priced part by part. Published
    /// once per provider call: the inspector is only honest if it shows the thing being sent.
    ContextMetered {
        session: SessionId,
        meter: ContextMeter,
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
