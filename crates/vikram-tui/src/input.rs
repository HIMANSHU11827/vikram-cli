//! Keyboard and paste handling.
//!
//! One rule drives this module: a key never reaches the runtime directly. It edits view state or
//! queues a [`UiCommand`], which keeps the input loop testable and keeps a UI concern (for example
//! a hidden panel) from cancelling runtime work.

use crossterm::event::{KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use vikram_contracts::AgentId;

use crate::app::{
    AgentAction, App, Focus, InspectorKind, Overlay, PromptAction, Suggestion, SuggestionPopup,
    UiCommand,
};
use crate::commands;
use crate::editor::Editor;
use crate::widgets::{Picker, PickerItem, PickerKind};

pub fn handle_key(app: &mut App, key: KeyEvent) -> bool {
    if key.kind == KeyEventKind::Release {
        return false;
    }
    // A pending question owns the keyboard: the runtime is blocked until it is answered or the
    // turn is cancelled, so characters belong to the answer field rather than the draft. Ctrl+C
    // falls through to the global handler so cancelling stays available.
    let ctrl_held = key.modifiers.contains(KeyModifiers::CONTROL);
    if app.pending_question.is_some() && !(ctrl_held && key.code == KeyCode::Char('c')) {
        return handle_question(app, key);
    }
    if app.suggestions.is_some() && handle_suggestion_key(app, key) {
        return true;
    }
    if key.code == KeyCode::Esc && app.selected_agent.is_some() {
        app.selected_agent = None;
        app.focus = Focus::Input;
        return true;
    }
    // Ctrl+C is the operator's stop, not the panel's key: a menu that traps it is a menu you
    // cannot leave in a hurry. Panels get every other key.
    if app.overlay.is_some() && !(ctrl_held && key.code == KeyCode::Char('c')) {
        return handle_overlay(app, key);
    }

    let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
    let shift = key.modifiers.contains(KeyModifiers::SHIFT);

    // ---- global shortcuts ------------------------------------------------
    match key.code {
        KeyCode::Char('c') if ctrl => {
            app.overlay = None;
            app.suggestions = None;
            if app.active_session().is_some_and(|s| s.is_busy()) {
                if let Some(session) = app.active_session() {
                    app.commands.push(UiCommand::Cancel {
                        session: session.id.clone(),
                    });
                }
            } else {
                app.commands.push(UiCommand::Quit);
                app.should_quit = true;
            }
            return true;
        }
        KeyCode::Char('n') if ctrl => {
            app.commands.push(UiCommand::NewSession);
            return true;
        }
        KeyCode::Tab if ctrl && shift => {
            app.cycle_session(false);
            return true;
        }
        KeyCode::BackTab if ctrl => {
            app.cycle_session(false);
            return true;
        }
        KeyCode::Tab if ctrl => {
            app.cycle_session(true);
            return true;
        }
        KeyCode::Char('b') if ctrl => {
            app.show_bots = !app.show_bots;
            return true;
        }
        KeyCode::Char('p') if ctrl => {
            app.overlay = Some(Overlay::Help);
            return true;
        }
        KeyCode::Char('l') if ctrl => {
            app.push_notice(
                vikram_runtime::types::NoticeLevel::Info,
                "screen redrawn; history preserved",
            );
            return true;
        }
        KeyCode::Char('t') if ctrl => {
            // Thinking is folded by default because a reasoning stream is the longest thing a
            // model emits and it should not push the answer off the screen.
            let Some(session) = app.active_session_mut() else {
                app.push_notice(
                    vikram_runtime::types::NoticeLevel::Info,
                    "no session to show thinking in",
                );
                return true;
            };
            session.expanded_thought = !session.expanded_thought;
            let state = if session.expanded_thought {
                "thinking shown"
            } else {
                "thinking folded"
            };
            app.push_notice(vikram_runtime::types::NoticeLevel::Info, state);
            return true;
        }
        KeyCode::F(1) => {
            app.overlay = Some(Overlay::Help);
            return true;
        }
        KeyCode::F(2) => {
            open_inspector(app, InspectorKind::Background);
            return true;
        }
        KeyCode::F(3) => {
            open_inspector(app, InspectorKind::Mcp);
            return true;
        }
        KeyCode::F(4) => {
            open_inspector(app, InspectorKind::Plugins);
            return true;
        }
        KeyCode::F(5) => {
            open_inspector(app, InspectorKind::SubAgents);
            return true;
        }
        KeyCode::F(6) => {
            open_inspector(app, InspectorKind::Context);
            return true;
        }
        KeyCode::F(7) => {
            open_inspector(app, InspectorKind::Model);
            return true;
        }
        KeyCode::F(8) => {
            app.overlay = Some(Overlay::Skills);
            return true;
        }
        KeyCode::Char('o') if ctrl => {
            if app.approvals.is_empty() {
                app.push_notice(
                    vikram_runtime::types::NoticeLevel::Info,
                    "no tool is waiting for approval",
                );
            } else {
                app.approval_selection = 0;
                app.overlay = Some(Overlay::Approvals);
            }
            return true;
        }
        KeyCode::Char('r') if ctrl => {
            let title = app
                .active_session()
                .map(|s| s.title.clone())
                .unwrap_or_default();
            app.overlay = Some(Overlay::RenameSession {
                editor: Editor::from_text(&title),
            });
            return true;
        }
        KeyCode::Char('a') if ctrl => {
            if app.agents.is_empty() {
                app.push_notice(
                    vikram_runtime::types::NoticeLevel::Info,
                    "no bots exist yet; spawn one with `vikram agents`",
                );
            } else {
                app.agent_selection = 0;
                app.overlay = Some(Overlay::AddAgent { selected: 0 });
            }
            return true;
        }
        KeyCode::Char('?') => {
            let empty = app
                .active_session()
                .map(|s| s.editor.is_empty())
                .unwrap_or(true);
            if empty {
                app.overlay = Some(Overlay::Help);
                return true;
            }
        }
        KeyCode::PageUp => {
            app.scroll_up(10);
            return true;
        }
        KeyCode::PageDown => {
            app.scroll_down(10);
            return true;
        }
        KeyCode::Esc => {
            if app.active_session().is_some_and(|s| s.is_busy()) {
                if let Some(session) = app.active_session() {
                    app.commands.push(UiCommand::Cancel {
                        session: session.id.clone(),
                    });
                }
            }
            return true;
        }
        _ => {}
    }

    let handled = match app.focus {
        Focus::Input => handle_input(app, key),
        Focus::Conversation => handle_conversation(app, key),
        Focus::Bots => handle_bots(app, key),
    };
    if app.focus == Focus::Input && app.pending_question.is_none() {
        refresh_suggestions(app);
    }
    handled
}

/// The question prompt is a one-line text field: characters edit it, Enter hands the answer to
/// [`App::submit_answer`], and Esc drops the prompt. Every key is consumed here so nothing leaks
/// into the draft while the runtime is blocked on an answer.
fn handle_question(app: &mut App, key: KeyEvent) -> bool {
    match key.code {
        KeyCode::Esc => {
            app.dismiss_question();
            true
        }
        KeyCode::Enter => {
            let answer = app
                .pending_question
                .as_ref()
                .map(|pending| pending.input.trim().to_owned())
                .unwrap_or_default();
            // An empty answer is not an answer: keep the prompt up instead of inventing one.
            if !answer.is_empty() {
                app.submit_answer(answer);
            }
            true
        }
        KeyCode::Backspace => {
            if let Some(pending) = app.pending_question.as_mut() {
                pending.input.pop();
            }
            true
        }
        KeyCode::Char(ch)
            if !key.modifiers.contains(KeyModifiers::CONTROL)
                && !key.modifiers.contains(KeyModifiers::ALT) =>
        {
            if let Some(pending) = app.pending_question.as_mut() {
                pending.input.push(ch);
            }
            true
        }
        _ => true,
    }
}

fn handle_input(app: &mut App, key: KeyEvent) -> bool {
    let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
    let shift = key.modifiers.contains(KeyModifiers::SHIFT);

    match key.code {
        KeyCode::Enter if shift => {
            if let Some(session) = app.active_session_mut() {
                session.editor.insert_newline();
            }
            true
        }
        KeyCode::Enter => {
            submit(app, false);
            true
        }
        KeyCode::Tab => {
            let text = app
                .active_session()
                .map(|s| s.editor.text())
                .unwrap_or_default();
            if text.trim().starts_with('/') {
                // Tab completes from the same table the palette and the dispatcher read, so it can
                // never offer a command that would be answered with "unknown command".
                let candidates: Vec<String> = crate::commands::starting_with(&text)
                    .into_iter()
                    .map(|command| format!("/{}", command.name))
                    .collect();
                if !candidates.is_empty() {
                    app.overlay = Some(Overlay::Autocomplete {
                        candidates,
                        selected: 0,
                    });
                    return true;
                }
            }
            let has_text = app
                .active_session()
                .map(|s| !s.editor.is_empty())
                .unwrap_or(false);
            if has_text {
                submit(app, true);
                true
            } else {
                app.focus = Focus::Conversation;
                true
            }
        }
        KeyCode::Backspace => {
            if let Some(session) = app.active_session_mut() {
                if ctrl {
                    session.editor.delete_word_left();
                } else {
                    session.editor.backspace();
                }
            }
            true
        }
        KeyCode::Delete => {
            if let Some(session) = app.active_session_mut() {
                session.editor.delete();
            }
            true
        }
        KeyCode::Left if ctrl => {
            if let Some(session) = app.active_session_mut() {
                session.editor.word_left();
            }
            true
        }
        KeyCode::Right if ctrl => {
            if let Some(session) = app.active_session_mut() {
                session.editor.word_right();
            }
            true
        }
        KeyCode::Left => {
            if let Some(session) = app.active_session_mut() {
                session.editor.move_left();
            }
            true
        }
        KeyCode::Right => {
            if let Some(session) = app.active_session_mut() {
                session.editor.move_right();
            }
            true
        }
        KeyCode::Up => {
            let single = app
                .active_session()
                .map(|s| s.editor.line_count() == 1)
                .unwrap_or(true);
            if let Some(session) = app.active_session_mut() {
                if single {
                    session.editor.history_previous();
                } else {
                    session.editor.move_up();
                }
            }
            true
        }
        KeyCode::Down => {
            let single = app
                .active_session()
                .map(|s| s.editor.line_count() == 1)
                .unwrap_or(true);
            if let Some(session) = app.active_session_mut() {
                if single {
                    session.editor.history_next();
                } else {
                    session.editor.move_down();
                }
            }
            true
        }
        KeyCode::Home if ctrl => {
            if let Some(session) = app.active_session_mut() {
                session.editor.home();
            }
            true
        }
        KeyCode::End if ctrl => {
            if let Some(session) = app.active_session_mut() {
                session.editor.end();
            }
            true
        }
        KeyCode::Home => {
            if let Some(session) = app.active_session_mut() {
                session.editor.home();
            }
            true
        }
        KeyCode::End => {
            if let Some(session) = app.active_session_mut() {
                session.editor.end();
            }
            true
        }
        KeyCode::Char('u') if ctrl => {
            if let Some(session) = app.active_session_mut() {
                session.editor.clear();
            }
            true
        }
        KeyCode::Char('w') if ctrl => {
            if let Some(session) = app.active_session_mut() {
                session.editor.word_left();
                session.editor.delete();
            }
            true
        }
        KeyCode::Char(ch) if !ctrl && !key.modifiers.contains(KeyModifiers::ALT) => {
            if let Some(session) = app.active_session_mut() {
                session.editor.insert_char(ch);
            }
            true
        }
        _ => false,
    }
}

fn handle_conversation(app: &mut App, key: KeyEvent) -> bool {
    match key.code {
        KeyCode::Up => {
            if let Some(session) = app.active_session_mut() {
                if !session.activity.is_empty() {
                    let last = session.activity.len() - 1;
                    let cur = session.selected_activity.unwrap_or(last);
                    session.selected_activity = Some(cur.saturating_sub(1));
                } else {
                    app.scroll_up(1);
                }
            }
            true
        }
        KeyCode::Down => {
            if let Some(session) = app.active_session_mut() {
                if !session.activity.is_empty() {
                    let last = session.activity.len() - 1;
                    let cur = session.selected_activity.unwrap_or(0);
                    session.selected_activity = Some((cur + 1).min(last));
                } else {
                    app.scroll_down(1);
                }
            }
            true
        }
        KeyCode::PageUp => {
            app.scroll_up(10);
            true
        }
        KeyCode::PageDown => {
            app.scroll_down(10);
            true
        }
        KeyCode::End => {
            app.scroll_to_bottom();
            true
        }
        KeyCode::Enter | KeyCode::Char(' ') | KeyCode::Char('e') => {
            if let Some(session) = app.active_session_mut() {
                if !session.activity.is_empty() {
                    let idx = session
                        .selected_activity
                        .unwrap_or(session.activity.len() - 1)
                        .min(session.activity.len() - 1);
                    session.selected_activity = Some(idx);
                    if let Some(activity) = session.activity.get_mut(idx) {
                        activity.expanded = !activity.expanded;
                        if activity.expanded {
                            session.expanded_activities.insert(idx);
                        } else {
                            session.expanded_activities.remove(&idx);
                        }
                    }
                }
            }
            true
        }
        KeyCode::Esc => {
            app.focus = Focus::Input;
            true
        }
        KeyCode::Tab => {
            app.focus = Focus::Bots;
            true
        }
        _ => false,
    }
}

fn handle_bots(app: &mut App, key: KeyEvent) -> bool {
    if app.agents.is_empty() {
        return false;
    }
    let last = app.agents.len() - 1;
    match key.code {
        KeyCode::Up => {
            app.agent_selection = app.agent_selection.saturating_sub(1);
            true
        }
        KeyCode::Down => {
            app.agent_selection = (app.agent_selection + 1).min(last);
            true
        }
        KeyCode::Enter => {
            if let Some(agent) = app.agents.get(app.agent_selection) {
                app.selected_agent = Some(agent.profile.id.clone());
            }
            app.focus = Focus::Input;
            true
        }
        KeyCode::Esc | KeyCode::Tab => {
            app.focus = Focus::Input;
            true
        }
        KeyCode::Char('p') => {
            if let Some(agent) = app.agents.get(app.agent_selection) {
                app.commands.push(UiCommand::AgentControl {
                    agent: agent.profile.id.clone(),
                    action: AgentAction::Pause,
                });
            }
            true
        }
        KeyCode::Char('s') => {
            if let Some(agent) = app.agents.get(app.agent_selection) {
                app.commands.push(UiCommand::AgentControl {
                    agent: agent.profile.id.clone(),
                    action: AgentAction::Stop,
                });
            }
            true
        }
        KeyCode::Char('r') => {
            if let Some(agent) = app.agents.get(app.agent_selection) {
                app.commands.push(UiCommand::AgentControl {
                    agent: agent.profile.id.clone(),
                    action: AgentAction::Resume,
                });
            }
            true
        }
        _ => false,
    }
}

fn handle_suggestion_key(app: &mut App, key: KeyEvent) -> bool {
    let Some(popup) = app.suggestions.as_ref() else {
        return false;
    };
    let len = popup.items.len();
    match key.code {
        KeyCode::Up => {
            if let Some(p) = app.suggestions.as_mut() {
                p.selected = p.selected.saturating_sub(1);
            }
            true
        }
        KeyCode::Down => {
            if let Some(p) = app.suggestions.as_mut() {
                p.selected = (p.selected + 1).min(len.saturating_sub(1));
            }
            true
        }
        KeyCode::Tab => {
            accept_suggestion(app);
            true
        }
        KeyCode::Enter => {
            // Enter always sends/runs the message. Suggestions are completed with Tab, so a
            // half-typed `!cmd`, `@file`, or `/cmd` is never silently replaced by a suggestion.
            app.suggestions = None;
            false
        }
        KeyCode::Esc => {
            app.suggestions = None;
            true
        }
        _ => false,
    }
}

/// The token being completed: its prefix, the query after it, and the char span it occupies.
struct Token {
    kind: PrefixKind,
    query: String,
    start: usize,
    end: usize,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum PrefixKind {
    Slash,
    Shell,
    At,
}

fn current_token(text: &str, cursor: usize) -> Option<Token> {
    if text.contains('\n') {
        return None;
    }
    let chars: Vec<char> = text.chars().collect();
    let cursor = cursor.min(chars.len());
    if chars.first() == Some(&'/') {
        let end = chars
            .iter()
            .position(|c| c.is_whitespace())
            .unwrap_or(cursor);
        if cursor > end {
            return None;
        }
        return Some(Token {
            kind: PrefixKind::Slash,
            query: chars[1..end].iter().collect(),
            start: 0,
            end,
        });
    }
    if chars.first() == Some(&'!') {
        let end = chars
            .iter()
            .position(|c| c.is_whitespace())
            .unwrap_or(cursor);
        if cursor > end {
            return None;
        }
        return Some(Token {
            kind: PrefixKind::Shell,
            query: chars[1..end].iter().collect(),
            start: 0,
            end,
        });
    }
    // @token: the last '@' before the cursor with no whitespace after it.
    let mut at = None;
    for i in (0..cursor).rev() {
        let c = chars[i];
        if c.is_whitespace() {
            break;
        }
        if c == '@' {
            at = Some(i);
            break;
        }
    }
    let at = at?;
    Some(Token {
        kind: PrefixKind::At,
        query: chars[at + 1..cursor].iter().collect(),
        start: at,
        end: cursor,
    })
}

fn refresh_suggestions(app: &mut App) {
    let Some(session) = app.active_session() else {
        app.suggestions = None;
        return;
    };
    let text = session.editor.text();
    let (_, col) = session.editor.cursor();
    let Some(token) = current_token(&text, col) else {
        app.suggestions = None;
        return;
    };
    let query = token.query.to_lowercase();
    let mut items: Vec<Suggestion> = Vec::new();
    match token.kind {
        PrefixKind::Slash => {
            for command in crate::commands::COMMANDS {
                if query.is_empty() || command.name.contains(&query) {
                    items.push(Suggestion {
                        text: format!("/{}", command.name),
                        description: command.description.to_owned(),
                        category: "command".to_owned(),
                    });
                }
            }
        }
        PrefixKind::Shell => {
            // Recent `!` commands from this session's input history first, then common commands.
            for entry in session.editor.history() {
                if let Some(cmd) = entry.trim().strip_prefix('!') {
                    let cmd = cmd.trim();
                    if !cmd.is_empty()
                        && (query.is_empty() || cmd.to_lowercase().contains(&query))
                        && !items.iter().any(|i| i.text == cmd)
                    {
                        items.push(Suggestion {
                            text: cmd.to_owned(),
                            description: "recent".to_owned(),
                            category: "shell".to_owned(),
                        });
                    }
                }
            }
            for cmd in crate::commands::SHELL_COMMANDS {
                if (query.is_empty() || cmd.to_lowercase().contains(&query))
                    && !items.iter().any(|i| i.text == *cmd)
                {
                    items.push(Suggestion {
                        text: (*cmd).to_owned(),
                        description: "shell".to_owned(),
                        category: "shell".to_owned(),
                    });
                }
            }
        }
        PrefixKind::At => {
            // References: bots, then workspace files.
            for agent in &app.agents {
                if query.is_empty() || agent.profile.name.to_lowercase().contains(&query) {
                    items.push(Suggestion {
                        text: agent.profile.name.clone(),
                        description: format!("bot · {}", agent.profile.role),
                        category: "bot".to_owned(),
                    });
                }
            }
            for name in [
                "main", "history", "context", "skills", "team", "mcp", "terminal",
            ] {
                if query.is_empty() || name.contains(&query) {
                    items.push(Suggestion {
                        text: name.to_owned(),
                        description: "reference".to_owned(),
                        category: "reference".to_owned(),
                    });
                }
            }
            let files = crate::commands::workspace_files(&session.cwd, 300);
            let mut scored: Vec<(u8, String)> = Vec::new();
            for file in files {
                let lower = file.to_lowercase();
                let name = lower.rsplit('/').next().unwrap_or(&lower);
                let score = if query.is_empty() {
                    2
                } else if name.starts_with(&query) {
                    0
                } else if lower.starts_with(&query) {
                    1
                } else if lower.contains(&query) {
                    2
                } else {
                    continue;
                };
                scored.push((score, file));
            }
            scored.sort_by(|a, b| a.0.cmp(&b.0).then_with(|| a.1.cmp(&b.1)));
            for (_, file) in scored {
                items.push(Suggestion {
                    text: file,
                    description: "file".to_owned(),
                    category: "file".to_owned(),
                });
                if items.len() >= 60 {
                    break;
                }
            }
        }
    }
    items.truncate(40);
    if items.is_empty() {
        app.suggestions = None;
    } else {
        let selected = app
            .suggestions
            .as_ref()
            .map(|p| p.selected.min(items.len().saturating_sub(1)))
            .unwrap_or(0);
        app.suggestions = Some(SuggestionPopup { items, selected });
    }
}

fn accept_suggestion(app: &mut App) {
    let Some(popup) = app.suggestions.take() else {
        return;
    };
    let Some(item) = popup.items.get(popup.selected) else {
        return;
    };
    let item = item.clone();
    let Some(session) = app.active_session_mut() else {
        return;
    };
    let text = session.editor.text();
    let (_, col) = session.editor.cursor();
    let Some(token) = current_token(&text, col) else {
        session.editor.set_text(&item.text);
        return;
    };
    let chars: Vec<char> = text.chars().collect();
    match token.kind {
        PrefixKind::Slash => {
            session.editor.set_text(&format!("{} ", item.text));
        }
        PrefixKind::Shell => {
            session.editor.set_text(&format!("!{}", item.text));
        }
        PrefixKind::At => {
            let mut out: String = chars[..token.start + 1].iter().collect();
            out.push_str(&item.text);
            out.extend(chars[token.end..].iter());
            session.editor.set_text(&out);
        }
    }
}

/// Walk the workspace for `@` file completion. Skips heavy/vendored directories and caps the walk.
/// Resolve @path references in text by reading file content and appending as context.
fn resolve_references(text: &str, cwd: std::path::PathBuf) -> String {
    let mut result = text.to_owned();
    // Find @path tokens
    let mut replacements = Vec::new();
    for token in text.split_whitespace() {
        if let Some(path) = token.strip_prefix('@') {
            let path = path.trim_matches(|c: char| c == ',' || c == '.' || c == ':');
            if path.is_empty() {
                continue;
            }
            let full = cwd.join(path);
            if let Ok(content) = std::fs::read_to_string(&full) {
                let snippet = if content.len() > 2000 {
                    format!("{}... (truncated)", &content[..2000])
                } else {
                    content
                };
                replacements.push((
                    format!("@{}", path),
                    format!(
                        "@{} (content below)
```
{}
```",
                        path, snippet
                    ),
                ));
            }
        }
    }
    for (from, to) in replacements {
        result = result.replace(&from, &to);
    }
    result
}

fn submit(app: &mut App, queue: bool) {
    let Some(session) = app.active_session_mut() else {
        return;
    };
    let text = session.editor.text();
    if text.trim().is_empty() {
        return;
    }
    let trimmed = text.trim().to_owned();
    let busy = session.is_busy();
    let id = session.id.clone();
    session.editor.remember(&text);
    session.editor.clear();

    // Intercept /, @, ! prefixes. A bare prefix opens the panel for what it introduces; anything
    // after it is an instruction and goes to the host, which turns a command into its chooser
    // rather than answering with text.
    if let Some(rest) = trimmed.strip_prefix('/') {
        if rest.trim().is_empty() {
            open_palette(app);
            return;
        }
        let mut parts = rest.splitn(2, char::is_whitespace);
        let cmd = parts.next().unwrap_or("").to_owned();
        let args = parts.next().unwrap_or("").trim().to_owned();
        app.commands.push(UiCommand::SlashCommand {
            session: id,
            command: cmd,
            args,
        });
        return;
    }
    if trimmed == "@" {
        open_file_picker(app);
        return;
    }
    if let Some(cmd) = trimmed.strip_prefix('!') {
        if cmd.trim().is_empty() {
            open_shell_picker(app);
            return;
        }
        app.commands.push(UiCommand::RunShell {
            session: id,
            command: cmd.trim().to_owned(),
        });
        return;
    }

    // Resolve @file references: read file content and append as context.
    let resolved = resolve_references(&trimmed, session.cwd.clone());

    if queue || busy {
        app.commands.push(UiCommand::Queue {
            session: id,
            text: resolved,
        });
    } else {
        app.commands.push(UiCommand::Submit {
            session: id,
            text: resolved,
        });
    }
}

/// The palette: every command, in the table's order, with what it does. Choosing a row is the same
/// instruction as typing `/name`, so there is only one thing to learn.
pub fn open_palette(app: &mut App) {
    let items: Vec<PickerItem> = commands::COMMANDS
        .iter()
        .map(|command| {
            PickerItem::new(
                format!("/{}", command.name),
                command.description,
                command.name,
            )
        })
        .collect();
    app.open_picker(Picker::new(PickerKind::Command, "Commands", items));
}

/// Workspace files as references. Choosing one inserts `@path` — what the operator would have
/// typed — and the file is not read until the message is sent.
pub fn open_file_picker(app: &mut App) {
    let Some(cwd) = app.active_session().map(|session| session.cwd.clone()) else {
        return;
    };
    let paths = commands::workspace_files(&cwd, 400);
    let empty = paths.is_empty();
    let items: Vec<PickerItem> = paths
        .into_iter()
        .map(|path| PickerItem::new(path.clone(), String::new(), path))
        .collect();
    let picker = Picker::new(PickerKind::File, "Reference a file", items);
    app.open_picker(if empty {
        picker.with_note("no files under this workspace")
    } else {
        picker
    });
}

/// Shell commands offered after `!`: this session's recent ones first, then the common list. The
/// `!` line still runs anything typed, so this is a shortcut, not a ceiling.
pub fn open_shell_picker(app: &mut App) {
    let mut items: Vec<PickerItem> = Vec::new();
    if let Some(session) = app.active_session() {
        for entry in session.editor.history() {
            if let Some(command) = entry.trim().strip_prefix('!') {
                let command = command.trim();
                if !command.is_empty() && !items.iter().any(|item| item.value == command) {
                    items.push(PickerItem::new(command, "recent", command));
                }
            }
        }
    }
    for command in commands::SHELL_COMMANDS {
        if !items.iter().any(|item| item.value == *command) {
            items.push(PickerItem::new(*command, "shell", *command));
        }
    }
    app.open_picker(Picker::new(PickerKind::Shell, "Run a shell command", items));
}

/// The picker's keys. Filter, move, choose; an empty result cannot be chosen, and Esc always
/// leaves the input exactly as it was.
fn handle_picker_overlay(app: &mut App, mut picker: Picker, key: KeyEvent) -> bool {
    let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
    match key.code {
        KeyCode::Esc => {
            app.overlay = None;
            true
        }
        KeyCode::Enter => {
            commit_verb(app, &picker, picker.kind.primary_verb());
            true
        }
        KeyCode::Up => {
            picker.move_by(-1);
            app.overlay = Some(Overlay::Picker(picker));
            true
        }
        KeyCode::Down => {
            picker.move_by(1);
            app.overlay = Some(Overlay::Picker(picker));
            true
        }
        KeyCode::PageUp => {
            picker.move_by(-5);
            app.overlay = Some(Overlay::Picker(picker));
            true
        }
        KeyCode::PageDown => {
            picker.move_by(5);
            app.overlay = Some(Overlay::Picker(picker));
            true
        }
        KeyCode::Char('d') if ctrl => {
            picker.move_by(5);
            app.overlay = Some(Overlay::Picker(picker));
            true
        }
        KeyCode::Char('u') if ctrl => {
            picker.move_by(-5);
            app.overlay = Some(Overlay::Picker(picker));
            true
        }
        KeyCode::Home => {
            picker.selected = 0;
            app.overlay = Some(Overlay::Picker(picker));
            true
        }
        KeyCode::End => {
            picker.selected = picker.visible().len().saturating_sub(1);
            app.overlay = Some(Overlay::Picker(picker));
            true
        }
        KeyCode::Backspace => {
            picker.backspace();
            app.overlay = Some(Overlay::Picker(picker));
            true
        }
        KeyCode::Char(c) if ctrl => {
            match picker.kind.verb_for_key(c) {
                Some(verb) => {
                    commit_verb(app, &picker, verb);
                }
                // A Ctrl key this panel does not declare belongs to nobody: swallowing it would
                // hide a typo, so the panel is simply re-drawn.
                None => app.overlay = Some(Overlay::Picker(picker)),
            }
            true
        }
        KeyCode::Char(c) => {
            picker.type_str(&c.to_string());
            app.overlay = Some(Overlay::Picker(picker));
            true
        }
        _ => {
            app.overlay = Some(Overlay::Picker(picker));
            true
        }
    }
}

/// Hand the row under the cursor to the host with the verb that was pressed. A verb on an empty
/// list does nothing: there is no row, so there is nothing to act on.
fn commit_verb(app: &mut App, picker: &Picker, verb: crate::widgets::Verb) {
    let Some(value) = picker.selected_item().map(|item| item.value.clone()) else {
        return;
    };
    let Some(session) = app.active_session() else {
        return;
    };
    app.commands.push(UiCommand::Act {
        kind: picker.kind,
        verb,
        value,
        session: session.id.clone(),
    });
    app.overlay = None;
}

fn handle_overlay(app: &mut App, key: KeyEvent) -> bool {
    let Some(overlay) = app.overlay.clone() else {
        return false;
    };
    match overlay {
        Overlay::Help | Overlay::Inspector(_) => match key.code {
            KeyCode::Esc | KeyCode::Enter | KeyCode::Char('q') | KeyCode::Char('?') => {
                app.overlay = None;
                true
            }
            _ => true,
        },
        Overlay::Approvals => {
            if app.approvals.is_empty() {
                app.overlay = None;
                return true;
            }
            let last = app.approvals.len() - 1;
            match key.code {
                KeyCode::Esc => {
                    app.overlay = None;
                    true
                }
                KeyCode::Up => {
                    app.approval_selection = app.approval_selection.saturating_sub(1);
                    true
                }
                KeyCode::Down => {
                    app.approval_selection = (app.approval_selection + 1).min(last);
                    true
                }
                KeyCode::Enter | KeyCode::Char('a') => {
                    answer_approval(app, true, false);
                    true
                }
                KeyCode::Char('p') => {
                    answer_approval(app, true, true);
                    true
                }
                KeyCode::Char('d') => {
                    answer_approval(app, false, false);
                    true
                }
                _ => true,
            }
        }
        Overlay::AddAgent { selected } => {
            let last = app.agents.len().saturating_sub(1);
            match key.code {
                KeyCode::Esc => {
                    app.overlay = None;
                    true
                }
                KeyCode::Up => {
                    app.overlay = Some(Overlay::AddAgent {
                        selected: selected.saturating_sub(1),
                    });
                    true
                }
                KeyCode::Down => {
                    app.overlay = Some(Overlay::AddAgent {
                        selected: (selected + 1).min(last),
                    });
                    true
                }
                KeyCode::Enter => {
                    spawn_agent(app, selected);
                    true
                }
                _ => true,
            }
        }
        Overlay::Autocomplete {
            candidates,
            selected,
        } => handle_autocomplete_overlay(app, candidates, selected, key),
        Overlay::Picker(picker) => handle_picker_overlay(app, picker, key),
        Overlay::Info { .. } => match key.code {
            KeyCode::Esc | KeyCode::Enter | KeyCode::Char('q') | KeyCode::Char('?') => {
                app.overlay = None;
                true
            }
            _ => true,
        },
        Overlay::Prompt {
            title,
            hint,
            mut editor,
            action,
        } => {
            let session_id = app.active_session().map(|view| view.id.clone());
            match key.code {
                KeyCode::Esc => {
                    app.overlay = None;
                    true
                }
                KeyCode::Enter => {
                    let text = editor.text().trim().to_owned();
                    if text.is_empty() {
                        // An empty line commits nothing: the panel stays up with its hint.
                        app.overlay = Some(Overlay::Prompt {
                            title,
                            hint: "type something first".to_owned(),
                            editor,
                            action,
                        });
                        return true;
                    }
                    if let Some(session) = session_id {
                        let command = match action {
                            PromptAction::SetModel => UiCommand::SlashCommand {
                                session,
                                command: "model".to_owned(),
                                args: text,
                            },
                            PromptAction::AddAllowRule => UiCommand::SlashCommand {
                                session,
                                command: "allow".to_owned(),
                                args: format!("add {text}"),
                            },
                        };
                        app.commands.push(command);
                    }
                    app.overlay = None;
                    true
                }
                KeyCode::Backspace => {
                    editor.backspace();
                    app.overlay = Some(Overlay::Prompt {
                        title,
                        hint,
                        editor,
                        action,
                    });
                    true
                }
                KeyCode::Left => {
                    editor.move_left();
                    app.overlay = Some(Overlay::Prompt {
                        title,
                        hint,
                        editor,
                        action,
                    });
                    true
                }
                KeyCode::Right => {
                    editor.move_right();
                    app.overlay = Some(Overlay::Prompt {
                        title,
                        hint,
                        editor,
                        action,
                    });
                    true
                }
                KeyCode::Char(c) if !key.modifiers.contains(KeyModifiers::CONTROL) => {
                    editor.insert_str(&c.to_string());
                    app.overlay = Some(Overlay::Prompt {
                        title,
                        hint,
                        editor,
                        action,
                    });
                    true
                }
                _ => {
                    app.overlay = Some(Overlay::Prompt {
                        title,
                        hint,
                        editor,
                        action,
                    });
                    true
                }
            }
        }
        Overlay::Confirm { yes, .. } => match key.code {
            KeyCode::Enter | KeyCode::Char('y') => {
                app.commands.push(*yes);
                app.overlay = None;
                true
            }
            KeyCode::Esc | KeyCode::Char('n') | KeyCode::Char('q') => {
                app.overlay = None;
                true
            }
            _ => true,
        },
        Overlay::Skills => match key.code {
            KeyCode::Esc | KeyCode::Char('q') => {
                app.overlay = None;
                true
            }
            _ => true,
        },
        Overlay::RenameSession { mut editor } => match key.code {
            KeyCode::Esc => {
                app.overlay = None;
                true
            }
            KeyCode::Enter => {
                let title = editor.text().trim().to_owned();
                if !title.is_empty() {
                    if let Some(session) = app.active_session() {
                        app.commands.push(UiCommand::RenameSession {
                            session: session.id.clone(),
                            title,
                        });
                    }
                }
                app.overlay = None;
                true
            }
            KeyCode::Backspace => {
                editor.backspace();
                app.overlay = Some(Overlay::RenameSession { editor });
                true
            }
            KeyCode::Left => {
                editor.move_left();
                app.overlay = Some(Overlay::RenameSession { editor });
                true
            }
            KeyCode::Right => {
                editor.move_right();
                app.overlay = Some(Overlay::RenameSession { editor });
                true
            }
            KeyCode::Home => {
                editor.home();
                app.overlay = Some(Overlay::RenameSession { editor });
                true
            }
            KeyCode::End => {
                editor.end();
                app.overlay = Some(Overlay::RenameSession { editor });
                true
            }
            KeyCode::Char(ch) if !key.modifiers.contains(KeyModifiers::CONTROL) => {
                editor.insert_char(ch);
                app.overlay = Some(Overlay::RenameSession { editor });
                true
            }
            _ => true,
        },
    }
}

fn answer_approval(app: &mut App, allow: bool, persist: bool) {
    let index = app
        .approval_selection
        .min(app.approvals.len().saturating_sub(1));
    if let Some(prompt) = app.approvals.get(index) {
        app.commands.push(UiCommand::AnswerApproval {
            approval: prompt.approval.clone(),
            allow,
            persist,
        });
    }
    app.overlay = None;
}

fn spawn_agent(app: &mut App, selected: usize) {
    let Some(agent) = app.agents.get(selected) else {
        app.overlay = None;
        return;
    };
    let profile: AgentId = agent.profile.id.clone();
    let Some(session) = app.active_session() else {
        app.overlay = None;
        return;
    };
    let session_id = session.id.clone();
    app.commands.push(UiCommand::SpawnAgent {
        profile,
        task: "Awaiting assignment".to_owned(),
        session: session_id,
    });
    app.overlay = None;
}

fn handle_autocomplete_overlay(
    app: &mut App,
    candidates: Vec<String>,
    selected: usize,
    key: KeyEvent,
) -> bool {
    let last = candidates.len().saturating_sub(1);
    match key.code {
        KeyCode::Esc => {
            app.overlay = None;
            true
        }
        KeyCode::Up => {
            app.overlay = Some(Overlay::Autocomplete {
                candidates: candidates.clone(),
                selected: selected.saturating_sub(1),
            });
            true
        }
        KeyCode::Down => {
            app.overlay = Some(Overlay::Autocomplete {
                candidates: candidates.clone(),
                selected: (selected + 1).min(last),
            });
            true
        }
        KeyCode::Enter | KeyCode::Tab => {
            if let Some(chosen) = candidates.get(selected).cloned() {
                if let Some(session) = app.active_session_mut() {
                    session.editor.clear();
                    session.editor.insert_str(&chosen);
                }
            }
            app.overlay = None;
            true
        }
        _ => true,
    }
}

pub fn handle_paste(app: &mut App, text: &str) -> bool {
    if let Some(pending) = app.pending_question.as_mut() {
        pending.input.push_str(text);
        return true;
    }
    if let Some(Overlay::RenameSession { editor }) = &mut app.overlay {
        editor.insert_str(text);
        return true;
    }
    if let Some(session) = app.active_session_mut() {
        session.editor.insert_str(text);
        return true;
    }
    false
}

/// Open the inspectors that the status bar advertises.
pub fn open_inspector(app: &mut App, kind: InspectorKind) {
    app.overlay = Some(Overlay::Inspector(kind));
}

#[cfg(test)]
mod tests {
    use super::*;
    use crossterm::event::KeyEvent;

    fn key(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::NONE)
    }

    fn ctrl(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::CONTROL)
    }

    fn app_with_session() -> App {
        use crate::app::SessionView;
        use vikram_runtime::types::SessionSnapshot;
        let mut app = App::new(false);
        app.sessions
            .push(SessionView::from_snapshot(SessionSnapshot {
                id: vikram_contracts::SessionId::generate(),
                title: "Session 1".into(),
                cwd: "/work".into(),
                model: None,
                messages: Vec::new(),
                turn: vikram_runtime::types::TurnState::Idle,
                tokens: Default::default(),
                created_ms: 0,
                updated_ms: 0,
                agent: None,
                queued: 0,
            }));
        app
    }

    #[test]
    fn typing_edits_the_draft_and_enter_emits_one_submit_command() {
        let mut app = app_with_session();
        for ch in "hello".chars() {
            assert!(handle_key(&mut app, key(KeyCode::Char(ch))));
        }
        assert_eq!(app.active_session().unwrap().editor.text(), "hello");
        handle_key(&mut app, key(KeyCode::Enter));
        let commands = app.take_commands();
        assert_eq!(commands.len(), 1);
        assert!(matches!(commands[0], UiCommand::Submit { .. }));
        assert!(app.active_session().unwrap().editor.is_empty());
    }

    #[test]
    fn shift_enter_inserts_a_newline_instead_of_submitting() {
        let mut app = app_with_session();
        handle_key(&mut app, key(KeyCode::Char('a')));
        let shift_enter = KeyEvent::new(KeyCode::Enter, KeyModifiers::SHIFT);
        handle_key(&mut app, shift_enter);
        assert_eq!(app.active_session().unwrap().editor.text(), "a\n");
        assert!(app.take_commands().is_empty());
    }

    #[test]
    fn tab_with_text_queues_and_tab_without_text_moves_focus() {
        let mut app = app_with_session();
        handle_key(&mut app, key(KeyCode::Tab));
        assert_eq!(app.focus, Focus::Conversation);

        app.focus = Focus::Input;
        handle_key(&mut app, key(KeyCode::Char('x')));
        handle_key(&mut app, key(KeyCode::Tab));
        let commands = app.take_commands();
        assert!(matches!(commands[0], UiCommand::Queue { .. }));
    }

    #[test]
    fn ctrl_c_quits_when_idle_and_interrupts_when_busy() {
        let mut app = app_with_session();
        handle_key(&mut app, ctrl(KeyCode::Char('c')));
        assert!(app.should_quit);

        let mut app = app_with_session();
        app.sessions[0].turn = vikram_runtime::types::TurnState::Streaming;
        handle_key(&mut app, ctrl(KeyCode::Char('c')));
        assert!(!app.should_quit);
        assert!(matches!(app.take_commands()[0], UiCommand::Cancel { .. }));
    }

    #[test]
    fn question_mark_opens_help_only_from_an_empty_draft() {
        let mut app = app_with_session();
        handle_key(&mut app, key(KeyCode::Char('?')));
        assert!(matches!(app.overlay, Some(Overlay::Help)));
    }

    #[test]
    fn ctrl_b_toggles_the_bots_panel() {
        let mut app = app_with_session();
        assert!(app.show_bots);
        handle_key(&mut app, ctrl(KeyCode::Char('b')));
        assert!(!app.show_bots);
    }

    #[test]
    fn paste_into_a_draft_preserves_newlines() {
        let mut app = app_with_session();
        handle_paste(&mut app, "a\nb");
        assert_eq!(app.active_session().unwrap().editor.text(), "a\nb");
    }

    fn app_with_question() -> App {
        let mut app = app_with_session();
        app.pending_question = Some(crate::app::PendingQuestion {
            request_id: "ask-1-0".into(),
            question: "Which file?".into(),
            choices: vec!["a".into(), "b".into()],
            input: String::new(),
        });
        app
    }

    #[test]
    fn a_pending_question_owns_the_keyboard_until_it_is_answered() {
        let mut app = app_with_question();
        handle_key(&mut app, key(KeyCode::Char('a')));
        handle_key(&mut app, key(KeyCode::Char('b')));
        handle_key(&mut app, key(KeyCode::Backspace));
        // Nothing leaks into the draft and no submit command is emitted while answering.
        assert_eq!(app.active_session().unwrap().editor.text(), "");
        assert!(app.take_commands().is_empty());

        handle_key(&mut app, key(KeyCode::Enter));
        assert!(app.pending_question.is_none());
        assert_eq!(
            app.take_answers(),
            vec![("ask-1-0".to_owned(), "a".to_owned())]
        );
    }

    #[test]
    fn escape_drops_a_pending_question_without_answering_it() {
        let mut app = app_with_question();
        handle_key(&mut app, key(KeyCode::Esc));
        assert!(app.pending_question.is_none());
        assert!(app.take_answers().is_empty());
        assert!(!app.should_quit);
    }

    #[test]
    fn an_empty_answer_keeps_the_prompt_up() {
        let mut app = app_with_question();
        handle_key(&mut app, key(KeyCode::Enter));
        assert!(app.pending_question.is_some());
        assert!(app.take_answers().is_empty());
    }

    #[test]
    fn paste_goes_to_the_question_prompt_when_one_is_pending() {
        let mut app = app_with_question();
        handle_paste(&mut app, "src/main.rs");
        assert_eq!(
            app.pending_question.as_ref().map(|q| q.input.as_str()),
            Some("src/main.rs")
        );
        assert!(app.active_session().unwrap().editor.is_empty());
    }

    #[test]
    fn current_token_recognises_slash_shell_and_at() {
        let t = current_token("/bo", 3).expect("slash token");
        assert_eq!(t.kind, PrefixKind::Slash);
        assert_eq!(t.query, "bo");
        let t = current_token("!git", 4).expect("shell token");
        assert_eq!(t.kind, PrefixKind::Shell);
        assert_eq!(t.query, "git");
        let t = current_token("Review @src/ma", 14).expect("at token");
        assert_eq!(t.kind, PrefixKind::At);
        assert_eq!(t.query, "src/ma");
        assert!(current_token("plain text", 10).is_none());
    }

    #[test]
    fn typing_slash_opens_the_command_popup_and_tab_accepts() {
        let mut app = app_with_session();
        handle_key(&mut app, key(KeyCode::Char('/')));
        handle_key(&mut app, key(KeyCode::Char('h')));
        handle_key(&mut app, key(KeyCode::Char('e')));
        let popup = app.suggestions.as_ref().expect("popup open");
        assert_eq!(popup.items[0].text, "/help");
        handle_key(&mut app, key(KeyCode::Tab));
        assert!(app.suggestions.is_none());
        assert_eq!(app.active_session().unwrap().editor.text(), "/help ");
    }

    #[test]
    fn typing_unknown_command_closes_the_popup() {
        let mut app = app_with_session();
        for ch in ['/', 'z', 'z', 'z'] {
            handle_key(&mut app, key(KeyCode::Char(ch)));
        }
        assert!(app.suggestions.is_none());
    }

    #[test]
    fn typing_bang_opens_shell_suggestions() {
        let mut app = app_with_session();
        for ch in ['!', 'g', 'i', 't'] {
            handle_key(&mut app, key(KeyCode::Char(ch)));
        }
        let popup = app.suggestions.as_ref().expect("shell popup");
        assert!(popup.items.iter().any(|i| i.text.starts_with("git")));
    }

    #[test]
    fn escape_dismisses_the_popup_without_losing_the_draft() {
        let mut app = app_with_session();
        handle_key(&mut app, key(KeyCode::Char('/')));
        assert!(app.suggestions.is_some());
        handle_key(&mut app, key(KeyCode::Esc));
        assert!(app.suggestions.is_none());
        assert_eq!(app.active_session().unwrap().editor.text(), "/");
    }

    fn picker_of(app: &App) -> &Picker {
        match app.overlay.as_ref() {
            Some(Overlay::Picker(picker)) => picker,
            other => panic!("expected a picker, got {other:?}"),
        }
    }

    #[test]
    fn a_lone_slash_opens_the_palette_naming_every_command_in_the_table() {
        let mut app = app_with_session();
        handle_key(&mut app, key(KeyCode::Char('/')));
        handle_key(&mut app, key(KeyCode::Enter));
        let picker = picker_of(&app);
        assert_eq!(picker.kind, PickerKind::Command);
        assert_eq!(picker.items.len(), commands::COMMANDS.len());
        assert!(
            picker.items.iter().any(|item| item.value == "model"),
            "the palette has to reach /model"
        );
        assert!(app.take_commands().is_empty(), "/ alone is not a message");
    }

    #[test]
    fn typing_in_the_palette_filters_and_enter_emits_the_choice_not_the_text() {
        let mut app = app_with_session();
        handle_key(&mut app, key(KeyCode::Char('/')));
        handle_key(&mut app, key(KeyCode::Enter));
        for ch in "mdl".chars() {
            handle_key(&mut app, key(KeyCode::Char(ch)));
        }
        assert_eq!(picker_of(&app).selected_item().unwrap().value, "model");
        handle_key(&mut app, key(KeyCode::Enter));
        assert!(app.overlay.is_none());
        let commands = app.take_commands();
        assert_eq!(commands.len(), 1);
        assert!(matches!(
            &commands[0],
            UiCommand::Act { kind: PickerKind::Command, value, .. } if value == "model"
        ));
    }

    #[test]
    fn escape_leaves_the_chooser_without_choosing_anything() {
        let mut app = app_with_session();
        handle_key(&mut app, key(KeyCode::Char('/')));
        handle_key(&mut app, key(KeyCode::Enter));
        handle_key(&mut app, key(KeyCode::Esc));
        assert!(app.overlay.is_none());
        assert!(app.take_commands().is_empty());
    }

    #[test]
    fn a_filter_that_matches_nothing_cannot_be_chosen() {
        let mut app = app_with_session();
        handle_key(&mut app, key(KeyCode::Char('/')));
        handle_key(&mut app, key(KeyCode::Enter));
        for ch in "zzz".chars() {
            handle_key(&mut app, key(KeyCode::Char(ch)));
        }
        handle_key(&mut app, key(KeyCode::Enter));
        assert!(app.take_commands().is_empty(), "no row, no instruction");
        assert!(
            matches!(app.overlay, Some(Overlay::Picker(_))),
            "the panel stays up"
        );
    }

    #[test]
    fn a_lone_at_opens_the_file_chooser_and_choosing_inserts_a_reference() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("main.rs"), "fn main() {}").unwrap();
        let mut app = app_with_session();
        app.sessions[0].cwd = dir.path().to_path_buf();
        handle_key(&mut app, key(KeyCode::Char('@')));
        handle_key(&mut app, key(KeyCode::Enter));
        let picker = picker_of(&app);
        assert_eq!(picker.kind, PickerKind::File);
        assert!(picker.items.iter().any(|item| item.value == "main.rs"));
        handle_key(&mut app, key(KeyCode::Enter));
        let command = app
            .take_commands()
            .pop()
            .expect("a file choice is a command");
        assert!(matches!(
            command,
            UiCommand::Act { kind: PickerKind::File, ref value, .. } if value == "main.rs"
        ));
    }

    #[test]
    fn a_lone_bang_opens_the_shell_chooser_with_the_common_commands() {
        let mut app = app_with_session();
        handle_key(&mut app, key(KeyCode::Char('!')));
        handle_key(&mut app, key(KeyCode::Enter));
        let picker = picker_of(&app);
        assert_eq!(picker.kind, PickerKind::Shell);
        assert!(picker.items.iter().any(|item| item.value == "git status"));
    }

    #[test]
    fn filtering_for_mode_does_not_put_model_on_the_cursor() {
        let mut app = app_with_session();
        handle_key(&mut app, key(KeyCode::Char('/')));
        handle_key(&mut app, key(KeyCode::Enter));
        for ch in "mode".chars() {
            handle_key(&mut app, key(KeyCode::Char(ch)));
        }
        assert_eq!(picker_of(&app).selected_item().unwrap().value, "mode");
        handle_key(&mut app, key(KeyCode::Enter));
        let command = app.take_commands().pop().expect("a choice");
        assert!(matches!(
            command,
            UiCommand::Act { kind: PickerKind::Command, ref value, .. } if value == "mode"
        ));
    }

    #[test]
    fn ctrl_c_leaves_a_panel_and_stops_instead_of_being_swallowed_by_it() {
        let mut app = app_with_session();
        handle_key(&mut app, key(KeyCode::Char('/')));
        handle_key(&mut app, key(KeyCode::Enter));
        assert!(matches!(app.overlay, Some(Overlay::Picker(_))));
        handle_key(&mut app, ctrl(KeyCode::Char('c')));
        assert!(app.overlay.is_none(), "a panel must not trap the stop key");
        assert!(app.should_quit);
        assert!(matches!(
            app.take_commands().pop(),
            Some(UiCommand::Quit)
        ));
    }

    fn panel(kind: PickerKind, rows: &[(&str, &str)]) -> Overlay {
        let items = rows
            .iter()
            .map(|(label, value)| PickerItem::new(*label, String::new(), *value))
            .collect();
        Overlay::Picker(Picker::new(kind, "test", items))
    }

    fn act_of(app: &mut App) -> Option<(PickerKind, crate::widgets::Verb, String)> {
        match app.take_commands().pop() {
            Some(UiCommand::Act { kind, verb, value, .. }) => Some((kind, verb, value)),
            other => {
                assert!(other.is_none(), "expected no command, got {other:?}");
                None
            }
        }
    }

    #[test]
    fn each_panel_answers_its_own_verbs_and_only_those() {
        use crate::widgets::Verb;
        let cases = [
            (PickerKind::Model, 't', Verb::Prompt),
            (PickerKind::Allow, 'a', Verb::Prompt),
            (PickerKind::Bot, 'n', Verb::New),
            (PickerKind::Session, 'r', Verb::Rename),
            (PickerKind::Session, 'x', Verb::Close),
            (PickerKind::Memory, 'd', Verb::Remove),
        ];
        for (kind, key, verb) in cases {
            let mut app = app_with_session();
            app.overlay = Some(panel(kind, &[("row", "value")]));
            handle_key(&mut app, ctrl(KeyCode::Char(key)));
            let got = act_of(&mut app).expect("a declared verb has to reach the host");
            assert_eq!(got.0, kind, "ctrl+{key} on {}", kind.as_str());
            assert_eq!(got.1, verb, "ctrl+{key} on {}", kind.as_str());
        }
    }

    #[test]
    fn a_letter_the_panel_does_not_declare_still_types_into_the_filter() {
        use crate::widgets::Verb;
        let mut app = app_with_session();
        app.overlay = Some(panel(PickerKind::Model, &[("gpt-5.2", "gpt-5.2")]));
        handle_key(&mut app, key(KeyCode::Char('x')));
        assert!(act_of(&mut app).is_none(), "`x` is not a model verb");
        let Overlay::Picker(picker) = app.overlay.as_ref().unwrap() else {
            panic!("panel closed");
        };
        assert_eq!(picker.query, "x");
        assert_eq!(PickerKind::Model.primary_verb(), Verb::Choose);
    }

    #[test]
    fn a_verb_with_no_row_to_act_on_does_nothing_at_all() {
        let mut app = app_with_session();
        app.overlay = Some(panel(PickerKind::Session, &[]));
        handle_key(&mut app, ctrl(KeyCode::Char('x')));
        assert!(act_of(&mut app).is_none(), "no row, no instruction");
        assert!(matches!(app.overlay, Some(Overlay::Picker(_))), "panel stays open");
    }

    #[test]
    fn the_wheel_and_page_keys_move_through_a_long_panel() {
        let rows: Vec<(String, String)> = (0..30)
            .map(|i| (format!("row-{i:02}"), format!("row-{i:02}")))
            .collect();
        let items = rows
            .iter()
            .map(|(label, value)| PickerItem::new(label.clone(), String::new(), value.clone()))
            .collect();
        let mut app = app_with_session();
        app.overlay = Some(Overlay::Picker(Picker::new(
            PickerKind::Command,
            "test",
            items,
        )));
        for _ in 0..5 {
            handle_key(&mut app, key(KeyCode::PageDown));
        }
        let Overlay::Picker(picker) = app.overlay.as_ref().unwrap() else {
            panic!("panel closed");
        };
        assert_eq!(picker.selected, 25, "five pages of five rows");
        handle_key(&mut app, key(KeyCode::Home));
        let Overlay::Picker(picker) = app.overlay.as_ref().unwrap() else {
            panic!("panel closed");
        };
        assert_eq!(picker.selected, 0);
        handle_key(&mut app, key(KeyCode::End));
        let Overlay::Picker(picker) = app.overlay.as_ref().unwrap() else {
            panic!("panel closed");
        };
        assert_eq!(picker.selected, 29, "End lands on the last row");
    }

    #[test]
    fn every_command_in_the_table_reaches_the_host_instead_of_being_rejected() {
        for command in commands::COMMANDS {
            let mut app = app_with_session();
            for ch in format!("/{}", command.name).chars() {
                handle_key(&mut app, key(KeyCode::Char(ch)));
            }
            handle_key(&mut app, key(KeyCode::Enter));
            let sent = app.take_commands();
            assert_eq!(sent.len(), 1, "/{} sent {sent:?}", command.name);
            assert!(
                matches!(&sent[0], UiCommand::SlashCommand { command: sent_name, .. } if sent_name == command.name),
                "/{} sent {sent:?}",
                command.name
            );
        }
    }

    #[test]
    fn tab_completes_only_to_a_command_the_dispatcher_accepts() {
        let mut app = app_with_session();
        for ch in "/mo".chars() {
            handle_key(&mut app, key(KeyCode::Char(ch)));
        }
        handle_key(&mut app, key(KeyCode::Tab));
        let text = app.active_session().unwrap().editor.text();
        let word = text.trim().trim_start_matches('/');
        assert!(
            commands::find(word).is_some(),
            "Tab produced `{text}`, which is not a command"
        );
    }

    #[test]
    fn the_completion_list_is_the_command_table_and_never_offers_a_ghost() {
        let candidates: Vec<String> = commands::starting_with("m")
            .into_iter()
            .map(|command| format!("/{}", command.name))
            .collect();        assert_eq!(
            candidates,
            vec![
                "/model".to_owned(),
                "/mode".to_owned(),
                "/memory".to_owned(),
                "/mcp".to_owned()
            ]
        );
        for candidate in &candidates {
            assert!(
                commands::find(candidate).is_some(),
                "{candidate} is offered"
            );
        }
    }
}
