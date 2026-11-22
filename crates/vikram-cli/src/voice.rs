//! `vikram voice` — local voice I/O diagnostics and push-to-talk capture.
//!
//! This is a transport, not the assistant: `listen` prints a transcript and `say` speaks a string.
//! Neither sends anything to a model, so a missing microphone or helper cannot be mistaken for a
//! working voice round-trip.

use std::path::PathBuf;

use clap::Subcommand;
use vikram_voice::{VoiceConfig, VoiceSession, probe};

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
            match session.listen_once().map_err(|err| err.to_string())? {
                Some(text) => {
                    println!("{text}");
                    Ok(())
                }
                None => {
                    println!("no speech detected");
                    Ok(())
                }
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
            session.speak(&text).map_err(|err| err.to_string())
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
        assert_eq!(status.lines().len(), 5);
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
