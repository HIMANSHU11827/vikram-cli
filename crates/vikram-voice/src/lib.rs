//! Voice I/O for VIKRAM: built-in engines first, helper shell-outs only as detected fallbacks.
//!
//! Phase 1 is push-to-talk: an operator invokes it explicitly, audio is captured with the
//! linked cpal engine (helper binaries are the fallback where cpal has no device), silence is
//! trimmed locally, a local ASR engine transcribes it, and a local TTS helper speaks the
//! model's real reply. Every stage is optional and reports why it is unavailable; nothing here
//! pretends a missing microphone or model is a working voice.
//!
//! Helpers are spawned with an argv vector, never a shell, and every invocation is bounded.
//! Model weights are operator-downloaded files, never bundled.

mod asr;
mod cancel;
mod capture;
mod helpers;
mod player;
mod session;
mod vad;

pub use asr::{
    AsrEngine, DEFAULT_MODEL, VoxtypeRecognizer, WavoRecognizer, models_dir, probe_asr,
    resolve_model, resolve_model_in,
};
pub use cancel::CancelToken;
pub use capture::{CpalSource, DeviceSource, input_devices, probe_capture};
pub use helpers::{first_available, missing_tool_message, probe, which};
pub use player::{CpalPlayer, DevicePlayer, output_devices, probe_playback};
pub use session::{
    AudioSource, HelperPlayer, HelperRecognizer, HelperSource, HelperSynth, Player, Recognizer,
    Synthesizer, VoiceSession,
};
pub use vad::{detect_speech, is_silent, noise_floor, speech_gate, trim_silence};

use std::path::PathBuf;
use thiserror::Error;

pub type Result<T> = std::result::Result<T, VoiceError>;

#[derive(Debug, Error)]
pub enum VoiceError {
    #[error("voice is disabled; set enabled = true for this session before opening audio devices")]
    Disabled,
    #[error("cancelled")]
    Cancelled,
    #[error("no audio device: {0}")]
    NoDevice(String),
    #[error("{0}")]
    MissingHelper(String),
    #[error("no usable ASR model: {0}")]
    ModelMissing(String),
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
#[derive(Clone, Debug, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(default)]
pub struct VoiceConfig {
    pub enabled: bool,
    pub capture_helper: String,
    pub capture_fallback: String,
    pub asr_helper: String,
    pub asr_model: PathBuf,
    /// None resolves under `models_dir` (or `<vikram home>/models` when unset).
    pub models_dir: Option<PathBuf>,
    /// None lets whisper auto-detect the spoken language.
    pub asr_language: Option<String>,
    /// Use the detected voxtype CLI as the ASR fast path. Absence changes nothing: the
    /// built-in whisper engine is the primary path either way.
    pub voxtype_fast_path: bool,
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
    /// A run shorter than this is a click, not a word.
    pub min_speech_ms: u64,
    /// How far above the measured noise floor a frame must sit to count as voiced.
    pub floor_ratio: f32,
    /// Input device name, matched case-insensitively against cpal's list. None = default.
    pub input_device: Option<String>,
    /// Output device name. None = default.
    pub output_device: Option<String>,
}

impl Default for VoiceConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            capture_helper: "parecord".into(),
            capture_fallback: "arecord".into(),
            asr_helper: "whisper-cli".into(),
            asr_model: PathBuf::new(),
            models_dir: None,
            asr_language: None,
            voxtype_fast_path: true,
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
            min_speech_ms: 250,
            floor_ratio: 2.5,
            input_device: None,
            output_device: None,
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
        if !(1.0..=20.0).contains(&self.floor_ratio) {
            return Err(VoiceError::Config(
                "floor_ratio must be between 1 and 20".into(),
            ));
        }
        if self.min_speech_ms == 0 {
            return Err(VoiceError::Config("min_speech_ms must be positive".into()));
        }
        Ok(())
    }
}

impl VoiceConfig {
    /// `$VIKRAM_HOME/voice.json`, else `~/.vikram/voice.json`.
    pub fn config_path() -> PathBuf {
        let base = std::env::var_os("VIKRAM_HOME")
            .filter(|home| !home.is_empty())
            .map(PathBuf::from)
            .or_else(|| {
                std::env::var_os("HOME")
                    .or_else(|| std::env::var_os("USERPROFILE"))
                    .map(PathBuf::from)
            })
            .unwrap_or_else(|| PathBuf::from("."));
        base.join(".vikram").join("voice.json")
    }

    /// Defaults overlaid with the operator's file. A malformed file is an error, not a silent
    /// reset: an operator who edited the wrong key should hear about it.
    pub fn load() -> Result<Self> {
        let path = Self::config_path();
        if !path.is_file() {
            return Ok(Self::default());
        }
        let text = std::fs::read_to_string(&path)
            .map_err(|err| VoiceError::Config(format!("read {}: {err}", path.display())))?;
        serde_json::from_str(&text)
            .map_err(|err| VoiceError::Config(format!("parse {}: {err}", path.display())))
    }

    pub fn save(&self) -> Result<PathBuf> {
        let path = Self::config_path();
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)
                .map_err(|err| VoiceError::Config(format!("create {}: {err}", parent.display())))?;
        }
        let text = serde_json::to_string_pretty(self)
            .map_err(|err| VoiceError::Config(format!("serialize voice config: {err}")))?;
        std::fs::write(&path, text)
            .map_err(|err| VoiceError::Config(format!("write {}: {err}", path.display())))?;
        Ok(path)
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum StageStatus {
    /// Usable on the built-in engine this stage prefers. The string names that engine.
    Ready(String),
    /// Usable, but only on a fallback path (helper binary, detected engine). The string says
    /// what is missing and what is being used instead.
    Degraded(String),
    Missing(String),
    Disabled(String),
}

impl StageStatus {
    pub fn is_ready(&self) -> bool {
        matches!(self, Self::Ready(_) | Self::Degraded(_))
    }

    pub fn is_degraded(&self) -> bool {
        !matches!(self, Self::Ready(_))
    }

    pub fn label(&self) -> String {
        match self {
            Self::Ready(engine) => format!("ready: {engine}"),
            Self::Degraded(reason) => format!("degraded: {reason}"),
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
    /// True when any stage is not running on its built-in engine: a reportable degradation,
    /// not a silent substitution.
    pub degraded: bool,
    /// Runtime degradation notes (fast-path failures, fallbacks taken at run time), appended
    /// to `lines()` so the CLI reports them instead of swallowing them.
    pub notes: Vec<String>,
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
        let mut lines = vec![
            format!("enabled: {}", self.enabled),
            format!("capture:  {}", self.capture.label()),
            format!("asr:      {}", self.asr.label()),
            format!("tts:      {}", self.tts.label()),
            format!("playback: {}", self.playback.label()),
        ];
        if self.degraded {
            lines.push("degraded: true (some stage is off its built-in engine)".into());
        }
        lines.extend(self.notes.iter().cloned());
        lines
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
        assert!(status.degraded);
        assert_eq!(status.lines().len(), 6);
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
    fn stage_labels_name_the_engine_or_the_problem() {
        assert_eq!(
            StageStatus::Ready("built-in cpal".into()).label(),
            "ready: built-in cpal"
        );
        assert_eq!(
            StageStatus::Degraded("no cpal device; using parec".into()).label(),
            "degraded: no cpal device; using parec"
        );
        assert_eq!(
            StageStatus::Missing("no whisper-cli".into()).label(),
            "missing: no whisper-cli"
        );
        assert_eq!(
            StageStatus::Disabled("operator opt-in".into()).label(),
            "disabled: operator opt-in"
        );
        assert!(StageStatus::Degraded("x".into()).is_ready());
        assert!(StageStatus::Degraded("x".into()).is_degraded());
        assert!(!StageStatus::Ready("built-in".into()).is_degraded());
    }

    #[test]
    fn runtime_notes_are_reported_not_swallowed() {
        let status = VoiceStatus {
            enabled: true,
            capture: StageStatus::Ready("built-in cpal".into()),
            asr: StageStatus::Ready("whisper".into()),
            tts: StageStatus::Degraded("no piper voice; using espeak-ng".into()),
            playback: StageStatus::Ready("built-in cpal".into()),
            degraded: true,
            notes: vec!["voxtype fast path failed; fell back to whisper".into()],
        };
        let lines = status.lines().join("\n");
        assert!(status.is_ready());
        assert!(lines.contains("voxtype fast path failed"));
        assert!(lines.contains("degraded: true"));
    }

    #[test]
    fn missing_helper_message_names_a_fix() {
        let text = missing_tool_message("parecord");
        assert!(text.contains("parecord"));
        assert!(text.contains("install"));
    }

    #[test]
    fn a_partial_config_file_keeps_the_defaults() {
        let config: VoiceConfig =
            serde_json::from_str(r#"{"max_record_ms": 5000, "asr_language": "en"}"#)
                .expect("partial json parses");
        assert_eq!(config.max_record_ms, 5_000);
        assert_eq!(config.asr_language.as_deref(), Some("en"));
        assert_eq!(config.floor_ratio, 2.5);
        assert!(!config.enabled);
    }

    #[test]
    fn the_config_path_sits_in_the_vikram_home() {
        let path = VoiceConfig::config_path();
        assert!(path.ends_with("voice.json"), "{}", path.display());
        assert!(path.to_string_lossy().contains(".vikram"));
    }

    #[test]
    fn a_ratio_at_or_below_one_is_refused() {
        let mut config = VoiceConfig::default();
        config.floor_ratio = 0.5;
        assert!(config.validate().is_err());
        config.floor_ratio = 2.5;
        config.min_speech_ms = 0;
        assert!(config.validate().is_err());
    }
}
