//! Terminal lifecycle and the interactive event loop.
//!
//! Raw mode, the alternate screen and mouse capture are installed in one place and restored on
//! every exit path, including a panic. The loop never blocks the runtime: terminal input, runtime
//! observations and the UI tick are three independent sources.

use std::io::{IsTerminal, self, Stdout};
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
use vikram_contracts::SessionId;
use vikram_runtime::config::Config;
use vikram_runtime::{AgentControl, Runtime, RuntimeHandle};
use vikram_tui::app::{AgentAction, App, UiCommand};
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
        terminal
            .draw(|frame| ui::draw(frame, app))
            .map_err(|err| format!("draw: {err}"))?;

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
            MouseEventKind::ScrollUp => app.scroll_up(3),
            MouseEventKind::ScrollDown => app.scroll_down(3),
            MouseEventKind::Down(crossterm::event::MouseButton::Left) => {
                let x = mouse.column;
                // + New
                if let Some((start, end)) = app.new_session_x {
                    if x >= start && x < end {
                        app.commands.push(UiCommand::NewSession);
                        return;
                    }
                }
                // close buttons
                for hit in app.tab_hits.clone() {
                    if x >= hit.close_x_start && x < hit.close_x_end {
                        app.commands.push(UiCommand::CloseSession {
                            session: hit.session,
                        });
                        return;
                    }
                }
                // tab bodies
                for hit in app.tab_hits.clone() {
                    if x >= hit.tab_x_start
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
            // Run via the runtime's shell tool with user trust.
            let ctx = handle.tool_context();
            let tool = handle.tools().get("shell.run");
            match tool {
                Some(t) => match t.run(&serde_json::json!({"command": command}), &ctx) {
                    Ok(out) => {
                        if let Some(view) = app.session_by_id_mut(&session) {
                            view.messages
                                .push(vikram_contracts::Message::system(format!(
                                    "$ {}
{}",
                                    command, out.content
                                )));
                            view.follow = true;
                        }
                    }
                    Err(err) => report(
                        app,
                        vikram_runtime::RuntimeError::InvalidState(err.to_string()),
                    ),
                },
                None => report(
                    app,
                    vikram_runtime::RuntimeError::InvalidState("shell tool unavailable".into()),
                ),
            }
        }
        UiCommand::SlashCommand {
            session,
            command,
            args,
        } => {
            // Minimal command dispatch.
            match command.as_str() {
                "help" => {
                    if let Some(view) = app.session_by_id_mut(&session) {
                        view.messages.push(vikram_contracts::Message::system(
                            "Commands: /help /status /new /clear /exit /bot /files /tools /plugins \
                             /mcp /permissions /mode /allow"
                                .to_owned(),
                        ));
                        view.follow = true;
                    }
                }
                "bot" | "bots" => {
                    let args = args.clone();
                    if let Some(view) = app.session_by_id_mut(&session) {
                        if args == "list" || args.is_empty() {
                            let names: Vec<String> = handle
                                .agent_snapshots()
                                .iter()
                                .map(|a| a.profile.name.clone())
                                .collect();
                            view.messages
                                .push(vikram_contracts::Message::system(format!(
                                    "Bots: {}",
                                    if names.is_empty() {
                                        "none".to_owned()
                                    } else {
                                        names.join(", ")
                                    }
                                )));
                        } else {
                            view.messages
                                .push(vikram_contracts::Message::system(format!("bot: {}", args)));
                        }
                        view.follow = true;
                    }
                }
                "files" => {
                    if let Some(view) = app.session_by_id_mut(&session) {
                        view.messages.push(vikram_contracts::Message::system(
                            "Use @path to reference files, or /search <query>".to_owned(),
                        ));
                        view.follow = true;
                    }
                }
                "tools" => {
                    let names: Vec<String> = handle.tools().names().to_vec();
                    if let Some(view) = app.session_by_id_mut(&session) {
                        view.messages
                            .push(vikram_contracts::Message::system(format!(
                                "Tools: {}",
                                names.join(", ")
                            )));
                        view.follow = true;
                    }
                }
                "plugins" => {
                    let count = handle.snapshot().metrics.plugins_installed;
                    if let Some(view) = app.session_by_id_mut(&session) {
                        view.messages
                            .push(vikram_contracts::Message::system(format!(
                                "Plugins: {} installed",
                                count
                            )));
                        view.follow = true;
                    }
                }
                "mcp" => {
                    let snap = handle.snapshot();
                    let (conn, cfg) = (snap.metrics.mcp_connected, snap.metrics.mcp_configured);
                    if let Some(view) = app.session_by_id_mut(&session) {
                        view.messages
                            .push(vikram_contracts::Message::system(format!(
                                "MCP: {}/{} connected",
                                conn, cfg
                            )));
                        view.follow = true;
                    }
                }
                "mode" => {
                    let reply = if args.trim().is_empty() {
                        let active = handle.mode();
                        let names: Vec<String> = handle
                            .modes()
                            .iter()
                            .map(|m| {
                                if *m == active {
                                    format!("[{}]", m.as_str())
                                } else {
                                    m.as_str().to_owned()
                                }
                            })
                            .collect();
                        format!(
                            "mode: {} — /mode <name> to change. Available: {}",
                            active.as_str(),
                            names.join(", ")
                        )
                    } else {
                        match vikram_contracts::PermissionMode::parse(args.trim()) {
                            Some(mode) => match handle.set_mode(&session, mode) {
                                Ok(()) => format!(
                                    "mode: {mode} — this changes what the runtime does with a \
                                     prompt, never what the rules permit",
                                    mode = mode.as_str()
                                ),
                                Err(err) => err.to_string(),
                            },
                            None => format!(
                                "unknown mode `{}`; available: {}",
                                args.trim(),
                                handle
                                    .modes()
                                    .iter()
                                    .map(|m| m.as_str())
                                    .collect::<Vec<_>>()
                                    .join(", ")
                            ),
                        }
                    };
                    if let Some(view) = app.session_by_id_mut(&session) {
                        view.messages
                            .push(vikram_contracts::Message::system(reply));
                        view.follow = true;
                    }
                }
                "allow" => {
                    let reply = match args.trim() {
                        "" | "list" => {
                            let entries = handle.allow_list();
                            if entries.is_empty() {
                                "allow-list: empty. Answer a prompt with \"p\" to keep it, and it \
                                 appears here, or use /allow add <line>"
                                    .to_owned()
                            } else {
                                entries
                                    .iter()
                                    .enumerate()
                                    .map(|(i, r)| format!("{i}  {}", r.describe()))
                                    .collect::<Vec<_>>()
                                    .join("\n")
                            }
                        }
                        rest => match rest.split_once(" ") {
                            Some(("add", line)) => match handle.allow(&session, line.trim()) {
                                Ok(()) => format!("saved: {}", line.trim()),
                                Err(err) => err.to_string(),
                            },
                            Some(("rm", index)) => match index.trim().parse::<usize>() {
                                Ok(index) => match handle.unallow(index) {
                                    Ok(rule) => format!("removed {}", rule.describe()),
                                    Err(err) => err.to_string(),
                                },
                                Err(_) => "usage: /allow rm <number from /allow list>".to_owned(),
                            },
                            _ => "usage: /allow [list | add <line> | rm <number>]".to_owned(),
                        },
                    };
                    if let Some(view) = app.session_by_id_mut(&session) {
                        view.messages
                            .push(vikram_contracts::Message::system(reply));
                        view.follow = true;
                    }
                }
                "permissions" => {
                    if let Some(view) = app.session_by_id_mut(&session) {
                        view.messages.push(vikram_contracts::Message::system(format!(
                            "Permissions: deny-first policy with a mandatory floor nothing can \
                             widen. Current mode: {} ({} saved approval(s))",
                            handle.mode().as_str(),
                            handle.allow_list().len()
                        )));
                        view.follow = true;
                    }
                }
                "clear" => {
                    if let Some(view) = app.session_by_id_mut(&session) {
                        view.messages.clear();
                        view.follow = true;
                    }
                }
                "new" => {
                    app.commands.push(UiCommand::NewSession);
                }
                "exit" => {
                    app.commands.push(UiCommand::Quit);
                    app.should_quit = true;
                }
                "status" => {
                    let status_label = app.status.label().to_owned();
                    let agent_count = app.agents.len();
                    if let Some(view) = app.session_by_id_mut(&session) {
                        view.messages
                            .push(vikram_contracts::Message::system(format!(
                                "status: {} | agents: {}",
                                status_label, agent_count
                            )));
                        view.follow = true;
                    }
                }
                other => {
                    if let Some(view) = app.session_by_id_mut(&session) {
                        view.messages
                            .push(vikram_contracts::Message::system(format!(
                                "unknown command: /{} {}",
                                other, args
                            )));
                        view.follow = true;
                    }
                }
            }
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

/// Kept for tests that drive the loop with synthetic keys.
#[allow(dead_code)]
pub fn press(app: &mut App, key: KeyEvent) {
    input::handle_key(app, key);
}
