//! The conversation transcript.
//!
//! Rendering is a pure function of [`App`] and [`SessionView`]: nothing here mutates state, emits a
//! command, or invents information. In particular there is no clock anywhere — a transcript line
//! says who spoke and what was said, never when.

use std::collections::{HashMap, HashSet};

use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::Style;
use ratatui::text::{Line, Span};
use ratatui::widgets::Paragraph;
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};
use vikram_contracts::{Block, Message, Role, ToolCallId};

use crate::app::{
    Activity, ActivityKind, App, PendingQuestion, SessionView, ThoughtHit, ThoughtKey,
    STREAMING_THOUGHT,
};
use crate::theme::Theme;
use crate::widgets::markdown;

/// A tool result longer than this is elided: the transcript is a summary, not a log file.
const TOOL_LINE_CAP: usize = 12;
/// A separator never spans a very wide terminal; it reads as a rule, not a border.
const SEPARATOR_CAP: usize = 72;
/// Argument summaries are compacted to one short line.
const ARGUMENT_CAP: usize = 80;
/// A folded thinking block previews at most this much of one line.
const THOUGHT_PREVIEW_CAP: usize = 64;

/// A folding header: where it was drawn, how wide its label is, which block it folds, and whether
/// that block is a tool call or a thinking block. Both live in the same key space but fold
/// different maps, so the flag travels with the mark.
type Mark = (usize, u16, ThoughtKey, bool);

/// Activity rows fold in their own key space so a tool row and a thinking block never share a
/// fold. The first half of the key is a marker no message index can reach.
const ACTIVITY_TOOL: usize = usize::MAX - 1;

/// What the tool blocks already say, so a call and its result can be drawn as one row.
///
/// The runtime emits a call block, then a result block carrying the same id, then a `Role::Tool`
/// message that repeats it. Reading the transcript once up front is what makes one row per call
/// possible; without it every tool payload renders twice under a `tool` heading.
struct ToolIndex<'a> {
    results: HashMap<&'a ToolCallId, (&'a str, bool)>,
    called: HashSet<&'a ToolCallId>,
    /// The runtime reports a call through an activity row, not through a `ToolCall` block, so a
    /// result is only an orphan when neither names it.
    activity: HashMap<&'a ToolCallId, &'a Activity>,
}

impl<'a> ToolIndex<'a> {
    fn scan(messages: &'a [Message], activity: &'a [Activity]) -> Self {
        let mut results = HashMap::new();
        let mut called = HashSet::new();
        let mut live = HashMap::new();
        for entry in activity {
            if let Some(call) = &entry.call {
                live.insert(call, entry);
            }
        }
        for message in messages {
            for block in &message.blocks {
                match block {
                    Block::ToolCall { call } => {
                        called.insert(&call.id);
                    }
                    Block::ToolResult {
                        call, content, ok, ..
                    } => {
                        results.insert(call, (content.as_str(), *ok));
                    }
                    _ => {}
                }
            }
        }
        Self {
            results,
            called,
            activity: live,
        }
    }

    fn paired(&self, call: &ToolCallId) -> bool {
        self.called.contains(call)
    }

    fn outcome(&self, call: &ToolCallId) -> Option<(&'a str, bool)> {
        self.results.get(call).copied()
    }

    fn result(&self, call: &ToolCallId) -> Option<(&'a str, bool)> {
        self.results.get(call).copied()
    }
}

/// Draw the transcript, and report where each folding header landed so the mouse can name the
/// block it was clicked on. Geometry is decided here because only the draw knows the scroll offset.
pub fn conversation(
    frame: &mut Frame,
    area: Rect,
    app: &App,
    session: &SessionView,
    theme: &Theme,
) -> Vec<ThoughtHit> {
    let width = area.width.saturating_sub(2) as usize;
    let (lines, marks) = conversation_lines_with_hits(app, session, width, theme);
    let total = clamp_u16(lines.len());
    let height = area.height;
    let offset = if session.follow {
        total.saturating_sub(height)
    } else {
        total
            .saturating_sub(height)
            .saturating_sub(clamp_u16(session.scroll))
    };
    frame.render_widget(
        Paragraph::new(lines)
            .scroll((offset, 0))
            .style(theme.body()),
        area,
    );

    marks
        .into_iter()
        .filter_map(|(line, label_width, key, tool)| {
            let rel = (line as i64) - (offset as i64);
            if rel < 0 || rel >= height as i64 {
                return None;
            }
            Some(ThoughtHit {
                row: area.y + rel as u16,
                x_start: area.x,
                x_end: area.x + label_width.saturating_sub(1),
                key,
                tool,
            })
        })
        .collect()
}

pub fn conversation_lines(
    app: &App,
    session: &SessionView,
    width: usize,
    theme: &Theme,
) -> Vec<Line<'static>> {
    conversation_lines_with_hits(app, session, width, theme).0
}

/// The transcript and every folding header's line index and label width. The public wrapper drops
/// the marks; the draw keeps them.
#[allow(clippy::type_complexity)]
fn conversation_lines_with_hits(
    app: &App,
    session: &SessionView,
    width: usize,
    theme: &Theme,
) -> (Vec<Line<'static>>, Vec<Mark>) {
    let width = width.max(4);
    let mut lines: Vec<Line<'static>> = Vec::new();
    let mut marks: Vec<Mark> = Vec::new();
    let index = ToolIndex::scan(&session.messages, &session.activity);
    // A call with no result yet may be waiting on a permission answer; the row says so rather
    // than reading as a call that is still running.
    let waiting: HashSet<&str> = app
        .approvals
        .iter()
        .map(|approval| approval.verb.as_str())
        .collect();

    // An empty session renders nothing: the interface does not fabricate a greeting or sample
    // conversation. Real content arrives through the runtime's message stream.

    for (message_index, message) in session.messages.iter().enumerate() {
        match message.role {
            Role::User => push_user(
                &mut lines,
                message,
                message_index,
                session,
                &mut marks,
                width,
                theme,
            ),
            Role::Assistant => push_assistant(
                &mut lines,
                message,
                message_index,
                session,
                &index,
                &waiting,
                &mut marks,
                width,
                theme,
            ),
            Role::System => push_system(
                &mut lines,
                message,
                message_index,
                &index,
                &mut marks,
                width,
                theme,
            ),
            Role::Tool => push_tool(&mut lines, message, &index, width, theme),
        }
    }

    for (idx, activity) in session.activity.iter().enumerate() {
        push_activity(
            &mut lines,
            app,
            activity,
            &index,
            session,
            &mut marks,
            width,
            theme,
            idx,
            session.selected_activity,
        );
    }

    if session.streaming.is_some() || session.streaming_thought.is_some() {
        lines.push(Line::from(""));
        lines.push(Line::from(vec![
            Span::styled(format!("{} ", theme.filled_dot()), theme.title()),
            Span::styled("VIKRAM", theme.title()),
        ]));
        if let Some(thought) = &session.streaming_thought {
            push_reasoning(
                &mut lines,
                thought,
                session.thought_expanded(STREAMING_THOUGHT),
                true,
                STREAMING_THOUGHT,
                &mut marks,
                width,
                theme,
            );
        }
        if let Some(stream) = &session.streaming {
            lines.extend(markdown::markdown_lines(&stream.text, width, theme));
        }
        // The turn is in flight whenever either stream is open, including the stretch before the
        // first token arrives: a spinner with nothing under it still tells you it is thinking.
        lines.push(Line::from(vec![
            Span::styled(
                format!("  {} ", app.thinking_frame()),
                Style::default().fg(theme.accent),
            ),
            Span::styled("Working", Style::default().fg(theme.accent)),
        ]));
    }

    if let Some(pending) = &app.pending_question {
        push_question_prompt(&mut lines, pending, width, theme);
    }

    (lines, marks)
}

/// A pending `ask_user` question, drawn at the foot of the transcript because the runtime is
/// blocked on it: what was asked, the choices it offered, and the answer being typed.
fn push_question_prompt(
    lines: &mut Vec<Line<'static>>,
    pending: &PendingQuestion,
    width: usize,
    theme: &Theme,
) {
    lines.push(Line::from(""));
    lines.push(Line::styled("VIKRAM asks", theme.title()));
    lines.extend(markdown::markdown_lines(&pending.question, width, theme));
    if !pending.choices.is_empty() {
        let separator = format!(" {} ", theme.bullet());
        let choices = pending.choices.join(separator.as_str());
        for line in markdown::wrap_plain(&format!("choices: {choices}"), width, theme.muted()) {
            lines.push(line);
        }
    }
    lines.push(Line::from(vec![
        Span::styled("Answer: ", Style::default().fg(theme.accent)),
        Span::styled(pending.input.clone(), Style::default().fg(theme.text)),
    ]));
}

// ---------------------------------------------------------------------------
// roles
// ---------------------------------------------------------------------------

fn push_user(
    lines: &mut Vec<Line<'static>>,
    message: &Message,
    message_index: usize,
    session: &SessionView,
    marks: &mut Vec<Mark>,
    width: usize,
    theme: &Theme,
) {
    let base = Style::default().fg(theme.user);
    lines.push(Line::from(""));
    lines.push(Line::styled(
        format!("{} User", theme.prompt()),
        theme.muted(),
    ));
    for (block_index, block) in message.blocks.iter().enumerate() {
        let key = (message_index, block_index);
        match block {
            Block::Text { text, .. } => {
                lines.extend(markdown::markdown_lines_styled(text, width, theme, base))
            }
            Block::Reasoning { text, .. } => push_reasoning(
                lines,
                text,
                session.thought_expanded(key),
                false,
                key,
                marks,
                width,
                theme,
            ),
            Block::ToolCall { call } => push_tool_call(lines, call, width, theme),
            Block::ToolResult { content, ok, .. } => {
                let style = if *ok {
                    base
                } else {
                    Style::default().fg(theme.error)
                };
                lines.extend(markdown::markdown_lines_styled(
                    content, width, theme, style,
                ));
            }
        }
    }
    lines.push(separator_line(width, theme));
}

fn push_assistant(
    lines: &mut Vec<Line<'static>>,
    message: &Message,
    message_index: usize,
    session: &SessionView,
    index: &ToolIndex,
    waiting: &HashSet<&str>,
    marks: &mut Vec<Mark>,
    width: usize,
    theme: &Theme,
) {
    lines.push(Line::from(""));
    lines.push(Line::styled("VIKRAM", theme.title()));
    for (block_index, block) in message.blocks.iter().enumerate() {
        let key = (message_index, block_index);
        match block {
            Block::Text { text, .. } => lines.extend(markdown::markdown_lines(text, width, theme)),
            Block::Reasoning { text, .. } => push_reasoning(
                lines,
                text,
                session.thought_expanded(key),
                false,
                key,
                marks,
                width,
                theme,
            ),
            Block::ToolCall { call } => push_tool_row(
                lines,
                call,
                index.outcome(&call.id),
                awaiting_answer(waiting, call),
                session.tool_expanded(key),
                key,
                marks,
                width,
                theme,
            ),
            Block::ToolResult { call, content, ok, .. } => {
                // The row above already renders this result; a paired block adds nothing.
                if !index.paired(call) {
                    push_tool_payload(lines, content, *ok, width, theme);
                }
            }
        }
    }
}

fn push_system(
    lines: &mut Vec<Line<'static>>,
    message: &Message,
    message_index: usize,
    index: &ToolIndex,
    marks: &mut Vec<Mark>,
    width: usize,
    theme: &Theme,
) {
    lines.push(Line::from(""));
    lines.push(Line::styled(dim_label(theme, "system"), theme.muted()));
    let style = theme.muted();
    for (block_index, block) in message.blocks.iter().enumerate() {
        let key = (message_index, block_index);
        match block {
            Block::Text { text, .. } => {
                lines.extend(markdown::markdown_lines_styled(text, width, theme, style))
            }
            Block::Reasoning { text, .. } => push_reasoning(lines, text, false, false, key, marks, width, theme),
            Block::ToolCall { call } => push_tool_row(
                lines,
                call,
                index.outcome(&call.id),
                false,
                false,
                key,
                marks,
                width,
                theme,
            ),
            Block::ToolResult { call, content, ok, .. } => {
                if !index.paired(call) {
                    push_tool_payload(lines, content, *ok, width, theme);
                }
            }
        }
    }
}

/// Results the transcript has no call for — a resumed session, or a tool the runtime answered on
/// its own. They have no row to hang from, so each gets one line of its own.
fn push_tool(
    lines: &mut Vec<Line<'static>>,
    message: &Message,
    index: &ToolIndex,
    width: usize,
    theme: &Theme,
) {
    for block in &message.blocks {
        match block {
            Block::ToolResult { call, content, ok, .. } => {
                if index.paired(call) || index.activity.contains_key(call) {
                    continue;
                }
                push_tool_payload(lines, content, *ok, width, theme);
            }
            Block::Text { text, .. } | Block::Reasoning { text, .. } => {
                if !text.trim().is_empty() {
                    lines.extend(markdown::markdown_lines_styled(
                        text,
                        width,
                        theme,
                        theme.muted(),
                    ));
                }
            }
            Block::ToolCall { call } => {
                let label = tool_label(call, theme);
                lines.push(Line::from(vec![
                    Span::styled(format!("{} ", theme.chevron()), theme.secondary()),
                    Span::styled(label.name, Style::default().fg(theme.tool)),
                ]));
            }
        }
    }
}

/// A call whose answer has not come back yet but which the host is holding a permission question
/// for. The row says `waiting` instead of spinning forever.
fn awaiting_answer(waiting: &HashSet<&str>, call: &vikram_contracts::ToolCall) -> bool {
    waiting.contains(call.name.as_str())
}

/// What a tool call is called on screen, split from what it was called with.
struct ToolLabel {
    name: String,
    detail: Option<String>,
}

/// One row per call.
///
/// The runtime's punctuation is not what a person reads: `web.search {"query": "news"}` is a
/// search for the news, and `fs.read {"path": "Cargo.toml"}` is reading that file. The arguments
/// become the sentence and the payload is folded behind the row, so a turn with four tools is
/// four lines rather than four screens.
fn push_tool_row(
    lines: &mut Vec<Line<'static>>,
    call: &vikram_contracts::ToolCall,
    outcome: Option<(&str, bool)>,
    waiting: bool,
    expanded: bool,
    key: ThoughtKey,
    marks: &mut Vec<Mark>,
    width: usize,
    theme: &Theme,
) {
    let (glyph, style) = match outcome {
        Some((_, true)) => (theme.check().to_owned(), Style::default().fg(theme.tool)),
        Some((_, false)) => (theme.cross().to_owned(), Style::default().fg(theme.error)),
        None if waiting => (
            if theme.ascii { "!" } else { "⏸" }.to_owned(),
            Style::default().fg(theme.accent),
        ),
        None => (
            if theme.ascii { "->" } else { "→" }.to_owned(),
            theme.secondary(),
        ),
    };
    let label = tool_label(call, theme);
    let head = format!("{glyph} {}", label.name);
    marks.push((lines.len(), head.width() as u16, key, true));

    let mut spans = vec![
        Span::styled(format!("{glyph} "), style),
        Span::styled(label.name, Style::default().fg(theme.text)),
    ];
    if let Some(detail) = label.detail {
        spans.push(Span::styled(format!("  {detail}"), theme.muted()));
    }
    if outcome.is_some() {
        spans.push(Span::styled(
            if expanded {
                " · click folds"
            } else {
                " · click"
            },
            Style::default().fg(theme.faint),
        ));
    }
    lines.push(Line::from(spans));

    if expanded {
        if let Some((content, ok)) = outcome {
            push_tool_payload(lines, content, ok, width, theme);
        }
    }
}

/// A tool row drawn from an activity instead of a call block.
fn push_activity_tool_row(
    lines: &mut Vec<Line<'static>>,
    activity: &Activity,
    content: &str,
    ok: bool,
    waiting: bool,
    expanded: bool,
    key: ThoughtKey,
    marks: &mut Vec<Mark>,
    width: usize,
    theme: &Theme,
) {
    let (glyph, style) = match activity.ok {
        Some(true) => (theme.check().to_owned(), Style::default().fg(theme.tool)),
        Some(false) => (theme.cross().to_owned(), Style::default().fg(theme.error)),
        None if waiting => (
            if theme.ascii { "!" } else { "⏸" }.to_owned(),
            Style::default().fg(theme.accent),
        ),
        None => (
            if theme.ascii { "->" } else { "→" }.to_owned(),
            theme.secondary(),
        ),
    };
    let label = tool_label_from_text(&activity.text, theme);
    let head = format!("{glyph} {}", label.name);
    marks.push((lines.len(), head.width() as u16, key, true));
    let mut spans = vec![
        Span::styled(format!("{glyph} "), style),
        Span::styled(label.name, Style::default().fg(theme.text)),
    ];
    if let Some(detail) = label.detail {
        spans.push(Span::styled(format!("  {detail}"), theme.muted()));
    }
    spans.push(Span::styled(
        if expanded { " · click folds" } else { " · click" },
        Style::default().fg(theme.faint),
    ));
    lines.push(Line::from(spans));
    if expanded {
        push_tool_payload(lines, content, ok, width, theme);
    }
}

fn activity_first_word(text: &str) -> &str {
    text.split_whitespace().next().unwrap_or_default()
}

/// The same sentence `tool_label` builds, recovered from the activity text, which holds the tool
/// name followed by its arguments as `key=value` pairs.
fn tool_label_from_text(text: &str, theme: &Theme) -> ToolLabel {
    let mut parts = text.splitn(2, char::is_whitespace);
    let name = parts.next().unwrap_or_default();
    let rest = parts.next().unwrap_or_default().trim();
    let friendly = match name {
        "web.search" | "web_search" => "Web search",
        "fs.read" | "read_file" => "Read",
        "fs.edit" | "fs.write" => "Edited",
        "fs.delete" => "Deleted",
        "shell.run" | "shell" => "Ran",
        "skill" => "Skill",
        "ask_user" => "Asked",
        other => {
            let detail = if rest.is_empty() {
                None
            } else {
                Some(truncate(rest, ARGUMENT_CAP, ellipsis(theme)))
            };
            return ToolLabel {
                name: humanize(other),
                detail,
            };
        }
    };
    let detail = ["query", "q", "path", "file", "command", "cmd", "name"]
        .iter()
        .find_map(|key| {
            let needle = format!("{key}=");
            rest.find(&needle)
                .map(|at| rest[at + needle.len()..].trim())
                .filter(|value| !value.is_empty())
                .map(|value| truncate(value, ARGUMENT_CAP, ellipsis(theme)))
        })
        .map(|value| {
            if name.starts_with("web") {
                format!("\"{value}\"")
            } else {
                value
            }
        });
    ToolLabel {
        name: friendly.to_owned(),
        detail,
    }
}

fn tool_label(call: &vikram_contracts::ToolCall, theme: &Theme) -> ToolLabel {
    let arg = |keys: &[&str]| string_arg(&call.arguments, keys, ARGUMENT_CAP, ellipsis(theme));
    let (name, detail) = match call.name.as_str() {
        "web.search" | "web_search" => (
            "Web search".to_owned(),
            arg(&["query", "q"]).map(|query| format!("\"{query}\"")),
        ),
        "fs.read" | "read_file" => ("Read".to_owned(), arg(&["path", "file", "filename"])),
        "fs.edit" | "fs.write" => ("Edited".to_owned(), arg(&["path", "file"])),
        "fs.delete" => ("Deleted".to_owned(), arg(&["path", "file"])),
        "shell.run" | "shell" => ("Ran".to_owned(), arg(&["command", "cmd"])),
        "skill" => ("Skill".to_owned(), arg(&["name", "skill"])),
        "ask_user" => ("Asked".to_owned(), arg(&["question"])),
        other => {
            let summary = argument_summary(&call.arguments, ARGUMENT_CAP, ellipsis(theme));
            let detail = if summary.is_empty() { None } else { Some(summary) };
            (humanize(other), detail)
        }
    };
    ToolLabel { name, detail }
}

fn string_arg(
    arguments: &serde_json::Value,
    keys: &[&str],
    max: usize,
    suffix: &str,
) -> Option<String> {
    keys.iter().find_map(|key| {
        arguments
            .get(*key)
            .and_then(|value| value.as_str())
            .map(|value| truncate(value, max, suffix))
    })
}

fn humanize(name: &str) -> String {
    let spaced = name.replace(['.', '_'], " ");
    let mut characters = spaced.chars();
    match characters.next() {
        Some(first) => first.to_uppercase().chain(characters).collect(),
        None => spaced,
    }
}

/// The payload behind a tool row, shown only when that row is opened.
fn push_tool_payload(
    lines: &mut Vec<Line<'static>>,
    content: &str,
    ok: bool,
    width: usize,
    theme: &Theme,
) {
    let inner = width.saturating_sub(4).max(1);
    let style = if ok {
        Style::default().fg(theme.tool)
    } else {
        Style::default().fg(theme.error)
    };
    let body = markdown::markdown_lines_styled(content, inner, theme, style);
    let total = body.len();
    for line in body.into_iter().take(TOOL_LINE_CAP) {
        lines.push(indent(line, "    "));
    }
    if total > TOOL_LINE_CAP {
        lines.push(Line::styled(
            format!(
                "    {} ({} more lines)",
                ellipsis(theme),
                total - TOOL_LINE_CAP
            ),
            theme.muted(),
        ));
    }
}

/// A thinking block, folded to the one line it should have taken in the first place.
///
/// Reasoning is the longest and least actionable text in a transcript, so it does not get to push
/// the answer off the screen. Folded, the line names the block, previews one line of it and says
/// what opens it; `Ctrl+T` renders the whole block. Nothing is discarded — the text stays on the
/// message either way. `live` previews the newest line of a stream rather than its opening, so a
/// folded thought still reads as moving.
fn push_reasoning(
    lines: &mut Vec<Line<'static>>,
    text: &str,
    expanded: bool,
    live: bool,
    key: ThoughtKey,
    marks: &mut Vec<Mark>,
    width: usize,
    theme: &Theme,
) {
    let label = Style::default().fg(theme.accent_dim);
    if text.trim().is_empty() {
        return;
    }
    let heading = if expanded { "Thoughts" } else { "Thought" };
    let heading_text = format!("{} {heading}", theme.thought_glyph());
    // Only the label folds the block, so a drag that starts on the text after it is still a copy.
    marks.push((lines.len(), heading_text.width() as u16, key, false));
    if expanded {
        lines.push(Line::from(vec![
            Span::styled(heading_text, label),
            Span::styled(format!(" {} Ctrl+T folds", theme.bullet()), theme.muted()),
            Span::styled(" · click", Style::default().fg(theme.faint)),
        ]));
        lines.extend(markdown::markdown_lines(text, width, theme));
        return;
    }
    let mut row = text
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty());
    let preview = if live {
        row.next_back().unwrap_or_default()
    } else {
        row.next().unwrap_or_default()
    };
    let count = text.lines().filter(|line| !line.trim().is_empty()).count();
    let mut detail = truncate(preview, THOUGHT_PREVIEW_CAP, ellipsis(theme));
    if count > 1 {
        detail.push_str(&format!(" {} {count} lines", theme.bullet()));
    }
    detail.push_str(&format!(" {} Ctrl+T opens", theme.bullet()));
    lines.push(Line::from(vec![
        Span::styled(heading_text, label),
        Span::styled(format!("  {detail}"), theme.muted()),
        Span::styled(" · click", Style::default().fg(theme.faint)),
    ]));
}

fn push_tool_call(
    lines: &mut Vec<Line<'static>>,
    call: &vikram_contracts::ToolCall,
    width: usize,
    theme: &Theme,
) {
    let bullet = if theme.ascii { "-" } else { "·" };
    lines.push(Line::from(vec![
        Span::styled(format!("{bullet} "), theme.muted()),
        Span::styled("tool call", theme.secondary()),
        Span::styled(format!("  {}", call.name), Style::default().fg(theme.tool)),
    ]));

    let summary = argument_summary(&call.arguments, ARGUMENT_CAP, ellipsis(theme));
    if !summary.is_empty() {
        let inner = width.saturating_sub(2).max(1);
        for line in markdown::wrap_plain(&summary, inner, theme.muted()) {
            lines.push(indent(line, "  "));
        }
    }
}

// ---------------------------------------------------------------------------
// activity
// ---------------------------------------------------------------------------

fn push_activity(
    lines: &mut Vec<Line<'static>>,
    app: &App,
    activity: &Activity,
    tool_index: &ToolIndex,
    session: &SessionView,
    marks: &mut Vec<Mark>,
    width: usize,
    theme: &Theme,
    position: usize,
    selected_activity: Option<usize>,
) {
    // A tool that already has a call block in the transcript is drawn there, as one row. Its
    // activity line is the same event a second time, so it is dropped rather than repeated.
    if let Some(call) = &activity.call {
        if tool_index.paired(call) {
            return;
        }
        // The activity IS the call: the runtime never stores a `ToolCall` block, so this row is
        // the only place a real turn can say what ran. It draws once, with its payload folded.
        if activity.kind == ActivityKind::Tool {
            if let Some((content, ok)) = tool_index.result(call) {
                let key = (ACTIVITY_TOOL, position);
                let waiting = app
                    .approvals
                    .iter()
                    .any(|prompt| prompt.verb == activity_first_word(&activity.text));
                push_activity_tool_row(
                    lines,
                    activity,
                    content,
                    ok,
                    waiting,
                    session.tool_expanded(key),
                    key,
                    marks,
                    width,
                    theme,
                );
                return;
            }
        }
    }
    let selected = selected_activity == Some(position);
    let toggle = if activity.expanded {
        if theme.ascii { "v" } else { "▼" }
    } else {
        if theme.ascii { ">" } else { "▶" }
    };

    // Distinct rendering for file edits / deletes emitted as "fs.edit" / "fs.delete" tools.
    if activity.kind == ActivityKind::Tool
        && (activity.text.starts_with("fs.edit") || activity.text.starts_with("fs.delete"))
        && activity.ok != Some(false)
    {
        push_fs_activity(lines, app, activity, width, theme, toggle, selected);
        return;
    }

    // Tool activities render with a toggle indicator; other kinds keep their status glyph.
    let (glyph, style) = match activity.kind {
        ActivityKind::Tool => match activity.ok {
            // Running: the approved thinking animation, in the accent colour.
            None => (
                app.thinking_frame().to_owned(),
                Style::default().fg(theme.accent),
            ),
            Some(true) => (theme.check().to_owned(), theme.secondary()),
            Some(false) => (theme.cross().to_owned(), Style::default().fg(theme.error)),
        },
        ActivityKind::Notice => (theme.bullet().to_owned(), theme.secondary()),
        ActivityKind::Error => (theme.cross().to_owned(), Style::default().fg(theme.error)),
        ActivityKind::Agent => (theme.subagents_glyph().to_owned(), theme.secondary()),
    };

    let prefix_width = glyph.width() + 1;
    let inner = width.saturating_sub(prefix_width).max(1);
    let mut wrapped = markdown::wrap_plain(&activity.text, inner, style).into_iter();

    // Toggle indicator prefix for tool activities.
    let is_tool = activity.kind == ActivityKind::Tool;

    match wrapped.next() {
        Some(first) => {
            let mut spans = Vec::new();
            if is_tool {
                spans.push(Span::styled(
                    format!("{toggle} "),
                    Style::default().fg(theme.accent),
                ));
            }
            if selected {
                spans.push(Span::styled("> ", theme.muted()));
            }
            spans.push(Span::styled(glyph.clone(), style));
            spans.push(Span::raw(" "));
            spans.extend(first.spans);
            lines.push(Line::from(spans));
        }
        None => {
            let mut spans = Vec::new();
            if is_tool {
                spans.push(Span::styled(
                    format!("{toggle} "),
                    Style::default().fg(theme.accent),
                ));
            }
            if selected {
                spans.push(Span::styled("> ", theme.muted()));
            }
            spans.push(Span::styled(glyph, style));
            lines.push(Line::from(spans));
            return;
        }
    }

    // Collapsed: the glance stops at the first line. Expanded: reveal every wrapped
    // line so the toggle indicator (▶/▼) actually changes what the user sees. Only
    // `fs.edit` / `fs.delete` activities took this path before, so a plain tool call
    // would flip its arrow without ever growing — that was the reported bug.
    if activity.expanded {
        for line in wrapped {
            lines.push(indent(line, &" ".repeat(prefix_width)));
        }
    } else if wrapped.next().is_some() {
        // Collapsed still trims a wrapped second line rather than leaving a dangling
        // iterator; the content is intentionally capped at the first line.
    }
}

/// Render an `fs.edit` / `fs.delete` activity with a distinct first line.
fn push_fs_activity(
    lines: &mut Vec<Line<'static>>,
    _app: &App,
    activity: &Activity,
    width: usize,
    theme: &Theme,
    toggle: &str,
    selected: bool,
) {
    let is_edit = activity.text.starts_with("fs.edit");
    let label = if is_edit { "Edited" } else { "Deleted" };
    let path = extract_file_path(&activity.text).unwrap_or_default();

    let style = match activity.ok {
        None => Style::default().fg(theme.accent),
        Some(true) => theme.secondary(),
        Some(false) => Style::default().fg(theme.error),
    };

    let mut spans = vec![Span::styled(
        format!("{toggle} "),
        Style::default().fg(theme.accent),
    )];
    if selected {
        spans.push(Span::styled("> ", theme.muted()));
    }
    spans.push(Span::styled(format!("→ {label} {path}"), style));
    lines.push(Line::from(spans));

    // Expanded: show the full content (diff / deleted content).
    if !activity.expanded {
        return;
    }

    // The summary after the " · " separator.
    let summary = activity
        .text
        .split_once('·')
        .map(|(_, rest)| rest.trim().to_owned())
        .unwrap_or_default();

    if is_edit {
        if !summary.is_empty() {
            let inner = width.saturating_sub(2).max(1);
            for line in markdown::wrap_plain(&summary, inner, style) {
                lines.push(indent(line, "  "));
            }
        }
    } else {
        // Extract the deleted content (after the "--- deleted content ---" marker).
        let content = if let Some(idx) = summary.find("--- deleted content ---") {
            let after = &summary[idx + "--- deleted content ---".len()..];
            after.trim_start_matches('\n').to_owned()
        } else {
            String::new()
        };
        let content_lines: Vec<&str> = content.lines().collect();
        let cap = 10usize;
        let total = content_lines.len();
        for line in content_lines.into_iter().take(cap) {
            lines.push(Line::styled(format!("  {line}"), style));
        }
        if total > cap {
            lines.push(Line::styled(
                format!("  {} ({} more lines)", ellipsis(theme), total - cap),
                theme.muted(),
            ));
        }
    }
}

// ---------------------------------------------------------------------------
// helpers
// ---------------------------------------------------------------------------

fn separator_line(width: usize, theme: &Theme) -> Line<'static> {
    Line::styled(
        theme.separator().repeat(width.clamp(1, SEPARATOR_CAP)),
        theme.frame(),
    )
}

/// `· system` in Unicode, plain `system` where the terminal cannot draw the dot.
fn dim_label(theme: &Theme, name: &str) -> String {
    if theme.ascii {
        name.to_owned()
    } else {
        format!("{} {name}", theme.bullet())
    }
}

fn ellipsis(theme: &Theme) -> &'static str {
    if theme.ascii { "..." } else { "…" }
}

fn indent(line: Line<'static>, prefix: &str) -> Line<'static> {
    let mut spans: Vec<Span<'static>> = Vec::with_capacity(line.spans.len() + 1);
    spans.push(Span::raw(prefix.to_owned()));
    spans.extend(line.spans);
    Line::from(spans)
}

/// A compact one-line rendering of a tool call's arguments.
///
/// `arguments` is a JSON value; the exact type is not named here so this stays a formatting
/// concern and the crate needs no JSON dependency of its own.
fn argument_summary(arguments: &impl std::fmt::Display, max: usize, suffix: &str) -> String {
    let raw = arguments.to_string();
    let compact = raw.split_whitespace().collect::<Vec<_>>().join(" ");
    if compact.is_empty() || matches!(compact.as_str(), "null" | "{}" | "[]") {
        return String::new();
    }
    truncate(&compact, max, suffix)
}

fn truncate(text: &str, max: usize, suffix: &str) -> String {
    if text.width() <= max {
        return text.to_owned();
    }
    let budget = max.saturating_sub(suffix.width());
    let mut out = String::new();
    let mut used = 0usize;
    for ch in text.chars() {
        let cell = ch.width().unwrap_or(0);
        if used + cell > budget {
            break;
        }
        used += cell;
        out.push(ch);
    }
    out.push_str(suffix);
    out
}

fn clamp_u16(value: usize) -> u16 {
    u16::try_from(value).unwrap_or(u16::MAX)
}

/// Extract the file path from an activity text of the form
/// "fs.edit · {summary}" or "fs.delete · {summary}".
pub fn extract_file_path(text: &str) -> Option<String> {
    let summary = text
        .split_once('·')
        .map(|(_, rest)| rest.trim())
        .unwrap_or("");
    let rest = if text.trim_start().starts_with("fs.edit") {
        summary.strip_prefix("edited ")?
    } else if text.trim_start().starts_with("fs.delete") {
        summary.strip_prefix("deleted ")?
    } else {
        return None;
    };
    if text.trim_start().starts_with("fs.edit") {
        // path runs up to the ": old -> new bytes" suffix
        if let Some(idx) = rest.find(": ") {
            Some(rest[..idx].to_owned())
        } else {
            Some(rest.to_owned())
        }
    } else {
        // path runs up to the first newline (or end)
        Some(rest.split('\n').next().unwrap_or(rest).to_owned())
    }
}

// Aliases for callers / tests that use a different helper name.
pub fn extract_path(text: &str) -> Option<String> {
    extract_file_path(text)
}
pub fn extract_activity_path(text: &str) -> Option<String> {
    extract_file_path(text)
}
pub fn file_path_from_activity(text: &str) -> Option<String> {
    extract_file_path(text)
}
pub fn extract_file_path_from_activity(text: &str) -> Option<String> {
    extract_file_path(text)
}
pub fn extract_fs_path(text: &str) -> Option<String> {
    extract_file_path(text)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::app::Streaming;
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;
    use vikram_contracts::{SessionId, ToolCall, ToolCallId, Trust, TurnId};
    use vikram_runtime::types::{SessionSnapshot, TurnState};

    fn sample_session() -> SessionView {
        SessionView::from_snapshot(SessionSnapshot {
            id: SessionId::generate(),
            title: "Session 1".into(),
            cwd: "/work".into(),
            model: None,
            messages: Vec::new(),
            turn: TurnState::Idle,
            tokens: Default::default(),
            created_ms: 0,
            updated_ms: 0,
            agent: None,
            queued: 0,
        })
    }

    fn text_of(line: &Line<'_>) -> String {
        line.spans
            .iter()
            .map(|span| span.content.as_ref())
            .collect()
    }

    fn transcript(lines: &[Line<'_>]) -> Vec<String> {
        lines.iter().map(text_of).collect()
    }

    /// A crude clock detector: two digits, a colon, two digits.
    fn looks_like_a_clock(text: &str) -> bool {
        let chars: Vec<char> = text.chars().collect();
        chars.windows(5).any(|window| {
            window[0].is_ascii_digit()
                && window[1].is_ascii_digit()
                && window[2] == ':'
                && window[3].is_ascii_digit()
                && window[4].is_ascii_digit()
        })
    }

    fn assert_no_timestamps(lines: &[String]) {
        for line in lines {
            assert!(!looks_like_a_clock(line), "timestamp-like text: {line:?}");
            assert!(!line.contains("[1"), "bracketed index-like text: {line:?}");
        }
    }

    #[test]
    fn an_empty_session_renders_no_fabricated_welcome() {
        let app = App::new(false);
        let session = sample_session();
        let theme = Theme::new(false);
        let lines = transcript(&conversation_lines(&app, &session, 76, &theme));
        assert!(lines.is_empty(), "expected no lines, got {lines:?}");
    }

    #[test]
    fn user_and_assistant_messages_are_labelled_without_timestamps() {
        let app = App::new(false);
        let mut session = sample_session();
        session.messages.push(Message::user("hello there"));
        session.messages.push(Message::text(
            Role::Assistant,
            "Hi. Here is **bold** and `code`.\n\n- one\n- two",
            Trust::Host,
        ));
        let theme = Theme::new(false);
        let lines = transcript(&conversation_lines(&app, &session, 76, &theme));
        let all = lines.join("\n");

        assert!(all.contains("❯ User"), "user label missing: {all}");
        assert!(all.contains("VIKRAM"), "assistant label missing: {all}");
        assert!(!all.contains("Welcome, sir!"), "welcome leaked: {all}");
        assert!(all.contains("hello there"));
        assert!(all.contains("bold") && !all.contains('*'));
        assert!(all.contains("• one") && all.contains("• two"));
        assert!(all.contains('─'), "separator missing under the user turn");
        assert_no_timestamps(&lines);
    }

    #[test]
    fn ascii_mode_uses_ascii_labels() {
        let app = App::new(true);
        let mut session = sample_session();
        session.messages.push(Message::user("hello"));
        let theme = Theme::new(true);
        let all = transcript(&conversation_lines(&app, &session, 60, &theme)).join("\n");
        assert!(all.contains("> User"), "ascii user label missing: {all}");
        assert!(!all.contains('❯'));
        assert!(!all.contains('─'));
    }

    #[test]
    fn a_system_message_and_an_orphan_result_render_dim_and_indented() {
        let app = App::new(false);
        let mut session = sample_session();
        session.messages.push(Message::system("policy loaded"));
        let content = (0..30)
            .map(|index| format!("row {index}"))
            .collect::<Vec<_>>()
            .join("\n");
        session
            .messages
            .push(Message::tool_result(ToolCallId::generate(), content, false));
        let theme = Theme::new(false);
        let lines = conversation_lines(&app, &session, 76, &theme);
        let all = transcript(&lines).join("\n");

        assert!(all.contains("· system"), "system label missing: {all}");
        assert!(
            !all.contains("· tool"),
            "a result with no call must not reopen the old tool heading: {all}"
        );
        assert!(all.contains("policy loaded"));
        assert!(all.contains("(18 more lines)"), "cap missing: {all}");
        assert!(all.contains("row 0"));

        // The failed tool result is tinted with the error colour, not the tool colour.
        let row = lines
            .iter()
            .find(|line| text_of(line).contains("row 0"))
            .expect("tool line missing");
        assert!(
            row.spans
                .iter()
                .any(|span| span.style.fg == Some(theme.error))
        );
    }

    #[test]
    fn streaming_shows_the_text_and_the_thinking_frame() {
        let app = App::new(false);
        let mut session = sample_session();
        session.streaming = Some(Streaming {
            turn: TurnId::generate(),
            text: "partial answer".into(),
        });
        session.activity.push(Activity {
            kind: ActivityKind::Tool,
            text: "shell · ls".into(),
            call: None,
            ok: None,
            expanded: false,
        });
        let theme = Theme::new(false);
        let lines = transcript(&conversation_lines(&app, &session, 76, &theme));
        let all = lines.join("\n");

        assert!(all.contains("partial answer"), "stream text missing: {all}");
        assert!(all.contains("Working"), "working line missing: {all}");
        assert!(all.contains("VIKRAM"), "stream label missing: {all}");
        assert!(
            all.contains(app.thinking_frame().to_owned().as_str()),
            "thinking frame missing: {all}"
        );
        assert!(all.contains("shell"), "activity missing: {all}");
        assert!(!all.contains("Welcome, sir!"));
        assert_no_timestamps(&lines);
    }

    #[test]
    fn completed_and_failed_activity_use_their_glyphs() {
        let app = App::new(false);
        let mut session = sample_session();
        session.messages.push(Message::user("go"));
        session.activity.push(Activity {
            kind: ActivityKind::Tool,
            text: "shell · ls".into(),
            call: None,
            ok: Some(true),
            expanded: false,
        });
        session.activity.push(Activity {
            kind: ActivityKind::Tool,
            text: "shell · cat".into(),
            call: None,
            ok: Some(false),
            expanded: false,
        });
        session.activity.push(Activity {
            kind: ActivityKind::Error,
            text: "provider unreachable".into(),
            call: None,
            ok: Some(false),
            expanded: false,
        });
        session.activity.push(Activity {
            kind: ActivityKind::Notice,
            text: "screen redrawn".into(),
            call: None,
            ok: None,
            expanded: false,
        });
        let theme = Theme::new(false);
        let all = transcript(&conversation_lines(&app, &session, 76, &theme)).join("\n");

        assert!(all.contains(theme.check()), "success glyph missing: {all}");
        assert!(all.contains(theme.cross()), "failure glyph missing: {all}");
        assert!(all.contains("provider unreachable"));
        assert!(all.contains("screen redrawn"));
    }

    #[test]
    fn expanding_a_plain_tool_activity_reveals_more_lines() {
        let theme = Theme::new(false);
        let long = "shell · ls -la /very/long/path/that/wraps/to/two/lines/and/beyond";

        let collapsed_app = App::new(false);
        let mut s1 = sample_session();
        s1.activity.push(Activity { kind: ActivityKind::Tool, text: long.into(), call: None, ok: Some(true), expanded: false });
        let collapsed = conversation_lines(&collapsed_app, &s1, 40, &theme);

        let expanded_app = App::new(false);
        let mut s2 = sample_session();
        s2.activity.push(Activity { kind: ActivityKind::Tool, text: long.into(), call: None, ok: Some(true), expanded: true });
        let expanded = conversation_lines(&expanded_app, &s2, 40, &theme);

        assert!(
            expanded.len() > collapsed.len(),
            "a plain tool activity must grow when expanded (collapsed={}, expanded={})",
            collapsed.len(),
            expanded.len(),
        );
    }

    #[test]
    fn a_very_long_transcript_still_draws() {
        let app = App::new(false);
        let mut session = sample_session();
        for index in 0..200 {
            session
                .messages
                .push(Message::user(format!("message number {index}")));
            session.messages.push(Message::text(
                Role::Assistant,
                format!("reply number {index}"),
                Trust::Host,
            ));
        }
        let theme = Theme::new(false);
        let lines = conversation_lines(&app, &session, 76, &theme);
        assert!(lines.len() > 1000, "expected a long transcript");

        let backend = TestBackend::new(80, 12);
        let mut terminal = Terminal::new(backend).expect("terminal");
        terminal
            .draw(|frame| {
                conversation(frame, frame.area(), &app, &session, &theme);
            })
            .expect("draw");
    }

    #[test]
    fn drawing_the_widget_does_not_panic() {
        let app = App::new(false);
        let mut session = sample_session();
        session.messages.push(Message::user("hello"));
        session.messages.push(Message::text(
            Role::Assistant,
            "# Heading\n\nbody text\n\n```rust\nfn main() {}\n```",
            Trust::Host,
        ));
        let theme = Theme::new(false);

        let backend = TestBackend::new(80, 24);
        let mut terminal = Terminal::new(backend).expect("terminal");
        terminal
            .draw(|frame| {
                conversation(frame, frame.area(), &app, &session, &theme);
            })
            .expect("draw");
        let buffer = terminal.backend().buffer().clone();
        let rendered: String = buffer.content().iter().map(|cell| cell.symbol()).collect();
        assert!(rendered.contains("VIKRAM"), "assistant label not drawn");
        assert!(rendered.contains("hello"), "user text not drawn");
    }

    #[test]
    fn drawing_an_empty_and_a_scrolled_session_does_not_panic() {
        let app = App::new(false);
        let theme = Theme::new(false);

        let session = sample_session();
        let backend = TestBackend::new(40, 10);
        let mut terminal = Terminal::new(backend).expect("terminal");
        terminal
            .draw(|frame| {
                conversation(frame, frame.area(), &app, &session, &theme);
            })
            .expect("draw");

        let mut scrolled = sample_session();
        for index in 0..40 {
            scrolled
                .messages
                .push(Message::user(format!("line {index}")));
        }
        scrolled.follow = false;
        scrolled.scroll = 1_000_000;
        let backend = TestBackend::new(40, 10);
        let mut terminal = Terminal::new(backend).expect("terminal");
        terminal
            .draw(|frame| {
                conversation(frame, frame.area(), &app, &scrolled, &theme);
            })
            .expect("draw");
    }

    #[test]
    fn a_pending_question_renders_its_prompt_and_the_answer_being_typed() {
        let mut app = App::new(false);
        app.pending_question = Some(crate::app::PendingQuestion {
            request_id: "ask-1-0".into(),
            question: "Which file should I edit?".into(),
            choices: vec!["main.rs".into(), "lib.rs".into()],
            input: "main".into(),
        });
        let session = sample_session();
        let theme = Theme::new(false);
        let lines = transcript(&conversation_lines(&app, &session, 76, &theme));
        let all = lines.join("\n");

        assert!(all.contains("VIKRAM asks"), "question label missing: {all}");
        assert!(all.contains("Which file should I edit?"), "{all}");
        assert!(all.contains("main.rs") && all.contains("lib.rs"), "{all}");
        assert!(all.contains("Answer: main"), "answer line missing: {all}");
    }

    #[test]
    fn no_question_prompt_is_drawn_when_nothing_is_pending() {
        let app = App::new(false);
        let session = sample_session();
        let theme = Theme::new(false);
        let all = transcript(&conversation_lines(&app, &session, 76, &theme)).join("\n");
        assert!(!all.contains("Answer:"), "{all}");
    }

    /// An assistant turn that thought at length and then answered.
    fn thinking_then_answer(lines: usize) -> Message {
        let thought = (1..=lines)
            .map(|i| format!("thought line {i}"))
            .collect::<Vec<_>>()
            .join("\n");
        Message {
            role: Role::Assistant,
            blocks: vec![
                Block::Reasoning {
                    text: thought,
                    trust: Trust::Model,
                },
                Block::Text {
                    text: "the answer".into(),
                    trust: Trust::Model,
                },
            ],
        }
    }

    #[test]
    fn thinking_is_folded_so_it_cannot_push_the_answer_off_the_screen() {
        let app = App::new(false);
        let mut session = sample_session();
        session.messages.push(thinking_then_answer(24));
        let theme = Theme::new(false);
        let all = transcript(&conversation_lines(&app, &session, 76, &theme)).join("\n");

        assert!(all.contains("Ctrl+T opens"), "no way to open it: {all}");
        assert!(all.contains("24 lines"), "the fold hides how big it is: {all}");
        assert!(
            all.contains("thought line 1"),
            "the preview should show what it is thinking about: {all}"
        );
        assert!(
            !all.contains("thought line 7"),
            "a folded block must not print its body: {all}"
        );
        assert_eq!(
            all.lines().filter(|line| line.contains("thought line")).count(),
            1,
            "folding is one line per block, not one per thought: {all}"
        );
        assert!(all.contains("the answer"), "the answer must survive: {all}");
    }

    #[test]
    fn opening_the_thinking_shows_all_of_it_and_says_how_to_fold_it_again() {
        let app = App::new(false);
        let mut session = sample_session();
        session.expanded_thought = true;
        session.messages.push(thinking_then_answer(24));
        let theme = Theme::new(false);
        let all = transcript(&conversation_lines(&app, &session, 76, &theme)).join("\n");

        assert!(all.contains("thought line 24"), "not the whole block: {all}");
        assert!(all.contains("Ctrl+T folds"), "no way to close it: {all}");
        assert!(!all.contains("Ctrl+T opens"), "{all}");
    }

    #[test]
    fn a_folded_live_thought_shows_the_newest_line_rather_than_the_whole_stream() {
        let app = App::new(false);
        let mut session = sample_session();
        session.streaming_thought = Some(
            "oldest line\nsecond thought line of the stream\nweighing the third option here"
                .into(),
        );
        let theme = Theme::new(false);
        let all = transcript(&conversation_lines(&app, &session, 76, &theme)).join("\n");

        assert!(
            all.contains("weighing the third option here"),
            "a folded thought still has to move: {all}"
        );
        assert!(
            !all.contains("oldest line") && !all.contains("second thought line"),
            "the whole buffer leaked through the fold: {all}"
        );
        assert!(all.contains("3 lines"), "{all}");
    }

    #[test]
    fn thinking_that_said_nothing_draws_no_header() {
        let mut session = sample_session();
        session.streaming = Some(Streaming {
            turn: TurnId::generate(),
            text: "the answer".into(),
        });
        for blank in ["", "   ", "\n\n"] {
            session.streaming_thought = Some(blank.into());
            let theme = Theme::new(false);
            let all = transcript(&conversation_lines(&App::new(false), &session, 76, &theme))
                .join("\n");
            assert!(!all.contains("Thought"), "empty thinking drew a header: {all}");
            assert!(all.contains("the answer"), "{all}");
        }
    }

    #[test]
    fn a_short_thought_does_not_claim_to_have_more_lines_than_it_has() {
        let app = App::new(false);
        let mut session = sample_session();
        session.messages.push(thinking_then_answer(1));
        let theme = Theme::new(false);
        let all = transcript(&conversation_lines(&app, &session, 76, &theme)).join("\n");

        assert!(!all.contains("1 lines"), "singular counted as plural: {all}");
        assert!(all.contains("thought line 1"), "{all}");
    }

    #[test]
    fn an_activity_call_and_its_result_draw_one_folded_row() {
        let mut session = sample_session();
        let id = ToolCall::new("web.search", serde_json::json!({ "query": "todays news" })).id;
        session
            .messages
            .push(Message::tool_result(id.clone(), "1. first hit\n2. second hit", true));
        session.activity.push(Activity {
            kind: ActivityKind::Tool,
            text: "web.search  limit=10 query=todays news".to_owned(),
            call: Some(id),
            ok: Some(true),
            expanded: false,
        });
        let app = App::new(false);

        let folded = transcript(&conversation_lines(&app, &session, 80, &Theme::new(false)));
        let rows: Vec<&String> = folded
            .iter()
            .filter(|row| row.contains("Web search"))
            .collect();
        assert_eq!(rows.len(), 1, "one call draws one row: {folded:#?}");
        assert!(
            rows[0].contains("todays news"),
            "the argument is the label: {rows:#?}"
        );
        assert!(
            !folded.iter().any(|row| row.contains("second hit")),
            "the payload stays folded: {folded:#?}"
        );
        assert!(
            !folded.iter().any(|row| row.contains("· tool")),
            "the old heading is gone: {folded:#?}"
        );

        session.toggle_tool((ACTIVITY_TOOL, 0));
        let open = transcript(&conversation_lines(&app, &session, 80, &Theme::new(false)));
        assert!(
            open.iter().any(|row| row.contains("second hit")),
            "clicking the row reveals the payload: {open:#?}"
        );
    }

    #[test]
    fn a_call_and_its_result_draw_one_row_and_keep_the_payload_folded() {
        let app = App::new(false);
        let mut session = sample_session();
        let call = ToolCall::new("fs.read", serde_json::json!({"path": "Cargo.toml"}));
        let id = call.id.clone();
        session.messages.push(Message {
            role: Role::Assistant,
            blocks: vec![Block::ToolCall { call }],
        });
        session.messages.push(Message {
            role: Role::Assistant,
            blocks: vec![Block::ToolResult {
                call: id.clone(),
                content: "line one\nline two".into(),
                trust: Trust::Host,
                ok: true,
            }],
        });
        session
            .messages
            .push(Message::tool_result(id, "line one\nline two", true));

        let theme = Theme::new(false);
        let rows = transcript(&conversation_lines(&app, &session, 76, &theme));
        let calls: Vec<&String> = rows.iter().filter(|row| row.contains("Read")).collect();
        assert_eq!(calls.len(), 1, "one call is one row: {rows:?}");
        assert!(calls[0].contains("Cargo.toml"), "label names the file: {rows:?}");
        assert!(
            !rows.iter().any(|row| row.contains("line two")),
            "the payload starts folded: {rows:?}"
        );
        assert!(
            !rows.iter().any(|row| row.contains("· tool")),
            "the old tool heading is gone: {rows:?}"
        );

        session.toggle_tool((0, 0));
        let opened = transcript(&conversation_lines(&app, &session, 76, &theme));
        assert!(
            opened.iter().any(|row| row.contains("line two")),
            "folding the row opens the payload: {opened:?}"
        );
    }
}
