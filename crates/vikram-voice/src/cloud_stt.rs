//! The opt-in cloud recognizer: Google Cloud Speech-to-Text, or any endpoint that speaks the
//! OpenAI-compatible transcription shape.
//!
//! The local wavo model is the default and needs no key; this module exists for an operator who
//! wants a different engine. It is built so a cloud problem can never cost a turn: every failure
//! is a typed error, and [`crate::session::VoiceRecognizer`] transcribes with the local model
//! instead, saying so once.
//!
//! Two shapes, both read off the published references (2026-11-23):
//!
//! * Google v1, <https://cloud.google.com/speech-to-text/docs/reference/rest/v1/speech/recognize>:
//!   `POST https://speech.googleapis.com/v1/speech:recognize?key=<KEY>` with
//!   `{"config":{"encoding":"LINEAR16","sampleRateHertz":16000,"languageCode":"en-US",
//!   "enableAutomaticPunctuation":true},"audio":{"content":"<base64 pcm16>"}}`; the transcript is
//!   `results[0].alternatives[0].transcript`. Google documents `X-goog-api-key` as the preferred
//!   carrier and the `key` query parameter as the fallback that exposes the key in the URL
//!   (<https://docs.cloud.google.com/docs/authentication/api-keys-use>), so the URL is never
//!   logged and every error message has the key redacted.
//! * OpenAI, <https://platform.openai.com/docs/api-reference/audio/createTranscription>:
//!   `POST {base}/audio/transcriptions`, `Authorization: Bearer <KEY>`, multipart with `file`
//!   (a WAV), `model` and an optional ISO-639-1 `language`; the transcript is `{"text": …}`.
//!   `custom` is this same shape against a base URL the operator supplies, which is why "any
//!   provider" works: it is the de-facto standard transcription API.
//!
//! The key is read from the environment variable [`VoiceConfig::stt_api_key_env`] names. It is
//! held in memory for the life of the recognizer, is never written to `voice.json`, never
//! printed, and never put in an error message.

use std::io::Read;
use std::time::Duration;

use crate::cloud_tts::{CloudAuth, read_key};
use crate::helpers::wav_bytes;
use crate::session::Recognizer;
use crate::{CancelToken, Result, StageStatus, SttProvider, VoiceConfig, VoiceError};

/// Google's synchronous recognize endpoint. Overridable by `stt_base_url` (a proxy), which is
/// used verbatim because the method is the last path segment, not a base.
pub const GOOGLE_ENDPOINT: &str = "https://speech.googleapis.com/v1/speech:recognize";

/// The OpenAI preset's API base. `/audio/transcriptions` is appended to it, and to any
/// `stt_base_url` the operator sets for `openai` or `custom`.
pub const OPENAI_BASE: &str = "https://api.openai.com/v1";

/// The path the OpenAI-compatible shape posts to.
pub const OPENAI_PATH: &str = "/audio/transcriptions";

/// The OpenAI preset's default model. Any id the endpoint accepts overrides it.
pub const OPENAI_MODEL: &str = "whisper-1";

/// Uncompressed 16-bit little-endian samples: the one shape the capture path already produces.
pub const ENCODING: &str = "LINEAR16";

/// One utterance is a few seconds of audio; a minute is the point where the operator would rather
/// use the local model than wait, and `timeout_global` bounds the whole exchange.
const TIMEOUT_SECS: u64 = 60;

/// Everything that goes on the wire, as one value, so a test can assert the request shape without
/// opening a socket. `Debug` is written by hand: the key must not reach a log line even through a
/// `{:?}` in an error path.
#[derive(Clone, PartialEq, Eq)]
pub struct CloudSttRequest {
    /// The full URL. For Google it carries the key in its query, so it is never logged.
    pub url: String,
    /// The key itself, for the length of one call and for redaction. Never printed, never logged.
    pub api_key: String,
    /// How the key travels: in the URL (Google), or as a header (the OpenAI shape).
    pub auth: CloudAuth,
    /// The `Content-Type` header: JSON for Google, multipart for the OpenAI shape.
    pub content_type: String,
    pub body: Vec<u8>,
}

impl CloudSttRequest {
    /// The message with the key replaced, for any error that quotes a URL or a response.
    fn redacted(&self, message: &str) -> String {
        redact(message, &self.api_key)
    }
}

impl std::fmt::Debug for CloudSttRequest {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CloudSttRequest")
            .field("url", &redact(&self.url, &self.api_key))
            .field("api_key", &"<redacted>")
            .field("auth", &self.auth)
            .field("content_type", &self.content_type)
            .field("body_bytes", &self.body.len())
            .finish()
    }
}

fn redact(message: &str, key: &str) -> String {
    if key.trim().is_empty() {
        message.to_owned()
    } else {
        message.replace(key, "<redacted>")
    }
}

/// The one thing the recognizer needs from the network, behind a trait so tests inject a fake
/// and the suite stays offline.
pub trait SttTransport: Send + Sync {
    /// POST the request and return the raw response body. `Cancelled` when the token fired.
    fn post(&self, request: &CloudSttRequest, cancel: &CancelToken) -> Result<Vec<u8>>;
}

/// The real transport: `ureq`, the same blocking client the cloud voice uses. One transport for
/// both shapes; [`CloudSttRequest::auth`] decides where the key goes.
pub struct HttpSttTransport {
    agent: ureq::Agent,
}

impl HttpSttTransport {
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

impl Default for HttpSttTransport {
    fn default() -> Self {
        Self::new()
    }
}

impl SttTransport for HttpSttTransport {
    fn post(&self, request: &CloudSttRequest, cancel: &CancelToken) -> Result<Vec<u8>> {
        cancel.check()?;
        let mut call = self
            .agent
            .post(&request.url)
            .header("content-type", request.content_type.as_str());
        match request.auth {
            CloudAuth::None => {}
            CloudAuth::Bearer => {
                call = call.header("authorization", &format!("Bearer {}", request.api_key));
            }
            CloudAuth::Header(name) => call = call.header(name, request.api_key.as_str()),
        }
        let mut response = call.send(request.body.as_slice()).map_err(|err| {
            // ureq's own message quotes the URL, which for Google holds the key.
            VoiceError::Cloud(request.redacted(&format!("request failed: {err}")))
        })?;
        let status = response.status();
        if !status.is_success() {
            let mut detail = String::new();
            let _ = response
                .body_mut()
                .as_reader()
                .take(400)
                .read_to_string(&mut detail);
            return Err(VoiceError::Cloud(request.redacted(&format!(
                "HTTP {}: {}",
                status.as_u16(),
                detail.trim()
            ))));
        }
        // Read in chunks rather than one `read_to_vec`, so a cancel lands within one chunk
        // instead of after the whole response has arrived.
        let mut body = Vec::new();
        let mut reader = response.body_mut().as_reader();
        let mut buf = [0u8; 8192];
        loop {
            if cancel.is_cancelled() {
                return Err(VoiceError::Cancelled);
            }
            match reader.read(&mut buf) {
                Ok(0) => break,
                Ok(read) => body.extend_from_slice(&buf[..read]),
                Err(err) => {
                    return Err(VoiceError::Cloud(
                        request.redacted(&format!("read response: {err}")),
                    ));
                }
            }
        }
        Ok(body)
    }
}

/// The endpoint a cloud recognizer posts to, with no key: safe to print.
pub fn stt_endpoint_of(config: &VoiceConfig) -> Result<String> {
    let provider = SttProvider::parse(&config.stt_provider).ok_or_else(|| {
        VoiceError::Config(format!(
            "stt_provider `{}` is not one of: {}",
            config.stt_provider.trim(),
            SttProvider::NAMES
        ))
    })?;
    endpoint_for(provider, config.stt_base_url.trim())
}

/// The model a cloud recognizer names, resolved against the preset's default when `stt_model`
/// is empty. Empty means the provider selects its own (Google).
pub fn stt_model_of(config: &VoiceConfig) -> String {
    match SttProvider::parse(&config.stt_provider) {
        Some(provider) => model_for(provider, config.stt_model.trim()),
        None => config.stt_model.trim().to_owned(),
    }
}

fn endpoint_for(provider: SttProvider, base_url: &str) -> Result<String> {
    match provider {
        SttProvider::Google => Ok(if base_url.is_empty() {
            GOOGLE_ENDPOINT.to_owned()
        } else {
            base_url.to_owned()
        }),
        SttProvider::OpenAi => Ok(format!(
            "{}{OPENAI_PATH}",
            if base_url.is_empty() {
                OPENAI_BASE
            } else {
                base_url
            }
            .trim_end_matches('/')
        )),
        SttProvider::Custom => {
            if base_url.is_empty() {
                return Err(VoiceError::Config(
                    "stt_provider is custom but stt_base_url is empty; custom has no preset \
                     endpoint"
                        .into(),
                ));
            }
            Ok(format!("{}{OPENAI_PATH}", base_url.trim_end_matches('/')))
        }
        SttProvider::Local => Err(VoiceError::Config(
            "stt_provider is local; there is no cloud endpoint to call".into(),
        )),
        SttProvider::WebSpeech => Err(VoiceError::Config(
            "stt_provider is webspeech; the browser bridge serves a page on loopback, it does \
             not call a cloud endpoint"
                .into(),
        )),
    }
}

fn model_for(provider: SttProvider, model: &str) -> String {
    match provider {
        SttProvider::Local | SttProvider::WebSpeech => String::new(),
        SttProvider::Google => model.to_owned(),
        SttProvider::OpenAi => {
            if model.is_empty() {
                OPENAI_MODEL.to_owned()
            } else {
                model.to_owned()
            }
        }
        SttProvider::Custom => model.to_owned(),
    }
}

/// The cloud recognizer: build the request, post it, read the transcript back.
pub struct CloudRecognizer {
    provider: SttProvider,
    base_url: String,
    model: String,
    language: String,
    sample_rate: u16,
    api_key_env: String,
    api_key: Option<String>,
    transport: Box<dyn SttTransport>,
}

impl CloudRecognizer {
    pub fn new(config: &VoiceConfig) -> Self {
        Self::with_parts(
            config,
            read_key(&config.stt_api_key_env),
            Box::new(HttpSttTransport::new()),
        )
    }

    /// The same recognizer with the key and the transport supplied by the caller: the seam the
    /// tests use, and the reason nothing in the suite touches the network.
    pub(crate) fn with_parts(
        config: &VoiceConfig,
        api_key: Option<String>,
        transport: Box<dyn SttTransport>,
    ) -> Self {
        Self {
            provider: SttProvider::parse(&config.stt_provider).unwrap_or(SttProvider::Local),
            base_url: config.stt_base_url.trim().to_owned(),
            model: config.stt_model.trim().to_owned(),
            language: config.stt_language.trim().to_owned(),
            sample_rate: config.sample_rate,
            api_key_env: config.stt_api_key_env.trim().to_owned(),
            api_key,
            transport,
        }
    }

    /// The provider this recognizer speaks to, as configured.
    pub fn provider(&self) -> SttProvider {
        self.provider
    }

    /// The model the request will name, or empty when the provider selects its own (Google).
    pub fn model(&self) -> String {
        model_for(self.provider, &self.model)
    }

    /// The endpoint the request will post to: the preset's own unless `stt_base_url` overrides
    /// it, with `/audio/transcriptions` appended for the OpenAI-compatible shape.
    pub fn endpoint(&self) -> Result<String> {
        endpoint_for(self.provider, &self.base_url)
    }

    fn key(&self) -> Result<&str> {
        self.api_key.as_deref().ok_or_else(|| {
            VoiceError::CloudUnconfigured(format!("{} is not set", self.api_key_env))
        })
    }

    /// The Google body: the config the reference documents, and the take as base64 LINEAR16.
    fn google_request(&self, pcm: &[i16]) -> Result<CloudSttRequest> {
        let key = self.key()?;
        let endpoint = self.endpoint()?;
        let separator = if endpoint.contains('?') { '&' } else { '?' };
        let mut config = serde_json::json!({
            "encoding": ENCODING,
            "sampleRateHertz": self.sample_rate,
            "languageCode": self.language,
            "enableAutomaticPunctuation": true,
        });
        // Google auto-selects a model when none is named; an explicit `stt_model` is passed on.
        let model = self.model();
        if !model.is_empty() {
            config["model"] = serde_json::Value::String(model);
        }
        let body = serde_json::json!({
            "config": config,
            "audio": { "content": base64_encode(&pcm_bytes(pcm)) },
        });
        Ok(CloudSttRequest {
            url: format!("{endpoint}{separator}key={key}"),
            api_key: key.to_owned(),
            auth: CloudAuth::None,
            content_type: "application/json".into(),
            body: body.to_string().into_bytes(),
        })
    }

    /// The OpenAI-compatible body: a WAV file part, the model, and the language's primary
    /// subtag (the reference asks for ISO-639-1, not the BCP-47 tag Google takes).
    fn compatible_request(&self, pcm: &[i16]) -> Result<CloudSttRequest> {
        let key = self.key()?;
        let endpoint = self.endpoint()?;
        let model = self.model();
        if model.is_empty() {
            return Err(VoiceError::Config(
                "stt_model is empty; the OpenAI-compatible shape needs a model name such as \
                 whisper-1"
                    .into(),
            ));
        }
        let boundary = format!(
            "vikram-voice-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0)
        );
        let wav = wav_bytes(pcm, self.sample_rate);
        let language = primary_subtag(&self.language);
        let body = multipart(&boundary, &model, language.as_deref(), &wav);
        Ok(CloudSttRequest {
            url: endpoint,
            api_key: key.to_owned(),
            auth: CloudAuth::Bearer,
            content_type: format!("multipart/form-data; boundary={boundary}"),
            body,
        })
    }

    fn request(&self, pcm: &[i16]) -> Result<CloudSttRequest> {
        match self.provider {
            SttProvider::Google => self.google_request(pcm),
            SttProvider::OpenAi | SttProvider::Custom => self.compatible_request(pcm),
            SttProvider::Local => Err(VoiceError::Config(
                "stt_provider is local; there is no cloud request to build".into(),
            )),
            SttProvider::WebSpeech => Err(VoiceError::Config(
                "stt_provider is webspeech; the browser posts the transcript, this process does \
                 not build an HTTP request"
                    .into(),
            )),
        }
    }

    fn transcribe_impl(&self, pcm: &[i16], cancel: &CancelToken) -> Result<Option<String>> {
        cancel.check()?;
        if pcm.is_empty() {
            return Ok(None);
        }
        let request = self.request(pcm)?;
        let body = self.transport.post(&request, cancel)?;
        cancel.check()?;
        transcript_of(self.provider, &body)
    }
}

impl Recognizer for CloudRecognizer {
    fn transcribe(&mut self, pcm: &[i16]) -> Result<Option<String>> {
        self.transcribe_cancellable(pcm, &CancelToken::new())
    }

    fn transcribe_cancellable(
        &mut self,
        pcm: &[i16],
        cancel: &CancelToken,
    ) -> Result<Option<String>> {
        self.transcribe_impl(pcm, cancel)
    }
}

/// What `voice status` says about the cloud half: ready only when the key is there, and a degraded
/// line that names the exact variable to fix otherwise.
pub fn probe_cloud_stt(config: &VoiceConfig) -> StageStatus {
    let provider = match SttProvider::parse(&config.stt_provider) {
        Some(provider) if provider.is_cloud() => provider,
        _ => return crate::asr::probe_asr(config),
    };
    let key_env = config.stt_api_key_env.trim();
    if read_key(key_env).is_none() {
        return StageStatus::Degraded(format!(
            "stt_provider is {} but {key_env} is not set; the built-in wavo transcribes instead",
            provider.as_str()
        ));
    }
    let endpoint = match stt_endpoint_of(config) {
        Ok(endpoint) => endpoint,
        Err(err) => return StageStatus::Degraded(err.to_string()),
    };
    let model = stt_model_of(config);
    match provider {
        SttProvider::Google => StageStatus::Ready(format!(
            "google ({}){}",
            config.stt_language.trim(),
            if model.is_empty() {
                String::new()
            } else {
                format!(", model {model}")
            }
        )),
        other => StageStatus::Ready(format!("{} ({endpoint}, model {model})", other.as_str())),
    }
}

/// The transcript of a successful response, or `None` for a response that carries no words.
fn transcript_of(provider: SttProvider, body: &[u8]) -> Result<Option<String>> {
    match provider {
        SttProvider::Google => {
            #[derive(serde::Deserialize)]
            struct Response {
                #[serde(default)]
                results: Vec<ResultItem>,
            }
            #[derive(serde::Deserialize)]
            struct ResultItem {
                #[serde(default)]
                alternatives: Vec<Alternative>,
            }
            #[derive(serde::Deserialize)]
            struct Alternative {
                #[serde(default)]
                transcript: String,
            }
            let response: Response = serde_json::from_slice(body).map_err(|err| {
                VoiceError::Cloud(format!(
                    "the service returned no readable transcript: {err}"
                ))
            })?;
            let text = response
                .results
                .first()
                .and_then(|result| result.alternatives.first())
                .map(|alternative| alternative.transcript.trim().to_owned())
                .unwrap_or_default();
            Ok((!text.is_empty()).then_some(text))
        }
        _ => {
            #[derive(serde::Deserialize)]
            struct Response {
                #[serde(default)]
                text: String,
            }
            let response: Response = serde_json::from_slice(body).map_err(|err| {
                VoiceError::Cloud(format!(
                    "the service returned no readable transcript: {err}"
                ))
            })?;
            let text = response.text.trim().to_owned();
            Ok((!text.is_empty()).then_some(text))
        }
    }
}

/// The OpenAI shape takes an ISO-639-1 tag; `stt_language` is BCP-47 for Google, so `en-US`
/// becomes `en` here.
fn primary_subtag(language: &str) -> Option<String> {
    let language = language.trim();
    if language.is_empty() {
        return None;
    }
    Some(
        language
            .split(['-', '_'])
            .next()
            .unwrap_or(language)
            .to_ascii_lowercase(),
    )
}

fn pcm_bytes(pcm: &[i16]) -> Vec<u8> {
    pcm.iter().flat_map(|sample| sample.to_le_bytes()).collect()
}

/// One multipart body: the file part first, then the text fields. Written by hand because the
/// crate has no multipart encoder and the shape is three fields.
fn multipart(boundary: &str, model: &str, language: Option<&str>, wav: &[u8]) -> Vec<u8> {
    let mut body = Vec::with_capacity(wav.len() + 512);
    body.extend_from_slice(
        format!(
            "--{boundary}\r\nContent-Disposition: form-data; name=\"file\"; \
             filename=\"utterance.wav\"\r\nContent-Type: audio/wav\r\n\r\n"
        )
        .as_bytes(),
    );
    body.extend_from_slice(wav);
    body.extend_from_slice(format!("\r\n--{boundary}\r\n").as_bytes());
    body.extend_from_slice(
        format!("Content-Disposition: form-data; name=\"model\"\r\n\r\n{model}\r\n").as_bytes(),
    );
    if let Some(language) = language {
        body.extend_from_slice(format!("--{boundary}\r\n").as_bytes());
        body.extend_from_slice(
            format!("Content-Disposition: form-data; name=\"language\"\r\n\r\n{language}\r\n")
                .as_bytes(),
        );
    }
    body.extend_from_slice(format!("--{boundary}--\r\n").as_bytes());
    body
}

/// RFC 4648 standard base64 with padding. Implemented here rather than added as a dependency:
/// this crate stays off the runtime's graph, and the encoder is twenty lines with the RFC's own
/// test vectors.
fn base64_encode(input: &[u8]) -> String {
    const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(input.len().div_ceil(3) * 4);
    for chunk in input.chunks(3) {
        let b0 = chunk[0] as u32;
        let b1 = *chunk.get(1).unwrap_or(&0) as u32;
        let b2 = *chunk.get(2).unwrap_or(&0) as u32;
        let triple = (b0 << 16) | (b1 << 8) | b2;
        out.push(ALPHABET[(triple >> 18) as usize & 0x3f] as char);
        out.push(ALPHABET[(triple >> 12) as usize & 0x3f] as char);
        out.push(if chunk.len() > 1 {
            ALPHABET[(triple >> 6) as usize & 0x3f] as char
        } else {
            '='
        });
        out.push(if chunk.len() > 2 {
            ALPHABET[triple as usize & 0x3f] as char
        } else {
            '='
        });
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Arc, Mutex};

    /// The calls a fake transport was handed, shared with the test that built it.
    #[derive(Clone, Default)]
    struct Calls(Arc<Mutex<Vec<CloudSttRequest>>>);

    impl Calls {
        fn push(&self, request: CloudSttRequest) {
            self.0.lock().unwrap().push(request);
        }

        fn all(&self) -> Vec<CloudSttRequest> {
            self.0.lock().unwrap().clone()
        }

        fn len(&self) -> usize {
            self.0.lock().unwrap().len()
        }
    }

    /// Records every request it is handed and answers with a fixed body or a fixed failure.
    struct Fake {
        calls: Calls,
        reply: Vec<u8>,
        fail: Option<String>,
    }

    impl Fake {
        fn answering(calls: &Calls, reply: &str) -> Self {
            Self {
                calls: calls.clone(),
                reply: reply.as_bytes().to_vec(),
                fail: None,
            }
        }

        fn failing(calls: &Calls, reason: &str) -> Self {
            Self {
                calls: calls.clone(),
                reply: Vec::new(),
                fail: Some(reason.into()),
            }
        }
    }

    impl SttTransport for Fake {
        fn post(&self, request: &CloudSttRequest, cancel: &CancelToken) -> Result<Vec<u8>> {
            cancel.check()?;
            self.calls.push(request.clone());
            match &self.fail {
                Some(reason) => Err(VoiceError::Cloud(reason.clone())),
                None => Ok(self.reply.clone()),
            }
        }
    }

    fn google() -> VoiceConfig {
        VoiceConfig {
            stt_provider: "google".into(),
            ..Default::default()
        }
    }

    fn openai() -> VoiceConfig {
        VoiceConfig {
            stt_provider: "openai".into(),
            ..Default::default()
        }
    }

    fn recognizer(config: &VoiceConfig, key: Option<&str>, transport: Fake) -> CloudRecognizer {
        CloudRecognizer::with_parts(config, key.map(str::to_owned), Box::new(transport))
    }

    const GOOGLE_REPLY: &str =
        r#"{"results":[{"alternatives":[{"transcript":"turn on the lights","confidence":0.94}]}]}"#;

    #[test]
    fn the_google_request_matches_the_documented_shape() {
        let config = google();
        let calls = Calls::default();
        let mut cloud = recognizer(
            &config,
            Some("test-key"),
            Fake::answering(&calls, GOOGLE_REPLY),
        );
        let text = cloud.transcribe(&[1, -2, 3]).unwrap();
        assert_eq!(text.as_deref(), Some("turn on the lights"));

        let sent = calls.all();
        assert_eq!(sent.len(), 1, "one request per utterance");
        let request = &sent[0];
        assert_eq!(
            request.url,
            "https://speech.googleapis.com/v1/speech:recognize?key=test-key"
        );
        assert_eq!(request.api_key, "test-key");
        assert_eq!(
            request.auth,
            CloudAuth::None,
            "Google's key rides in the query, not a header"
        );
        assert_eq!(request.content_type, "application/json");

        let body: serde_json::Value = serde_json::from_slice(&request.body).unwrap();
        let top = body.as_object().expect("an object");
        assert_eq!(top.len(), 2, "exactly config and audio: {body}");
        assert_eq!(body["config"]["encoding"], "LINEAR16");
        assert_eq!(body["config"]["sampleRateHertz"], 16_000);
        assert_eq!(body["config"]["languageCode"], "en-US");
        assert_eq!(body["config"]["enableAutomaticPunctuation"], true);
        assert_eq!(
            body["config"].as_object().unwrap().len(),
            4,
            "no model is named unless configured: {body}"
        );
        assert_eq!(
            body["audio"]["content"],
            base64_encode(&pcm_bytes(&[1i16, -2, 3]))
        );
    }

    #[test]
    fn a_google_model_is_passed_on_only_when_it_is_configured() {
        let config = VoiceConfig {
            stt_model: "latest_short".into(),
            ..google()
        };
        let calls = Calls::default();
        let mut cloud = recognizer(&config, Some("k"), Fake::answering(&calls, GOOGLE_REPLY));
        cloud.transcribe(&[1]).unwrap();
        let body: serde_json::Value = serde_json::from_slice(&calls.all()[0].body).unwrap();
        assert_eq!(body["config"]["model"], "latest_short");
        assert_eq!(body["config"].as_object().unwrap().len(), 5);
    }

    #[test]
    fn the_openai_request_is_multipart_with_the_documented_fields() {
        let config = openai();
        let calls = Calls::default();
        let mut cloud = recognizer(
            &config,
            Some("test-key"),
            Fake::answering(&calls, r#"{"text":"turn on the lights"}"#),
        );
        assert_eq!(
            cloud.transcribe(&[1, -2, 3]).unwrap().as_deref(),
            Some("turn on the lights")
        );

        let request = &calls.all()[0];
        assert_eq!(
            request.url,
            "https://api.openai.com/v1/audio/transcriptions"
        );
        assert_eq!(request.api_key, "test-key");
        assert_eq!(
            request.auth,
            CloudAuth::Bearer,
            "the OpenAI shape authenticates with a bearer header"
        );
        let prefix = "multipart/form-data; boundary=";
        assert!(request.content_type.starts_with(prefix), "{request:?}");
        let boundary = &request.content_type[prefix.len()..];

        let body = String::from_utf8_lossy(&request.body);
        assert!(body.contains(&format!("--{boundary}\r\n")), "{body}");
        assert!(
            body.contains(
                "Content-Disposition: form-data; name=\"file\"; \
                            filename=\"utterance.wav\""
            ) || body.contains("name=\"file\"; filename=\"utterance.wav\""),
            "{body}"
        );
        assert!(body.contains("Content-Type: audio/wav"), "{body}");
        assert!(body.contains("name=\"model\"\r\n\r\nwhisper-1"), "{body}");
        assert!(
            body.contains("name=\"language\"\r\n\r\nen\r\n"),
            "the BCP-47 tag is reduced to ISO-639-1: {body}"
        );
        assert!(
            body.contains("RIFF"),
            "the file part is the WAV the writer produces: {body}"
        );
        assert!(body.ends_with(&format!("--{boundary}--\r\n")), "{body}");
    }

    #[test]
    fn a_preset_is_overridden_by_an_explicit_base_url_and_model() {
        let config = VoiceConfig {
            stt_base_url: "https://speech.example.test/v1/".into(),
            stt_model: "gpt-4o-transcribe".into(),
            ..openai()
        };
        let calls = Calls::default();
        let mut cloud = recognizer(
            &config,
            Some("k"),
            Fake::answering(&calls, r#"{"text":"hi"}"#),
        );
        cloud.transcribe(&[1]).unwrap();
        let request = &calls.all()[0];
        assert_eq!(
            request.url, "https://speech.example.test/v1/audio/transcriptions",
            "the preset base is replaced, not appended to"
        );
        let body = String::from_utf8_lossy(&request.body);
        assert!(
            body.contains("name=\"model\"\r\n\r\ngpt-4o-transcribe"),
            "{body}"
        );
    }

    #[test]
    fn custom_uses_its_own_base_url_and_refuses_without_one() {
        let config = VoiceConfig {
            stt_provider: "custom".into(),
            stt_base_url: "https://asr.example.test/v1".into(),
            stt_model: "whisper-1".into(),
            ..Default::default()
        };
        let calls = Calls::default();
        let mut cloud = recognizer(
            &config,
            Some("k"),
            Fake::answering(&calls, r#"{"text":"hi"}"#),
        );
        cloud.transcribe(&[1]).unwrap();
        assert_eq!(
            calls.all()[0].url,
            "https://asr.example.test/v1/audio/transcriptions"
        );

        let no_url = VoiceConfig {
            stt_provider: "custom".into(),
            ..Default::default()
        };
        let calls = Calls::default();
        let mut cloud = recognizer(&no_url, Some("k"), Fake::answering(&calls, "{}"));
        let err = cloud.transcribe(&[1]).unwrap_err().to_string();
        assert!(err.contains("stt_base_url"), "{err}");
        assert_eq!(calls.len(), 0, "nothing was sent");
    }

    #[test]
    fn a_missing_key_is_typed_and_never_reaches_the_network() {
        for config in [google(), openai()] {
            let calls = Calls::default();
            let mut cloud = recognizer(&config, None, Fake::answering(&calls, GOOGLE_REPLY));
            match cloud.transcribe(&[1]).unwrap_err() {
                VoiceError::CloudUnconfigured(reason) => {
                    assert!(reason.contains("GOOGLE_API_KEY"), "{reason}");
                }
                other => panic!("expected an unconfigured error, got {other:?}"),
            }
            assert_eq!(calls.len(), 0, "nothing was sent");
        }
    }

    #[test]
    fn a_transport_failure_is_typed_and_says_what_went_wrong() {
        let calls = Calls::default();
        let mut cloud = recognizer(
            &google(),
            Some("k"),
            Fake::failing(&calls, "HTTP 403: PERMISSION_DENIED"),
        );
        match cloud.transcribe(&[1]).unwrap_err() {
            VoiceError::Cloud(reason) => assert!(reason.contains("403"), "{reason}"),
            other => panic!("expected a cloud error, got {other:?}"),
        }
    }

    #[test]
    fn a_cancelled_turn_sends_nothing() {
        let calls = Calls::default();
        let mut cloud = recognizer(&google(), Some("k"), Fake::answering(&calls, GOOGLE_REPLY));
        let cancel = CancelToken::new();
        cancel.cancel();
        assert!(matches!(
            cloud.transcribe_cancellable(&[1], &cancel),
            Err(VoiceError::Cancelled)
        ));
        assert_eq!(calls.len(), 0);
    }

    #[test]
    fn an_empty_take_sends_nothing_and_an_empty_result_is_silence() {
        let calls = Calls::default();
        let mut cloud = recognizer(&google(), Some("k"), Fake::answering(&calls, GOOGLE_REPLY));
        assert_eq!(cloud.transcribe(&[]).unwrap(), None);
        assert_eq!(calls.len(), 0, "silence is not worth a request");

        let calls = Calls::default();
        let mut cloud = recognizer(
            &google(),
            Some("k"),
            Fake::answering(&calls, r#"{"results":[]}"#),
        );
        assert_eq!(cloud.transcribe(&[1]).unwrap(), None);
    }

    #[test]
    fn a_body_without_a_transcript_is_an_error_not_a_blank_turn() {
        let calls = Calls::default();
        let mut cloud = recognizer(&google(), Some("k"), Fake::answering(&calls, "not json"));
        let err = cloud.transcribe(&[1]).unwrap_err().to_string();
        assert!(err.contains("no readable transcript"), "{err}");
    }

    #[test]
    fn base64_matches_the_rfc_vectors() {
        assert_eq!(base64_encode(b""), "");
        assert_eq!(base64_encode(b"f"), "Zg==");
        assert_eq!(base64_encode(b"fo"), "Zm8=");
        assert_eq!(base64_encode(b"foo"), "Zm9v");
        assert_eq!(base64_encode(b"foob"), "Zm9vYg==");
        assert_eq!(base64_encode(b"fooba"), "Zm9vYmE=");
        assert_eq!(base64_encode(b"foobar"), "Zm9vYmFy");
        assert_eq!(base64_encode(&[0xFB, 0xFF, 0xFE]), "+//+");
        assert_eq!(base64_encode(&[0xFB, 0xFF]), "+/8=");
    }

    #[test]
    fn the_key_is_redacted_from_anything_that_quotes_the_url() {
        let message =
            "request failed: https://speech.googleapis.com/v1/speech:recognize?key=SECRET123";
        assert_eq!(
            redact(message, "SECRET123"),
            "request failed: https://speech.googleapis.com/v1/speech:recognize?key=<redacted>"
        );
        assert_eq!(redact(message, ""), message);
    }

    #[test]
    fn status_names_the_env_var_when_the_key_is_missing() {
        let config = VoiceConfig {
            stt_provider: "google".into(),
            stt_api_key_env: "VIKRAM_TEST_KEY_THAT_IS_NOT_SET".into(),
            ..Default::default()
        };
        let status = probe_cloud_stt(&config);
        assert!(status.is_degraded());
        let label = status.label();
        assert!(label.contains("VIKRAM_TEST_KEY_THAT_IS_NOT_SET"), "{label}");
        assert!(label.contains("built-in wavo"), "{label}");
    }

    #[test]
    fn status_is_ready_and_names_the_language_or_the_endpoint() {
        // `PATH` is set in every environment this test runs in; it is only read as "a non-empty
        // variable exists" here, and nothing is sent anywhere.
        let google = VoiceConfig {
            stt_provider: "google".into(),
            stt_api_key_env: "PATH".into(),
            ..Default::default()
        };
        assert_eq!(probe_cloud_stt(&google).label(), "ready: google (en-US)");

        let openai = VoiceConfig {
            stt_provider: "openai".into(),
            stt_api_key_env: "PATH".into(),
            ..Default::default()
        };
        assert_eq!(
            probe_cloud_stt(&openai).label(),
            "ready: openai (https://api.openai.com/v1/audio/transcriptions, model whisper-1)"
        );
    }

    #[test]
    fn the_local_provider_probes_the_local_engine() {
        let config = VoiceConfig::default();
        assert_eq!(probe_cloud_stt(&config), crate::asr::probe_asr(&config));
    }
}
