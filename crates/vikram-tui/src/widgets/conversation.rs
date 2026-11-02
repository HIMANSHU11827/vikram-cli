//! The conversation transcript.
//!
//! Rendering is a pure function of [`App`] and [`SessionView`]: nothing here mutates state, emits a
//! command, or invents information. In particular there is no clock anywhere — a transcript line
//! says who spoke and what was said, never when.

use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::Style;
use ratatui::text::{Line, Span};
use ratatui::widgets::Paragraph;
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};
use vikram_contracts::{Block, Message, Role};

use crate::app::{Activity, ActivityKind, App, PendingQuestion, SessionView};
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

pub fn conversation(
    frame: &mut Frame,
    area: Rect,
    app: &App,
    session: &SessionView,
    theme: &Theme,
) {
    let width = area.width.saturating_sub(2) as usize;
    let lines = conversation_lines(app, session, width, theme);
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
}

pub fn conversation_lines(
    app: &App,
    session: &SessionView,
    width: usize,
    theme: &Theme,
) -> Vec<Line<'static>> {
    let width = width.max(4);
    let mut lines: Vec<Line<'static>> = Vec::new();

    // An empty session renders nothing: the interface does not fabricate a greeting or sample
    // conversation. Real content arrives through the runtime's message stream.

    for message in &session.messages {
        match message.role {
            Role::User => push_user(&mut lines, message, session.expanded_thought, width, theme),
            Role::Assistant => {
                push_assistant(&mut lines, message, session.expanded_thought, width, theme)
            }
            Role::System => {
                push_system(&mut lines, message, session.expanded_thought, width, theme)
            }
            Role::Tool => push_tool(&mut lines, message, width, theme),
        }
    }

    for (idx, activity) in session.activity.iter().enumerate() {
        push_activity(
            &mut lines,
            app,
            activity,
            width,
            theme,
            idx,
            session.selected_activity,
        );
    }

    if session.streaming.is_some() || session.streaming_thought.is_some() {
        lines.push(Line::from(""));
        lines.push(Line::styled("VIKRAM", theme.title()));
        if let Some(thought) = &session.streaming_thought {
            push_reasoning(
                &mut lines,
                thought,
                session.expanded_thought,
                true,
                width,
                theme,
            );
        }
        if let Some(stream) = &session.streaming {
            lines.extend(markdown::markdown_lines(&stream.text, width, theme));
            lines.push(Line::from(vec![
                Span::styled(
                    app.thinking_frame().to_owned(),
                    Style::default().fg(theme.accent),
                ),
                Span::styled(" Working...", Style::default().fg(theme.accent)),
            ]));
        }
    }

    if let Some(pending) = &app.pending_question {
        push_question_prompt(&mut lines, pending, width, theme);
    }

    lines
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
    expanded_thought: bool,
    width: usize,
    theme: &Theme,
) {
    let base = Style::default().fg(theme.user);
    lines.push(Line::from(""));
    lines.push(Line::styled(
        format!("{} User", theme.prompt()),
        theme.muted(),
    ));
    for block in &message.blocks {
        match block {
            Block::Text { text, .. } => {
                lines.extend(markdown::markdown_lines_styled(text, width, theme, base))
            }
            Block::Reasoning { text, .. } => {
                push_reasoning(lines, text, expanded_thought, false, width, theme)
            }
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
    expanded_thought: bool,
    width: usize,
    theme: &Theme,
) {
    lines.push(Line::from(""));
    lines.push(Line::styled("VIKRAM", theme.title()));
    for block in &message.blocks {
        match block {
            Block::Text { text, .. } => lines.extend(markdown::markdown_lines(text, width, theme)),
            Block::Reasoning { text, .. } => {
                push_reasoning(lines, text, expanded_thought, false, width, theme)
            }
            Block::ToolCall { call } => push_tool_call(lines, call, width, theme),
            Block::ToolResult { content, ok, .. } => {
                push_tool_body(lines, &[content.as_str()], *ok, width, theme);
            }
        }
    }
}

fn push_system(
    lines: &mut Vec<Line<'static>>,
    message: &Message,
    expanded_thought: bool,
    width: usize,
    theme: &Theme,
) {
    lines.push(Line::from(""));
    lines.push(Line::styled(dim_label(theme, "system"), theme.muted()));
    let style = theme.muted();
    for block in &message.blocks {
        match block {
            Block::Text { text, .. } => {
                lines.extend(markdown::markdown_lines_styled(text, width, theme, style))
            }
            Block::Reasoning { text, .. } => {
                push_reasoning(lines, text, expanded_thought, false, width, theme)
            }
            Block::ToolCall { call } => push_tool_call(lines, call, width, theme),
            Block::ToolResult { content, ok, .. } => {
                push_tool_body(lines, &[content.as_str()], *ok, width, theme);
            }
        }
    }
}

fn push_tool(lines: &mut Vec<Line<'static>>, message: &Message, width: usize, theme: &Theme) {
    let mut contents: Vec<&str> = Vec::new();
    let mut ok = true;
    for block in &message.blocks {
        match block {
            Block::ToolResult {
                content,
                ok: success,
                ..
            } => {
                ok &= *success;
                contents.push(content.as_str());
            }
            Block::Text { text, .. } => contents.push(text.as_str()),
            Block::Reasoning { text, .. } => contents.push(text.as_str()),
            Block::ToolCall { call } => push_tool_call(lines, call, width, theme),
        }
    }
    push_tool_body(lines, &contents, ok, width, theme);
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
    width: usize,
    theme: &Theme,
) {
    let label = Style::default().fg(theme.accent_dim);
    if text.trim().is_empty() {
        return;
    }
    if expanded {
        lines.push(Line::from(vec![
            Span::styled(format!("{} Thoughts", theme.thought_glyph()), label),
            Span::styled(format!(" {} Ctrl+T folds", theme.bullet()), theme.muted()),
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
        Span::styled(format!("{} Thought", theme.thought_glyph()), label),
        Span::styled(format!("  {detail}"), theme.muted()),
    ]));
}

/// A tool result is dim-labelled, indented, tinted by outcome, and capped in length.
fn push_tool_body(
    lines: &mut Vec<Line<'static>>,
    contents: &[&str],
    ok: bool,
    width: usize,
    theme: &Theme,
) {
    lines.push(Line::from(""));
    lines.push(Line::styled(dim_label(theme, "tool"), theme.muted()));

    let style = if ok {
        Style::default().fg(theme.tool)
    } else {
        Style::default().fg(theme.error)
    };
    let inner = width.saturating_sub(2).max(1);
    let mut rendered: Vec<Line<'static>> = Vec::new();
    for content in contents {
        rendered.extend(markdown::markdown_lines_styled(
            content, inner, theme, style,
        ));
    }

    let total = rendered.len();
    for line in rendered.into_iter().take(TOOL_LINE_CAP) {
        lines.push(indent(line, "  "));
    }
    if total > TOOL_LINE_CAP {
        lines.push(Line::styled(
            format!(
                "  {} ({} more lines)",
                ellipsis(theme),
                total - TOOL_LINE_CAP
            ),
            theme.muted(),
        ));
    }
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
    width: usize,
    theme: &Theme,
    index: usize,
    selected_activity: Option<usize>,
) {
    let selected = selected_activity == Some(index);
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
    use vikram_contracts::{SessionId, ToolCallId, Trust, TurnId};
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
    fn a_system_and_a_tool_message_render_dim_and_indented() {
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
        assert!(all.contains("· tool"), "tool label missing: {all}");
        assert!(all.contains("policy loaded"));
        assert!(all.contains("(18 more lines)"), "cap missing: {all}");
        assert!(all.contains("  row 0"));

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
        assert!(all.contains("Working..."), "working line missing: {all}");
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

        let mut collapsed = App::new(false);
        let mut s1 = sample_session();
        s1.activity.push(Activity { kind: ActivityKind::Tool, text: long.into(), call: None, ok: Some(true), expanded: false });
        let collapsed = conversation_lines(&collapsed, &s1, 40, &theme);

        let mut expanded = App::new(false);
        let mut s2 = sample_session();
        s2.activity.push(Activity { kind: ActivityKind::Tool, text: long.into(), call: None, ok: Some(true), expanded: true });
        let expanded = conversation_lines(&expanded, &s2, 40, &theme);

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
            .draw(|frame| conversation(frame, frame.area(), &app, &session, &theme))
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
            .draw(|frame| conversation(frame, frame.area(), &app, &session, &theme))
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
            .draw(|frame| conversation(frame, frame.area(), &app, &session, &theme))
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
            .draw(|frame| conversation(frame, frame.area(), &app, &scrolled, &theme))
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
}

