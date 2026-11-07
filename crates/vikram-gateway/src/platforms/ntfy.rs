//! ntfy adapter — publish over HTTP (`POST {server}/{topic}`), subscribe by polling
//! (`GET {server}/{topic}/json?poll=1`).
//!
//! Rust port of hermes-agent `plugins/platforms/ntfy/adapter.py`, narrowed to what fits the frozen
//! [`Transport`](crate::transport::Transport) surface: a server probe, the publish POST, and
//! one-shot JSON polling. Everything rides on [`HttpRequest`], so the whole wire behaviour is
//! testable with no network, no token and no ntfy account.
//!
//! Ported behaviour, with the hermes origin:
//! - `connect`: `GET {server}/v1/health`; when that endpoint 404s (self-hosted servers older than
//!   the health route) it falls back to the subscription URL the adapter would use anyway and
//!   accepts a 200 there.
//! - Publish: `POST {server}/{topic}` with the message as raw `text/plain; charset=utf-8` bytes,
//!   the optional `Title` / `Priority` / `Tags` / `Markdown` headers, and
//!   `Authorization: Bearer <token>` when a token is configured. The reply's `{"id": "..."}`
//!   becomes [`SendResult::message_id`].
//! - The 4096-character budget (`MAX_MESSAGE_LENGTH`) enforced through `truncate_message`; chunks
//!   are published sequentially and every id is reported in send order (`SendResult::ok_split`).
//! - Send failure taxonomy: `429` → `rate_limited` (retryable, with the server's `Retry-After`),
//!   `401`/`403` → `forbidden`, `404` → `not_found`, `400` → `bad_format`, `5xx` and transport
//!   failures → `transient` (retryable).
//! - Inbound: [`NtfyAdapter::poll_once`] reads the newline-delimited JSON `/json` feed and turns
//!   each `event == "message"` line into a [`MessageEvent`].
//!
//! Deliberately not ported (they need state this stateless wire adapter does not own — see
//! `docs/gateway-port-plan.md`): the long-lived `poll=false` streaming subscription with its
//! reconnect backoff (`RECONNECT_BACKOFF`), the fatal-401/404 latch, the message deduplicator, the
//! `hermes-agent` echo tag that suppresses the adapter's own publishes from its own feed, the
//! `publish_topic` override, the `user:pass` → Basic auth form, the markdown config/env flags, the
//! allowlist/`NTFY_*` env seeding, and the out-of-process `_standalone_send` cron path.
//! `poll_once` is the runner-callable substitute for the stream; a caller that wants dedup keeps
//! the ids.
//!
//! ntfy counts its 4096 limit in **bytes** while this port chunks by code points (hermes'
//! `len(message)`): ASCII payloads are identical, a chunk of multi-byte text can exceed the wire
//! cap and is then refused by the server as `bad_format`.

use async_trait::async_trait;
use serde_json::{Map, Value};

use crate::event::{MessageEvent, Platform, SessionSource};
use crate::helpers::{codepoint_len, truncate_message};
use crate::platform::{ChatInfo, PlatformAdapter, SendResult};
use crate::transport::{HttpRequest, HttpResponse, SharedTransport, TransportError};

/// ntfy's per-message body limit, in characters (hermes `MAX_MESSAGE_LENGTH`).
pub const NTFY_MAX_MESSAGE_LENGTH: usize = 4096;

/// Server used when no server is configured (hermes `DEFAULT_SERVER`).
const DEFAULT_SERVER: &str = "https://ntfy.sh";

/// Health route every current ntfy server serves; older self-hosted builds 404 it.
const HEALTH_PATH: &str = "/v1/health";

/// The only `/json` event kind that carries an inbound message.
const EVENT_MESSAGE: &str = "message";

/// The gateway platform name for this adapter. `event::platform_names` predates the ntfy plugin
/// (that module is frozen), so the name lives here.
const PLATFORM_NAME: &str = "ntfy";

/// ntfy topics are broadcast channels: there is no DM shape in the wire protocol.
const CHAT_TYPE_CHANNEL: &str = "channel";

/// ntfy publishes no authenticated author identity; the publisher-controlled `title` must not be
/// mistaken for one, so every inbound event reports this fixed id (hermes uses the topic name, this
/// port the service name, and keeps the topic on `source.chat_id`).
const NTFY_USER_ID: &str = "ntfy";

/// `since=all` — replay the topic's retained messages instead of starting from now.
const POLL_SINCE_ALL: &str = "all";

/// Publish body type: ntfy treats the body as plain text unless a `Markdown` header says otherwise.
const CONTENT_TYPE: &str = "text/plain; charset=utf-8";

/// Characters of an error body kept in a [`SendResult::error`] / poll error message.
const BODY_EXCERPT_CHARS: usize = 200;

/// ntfy platform adapter: HTTP publish out, JSON poll in.
///
/// Every method is stateless — `connect` only probes the server, so a failed `connect` can be
/// retried on the same instance and a poll cursor lives with the caller.
pub struct NtfyAdapter {
    transport: SharedTransport,
    server: String,
    topic: String,
    token: Option<String>,
}

impl NtfyAdapter {
    /// Build an adapter over `transport` for `topic` on `server`.
    ///
    /// An empty `server` selects `https://ntfy.sh` and a trailing `/` is stripped so endpoints
    /// never double up, matching hermes' `_server_url`. `token` is an ntfy access token, sent as
    /// `Authorization: Bearer <token>`; `None` (or a blank token) sends no credentials at all.
    /// Nothing is sent anywhere until a method is called.
    pub fn new(
        transport: SharedTransport,
        server: impl Into<String>,
        topic: impl Into<String>,
        token: Option<String>,
    ) -> Self {
        let server = server.into();
        let trimmed = server.trim();
        let server = if trimmed.is_empty() {
            DEFAULT_SERVER
        } else {
            trimmed
        };
        Self {
            transport,
            server: server.trim_end_matches('/').to_string(),
            topic: topic.into().trim().to_string(),
            token,
        }
    }

    /// Absolute URL of a topic on this server (`{server}/{topic}`).
    fn topic_url(&self, topic: &str) -> String {
        format!("{}/{topic}", self.server)
    }

    /// The poll URL for `since`: `{server}/{topic}/json?poll=1&since=<since>`.
    fn poll_url(&self, since: &str) -> String {
        format!("{}/json?poll=1&since={since}", self.topic_url(&self.topic))
    }

    /// Add `Authorization: Bearer <token>` when a usable token is configured.
    fn authorized(&self, request: HttpRequest) -> HttpRequest {
        match self.token.as_deref().map(str::trim) {
            Some(token) if !token.is_empty() => request.bearer(token),
            _ => request,
        }
    }

    /// The publish request for one already-sized `chunk`.
    ///
    /// `Content-Type` plus the ntfy publish headers ntfy accepts without the legacy `X-` prefix:
    /// `Title`, `Priority` (1–5 or a name), `Tags` (comma-separated) and `Markdown: yes`. Absent or
    /// unusable metadata keys are omitted rather than sent empty.
    fn publish_request(
        &self,
        topic: &str,
        chunk: &str,
        metadata: Option<&Map<String, Value>>,
    ) -> HttpRequest {
        let mut request = HttpRequest::post(self.topic_url(topic))
            .header("Content-Type", CONTENT_TYPE)
            .body(chunk.as_bytes().to_vec());
        if let Some(metadata) = metadata {
            if let Some(title) = non_empty_str(metadata.get("title")) {
                request = request.header("Title", title);
            }
            if let Some(priority) = priority_header(metadata.get("priority")) {
                request = request.header("Priority", priority);
            }
            if let Some(tags) = tags_header(metadata.get("tags")) {
                request = request.header("Tags", tags);
            }
            if metadata.get("markdown").and_then(Value::as_bool) == Some(true) {
                request = request.header("Markdown", "yes");
            }
        }
        self.authorized(request)
    }

    /// Publish one chunk and report its ntfy message id, when the reply carried one.
    ///
    /// A non-2xx response (or a transport failure) comes back as the classified [`SendResult`] the
    /// caller returns verbatim — boxed so the success path stays small.
    async fn publish(
        &self,
        topic: &str,
        chunk: &str,
        metadata: Option<&Map<String, Value>>,
    ) -> Result<Option<String>, Box<SendResult>> {
        let request = self.publish_request(topic, chunk, metadata);
        match self.transport.execute(request).await {
            Ok(response) if response.ok() => {
                Ok(response.json().ok().as_ref().and_then(message_id_from))
            }
            Ok(response) => Err(Box::new(classify_failure(&response))),
            Err(err) => Err(Box::new(transient_failure(&err))),
        }
    }

    /// Read the topic's JSON feed once and normalize every message event in it.
    ///
    /// `since` is ntfy's cursor — `all`, a duration (`30m`), a unix timestamp or a message id; an
    /// absent/blank cursor asks for `since=all`, which is also what `connect`'s fallback probes.
    /// `/json?poll=1` answers a body of newline-delimited JSON objects: `open`, `keepalive` and
    /// `poll_request` frames carry no message and are skipped, as are lines that are not JSON.
    ///
    /// Each `event == "message"` line becomes a [`MessageEvent`] whose `source.chat_id` is the
    /// event's `topic` (falling back to the configured topic), `source.chat_type` is `"channel"`,
    /// `text` is the body prefixed with `"<title>: "` when a title is present, `message_id` the
    /// ntfy id, `platform_update_id` the event `time` (unix seconds), and `raw_message` the parsed
    /// line. `priority` and `tags` land in `MessageEvent::metadata`. Transport errors and non-2xx
    /// statuses are reported as `Err` so a caller can back off.
    pub async fn poll_once(&self, since: Option<&str>) -> Result<Vec<MessageEvent>, String> {
        let since = match since.map(str::trim) {
            Some(value) if !value.is_empty() => value,
            _ => POLL_SINCE_ALL,
        };
        let request = self.authorized(HttpRequest::get(self.poll_url(since)));
        let response = self
            .transport
            .execute(request)
            .await
            .map_err(|err| format!("ntfy poll transport error: {err}"))?;
        if !response.ok() {
            return Err(format!(
                "ntfy poll failed with status {}: {}",
                response.status,
                body_excerpt(&response.body)
            ));
        }
        Ok(parse_events(&response.body, &self.topic))
    }
}

#[async_trait]
impl PlatformAdapter for NtfyAdapter {
    /// Probe the server: `GET {server}/v1/health`, `true` on 200.
    ///
    /// A 404 means an ntfy build without the health route, so the adapter falls back to the
    /// subscription URL (`{server}/{topic}/json?poll=1&since=all`) and accepts a 200 there — the
    /// same request [`NtfyAdapter::poll_once`] will make. Any other status, or a transport failure,
    /// is `false`. `is_reconnect` needs no different handling: the adapter holds no connection.
    async fn connect(&mut self, _is_reconnect: bool) -> bool {
        let request = self.authorized(HttpRequest::get(format!("{}{HEALTH_PATH}", self.server)));
        match self.transport.execute(request).await {
            Ok(response) if response.status == 200 => true,
            Ok(response) if response.status == 404 => {
                let probe = self.authorized(HttpRequest::get(self.poll_url(POLL_SINCE_ALL)));
                match self.transport.execute(probe).await {
                    Ok(probe) => probe.status == 200,
                    Err(_) => false,
                }
            }
            _ => false,
        }
    }

    /// Nothing to tear down: the adapter holds a transport handle, not a connection.
    async fn disconnect(&mut self) {}

    /// Publish `content` to `chat_id` (a topic name), falling back to the configured topic.
    ///
    /// `reply_to` is accepted for the trait contract and ignored: ntfy has no reply/thread
    /// primitive, so an anchor cannot be expressed. `metadata` may carry `title`, `priority`,
    /// `tags` and `markdown`, which become publish headers.
    ///
    /// The body is chunked to [`NTFY_MAX_MESSAGE_LENGTH`] and published sequentially; the ids of
    /// everything that landed are reported (`message_id` is the last one) so the delivery ledger
    /// resends only the tail after a mid-way failure. Blank content is refused as `bad_format`
    /// without a request — ntfy answers an empty body with a 400.
    async fn send(
        &self,
        chat_id: &str,
        content: &str,
        _reply_to: Option<&str>,
        metadata: Option<Value>,
    ) -> SendResult {
        let destination = {
            let trimmed = chat_id.trim();
            if trimmed.is_empty() {
                self.topic.as_str()
            } else {
                trimmed
            }
        };
        if destination.is_empty() {
            return SendResult::failed(
                "ntfy send has no topic: pass a chat id or configure a topic",
                Some("bad_format"),
            );
        }
        if content.trim().is_empty() {
            return SendResult::failed("ntfy send rejected blank content", Some("bad_format"));
        }
        let chunks = truncate_message(content, NTFY_MAX_MESSAGE_LENGTH, codepoint_len);
        if chunks.is_empty() {
            return SendResult::failed(
                "ntfy send produced no chunks for the content",
                Some("bad_format"),
            );
        }
        let metadata = metadata.as_ref().and_then(Value::as_object);
        let total = chunks.len();
        let mut delivered: Vec<String> = Vec::with_capacity(total);
        for chunk in &chunks {
            match self.publish(destination, chunk, metadata).await {
                Ok(Some(id)) => delivered.push(id),
                Ok(None) => {}
                Err(failure) => return mark_partial(*failure, &delivered, total),
            }
        }
        match delivered.last() {
            Some(last) => SendResult::ok_split(last.clone(), delivered),
            None => missing_id_result(),
        }
    }

    /// `{name: chat_id, chat_type: "channel"}`.
    ///
    /// ntfy exposes no topic-metadata endpoint (a topic exists as soon as a message is published to
    /// it and carries no name, description or member list), so there is nothing to look up: the
    /// topic name is the only identity there is. Topics are broadcast channels — there is no DM.
    async fn get_chat_info(&self, chat_id: &str) -> ChatInfo {
        ChatInfo {
            name: chat_id.to_string(),
            chat_type: CHAT_TYPE_CHANNEL.to_string(),
        }
    }

    /// ntfy has no typing indicator (a publish would be a visible message), so this is a no-op.
    async fn send_typing(&self, _chat_id: &str, _metadata: Option<Value>) {}

    /// `send` chunks against the ntfy cap; the delivery router must not truncate first.
    fn splits_long_messages(&self) -> bool {
        true
    }

    /// The ntfy cap the chunker measures against; length is counted in code points (the default).
    fn max_message_length(&self) -> usize {
        NTFY_MAX_MESSAGE_LENGTH
    }
}

/// The gateway platform handle for this adapter.
///
/// `Platform::new` only rejects empty names and [`PLATFORM_NAME`] cannot be one, so the fallback
/// keeps the function total instead of unwrapping.
fn ntfy_platform() -> Platform {
    Platform::new(PLATFORM_NAME).unwrap_or_else(Platform::local)
}

/// Parse a `/json` poll body: one newline-delimited JSON object per line.
fn parse_events(body: &str, fallback_topic: &str) -> Vec<MessageEvent> {
    body.lines()
        .filter_map(|line| parse_event(line, fallback_topic))
        .collect()
}

/// Turn one `/json` line into an event, or `None` when it is not an inbound message.
///
/// hermes streams the same feed and dispatches only `event == "message"` lines, skipping blank
/// lines and JSON decode errors; this port does the same and additionally drops a message that
/// carries no text at all (hermes `if not text: return`), because an empty turn is not actionable.
fn parse_event(line: &str, fallback_topic: &str) -> Option<MessageEvent> {
    let line = line.trim();
    if line.is_empty() {
        return None;
    }
    let payload: Value = serde_json::from_str(line).ok()?;
    if payload.get("event").and_then(Value::as_str) != Some(EVENT_MESSAGE) {
        return None;
    }
    let topic = payload
        .get("topic")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|topic| !topic.is_empty())
        .unwrap_or(fallback_topic);
    if topic.is_empty() {
        return None;
    }
    let message = payload
        .get("message")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .trim();
    let title = payload
        .get("title")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|title| !title.is_empty());
    let text = match title {
        Some(title) if message.is_empty() => title.to_string(),
        Some(title) => format!("{title}: {message}"),
        None => message.to_string(),
    };
    if text.is_empty() {
        return None;
    }

    let message_id = payload.get("id").and_then(value_to_id);
    let mut source = SessionSource::new(ntfy_platform(), topic.to_string());
    source.chat_name = Some(topic.to_string());
    source.chat_type = CHAT_TYPE_CHANNEL.to_string();
    source.user_id = Some(NTFY_USER_ID.to_string());
    source.message_id = message_id.clone();

    let mut event = MessageEvent::new(text, source);
    event.user_id = Some(NTFY_USER_ID.to_string());
    event.message_id = message_id;
    event.platform_update_id = payload.get("time").and_then(Value::as_i64);
    event.raw_message = Some(payload.clone());
    for key in ["priority", "tags"] {
        if let Some(value) = payload.get(key).filter(|value| !value.is_null()) {
            event.metadata.insert(key.to_string(), value.clone());
        }
    }
    Some(event)
}

/// An ntfy id as a string: ids arrive as strings, but a numeric id (fixtures, proxies) is accepted.
fn value_to_id(value: &Value) -> Option<String> {
    match value {
        Value::String(text) => {
            let trimmed = text.trim();
            if trimmed.is_empty() {
                None
            } else {
                Some(trimmed.to_string())
            }
        }
        Value::Number(number) => Some(number.to_string()),
        _ => None,
    }
}

/// The publish response's `id`, when it has a usable one (`{"id": "AbC123"}`).
fn message_id_from(body: &Value) -> Option<String> {
    body.get("id").and_then(value_to_id)
}

/// A metadata string value, trimmed; `None` when absent or blank.
fn non_empty_str(value: Option<&Value>) -> Option<&str> {
    value
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|text| !text.is_empty())
}

/// The ntfy `Priority` header: an integer (1–5) or a name (`min`/`low`/`default`/`high`/`urgent`).
fn priority_header(value: Option<&Value>) -> Option<String> {
    match value? {
        Value::Number(number) => Some(number.to_string()),
        Value::String(text) => {
            let trimmed = text.trim();
            if trimmed.is_empty() {
                None
            } else {
                Some(trimmed.to_string())
            }
        }
        _ => None,
    }
}

/// The ntfy `Tags` header: a comma-separated list, from either an array or a pre-joined string.
fn tags_header(value: Option<&Value>) -> Option<String> {
    match value? {
        Value::Array(items) => {
            let joined = items
                .iter()
                .filter_map(|item| item.as_str().map(str::trim).filter(|tag| !tag.is_empty()))
                .collect::<Vec<&str>>()
                .join(",");
            if joined.is_empty() {
                None
            } else {
                Some(joined)
            }
        }
        Value::String(text) => {
            let trimmed = text.trim();
            if trimmed.is_empty() {
                None
            } else {
                Some(trimmed.to_string())
            }
        }
        _ => None,
    }
}

/// A short single-line excerpt of a response body for error text.
fn body_excerpt(body: &str) -> String {
    let text = body.trim();
    if text.is_empty() {
        return "<empty body>".to_string();
    }
    let mut excerpt: String = text.chars().take(BODY_EXCERPT_CHARS).collect();
    if text.chars().count() > BODY_EXCERPT_CHARS {
        excerpt.push('…');
    }
    excerpt
}

/// Map a non-2xx ntfy response onto a classified [`SendResult`].
///
/// `429` → `rate_limited` (retryable, with the server's `Retry-After`), `401`/`403` → `forbidden`,
/// `404` → `not_found`, any other 4xx → `bad_format`, `5xx` (and anything else) → `transient`,
/// retryable.
fn classify_failure(response: &HttpResponse) -> SendResult {
    let message = format!(
        "ntfy api returned {}: {}",
        response.status,
        body_excerpt(&response.body)
    );
    match response.status {
        429 => SendResult::failed(message, Some("rate_limited")).retryable(response.retry_after),
        401 | 403 => SendResult::failed(message, Some("forbidden")),
        404 => SendResult::failed(message, Some("not_found")),
        400..=499 => SendResult::failed(message, Some("bad_format")),
        _ => SendResult::failed(message, Some("transient")).retryable(None),
    }
}

/// Classify a transport-level failure: no response arrived, so the send is worth retrying
/// (`transient`).
fn transient_failure(err: &TransportError) -> SendResult {
    SendResult::failed(format!("ntfy transport error: {err}"), Some("transient")).retryable(None)
}

/// Attach the ids of an already-published head to a failure so a retry sends only the tail
/// (hermes `_with_partial_send`).
fn mark_partial(mut result: SendResult, delivered: &[String], total: usize) -> SendResult {
    if delivered.is_empty() {
        return result;
    }
    result = result.with_partial_overflow(delivered.len() as u32, total as u32);
    result.continuation_message_ids = delivered.to_vec();
    result
}

/// A successful publish whose response carried no id: still a success, with no `message_id` to
/// invent (hermes substitutes a random uuid, which this dependency-free port will not fake).
fn missing_id_result() -> SendResult {
    let mut result = SendResult::ok(String::new());
    result.message_id = None;
    result
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::transport::{HttpMethod, Transport};
    use serde_json::json;
    use std::collections::VecDeque;
    use std::sync::{Arc, Mutex, MutexGuard};

    /// Records every request and replays scripted responses in order.
    struct MockTransport {
        requests: Mutex<Vec<HttpRequest>>,
        responses: Mutex<VecDeque<Result<HttpResponse, TransportError>>>,
    }

    impl MockTransport {
        fn new(responses: Vec<Result<HttpResponse, TransportError>>) -> Arc<Self> {
            Arc::new(Self {
                requests: Mutex::new(Vec::new()),
                responses: Mutex::new(responses.into()),
            })
        }

        fn requests(&self) -> Vec<HttpRequest> {
            lock(&self.requests).clone()
        }
    }

    #[async_trait]
    impl Transport for MockTransport {
        async fn execute(&self, request: HttpRequest) -> Result<HttpResponse, TransportError> {
            lock(&self.requests).push(request);
            match lock(&self.responses).pop_front() {
                Some(response) => response,
                None => Err(TransportError::Request(
                    "mock transport ran out of scripted responses".to_string(),
                )),
            }
        }
    }

    /// Poisoned-lock-free guard: a panic in one test must not cascade into an unreadable failure.
    fn lock<T>(guard: &Mutex<T>) -> MutexGuard<'_, T> {
        guard
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    /// The body of a publish request decoded as text (ntfy bodies are raw, never JSON).
    fn request_text(request: &HttpRequest) -> String {
        String::from_utf8_lossy(request.body.as_deref().unwrap_or_default()).into_owned()
    }

    /// A request header by case-insensitive name.
    fn header(request: &HttpRequest, name: &str) -> Option<String> {
        request
            .headers
            .iter()
            .find(|(key, _)| key.eq_ignore_ascii_case(name))
            .map(|(_, value)| value.clone())
    }

    fn response(status: u16, body: impl Into<String>) -> Result<HttpResponse, TransportError> {
        Ok(HttpResponse {
            status,
            body: body.into(),
            retry_after: None,
        })
    }

    fn rate_limited_response(
        status: u16,
        retry_after: f64,
    ) -> Result<HttpResponse, TransportError> {
        Ok(HttpResponse {
            status,
            body: "slow down".to_string(),
            retry_after: Some(retry_after),
        })
    }

    /// A 200 publish reply carrying an ntfy message id.
    fn published(id: &str) -> Result<HttpResponse, TransportError> {
        response(200, json!({ "id": id }).to_string())
    }

    fn adapter(mock: &Arc<MockTransport>) -> NtfyAdapter {
        NtfyAdapter::new(mock.clone(), DEFAULT_SERVER, "mytopic", None)
    }

    fn non_whitespace(text: &str) -> String {
        text.chars().filter(|ch| !ch.is_whitespace()).collect()
    }

    /// Content long enough to need more than one publish, built from `lines` sentences.
    fn long_content(lines: usize) -> String {
        let content: String = (0..lines)
            .map(|idx| format!("sentence {idx} with a few words to fill the message\n"))
            .collect();
        assert!(
            content.chars().count() > NTFY_MAX_MESSAGE_LENGTH,
            "test content must exceed one message"
        );
        content
    }

    #[tokio::test]
    async fn connect_accepts_a_healthy_server() {
        // An empty server argument must fall back to ntfy.sh.
        let mock = MockTransport::new(vec![response(200, r#"{"healthy":true}"#)]);
        let mut adapter = NtfyAdapter::new(mock.clone(), "   ", "mytopic", None);
        assert!(adapter.connect(false).await);

        let requests = mock.requests();
        assert_eq!(requests.len(), 1);
        assert_eq!(requests[0].method, HttpMethod::Get);
        assert_eq!(requests[0].url, "https://ntfy.sh/v1/health");
        // No token configured, so no credentials on the wire.
        assert!(header(&requests[0], "Authorization").is_none());
    }

    #[tokio::test]
    async fn connect_falls_back_to_polling_when_health_is_missing() {
        // A self-hosted server without the health route answers 404; the subscription URL is the
        // fallback probe.
        let mock = MockTransport::new(vec![
            response(404, "not found"),
            response(200, "{\"id\":\"kept\",\"event\":\"keepalive\"}\n"),
        ]);
        let mut adapter = NtfyAdapter::new(
            mock.clone(),
            "https://ntfy.example.test/",
            "mytopic",
            Some("tk_secret".to_string()),
        );
        assert!(adapter.connect(true).await);

        let requests = mock.requests();
        assert_eq!(requests.len(), 2);
        assert_eq!(
            requests[0].url, "https://ntfy.example.test/v1/health",
            "a trailing / must be stripped"
        );
        assert_eq!(
            requests[1].url,
            "https://ntfy.example.test/mytopic/json?poll=1&since=all"
        );
        assert_eq!(
            header(&requests[1], "Authorization").as_deref(),
            Some("Bearer tk_secret")
        );
    }

    #[tokio::test]
    async fn connect_fails_on_rejection_and_transport_error() {
        let rejected = MockTransport::new(vec![response(401, "unauthorized")]);
        assert!(!adapter(&rejected).connect(false).await);
        assert_eq!(
            rejected.requests().len(),
            1,
            "only a 404 may trigger the fallback probe"
        );

        let fallback_rejected =
            MockTransport::new(vec![response(404, "gone"), response(403, "no")]);
        assert!(!adapter(&fallback_rejected).connect(false).await);

        let down = MockTransport::new(vec![Err(TransportError::Request("dns".to_string()))]);
        assert!(!adapter(&down).connect(false).await);
    }

    #[tokio::test]
    async fn publish_posts_the_raw_body_to_the_topic() {
        let mock = MockTransport::new(vec![published("AbC123")]);
        let result = adapter(&mock)
            .send("mytopic", "hello world", None, None)
            .await;

        assert!(result.success);
        assert_eq!(result.message_id.as_deref(), Some("AbC123"));
        assert_eq!(result.error_kind, None);
        // One chunk: the id list holds that single id, and `message_id` is its last entry.
        assert_eq!(result.continuation_message_ids, vec!["AbC123".to_string()]);

        let requests = mock.requests();
        assert_eq!(requests.len(), 1);
        assert_eq!(requests[0].method, HttpMethod::Post);
        assert_eq!(requests[0].url, "https://ntfy.sh/mytopic");
        assert_eq!(request_text(&requests[0]), "hello world");
        assert_eq!(
            header(&requests[0], "Content-Type").as_deref(),
            Some("text/plain; charset=utf-8")
        );
        // No metadata and no token: no publish headers invented.
        assert!(header(&requests[0], "Title").is_none());
        assert!(header(&requests[0], "Priority").is_none());
        assert!(header(&requests[0], "Tags").is_none());
        assert!(header(&requests[0], "Markdown").is_none());
        assert!(header(&requests[0], "Authorization").is_none());
    }

    #[tokio::test]
    async fn publish_carries_the_bearer_token_when_configured() {
        let mock = MockTransport::new(vec![published("id-1")]);
        let adapter = NtfyAdapter::new(
            mock.clone(),
            DEFAULT_SERVER,
            "mytopic",
            Some("  tk_secret  ".to_string()),
        );
        assert!(adapter.send("mytopic", "hi", None, None).await.success);

        let requests = mock.requests();
        assert_eq!(
            header(&requests[0], "Authorization").as_deref(),
            Some("Bearer tk_secret"),
            "a pasted token's surrounding whitespace must not reach the header"
        );
    }

    #[tokio::test]
    async fn publish_maps_metadata_onto_ntfy_headers() {
        let mock = MockTransport::new(vec![published("id-2")]);
        let metadata = json!({
            "title": "Deploy finished",
            "priority": 4,
            "tags": ["rocket", "ci"],
            "markdown": true,
        });
        let result = adapter(&mock)
            .send("mytopic", "**done** in 12s", None, Some(metadata))
            .await;

        assert!(result.success);
        let request = &mock.requests()[0];
        assert_eq!(header(request, "Title").as_deref(), Some("Deploy finished"));
        assert_eq!(header(request, "Priority").as_deref(), Some("4"));
        assert_eq!(header(request, "Tags").as_deref(), Some("rocket,ci"));
        assert_eq!(header(request, "Markdown").as_deref(), Some("yes"));
        // The body stays the caller's markdown; only the header announces it.
        assert_eq!(request_text(request), "**done** in 12s");

        // A named priority, a pre-joined tag string, markdown off and a blank title: only the two
        // usable headers appear.
        let partial = MockTransport::new(vec![published("id-3")]);
        let metadata = json!({
            "title": "   ",
            "priority": "urgent",
            "tags": "a,b",
            "markdown": false,
        });
        assert!(
            adapter(&partial)
                .send("mytopic", "plain", None, Some(metadata))
                .await
                .success
        );
        let request = &partial.requests()[0];
        assert!(header(request, "Title").is_none());
        assert_eq!(header(request, "Priority").as_deref(), Some("urgent"));
        assert_eq!(header(request, "Tags").as_deref(), Some("a,b"));
        assert!(header(request, "Markdown").is_none());
    }

    #[tokio::test]
    async fn long_messages_are_chunked_sequentially() {
        let content = long_content(140);
        let mock = MockTransport::new((0..16).map(|idx| published(&format!("id{idx}"))).collect());
        let result = adapter(&mock).send("mytopic", &content, None, None).await;

        assert!(result.success);
        let requests = mock.requests();
        assert!(requests.len() > 1, "content should have been split");
        for request in &requests {
            let chunk = request_text(request);
            assert!(
                chunk.chars().count() <= NTFY_MAX_MESSAGE_LENGTH,
                "chunk of {} chars exceeds the cap: {chunk:?}",
                chunk.chars().count()
            );
        }
        // The last chunk's id is the message id; every id rides along in send order.
        let expected_ids: Vec<String> = (0..requests.len()).map(|idx| format!("id{idx}")).collect();
        assert_eq!(
            result.message_id.as_deref(),
            expected_ids.last().map(String::as_str)
        );
        assert_eq!(result.continuation_message_ids, expected_ids);
        // Chunking loses no visible content.
        let joined: String = requests.iter().map(request_text).collect();
        assert_eq!(non_whitespace(&joined), non_whitespace(&content));
    }

    #[tokio::test]
    async fn rate_limited_is_retryable_with_the_server_delay() {
        let mock = MockTransport::new(vec![rate_limited_response(429, 30.0)]);
        let result = adapter(&mock).send("mytopic", "hello", None, None).await;

        assert!(!result.success);
        assert_eq!(result.error_kind.as_deref(), Some("rate_limited"));
        assert!(result.retryable);
        assert_eq!(result.retry_after, Some(30.0));
        assert!(
            result.error.as_deref().unwrap_or_default().contains("429"),
            "{:?}",
            result.error
        );
    }

    #[tokio::test]
    async fn forbidden_and_missing_topics_are_classified() {
        let blocked = MockTransport::new(vec![response(403, "forbidden")]);
        let result = adapter(&blocked).send("mytopic", "hello", None, None).await;
        assert!(!result.success);
        assert_eq!(result.error_kind.as_deref(), Some("forbidden"));
        assert!(!result.retryable, "a refusal is not worth retrying");

        let unauthorized = MockTransport::new(vec![response(401, "unauthorized")]);
        let result = adapter(&unauthorized)
            .send("mytopic", "hello", None, None)
            .await;
        assert_eq!(result.error_kind.as_deref(), Some("forbidden"));

        let missing = MockTransport::new(vec![response(404, "topic not found")]);
        let result = adapter(&missing).send("mytopic", "hello", None, None).await;
        assert_eq!(result.error_kind.as_deref(), Some("not_found"));
    }

    #[tokio::test]
    async fn bad_requests_are_bad_format() {
        let mock = MockTransport::new(vec![response(400, "invalid request")]);
        let result = adapter(&mock).send("mytopic", "hello", None, None).await;
        assert!(!result.success);
        assert_eq!(result.error_kind.as_deref(), Some("bad_format"));
        assert!(!result.retryable);
    }

    #[tokio::test]
    async fn server_errors_and_transport_failures_are_transient() {
        let server = MockTransport::new(vec![response(503, "unavailable")]);
        let result = adapter(&server).send("mytopic", "hello", None, None).await;
        assert_eq!(result.error_kind.as_deref(), Some("transient"));
        assert!(result.retryable);

        let down = MockTransport::new(vec![Err(TransportError::Request(
            "connection reset".to_string(),
        ))]);
        let result = adapter(&down).send("mytopic", "hello", None, None).await;
        assert_eq!(result.error_kind.as_deref(), Some("transient"));
        assert!(result.retryable);
    }

    #[tokio::test]
    async fn mid_way_failure_keeps_the_published_head() {
        // Three chunks at least, so the third publish is the one that fails.
        let content = long_content(400);
        let mock = MockTransport::new(vec![
            published("head-1"),
            published("head-2"),
            response(403, "forbidden"),
        ]);
        let result = adapter(&mock).send("mytopic", &content, None, None).await;

        assert!(!result.success);
        assert_eq!(result.error_kind.as_deref(), Some("forbidden"));
        assert_eq!(
            result.continuation_message_ids,
            vec!["head-1".to_string(), "head-2".to_string()]
        );
        match result.partial_overflow() {
            Some((delivered, total)) => {
                assert_eq!(delivered, 2);
                assert!(total > 2, "chunks after the failure must be counted");
            }
            None => panic!("a failure after a published head must carry the partial marker"),
        }
    }

    #[tokio::test]
    async fn blank_content_is_rejected_without_a_request() {
        let mock = MockTransport::new(Vec::new());
        let result = adapter(&mock).send("mytopic", "  \n\t ", None, None).await;

        assert!(!result.success);
        assert_eq!(result.error_kind.as_deref(), Some("bad_format"));
        assert!(result.message_id.is_none());
        assert!(
            mock.requests().is_empty(),
            "blank content must never reach the wire"
        );
    }

    #[tokio::test]
    async fn empty_chat_id_falls_back_to_the_configured_topic() {
        let mock = MockTransport::new(vec![published("id-9"), published("id-10")]);
        let adapter = NtfyAdapter::new(mock.clone(), DEFAULT_SERVER, "configured-topic", None);
        assert!(adapter.send("", "hello", None, None).await.success);
        // Whitespace is as empty as "".
        assert!(adapter.send("   ", "hello", None, None).await.success);

        let requests = mock.requests();
        assert_eq!(requests.len(), 2);
        for request in &requests {
            assert_eq!(request.url, "https://ntfy.sh/configured-topic");
        }

        // With no configured topic either, there is nothing to publish to.
        let unusable = MockTransport::new(Vec::new());
        let adapter = NtfyAdapter::new(unusable.clone(), DEFAULT_SERVER, "", None);
        let result = adapter.send("", "hello", None, None).await;
        assert_eq!(result.error_kind.as_deref(), Some("bad_format"));
        assert!(unusable.requests().is_empty());
    }

    #[tokio::test]
    async fn poll_once_parses_messages_and_skips_noise() {
        let body = [
            r#"{"id":"keep1","time":1700000000,"event":"keepalive","topic":"mytopic"}"#,
            r#"{"id":"open1","time":1700000001,"event":"open","topic":"mytopic"}"#,
            "not json at all",
            "",
            r#"{"id":"msg1","time":1700000002,"event":"message","topic":"mytopic","title":"Deploy","message":"finished","priority":4,"tags":["white_check_mark"]}"#,
            r#"{"id":"msg2","time":1700000003,"event":"message","topic":"mytopic","message":"second"}"#,
        ]
        .join("\n");
        let mock = MockTransport::new(vec![response(200, body)]);
        let events = match adapter(&mock).poll_once(None).await {
            Ok(events) => events,
            Err(error) => panic!("poll failed: {error}"),
        };

        assert_eq!(
            events.len(),
            2,
            "keepalive/open/malformed lines are skipped"
        );

        let first = &events[0];
        assert_eq!(first.text, "Deploy: finished");
        assert_eq!(first.message_id.as_deref(), Some("msg1"));
        assert_eq!(first.platform_update_id, Some(1_700_000_002));
        assert_eq!(first.user_id.as_deref(), Some("ntfy"));
        assert_eq!(first.source.chat_id, "mytopic");
        assert_eq!(first.source.chat_type, "channel");
        assert_eq!(first.source.chat_name.as_deref(), Some("mytopic"));
        assert_eq!(first.source.platform.as_str(), PLATFORM_NAME);
        assert_eq!(first.metadata.get("priority"), Some(&json!(4)));
        assert_eq!(
            first.metadata.get("tags"),
            Some(&json!(["white_check_mark"]))
        );
        assert!(
            first.raw_message.is_some(),
            "the raw line is kept for adapter-specific handling"
        );

        let second = &events[1];
        assert_eq!(second.text, "second");
        assert_eq!(second.message_id.as_deref(), Some("msg2"));
        assert!(second.metadata.is_empty());

        // `since` defaults to `all` when the caller passes no cursor.
        assert_eq!(
            mock.requests()[0].url,
            "https://ntfy.sh/mytopic/json?poll=1&since=all"
        );
        assert_eq!(mock.requests()[0].method, HttpMethod::Get);
    }

    #[tokio::test]
    async fn poll_once_honours_the_since_cursor() {
        let mock = MockTransport::new(vec![response(200, "")]);
        let adapter = NtfyAdapter::new(
            mock.clone(),
            DEFAULT_SERVER,
            "mytopic",
            Some("tk".to_string()),
        );
        let events = match adapter.poll_once(Some(" 30m ")).await {
            Ok(events) => events,
            Err(error) => panic!("poll failed: {error}"),
        };
        assert!(events.is_empty());

        let requests = mock.requests();
        assert_eq!(
            requests[0].url,
            "https://ntfy.sh/mytopic/json?poll=1&since=30m"
        );
        assert_eq!(
            header(&requests[0], "Authorization").as_deref(),
            Some("Bearer tk"),
            "a private topic needs the token on the poll too"
        );
    }

    #[tokio::test]
    async fn poll_once_reports_poll_failures() {
        let rejected = MockTransport::new(vec![response(500, "boom")]);
        let error = adapter(&rejected)
            .poll_once(None)
            .await
            .expect_err("500 must fail");
        assert!(error.contains("500"), "{error}");

        let down = MockTransport::new(vec![Err(TransportError::Request("timeout".to_string()))]);
        let error = adapter(&down)
            .poll_once(None)
            .await
            .expect_err("transport error must fail");
        assert!(error.contains("timeout"), "{error}");

        // A body with no usable line is an empty poll, not a failure.
        let noise = MockTransport::new(vec![response(200, "garbage\n\n{}")]);
        assert!(
            adapter(&noise)
                .poll_once(None)
                .await
                .expect("noise is not an error")
                .is_empty()
        );
    }

    #[tokio::test]
    async fn get_chat_info_reports_a_channel() {
        let mock = MockTransport::new(Vec::new());
        let info = adapter(&mock).get_chat_info("mytopic").await;
        assert_eq!(info.name, "mytopic");
        assert_eq!(info.chat_type, CHAT_TYPE_CHANNEL);
        assert!(
            mock.requests().is_empty(),
            "ntfy has no topic-metadata endpoint to call"
        );
    }

    #[tokio::test]
    async fn send_typing_is_a_no_op() {
        let mock = MockTransport::new(Vec::new());
        let adapter = adapter(&mock);
        adapter
            .send_typing("mytopic", Some(json!({ "typing": true })))
            .await;
        adapter.stop_typing("mytopic").await;
        assert!(mock.requests().is_empty());
        // Chunking is native, so the router must not truncate first.
        assert!(adapter.splits_long_messages());
        assert_eq!(adapter.max_message_length(), NTFY_MAX_MESSAGE_LENGTH);
    }

    #[test]
    fn publish_helpers_trim_and_join_metadata() {
        assert_eq!(non_empty_str(Some(&json!("  Deploy "))), Some("Deploy"));
        assert_eq!(non_empty_str(Some(&json!("   "))), None);
        assert_eq!(non_empty_str(Some(&json!(7))), None);
        assert_eq!(non_empty_str(None), None);

        assert_eq!(priority_header(Some(&json!(5))).as_deref(), Some("5"));
        assert_eq!(
            priority_header(Some(&json!(" urgent "))).as_deref(),
            Some("urgent")
        );
        assert_eq!(priority_header(Some(&json!(true))), None);
        assert_eq!(priority_header(None), None);

        assert_eq!(
            tags_header(Some(&json!(["a", " b ", "", 3]))).as_deref(),
            Some("a,b")
        );
        assert_eq!(tags_header(Some(&json!("x,y"))).as_deref(), Some("x,y"));
        assert_eq!(tags_header(Some(&json!([]))), None);
        assert_eq!(tags_header(None), None);

        assert_eq!(
            message_id_from(&json!({ "id": "abc" })).as_deref(),
            Some("abc")
        );
        assert_eq!(message_id_from(&json!({ "id": "  " })), None);
        assert_eq!(message_id_from(&json!({})), None);
        assert_eq!(body_excerpt("   "), "<empty body>");
        assert_eq!(body_excerpt(" boom "), "boom");
    }

    #[test]
    fn parse_event_keeps_titles_and_drops_empty_bodies() {
        let titled = parse_event(
            r#"{"id":"m3","event":"message","topic":"t","title":"Only a title"}"#,
            "fallback",
        );
        match titled {
            Some(event) => assert_eq!(event.text, "Only a title"),
            None => panic!("a titled message is still a message"),
        }

        assert!(parse_event(r#"{"id":"m4","event":"message","topic":"t"}"#, "fallback").is_none());
        // A message with no topic falls back to the configured one.
        let untopiced = parse_event(
            r#"{"id":"m5","event":"message","message":"hi"}"#,
            "fallback",
        );
        match untopiced {
            Some(event) => assert_eq!(event.source.chat_id, "fallback"),
            None => panic!("a topicless message falls back to the configured topic"),
        }
    }
}
