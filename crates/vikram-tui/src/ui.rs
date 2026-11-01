//! Rendering entry point. Widgets live in [`crate::widgets`].

use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Paragraph};

use crate::app::{App, Focus};
use crate::layout;
use crate::theme::Theme;
use crate::widgets;

pub fn draw(frame: &mut Frame, app: &mut App) {
    let theme = Theme::new(app.ascii);
    let area = frame.area();

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

    match app.display_session() {
        Some(session) => widgets::conversation(frame, regions.conversation, app, session, &theme),
        None => render_empty(frame, regions.conversation, &theme),
    }

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
        widgets::overlay(frame, area, app, &theme);
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
