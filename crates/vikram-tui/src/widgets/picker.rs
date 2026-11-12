//! The searchable picker: the one overlay every `/` command that *chooses* something opens.
//!
//! A picker is not a list of strings. Each row carries the value the runtime needs (a model name, a
//! bot profile, a path) beside the text that identifies it, so choosing a row is an instruction the
//! runtime can act on rather than a word to paste into the input box. The filter, the selection and
//! the scroll window are all derived from those fields, which is what lets the same widget serve
//! `/model`, `/bots`, `/roster`, `/skills`, `/files`, `/mcp` and the command palette itself.

use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use unicode_width::UnicodeWidthStr;

use crate::theme::Theme;

/// How many rows are drawn at once. Anything more has to scroll, and the selection is what is kept
/// in view.
const WINDOW: usize = 12;

/// One thing a panel can do with the row under the cursor.
///
/// This is what keeps the panels from being one generic list: a model panel can *type a name*, a
/// session panel can *rename* and *close*, a memory panel can *forget*, an MCP panel can *flip
/// visibility*. If the runtime cannot do it, the verb is not declared and no key offers it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Verb {
    /// Enter: the row is the answer.
    Choose,
    /// Open a text field, because the value is not on the list.
    Prompt,
    /// Forget the row (allow-list rule, memory key).
    Remove,
    /// Flip the row's state (visibility, setting).
    Toggle,
    /// Stop the row's work (background task).
    Stop,
    /// Move the row one slot earlier in its list.
    MoveUp,
    /// Move the row one slot later in its list.
    MoveDown,
    /// Rename the row's subject (session).
    Rename,
    /// Close the row's subject (session).
    Close,
    /// Start a new row's subject (bot from a profile).
    New,
}

/// What a picker is for, which also decides what choosing a row means. A picker without a kind is
/// only a list.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PickerKind {
    Model,
    Provider,
    Mode,
    Allow,
    Bot,
    HiveRoster,
    Skill,
    File,
    Mcp,
    Command,
    Shell,
    /// A conversation to switch to.
    Session,
    /// A background task to stop.
    Task,
    /// Messages waiting behind the running turn: the steer row and the queue.
    Pending,
    /// A key the model remembered.
    Memory,
    /// An interface preference that takes effect immediately.
    Setting,
    /// Permissions policy and allow-list rules
    Permissions,
    /// Installed and loaded plugins
    Plugins,
    /// Available tools
    Tools,
    /// Runtime and session status
    Status,
    /// Token usage for the session
    Usage,
    /// Context window usage
    Context,
    /// Local prerequisite checks
    Doctor,
    /// Version and workspace info
    Version,
    /// Approvals waiting for your answer
    Approvals,
    /// Key bindings and shortcuts
    Keys,
    /// Run git diff
    Diff,
    /// Run test suite
    Test,
    /// Build workspace
    Build,
    /// Clear conversation
    Clear,
    /// Start new session
    NewSession,
    /// Quit VIKRAM
    Exit,
    /// Rename current session
    RenameSession,
    /// Close current session
    CloseSession,
    /// Scheduled jobs and the daemon that runs them
    Background,
}

impl PickerKind {
    /// The keys this panel answers, in the order the footer shows them. The first entry is Enter.
    ///
    /// Every non-Enter verb is a Ctrl+letter on purpose: printable keys belong to the filter, so a
    /// `d` you type to narrow a list can never also delete something.
    pub fn verbs(self) -> &'static [(Verb, char, &'static str)] {
        match self {
            Self::Model => &[
                (Verb::Choose, '\n', "choose"),
                (Verb::Prompt, 't', "type a name"),
            ],
            Self::Mode => &[(Verb::Choose, '\n', "choose")],
            Self::Allow => &[
                (Verb::Remove, '\n', "forget"),
                (Verb::Prompt, 'a', "add a rule"),
            ],
            Self::Bot => &[
                (Verb::Choose, '\n', "select"),
                (Verb::New, 'n', "spawn"),
                (Verb::Prompt, 'c', "new bot"),
                (Verb::Rename, 'r', "rename"),
                (Verb::Remove, 'x', "delete"),
                (Verb::Toggle, 't', "pause/resume"),
            ],
            Self::Session => &[
                (Verb::Choose, '\n', "switch"),
                (Verb::Rename, 'r', "rename"),
                (Verb::Close, 'x', "close"),
                (Verb::New, 'n', "new session"),
            ],
            Self::Task => &[(Verb::Stop, '\n', "stop")],
            Self::Pending => &[
                (Verb::Prompt, 'e', "edit"),
                (Verb::Remove, 'd', "delete"),
                (Verb::Toggle, 'f', "steer/queue"),
                (Verb::MoveUp, 'k', "earlier"),
                (Verb::MoveDown, 'j', "later"),
            ],
            Self::Memory => &[
                (Verb::Choose, '\n', "read"),
                (Verb::Remove, 'd', "forget"),
                (Verb::Prompt, 'a', "remember"),
            ],
            Self::Plugins => &[
                (Verb::Choose, '\n', "inspect"),
                (Verb::Toggle, 't', "enable/disable"),
                (Verb::Prompt, 'i', "install path"),
                (Verb::Remove, 'x', "uninstall"),
            ],
            Self::Setting => &[(Verb::Toggle, '\n', "flip")],
            Self::Mcp => &[(Verb::Toggle, '\n', "flip visibility")],
            Self::Skill => &[
                (Verb::Choose, '\n', "read"),
                (Verb::Toggle, 't', "enable/disable"),
                (Verb::New, 'n', "new skill"),
                (Verb::Prompt, 'i', "import path"),
                (Verb::Rename, 'r', "rename"),
                (Verb::Remove, 'd', "delete"),
            ],
            Self::Tools => &[
                (Verb::Choose, '\n', "inspect"),
                (Verb::Toggle, 't', "enable/disable"),
            ],
            Self::File => &[(Verb::Choose, '\n', "reference it")],
            Self::Shell => &[(Verb::Choose, '\n', "run it")],
            Self::Command => &[(Verb::Choose, '\n', "open")],
            Self::Provider => &[(Verb::Choose, '\n', "switch")],
            // The roster is the thing the operator edits, not a list to read: the runtime can
            // create a hive, add a member, drop a member and delete the hive, so each gets a key.
            Self::HiveRoster => &[
                (Verb::Choose, '\n', "open"),
                (Verb::New, 'n', "new hive"),
                (Verb::Remove, 'd', "remove member"),
                (Verb::Close, 'x', "delete hive"),
            ],
            Self::Permissions
            | Self::Status
            | Self::Usage
            | Self::Context
            | Self::Doctor
            | Self::Version
            | Self::Approvals
            | Self::Keys
            | Self::Diff
            | Self::Test
            | Self::Build
            | Self::Clear
            | Self::NewSession
            | Self::Exit
            | Self::RenameSession
            | Self::CloseSession => &[(Verb::Choose, '\n', "open")],
            Self::Background => &[(Verb::Toggle, '\n', "pause/resume")],
        }
    }

    /// The verb a Ctrl+letter names, if this panel declares it.
    pub fn verb_for_key(self, key: char) -> Option<Verb> {
        self.verbs()
            .iter()
            .find(|(_, k, _)| k.eq_ignore_ascii_case(&key))
            .map(|(verb, _, _)| *verb)
    }

    /// Whether the verb acts on the panel itself rather than on the row under the cursor.
    ///
    /// "New bot" has to work on an empty list: a panel that only offers creation once something
    /// already exists can never create the first one. The same goes for every verb that opens a
    /// text field, because what it reads comes from the operator, not from a row.
    pub fn panel_verb(self, verb: Verb) -> bool {
        matches!(
            (self, verb),
            (Self::Bot, Verb::New | Verb::Prompt)
                | (Self::Session, Verb::New)
                | (Self::HiveRoster, Verb::New)
                | (Self::Skill, Verb::New | Verb::Prompt)
                | (Self::Allow, Verb::Prompt)
                | (Self::Memory, Verb::Prompt)
                | (Self::Plugins, Verb::Prompt)
                | (Self::Model, Verb::Prompt)
        )
    }

    pub fn primary_verb(self) -> Verb {
        self.verbs()[0].0
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Model => "model",
            Self::Provider => "provider",
            Self::Mode => "mode",
            Self::Allow => "allow",
            Self::Shell => "shell",
            Self::Bot => "bot",
            Self::HiveRoster => "hive",
            Self::Skill => "skill",
            Self::File => "file",
            Self::Mcp => "mcp",
            Self::Session => "session",
            Self::Task => "task",
            Self::Pending => "pending",
            Self::Memory => "memory",
            Self::Setting => "setting",
            Self::Command => "command",
            Self::Permissions => "permissions",
            Self::Plugins => "plugins",
            Self::Tools => "tools",
            Self::Status => "status",
            Self::Usage => "usage",
            Self::Context => "context",
            Self::Doctor => "doctor",
            Self::Version => "version",
            Self::Approvals => "approvals",
            Self::Keys => "keys",
            Self::Diff => "diff",
            Self::Test => "test",
            Self::Build => "build",
            Self::Clear => "clear",
            Self::NewSession => "new",
            Self::Exit => "exit",
            Self::RenameSession => "rename",
            Self::CloseSession => "close",
            Self::Background => "background",
        }
    }
}

/// One row. `value` is what gets handed to the runtime; `label` and `detail` are what the operator
/// reads, and neither is a substitute for the other.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PickerItem {
    pub label: String,
    pub detail: String,
    pub value: String,
    /// Already in force — the picker marks it so the operator can see the current answer.
    pub current: bool,
}

impl PickerItem {
    pub fn new(
        label: impl Into<String>,
        detail: impl Into<String>,
        value: impl Into<String>,
    ) -> Self {
        Self {
            label: label.into(),
            detail: detail.into(),
            value: value.into(),
            current: false,
        }
    }

    pub fn as_current(mut self) -> Self {
        self.current = true;
        self
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Picker {
    pub kind: PickerKind,
    pub title: String,
    /// The whole list, in the order it should be shown. Filtering never reorders it.
    pub items: Vec<PickerItem>,
    pub query: String,
    /// Index into [`Picker::visible`], not into `items`: the operator selects what they can see.
    pub selected: usize,
    /// Why the list is short or empty — an endpoint that would not answer says so here rather than
    /// leaving the operator to guess whether there is nothing to pick.
    pub note: Option<String>,
}

impl Picker {
    pub fn new(kind: PickerKind, title: impl Into<String>, items: Vec<PickerItem>) -> Self {
        Self {
            kind,
            title: title.into(),
            items,
            query: String::new(),
            selected: 0,
            note: None,
        }
    }

    pub fn with_note(mut self, note: impl Into<String>) -> Self {
        self.note = Some(note.into());
        self
    }

    /// Indices of the rows the query matches, best match first. A query matches the label or the
    /// value, case-insensitively: an exact name, then a prefix, then a substring, then a
    /// subsequence of the label — the last one is what makes `mdl` find `model`.
    ///
    /// Rank matters more than it looks. `mode` is a substring of `model`, so a plain containment
    /// filter would put `/model` under the cursor for someone asking for `/mode`; an exact name
    /// always wins, and rows that rank the same keep their list order.
    pub fn visible(&self) -> Vec<usize> {
        let query = self.query.trim().to_lowercase();
        if query.is_empty() {
            return (0..self.items.len()).collect();
        }
        let mut ranked: Vec<(u8, usize)> = self
            .items
            .iter()
            .enumerate()
            .filter_map(|(index, item)| {
                let label = item.label.to_lowercase();
                let value = item.value.to_lowercase();
                let bare = label.trim_start_matches('/');
                let rank = if bare == query || value == query {
                    0
                } else if bare.starts_with(&query) || value.starts_with(&query) {
                    1
                } else if label.contains(&query) || value.contains(&query) {
                    2
                } else if subsequence(&query, &label) {
                    3
                } else {
                    return None;
                };
                Some((rank, index))
            })
            .collect();
        ranked.sort_by_key(|(rank, index)| (*rank, *index));
        ranked.into_iter().map(|(_, index)| index).collect()
    }

    /// The row under the cursor, if any. `None` is a real answer: an empty filter result or an empty
    /// list cannot be chosen, and the caller must not invent one.
    pub fn selected_item(&self) -> Option<&PickerItem> {
        let visible = self.visible();
        visible
            .get(self.selected)
            .and_then(|index| self.items.get(*index))
    }

    pub fn move_by(&mut self, delta: isize) {
        let visible = self.visible();
        if visible.is_empty() {
            self.selected = 0;
            return;
        }
        let len = visible.len() as isize;
        let next = (self.selected as isize + delta).rem_euclid(len);
        self.selected = next as usize;
    }

    /// Put the cursor on a visible position, which is what a click names. Out-of-range is ignored
    /// rather than clamped: a click that lands on a row that is not drawn chose nothing.
    pub fn select_position(&mut self, position: usize) -> bool {
        if position < self.visible().len() {
            self.selected = position;
            true
        } else {
            false
        }
    }

    /// Put the cursor on the row carrying `value`. A chooser opens on what is in force rather than
    /// on the first row, which is the difference between a list of options and a list with the
    /// current one buried alphabetically in the middle of four hundred others.
    pub fn select_value(&mut self, value: &str) -> bool {
        let visible = self.visible();
        match visible.iter().position(|index| {
            self.items
                .get(*index)
                .is_some_and(|item| item.value == value)
        }) {
            Some(position) => {
                self.selected = position;
                true
            }
            None => false,
        }
    }

    pub fn type_str(&mut self, text: &str) {
        self.query.push_str(text);
        self.selected = 0;
    }

    pub fn backspace(&mut self) {
        self.query.pop();
        self.selected = 0;
    }

    /// The scroll window: which visible indices are drawn, and where the first one sits.
    pub fn window(&self) -> (Vec<usize>, usize) {
        let visible = self.visible();
        if visible.len() <= WINDOW {
            return (visible, 0);
        }
        let start = self
            .selected
            .saturating_sub(WINDOW.saturating_sub(2))
            .min(visible.len() - WINDOW);
        (visible[start..start + WINDOW].to_vec(), start)
    }
}

fn subsequence(needle: &str, haystack: &str) -> bool {
    let mut chars = haystack.chars();
    needle.chars().all(|needle| chars.any(|hay| hay == needle))
}

pub fn picker_title(picker: &Picker) -> String {
    picker.title.clone()
}

/// One clickable button in a picker's footer: the verb it performs and where it was drawn.
///
/// The footer is not a hint line. Every verb the panel answers to is a button, so an operator with
/// a mouse never has to learn a chord to create, rename or delete anything.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct VerbChip {
    pub verb: Verb,
    /// Index into [`PickerBody::lines`] of the line the chip sits on.
    pub line: usize,
    /// Column range inside that line, in display cells from the line's first cell.
    pub start: u16,
    pub end: u16,
}

/// What a picker draws, and where the rows and buttons landed in what it drew.
///
/// The row offset travels with the lines because a click has to name a row: the caller that draws
/// owns the only copy of the header layout, and re-deriving "filter line, maybe a note, maybe a
/// rule" somewhere else is how a click ends up one row off.
pub struct PickerBody {
    pub lines: Vec<Line<'static>>,
    /// Index into `lines` of the first clickable row.
    pub first_row: usize,
    pub rows: usize,
    /// The footer buttons, with the columns each one occupies.
    pub verbs: Vec<VerbChip>,
}

/// Leading spaces every line of a panel body carries. One value, so the filter, the rows and the
/// footer align on the same column instead of each choosing its own.
const PAD: usize = 2;

/// Width of the cursor column in front of a label. Reserved on every row, drawn on one: a list
/// whose rows shift sideways as the cursor moves is a list that cannot be scanned.
const MARKER: usize = 2;

/// Gap between the label column and the detail column.
const GAP: usize = 2;

/// What marks the row already in force. Right-aligned, so it is a column rather than a suffix that
/// lands somewhere different on every line.
const CURRENT: &str = "(current)";

/// The most width a footnote — a row with no detail — is worth asking for. A sentence about where a
/// key lives is a remark, not a column: past this it is clipped rather than making every chooser
/// that carries one as wide as the terminal. 68 inside plus the frame and its padding is the 72 the
/// panel floors at, so a long footnote costs nothing and a short one still lets the panel be narrow.
const FOOTNOTE_CAP: usize = 68;

/// Room the detail column is never allowed to lose to the label column. A row that cannot show its
/// detail has shown nothing the operator can choose between.
const DETAIL_MIN: usize = 20;

/// The dialog body, laid out in columns: a filter line carrying the count, a rule, the visible rows
/// with the cursor banded on one of them, then the keys. Nothing here is a placeholder — an empty
/// list says the filter matched nothing, and an empty source list says there is nothing to choose.
///
/// `width` is the inside of the frame. It is what makes the detail column line up and the suffix a
/// column instead of an accident of how long each label happens to be.
pub fn picker_body(picker: &Picker, theme: &Theme, width: u16) -> PickerBody {
    let width = (width as usize).max(PAD + MARKER + GAP + 24);
    let cursor = if theme.ascii { ">" } else { "▸" };
    let bullet = if theme.ascii { "*" } else { "•" };
    let rule_char = if theme.ascii { '-' } else { '─' };
    let caret = if theme.ascii { "_" } else { "▌" };
    let mut lines: Vec<Line<'static>> = Vec::new();

    let visible = picker.visible();
    let (rows, start) = picker.window();

    // The label column is as wide as the widest label that actually has something beside it,
    // capped so a 60-character model name cannot push every detail out of the panel. Rows with no
    // detail are footnotes, not columns: they are measured against the full width instead, because
    // letting one of them into this measurement collapses every detail on the panel into `$…`.
    // Space the label column may not take: what the detail actually needs, not what it might need.
    // A panel whose details are all two characters (`ok`, `info`) would otherwise hand twenty
    // columns to a column that holds two and clip every label instead.
    let detail_need = rows
        .iter()
        .map(|index| &picker.items[*index])
        .filter(|item| !item.detail.is_empty())
        .map(|item| UnicodeWidthStr::width(item.detail.as_str()))
        .max()
        .unwrap_or(0)
        .clamp(4, DETAIL_MIN);
    let label_cap = width.saturating_sub(PAD + MARKER + GAP + CURRENT.len() + 1 + detail_need);
    let label_width = rows
        .iter()
        .map(|index| &picker.items[*index])
        .filter(|item| !item.detail.is_empty())
        .map(|item| UnicodeWidthStr::width(item.label.as_str()))
        .max()
        .unwrap_or(0)
        .min(label_cap.max(12));

    // ---- filter line, with the count on the right ------------------------------------------
    let counter = format!(
        "{} of {}",
        if visible.is_empty() {
            0
        } else {
            picker.selected + 1
        },
        visible.len()
    );
    let typed = format!("{}filter {}", " ".repeat(PAD), picker.query);
    let typed_width = UnicodeWidthStr::width(typed.as_str()) + 1;
    let gap = width.saturating_sub(typed_width + counter.len()).max(1);
    lines.push(Line::from(vec![
        Span::styled(format!("{}filter ", " ".repeat(PAD)), theme.muted()),
        Span::styled(
            picker.query.to_string(),
            Style::default()
                .fg(theme.accent)
                .add_modifier(Modifier::BOLD),
        ),
        Span::styled(caret, theme.muted()),
        Span::styled(" ".repeat(gap), theme.muted()),
        Span::styled(counter, theme.muted()),
    ]));

    if let Some(note) = &picker.note {
        lines.push(Line::from(vec![
            Span::styled(" ".repeat(PAD), theme.muted()),
            Span::styled(
                clip_to(note, width.saturating_sub(PAD), theme.ascii),
                Style::default().fg(theme.accent_dim),
            ),
        ]));
    }

    lines.push(Line::from(Span::styled(
        format!(
            "{}{}",
            " ".repeat(PAD),
            rule_char.to_string().repeat(width.saturating_sub(PAD))
        ),
        Style::default().fg(theme.border),
    )));

    // ---- rows ------------------------------------------------------------------------------
    let first_row = lines.len();
    if picker.items.is_empty() {
        lines.push(Line::from(Span::styled(
            format!("{}{bullet} nothing to choose yet", " ".repeat(PAD)),
            theme.secondary(),
        )));
    } else if rows.is_empty() {
        lines.push(Line::from(Span::styled(
            format!(
                "{}{bullet} nothing matches `{}`",
                " ".repeat(PAD),
                picker.query
            ),
            theme.secondary(),
        )));
    } else {
        for (position, index) in rows.iter().enumerate() {
            let item = &picker.items[*index];
            let on_cursor = start + position == picker.selected;
            let marker = if on_cursor { cursor } else { " " };

            // A row with a detail is a column. A row without one is a footnote — the `!` lines and
            // the model names — and it is allowed the whole width instead of being cut off at the
            // column the detailed rows agreed on.
            let footnote = item.detail.is_empty();
            let label_width_here = if footnote {
                width
                    .saturating_sub(PAD + MARKER)
                    .saturating_sub(if item.current { CURRENT.len() + 1 } else { 0 })
            } else {
                label_width
            };
            let label = clip_to(&item.label, label_width_here, theme.ascii);
            let label_pad = label_width_here.saturating_sub(UnicodeWidthStr::width(label.as_str()));
            let mut used = PAD + MARKER + label_width_here;
            let head_style = if on_cursor {
                Style::default()
                    .fg(theme.accent)
                    .add_modifier(Modifier::BOLD)
            } else {
                Style::default().fg(theme.text)
            };
            let mut spans = vec![
                Span::styled(" ".repeat(PAD), head_style),
                Span::styled(
                    format!("{marker} "),
                    if on_cursor { head_style } else { theme.muted() },
                ),
                Span::styled(label, head_style),
                Span::styled(" ".repeat(label_pad), head_style),
            ];

            if !item.detail.is_empty() {
                let room = width.saturating_sub(used + GAP + CURRENT.len() + 1);
                let detail = clip_to(&item.detail, room, theme.ascii);
                let detail_width = UnicodeWidthStr::width(detail.as_str());
                if detail_width > 0 {
                    // Faint text on the cursor band turns to mud; the band already says where the
                    // cursor is, so the detail keeps its own weight instead of dropping a shade.
                    let detail_style = if on_cursor {
                        Style::default().fg(theme.dim)
                    } else {
                        theme.muted()
                    };
                    spans.push(Span::styled(" ".repeat(GAP), detail_style));
                    spans.push(Span::styled(detail, detail_style));
                    used += GAP + detail_width;
                }
            }
            if item.current {
                let pad = width.saturating_sub(used + 1 + CURRENT.len()).max(1);
                spans.push(Span::styled(" ".repeat(pad), theme.muted()));
                spans.push(Span::styled(CURRENT, Style::default().fg(theme.accent_dim)));
                used += pad + CURRENT.len();
            }
            if on_cursor && used < width {
                // The band has to reach the frame, not stop at the last word: a highlight that ends
                // mid-row reads as text someone painted, not as a row someone selected.
                spans.push(Span::styled(" ".repeat(width - used), theme.selected()));
            }

            // The cursor row is banded across the whole width rather than recoloured: a colour
            // change on one word is easy to lose in a list of long names, and the band also makes
            // it obvious where the row starts and ends.
            let mut line = Line::from(spans);
            if on_cursor {
                line = line.style(theme.selected());
            }
            lines.push(line);
        }
    }
    let rows_drawn = lines.len() - first_row;

    // ---- footer: one button per verb --------------------------------------------------------
    lines.push(Line::from(Span::styled(
        format!(
            "{}{}",
            " ".repeat(PAD),
            rule_char.to_string().repeat(width.saturating_sub(PAD))
        ),
        Style::default().fg(theme.border),
    )));

    // Buttons, not a key legend. Each verb is drawn as a bracketed control carrying its chord, and
    // the columns it occupies are handed back so the mouse can press it. A button that does not fit
    // the row starts the next one rather than being cut in half: half a button is not clickable.
    let mut verbs: Vec<VerbChip> = Vec::new();
    let mut spans: Vec<Span<'static>> = vec![Span::styled(" ".repeat(PAD), theme.muted())];
    let mut col = PAD;
    let chip_style = Style::default()
        .fg(theme.accent)
        .add_modifier(Modifier::BOLD);
    for (verb, key, label) in picker.kind.verbs() {
        let text = if *key == '\n' {
            format!("[enter {label}]")
        } else {
            format!("[ctrl+{key} {label}]")
        };
        let text_width = UnicodeWidthStr::width(text.as_str());
        if col > PAD && col + text_width > width {
            lines.push(Line::from(std::mem::take(&mut spans)));
            spans.push(Span::styled(" ".repeat(PAD), theme.muted()));
            col = PAD;
        }
        let start = col;
        let room = width.saturating_sub(start);
        let drawn = clip_to(&text, room, theme.ascii);
        let drawn_width = UnicodeWidthStr::width(drawn.as_str());
        spans.push(Span::styled(drawn, chip_style));
        col += drawn_width;
        // A clipped label is still a button, but a button with nothing drawn is not.
        if drawn_width > 0 {
            verbs.push(VerbChip {
                verb: *verb,
                line: lines.len(),
                start: start as u16,
                end: col as u16,
            });
        }
        spans.push(Span::styled(" ".to_owned(), theme.muted()));
        col += 1;
    }
    lines.push(Line::from(spans));

    // Hints on their own line: a panel that clips its own footer has hidden the one thing the
    // operator needs to know.
    lines.push(Line::from(Span::styled(
        format!(
            "{}{}",
            " ".repeat(PAD),
            clip_to(
                &format!(
                    "type to filter  ·  {} move  ·  ctrl+u/d page  ·  esc closes",
                    if theme.ascii { "up/down" } else { "↑↓" }
                ),
                width.saturating_sub(PAD),
                theme.ascii
            )
        ),
        theme.muted(),
    )));

    PickerBody {
        lines,
        first_row,
        rows: rows_drawn,
        verbs,
    }
}

/// The body on its own, for a caller that only draws it. Clicks need [`picker_body`], which also
/// says where the rows landed.
pub fn picker_lines(picker: &Picker, theme: &Theme) -> Vec<Line<'static>> {
    picker_body(picker, theme, 72).lines
}

/// The inside width this picker needs to show its widest row without clipping anything. The caller
/// adds the frame and padding it draws around this.
///
/// A dialog sized by a fixed number clips the one column that tells two rows apart, and the operator
/// cannot then read what they are choosing between. Measured from the rows themselves.
pub fn wanted_width(picker: &Picker) -> u16 {
    // The label column and the detail column are measured apart, and a row with no detail is a
    // footnote that runs the full width — the same rule `picker_body` lays out with. One flat
    // maximum instead let a single long footnote stretch every chooser to the edge of the screen.
    let columns = picker.items.iter().filter(|item| !item.detail.is_empty());
    let label = columns
        .clone()
        .map(|item| UnicodeWidthStr::width(item.label.as_str()))
        .max()
        .unwrap_or(0);
    let detail = columns
        .map(|item| UnicodeWidthStr::width(item.detail.as_str()) + GAP)
        .max()
        .unwrap_or(0);
    let footnote = picker
        .items
        .iter()
        .filter(|item| item.detail.is_empty())
        .map(|item| UnicodeWidthStr::width(item.label.as_str()) + PAD + MARKER)
        .max()
        .unwrap_or(0)
        .min(FOOTNOTE_CAP);
    (PAD + MARKER + label + detail + CURRENT.len() + 1).max(footnote) as u16
}

/// Cut a cell to the width the column allows, counting display columns rather than characters so a
/// wide glyph does not overrun its neighbour.
fn clip_to(text: &str, width: usize, ascii: bool) -> String {
    if width == 0 {
        return String::new();
    }
    if UnicodeWidthStr::width(text) <= width {
        return text.to_owned();
    }
    let marker = if ascii { '>' } else { '…' };
    let mut out = String::new();
    let mut used = 0usize;
    for ch in text.chars() {
        let w = UnicodeWidthStr::width(ch.to_string().as_str());
        if used + w + 1 > width {
            out.push(marker);
            return out;
        }
        out.push(ch);
        used += w;
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn picker() -> Picker {
        Picker::new(
            PickerKind::Model,
            "Model",
            vec![
                PickerItem::new("gpt-5.2", "200k context", "gpt-5.2").as_current(),
                PickerItem::new("qwen3-coder", "128k context", "qwen3-coder"),
                PickerItem::new("llama3.1:8b", "local", "llama3.1:8b"),
            ],
        )
    }

    #[test]
    fn an_unfiltered_picker_offers_every_row_with_the_first_under_the_cursor() {
        let picker = picker();
        assert_eq!(picker.visible(), vec![0, 1, 2]);
        assert_eq!(picker.selected_item().unwrap().value, "gpt-5.2");
    }

    #[test]
    fn an_exact_name_beats_a_longer_name_that_merely_contains_the_query() {
        let mut picker = Picker::new(
            PickerKind::Command,
            "Commands",
            vec![
                PickerItem::new("/model", "choose the model", "model"),
                PickerItem::new("/mode", "choose the posture", "mode"),
            ],
        );
        picker.type_str("mode");
        assert_eq!(
            picker.selected_item().unwrap().value,
            "mode",
            "`mode` contains `mode` and `model` contains `mode`; the exact name has to win"
        );
        assert_eq!(
            picker.visible(),
            vec![1, 0],
            "both still offered, best first"
        );
    }

    #[test]
    fn typing_filters_without_reordering_and_a_stale_selection_does_not_choose_a_hidden_row() {
        let mut picker = picker();
        picker.selected = 2;
        picker.type_str("code");
        assert_eq!(
            picker.visible(),
            vec![1],
            "only the coder model survives the filter"
        );
        assert_eq!(
            picker.selected_item().unwrap().value,
            "qwen3-coder",
            "the cursor is reset with the filter, so it cannot sit on a row that is not drawn"
        );
    }

    #[test]
    fn a_query_matches_a_value_as_well_as_a_label_and_a_subsequence_of_one() {
        let mut picker = picker();
        picker.type_str("8b");
        assert_eq!(picker.visible(), vec![2]);
        picker.query.clear();
        picker.type_str("g52");
        assert_eq!(
            picker.visible(),
            vec![0],
            "`g52` is not a substring of `gpt-5.2` but every letter of it is in order"
        );
    }

    #[test]
    fn the_filter_can_match_nothing_and_that_is_a_state_not_an_error() {
        let mut picker = picker();
        picker.type_str("nothing-here");
        assert!(picker.visible().is_empty());
        assert!(picker.selected_item().is_none());
        let lines = picker_lines(&picker, &Theme::new(false));
        let text: String = lines
            .iter()
            .map(|line| {
                line.spans
                    .iter()
                    .map(|span| span.content.as_ref())
                    .collect::<String>()
            })
            .collect();
        assert!(text.contains("nothing matches"), "got {text}");
    }

    #[test]
    fn selection_wraps_within_what_is_visible() {
        let mut picker = picker();
        picker.move_by(-1);
        assert_eq!(picker.selected, 2, "up from the first row is the last one");
        picker.move_by(1);
        assert_eq!(picker.selected, 0);
        picker.type_str("code");
        picker.move_by(1);
        assert_eq!(
            picker.selected, 0,
            "one visible row cannot scroll to itself twice"
        );
    }

    #[test]
    fn a_long_list_scrolls_with_the_cursor_kept_in_view() {
        let items: Vec<PickerItem> = (0..40)
            .map(|i| PickerItem::new(format!("model-{i:02}"), "", format!("model-{i:02}")))
            .collect();
        let mut picker = Picker::new(PickerKind::Model, "Model", items);
        picker.selected = 20;
        let (rows, start) = picker.window();
        assert_eq!(rows.len(), WINDOW);
        assert!(
            rows.contains(&20),
            "the cursor row has to be on the screen, got window starting at {start}"
        );
    }

    #[test]
    fn the_row_carries_the_value_the_runtime_acts_on_not_the_text_the_operator_read() {
        let picker = picker();
        let current = picker.items.iter().find(|i| i.current).unwrap();
        assert_eq!(current.value, "gpt-5.2");
        assert_eq!(current.label, "gpt-5.2");
        let lines = picker_lines(&picker, &Theme::new(false));
        let text: String = lines
            .iter()
            .map(|line| {
                line.spans
                    .iter()
                    .map(|span| span.content.as_ref())
                    .collect::<String>()
            })
            .collect();
        assert!(text.contains("(current)"), "got {text}");
        assert!(text.contains("enter choose"), "got {text}");
    }

    #[test]
    fn a_note_explains_a_short_list_instead_of_leaving_it_unexplained() {
        let picker = Picker::new(PickerKind::Model, "Model", vec![])
            .with_note("the endpoint did not answer: connection refused");
        let lines = picker_lines(&picker, &Theme::new(false));
        let text: String = lines
            .iter()
            .map(|line| {
                line.spans
                    .iter()
                    .map(|span| span.content.as_ref())
                    .collect::<String>()
            })
            .collect();
        assert!(text.contains("connection refused"), "got {text}");
        assert!(text.contains("nothing to choose"), "got {text}");
    }

    #[test]
    fn a_chooser_opens_on_the_value_in_force_instead_of_the_first_row() {
        let items = vec![
            PickerItem::new("amazon/nova", "", "amazon/nova"),
            PickerItem::new(
                "nvidia/nemotron-3.5-super-vl:free",
                "",
                "nvidia/nemotron-3.5-super-vl:free",
            ),
            PickerItem::new("z-ai/glm-4.6", "", "z-ai/glm-4.6"),
        ];
        let mut picker = Picker::new(PickerKind::Model, "Model", items);
        assert_eq!(picker.selected_item().unwrap().value, "amazon/nova");

        assert!(picker.select_value("nvidia/nemotron-3.5-super-vl:free"));
        assert_eq!(
            picker.selected_item().unwrap().value,
            "nvidia/nemotron-3.5-super-vl:free",
            "the row in force is the row under the cursor"
        );

        assert!(
            !picker.select_value("a-model-this-endpoint-does-not-serve"),
            "an absent value has to say it was not found rather than seat the cursor somewhere else"
        );
        assert_eq!(
            picker.selected_item().unwrap().value,
            "nvidia/nemotron-3.5-super-vl:free"
        );
    }

    /// The text of a rendered line, for assertions that only care about what the operator reads.
    fn text_of(line: &Line<'static>) -> String {
        line.spans
            .iter()
            .map(|span| span.content.as_ref())
            .collect()
    }

    /// Column at which `needle` starts on a rendered line, in display columns.
    fn column_of(line: &Line<'static>, needle: &str) -> Option<usize> {
        let text = text_of(line);
        let at = text.find(needle)?;
        Some(UnicodeWidthStr::width(&text[..at]))
    }

    #[test]
    fn a_long_footnote_does_not_stretch_the_panel() {
        // The Provider chooser ends with a sentence about where the key lives. It sits on its own
        // row as a footnote, not in another column, so it must not widen the frame for the rows
        // above it — which is what one flat maximum over every label used to do.
        let picker = Picker::new(
            PickerKind::Provider,
            "Provider",
            vec![
                PickerItem::new("NVIDIA NIM", "$NVIDIA_API_KEY · key set", "nvidia").as_current(),
                PickerItem::new("Groq", "$GROQ_API_KEY · no key", "groq"),
                PickerItem::new(
                    "! the key itself is never stored here: it lives in $VIKRAM_HOME/.env",
                    "",
                    "",
                ),
            ],
        );
        let width = wanted_width(&picker);
        assert!(
            width <= 68,
            "a footnote stretched the panel to {width} columns"
        );

        let body = picker_body(&picker, &Theme::new(false), width);
        for line in &body.lines {
            let text = text_of(line);
            assert!(
                UnicodeWidthStr::width(text.as_str()) <= width as usize,
                "line wider than the panel: {text:?}"
            );
        }

        // Without the footnote the same rows ask for only what their columns need.
        let rows = Picker::new(
            PickerKind::Provider,
            "Provider",
            vec![
                PickerItem::new("NVIDIA NIM", "$NVIDIA_API_KEY · key set", "nvidia").as_current(),
                PickerItem::new("Groq", "$GROQ_API_KEY · no key", "groq"),
            ],
        );
        assert!(
            wanted_width(&rows) <= 52,
            "columns alone asked for {}",
            wanted_width(&rows)
        );
    }

    #[test]
    fn every_detail_starts_in_the_same_column() {
        let mut picker = Picker::new(
            PickerKind::Provider,
            "Provider",
            vec![
                PickerItem::new("OpenRouter", "$OPENROUTER_API_KEY · key set", "openrouter"),
                PickerItem::new("NVIDIA NIM", "$NVIDIA_API_KEY · key set", "nvidia").as_current(),
                PickerItem::new("Groq", "$GROQ_API_KEY · no key", "groq"),
            ],
        );
        picker.select_position(1);
        let body = picker_body(&picker, &Theme::new(false), 76);

        let columns: Vec<usize> = body.lines[body.first_row..body.first_row + body.rows]
            .iter()
            .map(|line| {
                let detail = line
                    .spans
                    .iter()
                    .map(|span| span.content.as_ref())
                    .find(|content| content.starts_with('$'))
                    .unwrap_or_else(|| panic!("row has no detail: {}", text_of(line)));
                column_of(line, detail).expect("detail is on the row")
            })
            .collect();
        assert_eq!(
            columns.len(),
            3,
            "three rows were offered, three rows must be drawn"
        );
        assert!(
            columns.windows(2).all(|pair| pair[0] == pair[1]),
            "details are ragged, which hides the column: {columns:?}"
        );
    }

    #[test]
    fn short_details_leave_the_width_to_the_labels() {
        // A doctor-style list: long labels, two-character details. Reserving a fixed twenty columns
        // for the detail column clipped every label in the panel, so the row that explains the
        // machine was the one the panel cut off.
        let picker = Picker::new(
            PickerKind::Doctor,
            "Doctor",
            vec![
                PickerItem::new(
                    "store /tmp/vikram/vikram.db · 1 session(s), 0 bot(s)",
                    "ok",
                    "store",
                ),
                PickerItem::new(
                    "model: nvidia/nemotron-3.5-super-vl-120b-a12b",
                    "ok",
                    "model",
                ),
            ],
        );
        let body = picker_body(&picker, &Theme::new(false), 90);

        let first = text_of(&body.lines[body.first_row]);
        assert!(
            first.contains("0 bot(s)"),
            "the label was clipped to make room for a detail that never needed it: {first}"
        );
    }

    #[test]
    fn the_row_in_force_says_so_in_its_own_column() {
        let picker = picker();
        let body = picker_body(&picker, &Theme::new(false), 72);
        let row = &body.lines[body.first_row];
        let text = text_of(row);
        assert!(
            text.trim_end().ends_with("(current)"),
            "the row in force is not marked: {text:?}"
        );
        let marker_start = column_of(row, "(current)").expect("the marker is on the row");
        assert!(
            marker_start + CURRENT.len() + 1 >= 72,
            "the marker is not a right-hand column: {text:?}"
        );
    }

    #[test]
    fn the_cursor_is_visible_without_relying_on_colour() {
        let picker = picker();
        let body = picker_body(&picker, &Theme::new(false), 72);
        let marked: Vec<usize> = (body.first_row..body.first_row + body.rows)
            .filter(|row| text_of(&body.lines[*row]).starts_with("  ▸"))
            .collect();
        assert_eq!(
            marked,
            vec![body.first_row],
            "exactly the cursor row carries the marker"
        );
    }

    #[test]
    fn no_row_is_wider_than_the_panel_it_is_drawn_in() {
        let mut picker = Picker::new(
            PickerKind::Model,
            "Model",
            vec![
                PickerItem::new(
                    "nvidia/nemotron-3.5-super-vl-120b-a12b",
                    "120b · vision · 128k context",
                    "a",
                )
                .as_current(),
                PickerItem::new(
                    "deepseek-ai/deepseek-coder-6.7b-instruct",
                    "6.7b · code · 16k context",
                    "b",
                ),
            ],
        );
        picker.select_position(0);
        for width in [40u16, 60, 100] {
            let body = picker_body(&picker, &Theme::new(false), width);
            for line in &body.lines {
                let drawn = UnicodeWidthStr::width(text_of(line).as_str());
                assert!(
                    drawn <= width as usize,
                    "a {drawn}-column line was drawn in a {width}-column panel: {:?}",
                    text_of(line)
                );
            }
        }
    }

    #[test]
    fn the_body_reports_where_its_rows_landed() {
        let picker = picker();
        let body = picker_body(&picker, &Theme::new(false), 72);
        assert_eq!(body.rows, 3, "one line per row offered");
        let first = text_of(&body.lines[body.first_row]);
        assert!(
            first.contains("gpt-5.2"),
            "the reported first row is not a row: {first:?}"
        );
        assert!(
            !text_of(&body.lines[body.first_row - 1]).contains("gpt-5.2"),
            "the line above the first row must not be a row"
        );
    }

    /// The indent is a string of spaces, and a constant that says how many spaces it is must not be
    /// printed as a digit. This is the bug the first draft shipped: the panel opened with `2filter`.
    #[test]
    fn every_line_is_indented_by_spaces_and_not_by_a_number() {
        let picker = picker();
        let body = picker_body(&picker, &Theme::new(false), 72);
        for (index, line) in body.lines.iter().enumerate() {
            let text = text_of(line);
            assert!(
                text.starts_with("  "),
                "line {index} is not indented with two spaces: {text:?}"
            );
            assert!(
                !text.starts_with("2filter") && !text.ends_with('2'),
                "line {index} printed the indent width instead of the indent: {text:?}"
            );
        }
    }
}
