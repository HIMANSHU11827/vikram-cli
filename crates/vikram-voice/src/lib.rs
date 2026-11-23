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
mod cloud_tts;
mod helpers;
mod player;
mod session;
mod vad;

pub use asr::{
    AsrBackend, AsrEngine, DEFAULT_MODEL, VoxtypeRecognizer, WavoRecognizer, models_dir, probe_asr,
    resolve_model, resolve_model_in,
};
pub use cancel::CancelToken;
pub use capture::{CpalSource, DeviceSource, input_devices, probe_capture};
pub use cloud_tts::{CloudRequest, CloudSynth, CloudTransport, HttpTransport, probe_cloud_tts};
pub use helpers::{first_available, missing_tool_message, probe, which};
pub use player::{CpalPlayer, DevicePlayer, output_devices, probe_playback};
pub use session::{
    AudioSource, Captioner, HelperPlayer, HelperRecognizer, HelperSource, HelperSynth, Player,
    Recognizer, Synthesizer, VoiceSession, VoiceSynth, pick_voice, piper_voices, spoken_text,
};
pub use vad::{detect_speech, is_silent, noise_floor, speech_gate, trim_silence};
/// The download types the CLI needs to pull a model by name: `Hub` and its `Quant`.
pub use wavo::hub::{Hub, Quant};

use std::ffi::OsStr;
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
    /// The cloud voice is selected but cannot run: no key, or no voice id. The session falls
    /// back to the local voice and says so; this is never the end of a turn.
    #[error("cloud tts is not configured: {0}")]
    CloudUnconfigured(String),
    /// The cloud voice ran and failed (network, HTTP, malformed audio). The session falls back to
    /// the local voice and says so; this is never the end of a turn.
    #[error("cloud tts failed: {0}")]
    Cloud(String),
}

/// Which synthesizer speaks a reply. `local` is the default and the only one that needs no key,
/// no network and no billing; `elevenlabs` is opt-in per operator.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TtsBackend {
    Local,
    ElevenLabs,
}

impl TtsBackend {
    /// `local` or `elevenlabs`; case and surrounding spaces don't matter.
    pub fn parse(value: &str) -> Option<Self> {
        match value.trim().to_ascii_lowercase().as_str() {
            "local" => Some(Self::Local),
            "elevenlabs" => Some(Self::ElevenLabs),
            _ => None,
        }
    }

    /// The name `parse` takes and `tts_backend` holds.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Local => "local",
            Self::ElevenLabs => "elevenlabs",
        }
    }

    /// Every name `parse` accepts, for an error that tells the operator what to type.
    pub const NAMES: &'static str = "local, elevenlabs";
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
    /// The quantization a hub download asks for: `q8_0` (the default), `q4_k_m`, `q5_k_m`,
    /// `q6_k` or `f16`. It only applies to a model named by hub, never to an explicit
    /// `asr_model` file, which is used as it is.
    pub asr_quant: String,
    /// Where the model runs: `vulkan` (the default: the GPU, with a CPU fallback that says so),
    /// `auto` (wavo's own pick for the machine) or `cpu`.
    pub asr_backend: String,
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
    /// Spoken language for synthesis (`en`, `en-gb`, `de`). None leaves the helper's own default,
    /// which on a fresh machine is whatever espeak-ng was built for.
    pub tts_language: Option<String>,
    /// Which synthesizer speaks: `local` (the default — the piper voice on this machine,
    /// `en_GB-jarvis-medium`, falling back to espeak-ng) or `elevenlabs` (opt-in cloud voice).
    /// Local stays the default because it needs no key, no network and no billing.
    pub tts_backend: String,
    /// The environment variable the cloud key is read from. The key itself is never written to
    /// `voice.json` and never printed; only this name is stored.
    pub tts_api_key_env: String,
    /// The ElevenLabs voice to speak with, for example `JBFqnCBsd6RMkjVDRZzb` (their published
    /// sample voice). Empty by default on purpose: a built-in voice id would bill an operator who
    /// never chose one. Set it with `vikram voice config set tts_voice_id <id>`.
    pub tts_voice_id: String,
    /// The ElevenLabs model id. `eleven_turbo_v2_5` is the low-latency default; their published
    /// default is `eleven_multilingual_v2`, which is slower but speaks more languages.
    /// ElevenLabs now lists Turbo as deprecated in favour of `eleven_flash_v2_5` (same voice,
    /// lower latency); either id works here, so switching is `config set tts_model`.
    pub tts_model: String,
    /// Speaking rate in words per minute (80..=450). 175 is espeak-ng's own default; piper gets
    /// the reciprocal as `--length_scale`.
    pub rate: u32,
    /// Output amplitude, 0.0 silent ..= 1.0 unattenuated.
    pub volume: f32,
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
            asr_quant: "q8_0".into(),
            asr_backend: "vulkan".into(),
            models_dir: None,
            asr_language: None,
            voxtype_fast_path: true,
            asr_timeout_secs: 120,
            tts_helper: "piper".into(),
            tts_fallback: "espeak-ng".into(),
            tts_voice: None,
            tts_language: None,
            tts_backend: "local".into(),
            tts_api_key_env: "ELEVENLABS_API_KEY".into(),
            tts_voice_id: String::new(),
            tts_model: "eleven_turbo_v2_5".into(),
            rate: 175,
            volume: 1.0,
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
        if !(80..=450).contains(&self.rate) {
            return Err(VoiceError::Config(
                "rate must be between 80 and 450 words per minute".into(),
            ));
        }
        if !(0.0..=1.0).contains(&self.volume) {
            return Err(VoiceError::Config("volume must be between 0 and 1".into()));
        }
        if let Err(err) = Quant::parse(&self.asr_quant) {
            return Err(VoiceError::Config(format!("asr_quant: {err}")));
        }
        if TtsBackend::parse(&self.tts_backend).is_none() {
            return Err(VoiceError::Config(format!(
                "tts_backend `{}` is not one of: {}",
                self.tts_backend.trim(),
                TtsBackend::NAMES
            )));
        }
        if self.tts_api_key_env.trim().is_empty() {
            return Err(VoiceError::Config(
                "tts_api_key_env is empty; name the environment variable that holds the key".into(),
            ));
        }
        if self.tts_model.trim().is_empty() {
            return Err(VoiceError::Config(
                "tts_model is empty; name a model such as eleven_turbo_v2_5".into(),
            ));
        }
        if AsrBackend::parse(&self.asr_backend).is_none() {
            return Err(VoiceError::Config(format!(
                "asr_backend `{}` is not one of: vulkan, auto, cpu",
                self.asr_backend.trim()
            )));
        }
        if let Some(language) = self.tts_language.as_deref() {
            let language = language.trim();
            if language.is_empty() {
                return Err(VoiceError::Config(
                    "tts_language is set but empty; set a tag like en or en-gb, or remove it"
                        .into(),
                ));
            }
            if language.chars().any(char::is_whitespace) {
                return Err(VoiceError::Config(format!(
                    "tts_language `{language}` contains whitespace; use an espeak voice tag like en-gb"
                )));
            }
        }
        Ok(())
    }
}

impl VoiceConfig {
    /// `$VIKRAM_HOME/voice.json` when the variable is set, else `~/.vikram/voice.json`. The
    /// variable *is* the home, exactly as `vikram_runtime::config::vikram_home` treats it.
    pub fn config_path() -> PathBuf {
        voice_config_path(
            std::env::var_os("VIKRAM_HOME").as_deref(),
            std::env::var_os("HOME")
                .or_else(|| std::env::var_os("USERPROFILE"))
                .as_deref(),
        )
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
        self.save_to(&path)?;
        Ok(path)
    }

    /// Write to an explicit path. `save` is this against `config_path`, and the split exists so the
    /// writing can be tested without touching the operator's real file.
    pub fn save_to(&self, path: &std::path::Path) -> Result<()> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)
                .map_err(|err| VoiceError::Config(format!("create {}: {err}", parent.display())))?;
        }
        let text = serde_json::to_string_pretty(self)
            .map_err(|err| VoiceError::Config(format!("serialize voice config: {err}")))?;
        std::fs::write(path, text)
            .map_err(|err| VoiceError::Config(format!("write {}: {err}", path.display())))
    }
}

/// The one place the `$VIKRAM_HOME` convention is applied to the voice config: a set variable
/// is already the home, so the file sits directly under it; without it the file sits under
/// `~/.vikram`.
fn voice_config_path(home: Option<&OsStr>, user_home: Option<&OsStr>) -> PathBuf {
    match home.filter(|home| !home.is_empty()) {
        Some(home) => PathBuf::from(home).join("voice.json"),
        None => user_home
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from("."))
            .join(".vikram")
            .join("voice.json"),
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
        // The status shape is what matters here, not how many stages happen to be degraded on
        // the machine running the test: a box with piper installed has one line fewer.
        let lines = status.lines();
        assert_eq!(lines[0], "enabled: false");
        for stage in ["capture:", "asr:", "tts:", "playback:"] {
            assert!(
                lines.iter().any(|line| line.starts_with(stage)),
                "{stage} missing from {lines:?}"
            );
        }
        assert!(lines.len() >= 5 && lines.len() <= 6, "{lines:?}");
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
    fn a_spoken_language_must_look_like_a_voice_tag() {
        let blank = VoiceConfig {
            tts_language: Some("  ".into()),
            ..Default::default()
        };
        assert!(blank.validate().is_err());
        let spaced = VoiceConfig {
            tts_language: Some("en gb".into()),
            ..Default::default()
        };
        assert!(spaced.validate().is_err());
        let fine = VoiceConfig {
            tts_language: Some("en-gb".into()),
            ..Default::default()
        };
        fine.validate().expect("a plain tag is accepted");
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
        assert_eq!(
            config.asr_quant, "q8_0",
            "an older voice.json keeps downloading Q8_0"
        );
        assert_eq!(config.asr_backend, "vulkan", "and keeps running on Vulkan");
    }

    #[test]
    fn the_asr_quant_and_backend_are_checked_against_what_the_engine_can_use() {
        let config = VoiceConfig::default();
        assert_eq!(config.asr_quant, "q8_0");
        assert_eq!(config.asr_backend, "vulkan");
        config.validate().expect("the defaults are valid");

        for quant in ["q8_0", "q4_k_m", "Q5_K_M", "q6_k", "f16"] {
            let config = VoiceConfig {
                asr_quant: quant.into(),
                ..Default::default()
            };
            config
                .validate()
                .unwrap_or_else(|err| panic!("{quant}: {err}"));
        }
        for backend in ["vulkan", "auto", "cpu", "CPU"] {
            let config = VoiceConfig {
                asr_backend: backend.into(),
                ..Default::default()
            };
            config
                .validate()
                .unwrap_or_else(|err| panic!("{backend}: {err}"));
        }

        let bad = VoiceConfig {
            asr_quant: "q3_k".into(),
            ..Default::default()
        };
        let err = bad.validate().unwrap_err().to_string();
        assert!(err.contains("asr_quant"), "{err}");
        assert!(err.contains("q4_k_m"), "the error lists the quants: {err}");

        let bad = VoiceConfig {
            asr_backend: "vulcan".into(),
            ..Default::default()
        };
        let err = bad.validate().unwrap_err().to_string();
        assert!(err.contains("asr_backend"), "{err}");
        assert!(err.contains("vulkan, auto, cpu"), "{err}");
    }

    #[test]
    fn the_config_path_follows_the_vikram_home_convention() {
        let path = voice_config_path(
            Some(OsStr::new("/srv/vikram")),
            Some(OsStr::new("/home/op")),
        );
        assert_eq!(path, PathBuf::from("/srv/vikram/voice.json"));
        assert!(
            !path.to_string_lossy().contains(".vikram"),
            "a set VIKRAM_HOME must not be doubled: {}",
            path.display()
        );
        let path = voice_config_path(None, Some(OsStr::new("/home/op")));
        assert_eq!(path, PathBuf::from("/home/op/.vikram/voice.json"));
        let path = voice_config_path(Some(OsStr::new("")), Some(OsStr::new("/home/op")));
        assert_eq!(path, PathBuf::from("/home/op/.vikram/voice.json"));
        assert!(VoiceConfig::config_path().ends_with("voice.json"));
    }

    #[test]
    fn a_ratio_at_or_below_one_is_refused() {
        let refused = VoiceConfig {
            floor_ratio: 0.5,
            ..Default::default()
        };
        assert!(refused.validate().is_err());
        let no_minimum = VoiceConfig {
            min_speech_ms: 0,
            ..Default::default()
        };
        assert!(no_minimum.validate().is_err());
    }

    #[test]
    fn the_local_voice_is_the_default_and_needs_no_key() {
        let config = VoiceConfig::default();
        config.validate().expect("the defaults are valid");
        assert_eq!(config.tts_backend, "local");
        assert_eq!(
            TtsBackend::parse(&config.tts_backend),
            Some(TtsBackend::Local)
        );
        assert_eq!(config.tts_api_key_env, "ELEVENLABS_API_KEY");
        assert!(
            config.tts_voice_id.is_empty(),
            "no voice id is baked in: a default would bill an operator who never chose one"
        );
        assert_eq!(config.tts_model, "eleven_turbo_v2_5");
        assert_eq!(
            TtsBackend::parse("ElevenLabs"),
            Some(TtsBackend::ElevenLabs)
        );
        assert_eq!(TtsBackend::ElevenLabs.as_str(), "elevenlabs");
    }

    #[test]
    fn an_unknown_tts_backend_is_refused_and_names_both() {
        let config = VoiceConfig {
            tts_backend: "azure".into(),
            ..Default::default()
        };
        let err = config.validate().unwrap_err().to_string();
        assert!(err.contains("tts_backend"), "{err}");
        assert!(err.contains("local, elevenlabs"), "{err}");

        let ok = VoiceConfig {
            tts_backend: "elevenlabs".into(),
            ..Default::default()
        };
        ok.validate().expect(
            "selecting the cloud voice before configuring it is legal: status says what is missing",
        );
    }

    #[test]
    fn an_older_voice_json_keeps_the_local_voice() {
        let config: VoiceConfig = serde_json::from_str(r#"{"tts_helper": "piper"}"#).unwrap();
        assert_eq!(config.tts_backend, "local");
        assert_eq!(config.tts_api_key_env, "ELEVENLABS_API_KEY");
        assert_eq!(config.tts_model, "eleven_turbo_v2_5");
        assert!(config.tts_voice_id.is_empty());
    }

    #[test]
    fn the_key_is_named_never_stored() {
        let config = VoiceConfig {
            tts_backend: "elevenlabs".into(),
            tts_voice_id: "abc".into(),
            ..Default::default()
        };
        let json = serde_json::to_string_pretty(&config).unwrap();
        assert!(
            json.contains("\"tts_api_key_env\": \"ELEVENLABS_API_KEY\""),
            "{json}"
        );
        assert!(
            !json.contains("xi-api-key") && !json.contains("\"tts_api_key\""),
            "the config file stores the variable name, never a key: {json}"
        );
    }

    #[test]
    fn saving_to_a_path_creates_it_and_round_trips() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("nested").join("voice.json");
        let config = VoiceConfig {
            enabled: true,
            floor_ratio: 3.0,
            ..Default::default()
        };
        config.save_to(&path).unwrap();
        let text = std::fs::read_to_string(&path).unwrap();
        assert!(text.contains("\"enabled\": true"), "{text}");
        let parsed: VoiceConfig = serde_json::from_str(&text).unwrap();
        assert!(parsed.enabled);
        assert_eq!(parsed.floor_ratio, 3.0);
    }
}
