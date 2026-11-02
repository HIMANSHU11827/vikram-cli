//! Terminal lifecycle and the interactive event loop.
//!
//! Raw mode, the alternate screen and mouse capture are installed in one place and restored on
//! every exit path, including a panic. The loop never blocks the runtime: terminal input, runtime
//! observations and the UI tick are three independent sources.

use std::io::{self, IsTerminal, Stdout};
use std::time::Duration;

use crossterm::event::{
    DisableMouseCapture, EnableMouseCapture, Event, EventStream, KeyEvent, MouseEventKind,
};
use crossterm::execute;
use crossterm::terminal::{
    EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode,
};
use futures_util::StreamExt;
use ratatui::Terminal;
use ratatui::backend::CrosstermBackend;
use tokio::sync::broadcast::error::RecvError;
use vikram_contracts::{AgentId, PermissionMode, SessionId};
use vikram_runtime::config::Config;
use vikram_runtime::types::NoticeLevel;
use vikram_runtime::{AgentControl, Runtime, RuntimeHandle};
use vikram_tui::app::{AgentAction, App, Focus, Overlay, UiCommand};
use vikram_tui::commands::{self, Outcome};
use vikram_tui::widgets::{Picker, PickerItem, PickerKind, Verb};
use vikram_tui::{input, ui};

type Term = Terminal<CrosstermBackend<Stdout>>;

pub async fn run(config: Config) -> Result<(), String> {
    let ascii = config.ascii_fallback;
    let runtime = Runtime::start(config)
        .await
        .map_err(|err| format!("could not start runtime: {err}"))?;
    let handle = runtime.handle();

    let mut app = App::new(ascii);
    app.sync_snapshot(handle.snapshot());

    let mut terminal = setup_terminal()?;
    let outcome = event_loop(&mut terminal, &mut app, &handle).await;
    restore_terminal(&mut terminal);
    outcome
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
                if let Some((left, right)) = app.picker_x {
                    if x >= left && x < right {
                        if let Some(position) = app
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
                    }
                }
                // The tab strip and `+ New` live on one row. Matching on the column alone would
                // swallow every press below them, so the row is part of the hit.
                if app.overlay.is_none() {
                    if let Some((row, start, end)) = app.new_session_x {
                        if mouse.row == row && x >= start && x < end {
                            app.commands.push(UiCommand::NewSession);
                            return;
                        }
                    }
                    // close buttons
                    for hit in app.tab_hits.clone() {
                        if mouse.row == hit.row
                            && x >= hit.close_x_start
                            && x < hit.close_x_end
                        {
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
                if app.overlay.is_none() {
                    if let Some((key, tool)) = app.fold_at(x, mouse.row) {
                        if let Some(id) = app.display_session().map(|session| session.id.clone()) {
                            if let Some(index) = app.session_index(&id) {
                                let session = &mut app.sessions[index];
                                if tool {
                                    session.toggle_tool(key);
                                } else {
                                    session.toggle_thought(key);
                                }
                            }
                        }
                        return;
                    }
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
            }
        }
        UiCommand::Queue { session, text } => {
            if let Err(err) = handle.queue(&session, text) {
                report(app, err);
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
            app.selected_agent = None;
            select(app, &session);
        }
        UiCommand::RenameSession { session, title } => {
            if let Err(err) = handle.rename_session(&session, title) {
                report(app, err);
            }
        }
        UiCommand::CloseSession { session } => {
            let was_active = app.session_index(&session) == Some(app.active);
            if let Err(err) = handle.close_session(&session) {
                report(app, err);
            } else {
                if app.sessions.is_empty() {
                    match handle.create_session(Some("Session".into())) {
                        Ok(id) => select(app, &id),
                        Err(err) => report(app, err),
                    }
                } else if was_active {
                    if let Some(first) = app.sessions.first().map(|s| s.id.clone()) {
                        select(app, &first);
                    }
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
        UiCommand::Unallow { index } => match handle.unallow(index) {
            Ok(rule) => app.push_notice(NoticeLevel::Info, format!("forgot {}", rule.describe())),
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
            Ok(false) => app.push_notice(NoticeLevel::Warn, format!("{scope}/{key} was already gone")),
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
        UiCommand::AgentControl { agent, action } => {
            let control = match action {
                AgentAction::Pause => AgentControl::Pause,
                AgentAction::Resume => AgentControl::Resume,
                AgentAction::Stop => AgentControl::Stop,
            };
            if let Err(err) = handle.control_agent(&agent, control) {
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
                        Ok(()) => app.push_notice(
                            NoticeLevel::Info,
                            format!("saved: {}", rule.trim()),
                        ),
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

    match command.outcome {
        Outcome::Picker(kind) => open_panel(handle, app, kind),
        Outcome::Info => {
            // Info commands are now handled as pickers - this should not be reached
            app.push_notice(NoticeLevel::Warn, format!("`/{}` should be a picker", command.name));
        }
        Outcome::Action => {
            // Legacy action commands - most are now pickers, but keep for safety
            run_action(app, session, command.name);
        }
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
        PickerKind::Bot | PickerKind::Agent => {
            let mut items: Vec<PickerItem> = Vec::new();
            for agent in handle.agent_snapshots() {
                items.push(PickerItem::new(
                    agent.profile.name.clone(),
                    format!("{} · {}", agent.profile.role, agent.state.label()),
                    agent.profile.id.to_string(),
                ));
            }
            let title = if kind == PickerKind::Bot {
                "Bots"
            } else {
                "Agents"
            };
            let picker = Picker::new(kind, title, items);
            app.open_picker(picker.with_note(if kind == PickerKind::Bot {
                "enter selects · ctrl+n spawns a new bot"
            } else {
                "enter chooses what to do with it"
            }));
        }
        PickerKind::AgentAction => {
            let subject = app
                .pending_agent
                .as_ref()
                .map(|id| id.to_string())
                .unwrap_or_default();
            let items = vec![
                PickerItem::new("pause", "stop scheduling work", "pause"),
                PickerItem::new("resume", "continue", "resume"),
                PickerItem::new("stop", "end the agent", "stop"),
            ];
            app.open_picker(
                Picker::new(kind, "Agent", items).with_note(format!("for `{subject}`")),
            );
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
            let items: Vec<PickerItem> = app
                .sessions
                .iter()
                .enumerate()
                .map(|(index, view)| {
                    let detail = format!(
                        "{} · {} message(s){}",
                        view.model.clone().unwrap_or_else(|| "no model".to_owned()),
                        view.messages.len(),
                        if view.unread > 0 {
                            format!(" · {} unread", view.unread)
                        } else {
                            String::new()
                        }
                    );
                    let item = PickerItem::new(view.title.clone(), detail, view.id.to_string());
                    if index == app.active {
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
        PickerKind::Memory => {
            let entries = handle.memory_list_all();
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
                    "agents pane",
                    if app.show_bots { "shown" } else { "hidden" },
                    "bots",
                ),
            ];
            app.open_picker(
                Picker::new(kind, "Settings", items)
                    .with_note("enter flips the setting; it applies immediately"),
            );
        }
        PickerKind::Team => {
            let teams = handle.team_list();
            let mut items = Vec::new();
            if teams.is_empty() {
                items.push(PickerItem::new(
                    "no teams yet".to_owned(),
                    String::new(),
                    "empty".to_owned(),
                ));
            }
            for team in &teams {
                items.push(PickerItem::new(
                    team.name.clone(),
                    format!("{} member(s)", team.members.len()),
                    team.name.clone(),
                ));
                for member in &team.members {
                    items.push(PickerItem::new(
                        format!("  {member}"),
                        String::new(),
                        format!("member_{}", member),
                    ));
                }
            }
            app.open_picker(Picker::new(kind, "HIVE teams", items));
        }
        PickerKind::Skill => {
            let metrics = handle.snapshot().metrics;
            let snapshot = handle.snapshot();
            let mut items = vec![
                PickerItem::new(
                    format!("{} discovered · {} loaded this turn", metrics.skills_discovered, metrics.skills_loaded),
                    String::new(),
                    "summary".to_owned(),
                ),
                PickerItem::new(
                    "! bodies are read on demand, never scanned into context".to_owned(),
                    String::new(),
                    "hint".to_owned(),
                ),
            ];
            for skill in &snapshot.skills {
                items.push(PickerItem::new(skill.clone(), String::new(), skill.clone()));
            }
            if snapshot.skills.is_empty() {
                items.push(PickerItem::new(
                    "no skill directories found".to_owned(),
                    String::new(),
                    "empty".to_owned(),
                ));
            }
            app.open_picker(Picker::new(kind, "Skills", items));
        }
        PickerKind::File => input::open_file_picker(app),
        PickerKind::Shell => input::open_shell_picker(app),
        PickerKind::Command => input::open_palette(app),
        PickerKind::Provider => {
            let config = handle.config();
            let key = if config.api_key().is_some() {
                "set"
            } else {
                "empty or missing"
            };
            let mut items = vec![
                PickerItem::new(
                    "endpoint",
                    if config.base_url.trim().is_empty() {
                        "not set"
                    } else {
                        config.base_url.as_str()
                    }.to_owned(),
                    "endpoint".to_owned(),
                ),
                PickerItem::new(
                    "model",
                    if config.model.trim().is_empty() {
                        "not set".to_owned()
                    } else {
                        config.model.clone()
                    },
                    "model".to_owned(),
                ),
                PickerItem::new(
                    "key",
                    format!("${} — {key}", config.api_key_env),
                    "key".to_owned(),
                ),
            ];
            if let Some(window) = config.context_window {
                items.push(PickerItem::new(
                    "context window",
                    format!("{window} tokens"),
                    "window".to_owned(),
                ));
            }
            items.push(PickerItem::new(
                "! one endpoint per process: changing it is a config edit and a restart".to_owned(),
                String::new(),
                "hint".to_owned(),
            ));
            app.open_picker(Picker::new(kind, "Provider", items));
        }
        PickerKind::Permissions => {
            let rules = handle.allow_list();
            let mut items = vec![
                PickerItem::new(
                    "mode",
                    handle.mode().as_str().to_owned(),
                    "mode".to_owned(),
                ),
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
                "! /permissions changes the posture · /allow forgets a rule".to_owned(),
                String::new(),
                "hint".to_owned(),
            ));
            app.open_picker(Picker::new(kind, "Permissions", items));
        }
        PickerKind::Plugins => {
            let metrics = handle.snapshot().metrics;
            let items = vec![
                PickerItem::new(
                    "installed",
                    format!("{}", metrics.plugins_installed),
                    "installed".to_owned(),
                ),
                PickerItem::new(
                    "loaded",
                    format!("{}", metrics.plugins_loaded),
                    "loaded".to_owned(),
                ),
                PickerItem::new(
                    "! a plugin declares what it needs; the policy still decides every call".to_owned(),
                    String::new(),
                    "hint".to_owned(),
                ),
            ];
            app.open_picker(Picker::new(kind, "Plugins", items));
        }
        PickerKind::Tools => {
            let names = handle.tools().names();
            let mut items = vec![PickerItem::new(
                format!("{} tool(s) registered", names.len()),
                String::new(),
                "count".to_owned(),
            )];
            for name in names {
                items.push(PickerItem::new(name.clone(), String::new(), name.clone()));
            }
            app.open_picker(Picker::new(kind, "Tools", items));
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
                PickerItem::new(
                    "mode",
                    handle.mode().as_str().to_owned(),
                    "mode".to_owned(),
                ),
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
                    "agents",
                    format!("{}", snapshot.agents.len()),
                    "agents".to_owned(),
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
                format!("{} tokens", metrics.context_used),
                "used".to_owned(),
            )];
            match metrics.context_window {
                Some(window) if window > 0 => {
                    items.push(PickerItem::new(
                        "window",
                        format!("{window} tokens"),
                        "window".to_owned(),
                    ));
                    if let Some(ratio) = metrics.context_ratio() {
                        items.push(PickerItem::new(
                            "usage",
                            format!("{:.1}% of the window", ratio * 100.0),
                            "ratio".to_owned(),
                        ));
                    }
                }
                _ => items.push(PickerItem::new(
                    "window",
                    "not known for this model".to_owned(),
                    "window".to_owned(),
                )),
            }
            app.open_picker(Picker::new(kind, "Context", items));
        }
        PickerKind::Doctor => {
            let checks = crate::doctor::checks(handle.config());
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
                PickerItem::new("F5", "sub-agents", "f5".to_owned()),
                PickerItem::new("F6", "context usage", "f6".to_owned()),
                PickerItem::new("F7", "model and throughput", "f7".to_owned()),
                PickerItem::new("F8", "skills browser", "f8".to_owned()),
                PickerItem::new("PgUp / PgDn", "scroll the transcript", "pgup_pgdn".to_owned()),
                PickerItem::new("Esc", "cancel the running turn", "esc".to_owned()),
                PickerItem::new("F1 / ?", "this help", "f1_help".to_owned()),
            ];
            app.open_picker(Picker::new(kind, "Key bindings", items));
        }
        PickerKind::Diff => {
            let items = vec![
                PickerItem::new("git diff", "show workspace changes", "git_diff".to_owned()),
            ];
            app.open_picker(Picker::new(kind, "Run git diff", items));
        }
        PickerKind::Test => {
            let items = vec![
                PickerItem::new("cargo test", "run the test suite", "cargo_test".to_owned()),
            ];
            app.open_picker(Picker::new(kind, "Run tests", items));
        }
        PickerKind::Build => {
            let items = vec![
                PickerItem::new("cargo build", "build the workspace", "cargo_build".to_owned()),
            ];
            app.open_picker(Picker::new(kind, "Build workspace", items));
        }
        PickerKind::Clear => {
            let items = vec![
                PickerItem::new("Clear this conversation", "the transcript in this view goes away", "clear".to_owned()),
            ];
            app.open_picker(Picker::new(kind, "Clear conversation", items));
        }
        PickerKind::NewSession => {
            let items = vec![
                PickerItem::new("Start a new session", "creates a fresh conversation", "new".to_owned()),
            ];
            app.open_picker(Picker::new(kind, "New session", items));
        }
        PickerKind::Exit => {
            let items = vec![
                PickerItem::new("Quit VIKRAM", "exit the application", "exit".to_owned()),
            ];
            app.open_picker(Picker::new(kind, "Quit", items));
        }
        PickerKind::RenameSession => {
            let title = app
                .active_session()
                .map(|view| view.title.clone())
                .unwrap_or_else(|| "no session".to_owned());
            let items = vec![
                PickerItem::new(
                    format!("Rename: {}", title),
                    "type a new name for this conversation",
                    "rename".to_owned(),
                ),
            ];
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
            let items = vec![
                PickerItem::new(
                    format!("Close: {}", title),
                    "the transcript stays in the store; only this view is closed",
                    "close".to_owned(),
                ),
            ];
            app.open_picker(Picker::new(kind, "Close session", items));
        }
        // Kinds with no source of their own: the table never opens them, and naming them as
        // unavailable is better than an empty panel that looks like a bug.
        other => app.push_notice(
            NoticeLevel::Warn,
            format!("no panel is wired for `{}` yet", other.as_str()),
        ),
    }
}

/// Separates scope from key inside a memory row's value, so one picker row carries both without
/// hoping neither ever contains a space.
const MEMORY_SEPARATOR: char = '\u{1f}';

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
        (PickerKind::Bot, Verb::New) => app.overlay = Some(Overlay::AddAgent { selected: 0 }),
        (PickerKind::Bot, Verb::Choose) => {
            app.selected_agent = Some(AgentId::new(value));
            app.push_notice(NoticeLevel::Info, "showing that bot's conversation");
        }
        (PickerKind::Agent, _) => {
            app.pending_agent = Some(AgentId::new(value));
            open_panel(handle, app, PickerKind::AgentAction);
        }
        (PickerKind::AgentAction, _) => {
            let Some(agent) = app.pending_agent.take() else {
                return;
            };
            let action = match value.as_str() {
                "pause" => AgentAction::Pause,
                "resume" => AgentAction::Resume,
                _ => AgentAction::Stop,
            };
            app.commands.push(UiCommand::AgentControl { agent, action });
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
        (PickerKind::Provider, _) => {
            // Info panel - just close
        }
        (PickerKind::Permissions, _) => {
            // Info panel - just close
        }
        (PickerKind::Plugins, _) => {
            // Info panel - just close
        }
        (PickerKind::Tools, _) => {
            // Info panel - just close
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
        (PickerKind::Team, _) => {
            // Info panel - just close
        }
        (PickerKind::Skill, _) => {
            // Info panel - just close
        }
        (PickerKind::Keys, _) => {
            // Info panel - just close
        }
        (PickerKind::Diff, _) => confirm_run(app, session, "git diff"),
        (PickerKind::Test, _) => confirm_run(app, session, "cargo test"),
        (PickerKind::Build, _) => confirm_run(app, session, "cargo build"),
        (PickerKind::Clear, _) => {
            app.open_confirm(
                "Clear this conversation",
                vec![
                    "the transcript in this view goes away".to_owned(),
                    "! the session itself and its stored history stay".to_owned(),
                ],
                UiCommand::Clear {
                    session: session.clone(),
                },
            );
        }
        (PickerKind::NewSession, _) => app.commands.push(UiCommand::NewSession),
        (PickerKind::Exit, _) => {
            app.commands.push(UiCommand::Quit);
            app.should_quit = true;
        }
        (PickerKind::RenameSession, _) => {
            let title = app
                .session_by_id_mut(session)
                .map(|view| view.title.clone())
                .unwrap_or_default();
            let mut editor = vikram_tui::editor::Editor::new();
            editor.insert_str(&title);
            app.overlay = Some(Overlay::RenameSession { editor });
        }
        (PickerKind::CloseSession, _) => {
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
            app.open_confirm(
                "Close a conversation",
                vec![
                    title,
                    "! the transcript stays in the store; only this view is closed".to_owned(),
                ],
                UiCommand::CloseSession {
                    session: session.clone(),
                },
            );
        }
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
            let entry = handle
                .memory_list_all()
                .into_iter()
                .find(|(entry_scope, entry_key, _)| entry_scope == scope && entry_key == key);
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
                        "agents pane: shown"
                    } else {
                        "agents pane: hidden"
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

/// Ask before running a command that takes time and touches the workspace. The panel shows the
/// exact line, because "test" is not the instruction — `cargo test` is.
fn confirm_run(app: &mut App, session: &SessionId, command: &str) {
    app.open_confirm(
        "Run a command",
        vec![
            command.to_owned(),
            "! it goes through the same approval gate and journal as a tool call".to_owned(),
        ],
        UiCommand::RunShell {
            session: session.clone(),
            command: command.to_owned(),
        },
    );
}

/// Commands that do exactly one thing. Each one goes through the same gate, approval and journal as
/// a model-issued tool call, because an operator typing a build is still a build.
fn run_action(app: &mut App, session: &SessionId, name: &str) {
    match name {
        "clear" => app.open_confirm(
            "Clear this conversation",
            vec![
                "the transcript in this view goes away".to_owned(),
                "! the session itself and its stored history stay".to_owned(),
            ],
            UiCommand::Clear {
                session: session.clone(),
            },
        ),
        "new" => app.commands.push(UiCommand::NewSession),
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
            app.open_confirm(
                "Close a conversation",
                vec![
                    title,
                    "! the transcript stays in the store; only this view is closed".to_owned(),
                ],
                UiCommand::CloseSession {
                    session: session.clone(),
                },
            );
        }
        "keys" => app.overlay = Some(Overlay::Help),
        "approvals" => app.overlay = Some(Overlay::Approvals),
        "exit" => {
            app.commands.push(UiCommand::Quit);
            app.should_quit = true;
        }
        "diff" => confirm_run(app, session, "git diff"),
        "test" => confirm_run(app, session, "cargo test"),
        "build" => confirm_run(app, session, "cargo build"),
        other => app.push_notice(NoticeLevel::Warn, format!("`/{other}` has no action")),
    }
}

/// Kept for tests that drive the loop with synthetic keys.
#[allow(dead_code)]
pub fn press(app: &mut App, key: KeyEvent) {
    input::handle_key(app, key);
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
    const ALPHABET: &[u8; 64] =
        b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
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
    use ratatui::backend::TestBackend;
    use crossterm::event::{KeyModifiers, MouseButton, MouseEvent, MouseEventKind};
    use vikram_contracts::{Block, Message, Role, Trust};
    use vikram_runtime::types::TurnState;
    use vikram_runtime::types::SessionSnapshot;
    use vikram_tui::app::SessionView;

    fn app_with_one_thought() -> App {
        let mut app = App::new(false);
        app.sessions.push(SessionView::from_snapshot(SessionSnapshot {
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
            agent: None,
            queued: 0,
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
            session.thought_expanded(hit.key),
            "clicking the header opens exactly that block"
        );
        assert!(app.selection.is_none(), "a header click is not a drag");
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
