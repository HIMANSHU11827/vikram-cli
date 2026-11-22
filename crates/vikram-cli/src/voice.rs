//! `vikram voice` — local voice I/O diagnostics and push-to-talk capture.
//!
//! This is a transport, not the assistant: `listen` prints a transcript and `say` speaks a string.
//! Neither sends anything to a model, so a missing microphone or helper cannot be mistaken for a
//! working voice round-trip.

use std::path::PathBuf;

use clap::Subcommand;
use vikram_runtime::config::Config;
use vikram_voice::{VoiceConfig, VoiceError, VoiceSession, probe};

use crate::chat;

#[derive(Debug, Subcommand)]
pub enum VoiceAction {
    /// Report which local voice helpers and models are available.
    Status {
        /// Whisper model to check, in addition to VIKRAM_ASR_MODEL.
        #[arg(long, value_name = "PATH")]
        model: Option<PathBuf>,
    },
    /// Record one utterance locally and print its transcript.
    Listen {
        /// Open an audio device for this invocation. Voice I/O is opt-in.
        #[arg(long)]
        enable: bool,
        /// Path to a downloaded Whisper ggml model. Defaults to VIKRAM_ASR_MODEL.
        #[arg(long, value_name = "PATH")]
        model: Option<PathBuf>,
        /// Recording cap in milliseconds.
        #[arg(long, value_name = "MS")]
        max_record_ms: Option<u64>,
    },
    /// Speak a string through the local player.
    Say {
        /// Open an audio device for this invocation. Voice I/O is opt-in.
        #[arg(long)]
        enable: bool,
        /// Text to speak.
        text: Vec<String>,
        /// Piper voice model. Defaults to VIKRAM_TTS_VOICE.
        #[arg(long, value_name = "PATH")]
        voice: Option<PathBuf>,
    },
    /// Record one utterance, run it as a normal turn, and speak the reply.
    Chat {
        /// Open an audio device for this invocation. Voice I/O is opt-in.
        #[arg(long)]
        enable: bool,
        /// Path to a downloaded Whisper ggml model. Defaults to VIKRAM_ASR_MODEL.
        #[arg(long, value_name = "PATH")]
        model: Option<PathBuf>,
        /// Recording cap in milliseconds.
        #[arg(long, value_name = "MS")]
        max_record_ms: Option<u64>,
        /// Piper voice model for the spoken reply. Defaults to VIKRAM_TTS_VOICE.
        #[arg(long, value_name = "PATH")]
        voice: Option<PathBuf>,
        /// Start a new session instead of continuing the most recent one.
        #[arg(long)]
        fresh: bool,
    },
}

fn config(
    enable: bool,
    model: Option<PathBuf>,
    voice: Option<PathBuf>,
    max_record_ms: Option<u64>,
) -> VoiceConfig {
    let mut config = VoiceConfig {
        enabled: enable,
        ..Default::default()
    };
    if let Some(path) = model.or_else(|| std::env::var("VIKRAM_ASR_MODEL").ok().map(PathBuf::from))
    {
        config.asr_model = path;
    }
    if let Some(path) = voice.or_else(|| std::env::var("VIKRAM_TTS_VOICE").ok().map(PathBuf::from))
    {
        config.tts_voice = Some(path);
    }
    if let Some(ms) = max_record_ms {
        config.max_record_ms = ms;
    }
    config
}

/// SIGINT during a voice turn cancels the session instead of killing the process; when the turn
/// ends, the previous handler is restored. The handler itself only flips an atomic flag — the
/// watcher thread does the token flip, so nothing unsafe happens under the signal.
#[cfg(unix)]
mod interrupt {
    use std::sync::atomic::{AtomicBool, Ordering};
    use vikram_voice::CancelToken;

    static TRIGGERED: AtomicBool = AtomicBool::new(false);
    static STOP: AtomicBool = AtomicBool::new(false);

    unsafe extern "C" fn on_sigint(_: libc::c_int) {
        TRIGGERED.store(true, Ordering::SeqCst);
    }

    pub struct Guard {
        previous: libc::sighandler_t,
        watcher: Option<std::thread::JoinHandle<()>>,
    }

    impl Guard {
        pub fn install(token: CancelToken) -> Self {
            let previous =
                unsafe { libc::signal(libc::SIGINT, on_sigint as *const () as libc::sighandler_t) };
            TRIGGERED.store(false, Ordering::SeqCst);
            STOP.store(false, Ordering::SeqCst);
            let watcher = std::thread::spawn(move || {
                loop {
                    if STOP.load(Ordering::SeqCst) {
                        return;
                    }
                    if TRIGGERED.swap(false, Ordering::SeqCst) {
                        token.cancel();
                        return;
                    }
                    std::thread::sleep(std::time::Duration::from_millis(10));
                }
            });
            Self {
                previous,
                watcher: Some(watcher),
            }
        }
    }

    impl Drop for Guard {
        fn drop(&mut self) {
            unsafe { libc::signal(libc::SIGINT, self.previous) };
            STOP.store(true, Ordering::SeqCst);
            if let Some(watcher) = self.watcher.take() {
                let _ = watcher.join();
            }
        }
    }
}

fn report_notes(session: &mut VoiceSession) {
    for note in session.status().notes {
        eprintln!("voice: {note}");
    }
}

/// The most of a reply the local synthesizer is asked to read. A turn can produce pages; reading
/// all of them buries the answer in narration, so the voice stops at the last sentence inside the
/// budget and the rest stays on screen.
const SPOKEN_MAX_BYTES: usize = 1_200;

/// The part of `reply` worth hearing: whole and under budget, cut at a sentence end when it has to
/// be cut at all.
fn speakable(reply: &str) -> &str {
    if reply.len() <= SPOKEN_MAX_BYTES {
        return reply.trim();
    }
    let mut end = SPOKEN_MAX_BYTES;
    while end > 0 && !reply.is_char_boundary(end) {
        end -= 1;
    }
    let slice = &reply[..end];
    match slice.rfind(['.', '!', '?', '\n']) {
        Some(pos) => &slice[..=pos],
        None => slice.trim(),
    }
}

/// A spoken turn: record one utterance, send its transcript through the same headless turn `vikram
/// chat` runs, then speak the reply. Each stage is honest about failing — no speech stops the turn
/// before it is sent, and a synthesizer that cannot run leaves the answer on screen instead of
/// failing the command.
pub async fn chat(runtime_config: Config, action: VoiceAction) -> Result<(), String> {
    let VoiceAction::Chat {
        enable,
        model,
        max_record_ms,
        voice,
        fresh,
    } = action
    else {
        return Err("not a voice chat action".into());
    };
    let voice_config = config(enable, model, voice, max_record_ms);
    voice_config.validate().map_err(|err| err.to_string())?;
    let mut session = VoiceSession::start(voice_config).map_err(|err| err.to_string())?;

    eprintln!("voice: listening — speak, then pause");
    let recording = {
        #[cfg(unix)]
        let guard = interrupt::Guard::install(session.cancel_token());
        let result = session.listen_once();
        #[cfg(unix)]
        drop(guard);
        result
    };
    let heard = match recording {
        Err(VoiceError::Cancelled) => {
            println!("voice: cancelled");
            return Ok(());
        }
        Err(err) => return Err(err.to_string()),
        Ok(None) => {
            report_notes(&mut session);
            println!("no speech detected");
            return Ok(());
        }
        Ok(Some(text)) => text,
    };
    if heard.trim().is_empty() {
        println!("no speech detected");
        return Ok(());
    }
    println!("you: {heard}");

    let reply = chat::run_capture(runtime_config, heard, fresh).await?;
    let spoken = speakable(&reply);
    if spoken.is_empty() {
        return Ok(());
    }
    if spoken.len() < reply.trim().len() {
        eprintln!(
            "voice: speaking the first {} bytes of the reply",
            spoken.len()
        );
    }
    if let Err(err) = session.speak(spoken) {
        report_notes(&mut session);
        eprintln!("voice: reply not spoken: {err}");
    }
    Ok(())
}

pub fn run(action: VoiceAction) -> Result<(), String> {
    match action {
        VoiceAction::Status { model } => {
            let config = config(false, model, None, None);
            let status = probe(&config);
            for line in status.lines() {
                println!("{line}");
            }
            if !status.enabled {
                println!("voice I/O is off; pass --enable to `voice listen` or `voice say`");
            }
            if !status.is_ready() && status.enabled {
                println!("not all stages are ready; fix the lines above before using voice");
            }
            Ok(())
        }
        VoiceAction::Listen {
            enable,
            model,
            max_record_ms,
        } => {
            let config = config(enable, model, None, max_record_ms);
            config.validate().map_err(|err| err.to_string())?;
            let mut session = VoiceSession::start(config).map_err(|err| err.to_string())?;
            #[cfg(unix)]
            let guard = interrupt::Guard::install(session.cancel_token());
            let result = session.listen_once();
            #[cfg(unix)]
            drop(guard);
            match result {
                Ok(Some(text)) => {
                    report_notes(&mut session);
                    println!("{text}");
                    Ok(())
                }
                Ok(None) => {
                    report_notes(&mut session);
                    println!("no speech detected");
                    Ok(())
                }
                Err(VoiceError::Cancelled) => {
                    println!("voice: cancelled");
                    Ok(())
                }
                Err(err) => Err(err.to_string()),
            }
        }
        VoiceAction::Say {
            enable,
            text,
            voice,
        } => {
            let text = text.join(" ");
            if text.trim().is_empty() {
                return Err("no text given".into());
            }
            let config = config(enable, None, voice, None);
            config.validate().map_err(|err| err.to_string())?;
            let mut session = VoiceSession::start(config).map_err(|err| err.to_string())?;
            #[cfg(unix)]
            let guard = interrupt::Guard::install(session.cancel_token());
            let result = session.speak(&text);
            #[cfg(unix)]
            drop(guard);
            if let Err(VoiceError::Cancelled) = result {
                println!("voice: cancelled");
                return Ok(());
            }
            report_notes(&mut session);
            result.map_err(|err| err.to_string())
        }
        VoiceAction::Chat { .. } => Err("voice chat runs through the async path".into()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn status_reports_every_stage_without_opening_devices() {
        let config = config(false, None, None, None);
        let status = probe(&config);
        assert!(status.lines().len() >= 5);
        assert!(!status.is_ready());
        assert!(!config.enabled);
    }

    #[test]
    fn explicit_model_and_voice_override_defaults() {
        let config = config(
            true,
            Some(PathBuf::from("model.bin")),
            Some(PathBuf::from("voice.onnx")),
            Some(4_000),
        );
        assert!(config.enabled);
        assert_eq!(config.asr_model, PathBuf::from("model.bin"));
        assert_eq!(config.tts_voice, Some(PathBuf::from("voice.onnx")));
        assert_eq!(config.max_record_ms, 4_000);
    }

    #[test]
    fn short_replies_are_spoken_whole() {
        let reply = "  The build passed.  ";
        assert_eq!(speakable(reply), "The build passed.");
    }

    #[test]
    fn long_replies_stop_at_the_last_sentence_inside_the_budget() {
        let sentence = "This sentence fills a fair share of the spoken budget. ";
        let reply = sentence.repeat(40);
        let spoken = speakable(&reply);
        assert!(spoken.len() <= SPOKEN_MAX_BYTES);
        assert!(spoken.ends_with('.'));
        assert!(reply.starts_with(spoken));
    }

    #[test]
    fn a_budget_cut_that_lands_mid_character_still_yields_valid_text() {
        let reply = "é".repeat(SPOKEN_MAX_BYTES);
        let spoken = speakable(&reply);
        assert!(spoken.len() <= SPOKEN_MAX_BYTES);
        assert!(spoken.chars().all(|c| c == 'é'));
    }
}
