//! Terminal lifecycle and the interactive event loop.
//!
//! Raw mode, the alternate screen and mouse capture are installed in one place and restored on
//! every exit path, including a panic. The loop never blocks the runtime: terminal input, runtime
//! observations and the UI tick are three independent sources.

use std::io::{self, IsTerminal, Stdout};
use std::time::Duration;

use crossterm::event::{
    DisableMouseCapture, EnableMouseCapture, Event, EventStream, MouseEventKind,
};
use crossterm::execute;
use crossterm::terminal::{
    EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode,
};
use futures_util::StreamExt;
use ratatui::Terminal;
use ratatui::backend::CrosstermBackend;
use ratatui::style::Color;
use tokio::sync::broadcast::error::RecvError;
use vikram_contracts::{BotId, PermissionMode, SessionId};
use vikram_runtime::config::Config;
use vikram_runtime::types::{NoticeLevel, TurnState};
use vikram_runtime::{BotControl, Runtime, RuntimeHandle};
use vikram_tui::app::{App, BotAction, Focus, Overlay, UiCommand};
use vikram_tui::commands::{self, Outcome};
use vikram_tui::widgets::{Picker, PickerItem, PickerKind, Verb};
use vikram_tui::{input, ui};

type Term = Terminal<CrosstermBackend<Stdout>>;

pub async fn run(config: Config) -> Result<(), String> {
    let ascii = config.ascii_fallback;
    let theme_bg = configured_colour(&config.theme.background, "theme.background");
    let theme_accent = configured_colour(&config.theme.accent, "theme.accent");
    let runtime = Runtime::start(config)
        .await
        .map_err(|err| format!("could not start runtime: {err}"))?;
    let handle = runtime.handle();

    let mut app = App::new(ascii);
    app.theme_bg = theme_bg;
    app.theme_accent = theme_accent;
    app.sync_snapshot(handle.snapshot());

    let mut terminal = setup_terminal()?;
    let outcome = event_loop(&mut terminal, &mut app, &handle).await;
    restore_terminal(&mut terminal);
    outcome
}

/// Read a `rrggbb` colour out of the config. A value that is not six hex digits is reported and
/// dropped, so a typo costs the operator the default colour and a sentence saying so, rather than
/// a window drawn in whatever the parser happened to make of it.
fn configured_colour(value: &str, field: &str) -> Option<Color> {
    let hex = value.trim().trim_start_matches('#');
    if hex.len() != 6 || !hex.chars().all(|c| c.is_ascii_hexdigit()) {
        eprintln!("config: {field} = {value:?} is not six hex digits (rrggbb); using the default");
        return None;
    }
    let byte = |at: usize| u8::from_str_radix(&hex[at..at + 2], 16).unwrap_or_default();
    Some(Color::Rgb(byte(0), byte(2), byte(4)))
}

fn setup_terminal() -> Result<Term, String> {
    install_panic_hook();
    if !io::stdin().is_terminal() {
        return Err("TUI requires a real terminal (not a pipe/redirect)".into());
    }
    enable_raw_mode().map_err(|err| format!("raw mode: {err}"))?;
    let mut stdout = io::stdout();
    execute!(stdout, EnterAlternateScreen, EnableMouseCapture)
        .map_err(|err| format!("alternate screen: {err}"))?;
    let backend = CrosstermBackend::new(stdout);
    Terminal::new(backend).map_err(|err| format!("terminal: {err}"))
}

fn install_panic_hook() {
    let original = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        let _ = restore_raw();
        original(info);
    }));
}

fn restore_raw() -> io::Result<()> {
    disable_raw_mode()?;
    let mut stdout = io::stdout();
    execute!(stdout, LeaveAlternateScreen, DisableMouseCapture)
}

fn restore_terminal(terminal: &mut Term) {
    let _ = disable_raw_mode();
    let _ = execute!(
        terminal.backend_mut(),
        LeaveAlternateScreen,
        DisableMouseCapture
    );
    let _ = terminal.show_cursor();
}

async fn event_loop(
    terminal: &mut Term,
    app: &mut App,
    handle: &RuntimeHandle,
) -> Result<(), String> {
    let mut runtime_events = handle.events();
    let mut term_events = EventStream::new();
    let mut ticker = tokio::time::interval(Duration::from_millis(120));
    ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);

    loop {
        let completed = terminal
            .draw(|frame| ui::draw(frame, app))
            .map_err(|err| format!("draw: {err}"))?;
        app.capture_screen(completed.buffer);

        for command in app.take_commands() {
            execute(handle, app, command);
        }
        for (request_id, answer) in app.take_answers() {
            if let Err(err) = handle.answer_question(&request_id, answer) {
                report(app, err);
            }
        }

        if app.should_quit {
            break;
        }

        tokio::select! {
            maybe_event = term_events.next() => {
                match maybe_event {
                    Some(Ok(event)) => apply_terminal_event(app, event),
                    Some(Err(err)) => return Err(format!("terminal input: {err}")),
                    None => break,
                }
            }
            received = runtime_events.recv() => {
                match received {
                    Ok(event) => app.on_event(event),
                    Err(RecvError::Lagged(_)) => {
                        app.sync_snapshot(handle.snapshot());
                        app.push_notice(
                            vikram_runtime::types::NoticeLevel::Warn,
                            "view fell behind the runtime; resynchronised",
                        );
                    }
                    Err(RecvError::Closed) => break,
                }
            }
            _ = ticker.tick() => {
                app.tick = app.tick.wrapping_add(1);
            }
        }

        // A copy is finished here rather than inside the key handler: the interface only says what
        // the text is, and the host is what knows how to reach the clipboard.
        if let Some(text) = app.take_clipboard() {
            copy_to_clipboard(&text);
        }
    }

    Ok(())
}

fn apply_terminal_event(app: &mut App, event: Event) {
    match event {
        Event::Key(key) => {
            if key.kind != crossterm::event::KeyEventKind::Release {
                input::handle_key(app, key);
            }
        }
        Event::Paste(text) => {
            input::handle_paste(app, &text);
        }
        Event::Mouse(mouse) => match mouse.kind {
            // The wheel scrolls whatever is under the cursor: an open panel moves its cursor,
            // otherwise the conversation scrolls. A panel that ignored the wheel would look frozen.
            MouseEventKind::ScrollUp => {
                if let Some(Overlay::Picker(picker)) = &mut app.overlay {
                    picker.move_by(-2);
                } else {
                    app.scroll_up(3);
                }
            }
            MouseEventKind::ScrollDown => {
                if let Some(Overlay::Picker(picker)) = &mut app.overlay {
                    picker.move_by(2);
                } else {
                    app.scroll_down(3);
                }
            }
            MouseEventKind::Down(crossterm::event::MouseButton::Left) => {
                let x = mouse.column;
                // A click inside a panel selects the row it landed on. The draw wrote those rows
                // down, so the click names a real row rather than a guess at the geometry.
                if let Some((left, right)) = app.picker_x
                    && x >= left
                    && x < right
                    && let Some(position) = app
                        .picker_rows
                        .iter()
                        .find(|(row, _)| *row == mouse.row)
                        .map(|(_, position)| *position)
                {
                    if let Some(Overlay::Picker(picker)) = &mut app.overlay {
                        picker.select_position(position);
                    }
                    return;
                }
                // The tab strip and `+ New` live on one row. Matching on the column alone would
                // swallow every press below them, so the row is part of the hit.
                if app.overlay.is_none() {
                    if let Some((row, start, end)) = app.new_session_x
                        && mouse.row == row
                        && x >= start
                        && x < end
                    {
                        app.commands.push(UiCommand::NewSession);
                        return;
                    }
                    // close buttons
                    for hit in app.tab_hits.clone() {
                        if mouse.row == hit.row && x >= hit.close_x_start && x < hit.close_x_end {
                            app.commands.push(UiCommand::CloseSession {
                                session: hit.session,
                            });
                            return;
                        }
                    }
                    // tab bodies
                    for hit in app.tab_hits.clone() {
                        if mouse.row == hit.row
                            && x >= hit.tab_x_start
                            && x < hit.tab_x_end
                            && !(x >= hit.close_x_start && x < hit.close_x_end)
                        {
                            app.commands.push(UiCommand::SwitchSession {
                                session: hit.session,
                            });
                            return;
                        }
                    }
                }
                // A header folds its own thinking block. This is checked before a selection starts
                // so a click on the label does not leave a one-cell drag behind. The fold belongs
                // to the session on screen, which an agent pane can show without making it active.
                if app.overlay.is_none()
                    && let Some((key, tool)) = app.fold_at(x, mouse.row)
                {
                    if let Some(id) = app.display_session().map(|session| session.id.clone())
                        && let Some(index) = app.session_index(&id)
                    {
                        let session = &mut app.sessions[index];
                        if tool {
                            session.toggle_tool(key);
                        } else {
                            session.toggle_thought(key);
                        }
                    }
                    return;
                }
                // Nothing else claimed the press, so it starts a drag. A press that never moves
                // selects nothing and copies nothing.
                app.selection = Some(vikram_tui::app::Selection::new((x, mouse.row)));
            }
            MouseEventKind::Drag(crossterm::event::MouseButton::Left) => {
                if let Some(selection) = &mut app.selection {
                    selection.cursor = (mouse.column, mouse.row);
                }
            }
            MouseEventKind::Up(crossterm::event::MouseButton::Left) => {
                // The highlight stays after the drag so the operator can see what was taken; a
                // plain click clears it.
                if let Some(selection) = app.selection {
                    if selection.is_click() {
                        app.selection = None;
                    } else {
                        app.copy_selection();
                    }
                }
            }
            _ => {}
        },
        Event::Resize(_, _) => {}
        _ => {}
    }
}

fn execute(handle: &RuntimeHandle, app: &mut App, command: UiCommand) {
    match command {
        UiCommand::Submit { session, text } => {
            if let Err(err) = handle.submit(&session, text) {
                report(app, err);
            } else if let Some(view) = app.session_by_id_mut(&session) {
                // The clock starts when the operator presses enter, not when the first token lands:
                // a turn that is thinking has been running for that whole time.
                view.turn = TurnState::Streaming;
                view.mark_working();
                view.follow = true;
            }
        }
        UiCommand::Queue { session, text } => {
            if let Err(err) = handle.queue(&session, text) {
                report(app, err);
            }
        }
        UiCommand::Steer { session, text } => {
            if let Err(err) = handle.steer(&session, text) {
                report(app, err);
            } else {
                app.push_notice(NoticeLevel::Info, "steered the running turn");
            }
        }
        UiCommand::EditPending {
            session,
            target,
            text,
        } => {
            let text = text.trim().to_owned();
            if text.is_empty() {
                app.push_notice(NoticeLevel::Warn, "empty edit — nothing changed");
            } else if let Err(err) = handle.pending_update(&session, &target, text) {
                report(app, err);
            }
        }
        UiCommand::CreateBot {
            name,
            role,
            description,
        } => {
            if name.trim().is_empty() {
                app.push_notice(NoticeLevel::Warn, "a bot needs a name");
            } else {
                match handle.bot_create(name.trim(), &role, &description) {
                    Ok(id) => {
                        app.push_notice(NoticeLevel::Info, format!("created bot `{name}` ({id})"))
                    }
                    Err(err) => report(app, err),
                }
            }
        }
        UiCommand::RenameBot { bot, name } => {
            if name.trim().is_empty() {
                app.push_notice(NoticeLevel::Warn, "a bot needs a name");
            } else {
                match handle.bot_rename(&bot, name.trim()) {
                    Ok(()) => {
                        app.push_notice(NoticeLevel::Info, format!("renamed bot to `{name}`"))
                    }
                    Err(err) => report(app, err),
                }
            }
        }
        UiCommand::DeleteBot { bot } => match handle.bot_delete(&bot) {
            Ok(()) => {
                app.push_notice(NoticeLevel::Info, "bot deleted");
                app.sync_snapshot(handle.snapshot());
            }
            Err(err) => report(app, err),
        },
        UiCommand::RenameSkill { skill, new_name } => {
            if new_name.trim().is_empty() {
                app.push_notice(NoticeLevel::Warn, "a skill needs a name");
            } else {
                match handle.skill_rename(&skill, new_name.trim()) {
                    Ok(()) => app.push_notice(
                        NoticeLevel::Info,
                        format!("renamed skill `{skill}` to `{new_name}`"),
                    ),
                    Err(err) => report(app, err),
                }
            }
        }
        UiCommand::InstallPlugin { path } => {
            let path = std::path::PathBuf::from(path.trim());
            if path.as_os_str().is_empty() {
                app.push_notice(NoticeLevel::Warn, "a plugin needs a path");
            } else {
                match handle.plugin_install(&path) {
                    Ok(id) => {
                        app.push_notice(NoticeLevel::Info, format!("installed plugin `{id}`"))
                    }
                    Err(err) => report(app, err),
                }
            }
        }
        UiCommand::UninstallPlugin { id } => match handle.plugin_uninstall(&id) {
            Ok(()) => app.push_notice(NoticeLevel::Info, format!("uninstalled plugin `{id}`")),
            Err(err) => report(app, err),
        },
        UiCommand::SaveMemory { key, value } => {
            if key.trim().is_empty() {
                app.push_notice(NoticeLevel::Warn, "a memory needs a key");
            } else {
                match handle.memory_save(key.trim(), value.trim(), "global") {
                    Ok(()) => app.push_notice(NoticeLevel::Info, format!("remembered `{key}`")),
                    Err(err) => report(app, err),
                }
            }
        }
        UiCommand::Cancel { session } => {
            if let Err(err) = handle.cancel(&session) {
                report(app, err);
            }
        }
        UiCommand::NewSession => match handle.create_session(None) {
            Ok(id) => {
                app.sync_snapshot(handle.snapshot());
                select(app, &id);
            }
            Err(err) => report(app, err),
        },
        UiCommand::SwitchSession { session } => {
            app.selected_bot = None;
            // A closed tab is a view that was closed, not a conversation that ended, so choosing it
            // from the catalogue loads it again instead of failing. Only when the store has no such
            // session is there nothing to select.
            let live = match app.session_index(&session) {
                Some(_) => true,
                None => match handle.open_session(&session) {
                    Ok(_) => {
                        app.sync_snapshot(handle.snapshot());
                        true
                    }
                    Err(err) => {
                        report(app, err);
                        false
                    }
                },
            };
            if live {
                select(app, &session);
            }
        }
        UiCommand::RenameSession { session, title } => {
            if let Err(err) = handle.rename_session(&session, title) {
                report(app, err);
            }
        }
        UiCommand::CloseSession { session } => {
            let was_active = app.session_index(&session) == Some(app.active);
            let title = app
                .sessions
                .iter()
                .find(|view| view.id == session)
                .map(|view| view.title.clone());
            if let Err(err) = handle.close_session(&session) {
                report(app, err);
            } else {
                // The tab is gone and the transcript is not: saying so once, here, is what keeps a
                // close from reading as a delete.
                let name = title.unwrap_or_else(|| "session".to_owned());
                app.push_notice(
                    NoticeLevel::Info,
                    format!("closed {name} — still in /sessions"),
                );
                if app.sessions.is_empty() {
                    match handle.create_session(Some("Session".into())) {
                        Ok(id) => select(app, &id),
                        Err(err) => report(app, err),
                    }
                } else if was_active && let Some(first) = app.sessions.first().map(|s| s.id.clone())
                {
                    select(app, &first);
                }
                app.sync_snapshot(handle.snapshot());
            }
        }
        UiCommand::RunShell { session, command } => {
            // The runtime gates it: same evaluator, same floor, same approval prompt and journal as
            // a model-issued tool call, with the operator's provenance instead of the model's.
            if let Err(err) = handle.run_user_command(&session, command) {
                report(app, err);
            }
        }
        UiCommand::SlashCommand {
            session,
            command,
            args,
        } => open_command(handle, app, &session, &command, &args),
        UiCommand::RefreshModels => handle.refresh_models(),
        UiCommand::SetEndpoint { base_url } => {
            // A custom endpoint keeps the key variable and dialect already configured: the operator
            // typed where the server lives, not which variable holds its token.
            let config = handle.config();
            match handle.set_provider(base_url, config.api_key_env, config.provider) {
                Ok(()) => {
                    app.models.clear();
                    app.models_error = None;
                    app.models_loading = true;
                    handle.refresh_models();
                }
                Err(err) => report(app, err),
            }
        }
        UiCommand::Unallow { index } => match handle.unallow(index) {
            Ok(rule) => app.push_notice(NoticeLevel::Info, format!("forgot {}", rule.describe())),
            Err(err) => report(app, err),
        },
        UiCommand::AllowRule { session, rule } => match handle.allow(&session, rule.trim()) {
            Ok(()) => app.push_notice(NoticeLevel::Info, format!("saved: {}", rule.trim())),
            Err(err) => report(app, err),
        },
        UiCommand::DeleteSkill { name } => match handle.skill_delete(&name) {
            Ok(()) => app.push_notice(
                NoticeLevel::Info,
                format!("deleted skill `{name}` (moved to trash)"),
            ),
            Err(err) => report(app, err),
        },
        UiCommand::CreateSkill { session: _, text } => {
            // parse "name: description"
            let (name, desc) = match text.split_once(':') {
                Some((n, d)) => (n.trim(), d.trim()),
                None => (text.trim(), ""),
            };
            if name.is_empty() {
                app.push_notice(NoticeLevel::Warn, "skill name is required");
                return;
            }
            let description = if desc.is_empty() { name } else { desc };
            match handle.skill_create(name, description, "") {
                Ok(()) => app.push_notice(NoticeLevel::Info, format!("created skill `{name}`")),
                Err(err) => report(app, err),
            }
        }
        UiCommand::ImportSkill { session: _, path } => match handle.skill_import(path.trim()) {
            Ok(()) => app.push_notice(
                NoticeLevel::Info,
                format!("imported skill from `{}`", path.trim()),
            ),
            Err(err) => report(app, err),
        },
        UiCommand::CreateHiveRoster { name } => match handle.hive_roster_create(name.trim()) {
            Ok(()) => app.push_notice(NoticeLevel::Info, format!("created hive `{}`", name.trim())),
            Err(err) => report(app, err),
        },
        UiCommand::Act {
            kind,
            verb,
            value,
            session,
        } => apply_act(handle, app, &session, kind, verb, value),
        UiCommand::Clear { session } => {
            if let Some(view) = app.session_by_id_mut(&session) {
                view.messages.clear();
                view.follow = true;
            }
        }
        UiCommand::ForgetMemory { scope, key } => match handle.memory_delete(&key, &scope) {
            Ok(true) => app.push_notice(NoticeLevel::Info, format!("forgot {scope}/{key}")),
            Ok(false) => {
                app.push_notice(NoticeLevel::Warn, format!("{scope}/{key} was already gone"))
            }
            Err(err) => report(app, err),
        },
        UiCommand::StopTask { id } => {
            handle.complete_task(&id, vikram_runtime::types::TaskState::Cancelled);
            app.push_notice(NoticeLevel::Info, format!("stopped task {id}"));
        }
        UiCommand::AnswerApproval {
            approval,
            allow,
            persist,
        } => {
            if let Err(err) = handle.answer_approval(&approval, allow, persist) {
                report(app, err);
            }
        }
        UiCommand::SpawnAgent {
            profile,
            task,
            session,
        } => {
            if let Err(err) = handle.spawn_agent(profile, task, session) {
                report(app, err);
            }
        }
        UiCommand::BotControl { bot, action } => {
            let control = match action {
                BotAction::Pause => BotControl::Pause,
                BotAction::Resume => BotControl::Resume,
                BotAction::Stop => BotControl::Stop,
            };
            if let Err(err) = handle.control_bot(&bot, control) {
                report(app, err);
            }
        }
        UiCommand::RefreshMetrics => {
            app.sync_snapshot(handle.snapshot());
        }
        UiCommand::Quit => {
            handle.shutdown();
            app.should_quit = true;
        }
    }
}

fn select(app: &mut App, id: &SessionId) {
    if let Some(index) = app.session_index(id) {
        app.active = index;
        if let Some(session) = app.sessions.get_mut(index) {
            session.unread = 0;
        }
        app.focus = vikram_tui::app::Focus::Input;
    }
}

fn report(app: &mut App, err: vikram_runtime::RuntimeError) {
    app.push_notice(vikram_runtime::types::NoticeLevel::Error, err.to_string());
}

/// Turn a typed `/word` into what it means.
///
/// A command with a chooser opens the chooser, one with a read-only answer opens the panel, and one
/// that acts acts. Nothing here writes a list of options into the conversation: a list you cannot
/// choose from is not an answer to a command that asks a question.
fn open_command(
    handle: &RuntimeHandle,
    app: &mut App,
    session: &SessionId,
    word: &str,
    args: &str,
) {
    let Some(command) = commands::find(word) else {
        // A near miss is the commonest reason a command "does nothing". Name the command the
        // operator almost typed instead of only saying the word was wrong.
        let hint = match commands::nearest(word) {
            Some(near) => format!(" — did you mean `/{}`?", near.name),
            None => " — type `/` and Enter to browse what exists".to_owned(),
        };
        app.push_notice(
            NoticeLevel::Warn,
            format!("`/{word}` is not a command{hint}"),
        );
        return;
    };

    // An inline argument is an instruction that needs no panel: `/mode plan` sets the posture,
    // `/model gpt-5.2` switches. The panel exists for when the operator has not decided yet.
    if !args.trim().is_empty() {
        match command.outcome {
            Outcome::Picker(PickerKind::Model) => {
                if let Err(err) = handle.set_model(session, args.trim().to_owned()) {
                    report(app, err);
                }
                return;
            }
            Outcome::Picker(PickerKind::Allow) => {
                let rest = args.trim();
                if let Some(rule) = rest.strip_prefix("add ") {
                    match handle.allow(session, rule.trim()) {
                        Ok(()) => {
                            app.push_notice(NoticeLevel::Info, format!("saved: {}", rule.trim()))
                        }
                        Err(err) => report(app, err),
                    }
                } else {
                    app.push_notice(
                        NoticeLevel::Warn,
                        "usage: /allow add <rule>, or /allow to choose one and forget it"
                            .to_owned(),
                    );
                }
                return;
            }
            Outcome::Picker(PickerKind::Mode) => {
                match PermissionMode::parse(args.trim()) {
                    Some(mode) => {
                        if let Err(err) = handle.set_mode(session, mode) {
                            report(app, err);
                        }
                    }
                    None => app.push_notice(
                        NoticeLevel::Warn,
                        format!("`{}` is not an approval posture", args.trim()),
                    ),
                }
                return;
            }
            _ => {}
        }
    }

    if command.name == "compact" {
        match handle.compact_now(session) {
            Ok(report) => app.push_notice(
                NoticeLevel::Info,
                format!(
                    "folded {} messages into a summary: {} -> {} tokens, {} kept verbatim",
                    report.folded, report.before_tokens, report.after_tokens, report.kept
                ),
            ),
            Err(err) => app.push_notice(NoticeLevel::Warn, err.to_string()),
        }
        return;
    }

    match command.outcome {
        Outcome::Picker(kind) => open_panel(handle, app, kind),
        Outcome::Info => {
            // No command in the table uses this outcome: a read-only answer is a picker too, so the
            // text reaches the screen. Kept so the match stays exhaustive.
            app.push_notice(
                NoticeLevel::Warn,
                format!("`/{}` has no panel", command.name),
            );
        }
        Outcome::Action => run_action(app, session, command.name),
    }
}

/// Build the chooser for `kind` from what the runtime actually knows.
///
/// Every row carries the value the runtime acts on, so choosing is an instruction. Where the source
/// is empty the picker says why instead of pretending there is nothing to want.
fn open_panel(handle: &RuntimeHandle, app: &mut App, kind: PickerKind) {
    match kind {
        PickerKind::Model => {
            // The listing is a network read: it is asked for here and lands later as
            // `ModelsListed`, which fills the panel that is already open.
            app.models_loading = true;
            app.models_error = None;
            handle.refresh_models();
            let picker = app.model_picker();
            app.open_picker(picker);
        }
        PickerKind::Mode => {
            let active = handle.mode();
            let items: Vec<PickerItem> = handle
                .modes()
                .into_iter()
                .map(|mode| {
                    let item = PickerItem::new(mode.label(), mode.as_str(), mode.as_str());
                    if mode == active {
                        item.as_current()
                    } else {
                        item
                    }
                })
                .collect();
            let picker = Picker::new(kind, "Approval posture", items);
            app.open_picker(if has_no_rows(&picker) {
                picker.with_note("this build permits no posture")
            } else {
                picker
            });
        }
        PickerKind::Allow => {
            let rules = handle.allow_list();
            let items: Vec<PickerItem> = rules
                .iter()
                .enumerate()
                .map(|(index, rule)| {
                    PickerItem::new(rule.describe(), "enter forgets it", index.to_string())
                })
                .collect();
            app.open_picker(
                Picker::new(kind, "Saved allow-list", items)
                    .with_note("ctrl+a saves a rule · enter forgets the one under the cursor"),
            );
        }
        PickerKind::Bot => {
            let mut items: Vec<PickerItem> = Vec::new();
            for agent in handle.bot_snapshots() {
                items.push(PickerItem::new(
                    agent.profile.name.clone(),
                    format!("{} · {}", agent.profile.role, agent.state.label()),
                    agent.profile.id.to_string(),
                ));
            }
            app.open_picker(Picker::new(kind, "Bots", items).with_note(
                "enter selects · ctrl+n spawn · ctrl+c new · ctrl+r rename · ctrl+x delete",
            ));
        }
        PickerKind::Mcp => {
            let servers = handle.mcp_servers();
            let items: Vec<PickerItem> = servers
                .iter()
                .map(|(name, visibility, tools)| {
                    PickerItem::new(
                        name.clone(),
                        format!("{visibility} · {tools} tool(s)"),
                        name.clone(),
                    )
                })
                .collect();
            let picker = Picker::new(kind, "MCP servers", items);
            app.open_picker(if has_no_rows(&picker) {
                picker.with_note("no MCP server has been configured for this process")
            } else {
                picker.with_note("enter flips visibility")
            });
        }
        PickerKind::Session => {
            // Built from the catalogue, not from the open tabs: a session whose tab was closed is
            // still a conversation, and the list that cannot show it is the list that teaches an
            // operator to stop closing tabs.
            let active_id = app.sessions.get(app.active).map(|view| view.id.clone());
            let items: Vec<PickerItem> = handle
                .session_catalogue()
                .into_iter()
                .map(|entry| {
                    let mut detail = format!(
                        "{} · {} message(s)",
                        entry.model.clone().unwrap_or_else(|| "no model".to_owned()),
                        entry.messages
                    );
                    let unread = app
                        .sessions
                        .iter()
                        .find(|view| view.id == entry.id)
                        .map(|view| view.unread)
                        .unwrap_or(0);
                    if unread > 0 {
                        detail.push_str(&format!(" · {unread} unread"));
                    }
                    if !entry.open {
                        detail.push_str(" · closed");
                    }
                    let item = PickerItem::new(entry.title, detail, entry.id.to_string());
                    if active_id.as_ref() == Some(&entry.id) {
                        item.as_current()
                    } else {
                        item
                    }
                })
                .collect();
            app.open_picker(Picker::new(kind, "Sessions", items));
        }
        PickerKind::Task => {
            let items: Vec<PickerItem> = app
                .tasks
                .iter()
                .map(|task| {
                    PickerItem::new(
                        task.name.clone(),
                        format!("{:?}", task.state).to_lowercase(),
                        task.id.clone(),
                    )
                })
                .collect();
            let picker = Picker::new(kind, "Background tasks", items);
            app.open_picker(if has_no_rows(&picker) {
                picker.with_note("nothing is running in the background")
            } else {
                picker.with_note("enter stops the task")
            });
        }
        PickerKind::Pending => {
            let Some(view) = app.active_session() else {
                app.push_notice(NoticeLevel::Warn, "no session is active");
                return;
            };
            let session_id = view.id.clone();
            match handle.pending_list(&session_id) {
                Ok(items) => {
                    let picker_items: Vec<PickerItem> = items
                        .iter()
                        .map(|m| {
                            let label: String = m.text.chars().take(60).collect();
                            let detail = if m.steer {
                                "steer — injected at the next safe point"
                            } else {
                                "queued — runs after the turn"
                            };
                            PickerItem::new(
                                if m.steer {
                                    format!("» {label}")
                                } else {
                                    label
                                },
                                detail,
                                m.target.clone(),
                            )
                        })
                        .collect();
                    let picker = Picker::new(kind, "Pending messages", picker_items);
                    app.open_picker(if has_no_rows(&picker) {
                        picker.with_note(
                            "nothing waiting — Enter queues while a turn runs, Ctrl+Enter steers",
                        )
                    } else {
                        picker.with_note(
                            "ctrl+e edit · ctrl+d delete · ctrl+f steer/queue · ctrl+k/j reorder",
                        )
                    });
                }
                Err(err) => report(app, err),
            }
        }
        PickerKind::Memory => {
            let entries = match handle.memory_list_all() {
                Ok(entries) => entries,
                Err(err) => {
                    app.push_notice(
                        NoticeLevel::Warn,
                        format!("memory could not be read: {err}"),
                    );
                    Vec::new()
                }
            };
            let items: Vec<PickerItem> = entries
                .iter()
                .map(|(scope, key, value)| {
                    let preview: String = value.chars().take(40).collect();
                    PickerItem::new(
                        key.clone(),
                        format!("{scope} · {preview}"),
                        format!("{scope}{MEMORY_SEPARATOR}{key}"),
                    )
                })
                .collect();
            let picker = Picker::new(kind, "Memory", items);
            app.open_picker(if has_no_rows(&picker) {
                picker.with_note("nothing has been remembered yet")
            } else {
                picker.with_note("enter reads the whole value")
            });
        }
        PickerKind::Setting => {
            let items = vec![
                PickerItem::new(
                    "interface glyphs",
                    if app.ascii { "ascii" } else { "unicode" },
                    "ascii",
                ),
                PickerItem::new(
                    "bots pane",
                    if app.show_bots { "shown" } else { "hidden" },
                    "bots",
                ),
            ];
            app.open_picker(
                Picker::new(kind, "Settings", items)
                    .with_note("enter flips the setting; it applies immediately"),
            );
        }
        PickerKind::HiveRoster => {
            let rosters = handle.hive_roster_list();
            let mut items = Vec::new();
            if rosters.is_empty() {
                items.push(PickerItem::new(
                    "no hives yet".to_owned(),
                    String::new(),
                    "empty".to_owned(),
                ));
            }
            for roster in &rosters {
                items.push(PickerItem::new(
                    roster.name.clone(),
                    format!("{} member(s)", roster.members.len()),
                    roster.name.clone(),
                ));
                for member in &roster.members {
                    items.push(PickerItem::new(
                        format!("  {member}"),
                        String::new(),
                        format!("member_{}", member),
                    ));
                }
            }
            app.open_picker(Picker::new(kind, "Hives", items));
        }
        PickerKind::Skill => {
            let snapshot = handle.snapshot();
            let skills = handle.skill_list();
            let mut items = vec![
                PickerItem::new(
                    format!(
                        "{} discovered · {} loaded this turn",
                        snapshot.metrics.skills_discovered, snapshot.metrics.skills_loaded
                    ),
                    String::new(),
                    "summary".to_owned(),
                ),
                PickerItem::new(
                    "! bodies are read on demand, never scanned into context".to_owned(),
                    String::new(),
                    "hint".to_owned(),
                ),
            ];
            if skills.is_empty() {
                items.push(PickerItem::new(
                    "no skill directories found".to_owned(),
                    String::new(),
                    "empty".to_owned(),
                ));
            } else {
                for skill in &skills {
                    let status = if skill.enabled { "on" } else { "off" };
                    let detail = if skill.description.is_empty() {
                        format!("{} · {}", skill.scope, status)
                    } else {
                        format!("{} · {} · {}", skill.description, skill.scope, status)
                    };
                    let item = PickerItem::new(skill.name.clone(), detail, skill.name.clone());
                    items.push(item);
                }
            }
            app.open_picker(Picker::new(kind, "Skills", items));
        }
        PickerKind::File => input::open_file_picker(app),
        PickerKind::Shell => input::open_shell_picker(app),
        PickerKind::Command => input::open_palette(app),
        PickerKind::Provider => {
            let config = handle.config();
            // One row per endpoint this build knows by name, then the escape hatch. A preset is a
            // shortcut for three config values, not a second registry: what is written is the same
            // `base_url`/`api_key_env`/`provider` triple a manual edit would set.
            let mut items: Vec<PickerItem> = PROVIDERS
                .iter()
                .map(|preset| {
                    // Each row reports its own variable, so "set" beside a row means that endpoint
                    // is usable — not that the endpoint currently in force happens to have a key.
                    let key = if vikram_runtime::config::Config::key_present(preset.api_key_env) {
                        "key set"
                    } else {
                        "no key"
                    };
                    let item = PickerItem::new(
                        preset.name,
                        // The scheme is dropped from the detail: it is the same for every row and
                        // costs the columns that the key state needs to stay readable.
                        format!(
                            "${env} · {key} · {host}",
                            env = preset.api_key_env,
                            host = preset
                                .base_url
                                .trim_start_matches("https://")
                                .trim_start_matches("http://"),
                        ),
                        preset.encode(),
                    );
                    if config.base_url.trim_end_matches('/') == preset.base_url {
                        item.as_current()
                    } else {
                        item
                    }
                })
                .collect();
            items.push(PickerItem::new(
                "custom endpoint…",
                format!("keep ${} and type a base URL", config.api_key_env),
                CUSTOM_PROVIDER.to_owned(),
            ));
            items.push(PickerItem::new(
                "! the key itself is never stored here: it lives in $VIKRAM_HOME/.env",
                String::new(),
                "hint".to_owned(),
            ));
            let mut picker = Picker::new(kind, "Provider", items);
            // Open on the endpoint in force, which is the answer to "where am I pointed?" without
            // reading every row.
            if let Some(current) = PROVIDERS
                .iter()
                .find(|preset| preset.base_url == config.base_url.trim_end_matches('/'))
            {
                picker.select_value(&current.encode());
            }
            app.open_picker(picker);
        }
        PickerKind::Permissions => {
            let rules = handle.allow_list();
            let mut items = vec![
                PickerItem::new("mode", handle.mode().as_str().to_owned(), "mode".to_owned()),
                PickerItem::new(
                    "policy",
                    "deny-first, with a floor no setting can widen".to_owned(),
                    "policy".to_owned(),
                ),
                PickerItem::new(
                    "saved rules",
                    format!("{}", rules.len()),
                    "rules_count".to_owned(),
                ),
            ];
            for rule in &rules {
                items.push(PickerItem::new(
                    format!("  {}", rule.describe()),
                    String::new(),
                    format!("rule_{}", rule.describe()),
                ));
            }
            if rules.is_empty() {
                items.push(PickerItem::new(
                    "  (none kept yet)".to_owned(),
                    String::new(),
                    "none".to_owned(),
                ));
            }
            items.push(PickerItem::new(
                "! enter changes the posture · ctrl+a saves a rule".to_owned(),
                String::new(),
                "hint".to_owned(),
            ));
            app.open_picker(Picker::new(kind, "Permissions", items));
        }
        PickerKind::Plugins => {
            let states = handle.plugin_states();
            let mut items: Vec<PickerItem> = states
                .iter()
                .map(|(id, name, state)| {
                    PickerItem::new(name.clone(), format!("{state} · {id}"), id.clone())
                })
                .collect();
            if items.is_empty() {
                items.push(PickerItem::new(
                    "no plugins installed".to_owned(),
                    String::new(),
                    "empty".to_owned(),
                ));
            }
            app.open_picker(Picker::new(kind, "Plugins", items).with_note(
                "enter inspects · ctrl+t enable/disable · ctrl+i install · ctrl+x uninstall",
            ));
        }
        PickerKind::Tools => {
            let names = handle.tools().names();
            let mut items = vec![PickerItem::new(
                format!("{} tool(s) registered", names.len()),
                String::new(),
                "count".to_owned(),
            )];
            for name in names {
                let enabled = handle.tools().is_enabled(&name);
                let desc = handle
                    .tools()
                    .get(&name)
                    .map(|t| t.spec().description)
                    .unwrap_or_default();
                let status = if enabled { "on" } else { "off" };
                let detail = if desc.is_empty() {
                    status.to_owned()
                } else {
                    format!("{} · {}", desc, status)
                };
                items.push(PickerItem::new(name.clone(), detail, name.clone()));
            }
            app.open_picker(Picker::new(kind, "Tools", items));
        }
        PickerKind::Background => {
            // The daemon is a separate process with its own rows in the same database, so this
            // panel reads state instead of asking the runtime: what is scheduled, when it runs
            // next, and how the last attempt ended.
            let mut items = Vec::new();
            let mut note = None;
            match vikram_daemon::cli::store() {
                Ok(store) => {
                    let home = vikram_daemon::cli::home();
                    let daemon = match vikram_daemon::daemon::owner(&home) {
                        Ok(Some(owner)) if owner.alive => {
                            format!("running · pid {}", owner.pid)
                        }
                        Ok(Some(owner)) => format!("stale lock · pid {} is gone", owner.pid),
                        Ok(None) => "not running · cron waits for it".to_owned(),
                        Err(err) => format!("unreadable lock: {err}"),
                    };
                    items.push(PickerItem::new("daemon", daemon, "daemon".to_owned()));
                    let enabled = store.state_bool("heartbeat.enabled", true).unwrap_or(true);
                    let interval = store
                        .state_ms("heartbeat.interval_ms")
                        .ok()
                        .flatten()
                        .map(vikram_daemon::time::humanize_ms)
                        .unwrap_or_else(|| "1m".to_owned());
                    items.push(PickerItem::new(
                        "heartbeat",
                        format!(
                            "{} · every {interval} · enter turns it {}",
                            if enabled { "on" } else { "off" },
                            if enabled { "off" } else { "on" }
                        ),
                        "heartbeat".to_owned(),
                    ));
                    let manager = vikram_daemon::jobs::JobManager::new(store.clone());
                    match manager.list() {
                        Ok(jobs) if jobs.is_empty() => {
                            note = Some(
                                "nothing is scheduled · `vikram cron add` puts work here"
                                    .to_owned(),
                            )
                        }
                        Ok(jobs) => {
                            for job in jobs {
                                let zone = vikram_daemon::time::Zone::parse(&job.timezone)
                                    .unwrap_or(vikram_daemon::time::Zone::Local);
                                let next = job
                                    .next_run_ms
                                    .map(|at| vikram_daemon::time::format_ms(zone, at))
                                    .unwrap_or_else(|| "no next run".to_owned());
                                let mut item = PickerItem::new(
                                    job.name.clone(),
                                    format!(
                                        "{} · next {next} · last {}",
                                        job.schedule, job.last_status
                                    ),
                                    job.id.clone(),
                                );
                                if job.enabled {
                                    item = item.as_current();
                                }
                                items.push(item);
                            }
                        }
                        Err(err) => note = Some(format!("jobs could not be read: {err}")),
                    }
                }
                Err(err) => note = Some(err),
            }
            let picker = Picker::new(kind, "Background", items);
            app.open_picker(match note {
                Some(note) => picker.with_note(note),
                None => picker,
            });
        }
        PickerKind::Status => {
            let snapshot = handle.snapshot();
            let metrics = snapshot.metrics.clone();
            let mut items = vec![
                PickerItem::new(
                    "runtime",
                    snapshot.status.label().to_owned(),
                    "runtime".to_owned(),
                ),
                PickerItem::new("mode", handle.mode().as_str().to_owned(), "mode".to_owned()),
                PickerItem::new(
                    "model",
                    metrics
                        .model
                        .clone()
                        .unwrap_or_else(|| "none set".to_owned()),
                    "model".to_owned(),
                ),
                PickerItem::new(
                    "sessions",
                    format!("{}", snapshot.sessions.len()),
                    "sessions".to_owned(),
                ),
                PickerItem::new(
                    "bots",
                    format!("{}", snapshot.bots.len()),
                    "bots".to_owned(),
                ),
                PickerItem::new(
                    "background tasks",
                    format!("{}", metrics.background_tasks),
                    "tasks".to_owned(),
                ),
                PickerItem::new(
                    "mcp",
                    format!("{}/{}", metrics.mcp_connected, metrics.mcp_configured),
                    "mcp".to_owned(),
                ),
                PickerItem::new(
                    "plugins",
                    format!("{}/{}", metrics.plugins_loaded, metrics.plugins_installed),
                    "plugins".to_owned(),
                ),
            ];
            if snapshot.approvals.is_empty() {
                items.push(PickerItem::new(
                    "approvals",
                    "nothing waiting".to_owned(),
                    "approvals".to_owned(),
                ));
            } else {
                items.push(PickerItem::new(
                    "approvals",
                    format!("{} waiting", snapshot.approvals.len()),
                    "approvals".to_owned(),
                ));
            }
            app.open_picker(Picker::new(kind, "Status", items));
        }
        PickerKind::Usage => {
            let metrics = handle.snapshot().metrics;
            let mut items = Vec::new();
            if let Some(view) = app.display_session() {
                items.push(PickerItem::new(
                    "input",
                    format!(
                        "{} reported · {} estimated",
                        view.tokens
                            .reported_in
                            .map(|n| n.to_string())
                            .unwrap_or_else(|| "—".to_owned()),
                        view.tokens.estimated_in
                    ),
                    "input".to_owned(),
                ));
                items.push(PickerItem::new(
                    "output",
                    format!(
                        "{} reported · {} estimated",
                        view.tokens
                            .reported_out
                            .map(|n| n.to_string())
                            .unwrap_or_else(|| "—".to_owned()),
                        view.tokens.estimated_out
                    ),
                    "output".to_owned(),
                ));
                items.push(PickerItem::new(
                    "provider calls",
                    format!("{}", view.tokens.calls),
                    "calls".to_owned(),
                ));
            } else {
                items.push(PickerItem::new(
                    "no session is selected".to_owned(),
                    String::new(),
                    "error".to_owned(),
                ));
            }
            if let Some(rate) = metrics.tokens_per_sec {
                items.push(PickerItem::new(
                    "rate",
                    format!("{rate:.1} tok/s"),
                    "rate".to_owned(),
                ));
            }
            items.push(PickerItem::new(
                "! reported numbers come from the provider; estimates never bill".to_owned(),
                String::new(),
                "hint".to_owned(),
            ));
            app.open_picker(Picker::new(kind, "Usage", items));
        }
        PickerKind::Context => {
            let metrics = handle.snapshot().metrics;
            let mut items = vec![PickerItem::new(
                "used",
                format!(
                    "{} tokens{}",
                    metrics.context_used,
                    if metrics.context_measured {
                        " (measured; newest tail estimated)"
                    } else {
                        " (estimated, self-calibrated)"
                    }
                ),
                "used".to_owned(),
            )];
            match metrics.context_window {
                Some(window) if window > 0 => {
                    items.push(PickerItem::new(
                        "window",
                        format!(
                            "{window} tokens{}",
                            if metrics.context_limit_assumed {
                                " (assumed: nothing advertised one)"
                            } else {
                                ""
                            }
                        ),
                        "window".to_owned(),
                    ));
                    items.push(PickerItem::new(
                        "usable",
                        format!(
                            "{} tokens (after reply reserve, tool schemas, safety margin)",
                            metrics.context_usable
                        ),
                        "usable".to_owned(),
                    ));
                    items.push(PickerItem::new(
                        "left",
                        format!(
                            "{} tokens",
                            metrics.context_usable.saturating_sub(metrics.context_used)
                        ),
                        "left".to_owned(),
                    ));
                    if let Some(ratio) = metrics.context_ratio() {
                        items.push(PickerItem::new(
                            "usage",
                            format!("{:.1}% of the window", ratio * 100.0),
                            "ratio".to_owned(),
                        ));
                    }
                    items.push(PickerItem::new(
                        "folds",
                        format!(
                            "{} (older turns summarised; originals stay in the journal)",
                            metrics.context_compactions
                        ),
                        "folds".to_owned(),
                    ));
                }
                _ => items.push(PickerItem::new(
                    "window",
                    "not known for this model".to_owned(),
                    "window".to_owned(),
                )),
            }
            // What the last request actually carried, part by part, when one has gone out.
            if let Some((_, meter)) = app
                .context_detail
                .as_ref()
                .filter(|(id, _)| app.active_session().is_some_and(|s| s.id == *id))
            {
                for part in &meter.parts {
                    items.push(PickerItem::new(
                        format!("  {}", part.label),
                        format!("{} tokens", part.tokens),
                        "part".to_owned(),
                    ));
                }
                items.push(PickerItem::new(
                    "reply reserve",
                    format!("{} tokens", meter.output_reserve),
                    "reserve".to_owned(),
                ));
            } else {
                items.push(PickerItem::new(
                    "! no request has gone out yet: these are totals, not one prompt".to_owned(),
                    String::new(),
                    "hint".to_owned(),
                ));
            }
            app.open_picker(Picker::new(kind, "Context", items));
        }
        PickerKind::Doctor => {
            let checks = crate::doctor::checks(&handle.config());
            let mut items: Vec<PickerItem> = checks
                .iter()
                .map(|check| {
                    PickerItem::new(
                        check.text.clone(),
                        check.level.label().to_owned(),
                        format!("check_{}", check.text),
                    )
                })
                .collect();
            let problems = checks
                .iter()
                .filter(|check| check.level.is_failure())
                .count();
            items.push(PickerItem::new(
                if problems == 0 {
                    "! no blocking problems".to_owned()
                } else {
                    format!("! {problems} blocking problem(s)")
                },
                String::new(),
                "summary".to_owned(),
            ));
            app.open_picker(Picker::new(kind, "Doctor", items));
        }
        PickerKind::Version => {
            let snapshot = handle.snapshot();
            let items = vec![
                PickerItem::new(
                    format!("vikram {}", env!("CARGO_PKG_VERSION")),
                    String::new(),
                    "version".to_owned(),
                ),
                PickerItem::new(
                    format!("workspace: {}", snapshot.workspace_root.display()),
                    String::new(),
                    "workspace".to_owned(),
                ),
            ];
            app.open_picker(Picker::new(kind, "Version", items));
        }
        PickerKind::Approvals => {
            let snapshot = handle.snapshot();
            let mut items = Vec::new();
            if snapshot.approvals.is_empty() {
                items.push(PickerItem::new(
                    "nothing waiting".to_owned(),
                    String::new(),
                    "empty".to_owned(),
                ));
            } else {
                for approval in &snapshot.approvals {
                    items.push(PickerItem::new(
                        approval.verb.clone(),
                        approval.resource.clone(),
                        format!("{:?}", approval.approval),
                    ));
                }
            }
            app.open_picker(Picker::new(kind, "Approvals", items));
        }
        PickerKind::Keys => {
            let items = vec![
                PickerItem::new("Enter", "send the message", "enter".to_owned()),
                PickerItem::new("Shift+Enter", "insert a newline", "shift_enter".to_owned()),
                PickerItem::new("Ctrl+N", "new session", "ctrl_n".to_owned()),
                PickerItem::new("Ctrl+Tab", "next session", "ctrl_tab".to_owned()),
                PickerItem::new("Ctrl+B", "toggle the BOTS panel", "ctrl_b".to_owned()),
                PickerItem::new("Ctrl+O", "approvals", "ctrl_o".to_owned()),
                PickerItem::new("Ctrl+R", "rename the session", "ctrl_r".to_owned()),
                PickerItem::new("r", "resume selected bot", "r".to_owned()),
                PickerItem::new("F2", "background tasks", "f2".to_owned()),
                PickerItem::new("F3", "MCP servers", "f3".to_owned()),
                PickerItem::new("F4", "plugins", "f4".to_owned()),
                PickerItem::new("F5", "sub-bots", "f5".to_owned()),
                PickerItem::new("F6", "context usage", "f6".to_owned()),
                PickerItem::new("F7", "model and throughput", "f7".to_owned()),
                PickerItem::new("F8", "skills browser", "f8".to_owned()),
                PickerItem::new(
                    "PgUp / PgDn",
                    "scroll the transcript",
                    "pgup_pgdn".to_owned(),
                ),
                PickerItem::new("Esc", "cancel the running turn", "esc".to_owned()),
                PickerItem::new("F1 / ?", "this help", "f1_help".to_owned()),
            ];
            app.open_picker(Picker::new(kind, "Key bindings", items));
        }
        PickerKind::Diff => {
            let items = vec![PickerItem::new(
                "git diff",
                "show workspace changes",
                "git_diff".to_owned(),
            )];
            app.open_picker(Picker::new(kind, "Run git diff", items));
        }
        PickerKind::Test => {
            let items = vec![PickerItem::new(
                "cargo test",
                "run the test suite",
                "cargo_test".to_owned(),
            )];
            app.open_picker(Picker::new(kind, "Run tests", items));
        }
        PickerKind::Build => {
            let items = vec![PickerItem::new(
                "cargo build",
                "build the workspace",
                "cargo_build".to_owned(),
            )];
            app.open_picker(Picker::new(kind, "Build workspace", items));
        }
        PickerKind::Clear => {
            let items = vec![PickerItem::new(
                "Clear this conversation",
                "the transcript in this view goes away",
                "clear".to_owned(),
            )];
            app.open_picker(Picker::new(kind, "Clear conversation", items));
        }
        PickerKind::NewSession => {
            let items = vec![PickerItem::new(
                "Start a new session",
                "creates a fresh conversation",
                "new".to_owned(),
            )];
            app.open_picker(Picker::new(kind, "New session", items));
        }
        PickerKind::Exit => {
            let items = vec![PickerItem::new(
                "Quit VIKRAM",
                "exit the application",
                "exit".to_owned(),
            )];
            app.open_picker(Picker::new(kind, "Quit", items));
        }
        PickerKind::RenameSession => {
            let title = app
                .active_session()
                .map(|view| view.title.clone())
                .unwrap_or_else(|| "no session".to_owned());
            let items = vec![PickerItem::new(
                format!("Rename: {}", title),
                "type a new name for this conversation",
                "rename".to_owned(),
            )];
            app.open_picker(Picker::new(kind, "Rename session", items));
        }
        PickerKind::CloseSession => {
            let title = app
                .active_session()
                .map(|view| view.title.clone())
                .unwrap_or_else(|| "no session".to_owned());
            if app.sessions.len() <= 1 {
                app.push_notice(
                    NoticeLevel::Warn,
                    "this is the only conversation; close it and there is nowhere to type",
                );
                return;
            }
            let items = vec![PickerItem::new(
                format!("Close: {}", title),
                "the transcript stays in the store; only this view is closed",
                "close".to_owned(),
            )];
            app.open_picker(Picker::new(kind, "Close session", items));
        }
    }
}

/// Separates scope from key inside a memory row's value, so one picker row carries both without
/// hoping neither ever contains a space.
const MEMORY_SEPARATOR: char = '\u{1f}';

/// Separates the three config values a provider row writes, so one row carries the endpoint, the
/// variable that holds its key, and the dialect it speaks.
const PROVIDER_SEPARATOR: char = '\u{1e}';

/// The row that opens a prompt instead of naming a preset.
const CUSTOM_PROVIDER: &str = "custom";

/// An endpoint this build knows by name.
///
/// A preset carries exactly the three things that make an endpoint usable and nothing else. The
/// config file stays the single source of truth: choosing a row writes those three values, which is
/// the same edit `vikram config set base_url … && vikram config set api_key_env …` makes.
struct ProviderPreset {
    name: &'static str,
    base_url: &'static str,
    api_key_env: &'static str,
    kind: &'static str,
}

impl ProviderPreset {
    /// The row value: `kind`, `base_url`, `api_key_env`, in that order, separated by
    /// [`PROVIDER_SEPARATOR`]. No URL or variable name contains that character, so splitting is
    /// exact rather than hopeful.
    fn encode(&self) -> String {
        format!(
            "{kind}{PROVIDER_SEPARATOR}{url}{PROVIDER_SEPARATOR}{env}",
            kind = self.kind,
            url = self.base_url,
            env = self.api_key_env
        )
    }
}

const PROVIDERS: &[ProviderPreset] = &[
    ProviderPreset {
        name: "OpenRouter",
        base_url: "https://openrouter.ai/api/v1",
        api_key_env: "OPENROUTER_API_KEY",
        kind: "openai",
    },
    ProviderPreset {
        name: "NVIDIA NIM",
        base_url: "https://integrate.api.nvidia.com/v1",
        api_key_env: "NVIDIA_API_KEY",
        kind: "openai",
    },
    ProviderPreset {
        name: "OpenAI",
        base_url: "https://api.openai.com/v1",
        api_key_env: "OPENAI_API_KEY",
        kind: "openai",
    },
    ProviderPreset {
        name: "Anthropic",
        base_url: "https://api.anthropic.com",
        api_key_env: "ANTHROPIC_API_KEY",
        kind: "anthropic",
    },
    ProviderPreset {
        name: "Groq",
        base_url: "https://api.groq.com/openai/v1",
        api_key_env: "GROQ_API_KEY",
        kind: "openai",
    },
    ProviderPreset {
        name: "Together",
        base_url: "https://api.together.xyz/v1",
        api_key_env: "TOGETHER_API_KEY",
        kind: "openai",
    },
    ProviderPreset {
        name: "Ollama (local)",
        base_url: "http://127.0.0.1:11434/v1",
        api_key_env: "OLLAMA_API_KEY",
        kind: "openai",
    },
    ProviderPreset {
        name: "LM Studio (local)",
        base_url: "http://127.0.0.1:1234/v1",
        api_key_env: "LMSTUDIO_API_KEY",
        kind: "openai",
    },
];

fn has_no_rows(picker: &Picker) -> bool {
    picker.items.is_empty()
}

/// The action half: what happens once a row is chosen. Every branch reaches the runtime method that
/// owns the decision; none of them patches a label and hopes the runtime agrees later.
fn apply_act(
    handle: &RuntimeHandle,
    app: &mut App,
    session: &SessionId,
    kind: PickerKind,
    verb: Verb,
    value: String,
) {
    match (kind, verb) {
        (PickerKind::Model, Verb::Prompt) => app.open_prompt(
            "Model",
            "name the model this session should run on, then press Enter",
            vikram_tui::app::PromptAction::SetModel,
        ),
        (PickerKind::Allow, Verb::Prompt) => app.open_prompt(
            "Save a rule",
            "a rule this runtime may act on without asking, then press Enter",
            vikram_tui::app::PromptAction::AddAllowRule,
        ),
        (PickerKind::Session, Verb::Rename) => {
            // Rename acts on the conversation the row names, not on whichever is active: switching
            // first is what makes the panel's row mean what it says.
            let title = app
                .sessions
                .iter()
                .find(|view| view.id.to_string() == value)
                .map(|view| view.title.clone())
                .unwrap_or_default();
            app.commands.push(UiCommand::SwitchSession {
                session: SessionId::new(value),
            });
            let mut editor = vikram_tui::editor::Editor::new();
            editor.insert_str(&title);
            app.overlay = Some(Overlay::RenameSession { editor });
        }
        (PickerKind::Session, Verb::Close) => {
            let title = app
                .sessions
                .iter()
                .find(|view| view.id.to_string() == value)
                .map(|view| view.title.clone())
                .unwrap_or_else(|| value.clone());
            app.open_confirm(
                "Close a conversation",
                vec![
                    title,
                    "! the transcript stays in the store; only this view is closed".to_owned(),
                ],
                UiCommand::CloseSession {
                    session: SessionId::new(value),
                },
            );
        }
        (PickerKind::Memory, Verb::Prompt) => app.open_prompt(
            "Remember something",
            "key: value — e.g. `editor: I prefer vim`",
            vikram_tui::app::PromptAction::SaveMemory,
        ),
        (PickerKind::Memory, Verb::Remove) => {
            if let Some((scope, key)) = value.split_once(MEMORY_SEPARATOR) {
                app.open_confirm(
                    "Forget a key",
                    vec![
                        format!("{scope}/{key}"),
                        "! what the model remembered under this key is deleted".to_owned(),
                    ],
                    UiCommand::ForgetMemory {
                        scope: scope.to_owned(),
                        key: key.to_owned(),
                    },
                );
            }
        }
        (PickerKind::Model, Verb::Choose) => {
            if let Err(err) = handle.set_model(session, value) {
                report(app, err);
            }
        }
        (PickerKind::Mode, Verb::Choose) => match PermissionMode::parse(&value) {
            Some(mode) => {
                if let Err(err) = handle.set_mode(session, mode) {
                    report(app, err);
                }
            }
            None => app.push_notice(NoticeLevel::Warn, format!("`{value}` is not a posture")),
        },
        (PickerKind::Allow, Verb::Remove) => {
            let index: usize = match value.trim().parse() {
                Ok(index) => index,
                Err(_) => {
                    app.push_notice(NoticeLevel::Error, "that row is not a rule");
                    return;
                }
            };
            let description = handle
                .allow_list()
                .get(index)
                .map(|rule| rule.describe())
                .unwrap_or_else(|| format!("rule {index}"));
            app.open_confirm(
                "Forget a rule",
                vec![
                    description,
                    "! a permission the operator saved once will have to be asked again".to_owned(),
                ],
                UiCommand::Unallow { index },
            );
        }
        (PickerKind::Bot, Verb::New) => app.overlay = Some(Overlay::AddBot { selected: 0 }),
        (PickerKind::Bot, Verb::Prompt) => app.open_prompt(
            "New bot",
            "name: role: description — e.g. `reviewer: code review: reads diffs and reports`",
            vikram_tui::app::PromptAction::CreateBot,
        ),
        (PickerKind::Bot, Verb::Rename) => app.open_prompt(
            "Rename bot",
            "the new name, then press Enter",
            vikram_tui::app::PromptAction::RenameBot {
                bot: BotId::new(value),
            },
        ),
        (PickerKind::Bot, Verb::Remove) => {
            let name = app
                .bots
                .iter()
                .find(|b| b.profile.id.to_string() == value)
                .map(|b| b.profile.name.clone())
                .unwrap_or_else(|| value.clone());
            app.open_confirm(
                "Delete a bot",
                vec![
                    name,
                    "! the profile is removed from the store and cannot be undone".to_owned(),
                ],
                UiCommand::DeleteBot {
                    bot: BotId::new(value),
                },
            );
        }
        (PickerKind::Bot, Verb::Toggle) => {
            let action = app
                .bots
                .iter()
                .find(|b| b.profile.id.to_string() == value)
                .map(|b| {
                    if matches!(b.state, vikram_runtime::types::BotState::Paused) {
                        BotAction::Resume
                    } else {
                        BotAction::Pause
                    }
                });
            match action {
                Some(action) => app.commands.push(UiCommand::BotControl {
                    bot: BotId::new(value),
                    action,
                }),
                None => app.push_notice(NoticeLevel::Warn, "that bot is not running"),
            }
        }
        (PickerKind::Bot, Verb::Choose) => {
            app.selected_bot = Some(BotId::new(value));
            app.push_notice(NoticeLevel::Info, "showing that bot's conversation");
        }
        (PickerKind::Pending, Verb::Prompt) => app.open_prompt(
            "Edit pending message",
            "rewrite the message, then press Enter",
            vikram_tui::app::PromptAction::EditPending {
                session: session.clone(),
                target: value,
            },
        ),
        (PickerKind::Pending, Verb::Remove) => {
            if let Err(err) = handle.pending_remove(session, &value) {
                report(app, err);
            }
        }
        (PickerKind::Pending, Verb::Toggle) => {
            if let Err(err) = handle.pending_flip(session, &value) {
                report(app, err);
            }
        }
        (PickerKind::Pending, Verb::MoveUp) => {
            if let Err(err) = handle.pending_move(session, &value, -1) {
                report(app, err);
            }
        }
        (PickerKind::Pending, Verb::MoveDown) => {
            if let Err(err) = handle.pending_move(session, &value, 1) {
                report(app, err);
            }
        }
        (PickerKind::Mcp, _) => match handle.toggle_mcp_visibility(&value) {
            Ok(state) => app.push_notice(NoticeLevel::Info, format!("{value}: {state}")),
            Err(err) => report(app, err),
        },
        (PickerKind::Session, Verb::Choose) => {
            app.commands.push(UiCommand::SwitchSession {
                session: SessionId::new(value),
            });
        }
        (PickerKind::Session, Verb::New) => {
            app.commands.push(UiCommand::NewSession);
        }
        (PickerKind::Provider, Verb::Choose) => {
            if value == CUSTOM_PROVIDER {
                app.open_prompt(
                    "Endpoint",
                    "a base URL for an OpenAI-compatible endpoint",
                    vikram_tui::app::PromptAction::SetEndpoint,
                );
                return;
            }
            let mut parts = value.split(PROVIDER_SEPARATOR);
            let (Some(kind), Some(base_url), Some(api_key_env)) =
                (parts.next(), parts.next(), parts.next())
            else {
                // A hint row is not a choice.
                return;
            };
            match handle.set_provider(base_url.to_owned(), api_key_env.to_owned(), kind.to_owned())
            {
                Ok(()) => {
                    // The list on screen belongs to the endpoint we just left, so it is dropped
                    // rather than offered: a model id from the old server is not a choice on the
                    // new one. The fresh read fills the panel in when it lands.
                    app.models.clear();
                    app.models_error = None;
                    app.models_loading = true;
                    handle.refresh_models();
                }
                Err(err) => report(app, err),
            }
        }
        (PickerKind::Permissions, _) => {
            // Info panel - just close
        }
        (PickerKind::Plugins, Verb::Choose) => {
            if value == "empty" {
                return;
            }
            match handle
                .plugin_states()
                .into_iter()
                .find(|(id, _, _)| id == &value)
            {
                Some((id, name, state)) => {
                    app.open_info(
                        format!("Plugin · {name}"),
                        vec![
                            format!("id: {id}"),
                            format!("state: {state}"),
                            "! a plugin declares what it needs; the policy still decides every call"
                                .to_owned(),
                        ],
                    );
                }
                None => app.push_notice(NoticeLevel::Warn, format!("plugin `{value}` not found")),
            }
        }
        (PickerKind::Plugins, Verb::Toggle) => {
            let state = handle
                .plugin_states()
                .into_iter()
                .find(|(id, _, _)| id == &value)
                .map(|(_, _, s)| s);
            let enabled = !matches!(state.as_deref(), Some("disabled"));
            match handle.plugin_set_enabled(&value, !enabled) {
                Ok(()) => app.push_notice(
                    NoticeLevel::Info,
                    format!(
                        "plugin `{value}` {}",
                        if enabled { "disabled" } else { "enabled" }
                    ),
                ),
                Err(err) => report(app, err),
            }
        }
        (PickerKind::Plugins, Verb::Prompt) => app.open_prompt(
            "Install plugin",
            "path to a plugin directory",
            vikram_tui::app::PromptAction::InstallPlugin,
        ),
        (PickerKind::Plugins, Verb::Remove) => {
            app.open_confirm(
                "Uninstall a plugin",
                vec![
                    value.clone(),
                    "! the plugin is removed from the registry".to_owned(),
                ],
                UiCommand::UninstallPlugin { id: value },
            );
        }
        (PickerKind::Tools, Verb::Choose) => {
            if let Some(tool) = handle.tools().get(&value) {
                let spec = tool.spec();
                let mut lines = Vec::new();
                if !spec.description.is_empty() {
                    lines.push(spec.description.clone());
                }
                let schema = serde_json::to_string_pretty(&spec.schema).unwrap_or_default();
                if !schema.is_empty() && schema != "null" {
                    lines.push(String::new());
                    lines.push(schema);
                }
                app.open_info(format!("Tool · {}", spec.name), lines);
            } else {
                app.push_notice(NoticeLevel::Warn, format!("tool `{value}` not found"));
            }
        }
        (PickerKind::Tools, Verb::Toggle) => {
            let enabled = handle.tools().is_enabled(&value);
            if enabled {
                handle.tools().tool_disable(&value);
                app.push_notice(NoticeLevel::Info, format!("tool `{value}` disabled"));
            } else {
                handle.tools().tool_enable(&value);
                app.push_notice(NoticeLevel::Info, format!("tool `{value}` enabled"));
            }
        }
        (PickerKind::Background, Verb::Toggle) => {
            // One panel, two kinds of row: the heartbeat row flips the beat, a job row flips that
            // job. Both write to the store the daemon reads, so the change takes effect on its
            // next tick without restarting anything.
            let outcome = (|| -> Result<String, String> {
                let store = vikram_daemon::cli::store()?;
                if value == "heartbeat" {
                    let on = store
                        .state_bool("heartbeat.enabled", true)
                        .map_err(|err| err.to_string())?;
                    return vikram_daemon::cli::heartbeat_set_enabled(!on);
                }
                if value == "daemon" {
                    return Err(
                        "the daemon row only reports; `vikram daemon start` runs it".to_owned()
                    );
                }
                let job = store
                    .get_job(&value)
                    .map_err(|err| err.to_string())?
                    .ok_or_else(|| format!("{value} is no longer scheduled"))?;
                if job.enabled {
                    vikram_daemon::cli::cron_pause(&job.id)
                } else {
                    vikram_daemon::cli::cron_resume(&job.id)
                }
            })();
            match outcome {
                Ok(text) => app.push_notice(NoticeLevel::Info, text),
                Err(err) => app.push_notice(NoticeLevel::Warn, err),
            }
            open_panel(handle, app, PickerKind::Background);
        }
        (PickerKind::Status, _) => {
            // Info panel - just close
        }
        (PickerKind::Usage, _) => {
            // Info panel - just close
        }
        (PickerKind::Context, _) => {
            // Info panel - just close
        }
        (PickerKind::Doctor, _) => {
            // Info panel - just close
        }
        (PickerKind::Version, _) => {
            // Info panel - just close
        }
        (PickerKind::HiveRoster, _) => {
            // Info panel - just close
        }
        (PickerKind::Skill, Verb::Choose) => {
            if let Some(skill) = handle.skill_get(&value) {
                let mut lines = Vec::new();
                if !skill.description.is_empty() {
                    lines.push(skill.description.clone());
                }
                lines.push(format!(
                    "scope: {} · enabled: {}",
                    skill.scope,
                    if skill.enabled { "yes" } else { "no" }
                ));
                lines.push(format!("loaded: {}", skill.loaded));
                if !skill.instructions.is_empty() {
                    lines.push(String::new());
                    lines.push(skill.instructions.clone());
                } else {
                    // load body on demand
                    if let Some(full) = handle.skill_get(&value)
                        && !full.instructions.is_empty()
                    {
                        lines.push(String::new());
                        lines.push(full.instructions.clone());
                    }
                }
                app.open_info(format!("Skill · {}", skill.name), lines);
            } else {
                app.push_notice(NoticeLevel::Warn, format!("skill `{value}` not found"));
            }
        }
        (PickerKind::Skill, Verb::Toggle) => {
            // determine current enabled state
            let enabled = handle
                .skill_list()
                .iter()
                .find(|s| s.name == value)
                .map(|s| s.enabled)
                .unwrap_or(true);
            let res = if enabled {
                handle.skill_disable(&value)
            } else {
                handle.skill_enable(&value)
            };
            match res {
                Ok(()) => app.push_notice(
                    NoticeLevel::Info,
                    format!(
                        "skill `{value}` {}",
                        if enabled { "disabled" } else { "enabled" }
                    ),
                ),
                Err(err) => report(app, err),
            }
        }
        (PickerKind::Skill, Verb::New) => {
            app.open_prompt(
                "New skill",
                "name: description (e.g. my-skill: what it does)",
                vikram_tui::app::PromptAction::CreateSkill,
            );
        }
        (PickerKind::Skill, Verb::Prompt) => {
            app.open_prompt(
                "Import skill",
                "path to a skill directory",
                vikram_tui::app::PromptAction::ImportSkill,
            );
        }
        (PickerKind::Skill, Verb::Rename) => app.open_prompt(
            "Rename skill",
            "the new skill name, then press Enter",
            vikram_tui::app::PromptAction::RenameSkill { skill: value },
        ),
        (PickerKind::Skill, Verb::Remove) => {
            let desc = handle
                .skill_get(&value)
                .map(|s| s.description)
                .unwrap_or_default();
            app.open_confirm(
                "Delete a skill",
                vec![
                    value.clone(),
                    if desc.is_empty() {
                        "moves it to trash; recoverable".to_owned()
                    } else {
                        desc
                    },
                ],
                UiCommand::DeleteSkill {
                    name: value.clone(),
                },
            );
        }
        (PickerKind::Keys, _) => {
            // Info panel - just close
        }
        // These kinds no longer have a panel of their own: the command that used to open one acts
        // directly now. Routed through `run_action` so there is still exactly one implementation.
        (PickerKind::Diff, _) => run_action(app, session, "diff"),
        (PickerKind::Test, _) => run_action(app, session, "test"),
        (PickerKind::Build, _) => run_action(app, session, "build"),
        (PickerKind::Clear, _) => run_action(app, session, "clear"),
        (PickerKind::NewSession, _) => run_action(app, session, "new"),
        (PickerKind::Exit, _) => run_action(app, session, "exit"),
        (PickerKind::RenameSession, _) => run_action(app, session, "rename"),
        (PickerKind::CloseSession, _) => run_action(app, session, "close"),
        (PickerKind::Approvals, _) => app.overlay = Some(Overlay::Approvals),
        (PickerKind::Task, _) => {
            app.open_confirm(
                "Stop a task",
                vec![
                    value.clone(),
                    "! the process is asked to stop; nothing is invented about its output"
                        .to_owned(),
                ],
                UiCommand::StopTask { id: value },
            );
        }
        (PickerKind::Memory, _) => {
            let (scope, key) = match value.split_once(MEMORY_SEPARATOR) {
                Some(parts) => parts,
                None => return,
            };
            let entry = match handle.memory_list_all() {
                Ok(entries) => entries
                    .into_iter()
                    .find(|(entry_scope, entry_key, _)| entry_scope == scope && entry_key == key),
                Err(err) => {
                    app.push_notice(
                        NoticeLevel::Warn,
                        format!("memory could not be read: {err}"),
                    );
                    return;
                }
            };
            match entry {
                Some((scope, key, remembered)) => {
                    let mut lines: Vec<String> = remembered.lines().map(str::to_owned).collect();
                    lines.push(format!("! scope: {scope} · key: {key}"));
                    app.open_info(format!("Memory · {key}"), lines);
                }
                None => app.push_notice(
                    NoticeLevel::Warn,
                    "that key is no longer remembered".to_owned(),
                ),
            }
        }
        (PickerKind::Setting, _) => match value.as_str() {
            "ascii" => {
                app.ascii = !app.ascii;
                app.push_notice(
                    NoticeLevel::Info,
                    if app.ascii {
                        "glyphs: ascii"
                    } else {
                        "glyphs: unicode"
                    },
                );
            }
            "bots" => {
                app.show_bots = !app.show_bots;
                app.push_notice(
                    NoticeLevel::Info,
                    if app.show_bots {
                        "bots pane: shown"
                    } else {
                        "bots pane: hidden"
                    },
                );
            }
            other => app.push_notice(NoticeLevel::Warn, format!("no setting `{other}`")),
        },
        (PickerKind::File, _) => {
            if let Some(view) = app.active_session_mut() {
                view.editor.insert_str(&format!("@{value} "));
            }
            app.focus = Focus::Input;
        }
        (PickerKind::Shell, _) => {
            app.commands.push(UiCommand::RunShell {
                session: session.clone(),
                command: value,
            });
        }
        (PickerKind::Command, _) => open_command(handle, app, session, &value, ""),
        (other, verb) => app.push_notice(
            NoticeLevel::Warn,
            format!("no `{:?}` action for a `{}` row yet", verb, other.as_str()),
        ),
    }
}

/// Commands that do exactly one thing. Each one goes through the same gate, approval and journal as
/// a model-issued tool call, because an operator typing a build is still a build.
///
/// Typing the command *is* the instruction: nothing here opens a panel asking to confirm what was
/// just asked for. Every one of these leaves a notice behind, so the screen still says what
/// happened after the transcript has scrolled.
fn run_action(app: &mut App, session: &SessionId, name: &str) {
    match name {
        "clear" => {
            app.commands.push(UiCommand::Clear {
                session: session.clone(),
            });
            app.push_notice(
                NoticeLevel::Info,
                "cleared this view; the stored conversation is untouched — /sessions brings it back",
            );
        }
        "new" => {
            app.commands.push(UiCommand::NewSession);
        }
        "rename" => {
            let title = app
                .session_by_id_mut(session)
                .map(|view| view.title.clone())
                .unwrap_or_default();
            // The existing rename panel, pre-filled with the name it currently has — editing beats
            // retyping, and the panel already knows how to commit the change.
            let mut editor = vikram_tui::editor::Editor::new();
            editor.insert_str(&title);
            app.overlay = Some(Overlay::RenameSession { editor });
        }
        "close" => {
            let title = app
                .session_by_id_mut(session)
                .map(|view| view.title.clone())
                .unwrap_or_default();
            if app.sessions.len() <= 1 {
                app.push_notice(
                    NoticeLevel::Warn,
                    "this is the only conversation; close it and there is nowhere to type",
                );
                return;
            }
            app.commands.push(UiCommand::CloseSession {
                session: session.clone(),
            });
            app.push_notice(
                NoticeLevel::Info,
                format!("closed {title}; it stays in the store — /sessions reopens it"),
            );
        }
        "keys" => app.overlay = Some(Overlay::Help),
        "copy" => {
            app.copy_last_assistant_message();
        }
        "approvals" => app.overlay = Some(Overlay::Approvals),
        "exit" => {
            app.commands.push(UiCommand::Quit);
            app.should_quit = true;
        }
        "diff" => run_shell(app, session, "git diff"),
        "test" => run_shell(app, session, "cargo test"),
        "build" => run_shell(app, session, "cargo build"),
        other => app.push_notice(NoticeLevel::Warn, format!("`/{other}` has no action")),
    }
}

/// Run a workspace command on the operator's word. It is queued as a shell call, so it passes the
/// same approval gate and lands in the same journal as a command the model asked for.
fn run_shell(app: &mut App, session: &SessionId, command: &str) {
    app.commands.push(UiCommand::RunShell {
        session: session.clone(),
        command: command.to_owned(),
    });
    app.push_notice(NoticeLevel::Info, format!("running {command}"));
}

/// Hand text to the system clipboard. Wayland first, then X11, then the terminal's own OSC 52 —
/// the last one works over SSH, where no display exists at all. A copy that fails everywhere says
/// so in the transcript instead of pretending it happened.
fn copy_to_clipboard(text: &str) {
    use std::io::Write as _;
    use std::process::{Command, Stdio};

    let piped = ["wl-copy", "xclip"].iter().find_map(|program| {
        let mut command = Command::new(program);
        if *program == "xclip" {
            command.args(["-selection", "clipboard"]);
        }
        let mut child = command
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .ok()?;
        let mut stdin = child.stdin.take()?;
        stdin.write_all(text.as_bytes()).ok()?;
        drop(stdin);
        let status = child.wait().ok()?;
        status.success().then_some(())
    });
    if piped.is_some() {
        return;
    }

    let encoded = base64(text.as_bytes());
    let mut stdout = io::stdout();
    let _ = write!(stdout, "\x1b]52;c;{encoded}\x07");
    let _ = stdout.flush();
}

/// Standard base64, because a clipboard escape takes it and the workspace has no encoder.
fn base64(input: &[u8]) -> String {
    const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(input.len().div_ceil(3) * 4);
    for chunk in input.chunks(3) {
        let b = [
            chunk[0],
            chunk.get(1).copied().unwrap_or(0),
            chunk.get(2).copied().unwrap_or(0),
        ];
        let n = (u32::from(b[0]) << 16) | (u32::from(b[1]) << 8) | u32::from(b[2]);
        out.push(ALPHABET[(n >> 18) as usize & 63] as char);
        out.push(ALPHABET[(n >> 12) as usize & 63] as char);
        out.push(if chunk.len() > 1 {
            ALPHABET[(n >> 6) as usize & 63] as char
        } else {
            '='
        });
        out.push(if chunk.len() > 2 {
            ALPHABET[n as usize & 63] as char
        } else {
            '='
        });
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crossterm::event::{KeyModifiers, MouseButton, MouseEvent, MouseEventKind};
    use ratatui::backend::TestBackend;
    use vikram_contracts::{Block, Message, Role, Trust};
    use vikram_runtime::types::SessionSnapshot;
    use vikram_runtime::types::TurnState;
    use vikram_tui::app::SessionView;

    fn app_with_one_thought() -> App {
        let mut app = App::new(false);
        app.sessions
            .push(SessionView::from_snapshot(SessionSnapshot {
                id: SessionId::generate(),
                title: "Session 1".to_owned(),
                cwd: "/work".into(),
                model: None,
                messages: vec![Message {
                    role: Role::Assistant,
                    blocks: vec![
                        Block::Reasoning {
                            text: "The user said hello.".to_owned(),
                            trust: Trust::Model,
                        },
                        Block::Text {
                            text: "Hello!".to_owned(),
                            trust: Trust::Model,
                        },
                    ],
                }],
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

    fn draw(app: &mut App) -> Terminal<TestBackend> {
        let mut terminal = Terminal::new(TestBackend::new(80, 24)).expect("test terminal");
        terminal
            .draw(|frame| ui::draw(frame, app))
            .expect("the frame draws");
        terminal
    }

    #[test]
    fn configured_colour_reads_six_hex_digits_and_rejects_everything_else() {
        assert_eq!(
            configured_colour("ff8a00", "theme.accent"),
            Some(Color::Rgb(255, 138, 0))
        );
        assert_eq!(
            configured_colour(" #4da3ff ", "theme.accent"),
            Some(Color::Rgb(77, 163, 255))
        );
        assert_eq!(
            configured_colour("232528", "theme.background"),
            Some(Color::Rgb(35, 37, 40))
        );
        assert_eq!(configured_colour("ff8a0", "theme.accent"), None);
        assert_eq!(configured_colour("zzzzzz", "theme.accent"), None);
        assert_eq!(configured_colour("", "theme.background"), None);
    }

    #[test]
    fn a_click_on_a_drawn_thought_header_folds_that_block() {
        let mut app = app_with_one_thought();
        let _terminal = draw(&mut app);
        let hit = *app
            .thought_hits
            .first()
            .expect("the drawn frame records where the header landed");

        apply_terminal_event(
            &mut app,
            Event::Mouse(MouseEvent {
                kind: MouseEventKind::Down(MouseButton::Left),
                column: hit.x_start + 1,
                row: hit.row,
                modifiers: KeyModifiers::empty(),
            }),
        );

        let session = app.active_session().expect("a session is shown");
        assert!(
            !session.turn_folded(hit.key),
            "clicking the header opens exactly that block"
        );
        assert!(app.selection.is_none(), "a header click is not a drag");
    }

    /// A session holding one tool row. `live` gives the call a payload, which is how a row that is
    /// still running draws itself; otherwise the row comes from the result the turn already stored.
    fn app_with_one_tool_row(live: bool) -> App {
        use vikram_contracts::ToolCall;
        use vikram_tui::app::{Activity, ActivityKind, Payload};

        let call = ToolCall::new(
            "web.search",
            serde_json::json!({ "query": "today's top news headlines", "limit": 10 }),
        );
        let stored = if live {
            Vec::new()
        } else {
            vec![Message::tool_result(
                call.id.clone(),
                "1. a headline\n2. another headline",
                true,
            )]
        };
        let mut session = SessionView::from_snapshot(SessionSnapshot {
            id: SessionId::generate(),
            title: "Session 1".to_owned(),
            cwd: "/work".into(),
            model: None,
            messages: stored,
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
        });
        if live {
            session.payloads.insert(
                call.id.clone(),
                Payload {
                    name: "web.search".to_owned(),
                    resource: "web".to_owned(),
                    arguments: "{\"query\":\"today's top news headlines\"}".to_owned(),
                    content: "1. a headline\n2. another headline".to_owned(),
                    cwd: "/work".to_owned(),
                    duration_ms: 1400,
                },
            );
        }
        session.activity.push(Activity {
            kind: ActivityKind::Tool,
            text: "web.search query=today's top news headlines".to_owned(),
            call: Some(call.id.clone()),
            ok: Some(true),
            expanded: false,
        });
        let mut app = App::new(false);
        app.sessions.push(session);
        app
    }

    /// The last written column of a drawn row: a row that ends by saying `· click` puts the
    /// invitation there, so that is the column a click has to be understood at.
    fn end_of_row(terminal: &Terminal<TestBackend>, row: u16) -> u16 {
        let buffer = terminal.backend().buffer();
        (0..buffer.area.width)
            .rev()
            .find(|x| !buffer[(*x, row)].symbol().trim().is_empty())
            .expect("the row has content")
    }

    fn click(app: &mut App, column: u16, row: u16) {
        apply_terminal_event(
            app,
            Event::Mouse(MouseEvent {
                kind: MouseEventKind::Down(MouseButton::Left),
                column,
                row,
                modifiers: KeyModifiers::empty(),
            }),
        );
    }

    fn tool_row_hit(app: &App) -> vikram_tui::app::ThoughtHit {
        *app.thought_hits
            .iter()
            .find(|hit| hit.tool)
            .expect("the drawn frame records where the tool row landed")
    }

    #[test]
    fn a_click_at_the_end_of_a_running_tool_row_expands_it() {
        let mut app = app_with_one_tool_row(true);
        let terminal = draw(&mut app);
        let hit = tool_row_hit(&app);
        assert!(
            !app.sessions[0].tool_expanded(hit.key),
            "a row starts folded"
        );

        click(&mut app, end_of_row(&terminal, hit.row), hit.row);

        let session = app.active_session().expect("a session is shown");
        assert!(
            session.tool_expanded(hit.key),
            "the row says `· click` at its right-hand end, so a click there has to open it"
        );
        assert!(app.selection.is_none(), "a row click is not a drag");
    }

    #[test]
    fn a_click_at_the_end_of_a_stored_tool_row_expands_it() {
        let mut app = app_with_one_tool_row(false);
        let terminal = draw(&mut app);
        let hit = tool_row_hit(&app);
        assert!(
            !app.sessions[0].tool_expanded(hit.key),
            "a row starts folded"
        );

        click(&mut app, end_of_row(&terminal, hit.row), hit.row);

        let session = app.active_session().expect("a session is shown");
        assert!(
            session.tool_expanded(hit.key),
            "a stored row folds from anywhere the operator can see, not only from its label"
        );
        assert!(app.selection.is_none(), "a row click is not a drag");
    }

    #[test]
    fn a_click_on_the_tab_row_still_switches_sessions_and_does_not_fold() {
        let mut app = app_with_one_thought();
        let _terminal = draw(&mut app);
        let hit = app.tab_hits.first().cloned().expect("the tabs were drawn");
        let thought_rows: Vec<u16> = app.thought_hits.iter().map(|thought| thought.row).collect();
        assert!(
            !thought_rows.contains(&hit.row),
            "the tabs and the transcript must not claim the same row"
        );

        apply_terminal_event(
            &mut app,
            Event::Mouse(MouseEvent {
                kind: MouseEventKind::Down(MouseButton::Left),
                column: hit.tab_x_start + 1,
                row: hit.row,
                modifiers: KeyModifiers::empty(),
            }),
        );

        assert!(matches!(
            app.commands.as_slice(),
            [UiCommand::SwitchSession { .. }]
        ));
        let session = app.active_session().expect("a session is shown");
        assert!(
            !session.thought_expanded((0, 0)),
            "a tab click is not a fold"
        );
    }
}
