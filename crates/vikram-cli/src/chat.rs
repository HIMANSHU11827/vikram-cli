//! Headless chat: one message in, the streamed turn out.
//!
//! Deliberately the same runtime and the same turn loop as the terminal interface, so the headless
//! path cannot silently diverge from the interactive one.

use std::io::Write;
use vikram_contracts::PermissionMode;
use vikram_runtime::Runtime;
use vikram_runtime::config::Config;
use vikram_runtime::types::{RuntimeEvent, TurnState};

pub async fn run(config: Config, message: String, fresh: bool) -> Result<(), String> {
    let runtime = Runtime::start(config)
        .await
        .map_err(|err| format!("could not start runtime: {err}"))?;
    let handle = runtime.handle();
    let snapshot = handle.snapshot();

    if let vikram_runtime::types::RuntimeStatus::Error(reason) = &snapshot.status {
        return Err(format!(
            "{reason}; run `vikram config set model <id>` and export the key"
        ));
    }

    let session = if fresh {
        handle
            .create_session(Some(session_title(&message)))
            .map_err(|err| err.to_string())?
    } else {
        snapshot
            .sessions
            .first()
            .map(|s| s.id.clone())
            .ok_or_else(|| "no session available".to_owned())?
    };

    let mut events = handle.events();

    // A pipe has no human at it. `ask` in a headless run would block forever, and answering it by
    // approving everything would be the runtime granting permission to itself, so a headless run
    // takes the refusing posture unless the operator chose another one with `--mode`.
    if handle.mode() == PermissionMode::Ask {
        handle
            .set_mode(&session, PermissionMode::Unattended)
            .map_err(|err| err.to_string())?;
        eprintln!("[vikram] headless: running unattended; anything needing approval is refused");
    }

    handle
        .submit(&session, message)
        .map_err(|err| err.to_string())?;

    let mut failed = false;
    let mut printed = false;

    loop {
        match events.recv().await {
            Ok(RuntimeEvent::StreamDelta {
                session: sid, text, ..
            }) if sid == session => {
                print!("{text}");
                let _ = std::io::stdout().flush();
                printed = true;
            }
            Ok(RuntimeEvent::ToolStarted {
                session: sid,
                name,
                summary,
                ..
            }) if sid == session => {
                eprintln!("\n[tool] {name} · {summary}");
            }
            Ok(RuntimeEvent::ToolFinished {
                session: sid,
                name,
                ok,
                summary,
                ..
            }) if sid == session => {
                eprintln!(
                    "[tool {}] {name} · {summary}",
                    if ok { "ok" } else { "failed" }
                );
            }
            Ok(RuntimeEvent::ApprovalRequested(prompt)) if prompt.session == session => {
                // Only reachable when the operator chose a prompting posture for a headless run.
                // Nothing is reading this, so the honest answer is no, never a silent yes.
                eprintln!(
                    "\n[denied: nobody to ask] {} {} — {}",
                    prompt.verb, prompt.resource, prompt.reason
                );
                let _ = handle.answer_approval(&prompt.approval, false, false);
            }
            Ok(RuntimeEvent::Error { text }) => {
                eprintln!("\nvikram: {text}");
                failed = true;
            }
            Ok(RuntimeEvent::Notice { text, .. }) => eprintln!("[notice] {text}"),
            Ok(RuntimeEvent::TurnFinished {
                session: sid,
                state,
                ..
            }) if sid == session => {
                if printed {
                    println!();
                }
                if !matches!(state, TurnState::Idle) && !matches!(state, TurnState::Cancelled) {
                    failed = true;
                }
                break;
            }
            Ok(RuntimeEvent::Shutdown) => break,
            Ok(_) => {}
            Err(err) => return Err(format!("event stream: {err}")),
        }
    }

    if failed {
        Err("turn did not complete".into())
    } else {
        Ok(())
    }
}

/// Title for a headless session, so the tab bar shows the request instead of a fixed word.
fn session_title(message: &str) -> String {
    const LIMIT: usize = 60;
    let collapsed = message.split_whitespace().collect::<Vec<_>>().join(" ");
    if collapsed.is_empty() {
        return "Session".into();
    }
    if collapsed.chars().count() <= LIMIT {
        return collapsed;
    }
    let clipped: String = collapsed.chars().take(LIMIT).collect();
    match clipped.rfind(' ') {
        Some(space) if space > LIMIT / 2 => format!("{}…", &clipped[..space]),
        _ => format!("{}…", clipped),
    }
}

#[cfg(test)]
mod tests {
    use super::session_title;

    #[test]
    fn a_headless_turn_is_titled_from_the_request_not_the_word_chat() {
        let title = session_title("Read the file Cargo.toml with the fs.read tool.");
        assert_eq!(title, "Read the file Cargo.toml with the fs.read tool.");
    }

    #[test]
    fn a_long_request_titles_to_one_bounded_line() {
        let title = session_title(
            "Search the whole workspace for every place we estimate tokens and then \
             explain     which of them are only budgeting and which are real billing",
        );
        assert!(title.chars().count() <= 60, "{title}");
        assert!(title.ends_with('…'), "a truncated title says so: {title}");
        assert!(!title.ends_with(' '), "no dangling space: {title}");
    }

    #[test]
    fn an_unusable_request_still_gets_a_session_name() {
        assert_eq!(session_title("   \n  "), "Session");
    }
}
