//! UI application state.
//!
//! This is a view model, not a runtime. It never executes tools, never owns a session's durable
//! state, and never cancels work by being dropped. Input produces [`UiCommand`]s that the host
//! applies to the runtime; runtime observations arrive as [`RuntimeEvent`]s and are folded in here.

use crate::editor::Editor;
use std::collections::{HashMap, VecDeque};
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
    /// The row the tabs were drawn on. A click names a row as well as a column, or every press in
    /// the left-hand columns would be read as a tab press.
    pub row: u16,
    pub tab_x_start: u16,
    pub tab_x_end: u16,
    pub close_x_start: u16,
    pub close_x_end: u16,
}

/// Which thinking block a fold belongs to: its message and the index of the block inside it.
/// A stream that has not been committed to a message yet gets [`STREAMING_THOUGHT`].
pub type ThoughtKey = (usize, usize);
pub const STREAMING_THOUGHT: ThoughtKey = (usize::MAX, 0);

/// The header line of a thinking block, as it was drawn. The draw writes it and the mouse reads it,
/// so a click names the block it landed on instead of guessing at the geometry.
#[derive(Clone, Copy, Debug)]
pub struct ThoughtHit {
    pub row: u16,
    pub x_start: u16,
    pub x_end: u16,
    pub key: ThoughtKey,
    /// A tool call folds its own payload; a thinking block folds its prose. Both share the key
    /// space, so the click has to say which map the fold belongs in.
    pub tool: bool,
}

/// A drag across the transcript. Screen coordinates, not transcript lines: the anchor names the
/// cell the press landed on, so a selection survives the transcript scrolling under it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Selection {
    pub anchor: (u16, u16),
    pub cursor: (u16, u16),
}

impl Selection {
    pub fn new(anchor: (u16, u16)) -> Self {
        Self {
            anchor,
            cursor: anchor,
        }
    }

    /// Anchor and cursor in drawing order, so a drag up-left reads the same as one down-right.
    pub fn ordered(&self) -> ((u16, u16), (u16, u16)) {
        let (a, c) = (self.anchor, self.cursor);
        if (a.1, a.0) <= (c.1, c.0) {
            (a, c)
        } else {
            (c, a)
        }
    }

    /// A press that never moved. It selects nothing, so it must not clobber the clipboard.
    pub fn is_click(&self) -> bool {
        self.anchor == self.cursor
    }

    /// The columns highlighted on `row`, or `None` when the row is outside the drag.
    pub fn span(&self, row: u16) -> Option<(u16, u16)> {
        let ((sx, sy), (ex, ey)) = self.ordered();
        if row < sy || row > ey {
            return None;
        }
        let start = if row == sy { sx } else { 0 };
        let end = if row == ey { ex } else { u16::MAX };
        Some((start, end))
    }
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

/// What a tool was asked and what it answered. Kept beside the activity row rather than inside it:
/// the row is the label, this is the receipts, and a call that never reports keeps an empty one.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Payload {
    /// The registered tool name, which decides how the row draws itself.
    pub name: String,
    pub resource: String,
    pub arguments: String,
    pub content: String,
    pub cwd: String,
    pub duration_ms: u64,
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

#[derive(Clone, Debug, PartialEq)]
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
    /// The searchable chooser that a `/` command opens instead of printing a list.
    Picker(crate::widgets::Picker),
    /// A panel of what the runtime already knows. Read-only on purpose: there is no choice to make,
    /// and offering options that do nothing would be worse than showing none.
    Info {
        title: String,
        lines: Vec<String>,
    },
    /// A panel that asks before something irreversible happens. `yes` is the command the host
    /// executes if the operator answers Enter.
    Confirm {
        title: String,
        lines: Vec<String>,
        yes: Box<UiCommand>,
    },
    /// A one-line field for the values a chooser cannot express: a model id the endpoint does not
    /// list, a rule to save. Enter commits it; Esc leaves everything as it was.
    Prompt {
        title: String,
        hint: String,
        editor: Editor,
        action: PromptAction,
    },
}

/// What a text panel's Enter means. A closed set, so a prompt can never submit something the host
/// has no path for.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PromptAction {
    /// The typed text is a model id; the host applies it with the same call the picker uses.
    SetModel,
    /// The typed text is an allow-list rule to save.
    AddAllowRule,
    /// The typed text is "name: description" for a new skill.
    CreateSkill,
    /// The typed text is a path to a skill directory to import.
    ImportSkill,
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
    /// The operator used a verb on a row. The kind and verb together say what to do — a model to
    /// switch to, a session to close, an MCP server to flip — so the host acts instead of the
    /// interface guessing which of several meanings was meant.
    Act {
        kind: crate::widgets::PickerKind,
        verb: crate::widgets::Verb,
        value: String,
        session: SessionId,
    },
    /// Read the configured endpoint's own model list.
    RefreshModels,
    /// Forget one saved allow-list rule, by its index in the list the operator was shown.
    Unallow {
        index: usize,
    },
    /// Delete a skill (moves to trash).
    DeleteSkill {
        name: String,
    },
    /// Create a skill from "name: description" text.
    CreateSkill {
        session: SessionId,
        text: String,
    },
    /// Import a skill from a directory path.
    ImportSkill {
        session: SessionId,
        path: String,
    },
    /// Save one allow-list rule the operator typed, bypassing the removed /allow command.
    AllowRule {
        session: SessionId,
        rule: String,
    },
    /// Stop a background task by id, which is what the tasks panel shows.
    StopTask {
        id: String,
    },
    /// Empty this conversation's transcript in the view.
    Clear {
        session: SessionId,
    },
    /// Forget one remembered key: the memory panel's second verb.
    ForgetMemory {
        scope: String,
        key: String,
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
    pub expanded_thought: bool,
    /// Folds the operator set by clicking a header, keyed by block. `expanded_thought` stays the
    /// default, so `Ctrl+T` still folds or unfolds every block at once.
    pub thought_folds: HashMap<ThoughtKey, bool>,
    /// Tool rows carry their payload hidden. Unlike thinking, a tool row starts closed and a
    /// click opens exactly the one that was clicked.
    pub tool_folds: HashMap<ThoughtKey, bool>,
    /// Receipts for the calls the activity rows name, keyed by call.
    pub payloads: HashMap<ToolCallId, Payload>,
    /// When the turn on screen started, so the header can count while it works. `None` means no
    /// turn is being timed, which is not the same as a turn that took no time.
    pub turn_started_ms: Option<u64>,
    /// How long the last finished turn took. Kept after the turn ends, because the header says so.
    pub turn_elapsed_ms: u64,
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
            expanded_thought: false,
            thought_folds: HashMap::new(),
            tool_folds: HashMap::new(),
            payloads: HashMap::new(),
            turn_started_ms: None,
            turn_elapsed_ms: 0,
            unread: 0,
        }
    }

    /// Whether one thinking block is open. A block the operator clicked answers for itself; every
    /// other block follows the session-wide default.
    pub fn thought_expanded(&self, key: ThoughtKey) -> bool {
        self.thought_folds
            .get(&key)
            .copied()
            .unwrap_or(self.expanded_thought)
    }

    /// Flip one block, leaving the others where they were.
    pub fn toggle_thought(&mut self, key: ThoughtKey) {
        let next = !self.thought_expanded(key);
        self.thought_folds.insert(key, next);
    }

    /// Whether one tool row shows its payload. There is no session-wide default: a payload is
    /// noise until someone asks for it.
    pub fn tool_expanded(&self, key: ThoughtKey) -> bool {
        self.tool_folds.get(&key).copied().unwrap_or(false)
    }

    pub fn toggle_tool(&mut self, key: ThoughtKey) {
        let next = !self.tool_expanded(key);
        self.tool_folds.insert(key, next);
    }

    /// The receipts for one row, if the runtime reported any.
    pub fn payload(&self, call: Option<&ToolCallId>) -> Option<&Payload> {
        call.and_then(|call| self.payloads.get(call))
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

    /// Whether a turn is in flight, which is what the header's clock and spinner read.
    pub fn working(&self) -> bool {
        matches!(
            self.turn,
            TurnState::Streaming | TurnState::Tooling | TurnState::WaitingApproval
        )
    }

    /// Whether anything is visibly in flight. The turn state and the live buffers can disagree for
    /// a moment — text arrives before the state catches up — and the header has to read what is on
    /// screen, not the lagging field, or a working turn renders as an idle one.
    pub fn busy(&self) -> bool {
        self.working() || self.streaming.is_some() || self.streaming_thought.is_some()
    }

    /// Start timing a turn if one is not already being timed. Idempotent: a turn reports itself
    /// many times and must be timed once.
    pub fn mark_working(&mut self) {
        if self.turn_started_ms.is_none() {
            self.turn_started_ms = Some(now_ms());
        }
    }

    /// Stop the clock and keep the duration. A turn that never started is left alone rather than
    /// credited with a duration it did not have.
    pub fn mark_settled(&mut self) {
        if let Some(started) = self.turn_started_ms.take() {
            self.turn_elapsed_ms = now_ms().saturating_sub(started);
        }
    }

    /// Seconds the turn on screen has run, live while it works and final once it is done.
    pub fn turn_seconds(&self) -> u64 {
        match self.turn_started_ms {
            Some(started) => now_ms().saturating_sub(started) / 1000,
            None => self.turn_elapsed_ms / 1000,
        }
    }

    pub fn apply_snapshot(&mut self, snapshot: SessionSnapshot) {
        let was_working = self.working();
        self.title = snapshot.title;
        self.cwd = snapshot.cwd;
        self.model = snapshot.model;
        self.messages = snapshot.messages;
        self.turn = snapshot.turn;
        self.tokens = snapshot.tokens;
        self.updated_ms = snapshot.updated_ms;
        self.agent = snapshot.agent;
        self.queued = snapshot.queued;
        // The turn state is the only authority on whether work is happening. Reading it after the
        // copy makes both transitions fire exactly once: `mark_working` is a no-op when a start is
        // already recorded, and `mark_settled` is a no-op when nothing is being timed.
        let _ = was_working;
        if self.working() {
            self.mark_working();
        } else {
            self.mark_settled();
        }
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
    /// The `+ New` control: its row, then the column range it spans.
    pub new_session_x: Option<(u16, u16, u16)>,
    /// Live `/`, `@`, `!` suggestion popup.
    pub suggestions: Option<SuggestionPopup>,
    /// Workspace root reported by the runtime; used to resolve `@` paths.
    pub workspace_root: PathBuf,
    /// The approval posture the runtime reported, so the status bar labels what it is running under.
    pub mode: PermissionMode,
    pub approval_selection: usize,
    /// Which of the three answers the approval panel has highlighted: allow once, deny, keep.
    pub approval_action: usize,
    pub agent_selection: usize,
    /// The question the runtime is currently blocked on, if any.
    pub pending_question: Option<PendingQuestion>,
    /// Answers typed here and not yet handed to the runtime, as `(request_id, answer)`.
    pub pending_answers: Vec<(String, String)>,
    pub commands: Vec<UiCommand>,
    /// The endpoint's own model list, as last reported. Empty until a listing arrives; `models_error`
    /// is what an endpoint that refused looks like, so an empty picker never means "no models".
    pub models: Vec<String>,
    pub models_error: Option<String>,
    pub models_loading: bool,
    /// The agent chosen in the agent panel, waiting for the verb panel to say what to do with it.
    /// Two panels, one subject: the second panel cannot be answered without the first.
    pub pending_agent: Option<AgentId>,
    /// Where each picker row was drawn, as `(y, visible position)`, and the panel's x range. The
    /// draw writes it and the mouse reads it, which is the only way a click can name a row.
    pub picker_rows: Vec<(u16, usize)>,
    pub picker_x: Option<(u16, u16)>,
    /// Where each thinking block's header was drawn, recomputed each draw. Cleared with the rest of
    /// the hit regions so a stale row can never fold the wrong block.
    pub thought_hits: Vec<ThoughtHit>,
    /// The drag in progress, if any. Screen coordinates, so it survives scrolling underneath.
    pub selection: Option<Selection>,
    /// The last drawn frame as plain text, one string per row. A drag copies from this, because the
    /// cells on screen are exactly what the operator pointed at.
    pub screen: Vec<String>,
    /// Text a copy produced, waiting for the host to hand it to the system clipboard.
    pub clipboard: Option<String>,
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
            approval_action: 0,
            agent_selection: 0,
            pending_question: None,
            pending_answers: Vec::new(),
            commands: Vec::new(),
            models: Vec::new(),
            models_error: None,
            models_loading: false,
            pending_agent: None,
            picker_rows: Vec::new(),
            picker_x: None,
            thought_hits: Vec::new(),
            selection: None,
            screen: Vec::new(),
            clipboard: None,
        }
    }

    /// Remember the frame that was just drawn. The host calls this right after drawing, so a drag
    /// copies the cells the operator actually sees rather than a re-render that may have moved.
    pub fn capture_screen(&mut self, buffer: &ratatui::buffer::Buffer) {
        let area = buffer.area;
        self.screen.clear();
        self.screen.reserve(area.height as usize);
        for y in area.y..area.y.saturating_add(area.height) {
            let mut row = String::with_capacity(area.width as usize);
            for x in area.x..area.x.saturating_add(area.width) {
                row.push_str(buffer[(x, y)].symbol());
            }
            self.screen.push(row);
        }
    }

    /// The selected cells as text, with trailing blanks trimmed and a blank row dropped at each end
    /// of a multi-row drag. `None` when nothing is selected or the drag covers only spaces.
    pub fn selection_text(&self) -> Option<String> {
        let selection = self.selection?;
        if selection.is_click() {
            return None;
        }
        let ((_, sy), (_, ey)) = selection.ordered();
        let mut rows: Vec<String> = Vec::new();
        for y in sy..=ey {
            let Some((start, end)) = selection.span(y) else {
                continue;
            };
            let Some(line) = self.screen.get(y as usize) else {
                continue;
            };
            let len = line.chars().count();
            let start = (start as usize).min(len);
            let end = (end as usize).saturating_add(1).min(len);
            if start >= end {
                continue;
            }
            let slice: String = line.chars().skip(start).take(end - start).collect();
            rows.push(slice.trim_end().to_string());
        }
        let text = rows.join("\n");
        if text.trim().is_empty() {
            None
        } else {
            Some(text)
        }
    }

    /// Ask the host to copy the selection. The interface never touches the clipboard itself.
    pub fn copy_selection(&mut self) {
        if let Some(text) = self.selection_text() {
            let chars = text.chars().count();
            self.clipboard = Some(text);
            self.push_notice(NoticeLevel::Info, format!("copied {chars} chars"));
        }
    }

    pub fn take_clipboard(&mut self) -> Option<String> {
        self.clipboard.take()
    }

    /// The folding header a click landed on, if it landed on one.
    pub fn thought_at(&self, x: u16, y: u16) -> Option<ThoughtKey> {
        self.fold_at(x, y).map(|(key, _)| key)
    }

    /// The same hit, saying whether it belongs to a tool row or a thinking block.
    pub fn fold_at(&self, x: u16, y: u16) -> Option<(ThoughtKey, bool)> {
        self.thought_hits
            .iter()
            .find(|hit| hit.row == y && x >= hit.x_start && x <= hit.x_end)
            .map(|hit| (hit.key, hit.tool))
    }

    /// Open the one-line field. The editor starts empty; the hint says what Enter will do, because a
    /// text box that does not say what it commits is a trap.
    pub fn open_prompt(
        &mut self,
        title: impl Into<String>,
        hint: impl Into<String>,
        action: PromptAction,
    ) {
        self.overlay = Some(Overlay::Prompt {
            title: title.into(),
            hint: hint.into(),
            editor: Editor::new(),
            action,
        });
    }

    /// Show a chooser. One overlay at a time: opening a panel replaces whatever was open, because
    /// two menus competing for the same keys is how an interface loses an instruction.
    pub fn open_picker(&mut self, picker: crate::widgets::Picker) {
        self.overlay = Some(Overlay::Picker(picker));
    }

    pub fn open_info(&mut self, title: impl Into<String>, lines: Vec<String>) {
        self.overlay = Some(Overlay::Info {
            title: title.into(),
            lines,
        });
    }

    pub fn open_confirm(&mut self, title: impl Into<String>, lines: Vec<String>, yes: UiCommand) {
        self.overlay = Some(Overlay::Confirm {
            title: title.into(),
            lines,
            yes: Box::new(yes),
        });
    }

    /// The model chooser, built from the endpoint's listing and the model this session is on. While
    /// the listing is in flight the rows are absent and the note says so, which is the difference
    /// between "still reading" and "this endpoint serves nothing".
    pub fn model_picker(&self) -> crate::widgets::Picker {
        let current = self
            .display_session()
            .and_then(|session| session.model.clone());
        let items: Vec<crate::widgets::PickerItem> = self
            .models
            .iter()
            .map(|name| {
                let item =
                    crate::widgets::PickerItem::new(name.clone(), String::new(), name.clone());
                if current.as_deref() == Some(name.as_str()) {
                    item.as_current()
                } else {
                    item
                }
            })
            .collect();
        let picker = crate::widgets::Picker::new(crate::widgets::PickerKind::Model, "Model", items);
        match (&self.models_error, self.models_loading, current) {
            (Some(error), _, _) => picker.with_note(error.clone()),
            (None, true, _) => picker.with_note("reading the endpoint's model list…"),
            (None, false, Some(model)) => picker.with_note(format!("now on {model}")),
            (None, false, None) => picker.with_note("no model is set for this session yet"),
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
                    // A request is the moment the clock starts, even before the first token.
                    view.mark_working();
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
                    view.mark_working();
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
                resource,
                arguments,
                cwd,
            } => {
                if let Some(view) = self.session_by_id(&session) {
                    view.turn = TurnState::Tooling;
                    view.mark_working();
                    view.payloads.insert(
                        call.clone(),
                        Payload {
                            name: name.clone(),
                            resource,
                            arguments,
                            content: String::new(),
                            cwd,
                            duration_ms: 0,
                        },
                    );
                    // A call that asks for approval announces itself twice. It is one piece of work
                    // and gets one row: the second announcement only sharpens its label.
                    match view
                        .activity
                        .iter_mut()
                        .rev()
                        .find(|a| a.call.as_ref() == Some(&call))
                    {
                        Some(activity) => activity.text = format!("{name} · {summary}"),
                        None => view.push_activity(Activity {
                            kind: ActivityKind::Tool,
                            text: format!("{name} · {summary}"),
                            call: Some(call),
                            ok: None,
                            expanded: false,
                        }),
                    }
                }
            }
            RuntimeEvent::ToolFinished {
                session,
                call,
                name,
                ok,
                summary,
                content,
                resource,
                arguments,
                cwd,
                duration_ms,
            } => {
                if let Some(view) = self.session_by_id(&session) {
                    view.payloads.insert(
                        call.clone(),
                        Payload {
                            name: name.clone(),
                            resource,
                            arguments,
                            content,
                            cwd,
                            duration_ms,
                        },
                    );
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
                    view.mark_settled();
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
                self.approval_action = 0;
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
            RuntimeEvent::ModelsListed { models, error } => {
                self.models_loading = false;
                self.models_error = error;
                self.models = models;
                // A chooser that was opened while the listing was in flight is filled here, once.
                // It is the only place the two halves meet, which is why "still reading" cannot
                // be mistaken for "this endpoint serves nothing".
                let showing_models = matches!(
                    &self.overlay,
                    Some(Overlay::Picker(picker))
                        if picker.kind == crate::widgets::PickerKind::Model
                );
                if showing_models {
                    let picker = self.model_picker();
                    self.overlay = Some(Overlay::Picker(picker));
                }
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

#[cfg(test)]
mod tests {
    use super::*;
    use vikram_runtime::types::SessionSnapshot;

    fn app_on(model: Option<&str>) -> App {
        let mut app = App::new(false);
        app.sessions
            .push(SessionView::from_snapshot(SessionSnapshot {
                id: SessionId::generate(),
                title: "one".to_owned(),
                cwd: "/work".into(),
                model: model.map(str::to_owned),
                messages: Vec::new(),
                turn: TurnState::Idle,
                tokens: Default::default(),
                created_ms: 0,
                updated_ms: 0,
                agent: None,
                queued: 0,
                project_id: None,
                status: "active".to_owned(),
            }));
        app
    }

    #[test]
    fn a_model_panel_that_is_waiting_says_so_and_is_filled_when_the_listing_lands() {
        let mut app = app_on(Some("qwen3-coder"));
        app.models_loading = true;
        app.open_picker(app.model_picker());
        let waiting = app.model_picker();
        assert!(waiting.items.is_empty());
        assert!(
            waiting.note.as_deref().unwrap().contains("reading"),
            "an unanswered endpoint must not look like an empty one: {:?}",
            waiting.note
        );

        app.on_event(RuntimeEvent::ModelsListed {
            models: vec!["gpt-5.2".to_owned(), "qwen3-coder".to_owned()],
            error: None,
        });

        let Some(Overlay::Picker(picker)) = app.overlay.as_ref() else {
            panic!("the panel that was open has to be the one that is filled");
        };
        assert_eq!(picker.items.len(), 2);
        assert!(
            picker
                .items
                .iter()
                .find(|item| item.value == "qwen3-coder")
                .is_some_and(|item| item.current),
            "the model in force is marked, not guessed"
        );
        assert!(!app.models_loading);
    }

    #[test]
    fn an_endpoint_that_refuses_is_reported_as_the_reason_rather_than_as_no_models() {
        let mut app = app_on(None);
        app.models_loading = true;
        app.on_event(RuntimeEvent::ModelsListed {
            models: Vec::new(),
            error: Some("connection refused".to_owned()),
        });
        let picker = app.model_picker();
        assert!(picker.items.is_empty());
        assert_eq!(picker.note.as_deref(), Some("connection refused"));
        assert!(!app.models_loading);
    }

    #[test]
    fn a_listing_that_arrives_when_no_panel_is_open_changes_nothing_visible() {
        let mut app = app_on(Some("qwen3-coder"));
        app.on_event(RuntimeEvent::ModelsListed {
            models: vec!["gpt-5.2".to_owned()],
            error: None,
        });
        assert!(app.overlay.is_none());
        assert_eq!(app.models, vec!["gpt-5.2".to_owned()]);
    }
}

#[cfg(test)]
mod selection_and_folds {
    use super::*;

    fn app_with_rows(rows: &[&str]) -> App {
        let mut app = App::new(false);
        app.screen = rows.iter().map(|row| (*row).to_owned()).collect();
        app
    }

    #[test]
    fn a_drag_copies_the_cells_it_covered_and_nothing_else() {
        let mut app = app_with_rows(&["hello world", "second line here"]);
        app.selection = Some(Selection {
            anchor: (6, 0),
            cursor: (3, 1),
        });
        assert_eq!(app.selection_text().as_deref(), Some("world\nseco"));
    }

    #[test]
    fn a_drag_upwards_reads_the_same_as_the_same_drag_downwards() {
        let up = Selection {
            anchor: (6, 0),
            cursor: (3, 1),
        };
        let down = Selection {
            anchor: (3, 1),
            cursor: (6, 0),
        };
        assert_eq!(up.ordered(), down.ordered());
        let mut app = app_with_rows(&["hello world", "second line here"]);
        app.selection = Some(up);
        let one = app.selection_text();
        app.selection = Some(down);
        assert_eq!(one, app.selection_text());
    }

    #[test]
    fn a_press_that_never_moved_copies_nothing() {
        let mut app = app_with_rows(&["hello"]);
        app.selection = Some(Selection::new((2, 0)));
        assert!(app.selection_text().is_none());
        app.copy_selection();
        assert!(app.take_clipboard().is_none());
    }

    #[test]
    fn a_clicked_header_folds_only_its_own_block() {
        let mut app = App::new(false);
        app.thought_hits = vec![
            ThoughtHit {
                row: 4,
                x_start: 0,
                x_end: 8,
                key: (0, 0),
                tool: false,
            },
            ThoughtHit {
                row: 9,
                x_start: 0,
                x_end: 8,
                key: (2, 1),
                tool: true,
            },
        ];
        assert_eq!(app.thought_at(3, 4), Some((0, 0)));
        assert_eq!(
            app.thought_at(3, 7),
            None,
            "the row beyond the label is text"
        );
        assert_eq!(app.thought_at(3, 9), Some((2, 1)));
        assert_eq!(
            app.fold_at(3, 9),
            Some(((2, 1), true)),
            "a tool row knows it is a tool"
        );
        assert_eq!(app.fold_at(3, 4), Some(((0, 0), false)));

        let mut session = SessionView::from_snapshot(SessionSnapshot {
            id: SessionId::generate(),
            title: "s".to_owned(),
            cwd: PathBuf::from("/"),
            model: None,
            messages: Vec::new(),
            turn: TurnState::Idle,
            tokens: Default::default(),
            created_ms: 0,
            updated_ms: 0,
            agent: None,
            queued: 0,
            project_id: None,
            status: "active".to_owned(),
        });
        session.toggle_thought((0, 0));
        assert!(session.thought_expanded((0, 0)));
        assert!(
            !session.thought_expanded((2, 1)),
            "the other block is untouched"
        );
    }
}
