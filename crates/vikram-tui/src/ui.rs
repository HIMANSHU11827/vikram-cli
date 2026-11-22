//! Rendering entry point. Widgets live in [`crate::widgets`].

use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Paragraph};
use unicode_width::UnicodeWidthStr;

use crate::app::{App, Focus, Overlay};
use crate::layout;
use crate::theme::Theme;
use crate::widgets;

pub fn draw(frame: &mut Frame, app: &mut App) {
    let theme = Theme::new(app.ascii).with_colours(app.theme_bg, app.theme_accent);
    let area = frame.area();
    app.thought_hits.clear();
    // Every hit region is rewritten by the widget that draws it, and only when that widget draws.
    // A panel that is not on screen this frame — closed overlay, hidden bots pane, a terminal too
    // small to lay out — must not leave the last frame's rectangles answering clicks.
    app.tab_hits.clear();
    app.new_session_x = None;
    app.picker_rows.clear();
    app.suggestion_rows.clear();
    app.picker_x = None;
    app.picker_verbs.clear();
    app.overlay_clicks.clear();
    app.overlay_rect = None;
    app.bot_hits.clear();
    app.bots_rect = None;
    app.conversation_rect = None;

    // Painted once, over the whole window, before anything is drawn on it. Every widget after this
    // writes a foreground and leaves the background alone, so one pass is enough — and a second
    // pass at the end would flatten every band the widgets painted on purpose. The tiny-terminal
    // branch paints its own, since it never reaches this line.
    paint_background(frame, area, &theme);

    if area.width < 24 || area.height < 6 {
        let message =
            Paragraph::new(Line::styled("terminal too small", theme.secondary())).centered();
        frame.render_widget(message, area);
        return;
    }

    let show_hive = !app.rosters.is_empty();
    let show_bots = app.show_bots && !app.bots.is_empty();
    let input_lines = app
        .display_session()
        .map(|s| s.editor.line_count())
        .unwrap_or(1);
    // A notice is feedback the operator asked for by pressing something. It gets a row of its own
    // directly above the input so it can never be confused with the transcript or the status line.
    let show_notice = app.live_notice().is_some();
    let regions = layout::layout(area, show_hive, show_bots, input_lines, show_notice);
    app.bots_rect = regions.bots.map(|r| (r.x, r.y, r.width, r.height));
    app.conversation_rect = Some((
        regions.conversation.x,
        regions.conversation.y,
        regions.conversation.width,
        regions.conversation.height,
    ));

    widgets::session_tabs(frame, regions.tabs, app, &theme);

    // While a tool waits for yes/no, the question is docked in rows reserved below the transcript.
    // Without the reservation the panel clears the rows it is asking about, which is exactly what
    // the operator needs to read.
    let approvals_open = matches!(app.overlay, Some(Overlay::Approvals));
    let reserved = if approvals_open {
        widgets::approval_panel_height(app, &theme)
            .min(regions.conversation.height / 2)
            .min(regions.conversation.height.saturating_sub(3))
    } else {
        0
    };
    let conversation = Rect {
        height: regions.conversation.height.saturating_sub(reserved),
        ..regions.conversation
    };
    let question = Rect {
        y: regions.conversation.y + conversation.height,
        height: reserved,
        ..regions.conversation
    };

    let thought_hits = match app.display_session() {
        // A session that has not said anything yet is the same state as no session at all from the
        // operator's side: an empty area with a cursor in the input. Both get the card. Activity is
        // checked as well as messages, because a session can be all tool rows and no prose.
        Some(session) if !(session.messages.is_empty() && session.activity.is_empty()) => {
            widgets::conversation(frame, conversation, app, session, &theme)
        }
        _ => {
            // The card is for an empty box. The moment a prefix opens its list the operator is
            // already working, and two surfaces fighting over the same rows read as a drawing bug.
            if app.suggestions.is_none() {
                render_empty(frame, conversation, &theme);
            }
            Vec::new()
        }
    };
    app.thought_hits = thought_hits;

    if let Some(hive_area) = regions.hive {
        widgets::hive_panel(frame, hive_area, app, &theme);
    }
    if let Some(bots_area) = regions.bots {
        widgets::bots_panel(frame, bots_area, app, &theme);
    }

    if let Some(notice_area) = regions.notice {
        widgets::plasma(frame, notice_area, app, &theme);
    }

    render_input(frame, regions.input, app, &theme);
    if app.suggestions.is_some() {
        widgets::suggestions_popup(frame, regions.input, app, &theme);
    }
    widgets::status_bar(frame, regions.status, app, &theme);

    if let Some(shortcuts_area) = regions.shortcuts {
        widgets::shortcuts(frame, shortcuts_area, app, &theme);
    }

    if app.overlay.is_some() {
        // Panels dock onto the last rows of the transcript, directly above the input box, so the
        // conversation they act on stays readable while they are open. The approvals panel gets
        // only the reserved strip, so it cannot cover the transcript at all.
        let area = if approvals_open && reserved > 0 {
            question
        } else {
            regions.conversation
        };
        widgets::overlay(frame, area, app, &theme);
    }

    if let Some(selection) = app.selection {
        widgets::selection_overlay(frame, selection);
    }
}

/// Put the application's own surface behind everything that was drawn on it.
fn paint_background(frame: &mut Frame, area: Rect, theme: &Theme) {
    frame
        .buffer_mut()
        .set_style(area, Style::default().bg(theme.bg));
}

fn render_empty(frame: &mut Frame, area: Rect, theme: &Theme) {
    // An empty transcript is the one screen that can still teach. It is drawn as a card at the
    // transcript's border weight — not the accent weight the panels use — with square corners and
    // three real examples, so it reads as a surface waiting for content rather than as blank space
    // with a caption lost in the middle of it.
    if area.height < 6 || area.width < 30 {
        return;
    }
    let width = area.width.min(54);
    let height = 8.min(area.height);
    let rect = Rect::new(
        area.x + (area.width - width) / 2,
        area.y + (area.height - height) / 2,
        width,
        height,
    );
    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(theme.border))
        .title(Line::from(Span::styled(
            " nothing said yet ",
            Style::default()
                .fg(theme.accent_dim)
                .add_modifier(Modifier::BOLD),
        )));
    let inner = block.inner(rect);
    frame.render_widget(block, rect);

    let mut lines: Vec<Line> = Vec::new();
    lines.push(Line::from(Span::styled(
        " Type a message and press Enter.",
        theme.secondary(),
    )));
    lines.push(Line::from(""));
    lines.push(Line::from(Span::styled(
        " or start with one of these",
        theme.muted(),
    )));
    for (key, action) in [
        ("/", "browse every command"),
        ("@", "reference a workspace file"),
        ("!", "run a shell command"),
    ] {
        lines.push(Line::from(vec![
            Span::raw("   "),
            Span::styled(
                format!("{key} "),
                Style::default()
                    .fg(theme.accent)
                    .add_modifier(Modifier::BOLD),
            ),
            Span::styled(action, theme.secondary()),
        ]));
    }
    lines.truncate(inner.height as usize);
    frame.render_widget(Paragraph::new(lines), inner);
}

/// One line, capped: the title is a label, not a transcript.
fn preview(text: &str) -> String {
    let flat: String = text.split_whitespace().collect::<Vec<_>>().join(" ");
    if flat.chars().count() > 48 {
        format!("{}…", flat.chars().take(47).collect::<String>())
    } else {
        flat
    }
}

fn render_input(frame: &mut Frame, area: Rect, app: &App, theme: &Theme) {
    if area.width == 0 || area.height == 0 {
        return;
    }
    let focused = app.focus == Focus::Input;
    // The composer is a strip, not a box: a full-width band with square edges that brightens while
    // the keyboard can land in it. The fill separates it from the transcript, so no frame is drawn
    // and no column is spent on a border the operator has to read past.
    let band = if focused {
        theme.prompt_band
    } else {
        theme.surface
    };
    frame
        .buffer_mut()
        .set_style(area, Style::default().bg(band));

    let Some(session) = app.display_session() else {
        return;
    };
    let editor = &session.editor;
    let prompt_width: u16 = 2;
    let visible_rows = area.height as usize;

    // A queued or steered follow-up is the one thing the operator must not lose sight of while
    // typing, so it takes the right margin ahead of everything else.
    let tag = if let Some(text) = &session.steer_text {
        Some(format!("steer {}", preview(text)))
    } else if session.queued > 0 {
        let msg = session
            .queue_text
            .as_deref()
            .map(preview)
            .unwrap_or_else(|| format!("{} pending", session.queued));
        Some(format!("queue {msg}"))
    } else {
        None
    };

    let mut lines: Vec<Line> = Vec::new();
    let mut cursor_row: u16 = 0;

    if editor.is_empty() {
        let hint = if theme.ascii {
            "/ commands | @ files | ! shell"
        } else {
            "/ commands · @ files · ! shell"
        };
        let placeholder = "Type your message...";
        let mut spans = vec![
            Span::styled(
                theme.prompt(),
                Style::default()
                    .fg(theme.accent)
                    .add_modifier(Modifier::BOLD),
            ),
            Span::raw(" "),
            Span::styled(placeholder, Style::default().fg(theme.faint)),
        ];
        let (right, right_style) = match &tag {
            Some(tag) => (tag.as_str(), Style::default().fg(theme.accent_dim)),
            None => (hint, theme.muted()),
        };
        let used = prompt_width as usize + UnicodeWidthStr::width(placeholder);
        let pad = (area.width as usize).saturating_sub(used + UnicodeWidthStr::width(right) + 2);
        if pad >= 1 {
            spans.push(Span::raw(" ".repeat(pad)));
            spans.push(Span::styled(right.to_owned(), right_style));
            spans.push(Span::raw(" "));
        }
        lines.push(Line::from(spans));
    } else {
        let (row, _col) = editor.cursor();
        let top = row.saturating_sub(visible_rows.saturating_sub(1));
        let end = (top + visible_rows).min(editor.line_count());
        for index in top..end {
            let Some(text) = editor.line(index) else {
                continue;
            };
            let opener = lines.is_empty();
            let mut spans = if opener {
                vec![
                    Span::styled(
                        theme.prompt(),
                        Style::default()
                            .fg(theme.accent)
                            .add_modifier(Modifier::BOLD),
                    ),
                    Span::raw(" "),
                ]
            } else {
                vec![Span::raw("  ")]
            };
            spans.push(Span::styled(
                text.to_owned(),
                Style::default().fg(theme.text),
            ));
            if opener {
                let right = tag
                    .clone()
                    .unwrap_or_else(|| app.send_mode.label().to_owned());
                let used = prompt_width as usize + UnicodeWidthStr::width(text);
                let pad = (area.width as usize)
                    .saturating_sub(used + UnicodeWidthStr::width(right.as_str()) + 2);
                if pad >= 1 {
                    spans.push(Span::raw(" ".repeat(pad)));
                    spans.push(Span::styled(right, Style::default().fg(theme.faint)));
                    spans.push(Span::raw(" "));
                }
            }
            lines.push(Line::from(spans));
            if index == row {
                cursor_row = (lines.len() - 1) as u16;
            }
        }
    }

    frame.render_widget(Paragraph::new(lines), area);

    let y = area
        .y
        .saturating_add(cursor_row)
        .min(area.y + area.height.saturating_sub(1));
    if focused && !editor.is_empty() {
        let (_, col) = editor.cursor();
        let x = area
            .x
            .saturating_add(prompt_width)
            .saturating_add(col as u16)
            .min(area.x + area.width.saturating_sub(1));
        frame.set_cursor_position((x, y));
    } else if focused {
        frame.set_cursor_position((area.x + prompt_width, y));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;
    use vikram_contracts::{ApprovalId, Message, SessionId};
    use vikram_runtime::types::{ApprovalPrompt, SessionSnapshot, TurnState};

    fn snapshot(title: &str, message: &str) -> SessionSnapshot {
        SessionSnapshot {
            id: SessionId::generate(),
            title: title.to_owned(),
            cwd: "/work".into(),
            model: None,
            messages: vec![Message::user(message)],
            turn: TurnState::Idle,
            tokens: Default::default(),
            created_ms: 0,
            updated_ms: 0,
            bot: None,
            queued: 0,
            queue: Vec::new(),
            steer_text: None,
            queue_text: None,
            project_id: None,
            status: "active".to_owned(),
            title_source: "manual".to_owned(),
            title_status: "ready".to_owned(),
            turn_times: Vec::new(),
        }
    }

    #[test]
    fn an_open_approval_question_does_not_cover_the_transcript() {
        let mut app = App::new(false);
        app.sessions
            .push(crate::app::SessionView::from_snapshot(snapshot(
                "Review",
                "TRANSCRIPT-MARKER: please keep this line readable",
            )));
        app.approvals.push(ApprovalPrompt {
            approval: ApprovalId::generate(),
            session: app.sessions[0].id.clone(),
            verb: "fs.write".to_owned(),
            resource: "/etc/hosts".to_owned(),
            reason: "outside the workspace".to_owned(),
            amendment: None,
        });
        app.overlay = Some(Overlay::Approvals);

        let mut terminal = Terminal::new(TestBackend::new(100, 30)).expect("terminal");
        terminal.draw(|frame| draw(frame, &mut app)).expect("draw");
        let text: String = terminal
            .backend()
            .buffer()
            .content()
            .iter()
            .map(|cell| cell.symbol())
            .collect();

        assert!(
            text.contains("TRANSCRIPT-MARKER"),
            "the row the question is about must stay readable"
        );
        assert!(
            text.contains("fs.write"),
            "the question itself must be drawn"
        );
        assert!(
            text.contains("Permissions 1"),
            "the panel title must be drawn"
        );
    }
}
