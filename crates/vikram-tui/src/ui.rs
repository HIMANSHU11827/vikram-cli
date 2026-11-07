//! Rendering entry point. Widgets live in [`crate::widgets`].

use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Paragraph};

use crate::app::{App, Focus, Overlay};
use crate::layout;
use crate::theme::Theme;
use crate::widgets;

pub fn draw(frame: &mut Frame, app: &mut App) {
    let theme = Theme::new(app.ascii);
    let area = frame.area();
    app.thought_hits.clear();

    if area.width < 24 || area.height < 6 {
        let message =
            Paragraph::new(Line::styled("terminal too small", theme.secondary())).centered();
        frame.render_widget(message, area);
        return;
    }

    let show_hive = !app.teams.is_empty();
    let show_bots = app.show_bots && !app.agents.is_empty();
    let input_lines = app
        .active_session()
        .map(|s| s.editor.line_count())
        .unwrap_or(1);
    let regions = layout::layout(area, show_hive, show_bots, input_lines);

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
        Some(session) => widgets::conversation(frame, conversation, app, session, &theme),
        None => {
            render_empty(frame, conversation, &theme);
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

fn render_empty(frame: &mut Frame, area: Rect, theme: &Theme) {
    let text = vec![
        Line::styled("No sessions yet", theme.title()),
        Line::from(""),
        Line::styled("Press Ctrl+N to create a session.", theme.secondary()),
    ];
    let h = 3.min(area.height);
    let block = Rect::new(area.x, area.y + (area.height - h) / 2, area.width, h);
    frame.render_widget(Paragraph::new(text).centered(), block);
}

fn render_input(frame: &mut Frame, area: Rect, app: &App, theme: &Theme) {
    let focused = app.focus == Focus::Input;
    let border = if focused { theme.accent } else { theme.border };
    let prompt = if theme.ascii {
        " input "
    } else {
        " ❯ input "
    };
    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(border))
        .title(Line::from(Span::styled(
            prompt,
            Style::default()
                .fg(theme.accent_dim)
                .add_modifier(Modifier::BOLD),
        )));
    let inner = block.inner(area);
    frame.render_widget(block, area);

    let Some(session) = app.active_session() else {
        return;
    };
    let editor = &session.editor;
    let prompt_width: u16 = 2;
    let visible_rows = inner.height as usize;

    if visible_rows == 0 {
        return;
    }

    let mut lines: Vec<Line> = Vec::new();
    let mut cursor_row: u16 = 0;

    if editor.is_empty() {
        lines.push(Line::from(vec![
            Span::styled(
                format!("{} ", theme.prompt()),
                Style::default().fg(theme.accent),
            ),
            Span::styled("Type your message...", Style::default().fg(theme.faint)),
        ]));
    } else {
        let (row, _col) = editor.cursor();
        let top = row.saturating_sub(visible_rows.saturating_sub(1));
        let end = (top + visible_rows).min(editor.line_count());
        for index in top..end {
            let Some(text) = editor.line(index) else {
                continue;
            };
            let prefix = if lines.is_empty() {
                Span::styled(
                    format!("{} ", theme.prompt()),
                    Style::default().fg(theme.accent),
                )
            } else {
                Span::raw("  ")
            };
            lines.push(Line::from(vec![
                prefix,
                Span::styled(text.to_owned(), Style::default().fg(theme.text)),
            ]));
            if index == row {
                cursor_row = (lines.len() - 1) as u16;
            }
        }
    }

    frame.render_widget(Paragraph::new(lines), inner);

    if focused && !editor.is_empty() {
        let (_, col) = editor.cursor();
        let x = inner
            .x
            .saturating_add(prompt_width)
            .saturating_add(col as u16)
            .min(inner.x + inner.width.saturating_sub(1));
        let y = inner
            .y
            .saturating_add(cursor_row)
            .min(inner.y + inner.height.saturating_sub(1));
        frame.set_cursor_position((x, y));
    } else if focused {
        frame.set_cursor_position((inner.x + prompt_width, inner.y));
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
            agent: None,
            queued: 0,
            project_id: None,
            status: "active".to_owned(),
            title_source: "manual".to_owned(),
            title_status: "ready".to_owned(),
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
