//! The chrome around the transcript: session tabs, the BOTS roster, the activity strip, the status
//! bar, the shortcut hints, and the modal overlays.
//!
//! Every panel is a pure function of [`App`]. None of them reads a clock, emits a command, or
//! invents state: the BOTS panel lists exactly what `app.bots` already holds, and a panel with
//! nothing to show says so instead of drawing a plausible placeholder.

use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, BorderType, Borders, Clear, Padding, Paragraph};

use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};
use vikram_runtime::types::{NoticeLevel, TaskState};

use crate::app::{ActivityKind, App, Focus, InspectorKind, Overlay, Selection, TabHit};
use crate::theme::{StatusSegment, Theme};

/// A tab is a reminder of what exists, not a synopsis of the session.
const TAB_TITLE_CAP: usize = 22;
const AGENT_NAME_CAP: usize = 14;
const AGENT_LINE_CAP: usize = 34;
const DIALOG_WIDTH: u16 = 62;

const HELP_ROWS: [(&str, &str); 19] = [
    ("Enter", "send the message"),
    ("Shift+Enter", "insert a newline"),
    ("Ctrl+N", "new session"),
    ("Ctrl+Tab", "next session"),
    ("Ctrl+B", "toggle the BOTS panel"),
    ("Ctrl+T", "show or fold the thinking"),
    ("Ctrl+O", "approvals"),
    ("Ctrl+R", "reverse-search input history"),
    ("r", "resume selected bot"),
    ("F2", "background tasks"),
    ("F3", "MCP servers"),
    ("F4", "plugins"),
    ("F5", "sub-bots"),
    ("F6", "context usage"),
    ("F7", "model and throughput"),
    ("F8", "skills browser"),
    ("PgUp / PgDn", "scroll the transcript"),
    ("Esc", "cancel the running turn"),
    ("F1 / ?", "this help"),
];

pub fn session_tabs(frame: &mut Frame, area: Rect, app: &mut App, theme: &Theme) {
    if area.height == 0 || area.width == 0 {
        return;
    }

    app.tab_hits.clear();
    app.new_session_x = None;

    let mut spans: Vec<Span> = Vec::new();
    let mut x: u16 = area.x;

    if app.sessions.is_empty() {
        let label = " no sessions";
        spans.push(Span::styled(label, theme.secondary()));
        x += label.width() as u16;
        let hint = "   Ctrl+N starts one   ";
        spans.push(Span::styled(hint, theme.muted()));
        x += hint.width() as u16;
        // + New control, drawn as a control: a bracket pair with the label inside it, so the one
        // thing on this row you can click looks clickable.
        let start = x;
        for (text, style) in [
            ("[ ", Style::default().fg(theme.border)),
            ("+ New", Style::default().fg(theme.accent)),
            (" ]", Style::default().fg(theme.border)),
        ] {
            spans.push(Span::styled(text, style));
            x += text.width() as u16;
        }
        app.new_session_x = Some((area.y, start, x));
        frame.render_widget(Paragraph::new(Line::from(spans)), area);
        return;
    }

    // Read once, before the loop borrows `app.sessions`: a tab that is working turns the dot into the
    // same spinner the transcript uses, so the one glyph says which of several sessions is moving
    // without the operator opening any of them.
    let spin = app.thinking_frame();

    for (index, session) in app.sessions.iter().enumerate() {
        if !spans.is_empty() {
            let sep = format!(" {} ", theme.separator());
            let w = sep.width() as u16;
            spans.push(Span::styled(sep, theme.frame()));
            x += w;
        }
        let tab_start = x;
        let active = index == app.active;
        // The tab wears the same asterisks the transcript uses for thinking, so the tab bar and the
        // conversation are one vocabulary: a filled star is a session that is here, a light one is a
        // session that is not, and the set cycles while one of them works.
        let glyph = if session.is_busy() {
            spin
        } else if active {
            if theme.ascii { "*" } else { "✳" }
        } else if theme.ascii {
            "+"
        } else {
            "✢"
        };
        let style = if active {
            Style::default()
                .fg(theme.accent)
                .add_modifier(Modifier::BOLD)
        } else if session.is_busy() {
            Style::default().fg(theme.dim)
        } else {
            theme.muted()
        };
        // The open tab carries a band, the way the row under the cursor does in a chooser: the
        // selected surface is one filled rectangle, not a brighter word. Square corners, no gap
        // between its parts, so it reads as a tab and not as coloured text.
        let band = if active { Some(theme.selection) } else { None };
        let on_band = |s: Style| match band {
            Some(bg) => s.bg(bg),
            None => s,
        };
        let title = clip(&session.title, TAB_TITLE_CAP, theme.ascii);
        let tab_text = format!(" {glyph} {title}");
        let tab_w = tab_text.width() as u16;
        spans.push(Span::styled(tab_text.clone(), on_band(style)));
        x += tab_w;
        if session.queued > 0 {
            let q = format!(" +{}", session.queued);
            let qw = q.width() as u16;
            spans.push(Span::styled(q, on_band(Style::default().fg(theme.warn))));
            x += qw;
        }
        if session.unread > 0 {
            let u = format!(" ({})", session.unread);
            let uw = u.width() as u16;
            spans.push(Span::styled(
                u,
                on_band(Style::default().fg(theme.accent_dim)),
            ));
            x += uw;
        }
        // close button
        let close_label = " × ";
        let close_w = close_label.width() as u16;
        let close_start = x;
        spans.push(Span::styled(close_label, on_band(theme.muted())));
        x += close_w;
        let tab_end = x;
        app.tab_hits.push(TabHit {
            session: session.id.clone(),
            row: area.y,
            tab_x_start: tab_start,
            tab_x_end: tab_end,
            close_x_start: close_start,
            close_x_end: close_start + close_w,
        });
    }

    // + New control
    let sep = "  ";
    let sw = sep.width() as u16;
    spans.push(Span::styled(sep, theme.frame()));
    x += sw;
    let start = x;
    for (text, style) in [
        ("[ ", Style::default().fg(theme.border)),
        ("+ New", Style::default().fg(theme.accent)),
        (" ]", Style::default().fg(theme.border)),
    ] {
        spans.push(Span::styled(text, style));
        x += text.width() as u16;
    }
    app.new_session_x = Some((area.y, start, x));

    frame.render_widget(Paragraph::new(Line::from(spans)), area);
}

pub fn hive_panel(frame: &mut Frame, area: Rect, app: &App, theme: &Theme) {
    if area.width < 6 || area.height < 3 {
        return;
    }

    let border = theme.border;
    let title = " HIVE ";
    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(border))
        .title(Line::from(Span::styled(
            title,
            Style::default()
                .fg(theme.accent_dim)
                .add_modifier(Modifier::BOLD),
        )));
    let inner = block.inner(area);
    frame.render_widget(block, area);

    if inner.height == 0 || inner.width == 0 {
        return;
    }

    let mut lines: Vec<Line> = Vec::new();
    if app.rosters.is_empty() {
        lines.push(Line::styled(" no hives", theme.muted()));
    } else {
        for roster in &app.rosters {
            let name = clip(
                &roster.name,
                (inner.width as usize).saturating_sub(12),
                theme.ascii,
            );
            let mut spans = vec![
                Span::styled(
                    if theme.ascii { ">" } else { "◈" },
                    Style::default().fg(theme.accent),
                ),
                Span::raw(" "),
                Span::styled(name, Style::default().fg(theme.text)),
            ];
            // Independent activity indicator - only when roster is working and not paused
            if roster.working && !roster.paused {
                let pulse = if theme.ascii {
                    "*"
                } else {
                    "░▒▓█▓▒░"
                };
                // Pad to right-align
                let used: usize = spans.iter().map(|s| s.content.width()).sum();
                let pulse_w = pulse.width();
                let filler = (inner.width as usize).saturating_sub(used + pulse_w);
                if filler > 0 {
                    spans.push(Span::raw(" ".repeat(filler)));
                }
                spans.push(Span::styled(pulse, Style::default().fg(theme.accent)));
            }
            lines.push(Line::from(spans));
        }
    }

    let visible = inner.height as usize;
    lines.truncate(visible);
    frame.render_widget(Paragraph::new(lines), inner);
}

pub fn bots_panel(frame: &mut Frame, area: Rect, app: &App, theme: &Theme) {
    if area.width < 6 || area.height < 3 {
        return;
    }

    let focused = app.focus == Focus::Bots;
    let border = if focused { theme.accent } else { theme.border };
    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(border))
        .title(Line::from(Span::styled(
            " BOTS ",
            Style::default()
                .fg(theme.accent_dim)
                .add_modifier(Modifier::BOLD),
        )));
    let inner = block.inner(area);
    frame.render_widget(block, area);

    if inner.height == 0 || inner.width == 0 {
        return;
    }

    let mut lines: Vec<Line> = Vec::new();
    if app.bots.is_empty() {
        lines.push(Line::styled(" no bots", theme.muted()));
        lines.push(Line::styled(" `vikram bots` lists them", theme.muted()));
    } else {
        for (index, agent) in app.bots.iter().enumerate() {
            let selected = focused && index == app.bot_selection;
            let marker = if selected { theme.chevron() } else { " " };
            let glyph = if agent.state.is_filled() {
                theme.filled_dot()
            } else {
                theme.hollow_dot()
            };
            let name_style = if selected {
                Style::default()
                    .fg(theme.accent)
                    .add_modifier(Modifier::BOLD)
            } else {
                Style::default().fg(theme.text)
            };
            let task_desc = agent
                .current_task
                .as_deref()
                .unwrap_or(agent.profile.role.as_str());
            // Line 1: ● AgentName — Current task description
            let header = vec![
                Span::styled(format!("{marker}{glyph} "), theme.agent_style(agent.state)),
                Span::styled(
                    clip(&agent.profile.name, AGENT_NAME_CAP, theme.ascii),
                    name_style,
                ),
                Span::styled(
                    format!(" — {}", clip(task_desc, AGENT_LINE_CAP, theme.ascii)),
                    theme.secondary(),
                ),
            ];
            lines.push(Line::from(header));

            // Line 2:    — Status
            let status = app.agent_state_label(agent.state);
            let spinner = if agent.state.is_active() {
                format!("{} ", app.thinking_frame())
            } else {
                String::new()
            };
            lines.push(Line::styled(
                format!("   {spinner}— {status}"),
                theme.agent_style(agent.state),
            ));
        }
    }

    let visible = inner.height as usize;
    lines.truncate(visible);
    frame.render_widget(Paragraph::new(lines), inner);
}

pub fn plasma(frame: &mut Frame, area: Rect, app: &App, theme: &Theme) {
    if area.height == 0 || area.width == 0 {
        return;
    }

    let width = area.width as usize;
    let busy = app.sessions.iter().any(|session| session.is_busy());
    let pulse = if busy {
        format!(" {} ", app.plasma_frame())
    } else {
        format!(" {} ", theme.separator())
    };
    let pulse_style = if busy {
        Style::default().fg(theme.accent)
    } else {
        theme.frame()
    };

    let running_tool = app.active_session().and_then(|session| {
        session
            .activity
            .iter()
            .rev()
            .find(|activity| activity.kind == ActivityKind::Tool && activity.ok.is_none())
    });

    let (text, style) = if let Some(tool) = running_tool {
        (tool.text.clone(), Style::default().fg(theme.tool))
    } else if let Some(notice) = app
        .notices
        .back()
        .filter(|notice| matches!(notice.level, NoticeLevel::Warn | NoticeLevel::Error))
    {
        // A warning or an error outranks the stream: "retrying in 2.0s" and "rate limited" are the
        // answer to why the clock is moving with nothing in the pane, and burying them under
        // "receiving" leaves the operator with no explanation at all.
        (notice.text.clone(), theme.notice_style(notice.level))
    } else if app.active_session().is_some_and(|s| s.streaming.is_some()) {
        (
            format!("{} receiving", app.thinking_frame()),
            Style::default().fg(theme.accent),
        )
    } else if let Some(notice) = app.notices.back() {
        (notice.text.clone(), theme.notice_style(notice.level))
    } else {
        ("ready".to_owned(), theme.muted())
    };

    let budget = width.saturating_sub(pulse.width() + 2);
    let text = clip(&text, budget, theme.ascii);
    let filler = width.saturating_sub(pulse.width() + text.width());

    let mut spans = vec![Span::styled(pulse, pulse_style), Span::styled(text, style)];
    if filler > 1 {
        spans.push(Span::styled(
            format!(" {}", theme.separator().repeat(filler - 1)),
            theme.frame(),
        ));
    }

    frame.render_widget(Paragraph::new(Line::from(spans)), area);
}

pub fn status_bar(frame: &mut Frame, area: Rect, app: &App, theme: &Theme) {
    if area.height == 0 || area.width == 0 {
        return;
    }

    let sep = if theme.ascii { " . " } else { " · " };
    let pipe = if theme.ascii { " | " } else { " │ " };

    // Left group: model/runtime metrics.
    let mut left: Vec<Span> = Vec::new();

    // ☤ Active model
    let model_glyph = if theme.ascii { "@" } else { "☤" };
    let model = app
        .metrics
        .model
        .clone()
        .or_else(|| app.active_session().and_then(|s| s.model.clone()))
        .unwrap_or_else(|| "unset".to_owned());
    // The whole identifier is shown wherever it fits: `nvidia/nemotron-3.5` is not the name of
    // anything, and the vendor prefix is the first thing an operator reads. The line as a whole is
    // still clipped to the terminal below, so a narrow window drops segments instead of wrapping.
    left.push(Span::styled(
        format!("{model_glyph} {}", clip(&model, 40, theme.ascii)),
        theme.status_style(StatusSegment::Value),
    ));

    // ctx — the boxed meter is drawn in every state, because the shape is what an operator reads at
    // a glance: how full, and how close to full. What changes behind it is the figure. A window the
    // provider reported gets a percentage; a window nobody reported gets the measured count instead,
    // since a share of an invented denominator would be a number with no meaning. A tilde says the
    // count is the estimator's, an asterisk says the window itself was assumed.
    if let Some(ratio) = app.metrics.context_ratio() {
        let bar = context_bar(ratio, 10, theme);
        let approx = if app.metrics.context_measured {
            ""
        } else {
            "~"
        };
        let assumed = if app.metrics.context_limit_assumed {
            "*"
        } else {
            ""
        };
        let figure = if app.metrics.context_limit_assumed {
            format!("{}{assumed}", human(app.metrics.context_used))
        } else {
            let pct = (f64::from(ratio) * 100.0).round() as u64;
            format!("{approx}{pct}%{assumed}")
        };
        left.push(Span::raw(sep.to_owned()));
        left.push(Span::styled(
            "ctx ",
            theme.status_style(StatusSegment::Label),
        ));
        left.push(Span::styled(
            format!("[{bar}] {figure}"),
            theme.status_style(StatusSegment::Value),
        ));
    } else if app.metrics.context_used > 0 {
        // Nothing to fill the box with, not even an assumed window, so it is not drawn: an empty
        // frame beside a real figure would read as zero usage rather than as an unknown window.
        left.push(Span::raw(sep.to_owned()));
        left.push(Span::styled(
            "ctx ",
            theme.status_style(StatusSegment::Label),
        ));
        left.push(Span::styled(
            format!("{} tok", human(app.metrics.context_used)),
            theme.status_style(StatusSegment::Value),
        ));
    }

    // ◷ the turn timer. It exists only while a turn is running, and that is the point: a second
    // clock beside it that also ran between turns would make two numbers move at once and neither
    // would say which question it answers.
    if let Some(turn) = app.metrics.turn_elapsed_ms {
        let turn_glyph = if theme.ascii { "o" } else { "◷" };
        left.push(Span::raw(sep.to_owned()));
        left.push(Span::styled(
            format!("{turn_glyph} {}", duration(turn)),
            theme.status_style(StatusSegment::Value),
        ));
    }

    // ⏱ the session clock — drawn in every state, because a segment that appears and disappears
    // makes the whole line jump and an operator cannot tell at a glance whether the machine is idle
    // or the number is missing. It measures the session, not the turn, so it never resets on send.
    left.push(Span::raw(sep.to_owned()));
    let t_glyph = if theme.ascii { "T" } else { "⏱" };
    left.push(Span::styled(
        format!("{t_glyph} {}", duration(app.metrics.elapsed_ms)),
        theme.status_style(StatusSegment::Value),
    ));

    // ↑ throughput — same rule. Before a turn has produced a rate the segment says so instead of
    // leaving a gap, so the line keeps its shape and the figure is never invented.
    left.push(Span::raw(sep.to_owned()));
    let up_glyph = if theme.ascii { "^" } else { "↑" };
    let rate = app
        .metrics
        .tokens_per_sec
        .map(|rate| format!("{up_glyph} {rate:.0} t/s"))
        .unwrap_or_else(|| format!("{up_glyph} -- t/s"));
    left.push(Span::styled(rate, theme.status_style(StatusSegment::Value)));

    // Everything above this line is the sentence about this session and it is not negotiable: a
    // clock that vanishes when the terminal narrows is worse than no clock, because the operator
    // reads its absence as stillness. What gives way is the totals column on the right, which
    // survives in the status bar's other forms and is the one thing nobody reads mid-turn.

    // Right group: system counts.
    let mut right: Vec<Span> = Vec::new();
    let bg_glyph = if theme.ascii { ">" } else { "⏵" };
    right.push(Span::styled(
        format!("{bg_glyph} Background {}", app.active_task_count()),
        theme.status_style(StatusSegment::Value),
    ));
    right.push(Span::raw(sep.to_owned()));
    let mcp_glyph = if theme.ascii { "#" } else { "⌘" };
    right.push(Span::styled(
        format!("{mcp_glyph} MCP {}", app.metrics.mcp_connected),
        theme.status_style(StatusSegment::Value),
    ));
    right.push(Span::raw(sep.to_owned()));
    let plug_glyph = if theme.ascii { "+" } else { "⧉" };
    right.push(Span::styled(
        format!("{plug_glyph} Plugins {}", app.metrics.plugins_loaded),
        theme.status_style(StatusSegment::Value),
    ));
    right.push(Span::raw(sep.to_owned()));
    let agent_glyph = if theme.ascii { "*" } else { "◇" };
    right.push(Span::styled(
        format!("{agent_glyph} Bots {}", app.bots.len()),
        theme.status_style(StatusSegment::Value),
    ));

    // The line opens with the posture and never with a message. A notice sat here for a while and
    // every segment after it shifted whenever one arrived — a status line that changes shape is one
    // the eye has to find again on each frame, and the message outranked the facts the row exists to
    // report. Notices are still recorded; they are simply not this row's business.
    let mut spans: Vec<Span> = Vec::new();
    // The posture, early in the line so a narrow terminal still shows it: an operator must be able
    // to see that something is approving without asking.
    let shield = if theme.ascii { "!" } else { "⛨" };
    spans.push(Span::styled(
        format!("{shield} {}", app.mode.as_str()),
        theme.status_style(if app.mode.auto_approves() {
            StatusSegment::Accent
        } else {
            StatusSegment::Value
        }),
    ));
    // Counts give way before the sentence does. The column stays when there is room for it, and
    // when there is not, whole segments drop from the right end rather than letting the sentence
    // that describes the session be cut in the middle.
    let fixed_w: usize = spans.iter().map(|span| span.content.width()).sum::<usize>() + sep.width();
    let left_w: usize = left.iter().map(|span| span.content.width()).sum();
    while !right.is_empty() {
        let right_w: usize = right.iter().map(|span| span.content.width()).sum();
        if fixed_w + left_w + right_w + 3 <= area.width as usize {
            break;
        }
        right.truncate(right.len().saturating_sub(2));
    }
    let right_w: usize = right.iter().map(|span| span.content.width()).sum();

    spans.push(Span::raw(sep.to_owned()));
    spans.extend(left);
    // The counts belong on the right margin: they are totals, not part of the sentence about what
    // this session is doing. Pinned to the edge they land in the same place on every frame, so the
    // eye stops having to hunt for them. When the line is too narrow to give them their own column,
    // they follow the sentence behind a pipe instead.
    let lead_w: usize = spans.iter().map(|span| span.content.width()).sum();
    let pad = (area.width as usize)
        .saturating_sub(lead_w)
        .saturating_sub(right_w);
    if pad >= 3 {
        spans.push(Span::raw(" ".repeat(pad)));
        spans.extend(right);
    } else {
        spans.push(Span::styled(pipe, theme.frame()));
        spans.extend(right);
    }

    // Clip to width (single logical line).
    let mut used = 0usize;
    let mut out: Vec<Span> = Vec::new();
    for span in spans {
        let w = span.content.width();
        if used + w > area.width as usize {
            break;
        }
        used += w;
        out.push(span);
    }
    frame.render_widget(Paragraph::new(Line::from(out)), area);
}

fn context_bar(ratio: f32, width: usize, theme: &Theme) -> String {
    let filled = ((ratio.clamp(0.0, 1.0) * width as f32).round() as usize).min(width);
    let (full, empty) = if theme.ascii {
        ('#', '-')
    } else {
        ('█', '░')
    };
    let mut out = String::with_capacity(width);
    for index in 0..width {
        out.push(if index < filled { full } else { empty });
    }
    out
}

pub fn suggestions_popup(frame: &mut Frame, anchor: Rect, app: &App, theme: &Theme) {
    let Some(popup) = app.suggestions.as_ref() else {
        return;
    };
    if popup.items.is_empty() || anchor.width < 20 || anchor.y < 5 {
        return;
    }

    // The list borrows rows from the transcript directly above the input, so it is capped, and it
    // never opens over the box it is filling in: the input keeps its own rows whatever the list
    // length. A footer is drawn only when there is a row to spare for it.
    let space = anchor.y.saturating_sub(1);
    let wanted = popup.items.len() as u16 + 2;
    let footer = wanted < 12 && wanted < space;
    let height = (if footer { wanted + 1 } else { wanted })
        .min(12)
        .min(space);
    if height < 4 {
        return;
    }

    // The list is the width of the box it fills in, so the two edges are the same edge and the eye
    // reads one surface rather than a small panel hovering over a large one.
    let width = anchor.width.saturating_sub(2);
    let rect = Rect::new(anchor.x + 1, anchor.y - height, width, height);
    frame.render_widget(Clear, rect);

    // Same ring, same body and same title-on-the-edge as every other panel, so the list an operator
    // types into reads as a surface of the interface rather than as a second, thinner vocabulary.
    let title = match popup.items.first().map(|item| item.category.as_str()) {
        Some("command") => " commands ",
        Some("shell") => " shell ",
        _ => " files ",
    };
    let block = Block::default()
        .borders(Borders::ALL)
        .border_type(BorderType::Thick)
        .border_style(Style::default().fg(theme.accent))
        .style(theme.dialog())
        .padding(Padding::new(1, 1, 0, 0))
        .title(Line::from(Span::styled(
            title,
            Style::default()
                .fg(theme.accent)
                .add_modifier(Modifier::BOLD),
        )));
    let inner = block.inner(rect);
    frame.render_widget(block, rect);

    let rows = (if footer {
        inner.height.saturating_sub(1)
    } else {
        inner.height
    }) as usize;
    if rows == 0 || inner.width < 8 {
        return;
    }

    // The cursor row stays on screen when the list is longer than the popup: the window scrolls
    // under the selection instead of running off the bottom of it.
    let offset = popup.selected.saturating_sub(rows.saturating_sub(1));
    let name_col = (inner.width.saturating_sub(14) as usize).clamp(10, 26);

    for (row, item) in popup.items.iter().skip(offset).take(rows).enumerate() {
        let selected = offset + row == popup.selected;
        let marker = if selected {
            if theme.ascii { ">" } else { "›" }
        } else {
            " "
        };
        let name = clip(&item.text, name_col, theme.ascii);
        let pad = " ".repeat(name_col.saturating_sub(name.width()));
        let desc_room = (inner.width as usize).saturating_sub(5 + name_col);
        let line = Line::from(vec![
            Span::styled(
                format!(" {marker} "),
                if selected {
                    Style::default().fg(theme.accent)
                } else {
                    theme.muted()
                },
            ),
            Span::styled(
                format!("{name}{pad}"),
                if selected {
                    Style::default()
                        .fg(theme.accent)
                        .add_modifier(Modifier::BOLD)
                } else {
                    Style::default().fg(theme.text)
                },
            ),
            Span::raw("  "),
            Span::styled(
                clip(&item.description, desc_room, theme.ascii),
                theme.muted(),
            ),
        ]);
        let style = if selected {
            theme.selected()
        } else {
            Style::default()
        };
        let area = Rect::new(inner.x, inner.y + row as u16, inner.width, 1);
        frame.render_widget(Paragraph::new(line).style(style), area);
    }

    if footer {
        let mut spans = vec![Span::raw(" ")];
        for (index, (key, action)) in [("tab", "fill"), ("↑↓", "move")].iter().enumerate() {
            if index > 0 {
                spans.push(Span::styled(
                    if theme.ascii { "  |  " } else { "  ·  " },
                    theme.frame(),
                ));
            }
            spans.push(Span::styled(*key, Style::default().fg(theme.text)));
            spans.push(Span::raw(" "));
            spans.push(Span::styled(*action, theme.muted()));
        }
        frame.render_widget(
            Paragraph::new(Line::from(spans)),
            Rect::new(
                inner.x,
                inner.y + inner.height.saturating_sub(1),
                inner.width,
                1,
            ),
        );
    }
}

pub fn shortcuts(frame: &mut Frame, area: Rect, app: &App, theme: &Theme) {
    if area.height == 0 || area.width == 0 {
        return;
    }

    // Key and action carry different weights: the key is what the hand has to find, the action is
    // what the head reads once it has found it. One flat sentence makes the row scannable at neither
    // speed. Hints drop whole, never clipped mid-word.
    let hints: &[(&str, &str)] = if app.overlay.is_some() {
        &[("Esc", "close"), ("Enter", "confirm")]
    } else {
        &[
            ("Enter", "send"),
            ("Tab", "queue"),
            ("Esc", "interrupt"),
            ("Ctrl+C", "stop"),
        ]
    };

    let mut spans: Vec<Span> = vec![Span::raw(" ")];
    let mut used = 1usize;
    for (index, (key, action)) in hints.iter().enumerate() {
        let separator = if index == 0 { 0 } else { 3 };
        let needs = separator + key.width() + 1 + action.width();
        if used + needs > area.width as usize {
            break;
        }
        if separator > 0 {
            spans.push(Span::styled(
                if theme.ascii { " | " } else { " · " },
                theme.frame(),
            ));
            used += separator;
        }
        spans.push(Span::styled(*key, Style::default().fg(theme.text)));
        spans.push(Span::raw(" "));
        spans.push(Span::styled(*action, theme.muted()));
        used += key.width() + 1 + action.width();
    }

    frame.render_widget(Paragraph::new(Line::from(spans)), area);
}

fn panel_frame(title: Line<'static>, theme: &Theme) -> Block<'static> {
    Block::default()
        .borders(Borders::ALL)
        .border_type(BorderType::Thick)
        .border_style(Style::default().fg(theme.accent))
        .style(theme.dialog())
        .padding(Padding::new(1, 1, 1, 0))
        .title(title)
}

pub fn overlay(frame: &mut Frame, area: Rect, app: &mut App, theme: &Theme) {
    // The overlay is cloned for the frame so the draw can write the click regions back into the
    // same `App` it is reading. A panel is small; a borrow that outlives the draw is not.
    let Some(current) = app.overlay.clone() else {
        app.picker_rows.clear();
        app.picker_x = None;
        app.picker_verbs.clear();
        app.overlay_clicks.clear();
        app.overlay_rect = None;
        return;
    };
    app.picker_rows.clear();
    app.picker_x = None;
    app.picker_verbs.clear();
    app.overlay_clicks.clear();
    app.overlay_rect = None;

    if let Overlay::Autocomplete {
        candidates,
        selected,
    } = current
    {
        // Render autocomplete dropdown
        let height = (candidates.len() as u16 + 2).min(area.height).min(12);
        let width = 40u16.min(area.width);
        let rect = Rect::new(
            area.x + 2,
            area.y + area.height.saturating_sub(height),
            width,
            height,
        );
        frame.render_widget(Clear, rect);
        let block = Block::default()
            .borders(Borders::ALL)
            .border_style(Style::default().fg(theme.accent));
        let inner = block.inner(rect);
        frame.render_widget(block, rect);
        let lines: Vec<Line> = candidates
            .iter()
            .enumerate()
            .map(|(i, c)| {
                let style = if i == selected {
                    Style::default()
                        .fg(theme.accent)
                        .add_modifier(Modifier::BOLD)
                } else {
                    theme.secondary()
                };
                Line::from(Span::styled(format!("  {}", c), style))
            })
            .collect();
        frame.render_widget(Paragraph::new(lines), inner);
        return;
    }

    if let Overlay::Picker(picker) = &current {
        // A chooser gets its own width: a model name and its detail do not fit in a 62-column
        // dialog, and a clipped row is a row the operator cannot tell apart from another. The width
        // comes from the rows themselves, measured in display columns, then capped by the region.
        let width = (crate::widgets::wanted_width(picker) + 4)
            .max(DIALOG_WIDTH.max(72))
            .min(area.width);
        let body = crate::widgets::picker_body(picker, theme, width.saturating_sub(4));
        let height = (body.lines.len() as u16 + 3).min(area.height);
        let rect = docked(area, width, height);
        frame.render_widget(Clear, rect);
        let block = panel_frame(
            Line::from(Span::styled(
                format!(" {} ", crate::widgets::picker_title(picker)),
                Style::default()
                    .fg(theme.accent)
                    .add_modifier(Modifier::BOLD),
            )),
            theme,
        );
        let inner = block.inner(rect);
        frame.render_widget(block, rect);
        frame.render_widget(Paragraph::new(body.lines), inner);

        // The rows, so a click can name one. `first_row` is the only thing that knows where the
        // header ended, so the hit test asks it instead of counting lines again.
        for position in 0..body.rows {
            let y = inner.y + body.first_row as u16 + position as u16;
            app.picker_rows.push((y, position));
        }
        app.picker_x = Some((rect.x, rect.x + rect.width));
        // The footer buttons, so a press can name one. Coordinates are absolute, because that is
        // what the mouse reports; the body only knows its columns inside the frame.
        for chip in &body.verbs {
            let y = inner.y + chip.line as u16;
            if y < rect.y + rect.height && y >= rect.y {
                app.picker_verbs.push((
                    y,
                    inner.x + chip.start,
                    inner.x + chip.end,
                    chip.verb,
                ));
            }
        }
        app.overlay_rect = Some((rect.x, rect.y, rect.width, rect.height));
        return;
    }

    let (title, lines, clicks) = overlay_content(&*app, &current, theme);
    // A panel is as wide as the widest line it has to show, up to the region. Sixty-two columns is
    // the floor, not the ceiling: an inspector or a doctor report that wraps at the floor hides the
    // one column that explains the row.
    let widest = lines
        .iter()
        .map(|line| {
            line.spans
                .iter()
                .map(|span| UnicodeWidthStr::width(span.content.as_ref()))
                .sum::<usize>()
        })
        .max()
        .unwrap_or(0);
    let width = ((widest as u16).saturating_add(8))
        .max(DIALOG_WIDTH)
        .min(area.width);
    let height = (lines.len() as u16 + 3).min(area.height);
    let rect = docked(area, width, height);
    frame.render_widget(Clear, rect);
    let block = panel_frame(
        Line::from(Span::styled(
            format!(" {title} "),
            Style::default()
                .fg(theme.accent)
                .add_modifier(Modifier::BOLD),
        )),
        theme,
    );
    let inner = block.inner(rect);
    frame.render_widget(block, rect);
    frame.render_widget(Paragraph::new(lines), inner);

    // The buttons and rows this panel drew, in screen coordinates.
    for hit in clicks {
        let y = inner.y + hit.line as u16;
        if y < rect.y + rect.height && y >= rect.y {
            let (start, end) = hit.columns.map_or((inner.x, inner.x + inner.width), |(s, e)| {
                (inner.x + s, inner.x + e)
            });
            app.overlay_clicks.push((y, start, end, hit.click));
        }
    }
    app.overlay_rect = Some((rect.x, rect.y, rect.width, rect.height));
}

fn overlay_content(
    app: &App,
    overlay: &Overlay,
    theme: &Theme,
) -> (String, Vec<Line<'static>>, Vec<OverlayHit>) {
    match overlay {
        Overlay::Help => {
            let mut lines: Vec<Line<'static>> = HELP_ROWS
                .iter()
                .map(|(key, description)| {
                    Line::from(vec![
                        Span::styled(format!("  {key:<14}"), Style::default().fg(theme.accent)),
                        Span::styled((*description).to_owned(), theme.secondary()),
                    ])
                })
                .collect();
            let clicks = push_chips(&mut lines, theme, &[("esc close", OverlayClick::PromptCancel)]);
            ("Keys".to_owned(), lines, clicks)
        }
        Overlay::Approvals => {
            let (title, lines, action_rows) = approval_lines(app, theme);
            let clicks = action_rows
                .into_iter()
                .enumerate()
                .map(|(index, line)| OverlayHit {
                    line,
                    columns: None,
                    click: OverlayClick::ApprovalAction(index),
                })
                .collect();
            (title, lines, clicks)
        }
        Overlay::Inspector(kind) => {
            let mut lines = inspector_lines(app, kind, theme);
            let clicks = push_chips(&mut lines, theme, &[("esc close", OverlayClick::PromptCancel)]);
            (inspector_title(kind).to_owned(), lines, clicks)
        }
        Overlay::AddBot { selected } => {
            let (title, mut lines) = add_agent_lines(app, *selected, theme);
            let mut clicks: Vec<OverlayHit> = (0..app.bots.len())
                .map(|index| OverlayHit {
                    line: index * 2,
                    columns: None,
                    click: OverlayClick::AddBotRow(index),
                })
                .collect();
            clicks.extend(push_chips(
                &mut lines,
                theme,
                &[
                    ("enter spawn", OverlayClick::AddBotSpawn),
                    ("esc close", OverlayClick::PromptCancel),
                ],
            ));
            (title, lines, clicks)
        }
        Overlay::RenameSession { editor } => {
            let (title, mut lines) = rename_lines(app, editor, theme);
            let clicks = push_chips(
                &mut lines,
                theme,
                &[
                    ("enter save", OverlayClick::PromptSubmit),
                    ("esc cancel", OverlayClick::PromptCancel),
                ],
            );
            (title, lines, clicks)
        }
        Overlay::HistorySearch { query, selected }
        | Overlay::ReverseSearch { query, selected }
        | Overlay::ReverseHistorySearch { query, selected } => {
            let matches = crate::input::history_matches(app, query);
            let selected = *selected;
            let mut lines: Vec<Line<'static>> = Vec::new();
            lines.push(Line::from(vec![
                Span::styled("  query ", theme.muted()),
                Span::styled(
                    query.clone(),
                    Style::default()
                        .fg(theme.accent)
                        .add_modifier(Modifier::BOLD),
                ),
                Span::styled("▌", theme.muted()),
            ]));
            if matches.is_empty() {
                lines.push(Line::from(Span::styled("  no matches", theme.secondary())));
            } else {
                for (i, entry) in matches.iter().enumerate() {
                    let style = if i == selected {
                        Style::default()
                            .fg(theme.accent)
                            .add_modifier(Modifier::BOLD)
                    } else {
                        theme.secondary()
                    };
                    lines.push(Line::from(Span::styled(format!("  {}", entry), style)));
                }
            }
            let clicks = push_chips(&mut lines, theme, &[("esc close", OverlayClick::PromptCancel)]);
            ("Reverse search".to_owned(), lines, clicks)
        }
        Overlay::Autocomplete { .. } => ("Autocomplete".to_owned(), vec![], Vec::new()),
        Overlay::Skills => {
            let (title, mut lines) = skills_lines(app, theme);
            let clicks = push_chips(&mut lines, theme, &[("esc close", OverlayClick::PromptCancel)]);
            (title, lines, clicks)
        }
        Overlay::Info { title, lines } => {
            let mut rendered = info_lines(lines, theme);
            let clicks = push_chips(
                &mut rendered,
                theme,
                &[("esc close", OverlayClick::PromptCancel)],
            );
            (title.clone(), rendered, clicks)
        }
        Overlay::Prompt {
            title,
            hint,
            editor,
            ..
        } => {
            let mut lines = vec![Line::from(Span::styled(format!("  {hint}"), theme.muted()))];
            lines.push(Line::from(vec![
                Span::styled("  › ", theme.accent),
                Span::styled(
                    editor.text(),
                    Style::default()
                        .fg(theme.accent)
                        .add_modifier(Modifier::BOLD),
                ),
                Span::styled("▌", theme.muted()),
            ]));
            let clicks = push_chips(
                &mut lines,
                theme,
                &[
                    ("enter save", OverlayClick::PromptSubmit),
                    ("esc cancel", OverlayClick::PromptCancel),
                ],
            );
            (title.clone(), lines, clicks)
        }
        Overlay::Confirm { title, lines, .. } => {
            let mut rendered = info_lines(lines, theme);
            let clicks = push_chips(
                &mut rendered,
                theme,
                &[
                    ("enter confirm", OverlayClick::ConfirmYes),
                    ("esc keep as is", OverlayClick::ConfirmNo),
                ],
            );
            (title.clone(), rendered, clicks)
        }
        // Drawn by `overlay` before this point; the arm exists so the match stays total.
        Overlay::Picker(_) => ("Choose".to_owned(), Vec::new(), Vec::new()),
    }
}

/// Append a blank separator and a row of buttons, rewriting the line indices into the body.
fn push_chips(
    lines: &mut Vec<Line<'static>>,
    theme: &Theme,
    chips: &[(&str, OverlayClick)],
) -> Vec<OverlayHit> {
    lines.push(Line::from(""));
    let (row, hits) = chip_row(theme, chips);
    let line = lines.len();
    lines.push(row);
    hits.into_iter()
        .map(|(start, end, click)| OverlayHit {
            line,
            columns: Some((start, end)),
            click,
        })
        .collect()
}

fn info_lines(lines: &[String], theme: &Theme) -> Vec<Line<'static>> {
    lines
        .iter()
        .map(|line| {
            if let Some(rest) = line.strip_prefix("! ") {
                Line::from(Span::styled(format!("  {rest}"), theme.muted()))
            } else {
                Line::from(Span::styled(format!("  {line}"), theme.secondary()))
            }
        })
        .collect()
}

/// The three answers the approval panel offers, in the order it draws them.
pub const APPROVAL_ACTIONS: [&str; 4] = [
    "Approve",
    "Deny",
    "Add to the allow list",
    "Approve and add to the allow list",
];

/// A press target inside a non-picker overlay. Every panel that can be answered is answerable with
/// the mouse too, so no dialog is keyboard-only.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum OverlayClick {
    /// One row of the spawn-a-bot list, by its index in `App::bots`.
    AddBotRow(usize),
    /// Spawn the bot the spawn list has selected.
    AddBotSpawn,
    /// One of [`APPROVAL_ACTIONS`], by index.
    ApprovalAction(usize),
    /// Commit whatever the open text field holds.
    PromptSubmit,
    /// Leave the open text field (or the spawn list) as it was.
    PromptCancel,
    /// Answer a confirmation with yes.
    ConfirmYes,
    /// Answer a confirmation with no.
    ConfirmNo,
}

/// One press target in a panel body, before the draw turns its line into a screen row.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct OverlayHit {
    /// Index into the body's lines.
    pub line: usize,
    /// Columns inside the body when the target is one button among several on a shared line.
    /// `None` means the whole row is the target.
    pub columns: Option<(u16, u16)>,
    pub click: OverlayClick,
}

/// A row of bracketed buttons, with the columns each one occupies. The same shape as the picker's
/// footer, so a button looks and behaves the same wherever it appears.
fn chip_row(
    theme: &Theme,
    chips: &[(&str, OverlayClick)],
) -> (Line<'static>, Vec<(u16, u16, OverlayClick)>) {
    let mut spans: Vec<Span<'static>> = vec![Span::styled("  ".to_owned(), theme.muted())];
    let mut hits: Vec<(u16, u16, OverlayClick)> = Vec::new();
    let mut col: u16 = 2;
    for (label, click) in chips {
        let text = format!("[{label}]");
        let width = UnicodeWidthStr::width(text.as_str()) as u16;
        hits.push((col, col + width, *click));
        spans.push(Span::styled(
            text,
            Style::default()
                .fg(theme.accent)
                .add_modifier(Modifier::BOLD),
        ));
        spans.push(Span::styled(" ".to_owned(), theme.muted()));
        col += width + 1;
    }
    (Line::from(spans), hits)
}

/// How many rows the approvals panel needs when it is docked under the transcript.
///
/// The caller reserves exactly this many rows so the question never covers the rows it is asking
/// about: the transcript is laid out above the reserved strip and stays readable behind it.
pub fn approval_panel_height(app: &App, theme: &Theme) -> u16 {
    let (_, lines, _) = approval_lines(app, theme);
    (lines.len() as u16 + 2).clamp(3, 16)
}

/// The panel body plus the line each answer button sits on, so a press can name one.
fn approval_lines(app: &App, theme: &Theme) -> (String, Vec<Line<'static>>, Vec<usize>) {
    let mut lines: Vec<Line> = Vec::new();
    let mut action_rows: Vec<usize> = Vec::new();
    if app.approvals.is_empty() {
        lines.push(Line::styled("  nothing is waiting", theme.secondary()));
    } else {
        for (index, approval) in app.approvals.iter().enumerate() {
            let selected = index == app.approval_selection;
            let marker = if selected { theme.chevron() } else { " " };
            let style = if selected {
                Style::default()
                    .fg(theme.accent)
                    .add_modifier(Modifier::BOLD)
            } else {
                Style::default().fg(theme.text)
            };
            lines.push(Line::from(vec![
                Span::styled(format!("{marker} {} ", theme.filled_dot()), style),
                Span::styled(format!("{} {}", approval.verb, approval.resource), style),
            ]));
            lines.push(Line::styled(
                format!("   {}", clip(&approval.reason, 56, theme.ascii)),
                theme.muted(),
            ));
            if let Some(amendment) = &approval.amendment {
                lines.push(Line::styled(
                    format!("   proposed: {}", clip(amendment, 48, theme.ascii)),
                    Style::default().fg(theme.ok),
                ));
            }
        }
    }
    lines.push(Line::from(""));
    // One button per answer, laid out so the arrow keys and the mouse can land on any of them.
    for (index, label) in APPROVAL_ACTIONS.iter().enumerate() {
        let selected = index == app.approval_action;
        let marker = if selected { theme.chevron() } else { " " };
        let style = if selected {
            Style::default()
                .fg(theme.accent)
                .add_modifier(Modifier::BOLD)
        } else {
            Style::default().fg(theme.text)
        };
        action_rows.push(lines.len());
        lines.push(Line::from(vec![
            Span::styled(format!("{marker} "), style),
            Span::styled(format!("[{label}]"), style),
        ]));
    }
    lines.push(Line::styled(
        "  ↑↓ choose · enter confirm · a approve · d deny · p add",
        theme.muted(),
    ));
    (
        format!("Permissions {}", app.approvals.len()),
        lines,
        action_rows,
    )
}

fn inspector_title(kind: &InspectorKind) -> &'static str {
    match kind {
        InspectorKind::Background => "Background tasks",
        InspectorKind::Mcp => "MCP servers",
        InspectorKind::Plugins => "Plugins",
        InspectorKind::SubAgents => "Sub-bots",
        InspectorKind::Context => "Context",
        InspectorKind::Model => "Model",
    }
}

fn inspector_lines(app: &App, kind: &InspectorKind, theme: &Theme) -> Vec<Line<'static>> {
    let mut lines: Vec<Line> = Vec::new();
    match kind {
        InspectorKind::Background => {
            if app.tasks.is_empty() {
                lines.push(Line::styled("  nothing is running", theme.secondary()));
            } else {
                for task in &app.tasks {
                    lines.push(Line::from(vec![
                        Span::styled(format!("  {} ", theme.bullet()), theme.muted()),
                        Span::styled(clip(&task.name, 32, theme.ascii), theme.secondary()),
                        Span::styled(
                            format!("  {}", task_state_label(task.state)),
                            task_state_style(task.state, theme),
                        ),
                    ]));
                }
            }
        }
        InspectorKind::Mcp => {
            lines.push(kv(
                "connected",
                app.metrics.mcp_connected.to_string(),
                theme,
            ));
            lines.push(kv(
                "configured",
                app.metrics.mcp_configured.to_string(),
                theme,
            ));
            lines.push(Line::from(""));
            lines.push(Line::styled(
                "  edit `mcp.json` in the runtime home to add servers",
                theme.muted(),
            ));
        }
        InspectorKind::Plugins => {
            lines.push(kv("loaded", app.metrics.plugins_loaded.to_string(), theme));
            lines.push(kv(
                "installed",
                app.metrics.plugins_installed.to_string(),
                theme,
            ));
        }
        InspectorKind::SubAgents => {
            let spawned: Vec<_> = app
                .bots
                .iter()
                .filter(|agent| agent.session.is_some() || agent.state.is_active())
                .collect();
            if spawned.is_empty() {
                lines.push(Line::styled("  none spawned", theme.secondary()));
                lines.push(Line::from(""));
                lines.push(Line::styled(
                    "  bots work in a session; a sub-agent is only created when you ask for one",
                    theme.muted(),
                ));
            } else {
                for agent in spawned {
                    lines.push(Line::from(vec![
                        Span::styled(format!("  {} ", theme.bullet()), theme.muted()),
                        Span::styled(
                            clip(&agent.profile.name, AGENT_NAME_CAP, theme.ascii),
                            Style::default().fg(theme.text),
                        ),
                        Span::styled(
                            format!("  {}", app.agent_state_label(agent.state).to_lowercase()),
                            theme.agent_style(agent.state),
                        ),
                    ]));
                    if let Some(task) = &agent.current_task {
                        lines.push(Line::styled(
                            format!("     {}", clip(task, 48, theme.ascii)),
                            theme.muted(),
                        ));
                    }
                }
            }
        }
        InspectorKind::Context => {
            // The meter is the request that was actually sent, so it beats the summary figures:
            // one says what the model was given, the other says what the runtime last totals.
            let detail = app
                .context_detail
                .as_ref()
                .filter(|(id, _)| app.active_session().is_some_and(|s| s.id == *id))
                .cloned()
                .map(|(_, meter)| meter);
            let (window, usable, used, measured, assumed) = match &detail {
                Some(meter) => (
                    meter.window,
                    meter.usable,
                    meter.used,
                    meter.measured,
                    meter.limits_assumed,
                ),
                None => (
                    app.metrics.context_window.unwrap_or(0),
                    app.metrics.context_usable,
                    app.metrics.context_used,
                    app.metrics.context_measured,
                    app.metrics.context_limit_assumed,
                ),
            };
            if window == 0 {
                lines.push(Line::styled(
                    "  no live session has a context window yet",
                    theme.secondary(),
                ));
            } else {
                lines.push(kv("used", human(used), theme));
                lines.push(kv("window", human(window), theme));
                lines.push(kv(
                    "usable",
                    format!("{} (after reply reserve, schemas, margin)", human(usable)),
                    theme,
                ));
                lines.push(kv("left", human(usable.saturating_sub(used)), theme));
                lines.push(kv(
                    "counting",
                    if measured {
                        "measured, newest tail estimated".to_owned()
                    } else {
                        "estimated and self-calibrated".to_owned()
                    },
                    theme,
                ));
                lines.push(kv(
                    "limit from",
                    if assumed {
                        "assumed floor: nothing advertised one"
                    } else {
                        "config, provider, or model registry"
                    }
                    .to_owned(),
                    theme,
                ));
                lines.push(kv(
                    "folds",
                    format!(
                        "{} (older turns summarised; originals stay in the journal)",
                        app.metrics.context_compactions
                    ),
                    theme,
                ));
                if let Some(meter) = &detail {
                    let fill = used_ratio(used, usable);
                    lines.push(Line::from(""));
                    lines.push(Line::styled(
                        format!("  {}", ratio_bar(fill, 40, theme)),
                        Style::default().fg(ratio_colour(fill, theme)),
                    ));
                    lines.push(Line::styled("  in the last request", theme.muted()));
                    for part in &meter.parts {
                        lines.push(kv(part.label.as_str(), human(part.tokens), theme));
                    }
                } else {
                    lines.push(Line::from(""));
                    lines.push(Line::styled(
                        format!("  {}", ratio_bar(used_ratio(used, window), 40, theme)),
                        Style::default().fg(ratio_colour(used_ratio(used, window), theme)),
                    ));
                    lines.push(Line::styled(
                        "  ! no request has gone out yet: these are totals, not one prompt"
                            .to_owned(),
                        theme.muted(),
                    ));
                }
            }
        }
        InspectorKind::Model => {
            lines.push(kv(
                "model",
                app.metrics
                    .model
                    .clone()
                    .unwrap_or_else(|| "unset".to_owned()),
                theme,
            ));
            lines.push(kv(
                "provider",
                app.metrics
                    .provider
                    .clone()
                    .unwrap_or_else(|| "unset".to_owned()),
                theme,
            ));
            let throughput = app
                .metrics
                .tokens_per_sec
                .map(|rate| format!("{rate:.1} tok/s"))
                .unwrap_or_else(|| "not measured".to_owned());
            lines.push(kv("throughput", throughput, theme));
            let turn = app
                .metrics
                .turn_elapsed_ms
                .map(|ms| format!("{:.1}s", ms as f64 / 1000.0))
                .unwrap_or_else(|| "not measured".to_owned());
            lines.push(kv("turn", turn, theme));
            lines.push(kv("uptime", human(app.metrics.elapsed_ms / 1000), theme));
        }
    }
    lines
}

fn add_agent_lines(app: &App, selected: usize, theme: &Theme) -> (String, Vec<Line<'static>>) {
    let mut lines: Vec<Line> = Vec::new();
    if app.bots.is_empty() {
        lines.push(Line::styled("  no bot profiles", theme.secondary()));
    } else {
        for (index, agent) in app.bots.iter().enumerate() {
            let active = index == selected;
            let marker = if active { theme.chevron() } else { " " };
            let style = if active {
                Style::default()
                    .fg(theme.accent)
                    .add_modifier(Modifier::BOLD)
            } else {
                Style::default().fg(theme.text)
            };
            lines.push(Line::from(vec![
                Span::styled(format!("{marker} "), style),
                Span::styled(
                    clip(&agent.profile.name, AGENT_NAME_CAP, theme.ascii),
                    style,
                ),
                Span::styled(
                    format!("  {}", agent.profile.role),
                    theme.status_style(StatusSegment::Label),
                ),
            ]));
            lines.push(Line::styled(
                format!("   {}", clip(&agent.profile.description, 54, theme.ascii)),
                theme.muted(),
            ));
        }
    }
    lines.push(Line::from(""));
    ("Spawn a bot".to_owned(), lines)
}

fn rename_lines(
    app: &App,
    editor: &crate::editor::Editor,
    theme: &Theme,
) -> (String, Vec<Line<'static>>) {
    let mut lines: Vec<Line> = Vec::new();
    if let Some(session) = app.active_session() {
        lines.push(Line::styled(
            format!("  now: {}", clip(&session.title, 50, theme.ascii)),
            theme.muted(),
        ));
    }
    let text = editor.text();
    if text.is_empty() {
        lines.push(Line::from(vec![
            Span::styled(
                format!("  {} ", theme.prompt()),
                Style::default().fg(theme.accent),
            ),
            Span::styled("new title", theme.muted()),
        ]));
    } else {
        for index in 0..editor.line_count() {
            let Some(line) = editor.line(index) else {
                continue;
            };
            let prefix = if index == 0 {
                Span::styled(
                    format!("  {} ", theme.prompt()),
                    Style::default().fg(theme.accent),
                )
            } else {
                Span::raw("    ")
            };
            lines.push(Line::from(vec![
                prefix,
                Span::styled(line.to_owned(), Style::default().fg(theme.text)),
            ]));
        }
    }
    lines.push(Line::from(""));
    ("Rename session".to_owned(), lines)
}

fn kv(key: &str, value: String, theme: &Theme) -> Line<'static> {
    Line::from(vec![
        Span::styled(format!("  {key:<12}"), theme.muted()),
        Span::styled(value, Style::default().fg(theme.text)),
    ])
}

fn task_state_label(state: TaskState) -> &'static str {
    match state {
        TaskState::Running => "running",
        TaskState::Completed => "done",
        TaskState::Failed => "failed",
        TaskState::Cancelled => "cancelled",
    }
}

fn task_state_style(state: TaskState, theme: &Theme) -> Style {
    match state {
        TaskState::Running => Style::default().fg(theme.accent),
        TaskState::Completed => Style::default().fg(theme.ok),
        TaskState::Failed => Style::default().fg(theme.error),
        TaskState::Cancelled => Style::default().fg(theme.warn),
    }
}

fn ratio_colour(ratio: f32, theme: &Theme) -> ratatui::style::Color {
    if ratio >= 0.9 {
        theme.error
    } else if ratio >= 0.7 {
        theme.warn
    } else {
        theme.accent
    }
}

/// How full a budget is, as a fraction. A zero denominator reads as empty, not as infinite.
fn used_ratio(used: u64, of: u64) -> f32 {
    if of == 0 {
        0.0
    } else {
        (used as f32 / of as f32).clamp(0.0, 1.0)
    }
}

fn ratio_bar(ratio: f32, width: usize, theme: &Theme) -> String {
    let filled = ((ratio.clamp(0.0, 1.0) * width as f32).round() as usize).min(width);
    let (full, empty) = if theme.ascii {
        ('#', '-')
    } else {
        ('█', '░')
    };
    let mut out = String::with_capacity(width);
    for index in 0..width {
        out.push(if index < filled { full } else { empty });
    }
    out
}

fn human(value: u64) -> String {
    if value < 1_000 {
        return value.to_string();
    }
    if value < 1_000_000 {
        return format!("{:.1}k", value as f64 / 1_000.0);
    }
    format!("{:.1}M", value as f64 / 1_000_000.0)
}

/// A span of time at the resolution a status line can use: seconds under a minute, then minutes
/// and seconds, then hours and minutes. The unit is always named, because `5` beside a box tells
/// nobody whether the machine has been up for five seconds or five hours.
fn duration(ms: u64) -> String {
    let total = ms / 1000;
    let (hours, minutes, seconds) = (total / 3600, (total % 3600) / 60, total % 60);
    if hours > 0 {
        format!("{hours}h{minutes:02}m")
    } else if minutes > 0 {
        format!("{minutes}m{seconds:02}s")
    } else {
        format!("{seconds}s")
    }
}

fn clip(text: &str, width: usize, ascii: bool) -> String {
    if width == 0 {
        return String::new();
    }
    if text.width() <= width {
        return text.to_owned();
    }
    let marker = if ascii { '>' } else { '…' };
    let budget = width.saturating_sub(1);
    let mut out = String::new();
    let mut used = 0usize;
    for ch in text.chars() {
        let advance = ch.width().unwrap_or(0);
        if used + advance > budget {
            break;
        }
        out.push(ch);
        used += advance;
    }
    out.push(marker);
    out
}

fn docked(area: Rect, width: u16, height: u16) -> Rect {
    let width = width.min(area.width);
    let height = height.min(area.height);
    // Hug the bottom-left corner of the region: the transcript stays readable above and to the
    // right, which is where the answer keeps appearing while a panel waits for an answer.
    Rect::new(
        area.x,
        area.y + area.height.saturating_sub(height),
        width,
        height,
    )
}

#[cfg(test)]
mod dock_tests {
    use super::*;

    #[test]
    fn a_panel_sits_on_the_last_rows_of_its_region() {
        let region = Rect::new(0, 1, 100, 20);
        let rect = docked(region, 72, 6);

        assert_eq!(rect.y + rect.height, region.y + region.height);
        assert_eq!(rect.y, 15, "a docked panel hangs from the bottom edge");
        assert_eq!(rect.x, region.x, "and sits against the left edge");
        assert_eq!(rect.width, 72);
    }

    #[test]
    fn a_panel_larger_than_its_region_is_clipped_to_it() {
        let region = Rect::new(0, 0, 40, 5);
        let rect = docked(region, 72, 30);

        assert_eq!(rect.height, 5);
        assert_eq!(rect.width, 40);
        assert_eq!(rect.y, 0);
    }
}

/// Paint the drag that is selecting text. Reversing the cells is the terminal's own way of marking
/// a selection, so no colour is invented and the text stays readable under any theme.
pub fn selection_overlay(frame: &mut Frame, selection: Selection) {
    if selection.is_click() {
        return;
    }
    let buffer = frame.buffer_mut();
    let area = buffer.area;
    let ((_, sy), (_, ey)) = selection.ordered();
    let last_row = area.y + area.height.saturating_sub(1);
    let last_col = area.x + area.width.saturating_sub(1);
    for row in sy.max(area.y)..=ey.min(last_row) {
        let Some((start, end)) = selection.span(row) else {
            continue;
        };
        let start = start.max(area.x);
        let end = end.min(last_col);
        if start > end {
            continue;
        }
        for x in start..=end {
            let cell = &mut buffer[(x, row)];
            cell.set_style(cell.style().add_modifier(Modifier::REVERSED));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::app::Activity;
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;
    use vikram_contracts::{ApprovalId, BotId, Message, SessionId};
    use vikram_runtime::types::{
        ApprovalPrompt, BackgroundTask, BotProfile, BotSnapshot, BotState, ContextMeter,
        ContextPart, Metrics, NoticeLevel, RuntimeStatus, SessionSnapshot, TurnState,
    };

    fn draw(width: u16, height: u16, render: impl FnOnce(&mut Frame)) -> String {
        let mut terminal = Terminal::new(TestBackend::new(width, height)).expect("terminal");
        terminal.draw(|frame| render(frame)).expect("draw");
        let buffer = terminal.backend().buffer().clone();
        buffer.content().iter().map(|cell| cell.symbol()).collect()
    }

    fn session(title: &str) -> SessionSnapshot {
        SessionSnapshot {
            id: SessionId::generate(),
            title: title.to_owned(),
            cwd: "/work".into(),
            model: None,
            messages: vec![Message::user("hi")],
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
            title_source: "generic".to_owned(),
            title_status: "pending".to_owned(),
        }
    }

    fn bot(name: &str, role: &str, state: BotState) -> BotSnapshot {
        BotSnapshot {
            profile: BotProfile {
                id: BotId::generate(),
                name: name.to_owned(),
                role: role.to_owned(),
                description: format!("{role} bot"),
                icon: None,
                created_ms: 0,
            },
            state,
            current_task: None,
            progress: None,
            session: None,
            unread: 0,
        }
    }

    fn app_with_default_bots() -> App {
        let mut app = App::new(false);
        app.sessions
            .push(crate::app::SessionView::from_snapshot(session("First")));
        app.bots = vec![
            bot("Rudra", "Implementer", BotState::Idle),
            bot("Karma", "Reviewer", BotState::Idle),
            bot("Verifier", "Verifier", BotState::Idle),
        ];
        app
    }

    #[test]
    fn tabs_name_every_session_and_mark_the_active_one() {
        let mut app = app_with_default_bots();
        app.sessions
            .push(crate::app::SessionView::from_snapshot(session("Second")));

        let rendered = draw(60, 1, |frame| {
            session_tabs(frame, frame.area(), &mut app, &Theme::new(false))
        });
        assert!(rendered.contains("First"), "first tab missing: {rendered}");
        assert!(
            rendered.contains("Second"),
            "second tab missing: {rendered}"
        );
    }

    #[test]
    fn a_working_tab_spins_where_an_idle_tab_shows_the_star() {
        let mut app = app_with_default_bots();
        app.sessions[0].turn = vikram_runtime::types::TurnState::Streaming;
        let busy = draw(40, 1, |frame| {
            session_tabs(frame, frame.area(), &mut app, &Theme::new(false))
        });
        assert!(
            crate::app::THINKING_FRAMES
                .iter()
                .any(|frame| busy.contains(&format!(" {frame} "))),
            "a working tab shows no spinner: {busy}"
        );
        app.tick = app.tick.wrapping_add(2);
        let busy_next = draw(40, 1, |frame| {
            session_tabs(frame, frame.area(), &mut app, &Theme::new(false))
        });
        assert_ne!(
            busy, busy_next,
            "a working tab stopped animating between ticks"
        );

        app.sessions[0].turn = vikram_runtime::types::TurnState::Idle;
        let idle = draw(40, 1, |frame| {
            session_tabs(frame, frame.area(), &mut app, &Theme::new(false))
        });
        assert!(idle.contains(" ✳ "), "an idle tab lost its star: {idle}");
        app.tick = app.tick.wrapping_add(2);
        let idle_next = draw(40, 1, |frame| {
            session_tabs(frame, frame.area(), &mut app, &Theme::new(false))
        });
        assert_eq!(idle, idle_next, "an idle tab kept animating");
    }

    #[test]
    fn tabs_are_honest_when_no_session_exists() {
        let mut app = App::new(false);
        let rendered = draw(60, 1, |frame| {
            session_tabs(frame, frame.area(), &mut app, &Theme::new(false))
        });
        assert!(rendered.contains("no sessions"), "got {rendered}");
        assert!(rendered.contains("Ctrl+N"), "got {rendered}");
    }

    #[test]
    fn bots_panel_lists_the_profile_names_and_state() {
        let mut app = app_with_default_bots();
        app.bots[0].state = BotState::Running;
        app.bots[0].current_task = Some("porting the store".to_owned());

        let rendered = draw(32, 10, |frame| {
            bots_panel(frame, frame.area(), &app, &Theme::new(false))
        });
        for name in ["Rudra", "Karma", "Verifier"] {
            assert!(rendered.contains(name), "{name} missing: {rendered}");
        }
        assert!(rendered.contains("Working"), "state missing: {rendered}");
        assert!(
            rendered.contains("porting the store"),
            "task missing: {rendered}"
        );
    }

    #[test]
    fn bots_panel_says_so_when_there_are_no_profiles() {
        let app = App::new(false);
        let rendered = draw(32, 10, |frame| {
            bots_panel(frame, frame.area(), &app, &Theme::new(false))
        });
        assert!(rendered.contains("no bots"), "got {rendered}");
    }

    #[test]
    fn bots_panel_never_exceeds_its_area() {
        let mut app = app_with_default_bots();
        app.bots
            .extend((0..12).map(|index| bot(&format!("bot {index}"), "Reviewer", BotState::Idle)));
        let rendered = draw(30, 5, |frame| {
            bots_panel(frame, frame.area(), &app, &Theme::new(false))
        });
        assert_eq!(rendered.chars().count(), 30 * 5);
    }

    #[test]
    fn plasma_reports_the_running_tool_then_falls_back() {
        let mut app = app_with_default_bots();
        app.sessions[0].activity.push(Activity {
            kind: ActivityKind::Tool,
            text: "shell · cargo test".to_owned(),
            call: None,
            ok: None,
            expanded: false,
        });
        let theme = Theme::new(false);
        let rendered = draw(50, 1, |frame| plasma(frame, frame.area(), &app, &theme));
        assert!(rendered.contains("cargo test"), "got {rendered}");

        app.sessions[0].activity[0].ok = Some(true);
        let rendered = draw(50, 1, |frame| plasma(frame, frame.area(), &app, &theme));
        assert!(rendered.contains("ready"), "got {rendered}");
    }

    #[test]
    fn plasma_surfaces_the_latest_notice() {
        let mut app = app_with_default_bots();
        app.push_notice(NoticeLevel::Warn, "approval pending");
        let rendered = draw(50, 1, |frame| {
            plasma(frame, frame.area(), &app, &Theme::new(false))
        });
        assert!(rendered.contains("approval pending"), "got {rendered}");
    }

    #[test]
    fn status_bar_labels_the_posture_that_is_in_force() {
        let mut app = app_with_default_bots();
        app.mode = vikram_contracts::PermissionMode::Ask;
        let asked = draw(130, 1, |frame| {
            status_bar(frame, frame.area(), &app, &Theme::new(false))
        });
        assert!(asked.contains("ask"), "got {asked}");
        app.mode = vikram_contracts::PermissionMode::Yolo;
        let bypassed = draw(130, 1, |frame| {
            status_bar(frame, frame.area(), &app, &Theme::new(false))
        });
        assert!(
            bypassed.contains("yolo"),
            "a posture that approves without asking is still labelled: {bypassed}"
        );
        // Narrow enough that segments are dropped: the posture is one of the things that survives.
        let tight = draw(46, 1, |frame| {
            status_bar(frame, frame.area(), &app, &Theme::new(false))
        });
        assert!(tight.contains("yolo"), "got {tight}");
    }

    #[test]
    fn status_bar_reports_the_model_context_and_bot_count() {
        let mut app = app_with_default_bots();
        app.status = RuntimeStatus::Active;
        app.metrics = Metrics {
            model: Some("gpt-5-codex".to_owned()),
            context_used: 60_000,
            context_window: Some(120_000),
            mcp_connected: 1,
            mcp_configured: 2,
            plugins_loaded: 1,
            plugins_installed: 1,
            ..Metrics::default()
        };
        // 150 columns, not a narrow terminal: the totals column yields to the session sentence when
        // the line runs out, so a width that cannot hold both would test the yielding rather than
        // the reporting this test is named for.
        let rendered = draw(150, 1, |frame| {
            status_bar(frame, frame.area(), &app, &Theme::new(false))
        });
        // The row opens with the posture and carries no runtime label and no notice: a leading
        // message shifted every segment after it, so the line no longer has one.
        assert!(
            rendered.starts_with("⛨ ask"),
            "the line opens with the posture: {rendered}"
        );
        assert!(
            !rendered.contains("Runtime"),
            "the runtime label left this row: {rendered}"
        );
        assert!(rendered.contains("gpt-5-codex"), "got {rendered}");
        assert!(rendered.contains("50%"), "got {rendered}");
        assert!(rendered.contains("Background"), "got {rendered}");
        assert!(rendered.contains("MCP"), "got {rendered}");
        assert!(rendered.contains("Plugins"), "got {rendered}");
        assert!(rendered.contains("Bots"), "got {rendered}");
    }

    #[test]
    fn the_status_bar_names_the_whole_model_identifier_when_it_fits() {
        let mut app = app_with_default_bots();
        app.metrics = Metrics {
            model: Some("nvidia/nemotron-3.5-super-vl:free".to_owned()),
            ..Metrics::default()
        };
        let rendered = draw(160, 1, |frame| {
            status_bar(frame, frame.area(), &app, &Theme::new(false))
        });
        assert!(
            rendered.contains("nvidia/nemotron-3.5-super-vl:free"),
            "a clipped model name does not say which model is running: {rendered}"
        );
    }

    #[test]
    fn status_bar_drops_segments_instead_of_wrapping() {
        let mut app = app_with_default_bots();
        app.metrics = Metrics {
            model: Some("a-very-long-model-identifier".to_owned()),
            context_used: 10,
            context_window: Some(100),
            ..Metrics::default()
        };
        let rendered = draw(24, 1, |frame| {
            status_bar(frame, frame.area(), &app, &Theme::new(false))
        });
        assert_eq!(rendered.chars().count(), 24);
    }

    #[test]
    fn status_bar_keeps_the_clock_and_throughput_segments_when_idle() {
        let mut app = app_with_default_bots();
        app.metrics = Metrics {
            model: Some("gpt-5-codex".to_owned()),
            elapsed_ms: 65_000,
            ..Metrics::default()
        };
        let rendered = draw(140, 1, |frame| {
            status_bar(frame, frame.area(), &app, &Theme::new(false))
        });
        assert!(
            rendered.contains("1m05s"),
            "the session clock is missing: {rendered}"
        );
        assert!(
            rendered.contains("↑ -- t/s"),
            "an unmeasured rate has to say so rather than vanish: {rendered}"
        );
        assert!(
            !rendered.contains('◷'),
            "the turn timer belongs to a running turn only: {rendered}"
        );
    }

    #[test]
    fn the_turn_timer_appears_beside_the_session_clock_only_while_a_turn_runs() {
        let mut app = app_with_default_bots();
        app.metrics = Metrics {
            model: Some("gpt-5-codex".to_owned()),
            elapsed_ms: 65_000,
            turn_elapsed_ms: Some(9_400),
            tokens_per_sec: Some(13.0),
            ..Metrics::default()
        };
        let rendered = draw(140, 1, |frame| {
            status_bar(frame, frame.area(), &app, &Theme::new(false))
        });
        assert!(rendered.contains("◷ 9s"), "turn timer missing: {rendered}");
        assert!(
            rendered.contains("⏱ 1m05s"),
            "session clock missing: {rendered}"
        );
        assert!(rendered.contains("↑ 13 t/s"), "rate missing: {rendered}");
    }

    #[test]
    fn duration_names_its_unit() {
        assert_eq!(duration(5_000), "5s");
        assert_eq!(duration(65_000), "1m05s");
        assert_eq!(duration(3_720_000), "1h02m");
    }

    #[test]
    fn shortcuts_show_the_approved_line() {
        let app = app_with_default_bots();
        let rendered = draw(80, 1, |frame| {
            shortcuts(frame, frame.area(), &app, &Theme::new(false))
        });
        // Key and action are drawn as separate spans, so the row reads at two speeds: the key is
        // what the hand looks for, the action is what the head reads. Lowercase, because the action
        // continues the key rather than starting a sentence.
        assert!(rendered.contains("Enter send"), "got {rendered}");
        assert!(rendered.contains("Tab queue"), "got {rendered}");
        assert!(rendered.contains("Esc interrupt"), "got {rendered}");
        assert!(rendered.contains("Ctrl+C stop"), "got {rendered}");
    }

    #[test]
    fn help_overlay_lists_real_bindings() {
        let mut app = app_with_default_bots();
        app.overlay = Some(Overlay::Help);
        let rendered = draw(80, 24, |frame| {
            overlay(frame, frame.area(), &mut app, &Theme::new(false))
        });
        assert!(rendered.contains("Keys"), "got {rendered}");
        assert!(rendered.contains("Ctrl+O"), "got {rendered}");
        assert!(
            rendered.contains("reverse-search input history"),
            "got {rendered}"
        );
    }

    #[test]
    fn approval_overlay_shows_the_amendment_and_the_keys() {
        let mut app = app_with_default_bots();
        app.approvals.push(ApprovalPrompt {
            approval: ApprovalId::generate(),
            session: app.sessions[0].id.clone(),
            verb: "fs.write".to_owned(),
            resource: "/etc/hosts".to_owned(),
            reason: "outside the workspace".to_owned(),
            amendment: Some("allow fs write resource=/etc/**".to_owned()),
        });
        app.overlay = Some(Overlay::Approvals);
        let rendered = draw(80, 24, |frame| {
            overlay(frame, frame.area(), &mut app, &Theme::new(false))
        });
        assert!(rendered.contains("fs.write"), "got {rendered}");
        assert!(rendered.contains("/etc/**"), "got {rendered}");
        assert!(rendered.contains("Permissions 1"), "got {rendered}");
        assert!(
            rendered.contains("Approve and add to the allow list"),
            "got {rendered}"
        );
        assert!(rendered.contains("Add to the allow list"), "got {rendered}");
        assert!(rendered.contains("Deny"), "got {rendered}");
    }

    #[test]
    fn sub_agents_inspector_distinguishes_bots_from_spawned_work() {
        let mut app = app_with_default_bots();
        app.overlay = Some(Overlay::Inspector(InspectorKind::SubAgents));
        let idle = draw(80, 24, |frame| {
            overlay(frame, frame.area(), &mut app, &Theme::new(false))
        });
        assert!(
            idle.contains("none spawned"),
            "default bots must not be reported as sub-bots: {idle}"
        );

        app.bots[1].session = Some(app.sessions[0].id.clone());
        app.bots[1].state = BotState::Running;
        app.bots[1].current_task = Some("review the diff".to_owned());
        let busy = draw(80, 24, |frame| {
            overlay(frame, frame.area(), &mut app, &Theme::new(false))
        });
        assert!(busy.contains("Karma"), "spawned agent missing: {busy}");
        assert!(busy.contains("review the diff"), "task missing: {busy}");
    }

    #[test]
    fn background_inspector_lists_tasks_with_their_state() {
        let mut app = app_with_default_bots();
        app.tasks.push(BackgroundTask {
            id: "t1".to_owned(),
            name: "cargo nextest".to_owned(),
            session: app.sessions[0].id.clone(),
            state: TaskState::Running,
            started_ms: 0,
        });
        app.overlay = Some(Overlay::Inspector(InspectorKind::Background));
        let rendered = draw(80, 24, |frame| {
            overlay(frame, frame.area(), &mut app, &Theme::new(false))
        });
        assert!(rendered.contains("cargo nextest"), "got {rendered}");
        assert!(rendered.contains("running"), "got {rendered}");
    }

    #[test]
    fn context_inspector_reports_absent_windows_rather_than_guessing() {
        let mut app = app_with_default_bots();
        app.overlay = Some(Overlay::Inspector(InspectorKind::Context));
        let rendered = draw(80, 24, |frame| {
            overlay(frame, frame.area(), &mut app, &Theme::new(false))
        });
        assert!(rendered.contains("no live session"), "got {rendered}");
        assert!(!rendered.contains(" 0% of"), "got {rendered}");
    }

    #[test]
    fn context_inspector_prices_the_last_request_part_by_part() {
        let mut app = app_with_default_bots();
        let id = app.sessions[0].id.clone();
        app.context_detail = Some((
            id,
            ContextMeter {
                window: 128_000,
                usable: 100_000,
                used: 42_000,
                measured: true,
                limits_assumed: false,
                output_reserve: 16_384,
                overhead: 12_000,
                parts: vec![
                    ContextPart::new("project instructions", 1_800),
                    ContextPart::new("tool schemas", 9_200),
                    ContextPart::new("conversation", 31_000),
                ],
            },
        ));
        app.overlay = Some(Overlay::Inspector(InspectorKind::Context));
        let rendered = draw(80, 30, |frame| {
            overlay(frame, frame.area(), &mut app, &Theme::new(false))
        });
        for needle in [
            "project instructions",
            "tool schemas",
            "conversation",
            "measured",
            "in the last request",
        ] {
            assert!(rendered.contains(needle), "missing {needle}: {rendered}");
        }
    }

    #[test]
    fn status_bar_draws_the_context_meter_in_every_state() {
        let mut app = app_with_default_bots();
        app.metrics = Metrics {
            context_used: 50_000,
            context_window: Some(100_000),
            context_measured: false,
            context_limit_assumed: true,
            ..Metrics::default()
        };
        let assumed = draw(130, 1, |frame| {
            status_bar(frame, frame.area(), &app, &Theme::new(false))
        });
        // The box is the signal; only the figure behind it changes. An assumed window keeps its
        // meter but reports the measured count, because a share of an invented number says nothing.
        assert!(assumed.contains("ctx ["), "got {assumed}");
        assert!(assumed.contains("50.0k*"), "got {assumed}");
        assert!(
            !assumed.contains('%'),
            "an assumed window carries no percentage: {assumed}"
        );
        app.metrics.context_limit_assumed = false;
        let estimated = draw(130, 1, |frame| {
            status_bar(frame, frame.area(), &app, &Theme::new(false))
        });
        assert!(estimated.contains("ctx ["), "got {estimated}");
        assert!(estimated.contains("~50%"), "got {estimated}");
        app.metrics.context_measured = true;
        let measured = draw(130, 1, |frame| {
            status_bar(frame, frame.area(), &app, &Theme::new(false))
        });
        assert!(
            measured.contains("[") && measured.contains("50%"),
            "got {measured}"
        );
        assert!(
            !measured.contains("~50%"),
            "a measured figure carries no approximation mark: {measured}"
        );
    }

    #[test]
    fn add_agent_overlay_marks_the_selection() {
        let mut app = app_with_default_bots();
        app.overlay = Some(Overlay::AddBot { selected: 1 });
        let rendered = draw(80, 24, |frame| {
            overlay(frame, frame.area(), &mut app, &Theme::new(false))
        });
        assert!(rendered.contains("Spawn a bot"), "got {rendered}");
        for name in ["Rudra", "Karma", "Verifier"] {
            assert!(rendered.contains(name), "{name} missing: {rendered}");
        }
    }

    #[test]
    fn rename_overlay_prefills_the_current_title() {
        let mut app = app_with_default_bots();
        app.overlay = Some(Overlay::RenameSession {
            editor: crate::editor::Editor::from_text("Renamed"),
        });
        let rendered = draw(80, 24, |frame| {
            overlay(frame, frame.area(), &mut app, &Theme::new(false))
        });
        assert!(rendered.contains("now: First"), "got {rendered}");
        assert!(rendered.contains("Renamed"), "got {rendered}");
        assert!(rendered.contains("[enter save]"), "got {rendered}");
    }

    #[test]
    fn ascii_mode_never_draws_wide_glyphs() {
        let app = app_with_default_bots();
        let theme = Theme::new(true);
        let rendered = draw(40, 8, |frame| bots_panel(frame, frame.area(), &app, &theme));
        for glyph in ['●', '○', '…', '▸', '█', '░'] {
            assert!(
                !rendered.contains(glyph),
                "ascii mode drew {glyph}: {rendered}"
            );
        }
    }

    #[test]
    fn human_readable_numbers() {
        assert_eq!(human(0), "0");
        assert_eq!(human(999), "999");
        assert_eq!(human(1_500), "1.5k");
        assert_eq!(human(2_400_000), "2.4M");
    }

    #[test]
    fn clip_never_exceeds_the_budget() {
        for width in 0..10 {
            let clipped = clip("abcdefghijklmnop", width, false);
            assert!(clipped.width() <= width, "{clipped:?} exceeds {width}");
        }
        assert_eq!(clip("short", 8, false), "short");
    }

    #[test]
    fn ratio_bar_fills_proportionally() {
        let theme = Theme::new(true);
        assert_eq!(ratio_bar(0.0, 4, &theme), "----");
        assert_eq!(ratio_bar(0.5, 4, &theme), "##--");
        assert_eq!(ratio_bar(1.0, 4, &theme), "####");
        assert_eq!(ratio_bar(2.0, 4, &theme), "####");
    }
}

fn skills_lines(_app: &App, theme: &Theme) -> (String, Vec<Line<'static>>) {
    let mut lines: Vec<Line> = Vec::new();
    let root = std::env::current_dir().unwrap_or_default();
    let registry = vikram_runtime::SkillRegistry::for_workspace(&root);
    let skills = registry.list();
    let count = skills.len();
    if skills.is_empty() {
        lines.push(Line::styled("  no skills found", theme.secondary()));
    } else {
        for skill in &skills {
            lines.push(Line::from(vec![
                Span::styled(format!("  {} ", theme.bullet()), theme.muted()),
                Span::styled(clip(&skill.name, 24, theme.ascii), theme.secondary()),
                Span::styled(
                    format!("  {}", clip(&skill.description, 48, theme.ascii)),
                    theme.muted(),
                ),
            ]));
        }
    }
    lines.push(Line::from(""));
    lines.push(Line::styled(
        "  F8 to close · bodies load on demand",
        theme.muted(),
    ));
    (format!("Skills {}", count), lines)
}

#[cfg(test)]
mod picker_panel_tests {
    use super::*;
    use crate::widgets::{Picker, PickerItem, PickerKind};
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;
    use vikram_contracts::Message;
    use vikram_runtime::types::{SessionSnapshot, TurnState};

    fn draw(width: u16, height: u16, render: impl FnOnce(&mut Frame)) -> String {
        let mut terminal = Terminal::new(TestBackend::new(width, height)).expect("terminal");
        terminal.draw(render).expect("draw");
        terminal
            .backend()
            .buffer()
            .content()
            .iter()
            .map(|cell| cell.symbol())
            .collect()
    }

    fn app() -> App {
        let mut app = App::new(false);
        app.sessions
            .push(crate::app::SessionView::from_snapshot(SessionSnapshot {
                id: vikram_contracts::SessionId::generate(),
                title: "First".to_owned(),
                cwd: "/work".into(),
                model: Some("nvidia/nemotron-3.5-ultra-550b-a55b".to_owned()),
                messages: vec![Message::user("hi")],
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
                title_source: "generic".to_owned(),
                title_status: "pending".to_owned(),
            }));
        app
    }

    #[test]
    fn a_long_model_name_is_not_clipped_by_the_dialog_width() {
        let mut app = app();
        app.open_picker(Picker::new(
            PickerKind::Model,
            "Model",
            vec![
                PickerItem::new(
                    "nvidia/nemotron-3.5-ultra-550b-a55b",
                    "",
                    "nvidia/nemotron-3.5-ultra-550b-a55b",
                )
                .as_current(),
                PickerItem::new("qwen3-coder", "128k context", "qwen3-coder"),
            ],
        ));
        let rendered = draw(120, 30, |frame| {
            overlay(frame, frame.area(), &mut app, &Theme::new(false))
        });
        assert!(
            rendered.contains("nemotron-3.5-ultra-550b-a55b"),
            "the row the runtime acts on has to be readable: {rendered}"
        );
        assert!(rendered.contains("(current)"), "got {rendered}");
        assert!(rendered.contains("enter choose"), "got {rendered}");
        assert!(
            rendered.contains("ctrl+t type a name"),
            "the model panel has to advertise its own verb: {rendered}"
        );
    }

    #[test]
    fn an_info_panel_draws_what_the_host_read_from_the_runtime() {
        let mut app = app();
        app.open_info(
            "Context",
            vec![
                "used: 1024 tokens".to_owned(),
                "window: not known for this model".to_owned(),
            ],
        );
        let rendered = draw(90, 20, |frame| {
            overlay(frame, frame.area(), &mut app, &Theme::new(false))
        });
        assert!(rendered.contains("Context"), "got {rendered}");
        assert!(rendered.contains("1024"), "got {rendered}");
        assert!(rendered.contains("not known"), "got {rendered}");
    }

    #[test]
    fn a_confirm_panel_says_what_would_change_and_how_to_keep_it() {
        let mut app = app();
        app.open_confirm(
            "Forget a rule",
            vec!["allow fs write resource=src/**".to_owned()],
            crate::app::UiCommand::Unallow { index: 0 },
        );
        let rendered = draw(90, 20, |frame| {
            overlay(frame, frame.area(), &mut app, &Theme::new(false))
        });
        assert!(rendered.contains("fs write"), "got {rendered}");
        assert!(rendered.contains("[enter confirm]"), "got {rendered}");
        assert!(rendered.contains("esc keep as is"), "got {rendered}");
    }
}
