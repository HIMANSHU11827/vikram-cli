//! The chrome around the transcript: session tabs, the BOTS roster, the activity strip, the status
//! bar, the shortcut hints, and the modal overlays.
//!
//! Every panel is a pure function of [`App`]. None of them reads a clock, emits a command, or
//! invents state: the BOTS panel lists exactly what `app.agents` already holds, and a panel with
//! nothing to show says so instead of drawing a plausible placeholder.

use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Clear, Paragraph};
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};
use vikram_runtime::types::TaskState;

use crate::app::{ActivityKind, App, Focus, InspectorKind, Overlay, TabHit};
use crate::theme::{StatusSegment, Theme};

/// A tab is a reminder of what exists, not a synopsis of the session.
const TAB_TITLE_CAP: usize = 22;
const AGENT_NAME_CAP: usize = 14;
const AGENT_LINE_CAP: usize = 34;
const DIALOG_WIDTH: u16 = 62;

const HELP_ROWS: [(&str, &str); 18] = [
    ("Enter", "send the message"),
    ("Shift+Enter", "insert a newline"),
    ("Ctrl+N", "new session"),
    ("Ctrl+Tab", "next session"),
    ("Ctrl+B", "toggle the BOTS panel"),
    ("Ctrl+O", "approvals"),
    ("Ctrl+R", "rename the session"),
    ("r", "resume selected bot"),
    ("F2", "background tasks"),
    ("F3", "MCP servers"),
    ("F4", "plugins"),
    ("F5", "sub-agents"),
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
        let hint = "   Ctrl+N starts one";
        spans.push(Span::styled(hint, theme.muted()));
        x += hint.width() as u16;
        // + New control
        let new_label = "  + New";
        app.new_session_x = Some((x, x + new_label.width() as u16));
        spans.push(Span::styled(new_label, Style::default().fg(theme.accent)));
        frame.render_widget(Paragraph::new(Line::from(spans)), area);
        return;
    }

    for (index, session) in app.sessions.iter().enumerate() {
        if !spans.is_empty() {
            let sep = format!(" {} ", theme.separator());
            let w = sep.width() as u16;
            spans.push(Span::styled(sep, theme.frame()));
            x += w;
        }
        let tab_start = x;
        let active = index == app.active;
        let glyph = if session.is_busy() || active {
            theme.filled_dot()
        } else {
            theme.hollow_dot()
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
        let title = clip(&session.title, TAB_TITLE_CAP, theme.ascii);
        let tab_text = format!(" {glyph} {title}");
        let tab_w = tab_text.width() as u16;
        spans.push(Span::styled(tab_text.clone(), style));
        x += tab_w;
        if session.queued > 0 {
            let q = format!(" +{}", session.queued);
            let qw = q.width() as u16;
            spans.push(Span::styled(q, Style::default().fg(theme.warn)));
            x += qw;
        }
        if session.unread > 0 {
            let u = format!(" ({})", session.unread);
            let uw = u.width() as u16;
            spans.push(Span::styled(u, Style::default().fg(theme.accent_dim)));
            x += uw;
        }
        // close button
        let close_label = " ×";
        let close_w = close_label.width() as u16;
        let close_start = x;
        spans.push(Span::styled(close_label, theme.muted()));
        x += close_w;
        let tab_end = x;
        app.tab_hits.push(TabHit {
            session: session.id.clone(),
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
    let new_label = "+ New";
    let new_w = new_label.width() as u16;
    app.new_session_x = Some((x, x + new_w));
    spans.push(Span::styled(new_label, Style::default().fg(theme.accent)));

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
    if app.teams.is_empty() {
        lines.push(Line::styled(" no teams", theme.muted()));
    } else {
        for team in &app.teams {
            let name = clip(
                &team.name,
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
            // Independent activity indicator - only when team is working and not paused
            if team.working && !team.paused {
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
    if app.agents.is_empty() {
        lines.push(Line::styled(" no bots", theme.muted()));
        lines.push(Line::styled(" `vikram agents` lists them", theme.muted()));
    } else {
        for (index, agent) in app.agents.iter().enumerate() {
            let selected = focused && index == app.agent_selection;
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
    left.push(Span::styled(
        format!("{model_glyph} {}", clip(&model, 20, theme.ascii)),
        theme.status_style(StatusSegment::Value),
    ));

    // ctx [bar] pct%
    if let Some(ratio) = app.metrics.context_ratio() {
        let pct = (f64::from(ratio) * 100.0).round() as u64;
        let bar = context_bar(ratio, 10, theme);
        left.push(Span::raw(sep.to_owned()));
        left.push(Span::styled(
            "ctx ",
            theme.status_style(StatusSegment::Label),
        ));
        left.push(Span::styled(
            format!("[{bar}] {pct}%"),
            theme.status_style(StatusSegment::Value),
        ));
    }

    // runtime timing (uptime)
    if app.metrics.elapsed_ms > 0 {
        left.push(Span::raw(sep.to_owned()));
        left.push(Span::styled(
            format!("{}s", app.metrics.elapsed_ms / 1000),
            theme.status_style(StatusSegment::Value),
        ));
    }

    // ⏱ turn timing
    if let Some(ms) = app.metrics.turn_elapsed_ms {
        left.push(Span::raw(sep.to_owned()));
        let t_glyph = if theme.ascii { "T" } else { "⏱" };
        left.push(Span::styled(
            format!("{t_glyph} {:.1}s", ms as f64 / 1000.0),
            theme.status_style(StatusSegment::Value),
        ));
    }

    // ~tok/s
    if let Some(rate) = app.metrics.tokens_per_sec {
        left.push(Span::raw(sep.to_owned()));
        left.push(Span::styled(
            format!("~{:.0} tok/s", rate),
            theme.status_style(StatusSegment::Value),
        ));
    }

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
        format!("{agent_glyph} Agents {}", app.agents.len()),
        theme.status_style(StatusSegment::Value),
    ));

    // Status dot + runtime label at the very left.
    let mut spans: Vec<Span> = Vec::new();
    spans.push(Span::styled(
        format!("{} {}", theme.filled_dot(), app.status.label()),
        theme.runtime_style(&app.status),
    ));
    spans.push(Span::raw(sep.to_owned()));
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
    spans.push(Span::raw(sep.to_owned()));
    spans.extend(left);
    // pipe separator between groups
    spans.push(Span::raw(pipe.to_owned()));
    spans.extend(right);

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
    if popup.items.is_empty() || anchor.width < 12 || anchor.y < 4 {
        return;
    }
    let height = (popup.items.len() as u16 + 2)
        .min(12)
        .min(anchor.y.saturating_sub(1));
    if height < 3 {
        return;
    }
    let width = anchor.width.saturating_sub(2).min(76);
    let rect = Rect::new(anchor.x + 1, anchor.y.saturating_sub(height), width, height);
    frame.render_widget(Clear, rect);
    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(theme.accent_dim));
    let inner = block.inner(rect);
    frame.render_widget(block, rect);

    let mut lines: Vec<Line> = Vec::new();
    for (index, item) in popup.items.iter().enumerate() {
        let selected = index == popup.selected;
        let marker = if selected {
            if theme.ascii { ">" } else { "›" }
        } else {
            " "
        };
        let name_style = if selected {
            Style::default()
                .fg(theme.accent)
                .add_modifier(Modifier::BOLD)
        } else {
            Style::default().fg(theme.text)
        };
        lines.push(Line::from(vec![
            Span::styled(format!(" {marker} "), theme.muted()),
            Span::styled(clip(&item.text, 28, theme.ascii), name_style),
            Span::styled(
                format!("  {}", clip(&item.description, 40, theme.ascii)),
                theme.muted(),
            ),
        ]));
    }
    lines.truncate(inner.height as usize);
    frame.render_widget(Paragraph::new(lines), inner);
}

pub fn shortcuts(frame: &mut Frame, area: Rect, app: &App, theme: &Theme) {
    if area.height == 0 || area.width == 0 {
        return;
    }

    let hints = if app.overlay.is_some() {
        "Esc close  ·  Enter confirm"
    } else {
        "Enter · Send    Tab · Queue    Esc · Interrupt    Ctrl+C · Stop"
    };

    let text = clip(hints, area.width.saturating_sub(2) as usize, theme.ascii);
    frame.render_widget(
        Paragraph::new(Line::from(Span::styled(format!(" {text}"), theme.muted()))),
        area,
    );
}

pub fn overlay(frame: &mut Frame, area: Rect, app: &App, theme: &Theme) {
    let Some(current) = app.overlay.as_ref() else {
        return;
    };

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
            area.y + area.height.saturating_sub(height + 4),
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
                let style = if i == *selected {
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

    let (title, lines) = overlay_content(app, current, theme);
    let height = (lines.len() as u16 + 2).min(area.height);
    let width = DIALOG_WIDTH.min(area.width);
    let rect = centered(area, width, height);

    frame.render_widget(Clear, rect);
    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(theme.accent))
        .title(Line::from(Span::styled(
            format!(" {title} "),
            Style::default()
                .fg(theme.accent_dim)
                .add_modifier(Modifier::BOLD),
        )));
    let inner = block.inner(rect);
    frame.render_widget(block, rect);
    frame.render_widget(Paragraph::new(lines), inner);
}

fn overlay_content(app: &App, overlay: &Overlay, theme: &Theme) -> (String, Vec<Line<'static>>) {
    match overlay {
        Overlay::Help => (
            "Keys".to_owned(),
            HELP_ROWS
                .iter()
                .map(|(key, description)| {
                    Line::from(vec![
                        Span::styled(format!("  {key:<14}"), Style::default().fg(theme.accent)),
                        Span::styled((*description).to_owned(), theme.secondary()),
                    ])
                })
                .collect(),
        ),
        Overlay::Approvals => approval_lines(app, theme),
        Overlay::Inspector(kind) => (
            inspector_title(kind).to_owned(),
            inspector_lines(app, kind, theme),
        ),
        Overlay::AddAgent { selected } => add_agent_lines(app, *selected, theme),
        Overlay::RenameSession { editor } => rename_lines(app, editor, theme),
        Overlay::Autocomplete { .. } => ("Autocomplete".to_owned(), vec![]),
        Overlay::Skills => skills_lines(app, theme),
    }
}

fn approval_lines(app: &App, theme: &Theme) -> (String, Vec<Line<'static>>) {
    let mut lines: Vec<Line> = Vec::new();
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
    lines.push(Line::styled(
        "  a allow once · p keep on the list · d deny",
        theme.muted(),
    ));
    (format!("Approvals {}", app.approvals.len()), lines)
}

fn inspector_title(kind: &InspectorKind) -> &'static str {
    match kind {
        InspectorKind::Background => "Background tasks",
        InspectorKind::Mcp => "MCP servers",
        InspectorKind::Plugins => "Plugins",
        InspectorKind::SubAgents => "Sub-agents",
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
                .agents
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
        InspectorKind::Context => match app.metrics.context_ratio() {
            Some(ratio) => {
                let window = app.metrics.context_window.unwrap_or(0);
                lines.push(kv("used", human(app.metrics.context_used), theme));
                lines.push(kv("window", human(window), theme));
                lines.push(Line::from(""));
                lines.push(Line::styled(
                    format!("  {}", ratio_bar(ratio, 40, theme)),
                    Style::default().fg(ratio_colour(ratio, theme)),
                ));
            }
            None => lines.push(Line::styled(
                "  the provider has not reported a window",
                theme.secondary(),
            )),
        },
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
    if app.agents.is_empty() {
        lines.push(Line::styled("  no bot profiles", theme.secondary()));
    } else {
        for (index, agent) in app.agents.iter().enumerate() {
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
    lines.push(Line::styled("  Enter spawn · Esc close", theme.muted()));
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
    lines.push(Line::styled("  Enter save · Esc cancel", theme.muted()));
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

fn centered(area: Rect, width: u16, height: u16) -> Rect {
    let width = width.min(area.width);
    let height = height.min(area.height);
    Rect::new(
        area.x + (area.width - width) / 2,
        area.y + (area.height - height) / 2,
        width,
        height,
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::app::Activity;
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;
    use vikram_contracts::{AgentId, ApprovalId, Message, SessionId};
    use vikram_runtime::types::{
        AgentProfile, AgentSnapshot, AgentState, ApprovalPrompt, BackgroundTask, Metrics,
        NoticeLevel, RuntimeStatus, SessionSnapshot, TurnState,
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
            agent: None,
            queued: 0,
        }
    }

    fn bot(name: &str, role: &str, state: AgentState) -> AgentSnapshot {
        AgentSnapshot {
            profile: AgentProfile {
                id: AgentId::generate(),
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
        app.agents = vec![
            bot("Rudra", "Implementer", AgentState::Idle),
            bot("Karma", "Reviewer", AgentState::Idle),
            bot("Verifier", "Verifier", AgentState::Idle),
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
        app.agents[0].state = AgentState::Running;
        app.agents[0].current_task = Some("porting the store".to_owned());

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
        app.agents.extend(
            (0..12).map(|index| bot(&format!("bot {index}"), "Reviewer", AgentState::Idle)),
        );
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
        let rendered = draw(130, 1, |frame| {
            status_bar(frame, frame.area(), &app, &Theme::new(false))
        });
        assert!(rendered.contains("Runtime active"), "got {rendered}");
        assert!(rendered.contains("gpt-5-codex"), "got {rendered}");
        assert!(rendered.contains("50%"), "got {rendered}");
        assert!(rendered.contains("Background"), "got {rendered}");
        assert!(rendered.contains("MCP"), "got {rendered}");
        assert!(rendered.contains("Plugins"), "got {rendered}");
        assert!(rendered.contains("Agents"), "got {rendered}");
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
    fn shortcuts_show_the_approved_line() {
        let app = app_with_default_bots();
        let rendered = draw(80, 1, |frame| {
            shortcuts(frame, frame.area(), &app, &Theme::new(false))
        });
        assert!(rendered.contains("Enter · Send"), "got {rendered}");
        assert!(rendered.contains("Tab · Queue"), "got {rendered}");
        assert!(rendered.contains("Esc · Interrupt"), "got {rendered}");
        assert!(rendered.contains("Ctrl+C · Stop"), "got {rendered}");
    }

    #[test]
    fn help_overlay_lists_real_bindings() {
        let mut app = app_with_default_bots();
        app.overlay = Some(Overlay::Help);
        let rendered = draw(80, 24, |frame| {
            overlay(frame, frame.area(), &app, &Theme::new(false))
        });
        assert!(rendered.contains("Keys"), "got {rendered}");
        assert!(rendered.contains("Ctrl+O"), "got {rendered}");
        assert!(rendered.contains("rename the session"), "got {rendered}");
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
            overlay(frame, frame.area(), &app, &Theme::new(false))
        });
        assert!(rendered.contains("fs.write"), "got {rendered}");
        assert!(rendered.contains("/etc/**"), "got {rendered}");
        assert!(rendered.contains("keep on the list"), "got {rendered}");
    }

    #[test]
    fn sub_agents_inspector_distinguishes_bots_from_spawned_work() {
        let mut app = app_with_default_bots();
        app.overlay = Some(Overlay::Inspector(InspectorKind::SubAgents));
        let idle = draw(80, 24, |frame| {
            overlay(frame, frame.area(), &app, &Theme::new(false))
        });
        assert!(
            idle.contains("none spawned"),
            "default bots must not be reported as sub-agents: {idle}"
        );

        app.agents[1].session = Some(app.sessions[0].id.clone());
        app.agents[1].state = AgentState::Running;
        app.agents[1].current_task = Some("review the diff".to_owned());
        let busy = draw(80, 24, |frame| {
            overlay(frame, frame.area(), &app, &Theme::new(false))
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
            overlay(frame, frame.area(), &app, &Theme::new(false))
        });
        assert!(rendered.contains("cargo nextest"), "got {rendered}");
        assert!(rendered.contains("running"), "got {rendered}");
    }

    #[test]
    fn context_inspector_reports_absent_windows_rather_than_guessing() {
        let mut app = app_with_default_bots();
        app.overlay = Some(Overlay::Inspector(InspectorKind::Context));
        let rendered = draw(80, 24, |frame| {
            overlay(frame, frame.area(), &app, &Theme::new(false))
        });
        assert!(rendered.contains("has not reported"), "got {rendered}");
    }

    #[test]
    fn add_agent_overlay_marks_the_selection() {
        let mut app = app_with_default_bots();
        app.overlay = Some(Overlay::AddAgent { selected: 1 });
        let rendered = draw(80, 24, |frame| {
            overlay(frame, frame.area(), &app, &Theme::new(false))
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
            overlay(frame, frame.area(), &app, &Theme::new(false))
        });
        assert!(rendered.contains("now: First"), "got {rendered}");
        assert!(rendered.contains("Renamed"), "got {rendered}");
        assert!(rendered.contains("Enter save"), "got {rendered}");
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
