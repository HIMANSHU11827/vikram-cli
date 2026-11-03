//! The searchable picker: the one overlay every `/` command that *chooses* something opens.
//!
//! A picker is not a list of strings. Each row carries the value the runtime needs (a model name, a
//! bot profile, a path) beside the text that identifies it, so choosing a row is an instruction the
//! runtime can act on rather than a word to paste into the input box. The filter, the selection and
//! the scroll window are all derived from those fields, which is what lets the same widget serve
//! `/model`, `/bots`, `/team`, `/skills`, `/files`, `/mcp` and the command palette itself.

use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};

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
    Agent,
    /// What to do with the agent just chosen: pause, resume or stop.
    AgentAction,
    Team,
    Skill,
    File,
    Mcp,
    Settings,
    Command,
    Shell,
    /// A conversation to switch to.
    Session,
    /// A background task to stop.
    Task,
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
            Self::Bot => &[(Verb::Choose, '\n', "select"), (Verb::New, 'n', "new bot")],
            Self::Agent => &[(Verb::Choose, '\n', "act on it")],
            Self::AgentAction => &[(Verb::Choose, '\n', "apply")],
            Self::Session => &[
                (Verb::Choose, '\n', "switch"),
                (Verb::Rename, 'r', "rename"),
                (Verb::Close, 'x', "close"),
                (Verb::New, 'n', "new session"),
            ],
            Self::Task => &[(Verb::Stop, '\n', "stop")],
            Self::Memory => &[(Verb::Choose, '\n', "read"), (Verb::Remove, 'd', "forget")],
            Self::Setting => &[(Verb::Toggle, '\n', "flip")],
            Self::Mcp => &[(Verb::Toggle, '\n', "flip visibility")],
            Self::Skill => &[
                (Verb::Choose, '\n', "read"),
                (Verb::Toggle, 't', "enable/disable"),
                (Verb::New, 'n', "new skill"),
                (Verb::Remove, 'd', "delete"),
                (Verb::Prompt, 'i', "import path"),
            ],
            Self::Tools => &[
                (Verb::Choose, '\n', "inspect"),
                (Verb::Toggle, 't', "enable/disable"),
            ],
            Self::File => &[(Verb::Choose, '\n', "reference it")],
            Self::Shell => &[(Verb::Choose, '\n', "run it")],
            Self::Command => &[(Verb::Choose, '\n', "open")],
            Self::Provider
            | Self::Team
            | Self::Settings
            | Self::Permissions
            | Self::Plugins
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
        }
    }

    /// The verb a Ctrl+letter names, if this panel declares it.
    pub fn verb_for_key(self, key: char) -> Option<Verb> {
        self.verbs()
            .iter()
            .find(|(_, k, _)| k.eq_ignore_ascii_case(&key))
            .map(|(verb, _, _)| *verb)
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
            Self::Agent => "agent",
            Self::AgentAction => "agent action",
            Self::Team => "team",
            Self::Skill => "skill",
            Self::File => "file",
            Self::Mcp => "mcp",
            Self::Settings => "settings",
            Self::Session => "session",
            Self::Task => "task",
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

/// The dialog body: the filter line, the visible rows with the cursor on one of them, and a footer
/// that names the keys. Nothing here is a placeholder — an empty list says the filter matched
/// nothing, and an empty source list says there is nothing to choose.
pub fn picker_lines(picker: &Picker, theme: &Theme) -> Vec<Line<'static>> {
    let cursor = if theme.ascii { ">" } else { "▸" };
    let bullet = if theme.ascii { "*" } else { "•" };
    let mut lines: Vec<Line<'static>> = Vec::new();

    lines.push(Line::from(vec![
        Span::styled("  filter ", theme.muted()),
        Span::styled(
            picker.query.to_string(),
            Style::default()
                .fg(theme.accent)
                .add_modifier(Modifier::BOLD),
        ),
        Span::styled("▌", theme.muted()),
    ]));

    if let Some(note) = &picker.note {
        lines.push(Line::from(Span::styled(
            format!("  {note}"),
            Style::default().fg(theme.accent_dim),
        )));
    }

    let (rows, start) = picker.window();
    if picker.items.is_empty() {
        lines.push(Line::from(Span::styled(
            format!("  {bullet} nothing to choose yet"),
            theme.secondary(),
        )));
    } else if rows.is_empty() {
        lines.push(Line::from(Span::styled(
            format!("  {bullet} nothing matches `{}`", picker.query),
            theme.secondary(),
        )));
    } else {
        for (position, index) in rows.iter().enumerate() {
            let item = &picker.items[*index];
            let on_cursor = start + position == picker.selected;
            let style = if on_cursor {
                Style::default()
                    .fg(theme.accent)
                    .add_modifier(Modifier::BOLD)
            } else {
                theme.secondary()
            };
            let mut spans = vec![
                Span::styled(
                    if on_cursor {
                        format!("{cursor} ")
                    } else {
                        "  ".to_owned()
                    },
                    style,
                ),
                Span::styled(item.label.clone(), style),
            ];
            if !item.detail.is_empty() {
                spans.push(Span::styled(format!("  {}", item.detail), theme.muted()));
            }
            if item.current {
                spans.push(Span::styled("  (current)", theme.muted()));
            }
            lines.push(Line::from(spans));
        }
    }

    let visible = picker.visible().len();
    let keys: Vec<String> = picker
        .kind
        .verbs()
        .iter()
        .map(|(_, key, label)| {
            if *key == '\n' {
                format!("enter {label}")
            } else {
                format!("ctrl+{key} {label}")
            }
        })
        .collect();
    lines.push(Line::from(Span::styled(
        format!(
            "  {} of {} · {}",
            if visible == 0 { 0 } else { picker.selected + 1 },
            visible,
            keys.join(" · ")
        ),
        theme.muted(),
    )));
    // Keys on their own line: a panel that clips its own footer has hidden the one thing the
    // operator needs to know.
    lines.push(Line::from(Span::styled(
        "  type to filter · ↑↓ move · ctrl+u/d page · esc closes".to_owned(),
        theme.muted(),
    )));
    lines
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
}
