use crate::cancel::CancelToken;
use crate::capture::DeviceSource;
use crate::helpers::{
    Scratch, first_available, missing_tool_message, pcm_from_bytes, run, run_cancellable, which,
    write_wav,
};
use crate::player::DevicePlayer;
use crate::vad::{is_silent, trim_silence};
use crate::{AsrEngine, Result, StageStatus, VoiceConfig, VoiceError, VoiceStatus, probe};
use std::fs;
use std::path::PathBuf;

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

pub struct HelperSynth {
    helper: String,
    fallback: String,
    voice: Option<PathBuf>,
}

impl HelperSynth {
    pub fn new(config: &VoiceConfig) -> Self {
        Self {
            helper: config.tts_helper.clone(),
            fallback: config.tts_fallback.clone(),
            voice: config.tts_voice.clone(),
        }
    }
}

impl Synthesizer for HelperSynth {
    fn synthesize(&self, text: &str) -> Result<Vec<u8>> {
        let scratch = Scratch::new()?;
        let output = scratch.path().join("reply.wav");
        let output_name = output.to_string_lossy();
        if let Some(voice) = self.voice.as_ref()
            && which(&self.helper).is_some()
        {
            let voice_name = voice.to_string_lossy();
            let args = ["--model", &voice_name, "--output_file", &output_name];
            run(&self.helper, &args, Some(text.as_bytes()), 60)?;
        } else if which(&self.fallback).is_some() {
            let args = ["-w", &output_name, "--", text];
            run(&self.fallback, &args, None, 60)?;
        } else {
            return Err(VoiceError::MissingHelper(missing_tool_message(
                &self.helper,
            )));
        }
        fs::read(&output).map_err(|err| VoiceError::Helper(format!("read speech: {err}")))
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
        if is_silent(&pcm, self.config.rms_threshold) {
            self.phase = VoicePhase::Idle;
            return Ok(None);
        }
        let (start, end) = trim_silence(
            &pcm,
            self.config.rms_threshold,
            self.config.hangover_ms,
            self.config.sample_rate,
        );
        if start == end {
            self.phase = VoicePhase::Idle;
            return Ok(None);
        }
        self.phase = VoicePhase::Transcribing;
        let turn = self
            .recognizer
            .transcribe_cancellable(&pcm[start..end], &self.cancel);
        self.phase = VoicePhase::Idle;
        turn
    }

    pub fn speak(&mut self, text: &str) -> Result<()> {
        self.cancel.check()?;
        let text = text.trim();
        if text.is_empty() {
            return Err(VoiceError::Config(
                "refusing to speak an empty reply".into(),
            ));
        }
        self.phase = VoicePhase::Speaking;
        let turn = self.speak_turn(text);
        self.phase = VoicePhase::Idle;
        turn
    }

    fn speak_turn(&mut self, text: &str) -> Result<()> {
        let wav = self.synth.synthesize(text)?;
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

    fn speech() -> Vec<i16> {
        (0..3_200)
            .map(|i| ((i as f64 * 0.08).sin() * 16_000.0) as i16)
            .collect()
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
}
