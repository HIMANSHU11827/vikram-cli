use crate::cancel::CancelToken;
use crate::capture::DeviceSource;
use crate::helpers::{
    Scratch, first_available, missing_tool_message, pcm_from_bytes, run_cancellable, which,
    write_wav,
};
use crate::player::DevicePlayer;
use crate::vad::detect_speech;
use crate::{AsrEngine, Result, StageStatus, VoiceConfig, VoiceError, VoiceStatus, probe};
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

/// One bounded capture. Implementations must return 16 kHz mono s16le samples.
pub trait AudioSource: Send {
    fn record(&mut self, max_ms: u64) -> Result<Vec<i16>>;

    /// Capture with cooperative cancellation. The default checks once and delegates; engines
    /// that can abort mid-capture override this so a cancel lands within one poll interval.
    fn record_cancellable(&mut self, max_ms: u64, cancel: &CancelToken) -> Result<Vec<i16>> {
        cancel.check()?;
        self.record(max_ms)
    }
}

/// Where a session is in its listen → transcribe → speak cycle. Idle is both the start and
/// the abort state: any cancelled or failed stage returns here before the error surfaces.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum VoicePhase {
    Idle,
    Capturing,
    Transcribing,
    Speaking,
}

/// Turn speech into text. `Ok(None)` means silence or an empty transcript, not failure.
pub trait Recognizer: Send {
    fn transcribe(&mut self, pcm: &[i16]) -> Result<Option<String>>;

    /// Transcribe with cooperative cancellation. Engines that can abort mid-inference override
    /// this; the default checks once and delegates.
    fn transcribe_cancellable(
        &mut self,
        pcm: &[i16],
        cancel: &crate::CancelToken,
    ) -> Result<Option<String>> {
        cancel.check()?;
        self.transcribe(pcm)
    }

    /// Degrade notes accumulated at run time (engine failures, fallbacks taken). Taking them
    /// clears them; the session merges them into `VoiceStatus` instead of swallowing them.
    fn notes(&mut self) -> Vec<String> {
        Vec::new()
    }
}

/// Turn the model's reply text into WAV bytes.
pub trait Synthesizer: Send {
    fn synthesize(&self, text: &str) -> Result<Vec<u8>>;

    /// Synthesize with cooperative cancellation. Engines that can abort mid-run (a helper
    /// process, an inference loop) override this so a cancel lands before the clip is finished.
    fn synthesize_cancellable(&self, text: &str, cancel: &CancelToken) -> Result<Vec<u8>> {
        cancel.check()?;
        self.synthesize(text)
    }

    /// Degrade notes accumulated at run time (voice resolution, fallbacks taken). Unlike
    /// [`Recognizer::notes`] these are not taken: which voice is speaking belongs in every
    /// status, not only the first one after a failure.
    fn notes(&mut self) -> Vec<String> {
        Vec::new()
    }
}

/// Play WAV bytes.
pub trait Player: Send {
    fn play(&mut self, wav: &[u8]) -> Result<()>;

    /// Play with cooperative cancellation: engines that can stop mid-playback override this so
    /// a cancel lands within one poll interval instead of after the whole clip.
    fn play_cancellable(&mut self, wav: &[u8], cancel: &CancelToken) -> Result<()> {
        cancel.check()?;
        self.play(wav)
    }
}

pub struct HelperSource {
    capture: String,
    fallback: String,
    sample_rate: u16,
}

impl HelperSource {
    pub fn new(config: &VoiceConfig) -> Self {
        Self {
            capture: config.capture_helper.clone(),
            fallback: config.capture_fallback.clone(),
            sample_rate: config.sample_rate,
        }
    }
}

impl AudioSource for HelperSource {
    fn record(&mut self, max_ms: u64) -> Result<Vec<i16>> {
        self.record_cancellable(max_ms, &CancelToken::new())
    }

    fn record_cancellable(&mut self, max_ms: u64, cancel: &CancelToken) -> Result<Vec<i16>> {
        cancel.check()?;
        let helper = first_available(&[self.capture.as_str(), self.fallback.as_str()])
            .ok_or_else(|| VoiceError::MissingHelper(missing_tool_message(&self.capture)))?;
        let seconds = max_ms.div_ceil(1000).max(1).to_string();
        let rate = self.sample_rate.to_string();
        let args: Vec<&str> = if helper == "parecord" {
            vec![
                "--file-format=raw",
                "--rate",
                &rate,
                "--channels=1",
                "--endian=little",
            ]
        } else {
            vec!["-f", "S16_LE", "-r", &rate, "-c", "1"]
        };
        let mut bounded = vec!["--signal=INT", &seconds, helper.as_str()];
        bounded.extend_from_slice(&args);
        let bytes = run_cancellable(
            "timeout",
            &bounded,
            None,
            seconds.parse::<u64>().unwrap_or(30) + 2,
            cancel,
        )?;
        pcm_from_bytes(&bytes)
    }
}

pub struct HelperRecognizer {
    helper: String,
    model: PathBuf,
    timeout_secs: u64,
    sample_rate: u16,
}

impl HelperRecognizer {
    pub fn new(config: &VoiceConfig) -> Self {
        Self {
            helper: config.asr_helper.clone(),
            model: config.asr_model.clone(),
            timeout_secs: config.asr_timeout_secs,
            sample_rate: config.sample_rate,
        }
    }
}

impl Recognizer for HelperRecognizer {
    fn transcribe(&mut self, pcm: &[i16]) -> Result<Option<String>> {
        self.transcribe_cancellable(pcm, &CancelToken::new())
    }

    fn transcribe_cancellable(
        &mut self,
        pcm: &[i16],
        cancel: &CancelToken,
    ) -> Result<Option<String>> {
        cancel.check()?;
        if self.model.as_os_str().is_empty() || !self.model.is_file() {
            let reason = if self.model.as_os_str().is_empty() {
                "asr_model is not configured".into()
            } else {
                format!("asr_model {} is not a file", self.model.display())
            };
            return Err(VoiceError::ModelMissing(reason));
        }
        if which(&self.helper).is_none() {
            return Err(VoiceError::MissingHelper(missing_tool_message(
                &self.helper,
            )));
        }
        let scratch = Scratch::new()?;
        let wav = scratch.path().join("utterance.wav");
        write_wav(&wav, pcm, self.sample_rate)?;
        let model = self.model.to_string_lossy();
        let wav = wav.to_string_lossy();
        let output = scratch.path().join("transcript");
        let output_name = output.to_string_lossy();
        let args = [
            "-m",
            &model,
            "-f",
            &wav,
            "-otxt",
            "-of",
            &output_name,
            "--no-prints",
        ];
        run_cancellable(&self.helper, &args, None, self.timeout_secs, cancel)?;
        let text = fs::read_to_string(output.with_extension("txt"))
            .map_err(|err| VoiceError::Helper(format!("read transcript: {err}")))?;
        let text = text.trim();
        Ok((!text.is_empty()).then(|| text.to_owned()))
    }
}

/// Piper voice models already on disk: `*.onnx` under `<vikram home>/models/piper`, then under
/// `~/.local/share/piper/voices` when that directory exists. Sorted, so "the first voice" means
/// the same thing on every run. Piper also needs the `.onnx.json` beside a voice; discovery does
/// not depend on it, so a half-installed voice is still visible here.
pub fn piper_voices() -> Vec<PathBuf> {
    let mut dirs = vec![crate::models_dir().join("piper")];
    if let Some(home) = std::env::var_os("HOME").filter(|home| !home.is_empty()) {
        dirs.push(PathBuf::from(home).join(".local/share/piper/voices"));
    }
    voices_in(&dirs)
}

fn voices_in(dirs: &[PathBuf]) -> Vec<PathBuf> {
    let mut found: Vec<PathBuf> = dirs
        .iter()
        .flat_map(|dir| fs::read_dir(dir).into_iter().flatten())
        .flatten()
        .map(|entry| entry.path())
        .filter(|path| path.extension().is_some_and(|ext| ext == "onnx") && path.is_file())
        .collect();
    found.sort();
    found.dedup();
    found
}

/// The voice to speak with: an existing configured path wins, else the first discovered voice,
/// else none (espeak-ng speaks instead). A configured path that is gone falls back rather than
/// failing the stage.
pub fn pick_voice(configured: Option<&Path>) -> Option<PathBuf> {
    pick_voice_in(configured, piper_voices())
}

fn pick_voice_in(configured: Option<&Path>, discovered: Vec<PathBuf>) -> Option<PathBuf> {
    configured
        .filter(|path| path.is_file())
        .map(Path::to_path_buf)
        .or_else(|| discovered.into_iter().next())
}

/// The part of a reply worth hearing. Replies are written for the screen: markdown layout,
/// backticked identifiers, file references and emoji all read badly aloud, so they are removed
/// or spelled out before synthesis.
pub fn spoken_text(text: &str) -> String {
    let text = strip_links(&strip_fences(text));
    let mut out = String::with_capacity(text.len());
    let mut blank = false;
    for line in text.lines() {
        let line = clean_line(line);
        if line.is_empty() {
            if !blank && !out.is_empty() {
                out.push('\n');
            }
            blank = true;
            continue;
        }
        blank = false;
        out.push_str(&speak_line(&line));
        out.push('\n');
    }
    out.trim().to_owned()
}

fn strip_fences(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut fenced = false;
    for line in text.lines() {
        if line.trim_start().starts_with("```") {
            fenced = !fenced;
            continue;
        }
        if !fenced {
            out.push_str(line);
            out.push('\n');
        }
    }
    out
}

fn strip_links(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(open) = rest.find('[') {
        out.push_str(&rest[..open]);
        let after = &rest[open + 1..];
        let Some(close) = after.find(']') else {
            out.push_str(&rest[open..]);
            return out;
        };
        let tail = &after[close + 1..];
        let after_paren = tail
            .strip_prefix('(')
            .and_then(|tail| tail.find(')').map(|end| &tail[end + 1..]));
        let Some(after_paren) = after_paren else {
            out.push('[');
            rest = after;
            continue;
        };
        if out.ends_with('!') {
            out.pop();
        }
        out.push_str(&after[..close]);
        rest = after_paren;
    }
    out.push_str(rest);
    out
}

fn clean_line(line: &str) -> String {
    let line = strip_line_marker(line);
    let mut out = String::with_capacity(line.len());
    let mut spaced = false;
    for c in line.chars() {
        let c = match c {
            '`' | '*' => continue,
            '\t' => ' ',
            c if is_emoji(c) => continue,
            c => c,
        };
        if c == ' ' {
            if spaced || out.is_empty() {
                continue;
            }
            spaced = true;
        } else {
            spaced = false;
        }
        out.push(c);
    }
    out.trim_end().to_owned()
}

fn strip_line_marker(line: &str) -> &str {
    let mut rest = line.trim_start_matches('#').trim_start();
    while let Some(quoted) = rest.strip_prefix('>') {
        rest = quoted.trim_start();
    }
    for marker in ["- ", "* ", "+ ", "\u{2022} "] {
        if let Some(after) = rest.strip_prefix(marker) {
            return after.trim_start();
        }
    }
    let digits = rest.chars().take_while(|c| c.is_ascii_digit()).count();
    if digits > 0 {
        let tail = &rest[digits..];
        for marker in [". ", ") "] {
            if let Some(after) = tail.strip_prefix(marker) {
                return after.trim_start();
            }
        }
    }
    rest
}

fn speak_line(line: &str) -> String {
    line.split_whitespace()
        .map(speak_token)
        .collect::<Vec<_>>()
        .join(" ")
}

fn speak_token(word: &str) -> String {
    let (core, tail) = split_trailing(word);
    match speak_file_ref(core) {
        Some(spoken) => format!("{spoken}{tail}"),
        None => word.to_owned(),
    }
}

/// Punctuation a word may carry without changing what it is.
fn split_trailing(word: &str) -> (&str, &str) {
    let end = word
        .char_indices()
        .rev()
        .find(|(_, c)| {
            !matches!(
                *c,
                '.' | ',' | ';' | ':' | '!' | '?' | ')' | ']' | '}' | '"' | '\''
            )
        })
        .map_or(0, |(index, c)| index + c.len_utf8());
    word.split_at(end)
}

/// A file reference read aloud: `session.rs:123` becomes "session dot rs line 123", and a long
/// path is cut to its last segment so the listener hears a name, not a directory tree.
fn speak_file_ref(token: &str) -> Option<String> {
    let (core, lines) = split_line_suffix(token);
    let segments: Vec<&str> = core.split('/').filter(|part| !part.is_empty()).collect();
    let name = *segments.last()?;
    match file_parts(name) {
        Some((stem, ext)) => {
            let mut spoken = format!("{stem} dot {ext}");
            if let Some(lines) = lines {
                spoken.push_str(&line_phrase(lines));
            }
            Some(spoken)
        }
        None if segments.len() >= 3 || core.len() > 24 => Some(name.to_owned()),
        None => None,
    }
}

fn split_line_suffix(token: &str) -> (&str, Option<&str>) {
    match token.split_once(':') {
        Some((core, lines))
            if !core.is_empty()
                && !lines.is_empty()
                && lines
                    .split(':')
                    .all(|part| !part.is_empty() && part.bytes().all(|b| b.is_ascii_digit())) =>
        {
            (core, Some(lines))
        }
        _ => (token, None),
    }
}

fn line_phrase(lines: &str) -> String {
    let mut parts = lines.split(':');
    let line = parts.next().unwrap_or_default();
    match parts.next() {
        Some(column) => format!(" line {line} column {column}"),
        None => format!(" line {line}"),
    }
}

fn file_parts(name: &str) -> Option<(&str, &str)> {
    let (stem, ext) = name.rsplit_once('.')?;
    (!stem.is_empty() && is_extension(ext)).then_some((stem, ext))
}

fn is_extension(ext: &str) -> bool {
    ext.chars().all(|c| c.is_ascii_alphanumeric())
        && (matches!(ext.len(), 2..=5) || matches!(ext, "c" | "h" | "o" | "a" | "m"))
}

fn is_emoji(c: char) -> bool {
    matches!(
        c as u32,
        0x200D | 0x20E3 | 0xFE0E | 0xFE0F | 0x2600..=0x27BF | 0x2B00..=0x2BFF | 0x1F000..=0x1FAFF
    )
}

fn voice_label(path: &Path) -> String {
    path.file_stem()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_else(|| path.display().to_string())
}

/// Said once, at construction: a voice found rather than configured is worth announcing, and a
/// configured path that vanished is worth naming instead of silently replacing.
fn voice_note(configured: Option<&Path>, picked: Option<&Path>) -> Option<String> {
    let picked = picked?;
    match configured {
        Some(path) if path == picked => None,
        Some(path) => Some(format!(
            "tts_voice {} is not a file; using piper voice {}",
            path.display(),
            voice_label(picked)
        )),
        None => Some(format!("using piper voice {}", voice_label(picked))),
    }
}

/// The espeak-ng note names the exact directory a voice goes in, because "install a model" is
/// not an instruction an operator can follow.
fn degrade_note(piper_installed: bool) -> String {
    let dir = crate::models_dir().join("piper");
    if piper_installed {
        format!(
            "no piper voice model; espeak-ng is speaking instead — drop a .onnx voice (and its              .onnx.json) into {}",
            dir.display()
        )
    } else {
        format!(
            "piper is not on PATH; espeak-ng is speaking instead — install piper, then drop a              .onnx voice (and its .onnx.json) into {}",
            dir.display()
        )
    }
}

/// The TTS half: a piper voice when one is configured or discoverable, espeak-ng otherwise.
/// Playback is not its job — it returns WAV bytes for the player stage.
pub struct HelperSynth {
    helper: String,
    fallback: String,
    voice: Option<PathBuf>,
    rate: u32,
    volume: f32,
    notes: Mutex<Vec<String>>,
}

impl HelperSynth {
    pub fn new(config: &VoiceConfig) -> Self {
        let voice = pick_voice(config.tts_voice.as_deref());
        let notes = voice_note(config.tts_voice.as_deref(), voice.as_deref())
            .into_iter()
            .collect();
        Self {
            helper: config.tts_helper.clone(),
            fallback: config.tts_fallback.clone(),
            voice,
            rate: config.rate,
            volume: config.volume,
            notes: Mutex::new(notes),
        }
    }

    /// The voice this synthesizer resolved: the configured file, a discovered one, or none.
    pub fn voice(&self) -> Option<&Path> {
        self.voice.as_deref()
    }

    fn note(&self, text: String) {
        let mut notes = self.notes.lock().unwrap_or_else(|err| err.into_inner());
        if !notes.contains(&text) {
            notes.push(text);
        }
    }

    /// piper wants the reciprocal of a word rate: 175 wpm is `--length_scale 1.0`, 350 is 0.5.
    fn length_scale(&self) -> String {
        format!("{:.2}", 175.0 / self.rate.max(1) as f32)
    }

    /// espeak-ng's `-a` runs 0..=200 for the same range this config calls 0..=1.
    fn espeak_amplitude(&self) -> String {
        ((self.volume.clamp(0.0, 1.0) * 200.0).round() as u32).to_string()
    }
}

impl Synthesizer for HelperSynth {
    fn synthesize(&self, text: &str) -> Result<Vec<u8>> {
        self.synthesize_cancellable(text, &CancelToken::new())
    }

    fn synthesize_cancellable(&self, text: &str, cancel: &CancelToken) -> Result<Vec<u8>> {
        cancel.check()?;
        let piper = which(&self.helper).is_some();
        let espeak = which(&self.fallback).is_some();
        let scratch = Scratch::new()?;
        let output = scratch.path().join("reply.wav");
        let output_name = output.to_string_lossy();
        if let Some(voice) = self.voice.as_deref()
            && piper
        {
            let voice_name = voice.to_string_lossy();
            let length_scale = self.length_scale();
            let volume = format!("{:.2}", self.volume.clamp(0.0, 1.0));
            let args = [
                "--model",
                &voice_name,
                "--output_file",
                &output_name,
                "--length_scale",
                &length_scale,
                "--volume",
                &volume,
            ];
            run_cancellable(&self.helper, &args, Some(text.as_bytes()), 60, cancel)?;
        } else if espeak {
            self.note(degrade_note(piper));
            let rate = self.rate.to_string();
            let amplitude = self.espeak_amplitude();
            let args = [
                "-s",
                &rate,
                "-a",
                &amplitude,
                "-w",
                &output_name,
                "--",
                text,
            ];
            run_cancellable(&self.fallback, &args, None, 60, cancel)?;
        } else {
            return Err(VoiceError::MissingHelper(missing_tool_message(
                &self.helper,
            )));
        }
        fs::read(&output).map_err(|err| VoiceError::Helper(format!("read speech: {err}")))
    }

    fn notes(&mut self) -> Vec<String> {
        self.notes
            .lock()
            .map(|notes| notes.clone())
            .unwrap_or_else(|err| err.into_inner().clone())
    }
}

pub struct HelperPlayer {
    helper: String,
    fallback: String,
}

impl HelperPlayer {
    pub fn new(config: &VoiceConfig) -> Self {
        Self {
            helper: config.player_helper.clone(),
            fallback: config.player_fallback.clone(),
        }
    }
}

impl Player for HelperPlayer {
    fn play(&mut self, wav: &[u8]) -> Result<()> {
        self.play_cancellable(wav, &CancelToken::new())
    }

    fn play_cancellable(&mut self, wav: &[u8], cancel: &CancelToken) -> Result<()> {
        cancel.check()?;
        let player = first_available(&[self.helper.as_str(), self.fallback.as_str()])
            .ok_or_else(|| VoiceError::MissingHelper(missing_tool_message(&self.helper)))?;
        run_cancellable(&player, &[], Some(wav), 60, cancel)?;
        Ok(())
    }
}

/// A push-to-talk loop. The four stages are traits so tests run without a microphone or model.
/// The default source is the built-in cpal engine, the default recognizer the built-in whisper
/// chain, and the default player the built-in cpal output; helper binaries are only fallback
/// tiers inside those engines.
pub struct VoiceSession<S = DeviceSource, R = AsrEngine, Y = HelperSynth, P = DevicePlayer> {
    config: VoiceConfig,
    cancel: CancelToken,
    phase: VoicePhase,
    source: S,
    recognizer: R,
    synth: Y,
    player: P,
}

impl VoiceSession {
    pub fn start(config: VoiceConfig) -> Result<Self> {
        config.validate()?;
        if !config.enabled {
            return Err(VoiceError::Disabled);
        }
        if let StageStatus::Missing(reason) = probe(&config).capture {
            return Err(VoiceError::MissingHelper(reason));
        }
        Ok(Self::with_parts(
            config.clone(),
            DeviceSource::new(&config),
            AsrEngine::new(&config),
            HelperSynth::new(&config),
            DevicePlayer::new(&config),
        ))
    }
}

impl<S, R, Y, P> VoiceSession<S, R, Y, P>
where
    S: AudioSource,
    R: Recognizer,
    Y: Synthesizer,
    P: Player,
{
    pub fn with_parts(config: VoiceConfig, source: S, recognizer: R, synth: Y, player: P) -> Self {
        Self {
            config,
            cancel: CancelToken::new(),
            phase: VoicePhase::Idle,
            source,
            recognizer,
            synth,
            player,
        }
    }

    /// Ask every stage of this session to stop what it is doing. Safe to call from any thread.
    pub fn cancel(&self) {
        self.cancel.cancel();
    }

    /// The session's cancel token, for callers that wire their own interrupt sources (a signal
    /// handler, a keypress watcher) to this session.
    pub fn cancel_token(&self) -> CancelToken {
        self.cancel.clone()
    }

    /// Where the session is right now. `Idle` after every abort, success or failure.
    pub fn phase(&self) -> VoicePhase {
        self.phase
    }

    /// Clear the cancel flag and return to `Idle`, for reuse after a completed or aborted turn.
    pub fn reset(&mut self) {
        self.cancel.reset();
        self.phase = VoicePhase::Idle;
    }

    pub fn listen_once(&mut self) -> Result<Option<String>> {
        self.phase = VoicePhase::Capturing;
        let turn = self.listen_turn();
        if turn.is_err() {
            self.phase = VoicePhase::Idle;
        }
        turn
    }

    fn listen_turn(&mut self) -> Result<Option<String>> {
        let pcm = self
            .source
            .record_cancellable(self.config.max_record_ms, &self.cancel)?;
        if pcm.len() < self.config.sample_rate as usize / 100 {
            self.phase = VoicePhase::Idle;
            return Ok(None);
        }
        let Some((start, end)) = detect_speech(
            &pcm,
            self.config.rms_threshold,
            self.config.floor_ratio,
            self.config.min_speech_ms,
            self.config.hangover_ms,
            self.config.sample_rate,
        ) else {
            self.phase = VoicePhase::Idle;
            return Ok(None);
        };
        self.phase = VoicePhase::Transcribing;
        let turn = self
            .recognizer
            .transcribe_cancellable(&pcm[start..end], &self.cancel);
        self.phase = VoicePhase::Idle;
        turn
    }

    /// Speak a reply. Interruptible: a cancel during synthesis or playback returns
    /// [`VoiceError::Cancelled`], which callers tell apart from the empty-reply refusal.
    pub fn speak(&mut self, text: &str) -> Result<()> {
        self.cancel.check()?;
        if text.trim().is_empty() {
            return Err(VoiceError::Config(
                "refusing to speak an empty reply".into(),
            ));
        }
        let spoken = spoken_text(text);
        if spoken.is_empty() {
            return Err(VoiceError::Config(
                "refusing to speak a reply with nothing but layout in it".into(),
            ));
        }
        self.phase = VoicePhase::Speaking;
        let turn = self.speak_turn(&spoken);
        self.phase = VoicePhase::Idle;
        turn
    }

    fn speak_turn(&mut self, text: &str) -> Result<()> {
        let wav = self.synth.synthesize_cancellable(text, &self.cancel)?;
        if wav.is_empty() {
            return Err(VoiceError::InvalidAudio(
                "synthesizer produced no audio".into(),
            ));
        }
        self.player.play_cancellable(&wav, &self.cancel)
    }

    /// Probe the stages, plus every run-time degrade note the engines have accumulated.
    pub fn status(&mut self) -> VoiceStatus {
        let mut status = probe(&self.config);
        status.notes.extend(self.recognizer.notes());
        status.notes.extend(self.synth.notes());
        status
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::{Arc, Mutex};

    struct FakeSource(Vec<i16>);
    impl AudioSource for FakeSource {
        fn record(&mut self, _: u64) -> Result<Vec<i16>> {
            Ok(self.0.clone())
        }
    }

    struct FakeRecognizer {
        text: Option<String>,
        calls: Arc<AtomicUsize>,
    }
    impl Recognizer for FakeRecognizer {
        fn transcribe(&mut self, _: &[i16]) -> Result<Option<String>> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            Ok(self.text.clone())
        }
    }

    struct FakeSynth(Vec<u8>);
    impl Synthesizer for FakeSynth {
        fn synthesize(&self, _: &str) -> Result<Vec<u8>> {
            Ok(self.0.clone())
        }
    }

    struct FakePlayer(Arc<Mutex<Vec<u8>>>);
    impl Player for FakePlayer {
        fn play(&mut self, wav: &[u8]) -> Result<()> {
            *self.0.lock().unwrap() = wav.to_vec();
            Ok(())
        }
    }

    type FakeSession = VoiceSession<FakeSource, FakeRecognizer, FakeSynth, FakePlayer>;

    fn session(
        pcm: Vec<i16>,
        transcript: Option<&str>,
    ) -> (FakeSession, Arc<AtomicUsize>, Arc<Mutex<Vec<u8>>>) {
        let calls = Arc::new(AtomicUsize::new(0));
        let heard = Arc::new(Mutex::new(Vec::new()));
        let config = VoiceConfig {
            enabled: true,
            ..Default::default()
        };
        let session = VoiceSession::with_parts(
            config,
            FakeSource(pcm),
            FakeRecognizer {
                text: transcript.map(str::to_owned),
                calls: calls.clone(),
            },
            FakeSynth(b"RIFFfake".to_vec()),
            FakePlayer(heard.clone()),
        );
        (session, calls, heard)
    }

    /// A word inside a quiet room: the gate needs a floor to measure against and a burst long
    /// enough to count as speech, so a bare tone is not a valid fixture any more.
    fn speech() -> Vec<i16> {
        let mut pcm = vec![0i16; 1_600];
        pcm.extend((0..4_800).map(|i| ((i as f64 * 0.08).sin() * 16_000.0) as i16));
        pcm.extend(vec![0i16; 1_600]);
        pcm
    }

    #[test]
    fn silence_never_reaches_the_recognizer() {
        let (mut session, calls, _) = session(vec![0; 3_200], Some("hello"));
        assert_eq!(session.listen_once().unwrap(), None);
        assert_eq!(calls.load(Ordering::SeqCst), 0);
    }

    #[test]
    fn speech_becomes_a_transcript() {
        let (mut session, calls, _) = session(speech(), Some("hello vikram"));
        assert_eq!(
            session.listen_once().unwrap().as_deref(),
            Some("hello vikram")
        );
        assert_eq!(calls.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn an_empty_transcript_is_silence_not_an_error() {
        let (mut session, _, _) = session(speech(), None);
        assert_eq!(session.listen_once().unwrap(), None);
    }

    #[test]
    fn speak_delivers_real_audio_and_refuses_empty_text() {
        let (mut session, _, heard) = session(Vec::new(), None);
        session.speak("  ").unwrap_err();
        session.speak("turn on the lights").unwrap();
        assert_eq!(*heard.lock().unwrap(), b"RIFFfake".to_vec());
    }

    #[test]
    fn start_requires_explicit_opt_in() {
        assert!(matches!(
            VoiceSession::start(VoiceConfig::default()),
            Err(VoiceError::Disabled)
        ));
    }

    #[test]
    fn cancel_before_listen_aborts_without_touching_the_source() {
        struct CountingSource(Arc<AtomicUsize>);
        impl AudioSource for CountingSource {
            fn record(&mut self, _: u64) -> Result<Vec<i16>> {
                self.0.fetch_add(1, Ordering::SeqCst);
                Ok(Vec::new())
            }
        }
        let calls = Arc::new(AtomicUsize::new(0));
        let heard = Arc::new(Mutex::new(Vec::new()));
        let config = VoiceConfig {
            enabled: true,
            ..Default::default()
        };
        let mut session = VoiceSession::with_parts(
            config,
            CountingSource(calls.clone()),
            FakeRecognizer {
                text: None,
                calls: Arc::new(AtomicUsize::new(0)),
            },
            FakeSynth(Vec::new()),
            FakePlayer(heard),
        );
        session.cancel();
        assert!(matches!(session.listen_once(), Err(VoiceError::Cancelled)));
        assert_eq!(calls.load(Ordering::SeqCst), 0);
    }

    struct BlockingSource {
        calls: Arc<AtomicUsize>,
    }

    impl AudioSource for BlockingSource {
        fn record(&mut self, _: u64) -> Result<Vec<i16>> {
            Err(VoiceError::Cancelled)
        }

        fn record_cancellable(&mut self, _: u64, cancel: &crate::CancelToken) -> Result<Vec<i16>> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            abort_on_cancel(cancel)?;
            Ok(Vec::new())
        }
    }

    struct BlockingRecognizer {
        calls: Arc<AtomicUsize>,
    }

    impl Recognizer for BlockingRecognizer {
        fn transcribe(&mut self, _: &[i16]) -> Result<Option<String>> {
            Err(VoiceError::Cancelled)
        }

        fn transcribe_cancellable(
            &mut self,
            _: &[i16],
            cancel: &crate::CancelToken,
        ) -> Result<Option<String>> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            abort_on_cancel(cancel)?;
            Ok(None)
        }
    }

    struct BlockingPlayer {
        calls: Arc<AtomicUsize>,
    }

    impl Player for BlockingPlayer {
        fn play(&mut self, _: &[u8]) -> Result<()> {
            Err(VoiceError::Cancelled)
        }

        fn play_cancellable(&mut self, _: &[u8], cancel: &crate::CancelToken) -> Result<()> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            abort_on_cancel(cancel)
        }
    }

    /// The engine contract: poll the token, not the deadline; a missing cancel must trip the
    /// test's safety fuse instead of hanging the suite.
    fn abort_on_cancel(cancel: &crate::CancelToken) -> Result<()> {
        let started = std::time::Instant::now();
        while !cancel.is_cancelled() {
            assert!(
                started.elapsed() < std::time::Duration::from_secs(5),
                "cancellation was not honored within 5s"
            );
            std::thread::sleep(std::time::Duration::from_millis(2));
        }
        Err(VoiceError::Cancelled)
    }
    fn cancel_soon(token: crate::CancelToken, after_ms: u64) {
        std::thread::spawn(move || {
            std::thread::sleep(std::time::Duration::from_millis(after_ms));
            token.cancel();
        });
    }

    #[test]
    fn cancel_during_capture_aborts_before_transcription() {
        let source_calls = Arc::new(AtomicUsize::new(0));
        let recognizer_calls = Arc::new(AtomicUsize::new(0));
        let config = VoiceConfig {
            enabled: true,
            ..Default::default()
        };
        let mut session = VoiceSession::with_parts(
            config,
            BlockingSource {
                calls: source_calls.clone(),
            },
            BlockingRecognizer {
                calls: recognizer_calls.clone(),
            },
            FakeSynth(Vec::new()),
            FakePlayer(Arc::new(Mutex::new(Vec::new()))),
        );
        cancel_soon(session.cancel_token(), 30);
        assert!(matches!(session.listen_once(), Err(VoiceError::Cancelled)));
        assert_eq!(session.phase(), VoicePhase::Idle);
        assert_eq!(source_calls.load(Ordering::SeqCst), 1);
        assert_eq!(recognizer_calls.load(Ordering::SeqCst), 0);
    }

    #[test]
    fn cancel_during_transcription_aborts_before_playback() {
        let recognizer_calls = Arc::new(AtomicUsize::new(0));
        let player_calls = Arc::new(AtomicUsize::new(0));
        let config = VoiceConfig {
            enabled: true,
            ..Default::default()
        };
        let mut session = VoiceSession::with_parts(
            config,
            FakeSource(speech()),
            BlockingRecognizer {
                calls: recognizer_calls.clone(),
            },
            FakeSynth(b"RIFFfake".to_vec()),
            BlockingPlayer {
                calls: player_calls.clone(),
            },
        );
        cancel_soon(session.cancel_token(), 30);
        assert!(matches!(session.listen_once(), Err(VoiceError::Cancelled)));
        assert_eq!(session.phase(), VoicePhase::Idle);
        assert_eq!(recognizer_calls.load(Ordering::SeqCst), 1);
        assert_eq!(player_calls.load(Ordering::SeqCst), 0);
    }

    #[test]
    fn cancel_during_playback_stops_the_speaker() {
        let player_calls = Arc::new(AtomicUsize::new(0));
        let config = VoiceConfig {
            enabled: true,
            ..Default::default()
        };
        let mut session = VoiceSession::with_parts(
            config,
            FakeSource(Vec::new()),
            FakeRecognizer {
                text: None,
                calls: Arc::new(AtomicUsize::new(0)),
            },
            FakeSynth(b"RIFFfake".to_vec()),
            BlockingPlayer {
                calls: player_calls.clone(),
            },
        );
        cancel_soon(session.cancel_token(), 30);
        assert!(matches!(
            session.speak("turn on the lights"),
            Err(VoiceError::Cancelled)
        ));
        assert_eq!(session.phase(), VoicePhase::Idle);
        assert_eq!(player_calls.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn reset_clears_the_cancel_for_the_next_turn() {
        let calls = Arc::new(AtomicUsize::new(0));
        let config = VoiceConfig {
            enabled: true,
            ..Default::default()
        };
        let mut session = VoiceSession::with_parts(
            config,
            FakeSource(speech()),
            FakeRecognizer {
                text: Some("hello".into()),
                calls: calls.clone(),
            },
            FakeSynth(b"RIFFfake".to_vec()),
            FakePlayer(Arc::new(Mutex::new(Vec::new()))),
        );
        session.cancel();
        assert!(matches!(session.listen_once(), Err(VoiceError::Cancelled)));
        session.reset();
        assert_eq!(session.phase(), VoicePhase::Idle);
        assert_eq!(session.listen_once().unwrap().as_deref(), Some("hello"));
    }

    #[test]
    fn spoken_text_strips_markdown_layout() {
        let reply = "# Build status\n\n- **done** in `session.rs:12`\n- see [the plan](https://example.com/plan.md)\n\n```rust\nlet x = 1;\n```\n";
        assert_eq!(
            spoken_text(reply),
            "Build status\n\ndone in session dot rs line 12\nsee the plan"
        );
    }

    #[test]
    fn spoken_text_spells_out_file_references() {
        assert_eq!(
            spoken_text(
                "open /home/himanshu/Desktop/VIKRAM-CLI/crates/vikram-voice/src/session.rs:123 now"
            ),
            "open session dot rs line 123 now"
        );
        assert_eq!(spoken_text("edit file.rs:12"), "edit file dot rs line 12");
        assert_eq!(
            spoken_text("the panic at main.rs:40:7"),
            "the panic at main dot rs line 40 column 7"
        );
        assert_eq!(spoken_text("see Cargo.toml"), "see Cargo dot toml");
        assert_eq!(
            spoken_text("and/or 24/7 TCP/IP stay words"),
            "and/or 24/7 TCP/IP stay words"
        );
    }

    #[test]
    fn spoken_text_drops_emoji_and_collapses_blank_lines() {
        assert_eq!(spoken_text("ship it 🚀 ✅"), "ship it");
        assert_eq!(spoken_text("a\n\n\n\nb"), "a\n\nb");
        assert_eq!(spoken_text("   \n\n"), "");
        assert_eq!(spoken_text("plain reply"), "plain reply");
    }

    #[test]
    fn voices_are_found_sorted_and_only_onnx() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("b_voice.onnx"), b"x").unwrap();
        fs::write(dir.path().join("a_voice.onnx"), b"x").unwrap();
        fs::write(dir.path().join("a_voice.onnx.json"), b"{}").unwrap();
        fs::write(dir.path().join("notes.txt"), b"x").unwrap();
        assert_eq!(
            voices_in(&[dir.path().to_path_buf()]),
            vec![
                dir.path().join("a_voice.onnx"),
                dir.path().join("b_voice.onnx")
            ]
        );
        assert!(voices_in(&[dir.path().join("absent")]).is_empty());
    }

    #[test]
    fn an_existing_configured_voice_wins_over_a_discovered_one() {
        let dir = tempfile::tempdir().unwrap();
        let configured = dir.path().join("chosen.onnx");
        let discovered = dir.path().join("other.onnx");
        fs::write(&configured, b"x").unwrap();
        fs::write(&discovered, b"x").unwrap();
        assert_eq!(
            pick_voice_in(Some(configured.as_path()), vec![discovered.clone()]),
            Some(configured)
        );
    }

    #[test]
    fn a_missing_configured_voice_falls_back_to_a_discovered_one() {
        let dir = tempfile::tempdir().unwrap();
        let discovered = dir.path().join("en_US-lessac-medium.onnx");
        fs::write(&discovered, b"x").unwrap();
        let missing = dir.path().join("gone.onnx");
        assert_eq!(
            pick_voice_in(Some(missing.as_path()), vec![discovered.clone()]),
            Some(discovered.clone())
        );
        assert_eq!(pick_voice_in(Some(missing.as_path()), Vec::new()), None);
        assert_eq!(
            pick_voice_in(None, vec![discovered.clone()]),
            Some(discovered)
        );
        assert_eq!(pick_voice_in(None, Vec::new()), None);
    }

    #[test]
    fn the_piper_voice_is_announced_once_and_the_note_names_it() {
        let picked = PathBuf::from("/voices/en_US-lessac-medium.onnx");
        assert_eq!(
            voice_note(None, Some(picked.as_path())).as_deref(),
            Some("using piper voice en_US-lessac-medium")
        );
        assert_eq!(
            voice_note(Some(picked.as_path()), Some(picked.as_path())),
            None
        );
        assert_eq!(voice_note(None, None), None);
        let note = voice_note(Some(Path::new("/gone.onnx")), Some(picked.as_path())).unwrap();
        assert!(note.contains("/gone.onnx"), "{note}");
        assert!(note.contains("en_US-lessac-medium"), "{note}");
    }

    #[test]
    fn the_espeak_degrade_note_names_the_directory_to_drop_a_voice_into() {
        let note = degrade_note(true);
        let dir = crate::models_dir().join("piper");
        assert!(note.contains(&dir.display().to_string()), "{note}");
        assert!(note.contains(".onnx"), "{note}");
        let note = degrade_note(false);
        assert!(note.contains("PATH"), "{note}");
        assert!(note.contains(&dir.display().to_string()), "{note}");
    }

    #[test]
    fn speech_rate_and_volume_are_bounded() {
        let mut config = VoiceConfig::default();
        assert_eq!(config.rate, 175);
        assert_eq!(config.volume, 1.0);
        config.validate().expect("defaults are valid");
        config.rate = 79;
        assert!(config.validate().is_err());
        config.rate = 451;
        assert!(config.validate().is_err());
        config.rate = 450;
        config.validate().expect("450 wpm is the ceiling");
        config.volume = 1.5;
        assert!(config.validate().is_err());
        config.volume = -0.1;
        assert!(config.validate().is_err());
        config.volume = 0.0;
        config.validate().expect("silence is a legal volume");
    }

    #[test]
    fn the_synth_maps_rate_and_volume_onto_both_helpers() {
        let config = VoiceConfig {
            rate: 350,
            volume: 0.5,
            ..Default::default()
        };
        let synth = HelperSynth::new(&config);
        assert_eq!(synth.length_scale(), "0.50");
        assert_eq!(synth.espeak_amplitude(), "100");
        assert_eq!(
            HelperSynth::new(&VoiceConfig::default()).length_scale(),
            "1.00"
        );
    }

    struct RecordingSynth(Arc<Mutex<String>>);
    impl Synthesizer for RecordingSynth {
        fn synthesize(&self, text: &str) -> Result<Vec<u8>> {
            *self.0.lock().unwrap() = text.to_owned();
            Ok(b"RIFFfake".to_vec())
        }
    }

    #[test]
    fn speak_reads_the_shaped_reply_not_the_markdown() {
        let heard = Arc::new(Mutex::new(String::new()));
        let config = VoiceConfig {
            enabled: true,
            ..Default::default()
        };
        let mut session = VoiceSession::with_parts(
            config,
            FakeSource(Vec::new()),
            FakeRecognizer {
                text: None,
                calls: Arc::new(AtomicUsize::new(0)),
            },
            RecordingSynth(heard.clone()),
            FakePlayer(Arc::new(Mutex::new(Vec::new()))),
        );
        session
            .speak("## Build\n\n- **done** in `session.rs:12`")
            .unwrap();
        assert_eq!(
            *heard.lock().unwrap(),
            "Build\n\ndone in session dot rs line 12"
        );
    }

    #[test]
    fn a_reply_with_nothing_but_layout_is_refused_not_spoken() {
        let (mut session, _, heard) = session(Vec::new(), None);
        assert!(matches!(
            session.speak("```rust\nlet x = 1;\n```"),
            Err(VoiceError::Config(_))
        ));
        assert!(heard.lock().unwrap().is_empty());
    }

    struct SlowSynth(Arc<AtomicUsize>);
    impl Synthesizer for SlowSynth {
        fn synthesize(&self, _: &str) -> Result<Vec<u8>> {
            Err(VoiceError::Cancelled)
        }

        fn synthesize_cancellable(&self, _: &str, cancel: &crate::CancelToken) -> Result<Vec<u8>> {
            self.0.fetch_add(1, Ordering::SeqCst);
            abort_on_cancel(cancel)?;
            Ok(b"RIFFfake".to_vec())
        }
    }

    #[test]
    fn cancel_during_synthesis_returns_the_cancelled_status() {
        let synth_calls = Arc::new(AtomicUsize::new(0));
        let player_calls = Arc::new(AtomicUsize::new(0));
        let config = VoiceConfig {
            enabled: true,
            ..Default::default()
        };
        let mut session = VoiceSession::with_parts(
            config,
            FakeSource(Vec::new()),
            FakeRecognizer {
                text: None,
                calls: Arc::new(AtomicUsize::new(0)),
            },
            SlowSynth(synth_calls.clone()),
            BlockingPlayer {
                calls: player_calls.clone(),
            },
        );
        assert!(matches!(session.speak("  "), Err(VoiceError::Config(_))));
        cancel_soon(session.cancel_token(), 30);
        assert!(matches!(
            session.speak("turn on the lights"),
            Err(VoiceError::Cancelled)
        ));
        assert_eq!(session.phase(), VoicePhase::Idle);
        assert_eq!(synth_calls.load(Ordering::SeqCst), 1);
        assert_eq!(player_calls.load(Ordering::SeqCst), 0);
    }

    #[test]
    fn a_cancelled_speak_is_not_reported_as_an_empty_reply() {
        let (mut session, _, heard) = session(Vec::new(), None);
        session.cancel();
        let err = session.speak("turn on the lights").unwrap_err();
        assert!(matches!(err, VoiceError::Cancelled));
        assert!(!matches!(err, VoiceError::Config(_)));
        assert!(heard.lock().unwrap().is_empty());
    }

    #[test]
    #[ignore = "needs a TTS helper (espeak-ng, or piper with a voice) on PATH"]
    fn a_real_helper_synthesizes_a_wav() {
        let config = VoiceConfig {
            enabled: true,
            ..Default::default()
        };
        let synth = HelperSynth::new(&config);
        let wav = synth
            .synthesize("turn on the lights")
            .expect("a TTS helper must be installed for this test");
        assert_eq!(&wav[..4], b"RIFF");
        assert!(wav.len() > 44);
    }
}
