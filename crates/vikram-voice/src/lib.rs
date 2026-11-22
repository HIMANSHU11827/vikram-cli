//! Voice I/O for VIKRAM, built on the same helper shell-out pattern as `computer.*`.
//!
//! Phase 1 is push-to-talk: an operator invokes it explicitly, audio is captured as raw PCM,
//! silence is trimmed locally, a local Whisper helper transcribes it, and a local TTS helper
//! speaks the model's real reply. Every stage is optional and reports why it is unavailable;
//! nothing here pretends a missing microphone or model is a working voice.
//!
//! No audio library is linked. Helpers are spawned with an argv vector, never a shell, and every
//! invocation is bounded. Model weights are operator-downloaded files, never bundled.

mod helpers;
mod session;
mod vad;

pub use helpers::{first_available, missing_tool_message, probe, which};
pub use session::{
    AudioSource, HelperPlayer, HelperRecognizer, HelperSource, HelperSynth, Player, Recognizer,
    Synthesizer, VoiceSession,
};
pub use vad::{is_silent, trim_silence};

use std::path::PathBuf;
use thiserror::Error;

pub type Result<T> = std::result::Result<T, VoiceError>;

#[derive(Debug, Error)]
pub enum VoiceError {
    #[error("voice is disabled; set enabled = true for this session before opening audio devices")]
    Disabled,
    #[error("{0}")]
    MissingHelper(String),
    #[error("ASR model file is missing: {0}. Set asr_model to a downloaded ggml model.")]
    ModelMissing(PathBuf),
    #[error("voice helper failed: {0}")]
    Helper(String),
    #[error("voice helper timed out after {0}s")]
    Timeout(u64),
    #[error("invalid audio: {0}")]
    InvalidAudio(String),
    #[error("invalid voice configuration: {0}")]
    Config(String),
}

/// The knob set an operator can set without recompiling. Defaults describe what is probed, not
/// what is claimed to work: `enabled` is false until the operator opts in.
#[derive(Clone, Debug, PartialEq)]
pub struct VoiceConfig {
    pub enabled: bool,
    pub capture_helper: String,
    pub capture_fallback: String,
    pub asr_helper: String,
    pub asr_model: PathBuf,
    pub asr_timeout_secs: u64,
    pub tts_helper: String,
    pub tts_fallback: String,
    pub tts_voice: Option<PathBuf>,
    pub player_helper: String,
    pub player_fallback: String,
    pub max_record_ms: u64,
    pub sample_rate: u16,
    pub rms_threshold: f32,
    pub hangover_ms: u64,
}

impl Default for VoiceConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            capture_helper: "parecord".into(),
            capture_fallback: "arecord".into(),
            asr_helper: "whisper-cli".into(),
            asr_model: PathBuf::new(),
            asr_timeout_secs: 120,
            tts_helper: "piper".into(),
            tts_fallback: "espeak-ng".into(),
            tts_voice: None,
            player_helper: "paplay".into(),
            player_fallback: "aplay".into(),
            max_record_ms: 8_000,
            sample_rate: 16_000,
            rms_threshold: 0.01,
            hangover_ms: 200,
        }
    }
}

impl VoiceConfig {
    pub fn validate(&self) -> Result<()> {
        if self.max_record_ms == 0 {
            return Err(VoiceError::Config("max_record_ms must be positive".into()));
        }
        if self.asr_timeout_secs == 0 {
            return Err(VoiceError::Config(
                "asr_timeout_secs must be positive".into(),
            ));
        }
        if !(0.0..=1.0).contains(&self.rms_threshold) {
            return Err(VoiceError::Config(
                "rms_threshold must be between 0 and 1".into(),
            ));
        }
        if self.sample_rate == 0 {
            return Err(VoiceError::Config("sample_rate must be positive".into()));
        }
        Ok(())
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum StageStatus {
    Ready,
    Missing(String),
    Disabled(String),
}

impl StageStatus {
    pub fn is_ready(&self) -> bool {
        matches!(self, Self::Ready)
    }

    pub fn label(&self) -> String {
        match self {
            Self::Ready => "ready".into(),
            Self::Missing(reason) => format!("missing: {reason}"),
            Self::Disabled(reason) => format!("disabled: {reason}"),
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct VoiceStatus {
    pub enabled: bool,
    pub capture: StageStatus,
    pub asr: StageStatus,
    pub tts: StageStatus,
    pub playback: StageStatus,
}

impl VoiceStatus {
    pub fn is_ready(&self) -> bool {
        self.enabled
            && self.capture.is_ready()
            && self.asr.is_ready()
            && self.tts.is_ready()
            && self.playback.is_ready()
    }

    pub fn lines(&self) -> Vec<String> {
        vec![
            format!("enabled: {}", self.enabled),
            format!("capture:  {}", self.capture.label()),
            format!("asr:      {}", self.asr.label()),
            format!("tts:      {}", self.tts.label()),
            format!("playback: {}", self.playback.label()),
        ]
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_are_off_until_the_operator_opts_in() {
        let config = VoiceConfig::default();
        assert!(!config.enabled);
        config.validate().expect("defaults must be valid");
        let status = probe(&config);
        assert!(!status.is_ready());
        assert_eq!(status.lines().len(), 5);
    }

    #[test]
    fn bad_config_is_rejected_before_any_device_is_opened() {
        let config = VoiceConfig {
            max_record_ms: 0,
            ..Default::default()
        };
        assert!(config.validate().is_err());
        let config = VoiceConfig {
            rms_threshold: 1.5,
            ..Default::default()
        };
        assert!(config.validate().is_err());
    }

    #[test]
    fn stage_labels_name_the_problem() {
        assert_eq!(StageStatus::Ready.label(), "ready");
        assert_eq!(
            StageStatus::Missing("no whisper-cli".into()).label(),
            "missing: no whisper-cli"
        );
        assert_eq!(
            StageStatus::Disabled("operator opt-in".into()).label(),
            "disabled: operator opt-in"
        );
    }

    #[test]
    fn missing_helper_message_names_a_fix() {
        let text = missing_tool_message("parecord");
        assert!(text.contains("parecord"));
        assert!(text.contains("install"));
    }
}
