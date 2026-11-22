//! Built-in ASR: whisper.cpp linked through whisper-rs.
//!
//! Model resolution follows the repo's durable-home convention (`$VIKRAM_HOME`, else `~/.vikram`):
//! an explicit `asr_model` wins if it exists, otherwise the `base` (default) and `tiny` (speed)
//! tiers are looked up under `<home>/models`. A missing model is a real error naming the exact
//! expected path and its download URL — never a silent pass. The whisper-cli helper stays as a
//! last-resort fallback tier, and a whisper failure that reaches it is recorded as a degrade
//! note for `VoiceStatus`.

use crate::cancel::CancelToken;
use crate::helpers::which;
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
    match resolve_model(config) {
        Ok(model) => {
            let fallback = which(&config.asr_helper)
                .map(|_| format!("; runtime fallback: {}", config.asr_helper))
                .unwrap_or_default();
            StageStatus::Ready(format!(
                "built-in whisper.cpp ({}){fallback}",
                model.display()
            ))
        }
        Err(err) => StageStatus::Missing(err.to_string()),
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
        let abort = cancel.clone();
        let mut params = FullParams::new(SamplingStrategy::Greedy { best_of: 1 });
        params.set_language(language.as_deref());
        params.set_print_progress(false);
        params.set_print_timestamps(false);
        params.set_suppress_nst(true);
        params.set_abort_callback_safe(move || abort.is_cancelled());
        let mut audio = vec![0f32; pcm.len()];
        whisper_rs::convert_integer_to_float_audio(pcm, &mut audio).map_err(whisper_error)?;
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

/// The ASR chain: built-in whisper first, whisper-cli helper only as a runtime fallback.
/// A whisper failure that reaches the helper is recorded as a degrade note, and a missing
/// model is reported as the model's own error — whisper-cli needs the same model file, so
/// a missing model is never rescued by the helper.
pub struct AsrEngine {
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
        if let Some(whisper) = self.whisper.as_mut() {
            match whisper.transcribe_cancellable(pcm, cancel) {
                Ok(text) => return Ok(text),
                Err(VoiceError::Cancelled) => return Err(VoiceError::Cancelled),
                Err(err) => self.notes.push(format!(
                    "built-in whisper failed: {err}; falling back to {helper_name}",
                    helper_name = self.helper_name
                )),
            }
        } else if let Some(err) = self.model_error.clone() {
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
        let StageStatus::Missing(reason) = missing else {
            panic!("expected missing, got {missing:?}")
        };
        assert!(reason.contains(BASE_TIER.1));

        let ready = probe_asr(&config_with_dir(ready_dir.path()));
        let StageStatus::Ready(detail) = ready else {
            panic!("expected ready, got {ready:?}")
        };
        assert!(detail.contains("built-in whisper.cpp"), "{detail}");
    }

    #[test]
    fn a_chain_without_a_model_fails_with_the_model_error_not_the_helper() {
        let dir = tempfile::tempdir().unwrap();
        let mut engine = AsrEngine::new(&config_with_dir(dir.path()));
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
}
