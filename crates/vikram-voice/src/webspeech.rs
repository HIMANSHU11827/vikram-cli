//! The optional keyless browser bridge: Chrome's `webkitSpeechRecognition` does the
//! transcription, and Vikram only carries the text back.
//!
//! `local` stays the default and needs no browser. This module exists for an operator who wants
//! speech-to-text without a key, an account or a model download. Chrome and Edge ship a speech
//! engine that is free and keyless **inside a page** — there is no such API for a process — so
//! the recognizer serves one small page on loopback, opens it, and waits for the transcript the
//! page posts back:
//!
//! 1. [`WebSpeechServer::bind`] listens on `127.0.0.1:0` (a random port) with a random
//!    single-use token, for `webspeech_timeout_ms` (60 s by default).
//! 2. [`WebSpeechServer::page`] is the whole page: no external asset, `webkitSpeechRecognition`
//!    started with `stt_language`, and a `fetch` POST of `{"text": "..."}` to
//!    `/result?token=<token>`.
//! 3. [`LocalBrowserTransport`] opens it with `xdg-open`.
//! 4. [`WebSpeechServer::wait`] accepts the POST, answers it, returns the transcript and closes
//!    the listener.
//!
//! **Where the audio goes:** the page sends it to Google's speech service, the same endpoint
//! Chrome uses for its own dictation. That is free and keyless, and it is also a third party:
//! `docs/voice-plan.md` §3.8 says so plainly. Nothing is uploaded by Vikram itself, no key is
//! involved, and `local` never leaves the machine.
//!
//! **What it needs:** a Chrome-family browser with Google's speech engine enabled. Plain
//! Chromium builds usually ship without the API key and fail with `network` or `not-allowed`;
//! Firefox has no recognition API at all. Every such failure is one typed error, and
//! [`crate::session::VoiceRecognizer`] falls back to the built-in model with one note — the
//! browser is never the only way to finish a turn.
//!
//! The HTTP is hand-written on `std::net::TcpListener` on purpose: two request shapes and one
//! response do not justify a server dependency in a crate that is otherwise dependency-frugal.

use std::io::{self, Read, Write};
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use crate::helpers::which;
use crate::session::Recognizer;
use crate::{CancelToken, Result, StageStatus, VoiceConfig, VoiceError};

/// The default wait for the browser, matching [`VoiceConfig::webspeech_timeout_ms`]. An
/// utterance is a few seconds; a minute is the point where the local model is the better answer.
pub const DEFAULT_TIMEOUT_MS: u64 = 60_000;

/// The only program this module runs: the desktop's own URL opener. No browser is hard-coded.
pub const OPENER: &str = "xdg-open";

/// A browser that hangs up mid-request must not hold the accept loop.
const READ_TIMEOUT: Duration = Duration::from_secs(2);

/// How often the accept loop looks at the clock and the cancel token.
const POLL: Duration = Duration::from_millis(20);

/// A request head larger than this is not a browser posting a transcript.
const MAX_HEADER_BYTES: usize = 8 * 1024;

/// A transcript larger than this is not a transcript.
const MAX_BODY_BYTES: usize = 64 * 1024;

/// What the bridge needs to know for one turn. `Debug` is safe: neither field is the token.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct WebSpeechRequest {
    /// The BCP-47 tag the page sets on `SpeechRecognition.lang`, from `stt_language`.
    pub language: String,
    /// How long to wait for the browser, from `webspeech_timeout_ms`.
    pub timeout_ms: u64,
}

/// The one thing the recognizer needs from a browser, behind a trait so the tests inject a fake
/// and the suite stays offline and window-free.
pub trait WebSpeechTransport: Send + Sync {
    /// Open the bridge and return the transcript the browser posted, or the typed reason it did
    /// not. `Cancelled` when the token fired.
    fn transcribe(
        &self,
        request: &WebSpeechRequest,
        cancel: &CancelToken,
    ) -> Result<Option<String>>;
}

/// What one connection meant to the bridge.
pub(crate) enum Outcome {
    /// The page was served, or the request was refused: keep waiting for the transcript.
    KeepWaiting,
    /// A valid POST carried these words. The turn is over.
    Text(String),
    /// A valid POST carried no words, or said why it could not listen. The turn is over, as a
    /// failure the session reports and falls back from.
    NoText(String),
}

/// The loopback listener and the page it serves: one turn, one token, one POST.
pub struct WebSpeechServer {
    /// `None` once the turn is over, which is what closes the socket before `wait` returns.
    listener: Option<TcpListener>,
    addr: SocketAddr,
    token: String,
    language: String,
    timeout: Duration,
    /// A second POST is refused: the browser answers once, and a local process that guessed the
    /// token does not get to replace the words the operator spoke.
    served: bool,
}

impl WebSpeechServer {
    /// Bind a fresh loopback listener and mint the single-use token. Nothing is opened yet.
    pub fn bind(request: &WebSpeechRequest) -> Result<Self> {
        let listener = TcpListener::bind(("127.0.0.1", 0)).map_err(|err| {
            VoiceError::WebSpeech(format!("cannot bind a loopback listener: {err}"))
        })?;
        listener
            .set_nonblocking(true)
            .map_err(|err| VoiceError::WebSpeech(format!("cannot poll the listener: {err}")))?;
        let addr = listener.local_addr().map_err(|err| {
            VoiceError::WebSpeech(format!("cannot read the bound address: {err}"))
        })?;
        Ok(Self {
            listener: Some(listener),
            addr,
            token: random_token(),
            language: request.language.trim().to_owned(),
            timeout: Duration::from_millis(request.timeout_ms.max(1)),
            served: false,
        })
    }

    /// The address the page is served on. Loopback only.
    pub fn addr(&self) -> SocketAddr {
        self.addr
    }

    /// The URL to hand to the browser. It carries the token, so it is never logged, never
    /// printed and never put in an error message.
    pub fn url(&self) -> String {
        format!("http://{}/?token={}", self.addr, self.token)
    }

    /// The single-use token, for the tests that prove a wrong one is refused.
    pub fn token(&self) -> &str {
        &self.token
    }

    /// The one page the bridge serves: inline, no external asset, no network of its own.
    pub fn page(&self) -> String {
        PAGE.replace("__LANG__", &json_string(&self.language))
            .replace("__TOKEN__", &json_string(&self.token))
    }

    /// Accept connections until the browser posts a transcript, the deadline passes, or the
    /// operator cancels. The listener is closed on every exit path, including the error ones.
    pub fn wait(&mut self, cancel: &CancelToken) -> Result<Option<String>> {
        let deadline = Instant::now() + self.timeout;
        loop {
            if cancel.is_cancelled() {
                self.listener = None;
                return Err(VoiceError::Cancelled);
            }
            if Instant::now() >= deadline {
                self.listener = None;
                return Err(VoiceError::Timeout(self.timeout.as_secs().max(1)));
            }
            let accepted = match self.listener.as_ref() {
                Some(listener) => listener.accept(),
                None => {
                    return Err(VoiceError::WebSpeech(
                        "the bridge closed before a transcript arrived".into(),
                    ));
                }
            };
            match accepted {
                Ok((mut stream, _)) => match self.handle_connection(&mut stream) {
                    Ok(Outcome::Text(text)) => {
                        self.listener = None;
                        return Ok(Some(text));
                    }
                    Ok(Outcome::NoText(reason)) => {
                        self.listener = None;
                        return Err(VoiceError::WebSpeech(reason));
                    }
                    // A served page, a refused token, a stray request: all normal while waiting.
                    Ok(Outcome::KeepWaiting) => {}
                    // A client that hung up mid-request is not the browser giving up; the
                    // deadline still bounds the wait.
                    Err(_) => {}
                },
                Err(err) if err.kind() == io::ErrorKind::WouldBlock => {
                    std::thread::sleep(POLL);
                }
                Err(err) => {
                    self.listener = None;
                    return Err(VoiceError::WebSpeech(format!("bridge listener: {err}")));
                }
            }
        }
    }

    /// One request, one response. Public to the crate so the tests can drive the refusal paths
    /// without a browser.
    pub(crate) fn handle_connection(&mut self, stream: &mut TcpStream) -> io::Result<Outcome> {
        let (method, target, body) = read_request(stream)?;
        let (path, query) = target.split_once('?').unwrap_or((target.as_str(), ""));
        let authorized = query_param(query, "token").as_deref() == Some(self.token.as_str());
        if !authorized {
            // The page URL carries the token; without it, nothing is served and nothing is
            // accepted. A refusal does not end the wait: the real browser may still arrive.
            respond_text(stream, 403, "Forbidden", "forbidden\n")?;
            return Ok(Outcome::KeepWaiting);
        }
        match (method.as_str(), path) {
            ("GET", "/") => {
                respond(stream, 200, "OK", "text/html; charset=utf-8", &self.page())?;
                Ok(Outcome::KeepWaiting)
            }
            ("POST", "/result") => {
                if self.served {
                    respond_text(stream, 409, "Conflict", "already answered\n")?;
                    return Ok(Outcome::KeepWaiting);
                }
                self.served = true;
                match transcript_from_body(&body) {
                    Ok(text) => {
                        respond_text(stream, 200, "OK", "ok\n")?;
                        Ok(Outcome::Text(text))
                    }
                    Err(reason) => {
                        // The page gets a 200 either way: it did its job by reporting. The
                        // failure is Vikram's to explain, and it is a typed one.
                        respond_text(stream, 200, "OK", "ok\n")?;
                        Ok(Outcome::NoText(reason))
                    }
                }
            }
            _ => {
                respond_text(stream, 404, "Not Found", "not found\n")?;
                Ok(Outcome::KeepWaiting)
            }
        }
    }
}

/// The real transport: bind, open the page in the desktop's browser, wait for the POST.
pub struct LocalBrowserTransport {
    opener: String,
}

impl LocalBrowserTransport {
    pub fn new() -> Self {
        Self {
            opener: OPENER.to_owned(),
        }
    }

    /// The same transport with the opener supplied: the seam the tests use to prove a missing
    /// opener is an honest error instead of a hang.
    #[cfg(test)]
    pub(crate) fn with_opener(opener: &str) -> Self {
        Self {
            opener: opener.to_owned(),
        }
    }
}

impl Default for LocalBrowserTransport {
    fn default() -> Self {
        Self::new()
    }
}

impl WebSpeechTransport for LocalBrowserTransport {
    fn transcribe(
        &self,
        request: &WebSpeechRequest,
        cancel: &CancelToken,
    ) -> Result<Option<String>> {
        cancel.check()?;
        let mut server = WebSpeechServer::bind(request)?;
        let opener = self.opener.trim();
        if which(opener).is_none() {
            return Err(VoiceError::WebSpeech(format!(
                "{opener} is not installed, so no browser can be opened; install it, or select \
                 the local provider"
            )));
        }
        // The URL holds the token, so it goes to the opener and nowhere else.
        let status = Command::new(opener)
            .arg(server.url())
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .map_err(|err| VoiceError::WebSpeech(format!("could not run {opener}: {err}")))?;
        if !status.success() {
            return Err(VoiceError::WebSpeech(format!(
                "{opener} could not open a browser ({status}); is a desktop session running?"
            )));
        }
        server.wait(cancel)
    }
}

/// The recognizer the session calls. It ignores the take: the browser records its own audio, and
/// sending the local capture as well would mean two microphones for one utterance.
pub struct WebSpeechRecognizer {
    transport: Box<dyn WebSpeechTransport>,
    request: WebSpeechRequest,
}

impl WebSpeechRecognizer {
    pub fn new(config: &VoiceConfig) -> Self {
        Self::with_parts(config, Box::new(LocalBrowserTransport::new()))
    }

    /// The same recognizer with the transport supplied by the caller: the seam the tests use.
    pub(crate) fn with_parts(config: &VoiceConfig, transport: Box<dyn WebSpeechTransport>) -> Self {
        Self {
            transport,
            request: WebSpeechRequest {
                language: config.stt_language.trim().to_owned(),
                timeout_ms: config.webspeech_timeout_ms,
            },
        }
    }

    /// The language the page will ask for.
    pub fn language(&self) -> &str {
        &self.request.language
    }

    /// How long the bridge waits for the browser.
    pub fn timeout_ms(&self) -> u64 {
        self.request.timeout_ms
    }
}

impl Recognizer for WebSpeechRecognizer {
    fn transcribe(&mut self, pcm: &[i16]) -> Result<Option<String>> {
        self.transcribe_cancellable(pcm, &CancelToken::new())
    }

    fn transcribe_cancellable(
        &mut self,
        _pcm: &[i16],
        cancel: &CancelToken,
    ) -> Result<Option<String>> {
        cancel.check()?;
        // `_pcm` is accepted so the trait shape is the same for every engine. It is never sent:
        // the page captures the microphone itself.
        self.transport.transcribe(&self.request, cancel)
    }

    fn captures_audio(&self) -> bool {
        false
    }
}

/// What `voice status` says about the bridge. It cannot prove a Chrome-family browser with
/// Google speech enabled is installed — only a real turn can — so it reports the opener, which
/// is the part that is knowable without opening a window.
pub fn probe_webspeech(config: &VoiceConfig) -> StageStatus {
    probe_webspeech_with(config, OPENER)
}

fn probe_webspeech_with(config: &VoiceConfig, opener: &str) -> StageStatus {
    if which(opener).is_none() {
        return StageStatus::Degraded(format!(
            "webspeech needs {opener} to open a browser and it is not installed; the built-in \
             wavo transcribes instead"
        ));
    }
    let timeout = config.webspeech_timeout_ms.max(1);
    StageStatus::Ready(format!(
        "webspeech (free, keyless, uses your browser's Chrome speech engine; language {}, {}s \
         timeout)",
        config.stt_language.trim(),
        timeout.div_ceil(1000)
    ))
}

/// The one page. `__LANG__` and `__TOKEN__` are replaced with JSON string literals, so a
/// language tag with a quote cannot break the script.
const PAGE: &str = r#"<!doctype html>
<html lang="en">
<head>
<meta charset="utf-8">
<meta name="referrer" content="no-referrer">
<title>Vikram voice</title>
<style>
  body { font-family: system-ui, sans-serif; margin: 3rem; background: #101010; color: #e8e8e8; }
  h1 { font-size: 1rem; font-weight: 600; letter-spacing: .02em; color: #9ad; }
  #state { font-size: 1.5rem; margin: 1.2rem 0; }
  #text { white-space: pre-wrap; border-left: 2px solid #444; padding-left: .9rem; min-height: 2rem; color: #9f9; }
  #warn { color: #fc6; font-size: .9rem; }
  button { background: #1d1d1d; color: #e8e8e8; border: 1px solid #444; padding: .5rem 1rem; font: inherit; }
</style>
</head>
<body>
<h1>VIKRAM &mdash; browser speech</h1>
<p id="state">starting&hellip;</p>
<pre id="text"></pre>
<p id="warn"></p>
<button id="again" type="button">Start listening</button>
<script>
(function () {
  var LANG = __LANG__;
  var TOKEN = __TOKEN__;
  var state = document.getElementById('state');
  var text = document.getElementById('text');
  var warn = document.getElementById('warn');
  var again = document.getElementById('again');
  var finalText = '';
  var lastError = '';
  var answered = false;

  function send(payload) {
    if (answered) { return; }
    answered = true;
    fetch('/result?token=' + encodeURIComponent(TOKEN), {
      method: 'POST',
      headers: { 'content-type': 'application/json' },
      body: JSON.stringify(payload)
    }).catch(function () {
      state.textContent = 'Vikram is not listening any more.';
    });
  }

  var API = window.SpeechRecognition || window.webkitSpeechRecognition;
  if (!API) {
    state.textContent = 'This browser has no speech recognition.';
    warn.textContent = 'Use Chrome or Edge with Google speech enabled. Plain Chromium builds usually ship without it.';
    send({ text: '', error: 'the browser has no SpeechRecognition API' });
    return;
  }

  var rec = new API();
  rec.lang = LANG;
  rec.interimResults = true;
  rec.continuous = false;
  rec.maxAlternatives = 1;

  rec.onstart = function () {
    state.textContent = 'listening\u2026 speak, then pause';
  };
  rec.onresult = function (event) {
    var interim = '';
    for (var i = event.resultIndex; i < event.results.length; i++) {
      var result = event.results[i];
      if (result.isFinal) { finalText += result[0].transcript; }
      else { interim += result[0].transcript; }
    }
    text.textContent = (finalText + ' ' + interim).trim();
  };
  rec.onerror = function (event) {
    lastError = event.error || 'unknown error';
    state.textContent = 'speech error: ' + lastError;
  };
  rec.onend = function () {
    var words = finalText.trim();
    if (!words) { state.textContent = 'no speech heard'; }
    send({ text: words, error: words ? '' : (lastError || 'the browser heard no speech') });
  };

  again.addEventListener('click', function () { rec.start(); });
  try { rec.start(); } catch (err) { state.textContent = 'press Start listening'; }
})();
</script>
</body>
</html>
"#;

/// The JSON body the page posts, as one value.
#[derive(serde::Deserialize)]
struct BridgeResult {
    #[serde(default)]
    text: String,
    #[serde(default)]
    error: String,
}

/// The transcript, or the reason there is none. Empty text is a failure, not silence: the
/// operator asked for a transcript, and the session's local fallback is the honest answer.
fn transcript_from_body(body: &[u8]) -> std::result::Result<String, String> {
    let parsed: BridgeResult = serde_json::from_slice(body)
        .map_err(|err| format!("the browser sent unreadable JSON: {err}"))?;
    let text = parsed.text.trim().to_owned();
    if !text.is_empty() {
        return Ok(text);
    }
    let reason = parsed.error.trim();
    Err(if reason.is_empty() {
        "the browser heard no speech".to_owned()
    } else {
        format!("the browser reported: {reason}")
    })
}

/// Read one request: request line, headers, and the `Content-Length` body. Bounded in every
/// direction so a stray client cannot hold the turn.
fn read_request(stream: &mut TcpStream) -> io::Result<(String, String, Vec<u8>)> {
    stream.set_read_timeout(Some(READ_TIMEOUT))?;
    let mut buffer = Vec::new();
    let mut chunk = [0u8; 1024];
    let header_end = loop {
        if let Some(position) = buffer.windows(4).position(|window| window == b"\r\n\r\n") {
            break position;
        }
        if buffer.len() > MAX_HEADER_BYTES {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "request head too large",
            ));
        }
        let read = stream.read(&mut chunk)?;
        if read == 0 {
            return Err(io::Error::new(
                io::ErrorKind::UnexpectedEof,
                "client hung up before the request head",
            ));
        }
        buffer.extend_from_slice(&chunk[..read]);
    };
    let head = String::from_utf8_lossy(&buffer[..header_end]).into_owned();
    let mut lines = head.split("\r\n");
    let request_line = lines.next().unwrap_or_default();
    let mut parts = request_line.split_whitespace();
    let method = parts.next().unwrap_or_default().to_owned();
    let target = parts.next().unwrap_or_default().to_owned();
    let mut content_length = 0usize;
    for line in lines {
        let Some((name, value)) = line.split_once(':') else {
            continue;
        };
        if name.trim().eq_ignore_ascii_case("content-length") {
            content_length = value.trim().parse().unwrap_or(0);
        }
    }
    if content_length > MAX_BODY_BYTES {
        return Err(io::Error::new(io::ErrorKind::InvalidData, "body too large"));
    }
    let mut body = buffer[header_end + 4..].to_vec();
    while body.len() < content_length {
        let read = stream.read(&mut chunk)?;
        if read == 0 {
            break;
        }
        body.extend_from_slice(&chunk[..read]);
    }
    body.truncate(content_length);
    Ok((method, target, body))
}

/// One fixed response. `Connection: close` because one connection carries one request here.
fn respond(
    stream: &mut TcpStream,
    status: u16,
    reason: &str,
    content_type: &str,
    body: &str,
) -> io::Result<()> {
    let head = format!(
        "HTTP/1.1 {status} {reason}\r\nContent-Type: {content_type}\r\n\
         Content-Length: {}\r\nCache-Control: no-store\r\nReferrer-Policy: no-referrer\r\n\
         Connection: close\r\n\r\n",
        body.len()
    );
    stream.write_all(head.as_bytes())?;
    stream.write_all(body.as_bytes())?;
    stream.flush()
}

fn respond_text(stream: &mut TcpStream, status: u16, reason: &str, body: &str) -> io::Result<()> {
    respond(stream, status, reason, "text/plain; charset=utf-8", body)
}

/// The `name=value` pair from a query string. The token is hex, so no percent-decoding is needed.
fn query_param(query: &str, name: &str) -> Option<String> {
    query.split('&').find_map(|pair| {
        let (key, value) = pair.split_once('=')?;
        (key == name).then(|| value.to_owned())
    })
}

/// A JSON string literal, so a language tag is embedded safely in the page's script.
fn json_string(value: &str) -> String {
    serde_json::to_string(value).unwrap_or_else(|_| "\"\"".to_owned())
}

/// 128 bits of single-use token. `/dev/urandom` where it exists; otherwise a clock, pid and
/// address mix, which is still unique per turn and only ever reachable over loopback.
fn random_token() -> String {
    let mut bytes = [0u8; 16];
    if let Ok(mut urandom) = std::fs::File::open("/dev/urandom")
        && urandom.read_exact(&mut bytes).is_ok()
    {
        return hex(&bytes);
    }
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|elapsed| elapsed.as_nanos() as u64)
        .unwrap_or(0);
    let mut state =
        nanos ^ ((std::process::id() as u64) << 32) ^ (&bytes as *const _ as usize as u64);
    for chunk in bytes.chunks_mut(8) {
        state ^= state << 13;
        state ^= state >> 7;
        state ^= state << 17;
        let word = state.to_le_bytes();
        chunk.copy_from_slice(&word[..chunk.len()]);
    }
    hex(&bytes)
}

fn hex(bytes: &[u8]) -> String {
    use std::fmt::Write as _;
    let mut out = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        let _ = write!(out, "{byte:02x}");
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Arc, Mutex};

    fn request(language: &str, timeout_ms: u64) -> WebSpeechRequest {
        WebSpeechRequest {
            language: language.into(),
            timeout_ms,
        }
    }

    /// Take the server side of a client that has already connected.
    fn accept_one(server: &WebSpeechServer) -> TcpStream {
        loop {
            match server.listener.as_ref().expect("still listening").accept() {
                Ok((stream, _)) => return stream,
                Err(err) if err.kind() == io::ErrorKind::WouldBlock => {
                    std::thread::sleep(Duration::from_millis(5));
                }
                Err(err) => panic!("accept: {err}"),
            }
        }
    }

    /// Connect, send the raw request, and let the server handle that one connection.
    fn drive(server: &mut WebSpeechServer, raw: &str) -> (Outcome, String) {
        let mut client = TcpStream::connect(server.addr()).expect("the bridge is listening");
        client
            .set_read_timeout(Some(Duration::from_secs(5)))
            .unwrap();
        let mut server_side = accept_one(server);
        client.write_all(raw.as_bytes()).unwrap();
        let outcome = server
            .handle_connection(&mut server_side)
            .expect("one request is read");
        // The response ends with the connection: drop the server side so the client sees EOF.
        drop(server_side);
        let mut response = String::new();
        let _ = client.read_to_string(&mut response);
        (outcome, response)
    }

    fn get(raw_token: &str) -> String {
        format!("GET /?token={raw_token} HTTP/1.1\r\nHost: 127.0.0.1\r\n\r\n")
    }

    fn post(raw_token: &str, body: &str) -> String {
        format!(
            "POST /result?token={raw_token} HTTP/1.1\r\nHost: 127.0.0.1\r\n\
             Content-Type: application/json\r\nContent-Length: {}\r\n\r\n{body}",
            body.len()
        )
    }

    #[test]
    fn the_page_carries_the_language_and_the_token_and_no_external_asset() {
        let server = WebSpeechServer::bind(&request("en-GB", DEFAULT_TIMEOUT_MS)).unwrap();
        let page = server.page();
        assert!(page.contains("en-GB"), "the language reaches the page");
        assert!(page.contains(server.token()), "the token reaches the page");
        assert!(
            page.contains("webkitSpeechRecognition"),
            "the page uses the browser's speech engine"
        );
        assert!(
            !page.contains("<script src")
                && !page.contains("http://")
                && !page.contains("https://"),
            "the page loads nothing external"
        );
    }

    #[test]
    fn a_language_with_a_quote_cannot_break_the_page_script() {
        let server = WebSpeechServer::bind(&request("en\"-x", 1000)).unwrap();
        let page = server.page();
        assert!(page.contains(r#""en\"-x""#), "the tag is a JSON literal");
    }

    #[test]
    fn a_wrong_token_is_refused_and_the_bridge_keeps_waiting() {
        let mut server = WebSpeechServer::bind(&request("en-US", DEFAULT_TIMEOUT_MS)).unwrap();
        let token = server.token().to_owned();

        let (_, page) = drive(&mut server, &get("wrong"));
        assert!(page.starts_with("HTTP/1.1 403"), "{page}");
        assert!(!page.contains(&token), "a refusal leaks nothing");

        let (outcome, response) = drive(&mut server, &post("wrong", r#"{"text":"not this"}"#));
        assert!(
            matches!(outcome, Outcome::KeepWaiting),
            "a wrong token ends nothing"
        );
        assert!(response.starts_with("HTTP/1.1 403"), "{response}");

        // The right token still works after the refusals.
        let (outcome, response) = drive(&mut server, &post(&token, r#"{"text":"the real words"}"#));
        assert!(response.starts_with("HTTP/1.1 200"), "{response}");
        match outcome {
            Outcome::Text(text) => assert_eq!(text, "the real words"),
            _ => panic!("the correct token must be accepted after a refusal"),
        }
    }

    #[test]
    fn a_second_post_is_refused() {
        let mut server = WebSpeechServer::bind(&request("en-US", DEFAULT_TIMEOUT_MS)).unwrap();
        let token = server.token().to_owned();

        match drive(&mut server, &post(&token, r#"{"text":"first"}"#)).0 {
            Outcome::Text(text) => assert_eq!(text, "first"),
            _ => panic!("the first post is accepted"),
        }
        let (outcome, response) = drive(&mut server, &post(&token, r#"{"text":"second"}"#));
        assert!(
            matches!(outcome, Outcome::KeepWaiting),
            "the second post must not replace the first"
        );
        assert!(response.starts_with("HTTP/1.1 409"), "{response}");
    }

    #[test]
    fn an_empty_transcript_is_a_reported_failure_not_silence() {
        let mut server = WebSpeechServer::bind(&request("en-US", DEFAULT_TIMEOUT_MS)).unwrap();
        let token = server.token().to_owned();
        let body = r#"{"text":"","error":"the browser has no SpeechRecognition API"}"#;
        match drive(&mut server, &post(&token, body)).0 {
            Outcome::NoText(reason) => assert!(reason.contains("SpeechRecognition"), "{reason}"),
            _ => panic!("empty text is a failure the session can fall back from"),
        }
    }

    #[test]
    fn an_unreadable_body_is_a_failure() {
        let mut server = WebSpeechServer::bind(&request("en-US", DEFAULT_TIMEOUT_MS)).unwrap();
        let token = server.token().to_owned();
        match drive(&mut server, &post(&token, "not json")).0 {
            Outcome::NoText(reason) => assert!(reason.contains("unreadable JSON"), "{reason}"),
            _ => panic!("a malformed body must not look like a transcript"),
        }
    }

    #[test]
    fn a_timeout_is_reported_and_closes_the_listener() {
        let mut server = WebSpeechServer::bind(&request("en-US", 60)).unwrap();
        let addr = server.addr();
        let err = server.wait(&CancelToken::new()).unwrap_err();
        assert!(matches!(err, VoiceError::Timeout(1)), "{err}");
        assert!(
            TcpStream::connect(addr).is_err(),
            "the listener is closed when the wait ends"
        );
    }

    #[test]
    fn a_cancel_closes_the_listener() {
        let mut server = WebSpeechServer::bind(&request("en-US", DEFAULT_TIMEOUT_MS)).unwrap();
        let addr = server.addr();
        let cancel = CancelToken::new();
        let twin = cancel.clone();
        let waiter = std::thread::spawn(move || server.wait(&twin));
        std::thread::sleep(Duration::from_millis(50));
        cancel.cancel();
        assert!(matches!(waiter.join().unwrap(), Err(VoiceError::Cancelled)));
        assert!(
            TcpStream::connect(addr).is_err(),
            "a cancelled bridge must not leave a socket open"
        );
    }

    #[test]
    fn the_wait_returns_the_posted_transcript() {
        let mut server = WebSpeechServer::bind(&request("en-US", 5_000)).unwrap();
        let addr = server.addr();
        let token = server.token().to_owned();
        let cancel = CancelToken::new();
        let waiter = std::thread::spawn(move || server.wait(&cancel));

        // `wait` handles the connection, so the client just speaks HTTP to it.
        let mut stream = TcpStream::connect(addr).unwrap();
        stream
            .set_read_timeout(Some(Duration::from_secs(5)))
            .unwrap();
        stream
            .write_all(post(&token, r#"{"text":"hello from the browser"}"#).as_bytes())
            .unwrap();
        let mut response = String::new();
        let _ = stream.read_to_string(&mut response);
        assert!(response.starts_with("HTTP/1.1 200"), "{response}");
        assert_eq!(
            waiter.join().unwrap().unwrap().as_deref(),
            Some("hello from the browser")
        );
    }

    /// A transport that answers without a browser, and records what it was asked.
    struct FakeBridge {
        reply: std::result::Result<Option<String>, String>,
        calls: Arc<Mutex<Vec<WebSpeechRequest>>>,
    }

    impl FakeBridge {
        fn answering(text: &str, calls: &Arc<Mutex<Vec<WebSpeechRequest>>>) -> Self {
            Self {
                reply: Ok(Some(text.into())),
                calls: calls.clone(),
            }
        }
    }

    impl WebSpeechTransport for FakeBridge {
        fn transcribe(
            &self,
            request: &WebSpeechRequest,
            cancel: &CancelToken,
        ) -> Result<Option<String>> {
            cancel.check()?;
            self.calls.lock().unwrap().push(request.clone());
            match &self.reply {
                Ok(text) => Ok(text.clone()),
                Err(reason) => Err(VoiceError::WebSpeech(reason.clone())),
            }
        }
    }

    fn config() -> VoiceConfig {
        VoiceConfig {
            stt_provider: "webspeech".into(),
            stt_language: "en-GB".into(),
            webspeech_timeout_ms: 12_345,
            ..Default::default()
        }
    }

    #[test]
    fn the_recognizer_asks_for_the_configured_language_and_timeout() {
        let calls = Arc::new(Mutex::new(Vec::new()));
        let mut recognizer = WebSpeechRecognizer::with_parts(
            &config(),
            Box::new(FakeBridge::answering("hello", &calls)),
        );
        assert_eq!(recognizer.language(), "en-GB");
        assert_eq!(recognizer.timeout_ms(), 12_345);
        assert_eq!(
            recognizer.transcribe(&[1, 2, 3]).unwrap().as_deref(),
            Some("hello"),
            "the take is not sent anywhere; the transport's words are the transcript"
        );
        let sent = calls.lock().unwrap();
        assert_eq!(sent.len(), 1);
        assert_eq!(sent[0].language, "en-GB");
        assert_eq!(sent[0].timeout_ms, 12_345);
    }

    #[test]
    fn a_bridge_failure_is_typed_and_a_cancel_is_not_a_failure() {
        let calls = Arc::new(Mutex::new(Vec::new()));
        let mut recognizer = WebSpeechRecognizer::with_parts(
            &config(),
            Box::new(FakeBridge {
                reply: Err("no browser".into()),
                calls: calls.clone(),
            }),
        );
        match recognizer.transcribe(&[]).unwrap_err() {
            VoiceError::WebSpeech(reason) => assert!(reason.contains("no browser"), "{reason}"),
            other => panic!("expected a bridge error, got {other:?}"),
        }

        let cancel = CancelToken::new();
        cancel.cancel();
        assert!(matches!(
            recognizer.transcribe_cancellable(&[], &cancel),
            Err(VoiceError::Cancelled)
        ));
        assert_eq!(calls.lock().unwrap().len(), 1, "a cancel opens no bridge");
    }

    #[test]
    fn a_missing_opener_is_reported_honestly() {
        let transport = LocalBrowserTransport::with_opener("vikram-no-such-opener");
        let err = transport
            .transcribe(&request("en-US", 1000), &CancelToken::new())
            .unwrap_err();
        let text = err.to_string();
        assert!(text.contains("vikram-no-such-opener"), "{text}");
        assert!(text.contains("not installed"), "{text}");
    }

    #[test]
    fn status_reports_ready_with_the_opener_and_degraded_without_it() {
        let label = probe_webspeech_with(&config(), OPENER).label();
        assert!(
            label.contains("webspeech")
                && (label.contains("free, keyless") || label.contains(OPENER)),
            "{label}"
        );

        let label = probe_webspeech_with(&config(), "vikram-no-such-opener").label();
        assert!(label.starts_with("degraded:"), "{label}");
        assert!(label.contains("vikram-no-such-opener"), "{label}");
        assert!(label.contains("built-in wavo"), "{label}");
    }

    #[test]
    fn the_token_is_hex_and_unique_per_bridge() {
        let first = WebSpeechServer::bind(&request("en-US", 1000)).unwrap();
        let second = WebSpeechServer::bind(&request("en-US", 1000)).unwrap();
        assert_eq!(first.token().len(), 32);
        assert!(first.token().chars().all(|c| c.is_ascii_hexdigit()));
        assert_ne!(first.token(), second.token());
        assert_ne!(first.addr(), second.addr(), "a random port per turn");
        assert!(first.url().contains("127.0.0.1"));
    }
}
