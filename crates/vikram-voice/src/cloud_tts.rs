//! The opt-in cloud synthesizer: ElevenLabs text-to-speech over HTTPS.
//!
//! The local voice is the default and needs no key; this module exists for an operator who wants
//! a different one. It is built so a cloud problem can never cost a turn: every failure is a
//! typed error, and [`crate::session::VoiceSynth`] speaks the reply with the local voice instead,
//! saying so once.
//!
//! Request shape, read off the published reference
//! (<https://elevenlabs.io/docs/api-reference/text-to-speech/convert>, 2026-11-23):
//! `POST /v1/text-to-speech/{voice_id}?output_format=pcm_16000`, headers `xi-api-key` and
//! `Content-Type: application/json`, body `{"text": …, "model_id": …}`. `pcm_16000` is raw
//! 16 kHz mono signed-16 little-endian PCM — the one shape the WAV writer already wraps, so the
//! player stage needs no change and no decoder.
//!
//! The key is read from the environment variable [`VoiceConfig::tts_api_key_env`] names. It is
//! held in memory for the life of the synthesizer, is never written to `voice.json`, never
//! printed, and never put in an error message.

use std::io::Read;
use std::sync::Mutex;
use std::time::Duration;

use crate::helpers::{pcm_from_bytes, wav_bytes};
use crate::session::Synthesizer;
use crate::{CancelToken, Result, StageStatus, VoiceConfig, VoiceError};

/// ElevenLabs' default host. Regional hosts exist (`api.eu.residency.elevenlabs.io` and friends);
/// a key issued anywhere works against the default one.
pub const ENDPOINT: &str = "https://api.elevenlabs.io/v1/text-to-speech";

/// Raw 16 kHz mono s16le: no container to decode, and 16 kHz is the rate the rest of the stack
/// already speaks.
pub const OUTPUT_FORMAT: &str = "pcm_16000";

/// The rate [`OUTPUT_FORMAT`] names, for the WAV header.
pub const SAMPLE_RATE: u16 = 16_000;

/// One spoken sentence is a few hundred KB of PCM; a minute is the point where the operator
/// would rather hear the local voice than wait, and `timeout_global` bounds the whole exchange.
const TIMEOUT_SECS: u64 = 60;

/// Everything that goes on the wire, as one value, so a test can assert the request shape
/// without opening a socket.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CloudRequest {
    pub url: String,
    /// The key itself, for the length of one call. Never printed, never logged.
    pub api_key: String,
    pub body: String,
}

/// The one thing the synthesizer needs from the network, behind a trait so tests inject a fake
/// and the suite stays offline.
pub trait CloudTransport: Send + Sync {
    /// POST the request and return the raw response body. `Cancelled` when the token fired.
    fn post(&self, request: &CloudRequest, cancel: &CancelToken) -> Result<Vec<u8>>;
}

/// The real transport: `ureq`, which is blocking by design and already in the build graph
/// (wavo's `hub` feature). `reqwest`'s blocking client panics when it is built inside an async
/// runtime, and `voice chat` speaks from one, so `reqwest` would need a wrapper thread per call.
pub struct HttpTransport {
    agent: ureq::Agent,
}

impl HttpTransport {
    pub fn new() -> Self {
        let config = ureq::Agent::config_builder()
            .timeout_global(Some(Duration::from_secs(TIMEOUT_SECS)))
            // A 4xx body says what the request got wrong; letting ureq turn the status into an
            // error would throw that explanation away.
            .http_status_as_error(false)
            .build();
        Self {
            agent: config.into(),
        }
    }
}

impl Default for HttpTransport {
    fn default() -> Self {
        Self::new()
    }
}

impl CloudTransport for HttpTransport {
    fn post(&self, request: &CloudRequest, cancel: &CancelToken) -> Result<Vec<u8>> {
        cancel.check()?;
        let mut response = self
            .agent
            .post(&request.url)
            .header("xi-api-key", request.api_key.as_str())
            .header("content-type", "application/json")
            .header("accept", "audio/pcm")
            .send(request.body.as_str())
            .map_err(|err| VoiceError::Cloud(format!("request failed: {err}")))?;
        let status = response.status();
        if !status.is_success() {
            let mut detail = String::new();
            let _ = response
                .body_mut()
                .as_reader()
                .take(400)
                .read_to_string(&mut detail);
            return Err(VoiceError::Cloud(format!(
                "HTTP {}: {}",
                status.as_u16(),
                detail.trim()
            )));
        }
        // Read in chunks rather than one `read_to_vec`, so a cancel lands within one chunk
        // instead of after the whole clip has arrived.
        let mut pcm = Vec::new();
        let mut reader = response.body_mut().as_reader();
        let mut buf = [0u8; 8192];
        loop {
            if cancel.is_cancelled() {
                return Err(VoiceError::Cancelled);
            }
            match reader.read(&mut buf) {
                Ok(0) => break,
                Ok(read) => pcm.extend_from_slice(&buf[..read]),
                Err(err) => return Err(VoiceError::Cloud(format!("read audio: {err}"))),
            }
        }
        Ok(pcm)
    }
}

/// The key from the named environment variable, or None when it is unset or blank. Blank counts
/// as unset, so an empty export cannot pass for a configured key.
pub fn read_key(name: &str) -> Option<String> {
    let name = name.trim();
    if name.is_empty() {
        return None;
    }
    std::env::var(name)
        .ok()
        .map(|value| value.trim().to_owned())
        .filter(|value| !value.is_empty())
}

/// The cloud synthesizer: request, then wrap the PCM the service returns as a WAV.
pub struct CloudSynth {
    voice_id: String,
    model: String,
    api_key_env: String,
    api_key: Option<String>,
    transport: Box<dyn CloudTransport>,
    notes: Mutex<Vec<String>>,
}

impl CloudSynth {
    pub fn new(config: &VoiceConfig) -> Self {
        Self::with_parts(
            config,
            read_key(&config.tts_api_key_env),
            Box::new(HttpTransport::new()),
        )
    }

    /// The same synthesizer with the key and the transport supplied by the caller: the seam the
    /// tests use, and the reason nothing in the suite touches the network.
    pub(crate) fn with_parts(
        config: &VoiceConfig,
        api_key: Option<String>,
        transport: Box<dyn CloudTransport>,
    ) -> Self {
        Self {
            voice_id: config.tts_voice_id.trim().to_owned(),
            model: config.tts_model.trim().to_owned(),
            api_key_env: config.tts_api_key_env.trim().to_owned(),
            api_key,
            transport,
            notes: Mutex::new(Vec::new()),
        }
    }

    /// The voice this synthesizer speaks with, as configured.
    pub fn voice_id(&self) -> &str {
        &self.voice_id
    }

    /// The model this synthesizer asks for, as configured.
    pub fn model(&self) -> &str {
        &self.model
    }

    /// Record a note once. A cloud failure repeated for every sentence is noise; the operator
    /// needs to hear it once, and the reply still has to be spoken.
    pub(crate) fn note_once(&self, prefix: &str, text: String) {
        let mut notes = self.notes.lock().unwrap_or_else(|err| err.into_inner());
        if !notes.iter().any(|note| note.starts_with(prefix)) {
            notes.push(text);
        }
    }

    fn request(&self, text: &str) -> Result<CloudRequest> {
        let api_key = self.api_key.as_deref().ok_or_else(|| {
            VoiceError::CloudUnconfigured(format!("{} is not set", self.api_key_env))
        })?;
        if self.voice_id.is_empty() {
            return Err(VoiceError::CloudUnconfigured(
                "tts_voice_id is empty; set one with `vikram voice config set tts_voice_id <id>`"
                    .into(),
            ));
        }
        Ok(CloudRequest {
            url: format!("{ENDPOINT}/{}?output_format={OUTPUT_FORMAT}", self.voice_id),
            api_key: api_key.to_owned(),
            body: serde_json::json!({ "text": text, "model_id": self.model }).to_string(),
        })
    }
}

impl Synthesizer for CloudSynth {
    fn synthesize(&self, text: &str) -> Result<Vec<u8>> {
        self.synthesize_cancellable(text, &CancelToken::new())
    }

    fn synthesize_cancellable(&self, text: &str, cancel: &CancelToken) -> Result<Vec<u8>> {
        cancel.check()?;
        let request = self.request(text)?;
        let bytes = self.transport.post(&request, cancel)?;
        let pcm = pcm_from_bytes(&bytes)?;
        if pcm.is_empty() {
            return Err(VoiceError::Cloud("the service returned no audio".into()));
        }
        Ok(wav_bytes(&pcm, SAMPLE_RATE))
    }

    fn notes(&mut self) -> Vec<String> {
        self.notes
            .lock()
            .map(|notes| notes.clone())
            .unwrap_or_else(|err| err.into_inner().clone())
    }
}

/// What `voice status` says about the cloud half: ready only when both the key and the voice id
/// are there, and a degraded line that names the exact thing to fix otherwise.
pub fn probe_cloud_tts(config: &VoiceConfig) -> StageStatus {
    let key_env = config.tts_api_key_env.trim();
    let voice_id = config.tts_voice_id.trim();
    let model = config.tts_model.trim();
    let key_missing = read_key(key_env).is_none();
    let voice_missing = voice_id.is_empty();
    match (key_missing, voice_missing) {
        (false, false) => {
            StageStatus::Ready(format!("elevenlabs (voice {voice_id}, model {model})"))
        }
        (true, false) => StageStatus::Degraded(format!(
            "tts_backend is elevenlabs but {key_env} is not set; the local voice speaks instead"
        )),
        (false, true) => StageStatus::Degraded(
            "tts_backend is elevenlabs but tts_voice_id is empty; set one with \
             `vikram voice config set tts_voice_id <id>`, or the local voice speaks instead"
                .into(),
        ),
        (true, true) => StageStatus::Degraded(format!(
            "tts_backend is elevenlabs but {key_env} is not set and tts_voice_id is empty; the \
             local voice speaks instead"
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Arc, Mutex};

    /// The calls a fake transport was handed, shared with the test that built it.
    #[derive(Clone, Default)]
    struct Calls(Arc<Mutex<Vec<CloudRequest>>>);

    impl Calls {
        fn push(&self, request: CloudRequest) {
            self.0.lock().unwrap().push(request);
        }

        fn all(&self) -> Vec<CloudRequest> {
            self.0.lock().unwrap().clone()
        }

        fn len(&self) -> usize {
            self.0.lock().unwrap().len()
        }
    }

    /// Records every request it is handed and answers with fixed PCM or a fixed failure.
    struct Fake {
        calls: Calls,
        pcm: Vec<u8>,
        fail: Option<String>,
    }

    impl Fake {
        fn answering(calls: &Calls, pcm: &[i16]) -> Self {
            Self {
                calls: calls.clone(),
                pcm: pcm.iter().flat_map(|sample| sample.to_le_bytes()).collect(),
                fail: None,
            }
        }

        fn failing(calls: &Calls, reason: &str) -> Self {
            Self {
                calls: calls.clone(),
                pcm: Vec::new(),
                fail: Some(reason.into()),
            }
        }
    }

    impl CloudTransport for Fake {
        fn post(&self, request: &CloudRequest, cancel: &CancelToken) -> Result<Vec<u8>> {
            cancel.check()?;
            self.calls.push(request.clone());
            match &self.fail {
                Some(reason) => Err(VoiceError::Cloud(reason.clone())),
                None => Ok(self.pcm.clone()),
            }
        }
    }

    fn configured() -> VoiceConfig {
        VoiceConfig {
            tts_backend: "elevenlabs".into(),
            tts_voice_id: "JBFqnCBsd6RMkjVDRZzb".into(),
            ..Default::default()
        }
    }

    fn synth(config: &VoiceConfig, key: Option<&str>, transport: Fake) -> CloudSynth {
        CloudSynth::with_parts(config, key.map(str::to_owned), Box::new(transport))
    }

    #[test]
    fn the_request_matches_the_documented_shape() {
        let config = configured();
        let calls = Calls::default();
        let cloud = synth(
            &config,
            Some("test-key"),
            Fake::answering(&calls, &[1, -2, 3]),
        );
        let wav = cloud.synthesize("turn on the lights").unwrap();

        assert_eq!(&wav[..4], b"RIFF");
        assert_eq!(&wav[8..12], b"WAVE");
        assert_eq!(u32::from_le_bytes(wav[24..28].try_into().unwrap()), 16_000);
        assert_eq!(u32::from_le_bytes(wav[40..44].try_into().unwrap()), 6);
        assert_eq!(wav.len(), 50, "44-byte header plus three samples");

        let sent = calls.all();
        assert_eq!(sent.len(), 1, "one request per sentence");
        let request = &sent[0];
        assert_eq!(
            request.url,
            "https://api.elevenlabs.io/v1/text-to-speech/JBFqnCBsd6RMkjVDRZzb?output_format=pcm_16000"
        );
        assert_eq!(request.api_key, "test-key");
        let body: serde_json::Value = serde_json::from_str(&request.body).unwrap();
        assert_eq!(body["text"], "turn on the lights");
        assert_eq!(body["model_id"], "eleven_turbo_v2_5");
    }

    #[test]
    fn a_missing_key_is_typed_and_never_reaches_the_network() {
        let config = configured();
        let calls = Calls::default();
        let cloud = synth(&config, None, Fake::answering(&calls, &[1]));
        match cloud.synthesize("hello").unwrap_err() {
            VoiceError::CloudUnconfigured(reason) => {
                assert!(reason.contains("ELEVENLABS_API_KEY"), "{reason}");
            }
            other => panic!("expected an unconfigured error, got {other:?}"),
        }
        assert_eq!(calls.len(), 0, "nothing was sent");
    }

    #[test]
    fn an_empty_voice_id_is_refused_before_the_request() {
        let config = VoiceConfig {
            tts_backend: "elevenlabs".into(),
            ..Default::default()
        };
        let calls = Calls::default();
        let cloud = synth(&config, Some("k"), Fake::answering(&calls, &[1]));
        let err = cloud.synthesize("hello").unwrap_err().to_string();
        assert!(err.contains("tts_voice_id"), "{err}");
        assert_eq!(calls.len(), 0);
    }

    #[test]
    fn a_transport_failure_is_typed_and_says_what_went_wrong() {
        let config = configured();
        let calls = Calls::default();
        let cloud = synth(
            &config,
            Some("k"),
            Fake::failing(&calls, "HTTP 401: invalid api key"),
        );
        match cloud.synthesize("hello").unwrap_err() {
            VoiceError::Cloud(reason) => assert!(reason.contains("401"), "{reason}"),
            other => panic!("expected a cloud error, got {other:?}"),
        }
    }

    #[test]
    fn a_cancelled_turn_sends_nothing() {
        let config = configured();
        let calls = Calls::default();
        let cloud = synth(&config, Some("k"), Fake::answering(&calls, &[1]));
        let cancel = CancelToken::new();
        cancel.cancel();
        assert!(matches!(
            cloud.synthesize_cancellable("hello", &cancel),
            Err(VoiceError::Cancelled)
        ));
        assert_eq!(calls.len(), 0);
    }

    #[test]
    fn an_empty_response_is_an_error_not_a_silent_clip() {
        let config = configured();
        let calls = Calls::default();
        let cloud = synth(&config, Some("k"), Fake::answering(&calls, &[]));
        let err = cloud.synthesize("hello").unwrap_err().to_string();
        assert!(err.contains("no audio"), "{err}");
    }

    #[test]
    fn the_key_is_read_from_the_named_variable_only() {
        assert!(read_key("VIKRAM_NO_SUCH_KEY_VARIABLE_SET_ANYWHERE").is_none());
        assert!(read_key("").is_none());
        assert!(read_key("  ").is_none());
    }

    #[test]
    fn status_names_the_env_var_when_the_key_is_missing() {
        let config = VoiceConfig {
            tts_backend: "elevenlabs".into(),
            tts_voice_id: "abc".into(),
            tts_api_key_env: "VIKRAM_TEST_KEY_THAT_IS_NOT_SET".into(),
            ..Default::default()
        };
        let status = probe_cloud_tts(&config);
        assert!(status.is_degraded());
        let label = status.label();
        assert!(label.contains("VIKRAM_TEST_KEY_THAT_IS_NOT_SET"), "{label}");
        assert!(label.contains("local voice"), "{label}");
    }

    #[test]
    fn status_is_ready_when_a_key_and_a_voice_are_both_there() {
        // `PATH` is set in every environment this test runs in; it is only ever read as "a
        // non-empty variable exists" here, and nothing is sent anywhere.
        let config = VoiceConfig {
            tts_backend: "elevenlabs".into(),
            tts_voice_id: "abc".into(),
            tts_api_key_env: "PATH".into(),
            ..Default::default()
        };
        assert_eq!(
            probe_cloud_tts(&config).label(),
            "ready: elevenlabs (voice abc, model eleven_turbo_v2_5)"
        );
    }

    #[test]
    fn status_names_the_voice_id_when_it_is_empty() {
        let config = VoiceConfig {
            tts_backend: "elevenlabs".into(),
            tts_api_key_env: "PATH".into(),
            ..Default::default()
        };
        let label = probe_cloud_tts(&config).label();
        assert!(label.starts_with("degraded:"), "{label}");
        assert!(label.contains("tts_voice_id"), "{label}");
    }

    #[test]
    fn a_note_is_recorded_once() {
        let calls = Calls::default();
        let mut cloud = synth(&configured(), Some("k"), Fake::answering(&calls, &[1]));
        cloud.note_once("elevenlabs", "elevenlabs: HTTP 500".into());
        cloud.note_once("elevenlabs", "elevenlabs: HTTP 502".into());
        assert_eq!(cloud.notes().len(), 1);
    }
}
