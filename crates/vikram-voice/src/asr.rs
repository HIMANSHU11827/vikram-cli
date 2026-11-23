//! Built-in ASR: wavo, a pure-Rust Whisper/Parakeet/Moonshine engine on wgpu.
//!
//! Models are GGUF files from wavo's hub (`handy-computer/*-gguf`). The repo's durable-home
//! convention (`$VIKRAM_HOME`, else `~/.vikram`) holds them under `<home>/models`, so a fresh
//! machine downloads the default model on the first utterance, not at `status` time. An explicit
//! `asr_model` may be a downloaded `.gguf` path or any wavo hub name. A missing model is a real
//! error naming what to fetch — never a silent pass. The whisper-cli helper stays as a last-resort
//! fallback tier, and a wavo failure that reaches it is recorded as a degrade note for
//! `VoiceStatus`.

use crate::cancel::CancelToken;
use crate::helpers::{Scratch, run_cancellable, which, write_wav};
use crate::session::Recognizer;
use crate::{Result, StageStatus, VoiceConfig, VoiceError};
use std::ffi::OsStr;
use std::path::{Path, PathBuf};
use std::sync::mpsc;
use wavo::hub::Hub;

/// The hub name of the default tier: 80 MB, English, 7× faster than whisper-turbo.
pub const DEFAULT_MODEL: &str = "whisper-base.en";

/// A caption needs at least 100 ms of 16 kHz audio; below that the answer is an empty string,
/// so a caller can caption from the first frame without an error.
const MIN_PARTIAL_SAMPLES: usize = 1_600;

/// `$VIKRAM_HOME/models`, else `~/.vikram/models`: the same root
/// `vikram_runtime::config::vikram_home` uses, and the variable *is* that root. Mirrored here
/// because `docs/voice-plan.md` §3 keeps this crate off the runtime's dependency graph; the
/// mirror must track any change to the runtime's convention.
pub fn models_dir() -> PathBuf {
    models_dir_in(
        std::env::var_os("VIKRAM_HOME").as_deref(),
        std::env::var_os("HOME")
            .or_else(|| std::env::var_os("USERPROFILE"))
            .as_deref(),
    )
}

fn models_dir_in(home: Option<&OsStr>, user_home: Option<&OsStr>) -> PathBuf {
    match home.filter(|home| !home.is_empty()) {
        Some(home) => PathBuf::from(home).join("models"),
        None => user_home
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from("."))
            .join(".vikram")
            .join("models"),
    }
}

/// Where a model comes from: a file on disk, or a hub name the engine may download once.
#[derive(Clone, Debug, PartialEq)]
pub(crate) enum ModelSource {
    File(PathBuf),
    Hub(String),
}

/// A hub name has no path separator and no extension; anything else is a path.
fn is_hub_name(arg: &str) -> bool {
    !arg.contains(['/', '\\']) && !arg.ends_with(".gguf")
}

fn source_of(config: &VoiceConfig) -> ModelSource {
    let arg = config.asr_model.to_string_lossy();
    if arg.trim().is_empty() {
        ModelSource::Hub(DEFAULT_MODEL.to_owned())
    } else if is_hub_name(&arg) {
        ModelSource::Hub(arg.into_owned())
    } else {
        ModelSource::File(config.asr_model.clone())
    }
}

fn hub(dir: &Path) -> Hub {
    Hub::at(dir)
}

/// Resolution: an explicit source wins (a file must exist; a hub name must be known);
/// otherwise the default tier is looked up offline under `dir`. A model that is known but not
/// downloaded yet is an honest error here — `probe` says it downloads on the first utterance.
pub fn resolve_model_in(dir: &Path, config: &VoiceConfig) -> Result<PathBuf> {
    match source_of(config) {
        ModelSource::File(model) => {
            if model.is_file() {
                Ok(model)
            } else {
                Err(VoiceError::ModelMissing(format!(
                    "asr_model {} is not a file; point it at a downloaded .gguf, or clear it to \
                     use {}",
                    model.display(),
                    DEFAULT_MODEL
                )))
            }
        }
        ModelSource::Hub(name) => {
            wavo::hub::lookup(&name).map_err(|err| VoiceError::ModelMissing(err.to_string()))?;
            match hub(dir).find(&name) {
                Some(path) if path.is_file() => Ok(path),
                _ => Err(VoiceError::ModelMissing(format!(
                    "{name} is not downloaded under {}; it downloads on the first utterance, or \
                     run `wavo pull {name}`",
                    dir.display()
                ))),
            }
        }
    }
}

pub fn resolve_model(config: &VoiceConfig) -> Result<PathBuf> {
    let dir = config.models_dir.clone().unwrap_or_else(models_dir);
    resolve_model_in(&dir, config)
}

pub fn probe_asr(config: &VoiceConfig) -> StageStatus {
    let fast_path = config.voxtype_fast_path && which(VOXTYPE).is_some();
    let detail = match resolve_model(config) {
        Ok(model) => format!("built-in wavo ({})", model.display()),
        Err(err) => {
            if fast_path {
                return StageStatus::Degraded(format!(
                    "voxtype fast path only; built-in wavo model missing: {err}"
                ));
            }
            return StageStatus::Missing(err.to_string());
        }
    };
    StageStatus::Ready(if fast_path {
        format!("voxtype fast path; {detail}")
    } else {
        detail
    })
}

fn wavo_error(err: wavo::Error) -> VoiceError {
    VoiceError::Helper(format!("wavo: {err}"))
}

/// The linked-in recognizer. Model load happens lazily on the first utterance so `status`
/// never costs a model read; a missing hub model is downloaded once, with a printed note.
pub struct WavoRecognizer {
    source: ModelSource,
    dir: PathBuf,
    language: Option<String>,
    model: Option<wavo::Model>,
    /// Where the model actually runs, once it has loaded: adapter name and graphics API.
    backend: Option<String>,
}

impl WavoRecognizer {
    pub(crate) fn new(source: ModelSource, dir: PathBuf, language: Option<String>) -> Self {
        Self {
            source,
            dir,
            language,
            model: None,
            backend: None,
        }
    }

    /// The adapter the model is running on, or `None` before the first utterance.
    pub fn backend(&self) -> Option<&str> {
        self.backend.as_deref()
    }

    /// Build from a resolved config: the same source rules the rest of the module uses.
    pub fn from_config(config: &VoiceConfig) -> Self {
        Self::new(
            source_of(config),
            config.models_dir.clone().unwrap_or_else(models_dir),
            config.asr_language.clone(),
        )
    }

    fn download(&self, name: &str) -> Result<PathBuf> {
        let (tx, rx) = mpsc::channel();
        let dir = self.dir.clone();
        let owned = name.to_owned();
        eprintln!("voice: downloading wavo model {name} (one time)");
        std::thread::spawn(move || {
            let result = hub(&dir).pull(&owned, |_| std::ops::ControlFlow::Continue(()));
            let _ = tx.send(result.map_err(|err| err.to_string()));
        });
        let outcome = rx
            .recv()
            .unwrap_or_else(|_| Err("download thread died".into()));
        match outcome {
            Ok(path) => {
                eprintln!("voice: {name} ready at {}", path.display());
                Ok(path)
            }
            Err(err) => Err(VoiceError::ModelMissing(format!(
                "could not download {name}: {err}"
            ))),
        }
    }

    fn ensure_loaded(&mut self) -> Result<&wavo::Model> {
        if self.model.is_none() {
            let path = match &self.source {
                ModelSource::File(model) => {
                    if !model.is_file() {
                        return Err(VoiceError::ModelMissing(format!(
                            "model file {} disappeared since startup",
                            model.display()
                        )));
                    }
                    model.clone()
                }
                ModelSource::Hub(name) => match hub(&self.dir).find(name) {
                    Some(path) if path.is_file() => path,
                    _ => self.download(name)?,
                },
            };
            // Vulkan explicitly, as the operator asked: an integrated Intel part still runs these
            // kernels several times faster than the CPU. A driver that cannot give a GPU adapter
            // falls back to wavo's own choice and says so, so the status line never lies about it.
            let model = match wavo::Model::load_with(&path, wavo::Backend::Gpu) {
                Ok(model) => model,
                Err(err) => {
                    eprintln!("voice: no vulkan adapter ({err}); transcribing on the cpu");
                    wavo::Model::load(&path).map_err(wavo_error)?
                }
            };
            let device = model.device();
            let label = format!("{} ({:?})", device.name, device.api);
            eprintln!("voice: asr on {label}");
            self.backend = Some(label);
            self.model = Some(model);
        }
        Ok(self.model.as_ref().expect("model loaded"))
    }

    /// The one place the configured language becomes wavo options: `None` leaves the model to
    /// detect the spoken language itself.
    fn options(&self) -> wavo::Options {
        let mut options = wavo::Options::default();
        options.language = self.language.clone();
        options
    }

    fn transcribe_impl(&mut self, pcm: &[i16], cancel: &CancelToken) -> Result<Option<String>> {
        cancel.check()?;
        let options = self.options();
        let model = self.ensure_loaded()?;
        let audio: Vec<f32> = pcm.iter().map(|sample| *sample as f32 / 32768.0).collect();
        let transcript = model
            .transcribe_with(&audio, &options)
            .map_err(wavo_error)?;
        // wavo has no abort hook; cancellation lands before inference and on its completion.
        if cancel.is_cancelled() {
            return Err(VoiceError::Cancelled);
        }
        let text = transcript.text.trim();
        Ok((!text.is_empty()).then(|| text.to_owned()))
    }

    /// Load the model only if it is already on disk: a caption never starts a download.
    fn ensure_resolved(&mut self) -> Result<&wavo::Model> {
        if self.model.is_none() {
            let path = match &self.source {
                ModelSource::File(model) => {
                    if !model.is_file() {
                        return Err(VoiceError::ModelMissing(format!(
                            "model file {} disappeared since startup",
                            model.display()
                        )));
                    }
                    model.clone()
                }
                ModelSource::Hub(name) => match hub(&self.dir).find(name) {
                    Some(path) if path.is_file() => path,
                    _ => {
                        return Err(VoiceError::ModelMissing(format!(
                            "{name} is not downloaded under {}; live captions never download \
                             mid-stream — run `wavo pull {name}`",
                            self.dir.display()
                        )));
                    }
                },
            };
            self.model = Some(wavo::Model::load(&path).map_err(wavo_error)?);
        }
        Ok(self.model.as_ref().expect("model loaded"))
    }

    /// The transcript of the buffer so far, for captioning while the operator is still speaking.
    /// A buffer under 100 ms is an empty caption, not an error, and the model must already be on
    /// disk: a caption must never stall on a download or a model that is still missing.
    pub fn transcribe_partial(&mut self, pcm: &[i16]) -> Result<String> {
        if pcm.len() < MIN_PARTIAL_SAMPLES {
            return Ok(String::new());
        }
        let options = self.options();
        let model = self.ensure_resolved()?;
        let audio: Vec<f32> = pcm.iter().map(|sample| *sample as f32 / 32768.0).collect();
        let transcript = model
            .transcribe_with(&audio, &options)
            .map_err(wavo_error)?;
        Ok(transcript.text.trim().to_owned())
    }
}

impl Recognizer for WavoRecognizer {
    fn transcribe(&mut self, pcm: &[i16]) -> Result<Option<String>> {
        self.transcribe_impl(pcm, &crate::CancelToken::new())
    }

    fn transcribe_cancellable(
        &mut self,
        pcm: &[i16],
        cancel: &crate::CancelToken,
    ) -> Result<Option<String>> {
        self.transcribe_impl(pcm, cancel)
    }
}

/// The voxtype CLI's name, the one external binary this crate may call — only when it is
/// already on PATH, never as a requirement.
pub const VOXTYPE: &str = "voxtype";

/// The transcript is the last paragraph of stdout that is not progress output: `voxtype -q
/// transcribe` prints VAD and whisper.cpp diagnostics around the text, and the order is not
/// fixed — a run that ended with `VAD: 2.56s speech (79.5% of audio)` used to return that line
/// as the transcript. So paragraphs are scanned from the end and the first one that does not
/// look like a log line wins.
fn last_paragraph(stdout: &[u8]) -> Option<String> {
    let text = String::from_utf8_lossy(stdout);
    let paragraphs: Vec<&str> = text.split("\n\n").map(str::trim).collect();
    paragraphs
        .into_iter()
        .rev()
        .find(|paragraph| !paragraph.is_empty() && !is_progress_output(paragraph))
        .map(str::to_owned)
}

/// Progress lines voxtype prints around the transcript: VAD summaries, whisper.cpp diagnostics
/// (`whisper_model_load:`, `whisper_backend_init_gpu:`, …), and bracketed status lines.
fn is_progress_output(paragraph: &str) -> bool {
    // A transcript paragraph never carries engine chatter. `any` rather than `all` because
    // voxtype prints its diagnostics in one block: "Loading audio file: …" followed by
    // "Audio format: …" and "Processing N samples" is a paragraph whose first line is a log
    // line and whose rest are too, and the old `all` test let it through as a transcript.
    paragraph.lines().any(|line| {
        let line = line.trim_start();
        line.starts_with("VAD")
            || line.starts_with("Loading audio file")
            || line.starts_with("Audio format:")
            || line.starts_with("Processing ")
            || line.starts_with("Detected language")
            || line.starts_with("Transcrib")
            || line.starts_with("whisper_")
            || line.starts_with("ggml_")
            || line.starts_with("main:")
            || line.starts_with("system_info")
            || line.starts_with('[')
    })
}

/// The optional fast path: voxtype's own engine stack (GPU-accelerated where available) over
/// the same 16 kHz mono WAV. Present only when the binary is detected; absence never affects
/// the built-in path.
pub struct VoxtypeRecognizer {
    timeout_secs: u64,
    sample_rate: u16,
}

impl VoxtypeRecognizer {
    /// `None` when the operator disabled the fast path or the binary is not on PATH.
    pub fn detect(config: &VoiceConfig) -> Option<Self> {
        if !config.voxtype_fast_path || which(VOXTYPE).is_none() {
            return None;
        }
        Some(Self {
            timeout_secs: config.asr_timeout_secs,
            sample_rate: config.sample_rate,
        })
    }
}

impl Recognizer for VoxtypeRecognizer {
    fn transcribe(&mut self, pcm: &[i16]) -> Result<Option<String>> {
        self.transcribe_cancellable(pcm, &CancelToken::new())
    }

    fn transcribe_cancellable(
        &mut self,
        pcm: &[i16],
        cancel: &CancelToken,
    ) -> Result<Option<String>> {
        cancel.check()?;
        let scratch = Scratch::new()?;
        let wav = scratch.path().join("utterance.wav");
        write_wav(&wav, pcm, self.sample_rate)?;
        let wav = wav.to_string_lossy().into_owned();
        let stdout = run_cancellable(
            VOXTYPE,
            &["-q", "transcribe", &wav],
            None,
            self.timeout_secs,
            cancel,
        )?;
        Ok(last_paragraph(&stdout))
    }
}

/// The ASR chain: the detected voxtype fast path first, built-in wavo second, the
/// whisper-cli helper only as a runtime fallback. Every handoff is recorded as a degrade
/// note for `VoiceStatus`, and a missing model is reported as the model's own error —
/// whisper-cli needs the same model file, so a missing model is never rescued by the helper.
pub struct AsrEngine {
    voxtype: Option<VoxtypeRecognizer>,
    wavo: Option<WavoRecognizer>,
    helper: crate::HelperRecognizer,
    helper_name: String,
    model_error: Option<String>,
    notes: Vec<String>,
}

impl AsrEngine {
    pub fn new(config: &VoiceConfig) -> Self {
        let (wavo, model_error) = match resolve_model(config) {
            Ok(_) => (Some(WavoRecognizer::from_config(config)), None),
            Err(err) => {
                // A hub model that is known but not downloaded is armed: the first
                // utterance downloads it. Only an unknown or missing file is an error.
                match source_of(config) {
                    ModelSource::Hub(name) if wavo::hub::lookup(&name).is_ok() => {
                        (Some(WavoRecognizer::from_config(config)), None)
                    }
                    _ => (None, Some(err.to_string())),
                }
            }
        };
        Self {
            voxtype: VoxtypeRecognizer::detect(config),
            wavo,
            helper: crate::HelperRecognizer::new(config),
            helper_name: config.asr_helper.clone(),
            model_error,
            notes: Vec::new(),
        }
    }

    /// Load the caption model now, so the first caption does not pay for a GPU kernel compile
    /// while the operator is already talking. A missing model is not an error here: the mode can
    /// run without captions, and the utterance path reports the real problem when it is used.
    pub fn warm(&mut self) -> Result<()> {
        match self.wavo.as_mut() {
            Some(wavo) => wavo.ensure_resolved().map(|_| ()),
            None => Ok(()),
        }
    }

    /// A caption for the audio so far: the built-in wavo engine only, never the voxtype CLI
    /// (it transcribes whole utterances) and never a mid-stream model download.
    pub fn transcribe_partial(&mut self, pcm: &[i16]) -> Result<String> {
        if pcm.len() < MIN_PARTIAL_SAMPLES {
            return Ok(String::new());
        }
        match self.wavo.as_mut() {
            Some(wavo) => wavo.transcribe_partial(pcm),
            None => match self.model_error.clone() {
                Some(err) => Err(VoiceError::ModelMissing(err)),
                None => Ok(String::new()),
            },
        }
    }
}

impl Recognizer for AsrEngine {
    fn transcribe(&mut self, pcm: &[i16]) -> Result<Option<String>> {
        self.transcribe_cancellable(pcm, &CancelToken::new())
    }

    fn transcribe_cancellable(
        &mut self,
        pcm: &[i16],
        cancel: &CancelToken,
    ) -> Result<Option<String>> {
        cancel.check()?;
        if let Some(voxtype) = self.voxtype.as_mut() {
            match voxtype.transcribe_cancellable(pcm, cancel) {
                Ok(text) => return Ok(text),
                Err(VoiceError::Cancelled) => return Err(VoiceError::Cancelled),
                Err(err) => self.notes.push(format!(
                    "voxtype fast path failed: {err}; trying built-in wavo"
                )),
            }
        }
        if let Some(wavo) = self.wavo.as_mut() {
            match wavo.transcribe_cancellable(pcm, cancel) {
                Ok(text) => return Ok(text),
                Err(VoiceError::Cancelled) => return Err(VoiceError::Cancelled),
                Err(err) => self.notes.push(format!(
                    "built-in wavo failed: {err}; falling back to {helper_name}",
                    helper_name = self.helper_name
                )),
            }
        }
        if let Some(err) = self.model_error.clone() {
            return Err(VoiceError::ModelMissing(err));
        }
        self.helper.transcribe(pcm)
    }

    fn notes(&mut self) -> Vec<String> {
        std::mem::take(&mut self.notes)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_models_dir_follows_the_vikram_home_convention() {
        assert_eq!(
            models_dir_in(
                Some(OsStr::new("/srv/vikram")),
                Some(OsStr::new("/home/op"))
            ),
            PathBuf::from("/srv/vikram/models"),
            "a set VIKRAM_HOME must not be doubled"
        );
        assert_eq!(
            models_dir_in(None, Some(OsStr::new("/home/op"))),
            PathBuf::from("/home/op/.vikram/models")
        );
        assert_eq!(
            models_dir_in(Some(OsStr::new("")), Some(OsStr::new("/home/op"))),
            PathBuf::from("/home/op/.vikram/models")
        );
    }

    #[test]
    fn a_hub_name_is_not_a_path() {
        assert!(is_hub_name("whisper-base.en"));
        assert!(!is_hub_name("weights.gguf"));
        assert!(!is_hub_name("/tmp/weights.gguf"));
        assert!(!is_hub_name("models/weights"));
    }

    fn config_with_dir(dir: &Path) -> VoiceConfig {
        VoiceConfig {
            models_dir: Some(dir.to_path_buf()),
            ..Default::default()
        }
    }

    #[test]
    fn an_explicit_model_that_exists_wins() {
        let dir = tempfile::tempdir().unwrap();
        let model = dir.path().join("my-model.gguf");
        std::fs::write(&model, b"weights").unwrap();
        let config = VoiceConfig {
            asr_model: model.clone(),
            ..config_with_dir(dir.path())
        };
        assert_eq!(resolve_model(&config).unwrap(), model);
    }

    #[test]
    fn an_explicit_model_that_is_missing_names_it() {
        let dir = tempfile::tempdir().unwrap();
        let config = VoiceConfig {
            asr_model: dir.path().join("gone.gguf"),
            ..config_with_dir(dir.path())
        };
        let err = resolve_model(&config).unwrap_err().to_string();
        assert!(err.contains("gone.gguf"), "error names the path: {err}");
    }

    #[test]
    fn an_unknown_hub_name_is_refused() {
        let dir = tempfile::tempdir().unwrap();
        let config = VoiceConfig {
            asr_model: dir.path().join("not-a-model").to_path_buf(),
            ..config_with_dir(dir.path())
        };
        let err = resolve_model(&config).unwrap_err().to_string();
        assert!(err.contains("not-a-model"), "{err}");
    }

    #[test]
    fn a_missing_default_model_says_it_downloads_on_first_utterance() {
        let dir = tempfile::tempdir().unwrap();
        let config = config_with_dir(dir.path());
        let err = resolve_model(&config).unwrap_err().to_string();
        assert!(err.contains(DEFAULT_MODEL), "{err}");
        assert!(
            err.contains(dir.path().display().to_string().as_str()),
            "{err}"
        );
        assert!(
            err.contains("first utterance") || err.contains("wavo pull"),
            "{err}"
        );
    }

    #[test]
    fn probe_reports_wavo_ready_only_when_the_model_is_on_disk() {
        let missing_dir = tempfile::tempdir().unwrap();
        match probe_asr(&config_with_dir(missing_dir.path())) {
            StageStatus::Degraded(reason) => assert!(reason.contains("voxtype fast path only")),
            StageStatus::Missing(reason) => assert!(reason.contains(DEFAULT_MODEL), "{reason}"),
            other => panic!("a missing model must not look ready: {other:?}"),
        }
    }

    #[test]
    fn a_chain_without_a_model_fails_with_the_model_error_not_the_helper() {
        let dir = tempfile::tempdir().unwrap();
        let config = VoiceConfig {
            voxtype_fast_path: false,
            asr_model: dir.path().join("missing.gguf"),
            ..config_with_dir(dir.path())
        };
        let mut engine = AsrEngine::new(&config);
        let err = engine.transcribe(&[0i16; 160]).unwrap_err();
        assert!(matches!(err, VoiceError::ModelMissing(_)));
        assert!(err.to_string().contains("missing.gguf"));
        assert!(
            engine.notes().is_empty(),
            "no degrade note: nothing was tried and silently failed"
        );
    }

    #[test]
    fn a_known_model_that_is_not_downloaded_still_arms_the_engine() {
        let dir = tempfile::tempdir().unwrap();
        let config = config_with_dir(dir.path());
        let engine = AsrEngine::new(&config);
        assert!(
            engine.wavo.is_some(),
            "the first utterance downloads the default model"
        );
        assert!(engine.model_error.is_none());
    }

    #[test]
    fn the_voxtype_fast_path_can_be_switched_off_entirely() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("model.gguf"), b"weights").unwrap();
        let config = VoiceConfig {
            voxtype_fast_path: false,
            asr_model: dir.path().join("model.gguf"),
            ..config_with_dir(dir.path())
        };
        assert!(VoxtypeRecognizer::detect(&config).is_none());
        let engine = AsrEngine::new(&config);
        assert!(
            engine.voxtype.is_none(),
            "disabled fast path must not be armed"
        );
        let _ = engine.wavo.expect("built-in engine must still arm");
    }

    #[test]
    fn probe_names_the_engines_the_machine_actually_has() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("model.gguf"), b"weights").unwrap();
        let config = VoiceConfig {
            asr_model: dir.path().join("model.gguf"),
            ..config_with_dir(dir.path())
        };
        let StageStatus::Ready(detail) = probe_asr(&config) else {
            panic!("model resolves, so ASR must be ready")
        };
        assert!(detail.contains("built-in wavo"), "{detail}");
        if VoxtypeRecognizer::detect(&config).is_some() {
            assert!(detail.contains("voxtype fast path"), "{detail}");
        }
    }

    #[test]
    fn the_configured_language_reaches_the_wavo_options() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("model.gguf"), b"weights").unwrap();
        let config = VoiceConfig {
            asr_model: dir.path().join("model.gguf"),
            asr_language: Some("en".into()),
            ..config_with_dir(dir.path())
        };
        let engine = AsrEngine::new(&config);
        let recognizer = engine.wavo.expect("a model file arms the built-in engine");
        assert_eq!(recognizer.options().language.as_deref(), Some("en"));
    }

    #[test]
    fn no_configured_language_leaves_wavo_auto_detection_on() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("model.gguf"), b"weights").unwrap();
        let config = VoiceConfig {
            asr_model: dir.path().join("model.gguf"),
            asr_language: None,
            ..config_with_dir(dir.path())
        };
        let recognizer = WavoRecognizer::from_config(&config);
        assert!(recognizer.options().language.is_none());
        let engine = AsrEngine::new(&config);
        let recognizer = engine.wavo.expect("a model file arms the built-in engine");
        assert!(recognizer.options().language.is_none());
    }

    #[test]
    fn a_short_buffer_is_an_empty_caption_not_an_error() {
        let dir = tempfile::tempdir().unwrap();
        let config = config_with_dir(dir.path());
        let mut recognizer = WavoRecognizer::from_config(&config);
        assert_eq!(recognizer.transcribe_partial(&[]).unwrap(), "");
        assert_eq!(
            recognizer
                .transcribe_partial(&vec![0i16; MIN_PARTIAL_SAMPLES - 1])
                .unwrap(),
            ""
        );
        let mut engine = AsrEngine::new(&config);
        assert_eq!(engine.transcribe_partial(&[0i16; 10]).unwrap(), "");
    }

    #[test]
    fn an_unresolved_hub_model_is_an_error_that_names_it_and_downloads_nothing() {
        let dir = tempfile::tempdir().unwrap();
        let config = config_with_dir(dir.path());
        let mut recognizer = WavoRecognizer::from_config(&config);
        let err = recognizer
            .transcribe_partial(&vec![0i16; MIN_PARTIAL_SAMPLES])
            .unwrap_err();
        assert!(err.to_string().contains(DEFAULT_MODEL), "{err}");
        assert_eq!(
            std::fs::read_dir(dir.path()).unwrap().count(),
            0,
            "a caption must never download mid-stream"
        );
        let mut engine = AsrEngine::new(&config);
        let err = engine
            .transcribe_partial(&vec![0i16; MIN_PARTIAL_SAMPLES])
            .unwrap_err();
        assert!(err.to_string().contains(DEFAULT_MODEL), "{err}");
    }

    #[test]
    #[ignore = "hardware/model test: needs a downloaded wavo model and VIKRAM_VOICE_TEST_WAV"]
    fn a_partial_transcript_captions_a_resolved_model() {
        let Ok(wav) = std::env::var("VIKRAM_VOICE_TEST_WAV") else {
            return;
        };
        let config = VoiceConfig::default();
        resolve_model(&config).expect("a model must be downloaded");
        let mut reader = hound::WavReader::open(&wav).expect("fixture wav must decode");
        let pcm: Vec<i16> = reader.samples::<i16>().map(|s| s.unwrap()).collect();
        let mut recognizer = WavoRecognizer::from_config(&config);
        let text = recognizer
            .transcribe_partial(&pcm)
            .expect("partial inference must run on a resolved model");
        eprintln!("partial: {text}");
        assert!(!text.is_empty(), "a full utterance must caption something");
    }

    #[test]
    fn voxtype_stdout_parses_to_the_last_paragraph() {
        let observed = "Loading audio file: \"/tmp/x.wav\"\n\
                        Processing 176000 samples (11.00s)...\n\
                        VAD: 8.22s speech (74.7% of audio)\n\n\
                        And so my fellow Americans, ask not what your country can do for you.\n";
        assert_eq!(
            last_paragraph(observed.as_bytes()).as_deref(),
            Some("And so my fellow Americans, ask not what your country can do for you.")
        );
        assert_eq!(last_paragraph(b"\n\n\n"), None);
        assert_eq!(last_paragraph(b""), None);
    }

    #[test]
    fn voxtype_progress_after_the_transcript_is_not_the_transcript() {
        let observed = "Hello Vikram, this is a voice test.\n\n\
                        VAD: 2.10s speech (100.0% of audio)\n";
        assert_eq!(
            last_paragraph(observed.as_bytes()).as_deref(),
            Some("Hello Vikram, this is a voice test.")
        );
        let logs = "whisper_model_load: n_mels = 80\n\n\
                    whisper_backend_init_gpu: using Vulkan0 backend\n\n\
                    VAD backend: Energy\n";
        assert_eq!(last_paragraph(logs.as_bytes()), None);
    }

    #[test]
    #[ignore = "hardware/model test: needs a downloaded wavo model and VIKRAM_VOICE_TEST_WAV"]
    fn transcribes_real_speech_with_a_downloaded_model() {
        let Ok(wav) = std::env::var("VIKRAM_VOICE_TEST_WAV") else {
            return;
        };
        let config = VoiceConfig::default();
        resolve_model(&config).expect("a model must be downloaded");
        let mut reader = hound::WavReader::open(&wav).expect("fixture wav must decode");
        let pcm: Vec<i16> = reader.samples::<i16>().map(|s| s.unwrap()).collect();
        assert!(
            pcm.len() > 16_000,
            "fixture must hold at least one second of audio"
        );
        let started = std::time::Instant::now();
        let mut recognizer = WavoRecognizer::from_config(&config);
        let text = recognizer
            .transcribe(&pcm)
            .expect("wavo must run")
            .expect("real speech must produce a transcript");
        eprintln!("transcript ({:?}): {text}", started.elapsed());
        assert!(text.split_whitespace().count() >= 3);
    }

    #[test]
    #[ignore = "hardware/model test: needs VIKRAM_VOICE_TEST_WAV; exercises the detected voxtype fast path"]
    fn the_voxtype_fast_path_transcribes_real_speech() {
        let Ok(wav) = std::env::var("VIKRAM_VOICE_TEST_WAV") else {
            return;
        };
        let config = VoiceConfig::default();
        let mut engine = AsrEngine::new(&config);
        if engine.voxtype.is_none() {
            return;
        }
        let mut reader = hound::WavReader::open(&wav).expect("fixture wav must decode");
        let pcm: Vec<i16> = reader.samples::<i16>().map(|s| s.unwrap()).collect();
        let started = std::time::Instant::now();
        let text = engine
            .transcribe(&pcm)
            .expect("voxtype must run")
            .expect("real speech must produce a transcript");
        eprintln!("fast path ({:?}): {text}", started.elapsed());
        assert!(text.split_whitespace().count() >= 3);
        assert!(
            engine.notes().is_empty(),
            "a healthy fast path leaves no degrade notes"
        );
    }
}
