//! The opt-in cloud synthesizers: ElevenLabs text-to-speech, or any endpoint that speaks the
//! OpenAI-compatible speech shape.
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
//! The OpenAI-compatible shape (`tts_provider` `openai` or `custom`) is
//! `POST {base}/audio/speech` with `Authorization: Bearer <KEY>` and
//! `{"model": …, "input": …, "voice": …, "response_format": "wav"}`, read off
//! <https://platform.openai.com/docs/api-reference/audio/createSpeech> (2026-11-23). It answers
//! with an audio file, so a WAV body is played as it arrives and a raw PCM body is wrapped the
//! same way the ElevenLabs reply is.
//!
//! The key is read from the environment variable [`VoiceConfig::tts_api_key_env`] names. It is
//! held in memory for the life of the synthesizer, is never written to `voice.json`, never
//! printed, and never put in an error message.

use std::io::Read;
use std::sync::Mutex;
use std::time::Duration;

use crate::helpers::{pcm_from_bytes, wav_bytes};
use crate::session::Synthesizer;
use crate::{CancelToken, Result, StageStatus, TtsProvider, VoiceConfig, VoiceError};

/// ElevenLabs' default host. Regional hosts exist (`api.eu.residency.elevenlabs.io` and friends);
/// a key issued anywhere works against the default one.
pub const ENDPOINT: &str = "https://api.elevenlabs.io/v1/text-to-speech";

/// Raw 16 kHz mono s16le: no container to decode, and 16 kHz is the rate the rest of the stack
/// already speaks.
pub const OUTPUT_FORMAT: &str = "pcm_16000";

/// The rate [`OUTPUT_FORMAT`] names, for the WAV header.
pub const SAMPLE_RATE: u16 = 16_000;

/// ElevenLabs' low-latency model, used when `tts_model` is empty.
pub const ELEVENLABS_MODEL: &str = "eleven_turbo_v2_5";

/// The OpenAI preset's API base. `/audio/speech` is appended to it, and to any `tts_base_url`
/// the operator sets for `openai` or `custom`.
pub const OPENAI_BASE: &str = "https://api.openai.com/v1";

/// The path the OpenAI-compatible speech shape posts to.
pub const OPENAI_PATH: &str = "/audio/speech";

/// The OpenAI preset's default model, used when `tts_model` is empty. Any id the endpoint accepts
/// overrides it.
pub const OPENAI_MODEL: &str = "tts-1";

/// The OpenAI preset's default voice, used when `tts_voice_id` is empty: the shape requires one.
pub const OPENAI_VOICE: &str = "alloy";

/// How the key travels on the wire.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CloudAuth {
    /// No key: it rides in the URL (Google's `?key=`), or the endpoint needs none.
    None,
    /// `Authorization: Bearer <key>`: the OpenAI-compatible shape.
    Bearer,
    /// A named header, such as ElevenLabs' `xi-api-key`.
    Header(&'static str),
}

/// One spoken sentence is a few hundred KB of PCM; a minute is the point where the operator
/// would rather hear the local voice than wait, and `timeout_global` bounds the whole exchange.
const TIMEOUT_SECS: u64 = 60;

/// Everything that goes on the wire, as one value, so a test can assert the request shape
/// without opening a socket. `Debug` is written by hand: the key must not reach a log line even
/// through a `{:?}` in an error path.
#[derive(Clone, PartialEq, Eq)]
pub struct CloudRequest {
    pub url: String,
    /// The key itself, for the length of one call. Never printed, never logged.
    pub api_key: String,
    /// How the key travels: a header for every TTS provider this module speaks.
    pub auth: CloudAuth,
    pub body: String,
}

impl std::fmt::Debug for CloudRequest {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CloudRequest")
            .field("url", &self.url)
            .field("api_key", &"<redacted>")
            .field("auth", &self.auth)
            .field("body", &self.body)
            .finish()
    }
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
        let mut call = self
            .agent
            .post(&request.url)
            .header("content-type", "application/json")
            .header("accept", "audio/pcm, audio/wav");
        call = match request.auth {
            CloudAuth::None => call,
            CloudAuth::Bearer => {
                call.header("authorization", &format!("Bearer {}", request.api_key))
            }
            CloudAuth::Header(name) => call.header(name, request.api_key.as_str()),
        };
        let mut response = call
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

/// The base a cloud synthesizer posts to: `tts_base_url` when set, else the provider's own.
/// No voice id and no key, so it is safe to print.
pub fn tts_base_of(config: &VoiceConfig) -> Result<String> {
    let configured = config.tts_base_url.trim();
    match TtsProvider::parse(&config.tts_provider) {
        Some(TtsProvider::ElevenLabs) => Ok(if configured.is_empty() {
            ENDPOINT.to_owned()
        } else {
            configured.to_owned()
        }),
        Some(TtsProvider::OpenAi) => Ok(if configured.is_empty() {
            OPENAI_BASE.to_owned()
        } else {
            configured.to_owned()
        }),
        Some(TtsProvider::Custom) => {
            if configured.is_empty() {
                return Err(VoiceError::Config(
                    "tts_provider is custom but tts_base_url is empty; custom has no preset \
                     endpoint"
                        .into(),
                ));
            }
            Ok(configured.to_owned())
        }
        _ => Err(VoiceError::Config(
            "tts_provider is vikram; there is no cloud endpoint to call".into(),
        )),
    }
}

/// The model id a cloud synthesizer asks for, resolved against the provider's default when
/// `tts_model` is empty.
pub fn tts_model_of(config: &VoiceConfig) -> String {
    let configured = config.tts_model.trim();
    if !configured.is_empty() {
        return configured.to_owned();
    }
    match TtsProvider::parse(&config.tts_provider) {
        Some(TtsProvider::OpenAi | TtsProvider::Custom) => OPENAI_MODEL.to_owned(),
        _ => ELEVENLABS_MODEL.to_owned(),
    }
}

/// The voice a cloud synthesizer asks for, resolved against the OpenAI shape's default when
/// `tts_voice_id` is empty. ElevenLabs has no default on purpose: a baked-in id would bill an
/// operator who never chose one.
pub fn tts_voice_of(config: &VoiceConfig) -> String {
    let configured = config.tts_voice_id.trim();
    if !configured.is_empty() {
        return configured.to_owned();
    }
    match TtsProvider::parse(&config.tts_provider) {
        Some(TtsProvider::OpenAi | TtsProvider::Custom) => OPENAI_VOICE.to_owned(),
        _ => String::new(),
    }
}

/// The cloud synthesizer: request, then wrap the PCM the service returns as a WAV.
pub struct CloudSynth {
    voice_id: String,
    model: String,
    base_url: String,
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
            voice_id: tts_voice_of(config),
            model: tts_model_of(config),
            base_url: tts_base_of(config).unwrap_or_else(|_| ENDPOINT.to_owned()),
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
        let base = self.base_url.trim_end_matches('/');
        Ok(CloudRequest {
            url: format!("{base}/{}?output_format={OUTPUT_FORMAT}", self.voice_id),
            api_key: api_key.to_owned(),
            auth: CloudAuth::Header("xi-api-key"),
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
    let model = tts_model_of(config);
    let key_missing = read_key(key_env).is_none();
    let voice_missing = voice_id.is_empty();
    match (key_missing, voice_missing) {
        (false, false) => StageStatus::Ready(format!(
            "elevenlabs (voice {voice_id}, model {model}, {})",
            tts_base_of(config).unwrap_or_else(|_| ENDPOINT.to_owned())
        )),
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

/// The OpenAI-compatible synthesizer: `POST {base}/audio/speech` with a bearer key. `openai` is
/// this shape against OpenAI's base; `custom` is the same shape against a base the operator
/// supplies, which is what makes "any provider" work.
pub struct OpenAiSynth {
    provider: TtsProvider,
    base_url: String,
    model: String,
    voice: String,
    api_key_env: String,
    api_key: Option<String>,
    transport: Box<dyn CloudTransport>,
    notes: Mutex<Vec<String>>,
}

impl OpenAiSynth {
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
            provider: TtsProvider::parse(&config.tts_provider).unwrap_or(TtsProvider::OpenAi),
            base_url: tts_base_of(config).unwrap_or_default(),
            model: tts_model_of(config),
            voice: tts_voice_of(config),
            api_key_env: config.tts_api_key_env.trim().to_owned(),
            api_key,
            transport,
            notes: Mutex::new(Vec::new()),
        }
    }

    /// The provider this synthesizer speaks to, as configured.
    pub fn provider(&self) -> TtsProvider {
        self.provider
    }

    /// The model the request names.
    pub fn model(&self) -> &str {
        &self.model
    }

    /// The voice the request names.
    pub fn voice(&self) -> &str {
        &self.voice
    }

    /// The endpoint the request posts to, with `/audio/speech` appended.
    pub fn endpoint(&self) -> Result<String> {
        if self.base_url.is_empty() {
            return Err(VoiceError::Config(
                "tts_provider is custom but tts_base_url is empty; custom has no preset endpoint"
                    .into(),
            ));
        }
        Ok(format!(
            "{}{OPENAI_PATH}",
            self.base_url.trim_end_matches('/')
        ))
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
        Ok(CloudRequest {
            url: self.endpoint()?,
            api_key: api_key.to_owned(),
            auth: CloudAuth::Bearer,
            body: serde_json::json!({
                "model": self.model,
                "input": text,
                "voice": self.voice,
                "response_format": "wav",
            })
            .to_string(),
        })
    }
}

impl Synthesizer for OpenAiSynth {
    fn synthesize(&self, text: &str) -> Result<Vec<u8>> {
        self.synthesize_cancellable(text, &CancelToken::new())
    }

    fn synthesize_cancellable(&self, text: &str, cancel: &CancelToken) -> Result<Vec<u8>> {
        cancel.check()?;
        let request = self.request(text)?;
        let bytes = self.transport.post(&request, cancel)?;
        if bytes.is_empty() {
            return Err(VoiceError::Cloud("the service returned no audio".into()));
        }
        // `response_format: "wav"` asks for a container, and that is played as it arrives. A body
        // that is not WAV is treated as raw PCM and wrapped at the rate the rest of the stack
        // speaks; the request asks for wav, so this is a tolerance path, not the expected shape.
        if bytes.len() >= 12 && &bytes[..4] == b"RIFF" && &bytes[8..12] == b"WAVE" {
            return Ok(bytes);
        }
        let pcm = pcm_from_bytes(&bytes)?;
        Ok(wav_bytes(&pcm, SAMPLE_RATE))
    }

    fn notes(&mut self) -> Vec<String> {
        self.notes
            .lock()
            .map(|notes| notes.clone())
            .unwrap_or_else(|err| err.into_inner().clone())
    }
}

/// What `voice status` says about the OpenAI-compatible half: ready only when the key is there,
/// and a degraded line that names the exact variable to fix otherwise.
pub fn probe_openai_tts(config: &VoiceConfig) -> StageStatus {
    let provider = match TtsProvider::parse(&config.tts_provider) {
        Some(provider) if provider.is_compatible() => provider,
        _ => return StageStatus::Missing("no cloud synthesizer is selected".into()),
    };
    let key_env = config.tts_api_key_env.trim();
    if read_key(key_env).is_none() {
        return StageStatus::Degraded(format!(
            "tts_provider is {} but {key_env} is not set; the vikram voice speaks instead",
            provider.as_str()
        ));
    }
    match tts_base_of(config) {
        Ok(base) => StageStatus::Ready(format!(
            "{} ({}{OPENAI_PATH}, model {}, voice {})",
            provider.as_str(),
            base.trim_end_matches('/'),
            tts_model_of(config),
            tts_voice_of(config)
        )),
        Err(err) => StageStatus::Degraded(err.to_string()),
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
        fn answering_bytes(calls: &Calls, body: &[u8]) -> Self {
            Self {
                calls: calls.clone(),
                pcm: body.to_vec(),
                fail: None,
            }
        }

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
            tts_provider: "elevenlabs".into(),
            tts_voice_id: "JBFqnCBsd6RMkjVDRZzb".into(),
            ..Default::default()
        }
    }

    fn openai() -> VoiceConfig {
        VoiceConfig {
            tts_provider: "openai".into(),
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
        assert_eq!(
            request.auth,
            CloudAuth::Header("xi-api-key"),
            "ElevenLabs authenticates with its own header"
        );
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
            tts_provider: "elevenlabs".into(),
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
            tts_provider: "elevenlabs".into(),
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
            tts_provider: "elevenlabs".into(),
            tts_voice_id: "abc".into(),
            tts_api_key_env: "PATH".into(),
            ..Default::default()
        };
        assert_eq!(
            probe_cloud_tts(&config).label(),
            "ready: elevenlabs (voice abc, model eleven_turbo_v2_5, \
             https://api.elevenlabs.io/v1/text-to-speech)"
        );
    }

    #[test]
    fn status_names_the_voice_id_when_it_is_empty() {
        let config = VoiceConfig {
            tts_provider: "elevenlabs".into(),
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

    fn openai_synth(config: &VoiceConfig, key: Option<&str>, transport: Fake) -> OpenAiSynth {
        OpenAiSynth::with_parts(config, key.map(str::to_owned), Box::new(transport))
    }

    #[test]
    fn the_openai_request_matches_the_documented_shape() {
        let config = openai();
        let calls = Calls::default();
        let cloud = openai_synth(
            &config,
            Some("test-key"),
            Fake::answering(&calls, &[1, -2, 3]),
        );
        // A body that is not WAV is wrapped; a WAV body is passed through (see the next test).
        let wav = cloud.synthesize("turn on the lights").unwrap();
        assert_eq!(&wav[..4], b"RIFF");
        assert_eq!(wav.len(), 50, "44-byte header plus three samples");

        let sent = calls.all();
        assert_eq!(sent.len(), 1);
        let request = &sent[0];
        assert_eq!(request.url, "https://api.openai.com/v1/audio/speech");
        assert_eq!(request.api_key, "test-key");
        assert_eq!(request.auth, CloudAuth::Bearer);
        let body: serde_json::Value = serde_json::from_str(&request.body).unwrap();
        assert_eq!(body["model"], "tts-1");
        assert_eq!(body["input"], "turn on the lights");
        assert_eq!(body["voice"], "alloy");
        assert_eq!(body["response_format"], "wav");
        assert_eq!(body.as_object().unwrap().len(), 4, "{body}");
    }

    #[test]
    fn a_wav_reply_is_played_as_it_arrives_and_a_preset_is_overridable() {
        let config = VoiceConfig {
            tts_base_url: "https://speech.example.test/v1/".into(),
            tts_model: "gpt-4o-mini-tts".into(),
            tts_voice_id: "nova".into(),
            ..openai()
        };
        let calls = Calls::default();
        let mut wav = b"RIFF".to_vec();
        wav.extend_from_slice(&[0u8; 4]);
        wav.extend_from_slice(b"WAVEfmt ");
        let cloud = openai_synth(&config, Some("k"), Fake::answering_bytes(&calls, &wav));
        assert_eq!(cloud.synthesize("hello").unwrap(), wav);

        let request = &calls.all()[0];
        assert_eq!(
            request.url, "https://speech.example.test/v1/audio/speech",
            "the preset base is replaced, not appended to"
        );
        let body: serde_json::Value = serde_json::from_str(&request.body).unwrap();
        assert_eq!(body["model"], "gpt-4o-mini-tts");
        assert_eq!(body["voice"], "nova");
    }

    #[test]
    fn custom_uses_its_own_base_url_and_refuses_without_one() {
        let config = VoiceConfig {
            tts_provider: "custom".into(),
            tts_base_url: "https://tts.example.test/v1".into(),
            ..Default::default()
        };
        let calls = Calls::default();
        let cloud = openai_synth(&config, Some("k"), Fake::answering(&calls, &[1]));
        cloud.synthesize("hello").unwrap();
        assert_eq!(
            calls.all()[0].url,
            "https://tts.example.test/v1/audio/speech"
        );

        let no_url = VoiceConfig {
            tts_provider: "custom".into(),
            ..Default::default()
        };
        let calls = Calls::default();
        let cloud = openai_synth(&no_url, Some("k"), Fake::answering(&calls, &[1]));
        let err = cloud.synthesize("hello").unwrap_err().to_string();
        assert!(err.contains("tts_base_url"), "{err}");
        assert_eq!(calls.len(), 0, "nothing was sent");
    }

    #[test]
    fn an_openai_missing_key_is_typed_and_never_reaches_the_network() {
        let calls = Calls::default();
        let cloud = openai_synth(&openai(), None, Fake::answering(&calls, &[1]));
        match cloud.synthesize("hello").unwrap_err() {
            VoiceError::CloudUnconfigured(reason) => {
                assert!(reason.contains("ELEVENLABS_API_KEY"), "{reason}");
            }
            other => panic!("expected an unconfigured error, got {other:?}"),
        }
        assert_eq!(calls.len(), 0);
    }

    #[test]
    fn the_openai_status_names_the_endpoint_and_model_without_the_key() {
        let config = VoiceConfig {
            tts_api_key_env: "PATH".into(),
            ..openai()
        };
        assert_eq!(
            probe_openai_tts(&config).label(),
            "ready: openai (https://api.openai.com/v1/audio/speech, model tts-1, voice alloy)"
        );

        let missing = VoiceConfig {
            tts_api_key_env: "VIKRAM_TEST_KEY_THAT_IS_NOT_SET".into(),
            ..openai()
        };
        let label = probe_openai_tts(&missing).label();
        assert!(label.starts_with("degraded:"), "{label}");
        assert!(label.contains("VIKRAM_TEST_KEY_THAT_IS_NOT_SET"), "{label}");
        assert!(label.contains("vikram voice"), "{label}");
    }
}
