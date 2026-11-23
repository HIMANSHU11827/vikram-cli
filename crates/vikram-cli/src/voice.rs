//! `vikram voice` — local voice I/O diagnostics and push-to-talk capture.
//!
//! This is a transport, not the assistant: `listen` prints a transcript and `say` speaks a string.
//! Neither sends anything to a model, so a missing microphone or helper cannot be mistaken for a
//! working voice round-trip.
//!
//! Every tunable lives in `<vikram home>/.vikram/voice.json`. `voice config` shows, prints, and
//! edits that file; the device, language, and gate flags on `listen`/`chat` override it for one
//! run and are written back afterwards, so the next run needs no flags.

use std::path::{Path, PathBuf};

use clap::{Args, Subcommand};
use vikram_runtime::config::Config;
use vikram_voice::{AsrEngine, Recognizer, VoiceConfig, VoiceError, VoiceSession, probe};

use crate::chat;

#[derive(Debug, Subcommand)]
pub enum VoiceAction {
    /// Report which local voice helpers and models are available.
    Status {
        /// Whisper model to check, in addition to VIKRAM_ASR_MODEL.
        #[arg(long = "asr-model", value_name = "PATH")]
        asr_model: Option<PathBuf>,
    },
    /// Transcribe a 16 kHz mono WAV file and print the text. No microphone involved: the same
    /// engine the voice mode uses, so two models can be compared on one clip.
    Transcribe {
        /// WAV file to transcribe.
        #[arg(value_name = "WAV")]
        file: PathBuf,
        /// Model to use. Defaults to the configured one.
        #[arg(long = "asr-model", value_name = "PATH")]
        asr_model: Option<PathBuf>,
    },
    /// Record one utterance locally and print its transcript.
    Listen {
        /// Open an audio device for this invocation. Voice I/O is opt-in.
        #[arg(long)]
        enable: bool,
        /// Path to a downloaded Whisper ggml model. Defaults to VIKRAM_ASR_MODEL.
        #[arg(long = "asr-model", value_name = "PATH")]
        asr_model: Option<PathBuf>,
        /// Recording cap in milliseconds.
        #[arg(long, value_name = "MS")]
        max_record_ms: Option<u64>,
        #[command(flatten)]
        overrides: OverrideArgs,
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
        #[arg(long = "asr-model", value_name = "PATH")]
        asr_model: Option<PathBuf>,
        /// Recording cap in milliseconds.
        #[arg(long, value_name = "MS")]
        max_record_ms: Option<u64>,
        /// Piper voice model for the spoken reply. Defaults to VIKRAM_TTS_VOICE.
        #[arg(long, value_name = "PATH")]
        voice: Option<PathBuf>,
        /// Start a new session instead of continuing the most recent one.
        #[arg(long)]
        fresh: bool,
        #[command(flatten)]
        overrides: OverrideArgs,
    },
    /// Show or change the saved voice configuration.
    Config {
        #[command(subcommand)]
        action: VoiceConfigAction,
    },
}

#[derive(Debug, Subcommand)]
pub enum VoiceConfigAction {
    /// Print the saved config as JSON, and the file it was read from.
    Show,
    /// Print the config file path and nothing else, so it can be scripted.
    Path,
    /// Set one field, validate the result, and save it.
    Set {
        /// Field name: the JSON name, its kebab spelling, or a dotted path such as
        /// `voice.input_device` or `asr.language`.
        key: String,
        /// New value. `none` clears an optional field.
        value: String,
    },
}

/// Flags that override the saved config for one run. An absent flag is `None`, which is what keeps
/// the precedence honest: flag, then the saved file, then the built-in default.
#[derive(Debug, Clone, Default, PartialEq, Args)]
pub struct OverrideArgs {
    /// Whisper language code; `auto` lets whisper detect it.
    #[arg(long, value_name = "CODE")]
    pub language: Option<String>,
    /// Input device name, matched case-insensitively against the device list.
    #[arg(long, value_name = "NAME")]
    pub input_device: Option<String>,
    /// Output device name, matched case-insensitively against the device list.
    #[arg(long, value_name = "NAME")]
    pub output_device: Option<String>,
    /// RMS level a frame must clear to count as voiced (0-1).
    #[arg(long, value_name = "LEVEL")]
    pub threshold: Option<f32>,
    /// A voiced run shorter than this many milliseconds is a click, not a word.
    #[arg(long = "min-speech", value_name = "MS")]
    pub min_speech_ms: Option<u64>,
    /// How far above the measured noise floor a frame must sit to count as voiced.
    #[arg(long = "floor-ratio", value_name = "RATIO")]
    pub floor_ratio: Option<f32>,
}

impl OverrideArgs {
    fn is_empty(&self) -> bool {
        self.language.is_none()
            && self.input_device.is_none()
            && self.output_device.is_none()
            && self.threshold.is_none()
            && self.min_speech_ms.is_none()
            && self.floor_ratio.is_none()
    }

    fn apply(&self, config: &mut VoiceConfig) {
        if let Some(code) = &self.language {
            let code = code.trim();
            config.asr_language = if code.eq_ignore_ascii_case("auto") {
                None
            } else {
                Some(code.to_string())
            };
        }
        if let Some(name) = &self.input_device {
            config.input_device = Some(name.trim().to_string());
        }
        if let Some(name) = &self.output_device {
            config.output_device = Some(name.trim().to_string());
        }
        if let Some(level) = self.threshold {
            config.rms_threshold = level;
        }
        if let Some(ms) = self.min_speech_ms {
            config.min_speech_ms = ms;
        }
        if let Some(ratio) = self.floor_ratio {
            config.floor_ratio = ratio;
        }
    }

    /// The flags this run used, named by their config field so the saved line matches the file.
    fn summary(&self) -> String {
        let mut parts = Vec::new();
        if let Some(code) = &self.language {
            parts.push(format!("asr_language={code}"));
        }
        if let Some(name) = &self.input_device {
            parts.push(format!("input_device={name}"));
        }
        if let Some(name) = &self.output_device {
            parts.push(format!("output_device={name}"));
        }
        if let Some(level) = self.threshold {
            parts.push(format!("rms_threshold={level}"));
        }
        if let Some(ms) = self.min_speech_ms {
            parts.push(format!("min_speech_ms={ms}"));
        }
        if let Some(ratio) = self.floor_ratio {
            parts.push(format!("floor_ratio={ratio}"));
        }
        parts.join(", ")
    }
}

fn env_path(name: &str) -> Option<PathBuf> {
    std::env::var(name)
        .ok()
        .map(PathBuf::from)
        .filter(|path| !path.as_os_str().is_empty())
}

fn config(
    base: &VoiceConfig,
    enable: bool,
    asr_model: Option<PathBuf>,
    voice: Option<PathBuf>,
    max_record_ms: Option<u64>,
    overrides: &OverrideArgs,
) -> VoiceConfig {
    let mut config = base.clone();
    config.enabled = enable || base.enabled;
    if let Some(path) = asr_model.or_else(|| env_path("VIKRAM_ASR_MODEL")) {
        config.asr_model = path;
    }
    if let Some(path) = voice.or_else(|| env_path("VIKRAM_TTS_VOICE")) {
        config.tts_voice = Some(path);
    }
    if let Some(ms) = max_record_ms {
        config.max_record_ms = ms;
    }
    overrides.apply(&mut config);
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
pub(crate) fn speakable(reply: &str) -> &str {
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

/// The field names the config file actually uses, read off the struct so the accepted-key list
/// cannot drift from the JSON.
fn config_keys() -> Vec<String> {
    match serde_json::to_value(VoiceConfig::default()) {
        Ok(serde_json::Value::Object(map)) => map.keys().cloned().collect(),
        _ => Vec::new(),
    }
}

fn unknown_key(key: &str) -> String {
    format!(
        "unknown voice config key {key:?}; valid keys: {}",
        config_keys().join(", ")
    )
}

/// Accept the JSON name, the kebab spelling a flag uses, and a dotted path such as
/// `voice.input_device` or `asr.language`.
fn normalize_key(key: &str) -> String {
    let key = key.trim().to_ascii_lowercase();
    let stripped = key
        .strip_prefix("voice.")
        .or_else(|| key.strip_prefix("voice_"))
        .unwrap_or(&key);
    stripped.replace(['.', '-'], "_")
}

fn parse_bool(key: &str, value: &str) -> Result<bool, String> {
    match value.trim().to_ascii_lowercase().as_str() {
        "true" | "1" | "yes" | "on" => Ok(true),
        "false" | "0" | "no" | "off" => Ok(false),
        other => Err(format!("{key}: expected true or false, got {other:?}")),
    }
}

fn parse_u64(key: &str, value: &str) -> Result<u64, String> {
    value
        .trim()
        .parse::<u64>()
        .map_err(|_| format!("{key}: expected a whole number, got {value:?}"))
}

fn parse_u16(key: &str, value: &str) -> Result<u16, String> {
    value.trim().parse::<u16>().map_err(|_| {
        format!(
            "{key}: expected a whole number up to {}, got {value:?}",
            u16::MAX
        )
    })
}

fn parse_f32(key: &str, value: &str) -> Result<f32, String> {
    let parsed = value
        .trim()
        .parse::<f32>()
        .map_err(|_| format!("{key}: expected a number, got {value:?}"))?;
    if !parsed.is_finite() {
        return Err(format!("{key}: expected a finite number, got {value:?}"));
    }
    Ok(parsed)
}

fn required_text(key: &str, value: &str) -> Result<String, String> {
    let text = value.trim();
    if text.is_empty() {
        return Err(format!("{key}: expected a non-empty value"));
    }
    Ok(text.to_string())
}

/// `none`, `null`, and the empty string all clear an optional field.
fn optional_text(value: &str) -> Option<String> {
    let text = value.trim();
    if text.is_empty() || text.eq_ignore_ascii_case("none") || text.eq_ignore_ascii_case("null") {
        return None;
    }
    Some(text.to_string())
}

fn render_path(path: &Path) -> String {
    if path.as_os_str().is_empty() {
        "none".into()
    } else {
        path.display().to_string()
    }
}

fn render_optional_text(value: &Option<String>) -> String {
    value.clone().unwrap_or_else(|| "none".into())
}

fn render_optional_path(value: &Option<PathBuf>) -> String {
    value
        .as_ref()
        .map(|path| render_path(path))
        .unwrap_or_else(|| "none".into())
}

fn set_field<T>(
    slot: &mut T,
    value: T,
    render: impl FnOnce(&T) -> String,
) -> Result<String, String> {
    *slot = value;
    Ok(render(slot))
}

fn apply_setting(config: &mut VoiceConfig, key: &str, value: &str) -> Result<String, String> {
    match key {
        "enabled" => set_field(&mut config.enabled, parse_bool(key, value)?, |v| {
            v.to_string()
        }),
        "capture_helper" => set_field(
            &mut config.capture_helper,
            required_text(key, value)?,
            |v| v.clone(),
        ),
        "capture_fallback" => set_field(
            &mut config.capture_fallback,
            required_text(key, value)?,
            |v| v.clone(),
        ),
        "asr_helper" => set_field(&mut config.asr_helper, required_text(key, value)?, |v| {
            v.clone()
        }),
        "asr_model" => set_field(&mut config.asr_model, PathBuf::from(value.trim()), |v| {
            render_path(v)
        }),
        "models_dir" => set_field(
            &mut config.models_dir,
            optional_text(value).map(PathBuf::from),
            render_optional_path,
        ),
        "asr_language" => set_field(
            &mut config.asr_language,
            optional_text(value),
            render_optional_text,
        ),
        "voxtype_fast_path" => set_field(
            &mut config.voxtype_fast_path,
            parse_bool(key, value)?,
            |v| v.to_string(),
        ),
        "asr_timeout_secs" => {
            set_field(&mut config.asr_timeout_secs, parse_u64(key, value)?, |v| {
                v.to_string()
            })
        }
        "tts_helper" => set_field(&mut config.tts_helper, required_text(key, value)?, |v| {
            v.clone()
        }),
        "tts_fallback" => set_field(&mut config.tts_fallback, required_text(key, value)?, |v| {
            v.clone()
        }),
        "tts_voice" => set_field(
            &mut config.tts_voice,
            optional_text(value).map(PathBuf::from),
            render_optional_path,
        ),
        "tts_language" => set_field(
            &mut config.tts_language,
            optional_text(value),
            render_optional_text,
        ),
        "player_helper" => set_field(&mut config.player_helper, required_text(key, value)?, |v| {
            v.clone()
        }),
        "player_fallback" => set_field(
            &mut config.player_fallback,
            required_text(key, value)?,
            |v| v.clone(),
        ),
        "max_record_ms" => set_field(&mut config.max_record_ms, parse_u64(key, value)?, |v| {
            v.to_string()
        }),
        "sample_rate" => set_field(&mut config.sample_rate, parse_u16(key, value)?, |v| {
            v.to_string()
        }),
        "rms_threshold" => set_field(&mut config.rms_threshold, parse_f32(key, value)?, |v| {
            v.to_string()
        }),
        "hangover_ms" => set_field(&mut config.hangover_ms, parse_u64(key, value)?, |v| {
            v.to_string()
        }),
        "min_speech_ms" => set_field(&mut config.min_speech_ms, parse_u64(key, value)?, |v| {
            v.to_string()
        }),
        "floor_ratio" => set_field(&mut config.floor_ratio, parse_f32(key, value)?, |v| {
            v.to_string()
        }),
        "input_device" => set_field(
            &mut config.input_device,
            optional_text(value),
            render_optional_text,
        ),
        "output_device" => set_field(
            &mut config.output_device,
            optional_text(value),
            render_optional_text,
        ),
        other => set_untyped(config, other, value),
    }
}

/// Set a field this command has no typed arm for, using the JSON type the file already holds.
/// Keeps `set` working when the config grows a field: the key must exist in the file, so a typo is
/// still an unknown key, and a value the field's own type refuses leaves the config untouched.
fn set_untyped(config: &mut VoiceConfig, key: &str, value: &str) -> Result<String, String> {
    let document = serde_json::to_value(&*config).map_err(|err| format!("{key}: {err}"))?;
    let Some(slot) = document.get(key) else {
        return Err(unknown_key(key));
    };
    let replacement = match slot {
        serde_json::Value::Bool(_) => serde_json::Value::Bool(parse_bool(key, value)?),
        serde_json::Value::Number(number) if number.is_f64() => {
            serde_json::Value::from(parse_f32(key, value)?)
        }
        serde_json::Value::Number(_) => serde_json::Value::from(parse_u64(key, value)?),
        serde_json::Value::Null => match optional_text(value) {
            Some(text) => serde_json::Value::String(text),
            None => serde_json::Value::Null,
        },
        _ => serde_json::Value::String(value.trim().to_string()),
    };
    let mut document = document;
    if let Some(object) = document.as_object_mut() {
        object.insert(key.to_string(), replacement.clone());
    }
    *config = serde_json::from_value(document).map_err(|err| format!("{key}: {err}"))?;
    Ok(render_any(&replacement))
}

fn render_any(value: &serde_json::Value) -> String {
    match value {
        serde_json::Value::String(text) => text.clone(),
        serde_json::Value::Null => "none".into(),
        other => other.to_string(),
    }
}

fn set(key: &str, value: &str) -> Result<(), String> {
    let field = normalize_key(key);
    let mut config = VoiceConfig::load().map_err(|err| err.to_string())?;
    let rendered = apply_setting(&mut config, &field, value)?;
    config.validate().map_err(|err| format!("{field}: {err}"))?;
    let path = config.save().map_err(|err| err.to_string())?;
    println!("{field} = {rendered}");
    println!("saved to {}", path.display());
    Ok(())
}

fn render_path_line() -> String {
    VoiceConfig::config_path().display().to_string()
}

fn render_show() -> Result<String, String> {
    let config = VoiceConfig::load().map_err(|err| err.to_string())?;
    let json = serde_json::to_string_pretty(&config).map_err(|err| err.to_string())?;
    Ok(format!("path: {}\n{json}\n", render_path_line()))
}

pub fn config_command(action: VoiceConfigAction) -> Result<(), String> {
    match action {
        VoiceConfigAction::Show => print!("{}", render_show()?),
        VoiceConfigAction::Path => println!("{}", render_path_line()),
        VoiceConfigAction::Set { key, value } => set(&key, &value)?,
    }
    Ok(())
}

/// Write the flags this run used back to the config file. Only flag-provided fields are persisted:
/// `enabled` stays per-invocation, and an env-derived model path is not baked into the file.
fn persist(overrides: &OverrideArgs) -> Result<Option<PathBuf>, String> {
    if overrides.is_empty() {
        return Ok(None);
    }
    let mut saved = VoiceConfig::load().map_err(|err| err.to_string())?;
    overrides.apply(&mut saved);
    saved.validate().map_err(|err| err.to_string())?;
    let path = saved.save().map_err(|err| err.to_string())?;
    Ok(Some(path))
}

fn save_run_flags(overrides: &OverrideArgs) {
    match persist(overrides) {
        Ok(Some(path)) => println!("voice: saved {} to {}", overrides.summary(), path.display()),
        Ok(None) => {}
        Err(err) => eprintln!("voice: flags not saved: {err}"),
    }
}

/// A spoken turn: record one utterance, send its transcript through the same headless turn `vikram
/// chat` runs, then speak the reply. Each stage is honest about failing — no speech stops the turn
/// before it is sent, and a synthesizer that cannot run leaves the answer on screen instead of
/// failing the command.
pub async fn chat(runtime_config: Config, action: VoiceAction) -> Result<(), String> {
    let VoiceAction::Chat {
        enable,
        asr_model,
        max_record_ms,
        voice,
        fresh,
        overrides,
    } = action
    else {
        return Err("not a voice chat action".into());
    };
    let base = VoiceConfig::load().map_err(|err| err.to_string())?;
    let voice_config = config(&base, enable, asr_model, voice, max_record_ms, &overrides);
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
    save_run_flags(&overrides);

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
        VoiceAction::Config { action } => config_command(action),
        VoiceAction::Status { asr_model } => {
            let base = VoiceConfig::load().map_err(|err| err.to_string())?;
            let config = config(
                &base,
                false,
                asr_model,
                None,
                None,
                &OverrideArgs::default(),
            );
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
        VoiceAction::Transcribe { file, asr_model } => {
            let base = VoiceConfig::load().map_err(|err| err.to_string())?;
            let mut config = config(&base, true, asr_model, None, None, &OverrideArgs::default());
            config.enabled = true;
            let bytes = std::fs::read(&file).map_err(|err| format!("{}: {err}", file.display()))?;
            let pcm = wav_pcm(&bytes)?;
            let started = std::time::Instant::now();
            let mut engine = AsrEngine::new(&config);
            let text = engine.transcribe(&pcm).map_err(|err| err.to_string())?;
            let elapsed = started.elapsed();
            match text {
                Some(text) => println!("{text}"),
                None => println!("(no speech)"),
            }
            eprintln!("{} samples in {:.2}s", pcm.len(), elapsed.as_secs_f32());
            Ok(())
        }
        VoiceAction::Listen {
            enable,
            asr_model,
            max_record_ms,
            overrides,
        } => {
            let base = VoiceConfig::load().map_err(|err| err.to_string())?;
            let config = config(&base, enable, asr_model, None, max_record_ms, &overrides);
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
                    save_run_flags(&overrides);
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
            let base = VoiceConfig::load().map_err(|err| err.to_string())?;
            let config = config(&base, enable, None, voice, None, &OverrideArgs::default());
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
    use vikram_runtime::config::lock_security_env;

    /// A temp `VIKRAM_HOME` plus the process-wide env lock, so tests that read or write
    /// `voice.json` cannot see each other.
    fn temp_home() -> (tempfile::TempDir, std::sync::MutexGuard<'static, ()>) {
        let guard = lock_security_env();
        let dir = tempfile::tempdir().unwrap();
        unsafe { std::env::set_var("VIKRAM_HOME", dir.path()) };
        (dir, guard)
    }

    #[test]
    fn status_reports_every_stage_without_opening_devices() {
        let config = config(
            &VoiceConfig::default(),
            false,
            None,
            None,
            None,
            &OverrideArgs::default(),
        );
        let status = probe(&config);
        assert!(status.lines().len() >= 5);
        assert!(!status.is_ready());
        assert!(!config.enabled);
    }

    #[test]
    fn explicit_model_and_voice_override_defaults() {
        let config = config(
            &VoiceConfig::default(),
            true,
            Some(PathBuf::from("model.bin")),
            Some(PathBuf::from("voice.onnx")),
            Some(4_000),
            &OverrideArgs::default(),
        );
        assert!(config.enabled);
        assert_eq!(config.asr_model, PathBuf::from("model.bin"));
        assert_eq!(config.tts_voice, Some(PathBuf::from("voice.onnx")));
        assert_eq!(config.max_record_ms, 4_000);
    }

    #[test]
    fn saved_values_apply_and_flags_win_over_them() {
        let saved = VoiceConfig {
            asr_language: Some("de".into()),
            input_device: Some("saved mic".into()),
            output_device: Some("saved out".into()),
            rms_threshold: 0.02,
            min_speech_ms: 300,
            floor_ratio: 3.0,
            ..Default::default()
        };

        let kept = config(&saved, true, None, None, None, &OverrideArgs::default());
        assert_eq!(kept.asr_language.as_deref(), Some("de"));
        assert_eq!(kept.input_device.as_deref(), Some("saved mic"));
        assert_eq!(kept.rms_threshold, 0.02);

        let overrides = OverrideArgs {
            language: Some("en".into()),
            input_device: Some("usb mic".into()),
            output_device: Some("headphones".into()),
            threshold: Some(0.05),
            min_speech_ms: Some(120),
            floor_ratio: Some(4.5),
        };
        let run = config(&saved, true, None, None, None, &overrides);
        assert_eq!(run.asr_language.as_deref(), Some("en"));
        assert_eq!(run.input_device.as_deref(), Some("usb mic"));
        assert_eq!(run.output_device.as_deref(), Some("headphones"));
        assert_eq!(run.rms_threshold, 0.05);
        assert_eq!(run.min_speech_ms, 120);
        assert_eq!(run.floor_ratio, 4.5);
        assert_eq!(run.hangover_ms, saved.hangover_ms);
    }

    #[test]
    fn a_flag_clears_the_language_when_it_says_auto() {
        let saved = VoiceConfig {
            asr_language: Some("de".into()),
            ..Default::default()
        };
        let overrides = OverrideArgs {
            language: Some("auto".into()),
            ..Default::default()
        };
        let run = config(&saved, true, None, None, None, &overrides);
        assert_eq!(run.asr_language, None);
    }

    #[test]
    fn set_show_and_path_round_trip_through_a_temp_home() {
        let (home, _guard) = temp_home();
        let path = VoiceConfig::config_path();
        assert_eq!(render_path_line(), path.display().to_string());
        assert!(!path.exists(), "a fresh home has no config yet");

        set("input_device", "USB Mic").expect("a device name is a valid value");
        set("floor-ratio", "4.5").expect("the kebab spelling is accepted");
        set("voice.asr_language", "en").expect("the dotted spelling is accepted");

        let loaded = VoiceConfig::load().expect("the saved file loads");
        assert_eq!(loaded.input_device.as_deref(), Some("USB Mic"));
        assert_eq!(loaded.floor_ratio, 4.5);
        assert_eq!(loaded.asr_language.as_deref(), Some("en"));
        assert_eq!(
            path,
            home.path().join("voice.json"),
            "the config lands at the root of VIKRAM_HOME"
        );
        assert!(path.is_file());

        let shown = render_show().expect("show renders");
        assert!(shown.contains(&path.display().to_string()), "{shown}");
        assert!(shown.contains("\"input_device\": \"USB Mic\""), "{shown}");
        assert!(shown.contains("\"floor_ratio\": 4.5"), "{shown}");
    }

    #[test]
    fn unknown_key_lists_the_valid_keys() {
        let (_home, _guard) = temp_home();
        let err = set("louder", "1").expect_err("an unknown key must fail");
        assert!(err.contains("louder"), "{err}");
        assert!(err.contains("valid keys"), "{err}");
        assert!(err.contains("input_device"), "{err}");
        assert!(err.contains("floor_ratio"), "{err}");
        assert!(!VoiceConfig::config_path().exists(), "nothing was written");
    }

    #[test]
    fn a_bad_value_errors_and_leaves_the_file_untouched() {
        let (_home, _guard) = temp_home();
        set("rms_threshold", "0.02").expect("a valid threshold saves");
        let path = VoiceConfig::config_path();
        let before = std::fs::read_to_string(&path).unwrap();

        let err = set("floor_ratio", "99").expect_err("validate rejects a ratio above 20");
        assert!(err.contains("floor_ratio"), "{err}");
        assert_eq!(std::fs::read_to_string(&path).unwrap(), before);

        let err = set("rms_threshold", "loud").expect_err("a word is not a number");
        assert!(err.contains("rms_threshold"), "{err}");
        assert_eq!(std::fs::read_to_string(&path).unwrap(), before);

        let err = set("asr_helper", "   ").expect_err("a required helper cannot be blank");
        assert!(err.contains("asr_helper"), "{err}");
        assert!(err.contains("non-empty"), "{err}");
        assert_eq!(std::fs::read_to_string(&path).unwrap(), before);
    }

    #[test]
    fn an_optional_field_is_cleared_with_none() {
        let (_home, _guard) = temp_home();
        set("input_device", "USB Mic").expect("set a device");
        set("input_device", "none").expect("clear a device");
        let loaded = VoiceConfig::load().unwrap();
        assert_eq!(loaded.input_device, None);
    }

    #[test]
    fn every_config_key_can_be_set() {
        let sample = |value: &serde_json::Value| match value {
            serde_json::Value::Bool(_) => "true".to_string(),
            serde_json::Value::Number(number) if number.is_f64() => "2.0".to_string(),
            serde_json::Value::Number(_) => "1".to_string(),
            serde_json::Value::Null => "x".to_string(),
            _ => "/tmp/x".to_string(),
        };
        let defaults = serde_json::to_value(VoiceConfig::default()).unwrap();
        let map = defaults.as_object().unwrap();
        for (key, value) in map {
            let mut config = VoiceConfig::default();
            apply_setting(&mut config, key, &sample(value))
                .unwrap_or_else(|err| panic!("{key} is listed but not settable: {err}"));
        }
    }

    #[test]
    fn flags_are_written_back_so_the_next_run_needs_none() {
        let (_home, _guard) = temp_home();
        let overrides = OverrideArgs {
            language: Some("en".into()),
            input_device: Some("USB Mic".into()),
            threshold: Some(0.03),
            ..Default::default()
        };
        save_run_flags(&overrides);

        let path = VoiceConfig::config_path();
        assert!(path.is_file(), "a flag-provided value is persisted");
        let loaded = VoiceConfig::load().unwrap();
        assert_eq!(loaded.asr_language.as_deref(), Some("en"));
        assert_eq!(loaded.input_device.as_deref(), Some("USB Mic"));
        assert_eq!(loaded.rms_threshold, 0.03);
        assert!(!loaded.enabled, "the run flag is not baked into the file");
        assert_eq!(
            loaded.output_device, None,
            "untouched fields stay untouched"
        );

        save_run_flags(&OverrideArgs::default());
        let text = std::fs::read_to_string(&path).unwrap();
        assert!(
            text.contains("\"input_device\": \"USB Mic\""),
            "an empty flag set writes nothing: {text}"
        );
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

/// The samples of a 16 kHz mono 16-bit WAV. Anything else is refused by name rather than silently
/// resampled: comparing two models is only meaningful on identical audio.
fn wav_pcm(bytes: &[u8]) -> Result<Vec<i16>, String> {
    if bytes.len() < 44 || &bytes[..4] != b"RIFF" || &bytes[8..12] != b"WAVE" {
        return Err("not a RIFF/WAVE file".into());
    }
    let mut channels = 0u16;
    let mut rate = 0u32;
    let mut bits = 0u16;
    let mut offset = 12usize;
    while offset + 8 <= bytes.len() {
        let id = &bytes[offset..offset + 4];
        let size = u32::from_le_bytes(
            bytes[offset + 4..offset + 8]
                .try_into()
                .map_err(|_| "truncated chunk header".to_owned())?,
        ) as usize;
        let body = offset + 8;
        match id {
            b"fmt " => {
                if body + 16 > bytes.len() {
                    return Err("truncated fmt chunk".into());
                }
                channels = u16::from_le_bytes([bytes[body + 2], bytes[body + 3]]);
                rate = u32::from_le_bytes([
                    bytes[body + 4],
                    bytes[body + 5],
                    bytes[body + 6],
                    bytes[body + 7],
                ]);
                bits = u16::from_le_bytes([bytes[body + 14], bytes[body + 15]]);
            }
            b"data" => {
                if channels != 1 || rate != 16_000 || bits != 16 {
                    return Err(format!(
                        "need 16 kHz mono 16-bit audio, got {rate} Hz, {channels} channel(s), {bits}-bit"
                    ));
                }
                let end = (body + size).min(bytes.len());
                return Ok(bytes[body..end]
                    .as_chunks::<2>()
                    .0
                    .iter()
                    .map(|pair| i16::from_le_bytes(*pair))
                    .collect());
            }
            _ => {}
        }
        offset = body + size + (size & 1);
    }
    Err("no data chunk in the wav".into())
}
