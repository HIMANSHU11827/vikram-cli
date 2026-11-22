//! Built-in ASR: whisper.cpp linked through whisper-rs.
//!
//! Model resolution follows the repo's durable-home convention (`$VIKRAM_HOME`, else `~/.vikram`):
//! an explicit `asr_model` wins if it exists, otherwise the `base` (default) and `tiny` (speed)
//! tiers are looked up under `<home>/models`. A missing model is a real error naming the exact
//! expected path and its download URL — never a silent pass. The whisper-cli helper stays as a
//! last-resort fallback tier, and a whisper failure that reaches it is recorded as a degrade
//! note for `VoiceStatus`.

use crate::cancel::CancelToken;
use crate::helpers::{Scratch, run_cancellable, which, write_wav};
use crate::session::Recognizer;
use crate::{Result, StageStatus, VoiceConfig, VoiceError};
use std::path::{Path, PathBuf};
use whisper_rs::{
    FullParams, SamplingStrategy, WhisperContext, WhisperContextParameters, WhisperState,
};

pub const BASE_TIER: (&str, &str) = (
    "ggml-base.bin",
    "https://huggingface.co/ggerganov/whisper.cpp/resolve/main/ggml-base.bin",
);
pub const TINY_TIER: (&str, &str) = (
    "ggml-tiny.bin",
    "https://huggingface.co/ggerganov/whisper.cpp/resolve/main/ggml-tiny.bin",
);

/// `$VIKRAM_HOME`, else `~/.vikram`: the same root `vikram_runtime::config::vikram_home` uses.
/// Mirrored here because `docs/voice-plan.md` §3 keeps this crate off the runtime's dependency
/// graph; the mirror must track any change to the runtime's convention.
pub fn models_dir() -> PathBuf {
    let home = std::env::var_os("VIKRAM_HOME")
        .filter(|home| !home.is_empty())
        .map(PathBuf::from);
    let base = home
        .or_else(|| {
            std::env::var_os("HOME")
                .or_else(|| std::env::var_os("USERPROFILE"))
                .map(PathBuf::from)
        })
        .unwrap_or_else(|| PathBuf::from("."));
    base.join(".vikram").join("models")
}

/// Resolution order: explicit `asr_model` if it is a file; then `base`, then `tiny`, under
/// `dir`. The error names the exact expected path and where to download it.
pub fn resolve_model_in(dir: &Path, config: &VoiceConfig) -> Result<PathBuf> {
    if !config.asr_model.as_os_str().is_empty() {
        let explicit = &config.asr_model;
        if explicit.is_file() {
            return Ok(explicit.clone());
        }
        return Err(VoiceError::ModelMissing(format!(
            "asr_model {} is not a file; point it at a downloaded ggml model, or clear it to \
             use {}",
            explicit.display(),
            dir.join(BASE_TIER.0).display()
        )));
    }
    for (file, _) in [BASE_TIER, TINY_TIER] {
        let candidate = dir.join(file);
        if candidate.is_file() {
            return Ok(candidate);
        }
    }
    Err(VoiceError::ModelMissing(format!(
        "expected {} (default tier) or {} under {}; download one, e.g. curl -L -o '{}' {}",
        BASE_TIER.0,
        TINY_TIER.0,
        dir.display(),
        dir.join(BASE_TIER.0).display(),
        BASE_TIER.1
    )))
}

pub fn resolve_model(config: &VoiceConfig) -> Result<PathBuf> {
    let dir = config.models_dir.clone().unwrap_or_else(models_dir);
    resolve_model_in(&dir, config)
}

pub fn probe_asr(config: &VoiceConfig) -> StageStatus {
    let fast_path = config.voxtype_fast_path && which(VOXTYPE).is_some();
    match resolve_model(config) {
        Ok(model) => StageStatus::Ready(if fast_path {
            format!(
                "voxtype fast path; built-in whisper.cpp ({})",
                model.display()
            )
        } else {
            format!("built-in whisper.cpp ({})", model.display())
        }),
        Err(err) => {
            if fast_path {
                StageStatus::Degraded(format!(
                    "voxtype fast path only; built-in whisper model missing: {err}"
                ))
            } else {
                StageStatus::Missing(err.to_string())
            }
        }
    }
}

fn whisper_error(err: whisper_rs::WhisperError) -> VoiceError {
    VoiceError::Helper(format!("whisper.cpp: {err:?}"))
}

/// The linked-in recognizer. Model load happens lazily on the first utterance so `status`
/// never costs a multi-second model read.
pub struct WhisperRecognizer {
    model: PathBuf,
    language: Option<String>,
    state: Option<WhisperState>,
    context: Option<WhisperContext>,
}

impl WhisperRecognizer {
    pub fn new(model: PathBuf, language: Option<String>) -> Self {
        Self {
            model,
            language,
            state: None,
            context: None,
        }
    }

    fn ensure_loaded(&mut self) -> Result<&mut WhisperState> {
        if self.context.is_none() {
            if !self.model.is_file() {
                return Err(VoiceError::ModelMissing(format!(
                    "model file {} disappeared since startup; re-download from {}",
                    self.model.display(),
                    BASE_TIER.1
                )));
            }
            whisper_rs::install_logging_hooks();
            let context =
                WhisperContext::new_with_params(&self.model, WhisperContextParameters::default())
                    .map_err(whisper_error)?;
            self.context = Some(context);
        }
        if self.state.is_none() {
            let state = self
                .context
                .as_ref()
                .expect("context exists")
                .create_state()
                .map_err(whisper_error)?;
            self.state = Some(state);
        }
        Ok(self.state.as_mut().expect("state exists"))
    }

    fn transcribe_impl(&mut self, pcm: &[i16], cancel: &CancelToken) -> Result<Option<String>> {
        cancel.check()?;
        let language = self.language.clone();
        let state = self.ensure_loaded()?;
        let mut params = FullParams::new(SamplingStrategy::Greedy { best_of: 1 });
        params.set_language(language.as_deref());
        params.set_print_progress(false);
        params.set_print_timestamps(false);
        params.set_suppress_nst(true);
        let mut audio = vec![0f32; pcm.len()];
        whisper_rs::convert_integer_to_float_audio(pcm, &mut audio).map_err(whisper_error)?;
        // Mid-inference abort exists upstream (`set_abort_callback_safe`), but whisper-rs
        // 0.16.0 miswires its trampoline — the double-boxed closure is cast back to the wrong
        // type, so the callback reads garbage and aborts every encode. Until that is fixed
        // upstream, cancellation lands here: before inference and on its completion.
        state.full(params, &audio).map_err(whisper_error)?;
        if cancel.is_cancelled() {
            return Err(VoiceError::Cancelled);
        }
        let mut text = String::new();
        for index in 0..state.full_n_segments() {
            if let Some(segment) = state.get_segment(index)
                && let Ok(line) = segment.to_str_lossy()
            {
                text.push_str(&line);
            }
        }
        let text = text.trim();
        Ok((!text.is_empty()).then(|| text.to_owned()))
    }
}

impl Recognizer for WhisperRecognizer {
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

/// The transcript is the stdout paragraph after the last blank line: `voxtype -q transcribe`
/// prints its progress first and the text last, which is the observed CLI contract this
/// parses against.
fn last_paragraph(stdout: &[u8]) -> Option<String> {
    let text = String::from_utf8_lossy(stdout);
    let text = text.trim();
    let tail = text
        .rsplit_once("\n\n")
        .map(|(_, tail)| tail)
        .unwrap_or(text);
    let tail = tail.trim();
    (!tail.is_empty()).then(|| tail.to_owned())
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

/// The ASR chain: the detected voxtype fast path first, built-in whisper second, the
/// whisper-cli helper only as a runtime fallback. Every handoff is recorded as a degrade
/// note for `VoiceStatus`, and a missing model is reported as the model's own error —
/// whisper-cli needs the same model file, so a missing model is never rescued by the helper.
pub struct AsrEngine {
    voxtype: Option<VoxtypeRecognizer>,
    whisper: Option<WhisperRecognizer>,
    helper: crate::HelperRecognizer,
    helper_name: String,
    model_error: Option<String>,
    notes: Vec<String>,
}

impl AsrEngine {
    pub fn new(config: &VoiceConfig) -> Self {
        let (whisper, model_error) = match resolve_model(config) {
            Ok(model) => (
                Some(WhisperRecognizer::new(model, config.asr_language.clone())),
                None,
            ),
            Err(err) => (None, Some(err.to_string())),
        };
        Self {
            voxtype: VoxtypeRecognizer::detect(config),
            whisper,
            helper: crate::HelperRecognizer::new(config),
            helper_name: config.asr_helper.clone(),
            model_error,
            notes: Vec::new(),
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
                    "voxtype fast path failed: {err}; trying built-in whisper"
                )),
            }
        }
        if let Some(whisper) = self.whisper.as_mut() {
            match whisper.transcribe_cancellable(pcm, cancel) {
                Ok(text) => return Ok(text),
                Err(VoiceError::Cancelled) => return Err(VoiceError::Cancelled),
                Err(err) => self.notes.push(format!(
                    "built-in whisper failed: {err}; falling back to {helper_name}",
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

    fn config_with_dir(dir: &Path) -> VoiceConfig {
        VoiceConfig {
            models_dir: Some(dir.to_path_buf()),
            ..Default::default()
        }
    }

    #[test]
    fn an_explicit_model_that_exists_wins() {
        let dir = tempfile::tempdir().unwrap();
        let model = dir.path().join("my-model.bin");
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
            asr_model: dir.path().join("gone.bin"),
            ..config_with_dir(dir.path())
        };
        let err = resolve_model(&config).unwrap_err().to_string();
        assert!(err.contains("gone.bin"), "error names the path: {err}");
    }

    #[test]
    fn base_is_the_default_tier_and_tiny_is_second() {
        let dir = tempfile::tempdir().unwrap();
        let base = dir.path().join("ggml-base.bin");
        std::fs::write(&base, b"weights").unwrap();
        let config = config_with_dir(dir.path());
        assert_eq!(resolve_model(&config).unwrap(), base);

        let tiny_dir = tempfile::tempdir().unwrap();
        let tiny = tiny_dir.path().join("ggml-tiny.bin");
        std::fs::write(&tiny, b"weights").unwrap();
        assert_eq!(
            resolve_model(&config_with_dir(tiny_dir.path())).unwrap(),
            tiny
        );
    }

    #[test]
    fn a_missing_model_names_the_exact_path_and_download_url() {
        let dir = tempfile::tempdir().unwrap();
        let config = config_with_dir(dir.path());
        let err = resolve_model(&config).unwrap_err().to_string();
        assert!(err.contains("ggml-base.bin"), "{err}");
        assert!(
            err.contains(dir.path().display().to_string().as_str()),
            "{err}"
        );
        assert!(err.contains(BASE_TIER.1), "download URL: {err}");
        assert!(err.contains("curl"), "{err}");
    }

    #[test]
    fn probe_reports_whisper_ready_only_when_a_model_resolves() {
        let dir = tempfile::tempdir().unwrap();
        let ready_dir = tempfile::tempdir().unwrap();
        std::fs::write(ready_dir.path().join("ggml-base.bin"), b"weights").unwrap();

        let missing = probe_asr(&config_with_dir(dir.path()));
        match missing {
            StageStatus::Degraded(reason) => {
                assert!(reason.contains("voxtype fast path only"));
                assert!(reason.contains(BASE_TIER.1));
            }
            StageStatus::Missing(reason) => {
                assert!(reason.contains(BASE_TIER.1), "{reason}");
            }
            other => panic!("a missing model must not look ready: {other:?}"),
        }

        let ready = probe_asr(&config_with_dir(ready_dir.path()));
        let StageStatus::Ready(detail) = ready else {
            panic!("expected ready, got {ready:?}")
        };
        assert!(detail.contains("built-in whisper.cpp"), "{detail}");
    }

    #[test]
    fn a_chain_without_a_model_fails_with_the_model_error_not_the_helper() {
        let dir = tempfile::tempdir().unwrap();
        let config = VoiceConfig {
            voxtype_fast_path: false,
            ..config_with_dir(dir.path())
        };
        let mut engine = AsrEngine::new(&config);
        let err = engine.transcribe(&[0i16; 160]).unwrap_err();
        assert!(matches!(err, VoiceError::ModelMissing(_)));
        assert!(err.to_string().contains(BASE_TIER.1));
        assert!(
            engine.notes().is_empty(),
            "no degrade note: nothing was tried and silently failed"
        );
    }

    #[test]
    fn a_chain_with_a_model_arms_the_built_in_engine() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("ggml-tiny.bin"), b"weights").unwrap();
        let engine = AsrEngine::new(&config_with_dir(dir.path()));
        assert!(engine.whisper.is_some());
        assert!(engine.model_error.is_none());
    }

    #[test]
    fn the_voxtype_fast_path_can_be_switched_off_entirely() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("ggml-base.bin"), b"weights").unwrap();
        let config = VoiceConfig {
            voxtype_fast_path: false,
            ..config_with_dir(dir.path())
        };
        assert!(VoxtypeRecognizer::detect(&config).is_none());
        let engine = AsrEngine::new(&config);
        assert!(
            engine.voxtype.is_none(),
            "disabled fast path must not be armed"
        );
        let _ = engine.whisper.expect("built-in engine must still arm");
    }

    #[test]
    fn probe_names_the_engines_the_machine_actually_has() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("ggml-base.bin"), b"weights").unwrap();
        let config = config_with_dir(dir.path());
        let StageStatus::Ready(detail) = probe_asr(&config) else {
            panic!("model resolves, so ASR must be ready")
        };
        if VoxtypeRecognizer::detect(&config).is_some() {
            assert!(detail.contains("voxtype fast path"), "{detail}");
        } else {
            assert!(detail.contains("built-in whisper.cpp"), "{detail}");
        }

        let bare_dir = tempfile::tempdir().unwrap();
        let bare = config_with_dir(bare_dir.path());
        match probe_asr(&bare) {
            StageStatus::Degraded(reason) if VoxtypeRecognizer::detect(&bare).is_some() => {
                assert!(reason.contains("voxtype fast path only"), "{reason}");
                assert!(reason.contains(BASE_TIER.1), "{reason}");
            }
            StageStatus::Missing(reason) => {
                assert!(reason.contains(BASE_TIER.1), "{reason}");
            }
            other => panic!("a machine with no model and no voxtype must miss honestly: {other:?}"),
        }
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
    #[ignore = "hardware/model test: needs a model under ~/.vikram/models and VIKRAM_VOICE_TEST_WAV"]
    fn transcribes_real_speech_with_a_downloaded_model() {
        let Ok(wav) = std::env::var("VIKRAM_VOICE_TEST_WAV") else {
            return;
        };
        let config = VoiceConfig::default();
        let model = resolve_model(&config).expect("a model must resolve");
        let mut reader = hound::WavReader::open(&wav).expect("fixture wav must decode");
        let pcm: Vec<i16> = reader.samples::<i16>().map(|s| s.unwrap()).collect();
        assert!(
            pcm.len() > 16_000,
            "fixture must hold at least one second of audio"
        );
        let started = std::time::Instant::now();
        let mut recognizer = WhisperRecognizer::new(model, None);
        let text = recognizer
            .transcribe(&pcm)
            .expect("whisper must run")
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
