//! UI application state.
//!
//! This is a view model, not a runtime. It never executes tools, never owns a session's durable
//! state, and never cancels work by being dropped. Input produces [`UiCommand`]s that the host
//! applies to the runtime; runtime observations arrive as [`RuntimeEvent`]s and are folded in here.

use crate::editor::Editor;
use std::collections::VecDeque;
use std::path::PathBuf;
use vikram_contracts::{
    AgentId, ApprovalId, Message, PermissionMode, SessionId, ToolCallId, TurnId,
};
use vikram_kernel::TokenMeter;
use vikram_runtime::types::{
    AgentSnapshot, AgentState, ApprovalPrompt, BackgroundTask, Metrics, NoticeLevel, RuntimeEvent,
    RuntimeStatus, SessionSnapshot, StateSnapshot, TaskState, TeamSnapshot, TurnState, now_ms,
};

#[derive(Clone, Debug)]
pub struct Suggestion {
    pub text: String,
    pub description: String,
    pub category: String,
}

#[derive(Clone, Debug, Default)]
pub struct SuggestionPopup {
    pub items: Vec<Suggestion>,
    pub selected: usize,
}

#[derive(Clone, Debug)]
pub struct TabHit {
    pub session: SessionId,
    pub tab_x_start: u16,
    pub tab_x_end: u16,
    pub close_x_start: u16,
    pub close_x_end: u16,
}

pub const THINKING_FRAMES: [&str; 6] = ["·", "✢", "✳", "✶", "✻", "✽"];
pub const THINKING_FRAMES_ASCII: [&str; 6] = [".", "*", "+", "x", "o", "O"];
pub const PLASMA_FRAMES: [&str; 7] = [
    "░▒▓█▓▒░",
    "▒▓█▓▒░▒",
    "▓█▓▒░▒▓",
    "█▓▒░▒▓█",
    "▓▒░▒▓█▓",
    "▒░▒▓█▓▒",
    "░▒▓█▓▒░",
];
pub const PLASMA_FRAMES_ASCII: [&str; 4] = [".:-=-:.", ":-=-:.-", "-=-:.-:", "=-:.-:-"];

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Focus {
    Input,
    Conversation,
    Bots,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ActivityKind {
    Tool,
    Notice,
    Error,
    Agent,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Activity {
    pub kind: ActivityKind,
    pub text: String,
    pub call: Option<ToolCallId>,
    /// `None` while a tool is still running.
    pub ok: Option<bool>,
    /// Whether the activity's full content is expanded in the transcript.
    pub expanded: bool,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Streaming {
    pub turn: TurnId,
    pub text: String,
}

/// An `ask_user` question the runtime is blocked on. The view holds the text buffer; the host
/// drains the finished answer through [`App::take_answers`] and hands it to the runtime.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PendingQuestion {
    pub request_id: String,
    pub question: String,
    pub choices: Vec<String>,
    pub input: String,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Notice {
    pub level: NoticeLevel,
    pub text: String,
    pub at_ms: u64,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum InspectorKind {
    Background,
    Mcp,
    Plugins,
    SubAgents,
    Context,
    Model,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Overlay {
    Help,
    Approvals,
    Inspector(InspectorKind),
    AddAgent {
        selected: usize,
    },
    RenameSession {
        editor: Editor,
    },
    Autocomplete {
        candidates: Vec<String>,
        selected: usize,
    },
    Skills,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AgentAction {
    Pause,
    Resume,
    Stop,
}

#[derive(Clone, Debug, PartialEq)]
pub enum UiCommand {
    Submit {
        session: SessionId,
        text: String,
    },
    Queue {
        session: SessionId,
        text: String,
    },
    RunShell {
        session: SessionId,
        command: String,
    },
    SlashCommand {
        session: SessionId,
        command: String,
        args: String,
    },
    Cancel {
        session: SessionId,
    },
    NewSession,
    SwitchSession {
        session: SessionId,
    },
    RenameSession {
        session: SessionId,
        title: String,
    },
    CloseSession {
        session: SessionId,
    },
    AnswerApproval {
        approval: ApprovalId,
        allow: bool,
        persist: bool,
    },
    SpawnAgent {
        profile: AgentId,
        task: String,
        session: SessionId,
    },
    AgentControl {
        agent: AgentId,
        action: AgentAction,
    },
    RefreshMetrics,
    Quit,
}

#[derive(Clone, Debug)]
pub struct SessionView {
    pub id: SessionId,
    pub title: String,
    pub cwd: PathBuf,
    pub model: Option<String>,
    pub messages: Vec<Message>,
    pub streaming: Option<Streaming>,
    pub streaming_thought: Option<String>,
    pub turn: TurnState,
    pub tokens: TokenMeter,
    pub created_ms: u64,
    pub updated_ms: u64,
    pub agent: Option<AgentId>,
    pub editor: Editor,
    pub scroll: usize,
    pub follow: bool,
    pub queued: usize,
    pub activity: Vec<Activity>,
    pub expanded_activities: std::collections::HashSet<usize>,
    pub selected_activity: Option<usize>,
    pub unread: usize,
}

impl SessionView {
    pub fn from_snapshot(snapshot: SessionSnapshot) -> Self {
        Self {
            id: snapshot.id,
            title: snapshot.title,
            cwd: snapshot.cwd,
            model: snapshot.model,
            messages: snapshot.messages,
            streaming: None,
            streaming_thought: None,
            turn: snapshot.turn,
            tokens: snapshot.tokens,
            created_ms: snapshot.created_ms,
            updated_ms: snapshot.updated_ms,
            agent: snapshot.agent,
            editor: Editor::new(),
            scroll: 0,
            follow: true,
            queued: snapshot.queued,
            activity: Vec::new(),
            expanded_activities: std::collections::HashSet::new(),
            selected_activity: None,
            unread: 0,
        }
    }

    pub fn is_busy(&self) -> bool {
        matches!(
            self.turn,
            TurnState::Streaming | TurnState::Tooling | TurnState::WaitingApproval
        )
    }

    /// Append an activity entry, keeping the transcript glance bounded.
    pub fn push_activity(&mut self, activity: Activity) {
        self.activity.push(activity);
        const CAP: usize = 64;
        if self.activity.len() > CAP {
            let drop = self.activity.len() - CAP;
            self.activity.drain(0..drop);
            // Keep expanded/selection indices aligned with the retained slice.
            self.expanded_activities = self
                .expanded_activities
                .iter()
                .filter_map(|i| i.checked_sub(drop))
                .filter(|i| *i < self.activity.len())
                .collect();
            if let Some(sel) = self.selected_activity {
                if sel < drop {
                    self.selected_activity = None;
                } else {
                    let new = sel - drop;
                    if new < self.activity.len() {
                        self.selected_activity = Some(new);
                    } else {
                        self.selected_activity = None;
                    }
                }
            }
        }
    }

    pub fn apply_snapshot(&mut self, snapshot: SessionSnapshot) {
        self.title = snapshot.title;
        self.cwd = snapshot.cwd;
        self.model = snapshot.model;
        self.messages = snapshot.messages;
        self.turn = snapshot.turn;
        self.tokens = snapshot.tokens;
        self.updated_ms = snapshot.updated_ms;
        self.agent = snapshot.agent;
        self.queued = snapshot.queued;
    }
}

#[derive(Debug)]
pub struct App {
    pub sessions: Vec<SessionView>,
    pub active: usize,
    pub agents: Vec<AgentSnapshot>,
    pub teams: Vec<TeamSnapshot>,
    pub selected_agent: Option<AgentId>,
    pub tasks: Vec<BackgroundTask>,
    pub approvals: Vec<ApprovalPrompt>,
    pub status: RuntimeStatus,
    pub metrics: Metrics,
    pub notices: VecDeque<Notice>,
    pub overlay: Option<Overlay>,
    pub focus: Focus,
    pub ascii: bool,
    pub tick: u64,
    pub show_bots: bool,
    pub should_quit: bool,
    /// Hit regions for session tabs and their close buttons, recomputed each draw.
    pub tab_hits: Vec<TabHit>,
    /// X range (start, end) of the "+ New" control, if rendered.
    pub new_session_x: Option<(u16, u16)>,
    /// Live `/`, `@`, `!` suggestion popup.
    pub suggestions: Option<SuggestionPopup>,
    /// Workspace root reported by the runtime; used to resolve `@` paths.
    pub workspace_root: PathBuf,
    /// The approval posture the runtime reported, so the status bar labels what it is running under.
    pub mode: PermissionMode,
    pub approval_selection: usize,
    pub agent_selection: usize,
    /// The question the runtime is currently blocked on, if any.
    pub pending_question: Option<PendingQuestion>,
    /// Answers typed here and not yet handed to the runtime, as `(request_id, answer)`.
    pub pending_answers: Vec<(String, String)>,
    pub commands: Vec<UiCommand>,
}

impl App {
    pub fn new(ascii: bool) -> Self {
        Self {
            sessions: Vec::new(),
            active: 0,
            agents: Vec::new(),
            teams: Vec::new(),
            selected_agent: None,
            tasks: Vec::new(),
            approvals: Vec::new(),
            status: RuntimeStatus::Initializing,
            metrics: Metrics::default(),
            notices: VecDeque::new(),
            overlay: None,
            focus: Focus::Input,
            ascii,
            tick: 0,
            show_bots: true,
            should_quit: false,
            tab_hits: Vec::new(),
            new_session_x: None,
            suggestions: None,
            workspace_root: std::env::current_dir().unwrap_or_else(|_| PathBuf::from(".")),
            mode: PermissionMode::default(),
            approval_selection: 0,
            agent_selection: 0,
            pending_question: None,
            pending_answers: Vec::new(),
            commands: Vec::new(),
        }
    }

    pub fn active_session(&self) -> Option<&SessionView> {
        self.sessions.get(self.active)
    }

    /// The session currently shown in the conversation pane: an agent's dedicated session when one
    /// is selected, otherwise the active session. Selecting an agent never changes `active`.
    pub fn display_session(&self) -> Option<&SessionView> {
        if let Some(agent) = self.selected_agent_snapshot() {
            if let Some(id) = &agent.session {
                if let Some(view) = self.sessions.iter().find(|s| &s.id == id) {
                    return Some(view);
                }
            }
        }
        self.active_session()
    }

    pub fn active_session_mut(&mut self) -> Option<&mut SessionView> {
        self.sessions.get_mut(self.active)
    }

    pub fn session_index(&self, id: &SessionId) -> Option<usize> {
        self.sessions.iter().position(|s| s.id == *id)
    }

    pub fn take_commands(&mut self) -> Vec<UiCommand> {
        std::mem::take(&mut self.commands)
    }

    /// Hand the runtime every answer typed since the last drain, as `(request_id, answer)`.
    /// The host calls `RuntimeHandle::answer_question` for each pair.
    pub fn take_answers(&mut self) -> Vec<(String, String)> {
        std::mem::take(&mut self.pending_answers)
    }

    /// Accept the typed text as the answer to the pending question. The question is cleared here,
    /// so a single question can only be answered once; the runtime resolves the wait when the host
    /// drains the queue.
    pub fn submit_answer(&mut self, answer: String) -> bool {
        let Some(pending) = self.pending_question.take() else {
            return false;
        };
        self.pending_answers.push((pending.request_id, answer));
        true
    }

    /// Abandon a pending question locally. The turn keeps waiting until it is cancelled or the
    /// prompt is shown again; nothing is invented on the model's behalf.
    pub fn dismiss_question(&mut self) -> bool {
        self.pending_question.take().is_some()
    }

    /// Whether keyboard input belongs to the question prompt rather than the draft.
    pub fn awaiting_answer(&self) -> bool {
        self.pending_question.is_some()
    }

    pub fn push_notice(&mut self, level: NoticeLevel, text: impl Into<String>) {
        self.notices.push_back(Notice {
            level,
            text: text.into(),
            at_ms: now_ms(),
        });
        while self.notices.len() > 50 {
            self.notices.pop_front();
        }
    }

    pub fn session_by_id_mut(&mut self, id: &SessionId) -> Option<&mut SessionView> {
        self.sessions.iter_mut().find(|s| s.id == *id)
    }

    fn session_by_id(&mut self, id: &SessionId) -> Option<&mut SessionView> {
        self.sessions.iter_mut().find(|s| s.id == *id)
    }

    pub fn sync_snapshot(&mut self, snapshot: StateSnapshot) {
        self.status = snapshot.status;
        self.metrics = snapshot.metrics;
        self.agents = snapshot.agents;
        self.teams = snapshot.teams;
        self.workspace_root = snapshot.workspace_root;
        self.mode = snapshot.mode;
        self.tasks = snapshot.tasks;
        self.approvals = snapshot.approvals;

        let live: Vec<SessionId> = snapshot.sessions.iter().map(|r| r.id.clone()).collect();
        for row in snapshot.sessions {
            match self.session_index(&row.id) {
                Some(index) => self.sessions[index].apply_snapshot(row),
                None => self.sessions.push(SessionView::from_snapshot(row)),
            }
        }
        // A view whose session disappeared is dropped, unless work is still running against it;
        // the runtime reports the close once that work finishes.
        self.sessions
            .retain(|s| live.contains(&s.id) || s.is_busy());
        self.active = self.active.min(self.sessions.len().saturating_sub(1));
    }

    pub fn upsert_session(&mut self, snapshot: SessionSnapshot) {
        match self.session_index(&snapshot.id) {
            Some(index) => self.sessions[index].apply_snapshot(snapshot),
            None => {
                self.sessions.push(SessionView::from_snapshot(snapshot));
                self.active = self.sessions.len() - 1;
            }
        }
    }

    pub fn remove_session(&mut self, id: &SessionId) {
        if let Some(index) = self.session_index(id) {
            self.sessions.remove(index);
            self.active = self.active.min(self.sessions.len().saturating_sub(1));
        }
    }

    pub fn on_event(&mut self, event: RuntimeEvent) {
        match event {
            RuntimeEvent::Status(status) => self.status = status,
            RuntimeEvent::Metrics(metrics) => self.metrics = metrics,
            RuntimeEvent::SessionCreated(snapshot) | RuntimeEvent::SessionUpdated(snapshot) => {
                self.upsert_session(snapshot)
            }
            RuntimeEvent::SessionClosed(id) => self.remove_session(&id),
            RuntimeEvent::UserMessage {
                session,
                turn: _,
                text,
            } => {
                let active_id = self.active_session().map(|s| s.id.clone());
                if let Some(view) = self.session_by_id(&session) {
                    view.messages.push(Message::user(text));
                    view.follow = true;
                    if active_id.as_ref() != Some(&session) {
                        view.unread += 1;
                    }
                }
            }
            RuntimeEvent::StreamStarted { session, turn } => {
                if let Some(view) = self.session_by_id(&session) {
                    view.streaming = Some(Streaming {
                        turn,
                        text: String::new(),
                    });
                    view.turn = TurnState::Streaming;
                    view.follow = true;
                }
            }
            RuntimeEvent::StreamDelta {
                session,
                turn,
                text,
            } => {
                if let Some(view) = self.session_by_id(&session) {
                    match &mut view.streaming {
                        Some(stream) if stream.turn == turn => stream.text.push_str(&text),
                        _ => {
                            view.streaming = Some(Streaming {
                                turn,
                                text: text.clone(),
                            })
                        }
                    }
                    view.follow = true;
                }
            }
            RuntimeEvent::StreamThinkingDelta {
                session,
                turn: _,
                text,
            } => {
                if let Some(view) = self.session_by_id(&session) {
                    match &mut view.streaming_thought {
                        Some(buf) => buf.push_str(&text),
                        None => view.streaming_thought = Some(text),
                    }
                    view.follow = true;
                }
            }
            RuntimeEvent::MessageCommitted { session, message } => {
                let active_id = self.active_session().map(|s| s.id.clone());
                if let Some(view) = self.session_by_id(&session) {
                    if message.role == vikram_contracts::Role::Assistant {
                        view.streaming = None;
                    }
                    view.messages.push(message);
                    view.follow = true;
                    if active_id.as_ref() != Some(&session) {
                        view.unread += 1;
                    }
                }
            }
            RuntimeEvent::ToolStarted {
                session,
                call,
                name,
                summary,
            } => {
                if let Some(view) = self.session_by_id(&session) {
                    view.turn = TurnState::Tooling;
                    view.push_activity(Activity {
                        kind: ActivityKind::Tool,
                        text: format!("{name} · {summary}"),
                        call: Some(call),
                        ok: None,
                        expanded: false,
                    });
                }
            }
            RuntimeEvent::ToolFinished {
                session,
                call,
                name,
                ok,
                summary,
            } => {
                if let Some(view) = self.session_by_id(&session) {
                    let existing = view
                        .activity
                        .iter_mut()
                        .rev()
                        .find(|a| a.call.as_ref() == Some(&call));
                    match existing {
                        Some(activity) => {
                            activity.ok = Some(ok);
                            activity.text = format!("{name} · {summary}");
                        }
                        None => view.push_activity(Activity {
                            kind: ActivityKind::Tool,
                            text: format!("{name} · {summary}"),
                            call: Some(call),
                            ok: Some(ok),
                            expanded: false,
                        }),
                    }
                }
            }
            RuntimeEvent::TurnFinished {
                session,
                turn: _,
                state,
            } => {
                if let Some(view) = self.session_by_id(&session) {
                    view.turn = state;
                    view.streaming = None;
                    view.streaming_thought = None;
                    view.follow = true;
                }
            }
            RuntimeEvent::AgentChanged(agent) => {
                match self
                    .agents
                    .iter_mut()
                    .find(|a| a.profile.id == agent.profile.id)
                {
                    Some(existing) => *existing = agent,
                    None => self.agents.push(agent),
                }
            }
            RuntimeEvent::AgentRemoved(id) => {
                self.agents.retain(|a| a.profile.id != id);
                if self.selected_agent.as_ref() == Some(&id) {
                    self.selected_agent = None;
                }
            }
            RuntimeEvent::TaskChanged(task) => {
                match self.tasks.iter_mut().find(|t| t.id == task.id) {
                    Some(existing) => *existing = task,
                    None => self.tasks.push(task),
                }
            }
            RuntimeEvent::ApprovalRequested(prompt) => {
                self.approvals.push(prompt);
                self.overlay = Some(Overlay::Approvals);
                self.approval_selection = 0;
            }
            RuntimeEvent::ApprovalResolved { approval, allowed } => {
                self.approvals.retain(|a| a.approval != approval);
                let level = if allowed {
                    NoticeLevel::Info
                } else {
                    NoticeLevel::Warn
                };
                self.push_notice(
                    level,
                    if allowed {
                        "Approval granted"
                    } else {
                        "Approval denied"
                    },
                );
            }
            RuntimeEvent::McpChanged {
                configured,
                connected,
                tools: _,
            } => {
                self.metrics.mcp_configured = configured;
                self.metrics.mcp_connected = connected;
            }
            RuntimeEvent::PluginsChanged { installed, loaded } => {
                self.metrics.plugins_installed = installed;
                self.metrics.plugins_loaded = loaded;
            }
            RuntimeEvent::SkillsChanged { discovered, loaded } => {
                self.metrics.skills_discovered = discovered;
                self.metrics.skills_loaded = loaded;
            }
            RuntimeEvent::Notice { level, text } => self.push_notice(level, text),
            RuntimeEvent::ModeChanged { mode, .. } => {
                self.mode = mode;
                self.push_notice(NoticeLevel::Info, format!("mode: {}", mode.as_str()))
            }
            RuntimeEvent::Error { text } => {
                self.push_notice(NoticeLevel::Error, text.clone());
                if let Some(view) = self.active_session_mut() {
                    view.push_activity(Activity {
                        kind: ActivityKind::Error,
                        text,
                        call: None,
                        ok: Some(false),
                        expanded: false,
                    });
                }
            }
            RuntimeEvent::Shutdown => {
                self.should_quit = true;
            }
            RuntimeEvent::Activity(_) => {}
            RuntimeEvent::UserQuestion {
                question,
                choices,
                request_id,
                ..
            } => {
                let mut text = format!("Question: {question}");
                if !choices.is_empty() {
                    text.push_str(&format!("\nChoices: {}", choices.join(", ")));
                }
                self.push_notice(NoticeLevel::Info, text);
                // The turn is blocked until this is answered, so the prompt takes the keyboard.
                self.pending_question = Some(PendingQuestion {
                    request_id,
                    question,
                    choices,
                    input: String::new(),
                });
                self.focus = Focus::Input;
            }
            RuntimeEvent::UserAnswerResolved { request_id, answer } => {
                if self
                    .pending_question
                    .as_ref()
                    .is_some_and(|pending| pending.request_id == request_id)
                {
                    self.pending_question = None;
                }
                self.push_notice(NoticeLevel::Info, format!("Answered: {answer}"));
            }
            RuntimeEvent::TeamChanged(team) => {
                match self.teams.iter_mut().find(|t| t.id == team.id) {
                    Some(existing) => *existing = team,
                    None => self.teams.push(team),
                }
            }
            RuntimeEvent::TeamRemoved(id) => {
                self.teams.retain(|t| t.id != id);
            }
        }
    }

    pub fn thinking_frame(&self) -> &'static str {
        let frames = if self.ascii {
            &THINKING_FRAMES_ASCII
        } else {
            &THINKING_FRAMES
        };
        frames[(self.tick as usize / 2) % frames.len()]
    }

    pub fn plasma_frame(&self) -> &'static str {
        let frames: &[&str] = if self.ascii {
            &PLASMA_FRAMES_ASCII
        } else {
            &PLASMA_FRAMES
        };
        frames[(self.tick as usize / 2) % frames.len()]
    }

    pub fn active_task_count(&self) -> usize {
        self.tasks
            .iter()
            .filter(|t| t.state == TaskState::Running)
            .count()
    }

    pub fn active_agent_count(&self) -> usize {
        self.agents.iter().filter(|a| a.state.is_active()).count()
    }

    pub fn selected_agent_snapshot(&self) -> Option<&AgentSnapshot> {
        self.selected_agent
            .as_ref()
            .and_then(|id| self.agents.iter().find(|a| a.profile.id == *id))
    }

    pub fn agent_state_label(&self, state: AgentState) -> &'static str {
        match state {
            AgentState::Paused => "Paused",
            AgentState::Completed => "Finished",
            AgentState::Failed | AgentState::Stopped => "Paused",
            _ => {
                if self.metrics.thinking {
                    "Thinking"
                } else {
                    "Working"
                }
            }
        }
    }

    pub fn cycle_session(&mut self, forward: bool) {
        if self.sessions.is_empty() {
            return;
        }
        let len = self.sessions.len();
        self.active = if forward {
            (self.active + 1) % len
        } else {
            (self.active + len - 1) % len
        };
        if let Some(session) = self.sessions.get_mut(self.active) {
            session.unread = 0;
        }
        self.focus = Focus::Input;
    }

    pub fn scroll_up(&mut self, lines: usize) {
        if let Some(view) = self.active_session_mut() {
            view.scroll = view.scroll.saturating_add(lines);
            view.follow = false;
        }
    }

    pub fn scroll_down(&mut self, lines: usize) {
        if let Some(view) = self.active_session_mut() {
            view.scroll = view.scroll.saturating_sub(lines);
            if view.scroll == 0 {
                view.follow = true;
            }
        }
    }

    pub fn scroll_to_bottom(&mut self) {
        if let Some(view) = self.active_session_mut() {
            view.scroll = 0;
            view.follow = true;
        }
    }
}
