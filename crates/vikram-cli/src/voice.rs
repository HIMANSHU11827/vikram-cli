//! `vikram voice` — local voice I/O diagnostics and push-to-talk capture.
//!
//! This is a transport, not the assistant: `listen` prints a transcript and `say` speaks a string.
//! Neither sends anything to a model, so a missing microphone or helper cannot be mistaken for a
//! working voice round-trip.

use std::path::PathBuf;

use clap::Subcommand;
use vikram_voice::{VoiceConfig, VoiceError, VoiceSession, probe};

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
}
