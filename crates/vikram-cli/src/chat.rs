//! Headless chat: one message in, the streamed turn out.
//!
//! Deliberately the same runtime and the same turn loop as the terminal interface, so the headless
//! path cannot silently diverge from the interactive one.

use std::io::Write;
use std::path::{Path, PathBuf};
use vikram_contracts::{ImageSource, Message, PermissionMode, SessionId};
use vikram_runtime::Runtime;
use vikram_runtime::RuntimeHandle;
use vikram_runtime::config::Config;
use vikram_runtime::types::{RuntimeEvent, TurnState};

pub async fn run(config: Config, message: String, fresh: bool) -> Result<(), String> {
    run_with_images(config, message, fresh, Vec::new()).await
}

/// The same headless turn, with images read from the local filesystem and attached to it.
pub async fn run_with_images(
    config: Config,
    message: String,
    fresh: bool,
    images: Vec<PathBuf>,
) -> Result<(), String> {
    // Read the attachments before the runtime is built: a path that cannot be read is the
    // operator's mistake, and it should cost no session and no turn.
    let sources = images
        .iter()
        .map(|path| load_image(path))
        .collect::<Result<Vec<_>, _>>()?;

    let runtime = Runtime::start(config)
        .await
        .map_err(|err| format!("could not start runtime: {err}"))?;
    let handle = runtime.handle();
    let session = open_session(&handle, &message, fresh)?;
    let mut events = handle.events();

    if sources.is_empty() {
        handle
            .submit(&session, message)
            .map_err(|err| err.to_string())?;
    } else {
        handle
            .submit_message(&session, Message::user_with_images(message, sources))
            .map_err(|err| err.to_string())?;
    }
    let failed = stream_turn(&handle, &session, &mut events).await?;

    settle_learning(&handle).await;
    if failed {
        Err("turn did not complete".into())
    } else {
        Ok(())
    }
}

/// The most one attachment may weigh. The encoded image is carried in the message, so it is paid
/// for on every later request of the conversation: an unbounded file is a bill nobody agreed to.
const MAX_IMAGE_BYTES: u64 = 10 * 1024 * 1024;

/// A file the operator named, as the message block the provider receives. Every failure names the
/// file, because "could not read" is useless when several were attached at once.
fn load_image(path: &Path) -> Result<ImageSource, String> {
    let size = std::fs::metadata(path)
        .map_err(|err| format!("image {}: {err}", path.display()))?
        .len();
    if size > MAX_IMAGE_BYTES {
        return Err(format!(
            "image {}: {size} bytes is over the {MAX_IMAGE_BYTES} byte limit",
            path.display()
        ));
    }
    let bytes = std::fs::read(path).map_err(|err| format!("image {}: {err}", path.display()))?;
    Ok(ImageSource::data(media_type(&bytes), base64(&bytes)))
}

/// The media type of an image, from its magic bytes. Bytes nothing recognises are labelled
/// `application/octet-stream` rather than guessed from the extension: the label is what the
/// provider is told the payload is.
fn media_type(bytes: &[u8]) -> &'static str {
    const PNG: &[u8] = &[0x89, b'P', b'N', b'G', 0x0d, 0x0a, 0x1a, 0x0a];
    if bytes.starts_with(PNG) {
        return "image/png";
    }
    if bytes.starts_with(&[0xff, 0xd8, 0xff]) {
        return "image/jpeg";
    }
    if bytes.starts_with(b"GIF87a") || bytes.starts_with(b"GIF89a") {
        return "image/gif";
    }
    if bytes.len() >= 12 && bytes.starts_with(b"RIFF") && &bytes[8..12] == b"WEBP" {
        return "image/webp";
    }
    "application/octet-stream"
}

/// Standard padded base64 (RFC 4648) of raw bytes. Written here rather than pulled in: the CLI has
/// no encoding dependency and this is the whole of what an inline image needs.
fn base64(bytes: &[u8]) -> String {
    const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(bytes.len().div_ceil(3) * 4);
    for chunk in bytes.chunks(3) {
        let b1 = u32::from(chunk[0]);
        let b2 = chunk.get(1).copied().map_or(0, u32::from);
        let b3 = chunk.get(2).copied().map_or(0, u32::from);
        let triple = (b1 << 16) | (b2 << 8) | b3;
        let digit = |shift: u32| ALPHABET[(triple >> shift) as usize & 0x3f] as char;
        out.push(digit(18));
        out.push(digit(12));
        out.push(if chunk.len() > 1 { digit(6) } else { '=' });
        out.push(if chunk.len() > 2 { digit(0) } else { '=' });
    }
    out
}

/// Multi-turn chat on one session: read a line, stream the turn, read the next.
///
/// This is the interactive counterpart to [`run`]; it uses the same runtime and the same turn loop,
/// so the two paths cannot diverge. Blank lines are ignored and never become turns.
pub async fn run_interactive(
    config: Config,
    first: Option<String>,
    fresh: bool,
) -> Result<(), String> {
    use tokio::io::{AsyncBufReadExt, BufReader};

    let runtime = Runtime::start(config)
        .await
        .map_err(|err| format!("could not start runtime: {err}"))?;
    let handle = runtime.handle();
    let title = first.as_deref().unwrap_or("Interactive");
    let mut session = open_session(&handle, title, fresh)?;
    let mut events = handle.events();

    eprintln!("[vikram] interactive session {session}; /help for commands, /exit to leave");

    if let Some(message) = first {
        handle
            .submit(&session, message)
            .map_err(|err| err.to_string())?;
        stream_turn(&handle, &session, &mut events).await?;
    }

    let mut lines = BufReader::new(tokio::io::stdin()).lines();
    loop {
        print!("vikram> ");
        let _ = std::io::stdout().flush();
        let Some(line) = lines
            .next_line()
            .await
            .map_err(|err| format!("stdin: {err}"))?
        else {
            println!();
            break;
        };
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        match parse_slash(line) {
            Some(Slash::Exit) => break,
            Some(Slash::Help) => {
                eprintln!(
                    "/new            start a fresh session\n\
                     /session <id>   continue an existing session\n\
                     /mode <name>    change the approval posture\n\
                     /exit           leave (same as Ctrl+D)"
                );
            }
            Some(Slash::New) => {
                session = handle.create_session(None).map_err(|err| err.to_string())?;
                eprintln!("[vikram] new session {session}");
            }
            Some(Slash::Switch(id)) => {
                let id = SessionId::new(id);
                if handle.snapshot().sessions.iter().any(|s| s.id == id) {
                    session = id;
                    eprintln!("[vikram] continuing {session}");
                } else {
                    eprintln!("[vikram] no such session: {session}");
                }
            }
            Some(Slash::Mode(name)) => match PermissionMode::parse(&name) {
                Some(mode) => {
                    handle
                        .set_mode(&session, mode)
                        .map_err(|err| err.to_string())?;
                    eprintln!("[vikram] mode is now {}", mode.as_str());
                }
                None => eprintln!("[vikram] unknown mode `{name}`"),
            },
            Some(Slash::Unknown(name)) => eprintln!("[vikram] unknown command `/{name}`"),
            None => {
                handle
                    .submit(&session, line.to_owned())
                    .map_err(|err| err.to_string())?;
                stream_turn(&handle, &session, &mut events).await?;
            }
        }
    }

    settle_learning(&handle).await;
    Ok(())
}

/// The one place the headless paths choose a session, so `--fresh` means the same thing in both.
fn open_session(handle: &RuntimeHandle, message: &str, fresh: bool) -> Result<SessionId, String> {
    let snapshot = handle.snapshot();
    if let vikram_runtime::types::RuntimeStatus::Error(reason) = &snapshot.status {
        return Err(format!(
            "{reason}; run `vikram config set model <id>` and export the key"
        ));
    }
    if fresh {
        handle
            .create_session(Some(session_title(message)))
            .map_err(|err| err.to_string())
    } else {
        snapshot
            .sessions
            .first()
            .map(|s| s.id.clone())
            .ok_or_else(|| "no session available".to_owned())
    }
}

/// Stream one turn to the terminal. Returns `true` when the turn failed.
async fn stream_turn(
    handle: &RuntimeHandle,
    session: &SessionId,
    events: &mut tokio::sync::broadcast::Receiver<RuntimeEvent>,
) -> Result<bool, String> {
    let mut failed = false;
    let mut printed = false;

    // A pipe has no human at it. `ask` in a headless run would block forever, and answering it by
    // approving everything would be the runtime granting permission to itself, so a headless run
    // takes the refusing posture unless the operator chose another one with `--mode`. The
    // interactive prompt has the same constraint: its answers are not wired to approvals.
    if handle.mode() == PermissionMode::Ask {
        handle
            .set_mode(session, PermissionMode::Unattended)
            .map_err(|err| err.to_string())?;
        eprintln!("[vikram] headless: running unattended; anything needing approval is refused");
    }

    loop {
        match events.recv().await {
            Ok(RuntimeEvent::StreamDelta {
                session: sid, text, ..
            }) if sid == *session => {
                print!("{text}");
                let _ = std::io::stdout().flush();
                printed = true;
            }
            Ok(RuntimeEvent::ToolStarted {
                session: sid,
                name,
                summary,
                ..
            }) if sid == *session => {
                eprintln!("\n[tool] {name} · {summary}");
            }
            Ok(RuntimeEvent::ToolFinished {
                session: sid,
                name,
                ok,
                summary,
                ..
            }) if sid == *session => {
                eprintln!(
                    "[tool {}] {name} · {summary}",
                    if ok { "ok" } else { "failed" }
                );
            }
            Ok(RuntimeEvent::ApprovalRequested(prompt)) if prompt.session == *session => {
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
            }) if sid == *session => {
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
    Ok(failed)
}

/// The turn is over, but the learning it started may still be running: it spent the tokens the
/// operator configured, so give it a bounded moment to land rather than dropping it on exit.
async fn settle_learning(handle: &RuntimeHandle) {
    if handle.learning_busy() {
        eprintln!("[vikram] finishing what this turn taught…");
        if !handle
            .wait_for_learning(std::time::Duration::from_secs(20))
            .await
        {
            eprintln!("[vikram] learning review did not settle in time");
        }
    }
}

/// A line the interactive prompt treats as an instruction to the CLI rather than to the model.
#[derive(Debug, PartialEq, Eq)]
enum Slash {
    Exit,
    Help,
    New,
    Switch(String),
    Mode(String),
    Unknown(String),
}

fn parse_slash(line: &str) -> Option<Slash> {
    let rest = line.strip_prefix('/')?;
    let mut parts = rest.split_whitespace();
    let name = parts.next().unwrap_or("").to_lowercase();
    let argument = parts.next().unwrap_or("").to_owned();
    Some(match name.as_str() {
        "exit" | "quit" | "q" => Slash::Exit,
        "help" | "?" => Slash::Help,
        "new" => Slash::New,
        "session" if !argument.is_empty() => Slash::Switch(argument),
        "mode" if !argument.is_empty() => Slash::Mode(argument),
        _ => Slash::Unknown(name),
    })
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
    use super::{MAX_IMAGE_BYTES, Slash, base64, load_image, media_type, parse_slash};
    use vikram_contracts::ImageSource;

    #[test]
    fn base64_is_standard_and_pads_the_tail() {
        assert_eq!(base64(b""), "");
        assert_eq!(base64(b"f"), "Zg==");
        assert_eq!(base64(b"fo"), "Zm8=");
        assert_eq!(base64(b"foo"), "Zm9v");
        assert_eq!(base64(b"foobar"), "Zm9vYmFy");
        assert_eq!(base64(&[0x00, 0xff, 0x10]), "AP8Q");
        assert_eq!(base64(&[0xfb, 0xff]), "+/8=");
        assert_eq!(
            base64(b"\x89PNG\r\n\x1a\n"),
            "iVBORw0KGgo=",
            "the bytes of a PNG signature encode to the published value"
        );
    }

    #[test]
    fn an_image_is_labelled_from_its_magic_bytes_not_its_name() {
        assert_eq!(media_type(b"\x89PNG\r\n\x1a\n rest"), "image/png");
        assert_eq!(media_type(&[0xff, 0xd8, 0xff, 0xe0, 0x00]), "image/jpeg");
        assert_eq!(media_type(b"GIF89a rest"), "image/gif");
        assert_eq!(media_type(b"GIF87a rest"), "image/gif");
        assert_eq!(media_type(b"RIFF\x24\x00\x00\x00WEBPVP8 "), "image/webp");
        assert_eq!(
            media_type(b"RIFF\x24\x00\x00\x00WAVEfmt "),
            "application/octet-stream",
            "a RIFF container that is not WebP is not an image"
        );
        assert_eq!(
            media_type(b"RIFF\x24\x00\x00\x00"),
            "application/octet-stream"
        );
        assert_eq!(media_type(b"plain text"), "application/octet-stream");
        assert_eq!(media_type(b""), "application/octet-stream");
    }

    #[test]
    fn a_readable_image_becomes_a_data_source_the_provider_can_decode() {
        let dir = tempfile::tempdir().expect("temp dir");
        let path = dir.path().join("screenshot.png");
        std::fs::write(&path, b"\x89PNG\r\n\x1a\n").expect("the file is writable");
        assert_eq!(
            load_image(&path).expect("a readable file is accepted"),
            ImageSource::data("image/png", "iVBORw0KGgo=")
        );
    }

    #[test]
    fn an_unusable_image_is_an_error_that_names_the_file() {
        let dir = tempfile::tempdir().expect("temp dir");
        let missing = dir.path().join("not-there.jpg");
        let refusal = load_image(&missing).expect_err("a missing file cannot be attached");
        assert!(refusal.contains("not-there.jpg"), "{refusal}");
        assert!(refusal.contains("image"), "{refusal}");

        let huge = dir.path().join("huge.gif");
        std::fs::write(&huge, vec![0u8; MAX_IMAGE_BYTES as usize + 1])
            .expect("the file is writable");
        let refusal = load_image(&huge).expect_err("an oversized file is refused");
        assert!(refusal.contains("huge.gif"), "{refusal}");
        assert!(refusal.contains("limit"), "{refusal}");
    }

    #[test]
    fn the_prompt_commands_parse_and_plain_text_does_not() {
        assert_eq!(parse_slash("/exit"), Some(Slash::Exit));
        assert_eq!(parse_slash("/quit"), Some(Slash::Exit));
        assert_eq!(parse_slash("/q"), Some(Slash::Exit));
        assert_eq!(parse_slash("/help"), Some(Slash::Help));
        assert_eq!(parse_slash("/new"), Some(Slash::New));
        assert_eq!(
            parse_slash("/session ses-1"),
            Some(Slash::Switch("ses-1".into()))
        );
        assert_eq!(parse_slash("/mode plan"), Some(Slash::Mode("plan".into())));
        assert_eq!(parse_slash("/mode"), Some(Slash::Unknown("mode".into())));
        assert_eq!(parse_slash("/wat"), Some(Slash::Unknown("wat".into())));
        assert_eq!(parse_slash("explain this repo"), None);
        assert_eq!(parse_slash(""), None);
        assert_eq!(parse_slash("/"), Some(Slash::Unknown(String::new())));
    }

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
