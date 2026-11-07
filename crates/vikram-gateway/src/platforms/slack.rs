//! Slack adapter — the Slack Web API (`auth.test`, `chat.postMessage`, `chat.update`,
//! `chat.delete`, `conversations.info`, `reactions.add`, `reactions.remove`, the native streaming
//! `chat.startStream` / `chat.appendStream` / `chat.stopStream` trio).
//!
//! Port of the text-transport half of hermes-agent `gateway/platforms/slack/adapter.py`: bot
//! authentication, chunked posting, markdown → mrkdwn normalization, edit/delete, thread replies,
//! channel metadata, outbound reactions and native draft streaming. Everything that needs the
//! persistent Socket Mode connection — inbound event dispatch (reaction *events* included), Block
//! Kit *rendering* (`render_blocks`, markdown → blocks), slash-command lanes, multi-workspace
//! routing — is deliberately out of scope for this pass (it is inbound work or needs a websocket
//! the frozen [`crate::transport::Transport`] does not model).
//!
//! Outbound Block Kit *payloads* are in scope: [`PlatformAdapter::send_card`] posts a caller-built
//! `blocks` array with `chat.postMessage`, exactly the argument pair `_post_interactive_blocks`
//! sends (adapter.py:4910-4921), deriving a `text` fallback from the card when the caller gave none.
//!
//! Wire facts this module encodes:
//!
//! - Slack answers **HTTP 200 even for failures**; the truth is the `ok` field of the JSON
//!   envelope, so every call is judged by `ok` first and only then by the status code.
//! - `retry_after` arrives as a `Retry-After` header, which the transport already parses into
//!   [`HttpResponse::retry_after`].
//! - `text` is mrkdwn, so standard markdown is rewritten by [`markdown_to_slack`] first.
//!
//! Outbound media (added on top of the text transport) follows Slack's **external upload** flow —
//! `files.getUploadURLExternal` → `POST` to the returned URL → `files.completeUploadExternal` —
//! which is what hermes' `files_upload_v2` call is (adapter.py:2931-2933, 6627-6632) and what the
//! slack-sdk helper implements internally (`files_upload_v2`,
//! slack_sdk/web/async_client.py:4637-4698). The upload POST is the one call in this module judged
//! by its **status code**, not by an `ok` envelope: Slack answers it with a plain-text receipt
//! such as `OK - 12`.

use async_trait::async_trait;
use serde_json::{Value, json};
use std::collections::HashMap;
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};

use crate::event::MessageEvent;
use crate::helpers::{codepoint_len, truncate_message};
use crate::platform::{
    ChatInfo, MediaKind, MediaSource, OutboundMedia, PlatformAdapter, SendResult,
};
use crate::transport::{HttpRequest, HttpResponse, SharedTransport};

/// Base URL of the Slack Web API.
pub const SLACK_API_BASE: &str = "https://slack.com/api";

/// Largest file one upload may carry: 20 MiB.
///
/// hermes states the same ceiling while triaging files ("Slack's bot upload cap is 20 MB",
/// adapter.py:4832-4833); its outbound path has no check of its own and lets Slack reject
/// oversized uploads with `file_upload_size_restricted`. The port refuses locally instead, so an
/// oversized payload never reaches the wire (and never pays for a `files.getUploadURLExternal`
/// round trip that is then thrown away).
pub const SLACK_MAX_UPLOAD_BYTES: usize = 20 * 1024 * 1024;

/// Character budget for one `chat.postMessage` / `chat.update` call.
///
/// slack-postMessage itself accepts roughly 40,000 characters; hermes splits at 39,000 to keep a
/// margin. The port splits at 4,000 so every chunk stays well inside the API limit and inside the
/// `chat.update` cap as well (an edit cannot split, so it truncates to this budget instead).
pub const SLACK_MAX_MESSAGE_LENGTH: usize = 4000;

/// Slack Web API adapter: bot token in, mrkdwn messages out.
pub struct SlackAdapter {
    transport: SharedTransport,
    bot_token: String,
    signing_secret: Option<String>,
    api_base: String,
    /// Open native streams keyed by `(channel, draft_id)`; see [`NativeStream`] and
    /// [`PlatformAdapter::send_draft`]. hermes keeps the same registry per thread key
    /// (`_active_streams`, adapter.py:1129-1133); this port keys it by the identity
    /// [`PlatformAdapter::send_draft`] can actually express.
    streams: Mutex<HashMap<(String, u64), NativeStream>>,
    /// Sticky "Slack refuses the native stream here" latch (`_native_stream_unsupported`,
    /// adapter.py:1136): set by the first error code in [`NATIVE_STREAM_UNSUPPORTED_MARKERS`] and
    /// never cleared, so later frames fail fast instead of paying a doomed round trip each time.
    native_stream_unsupported: AtomicBool,
}

/// One open native Slack stream: where it posts and how much text it already shows.
///
/// Mirrors the per-stream dict the reference keeps in `_active_streams` (`ts`, `sent`, adapter.py:
/// 2573-2575): `chat.appendStream` is append-only, so the next frame appends the delta past `sent`.
#[derive(Debug, Clone)]
struct NativeStream {
    /// `ts` of the streamed message, the handle every append addresses.
    ts: String,
    /// The full accumulated text the stream has been sent so far.
    sent: String,
}

impl SlackAdapter {
    /// Build an adapter over `transport` authenticating with `bot_token` (an `xoxb-…` bot token).
    pub fn new(transport: SharedTransport, bot_token: impl Into<String>) -> Self {
        Self {
            transport,
            bot_token: bot_token.into(),
            signing_secret: None,
            api_base: SLACK_API_BASE.to_string(),
            streams: Mutex::new(HashMap::new()),
            native_stream_unsupported: AtomicBool::new(false),
        }
    }

    /// Point the adapter at a mirror or a local test double.
    pub fn with_api_base(mut self, base: impl Into<String>) -> Self {
        self.api_base = base.into().trim_end_matches('/').to_string();
        self
    }

    /// Enable Events API signature verification with this app's signing secret.
    pub fn with_signing_secret(mut self, secret: impl Into<String>) -> Self {
        self.signing_secret = Some(secret.into());
        self
    }

    /// Slack's `url_verification` handshake, if this body is one.
    pub fn webhook_challenge(&self, body: &str) -> Option<String> {
        let value: Value = serde_json::from_str(body).ok()?;
        if value.get("type").and_then(Value::as_str) != Some("url_verification") {
            return None;
        }
        value
            .get("challenge")
            .and_then(Value::as_str)
            .map(str::to_owned)
    }

    /// `X-Slack-Signature` = `v0=` + HMAC-SHA256(signing_secret, `v0:{timestamp}:{body}`).
    pub fn verify_webhook(&self, body: &[u8], timestamp: &str, signature: &str) -> bool {
        let Some(secret) = self.signing_secret.as_deref() else {
            return false;
        };
        let mut base = Vec::with_capacity(body.len() + timestamp.len() + 4);
        base.extend_from_slice(b"v0:");
        base.extend_from_slice(timestamp.as_bytes());
        base.push(b':');
        base.extend_from_slice(body);
        let expected = format!("v0={}", hmac_sha256_hex(secret.as_bytes(), &base));
        constant_time_eq(expected.as_bytes(), signature.as_bytes())
    }

    /// Parse an Events API `event_callback` into messages. Bots, joins and edits are skipped.
    pub fn parse_webhook(&self, body: &str) -> Result<Vec<MessageEvent>, String> {
        let root: Value =
            serde_json::from_str(body).map_err(|err| format!("invalid json: {err}"))?;
        if root.get("type").and_then(Value::as_str) != Some("event_callback") {
            return Ok(Vec::new());
        }
        let Some(event) = root.get("event") else {
            return Ok(Vec::new());
        };
        if event.get("type").and_then(Value::as_str) != Some("message") {
            return Ok(Vec::new());
        }
        // Bots and system subtypes are not user turns.
        if event.get("bot_id").is_some() {
            return Ok(Vec::new());
        }
        if let Some(subtype) = event.get("subtype").and_then(Value::as_str)
            && subtype != "file_share"
        {
            return Ok(Vec::new());
        }
        let text = event
            .get("text")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .trim()
            .to_string();
        if text.is_empty() {
            return Ok(Vec::new());
        }
        let Some(channel) = event.get("channel").and_then(Value::as_str) else {
            return Ok(Vec::new());
        };
        let chat_type = if channel.starts_with('D') {
            "dm"
        } else {
            "group"
        };
        let platform = crate::event::Platform::new("slack").expect("valid platform");
        let mut source = crate::event::SessionSource::new(platform, channel);
        source.chat_type = chat_type.to_string();
        source.user_id = event.get("user").and_then(Value::as_str).map(str::to_owned);
        source.user_name = source.user_id.clone();
        source.message_id = event.get("ts").and_then(Value::as_str).map(str::to_owned);
        source.thread_id = event
            .get("thread_ts")
            .and_then(Value::as_str)
            .map(str::to_owned);

        let mut message = crate::event::MessageEvent::new(text, source);
        message.user_id = message.source.user_id.clone();
        message.user_name = message.source.user_name.clone();
        message.message_id = message.source.message_id.clone();
        message.reply_to_message_id = message.source.thread_id.clone();
        message.raw_message = Some(root);
        Ok(vec![message])
    }

    /// Absolute URL of one Web API method (`chat.postMessage`, `auth.test`, …).
    fn api_url(&self, method: &str) -> String {
        format!("{}/{method}", self.api_base)
    }

    /// POST `method` with a JSON body and unwrap Slack's `{"ok": …}` envelope.
    async fn post_api(&self, method: &str, payload: &Value) -> Result<Value, ApiFailure> {
        let request = HttpRequest::json(self.api_url(method), payload).bearer(&self.bot_token);
        self.api_call(request).await
    }

    /// Execute one request and interpret it as a Slack Web API reply.
    async fn api_call(&self, request: HttpRequest) -> Result<Value, ApiFailure> {
        match self.transport.execute(request).await {
            Ok(response) => interpret(&response),
            // No HTTP status at all: connection reset, timeout, DNS. Worth another attempt.
            Err(err) => Err(
                ApiFailure::new(format!("slack transport error: {err}"), "transient")
                    .retryable(None),
            ),
        }
    }

    /// [`api_call`](Self::api_call) for the file endpoints, whose error codes are classified by
    /// [`upload_error_kind`] rather than by the text lane's table.
    async fn upload_api_call(&self, request: HttpRequest) -> Result<Value, ApiFailure> {
        match self.transport.execute(request).await {
            Ok(response) => interpret_with(&response, upload_error_kind),
            Err(err) => Err(
                ApiFailure::new(format!("slack transport error: {err}"), "transient")
                    .retryable(None),
            ),
        }
    }

    /// [`api_call`](Self::api_call) for the card lane, judged by [`interpret_card`] so a Block Kit
    /// payload Slack could not parse is reported as `bad_format` rather than as a transient fault.
    async fn card_api_call(&self, request: HttpRequest) -> Result<Value, ApiFailure> {
        match self.transport.execute(request).await {
            Ok(response) => interpret_card(&response),
            // No HTTP status at all: connection reset, timeout, DNS. Worth another attempt.
            Err(err) => Err(ApiFailure::new(
                format!("slack card transport error: {err}"),
                "transient",
            )
            .retryable(None)),
        }
    }

    /// `chat.postMessage` one already-chunked body; returns the message `ts` and the raw reply.
    async fn post_chunk(
        &self,
        chat_id: &str,
        chunk: &str,
        thread_ts: Option<&str>,
    ) -> Result<(String, Value), ApiFailure> {
        let mut payload = json!({
            "channel": chat_id,
            "text": chunk,
            "mrkdwn": true,
        });
        if let Some(thread_ts) = thread_ts
            && let Some(object) = payload.as_object_mut()
        {
            object.insert("thread_ts".to_string(), json!(thread_ts));
        }
        let body = self.post_api("chat.postMessage", &payload).await?;
        match message_ts(&body) {
            Some(ts) => Ok((ts, body)),
            None => Err(ApiFailure::new(
                "slack chat.postMessage reply carried no message ts",
                "unknown",
            )),
        }
    }

    /// Upload step 1: `files.getUploadURLExternal` for `length` bytes filed under `filename`.
    ///
    /// The method's content type is `application/x-www-form-urlencoded` with `filename` and
    /// `length` as the required arguments, so the body is form-encoded rather than JSON — exactly
    /// what hermes' `files_upload_v2` asks the SDK for (`channel=…, filename=…, initial_comment=…,
    /// thread_ts=…`, adapter.py:2931-2933 / 6627-6632) and what the SDK turns into
    /// `files.getUploadURLExternal(filename=…, length=…)` (slack-sdk async_client.py:4700-4721),
    /// `length` being the byte count of the payload (internal_utils.py:377).
    ///
    /// Returns `(upload_url, file_id)`. A reply without either is a failure, never a quiet
    /// success: the next two steps cannot run without them.
    async fn request_upload_url(
        &self,
        filename: &str,
        length: usize,
    ) -> Result<(String, String), ApiFailure> {
        let fields = [
            ("filename", filename.to_string()),
            ("length", length.to_string()),
        ];
        let request = HttpRequest::form(self.api_url("files.getUploadURLExternal"), &fields)
            .bearer(&self.bot_token);
        let body = self.upload_api_call(request).await?;
        let upload_url = body
            .get("upload_url")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|url| !url.is_empty())
            .ok_or_else(|| {
                ApiFailure::new(
                    "slack files.getUploadURLExternal reply carried no upload_url",
                    "unknown",
                )
            })?;
        let file_id = body
            .get("file_id")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|id| !id.is_empty())
            .ok_or_else(|| {
                ApiFailure::new(
                    "slack files.getUploadURLExternal reply carried no file_id",
                    "unknown",
                )
            })?;
        Ok((upload_url.to_string(), file_id.to_string()))
    }

    /// Upload step 2: `POST` the bytes to the pre-signed `upload_url`, as `multipart/form-data`.
    ///
    /// Slack accepts the file here either as raw bytes or as a multipart form
    /// (`files.getUploadURLExternal` usage notes: "Files can be sent as raw bytes or can be
    /// multipart form encoded … Slack will return HTTP 200 if the upload is successful"), and the
    /// multipart framing is what the documented `curl -F filename=@text.txt [-F file=@…]` form
    /// sends. No `Authorization` header rides along: the URL is already signed, the docs' step-2
    /// example sends none, and slack-sdk uploads with `headers={}` (internal_utils.py:407) — the
    /// bot token must never be handed to a host that a response body named.
    async fn upload_bytes(
        &self,
        upload_url: &str,
        filename: &str,
        mime: &str,
        bytes: &[u8],
    ) -> Result<(), ApiFailure> {
        let boundary = multipart_boundary();
        let body = encode_upload_multipart(&boundary, filename, mime, bytes);
        let request = HttpRequest::post(upload_url)
            .header(
                "Content-Type",
                format!("multipart/form-data; boundary={boundary}"),
            )
            .body(body);
        match self.transport.execute(request).await {
            Ok(response) => interpret_upload(&response),
            // No HTTP status at all: connection reset, timeout, DNS. Worth another attempt.
            Err(err) => Err(ApiFailure::new(
                format!("slack file upload transport error: {err}"),
                "transient",
            )
            .retryable(None)),
        }
    }

    /// Upload step 3: `files.completeUploadExternal`, which is what actually shares the file.
    ///
    /// Field names mirror the reference call one for one — `channel` (adapter.py:2932 / 6628) is
    /// `channel_id`, `initial_comment` carries the caption (adapter.py:2932 / 6629, blank when
    /// there is none) and `thread_ts` threads the share inside a reply (adapter.py:2933 / 6630-6631)
    /// — plus the `files` array of `{id, title}` the SDK builds (async_client.py:4687-4698), whose
    /// `title` is the filename when the caller gave none (internal_utils.py:359-372).
    /// `initial_comment` and `thread_ts` are only sent when they have a value, so a bare upload
    /// posts the same fields the reference does (Slack treats an empty string as absent).
    async fn complete_upload(
        &self,
        file_id: &str,
        filename: &str,
        chat_id: &str,
        caption: Option<&str>,
        thread_ts: Option<&str>,
    ) -> Result<Value, ApiFailure> {
        let files = json!([{ "id": file_id, "title": filename }]).to_string();
        let mut fields: Vec<(&str, String)> = Vec::with_capacity(4);
        fields.push(("files", files));
        fields.push(("channel_id", chat_id.to_string()));
        if let Some(caption) = caption {
            fields.push(("initial_comment", caption.to_string()));
        }
        if let Some(thread_ts) = thread_ts {
            fields.push(("thread_ts", thread_ts.to_string()));
        }
        let request = HttpRequest::form(self.api_url("files.completeUploadExternal"), &fields)
            .bearer(&self.bot_token);
        self.upload_api_call(request).await
    }

    /// One `reactions.add` / `reactions.remove` call: true only when Slack accepted it.
    ///
    /// hermes funnels both directions through `_react` (adapter.py:3238-3260), which calls
    /// `reactions_add` / `reactions_remove` with exactly `channel`, `timestamp` and `name` and turns
    /// any exception — `already_reacted`, `no_reaction`, `missing_scope`, a rate limit — into
    /// `False` (it only debug-logs). The port keeps those three arguments and the same truth rule,
    /// but asks [`post_api`](Self::post_api) (so `ok: false` with HTTP 200 is a failure too) and
    /// refuses blank input before any request is made.
    async fn react(&self, chat_id: &str, message_id: &str, emoji: &str, method: &str) -> bool {
        let Some(name) = slack_reaction_name(emoji) else {
            return false;
        };
        let channel = chat_id.trim();
        let timestamp = message_id.trim();
        if channel.is_empty() || timestamp.is_empty() {
            return false;
        }
        let payload = json!({
            "channel": channel,
            "timestamp": timestamp,
            "name": name,
        });
        self.post_api(method, &payload).await.is_ok()
    }

    /// The stream registry. Poison-tolerant: a panic in another thread must not turn every later
    /// frame into a panic of its own.
    fn lock_streams(&self) -> std::sync::MutexGuard<'_, HashMap<(String, u64), NativeStream>> {
        self.streams
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    /// `chat.startStream` — the first frame of a stream (`_start_stream`, adapter.py:2552-2577).
    ///
    /// The reference sends `channel`, the `thread_ts` every stream must anchor to, the optional
    /// `recipient_user_id`/`recipient_team_id` that channels need, and `markdown_text` when the
    /// frame has text. All three of the extras come from the call's `metadata`, which
    /// [`PlatformAdapter::send_draft`] is not handed, so the frame carries `channel` and, when
    /// there is text, `markdown_text`.
    ///
    /// Returns the message `ts`. An `ok: true` envelope without a `ts` cannot be appended to and is
    /// therefore a failure, never a quiet success (the reference raises the same way).
    async fn start_stream_frame(
        &self,
        chat_id: &str,
        markdown_text: Option<&str>,
    ) -> Result<String, ApiFailure> {
        let mut payload = json!({ "channel": chat_id });
        if let Some(text) = markdown_text
            && let Some(object) = payload.as_object_mut()
        {
            object.insert("markdown_text".to_string(), json!(text));
        }
        let body = self.post_api("chat.startStream", &payload).await?;
        match non_empty_str(body.get("ts")) {
            Some(ts) => Ok(ts.to_string()),
            None => Err(ApiFailure::new(
                "slack chat.startStream reply carried no ts",
                "unknown",
            )),
        }
    }

    /// `chat.appendStream` — one delta onto an open stream (`adapter.py:2519`).
    async fn append_stream_frame(
        &self,
        chat_id: &str,
        ts: &str,
        markdown_text: &str,
    ) -> Result<(), ApiFailure> {
        let payload = json!({
            "channel": chat_id,
            "ts": ts,
            "markdown_text": markdown_text,
        });
        self.post_api("chat.appendStream", &payload)
            .await
            .map(|_| ())
    }

    /// Best-effort `chat.stopStream` for a stream the caller has given up on (`_seal_stream`,
    /// adapter.py:2631-2647): the reply is dropped, because the stream is already dead — the point
    /// is only that Slack stops drawing a live-typing indicator on it.
    async fn seal_stream(&self, chat_id: &str, ts: &str) -> Result<(), ApiFailure> {
        let payload = json!({ "channel": chat_id, "ts": ts });
        self.post_api("chat.stopStream", &payload).await.map(|_| ())
    }

    /// Turn one native-stream failure into a [`SendResult`], latching the unsupported state when
    /// Slack says the app cannot stream at all (`adapter.py:2537-2550`).
    ///
    /// A feature-gate answer — `missing_scope`, `not_allowed`, `unknown_method`, … — is not worth
    /// retrying: it is reported as `forbidden` and remembered, so
    /// [`supports_draft_streaming`](PlatformAdapter::supports_draft_streaming) turns false and the
    /// consumer falls back to its edit-based preview. Every other failure keeps the taxonomy
    /// [`ApiFailure::into_send_result`] applies (rate limits, transient, …).
    fn stream_failure(&self, failure: ApiFailure) -> SendResult {
        let marker = NATIVE_STREAM_UNSUPPORTED_MARKERS
            .iter()
            .find(|marker| failure.message.contains(**marker))
            .copied();
        match marker {
            Some(marker) => {
                self.native_stream_unsupported
                    .store(true, Ordering::Relaxed);
                SendResult::failed(
                    format!(
                        "{} (native streaming unavailable: {marker}; the edit-based preview takes \
                         over)",
                        failure.message
                    ),
                    Some("forbidden"),
                )
            }
            None => failure.into_send_result(),
        }
    }
}

#[async_trait]
impl PlatformAdapter for SlackAdapter {
    /// `auth.test`: authenticated = the token is accepted by the workspace.
    async fn connect(&mut self, _is_reconnect: bool) -> bool {
        let request = HttpRequest::post(self.api_url("auth.test")).bearer(&self.bot_token);
        match self.api_call(request).await {
            Ok(body) => body.get("ok").and_then(Value::as_bool) == Some(true),
            Err(_) => false,
        }
    }

    /// Nothing to tear down: every call is an independent HTTP request.
    async fn disconnect(&mut self) {}

    /// Convert to mrkdwn, chunk to the 4,000-character budget and post every chunk in order.
    ///
    /// `reply_to` is a `thread_ts`; when present every chunk is posted in that thread. On the
    /// first failing chunk the failure is returned as-is — the chunks already posted stay posted
    /// (Slack has no transactional multi-post).
    async fn send(
        &self,
        chat_id: &str,
        content: &str,
        reply_to: Option<&str>,
        _metadata: Option<Value>,
    ) -> SendResult {
        if content.trim().is_empty() {
            // Slack rejects blank posts with `no_text`; failing here saves a round trip.
            return SendResult::failed("slack message content is empty", Some("bad_format"));
        }
        let formatted = markdown_to_slack(content);
        let chunks = truncate_message(&formatted, SLACK_MAX_MESSAGE_LENGTH, codepoint_len);
        let thread_ts = reply_to.filter(|ts| !ts.is_empty());

        let mut sent: Vec<String> = Vec::with_capacity(chunks.len());
        for chunk in &chunks {
            match self.post_chunk(chat_id, chunk, thread_ts).await {
                Ok((ts, _body)) => sent.push(ts),
                Err(failure) => return failure.into_send_result(),
            }
        }
        match sent.last() {
            Some(last) => SendResult::ok_split(last.clone(), sent),
            // Unreachable for non-empty content, but never report a phantom success.
            None => SendResult::failed("slack message produced no chunks", Some("bad_format")),
        }
    }

    /// `chat.update`: an edit cannot split, so the content is truncated to the budget.
    async fn edit_message(
        &self,
        chat_id: &str,
        message_id: &str,
        content: &str,
        _finalize: bool,
    ) -> SendResult {
        let formatted = markdown_to_slack(content);
        let text = truncate_message(&formatted, SLACK_MAX_MESSAGE_LENGTH, codepoint_len)
            .into_iter()
            .next()
            .unwrap_or_default();
        let payload = json!({
            "channel": chat_id,
            "ts": message_id,
            "text": text,
        });
        match self.post_api("chat.update", &payload).await {
            Ok(_) => SendResult::ok(message_id),
            Err(failure) => failure.into_send_result(),
        }
    }

    /// `chat.delete`, best effort: the result is ignored (hermes logs it at debug level only).
    async fn delete_message(&self, chat_id: &str, message_id: &str) {
        let payload = json!({
            "channel": chat_id,
            "ts": message_id,
        });
        let _ = self.post_api("chat.delete", &payload).await;
    }

    /// Media uploads are implemented: one file per call through Slack's external upload flow.
    fn supports_media(&self) -> bool {
        true
    }

    /// Reactions are implemented through `reactions.add` / `reactions.remove`.
    fn supports_reactions(&self) -> bool {
        true
    }

    /// Rich cards are implemented: a Block Kit payload rides on `chat.postMessage`'s `blocks`
    /// argument (`_post_interactive_blocks`, adapter.py:4910-4921).
    fn supports_cards(&self) -> bool {
        true
    }

    /// Add an emoji reaction to `message_id` (a message `ts`) with `reactions.add`.
    ///
    /// The body is the reference's `_react(remove=False)` call — `channel`, `timestamp`, `name`
    /// (adapter.py:3246-3247) — with the name normalized by [`slack_reaction_name`] first, since a
    /// caller may hand over `:eyes:`, a glyph or an alias. A reaction Slack refuses
    /// (`already_reacted`, `missing_scope`, `invalid_name`, …) is reported as `false`: Slack answers
    /// HTTP 200 for those, so the `ok` field is what decides.
    async fn add_reaction(&self, chat_id: &str, message_id: &str, emoji: &str) -> bool {
        self.react(chat_id, message_id, emoji, "reactions.add")
            .await
    }

    /// Remove a reaction this bot added, with `reactions.remove` (same three arguments as the add).
    ///
    /// Slack's `no_reaction` ("this bot never reacted that way") and `missing_scope` failures surface
    /// as `false` for the same reason as on the add path: the call is judged by the envelope, never
    /// by the HTTP status alone.
    async fn remove_reaction(&self, chat_id: &str, message_id: &str, emoji: &str) -> bool {
        self.react(chat_id, message_id, emoji, "reactions.remove")
            .await
    }

    /// Upload one file through Slack's three-step external upload flow.
    ///
    /// The reference is hermes' `files_upload_v2` call — `channel=chat_id, file/content=…,
    /// filename=…, initial_comment=caption or "", thread_ts=thread_ts` (adapter.py:2931-2933 and
    /// the standalone sender at adapter.py:6627-6632) — which is the SDK wrapper around:
    ///
    /// 1. `POST https://slack.com/api/files.getUploadURLExternal` (`filename`, `length`) →
    ///    `{ok, upload_url, file_id}` (slack-sdk async_client.py:4700-4721);
    /// 2. `POST <upload_url>` with the bytes (async_client.py:4670-4686; the SDK posts raw bytes,
    ///    Slack also accepts `multipart/form-data`, which is what this adapter frames);
    /// 3. `POST https://slack.com/api/files.completeUploadExternal` with
    ///    `files=[{id,title}]`, `channel_id`, `initial_comment`, `thread_ts`
    ///    (async_client.py:4687-4698).
    ///
    /// Every step is fatal for the ones after it: a failed upload-URL request stops before any
    /// byte is sent, a failed upload stops before completion (Slack discards an upload that is
    /// never completed — `files.completeUploadExternal` "can only be called once"), and only a
    /// completed upload reports success, under the id described by [`completed_upload_id`].
    ///
    /// `caption` is the upload's `initial_comment`, mrkdwn-normalized the way the reference
    /// normalizes a standalone caption (adapter.py:6770); `reply_to` is the `thread_ts` the
    /// reference threads the share with (adapter.py:6630-6631).
    ///
    /// [`MediaSource::Url`] is refused before the network is touched: hermes downloads remote
    /// media into bytes and uploads those (adapter.py:3049-3056), and this transport hands back
    /// text bodies only, so a fetch-through would corrupt the payload. The refusal names
    /// [`MediaSource::Bytes`] as the supported source.
    async fn send_media(
        &self,
        chat_id: &str,
        media: &OutboundMedia,
        reply_to: Option<&str>,
    ) -> SendResult {
        let bytes = match &media.source {
            MediaSource::Bytes(bytes) => bytes,
            MediaSource::Url(url) => {
                return SendResult::failed(
                    format!(
                        "slack media upload needs bytes: refusing MediaSource::Url({url}) because \
                         the reference downloads URL media and uploads the downloaded bytes \
                         (adapter.py `_collect_image_uploads`, 3049-3056) and this transport only \
                         returns text bodies, so a fetch-through would corrupt the file — pass \
                         MediaSource::Bytes"
                    ),
                    Some("bad_format"),
                );
            }
        };
        if bytes.is_empty() {
            // `files.getUploadURLExternal` rejects a `length` of 0 with `missing_argument`.
            return SendResult::failed(
                "slack media upload: refusing to upload empty bytes",
                Some("bad_format"),
            );
        }
        if bytes.len() > SLACK_MAX_UPLOAD_BYTES {
            return SendResult::failed(
                format!(
                    "slack media upload: {} bytes exceeds the {} byte per-file upload cap",
                    bytes.len(),
                    SLACK_MAX_UPLOAD_BYTES
                ),
                Some("too_long"),
            );
        }

        let filename = media_filename(media);
        let mime = media_mime(media);
        let caption = media
            .caption
            .as_deref()
            .map(str::trim)
            .filter(|caption| !caption.is_empty())
            .map(markdown_to_slack)
            .map(|caption| caption.trim().to_string())
            .filter(|caption| !caption.is_empty());
        let thread_ts = reply_to.filter(|ts| !ts.is_empty());

        let (upload_url, file_id) = match self.request_upload_url(&filename, bytes.len()).await {
            Ok(located) => located,
            Err(failure) => return failure.into_send_result(),
        };
        if let Err(failure) = self
            .upload_bytes(&upload_url, &filename, &mime, bytes)
            .await
        {
            return failure.into_send_result();
        }
        let completion = match self
            .complete_upload(&file_id, &filename, chat_id, caption.as_deref(), thread_ts)
            .await
        {
            Ok(body) => body,
            Err(failure) => return failure.into_send_result(),
        };
        SendResult::ok(completed_upload_id(&completion, &file_id))
    }

    /// Post a Block Kit card with `chat.postMessage`.
    ///
    /// `card` is either the `blocks` array itself or the object form
    /// `{"blocks": [...], "text": "…"}` — the pair `chat.postMessage` itself takes. The body is
    /// exactly the reference's card post: `channel`, `text` and `blocks`
    /// (`_post_interactive_blocks`, adapter.py:4914-4916), where `text` is the
    /// notification/accessibility fallback (`_post_chunks`, adapter.py:2287-2300: "``text`` stays
    /// the notification/accessibility fallback"). When the caller supplied no `text` it is derived
    /// from the card's own text objects by [`card_fallback_text`], so a notification is never
    /// blank; a card carrying no text at all (dividers, images, inputs only) posts without `text`,
    /// because Slack accepts a blocks-only message and placeholder copy would be invented words.
    ///
    /// Refused locally, with no request: a blank channel id, a `card` that is neither a blocks
    /// array nor an object with a `blocks` array, an empty blocks list, a block that is not an
    /// object with a `type` (Slack answers `invalid_blocks` for one of those and drops the WHOLE
    /// message — the reason `sanitize_blocks` exists, block_kit.py:574-596), and more than
    /// [`SLACK_MAX_BLOCKS`] blocks.
    ///
    /// Two things the reference does are not done here, both because of what this method is handed:
    /// the `thread_ts` a card is threaded with comes from metadata (`_post_interactive_blocks`,
    /// adapter.py:4917-4919), and this trait passes none; and the text objects are not clamped to
    /// Slack's 3000/150-character caps (`sanitize_blocks`, block_kit.py:582-593) — a caller-sized
    /// card is posted as given, and Slack's own `invalid_blocks` answer is reported as `bad_format`.
    ///
    /// Success is only the `ts` Slack returned, with the raw envelope in
    /// [`SendResult::raw_response`] the way `send` reports it (adapter.py:2269). `ok: false` on
    /// HTTP 200, a 4xx (400 `invalid_blocks` included), a 5xx or a transport error is a typed
    /// failure via [`card_error_kind`] / [`interpret_card`] — never a claimed delivery.
    async fn send_card(&self, chat_id: &str, card: &Value) -> SendResult {
        let channel = chat_id.trim();
        if channel.is_empty() {
            return SendResult::failed("slack send card: channel id is empty", Some("bad_format"));
        }
        let payload = match card_payload(card) {
            Ok(payload) => payload,
            Err(reason) => {
                return SendResult::failed(
                    format!("slack send card: {reason}"),
                    Some("bad_format"),
                );
            }
        };
        if payload.blocks.is_empty() {
            return SendResult::failed(
                "slack send card: the card carries no blocks",
                Some("bad_format"),
            );
        }
        if payload.blocks.len() > SLACK_MAX_BLOCKS {
            return SendResult::failed(
                format!(
                    "slack send card: {} blocks exceeds Slack's {} block per-message cap",
                    payload.blocks.len(),
                    SLACK_MAX_BLOCKS
                ),
                Some("too_long"),
            );
        }
        for (index, block) in payload.blocks.iter().enumerate() {
            if block_type(block).is_none() {
                return SendResult::failed(
                    format!("slack send card: block {index} is not an object carrying a `type`"),
                    Some("bad_format"),
                );
            }
        }

        let text = payload
            .text
            .map(str::to_string)
            .unwrap_or_else(|| card_fallback_text(payload.blocks));
        let mut body = json!({ "channel": channel, "blocks": payload.blocks });
        if !text.is_empty()
            && let Some(object) = body.as_object_mut()
        {
            object.insert("text".to_string(), json!(text));
        }
        let request =
            HttpRequest::json(self.api_url("chat.postMessage"), &body).bearer(&self.bot_token);
        match self.card_api_call(request).await {
            Ok(reply) => match message_ts(&reply) {
                Some(ts) => {
                    let mut result = SendResult::ok(ts);
                    result.raw_response = Some(reply);
                    result
                }
                None => SendResult::failed(
                    "slack chat.postMessage reply carried no message ts",
                    Some("unknown"),
                ),
            },
            Err(failure) => failure.into_send_result(),
        }
    }

    /// Slack streams a message natively through `chat.startStream` / `chat.appendStream`, so the
    /// native preview is advertised while the workspace accepts it.
    ///
    /// hermes answers false in two cases (`supports_draft_streaming`, adapter.py:2466-2475): a
    /// previous native attempt told it the app cannot stream (`_native_stream_unsupported`), or a
    /// link-preview behaviour is configured, because `chat.*Stream` carries no
    /// `unfurl_links`/`unfurl_media` controls. This port has no unfurl configuration surface, so
    /// only the first condition can apply: the flag is true until Slack refuses — and then it stays
    /// false, so the consumer never pays a doomed round trip twice.
    fn supports_draft_streaming(&self) -> bool {
        !self.native_stream_unsupported.load(Ordering::Relaxed)
    }

    /// Stream one frame through Slack's native streaming API — `chat.startStream` for the first
    /// frame of a `(channel, draft_id)` stream, `chat.appendStream` for every later one.
    ///
    /// Port of hermes `send_draft` (adapter.py:2483-2550) with its `_start_stream`
    /// (adapter.py:2552-2577) and `_seal_stream` (adapter.py:2631-2647):
    ///
    /// - `content` is the full accumulated text, as the reference documents ("``content`` is the
    ///   full accumulated text"). The first frame posts it; a later frame appends only the delta
    ///   past what the stream already shows, because `chat.appendStream` is append-only. A frame
    ///   identical to the sent text costs no request and reports the stream's `ts`
    ///   (adapter.py:2509-2510).
    /// - The consumer's trailing cursor glyph is stripped first ([`strip_stream_cursor`]), so a
    ///   glyph that marked "still streaming" does not survive into the finished message.
    /// - Stream state is per `(channel, draft_id)`: two drafts in one channel never append into each
    ///   other's message, and a new `draft_id` opens a new stream instead of reusing the previous
    ///   one. hermes keys by `(team, channel, thread_ts)` (`_stream_key`, adapter.py:2590-2600)
    ///   because its gateway stamps a `thread_id` on every frame; this trait hands `send_draft` no
    ///   metadata, so the identity the caller *can* express — the channel plus the draft being
    ///   animated — is the key.
    /// - Omitted against the reference, because no metadata reaches this method:
    ///   `chat.startStream`'s `thread_ts` anchor and its `recipient_user_id`/`recipient_team_id`
    ///   channel-recipient fields (`_start_stream`, adapter.py:2556-2568). A workspace that
    ///   requires them answers with an error, which surfaces as a typed failure below — never as a
    ///   claimed delivery.
    /// - A frame that rewrote the already-streamed prefix cannot be appended to: the stream is
    ///   sealed with `chat.stopStream` and the frame fails, so the consumer falls back to the
    ///   edit-based preview (adapter.py:2511-2516).
    /// - `message_not_in_streaming_state` — Slack sealed the message server-side — reopens a fresh
    ///   stream seeded with the still-unsent tail, keeping the full text as the new high-water mark
    ///   (adapter.py:2518-2535).
    /// - An error naming one of [`NATIVE_STREAM_UNSUPPORTED_MARKERS`] latches the unsupported state
    ///   exactly as the reference does (adapter.py:2540-2547): this and later frames fail without
    ///   another request, and [`supports_draft_streaming`](Self::supports_draft_streaming) turns
    ///   false. Any other failure keeps the send taxonomy (rate limits stay retryable, …).
    /// - Blank content or a blank channel is refused before any request, and every failure drops
    ///   the recorded stream so the next frame opens a fresh one instead of appending into a state
    ///   Slack no longer holds (adapter.py:2537-2538).
    async fn send_draft(&self, chat_id: &str, draft_id: u64, content: &str) -> SendResult {
        let chat_id = chat_id.trim();
        if chat_id.is_empty() {
            return SendResult::failed("slack send draft: channel id is empty", Some("bad_format"));
        }
        if content.trim().is_empty() {
            return SendResult::failed("slack send draft: content is empty", Some("bad_format"));
        }
        if self.native_stream_unsupported.load(Ordering::Relaxed) {
            return SendResult::failed(
                "slack native streaming is unsupported on this app; use the edit-based preview",
                Some("forbidden"),
            );
        }
        let text = strip_stream_cursor(content);
        if text.is_empty() {
            // The frame was nothing but a cursor glyph: there is no text to stream.
            return SendResult::failed(
                "slack send draft: frame carried no text once the cursor glyph was stripped",
                Some("bad_format"),
            );
        }
        let key = (chat_id.to_string(), draft_id);
        let Some(stream) = self.lock_streams().get(&key).cloned() else {
            return match self.start_stream_frame(chat_id, Some(&text)).await {
                Ok(ts) => {
                    self.lock_streams().insert(
                        key,
                        NativeStream {
                            ts: ts.clone(),
                            sent: text,
                        },
                    );
                    SendResult::ok(ts)
                }
                Err(failure) => self.stream_failure(failure),
            };
        };
        if text == stream.sent {
            // Nothing new to append; the stream already shows this frame.
            return SendResult::ok(stream.ts);
        }
        if !text.starts_with(&stream.sent) {
            // The frame rewrote the prefix: an append-only stream cannot show that. Seal it so it
            // does not hang, forget it, and fail the frame so the consumer edits instead.
            self.lock_streams().remove(&key);
            let _ = self.seal_stream(chat_id, &stream.ts).await;
            return SendResult::failed(
                "slack send draft: frame rewrote the already-streamed text",
                Some("bad_format"),
            );
        }
        let delta = &text[stream.sent.len()..];
        match self.append_stream_frame(chat_id, &stream.ts, delta).await {
            Ok(()) => {
                self.lock_streams().insert(
                    key,
                    NativeStream {
                        ts: stream.ts.clone(),
                        sent: text,
                    },
                );
                SendResult::ok(stream.ts)
            }
            Err(failure) if failure.message.contains("message_not_in_streaming_state") => {
                // Slack sealed the message server-side. Reopen a fresh stream holding only the
                // unsent tail; `sent` keeps the full text so later deltas still diff correctly.
                match self.start_stream_frame(chat_id, Some(delta)).await {
                    Ok(ts) => {
                        self.lock_streams().insert(
                            key,
                            NativeStream {
                                ts: ts.clone(),
                                sent: text,
                            },
                        );
                        SendResult::ok(ts)
                    }
                    Err(failure) => {
                        self.lock_streams().remove(&key);
                        self.stream_failure(failure)
                    }
                }
            }
            Err(failure) => {
                self.lock_streams().remove(&key);
                self.stream_failure(failure)
            }
        }
    }

    /// `conversations.info`: `#name` plus the chat kind, falling back to `chat_id`/`channel`.
    async fn get_chat_info(&self, chat_id: &str) -> ChatInfo {
        let fallback = ChatInfo {
            name: chat_id.to_string(),
            chat_type: "channel".to_string(),
        };
        let url = format!(
            "{SLACK_API_BASE}/conversations.info?channel={}",
            encode_query_value(chat_id)
        );
        let request = HttpRequest::get(url).bearer(&self.bot_token);
        let body = match self.api_call(request).await {
            Ok(body) => body,
            Err(_) => return fallback,
        };
        let channel = match body.get("channel") {
            Some(channel) if channel.is_object() => channel,
            _ => return fallback,
        };
        let chat_type = if flag(channel, "is_im") {
            "dm"
        } else if flag(channel, "is_mpim") {
            "group"
        } else {
            "channel"
        };
        let name = match channel.get("name").and_then(Value::as_str) {
            Some(name) if !name.is_empty() => format!("#{name}"),
            _ => chat_id.to_string(),
        };
        ChatInfo {
            name,
            chat_type: chat_type.to_string(),
        }
    }

    /// Slack mrkdwn renders fenced code blocks.
    fn supports_code_blocks(&self) -> bool {
        true
    }

    /// `send` chunks natively at [`SLACK_MAX_MESSAGE_LENGTH`], so the router must not truncate.
    fn splits_long_messages(&self) -> bool {
        true
    }

    fn max_message_length(&self) -> usize {
        SLACK_MAX_MESSAGE_LENGTH
    }

    /// The Slack Web API has no typing endpoint (typing lives in the Socket Mode RTM stream).
    async fn send_typing(&self, _chat_id: &str, _metadata: Option<Value>) {}
}

// ----- Outbound media: naming, multipart framing, receipts -----

/// Longest filename kept from a caller value, so one absurd name cannot bloat a part header.
const MAX_FILENAME_CHARS: usize = 200;

/// Default filename and MIME type for a [`MediaKind`] with no caller-supplied values.
///
/// The reference always receives a real path and derives both from it (`filename=
/// os.path.basename(media_path)`, adapter.py:6628 / 2954); a Rust caller may hold bytes with
/// neither, so the kind supplies a plausible pair. Slack reads the type from the filename
/// extension as well as the part header, so the two stay consistent.
fn default_media_attachment(kind: MediaKind) -> (&'static str, &'static str) {
    match kind {
        MediaKind::Image => ("image.png", "image/png"),
        MediaKind::Video => ("video.mp4", "video/mp4"),
        MediaKind::Audio => ("audio.mp3", "audio/mpeg"),
        MediaKind::Document => ("attachment.bin", "application/octet-stream"),
    }
}

/// Make a caller string safe to interpolate into a part header: no CR/LF (header injection), no
/// quote or backslash (parameter breakout), no other control characters, no unbounded length.
fn sanitize_part_token(raw: &str) -> String {
    let cleaned: String = raw
        .chars()
        .filter(|ch| !ch.is_control() && *ch != '"' && *ch != '\\')
        .take(MAX_FILENAME_CHARS)
        .collect();
    cleaned.trim().to_string()
}

/// Filename sent as `files.getUploadURLExternal`'s `filename` and as the upload part's filename.
fn media_filename(media: &OutboundMedia) -> String {
    let (default_name, _) = default_media_attachment(media.kind);
    media
        .filename
        .as_deref()
        .map(sanitize_part_token)
        .filter(|name| !name.is_empty())
        .unwrap_or_else(|| default_name.to_string())
}

/// MIME type of the upload part. Slack sniffs the bytes too, so a caller value is only used when
/// it actually looks like a type (`type/subtype`).
fn media_mime(media: &OutboundMedia) -> String {
    let (_, default_mime) = default_media_attachment(media.kind);
    media
        .mime
        .as_deref()
        .map(sanitize_part_token)
        .filter(|mime| mime.contains('/'))
        .unwrap_or_else(|| default_mime.to_string())
}

/// A fresh `multipart/form-data` boundary for one upload.
///
/// The delimiter must not occur inside the file, so it mixes the clock with a process counter
/// rather than being a constant. The clock read cannot fail a send: an unavailable or pre-epoch
/// system clock contributes a zero nonce and the counter still makes the token unique.
fn multipart_boundary() -> String {
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let seq = COUNTER.fetch_add(1, Ordering::Relaxed);
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|elapsed| elapsed.as_nanos() as u64)
        .unwrap_or(0);
    format!("----VIKRAMSlackUpload{nanos:016x}{seq:08x}")
}

/// `multipart/form-data` body for the upload POST: one `file` part carrying the bytes.
///
/// Slack's documented shape for this step is `-F <field>=@"<path>"` (or raw bytes); slack-sdk
/// names the part `file` for the external-upload lane, and `Content-Disposition` keeps the
/// filename so the file lands under the name `files.getUploadURLExternal` reserved.
fn encode_upload_multipart(boundary: &str, filename: &str, mime: &str, bytes: &[u8]) -> Vec<u8> {
    let mut body = Vec::with_capacity(bytes.len() + 256);
    body.extend_from_slice(format!("--{boundary}\r\n").as_bytes());
    body.extend_from_slice(
        format!("Content-Disposition: form-data; name=\"file\"; filename=\"{filename}\"\r\n")
            .as_bytes(),
    );
    body.extend_from_slice(format!("Content-Type: {mime}\r\n\r\n").as_bytes());
    body.extend_from_slice(bytes);
    body.extend_from_slice(b"\r\n");
    body.extend_from_slice(format!("--{boundary}--\r\n").as_bytes());
    body
}

/// Judge the response to the pre-signed upload POST.
///
/// This is the one call in the module without an `ok` envelope: Slack answers it with HTTP 200 and
/// a plain-text receipt (`OK - 12`) and says to "check the HTTP status code to verify the file's
/// successful upload"; slack-sdk likewise only compares `status != 200`
/// (async_client.py:4678-4685). Two HTTP-200 bodies are still failures, because neither means the
/// bytes landed:
///
/// - a JSON envelope with `ok: false` (a proxy or an edge error answering in Slack's own shape),
///   reported with its error code and the 429 `Retry-After` rule;
/// - an HTML document, the login/captive-proxy page hermes diagnoses as a scope or auth problem
///   (adapter.py:1457-1460).
fn interpret_upload(response: &HttpResponse) -> Result<(), ApiFailure> {
    if !response.ok() {
        let body = response.json().unwrap_or(Value::Null);
        return Err(http_failure(response, &body));
    }
    if response.body.trim().starts_with('<') {
        return Err(ApiFailure::new(
            "slack file upload returned an HTML page instead of an upload receipt (usually a \
             scope, auth or proxy problem)",
            "forbidden",
        ));
    }
    if let Ok(body) = response.json()
        && body.get("ok").and_then(Value::as_bool) == Some(false)
    {
        let code = body
            .get("error")
            .and_then(Value::as_str)
            .unwrap_or("unknown_error");
        let failure = ApiFailure::new(
            format!("slack file upload error: {code}"),
            upload_error_kind(code),
        );
        return Err(if code == "ratelimited" {
            failure.retryable(response.retry_after)
        } else {
            failure
        });
    }
    Ok(())
}

/// Slack's upload-specific `error` codes → [`crate::platform::SEND_ERROR_KINDS`].
///
/// The text lane keeps [`error_kind`] untouched; this table only refines what the file endpoints
/// (all of these reach the caller through `files_upload_v2` in hermes) mean for a delivery ledger:
/// "this team forbids uploads" is not worth retrying, "the file id is gone" is not found.
fn upload_error_kind(code: &str) -> &'static str {
    if matches!(
        code,
        "file_upload_size_restricted" | "snippet_too_large" | "alt_txt_too_large"
    ) {
        return "too_long";
    }
    if matches!(
        code,
        "access_denied"
            | "no_permission"
            | "not_allowed_token_type"
            | "missing_scope"
            | "file_uploads_disabled"
            | "file_uploads_except_images_disabled"
            | "file_type_not_allowed"
            | "posting_to_channel_denied"
            | "user_is_external_guest"
            | "restricted_action"
            | "not_authed"
            | "invalid_auth"
            | "token_revoked"
            | "account_inactive"
    ) {
        return "forbidden";
    }
    match code {
        "file_not_found" | "channel_not_found" | "invalid_channel" => "not_found",
        "ratelimited" => "rate_limited",
        other => error_kind(other),
    }
}

/// The id a completed upload is reported under: the shared message `ts` when Slack returned one,
/// else the file id.
///
/// hermes reads the id out of the upload reply in this order (adapter.py:6636-6650):
/// `file.shares.*.<channel>[0].ts` → `file.timestamp` → `ts`. The completion envelope carries the
/// file under `files[0]` (`files.completeUploadExternal` response), while share paths answer with a
/// single `file`, so both shapes are walked. When none of the three carries an id the file's own
/// `id` is reported instead of a null message id: the upload *did* complete, and a ledger entry
/// with no id at all would lose the only handle the file has. Never invents an id Slack did not
/// return — the last resort is the id `files.getUploadURLExternal` handed out for this upload.
fn completed_upload_id(body: &Value, file_id: &str) -> String {
    let file = body
        .get("file")
        .filter(|file| file.is_object())
        .or_else(|| {
            body.get("files")
                .and_then(Value::as_array)
                .and_then(|files| files.first())
        });
    if let Some(ts) = file.and_then(share_ts) {
        return ts;
    }
    if let Some(ts) = file.and_then(timestamp_field) {
        return ts;
    }
    if let Some(ts) = non_empty_str(body.get("ts")) {
        return ts.to_string();
    }
    if let Some(id) = file.and_then(|file| non_empty_str(file.get("id"))) {
        return id.to_string();
    }
    file_id.to_string()
}

/// First `shares.<bucket>.<channel>[0].ts` in a file object (the shared message's timestamp).
fn share_ts(file: &Value) -> Option<String> {
    let shares = file.get("shares")?.as_object()?;
    for bucket in shares.values() {
        let Some(channels) = bucket.as_object() else {
            continue;
        };
        for entries in channels.values() {
            let Some(shared) = entries.as_array().and_then(|list| list.first()) else {
                continue;
            };
            if let Some(ts) = non_empty_str(shared.get("ts")) {
                return Some(ts.to_string());
            }
        }
    }
    None
}

/// `file.timestamp`, which a file object carries as a number in the documented payload shape and
/// as a string in some SDK generations.
fn timestamp_field(file: &Value) -> Option<String> {
    match file.get("timestamp") {
        Some(Value::Number(number)) => Some(number.to_string()),
        other => non_empty_str(other).map(str::to_string),
    }
}

/// `value` when it is a non-empty JSON string.
fn non_empty_str(value: Option<&Value>) -> Option<&str> {
    value
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|text| !text.is_empty())
}

// ----- Rich cards: Block Kit payloads -----

/// Slack's hard cap on the blocks in one message (`MAX_BLOCKS`, block_kit.py:35; `sanitize_blocks`
/// clamps to it at block_kit.py:657). A longer payload is answered with `invalid_blocks`.
pub const SLACK_MAX_BLOCKS: usize = 50;

/// A caller's card, split into the two values `chat.postMessage` takes: the blocks, and the
/// caller's own `text` fallback when it sent the object form.
struct CardPayload<'a> {
    blocks: &'a [Value],
    text: Option<&'a str>,
}

/// Split a `card` argument into blocks plus an optional caller-supplied `text`.
///
/// Two shapes are accepted, matching how Slack spells the pair on `chat.postMessage`
/// (`channel`, `text`, `blocks` — adapter.py:4914-4916):
///
/// - the bare blocks array, `[{"type": "header", …}, …]`;
/// - the object form, `{"blocks": [...], "text": "…"}` — `text` is optional, and blank or
///   non-string values are treated as absent so they cannot become an empty notification.
///
/// Anything else (a string, a number, an object with no `blocks` array) is not a payload Slack can
/// render, so it is refused with the reason and no request at all.
fn card_payload(card: &Value) -> Result<CardPayload<'_>, String> {
    match card {
        Value::Array(blocks) => Ok(CardPayload { blocks, text: None }),
        Value::Object(object) => {
            let Some(blocks) = object.get("blocks").and_then(Value::as_array) else {
                return Err(
                    "a card object must carry a `blocks` array (Block Kit blocks, optionally \
                     alongside a `text` fallback)"
                        .to_string(),
                );
            };
            let text = object
                .get("text")
                .and_then(Value::as_str)
                .map(str::trim)
                .filter(|text| !text.is_empty());
            Ok(CardPayload { blocks, text })
        }
        other => Err(format!(
            "a card must be a Block Kit blocks array or an object with a `blocks` array, got {}",
            json_type_name(other)
        )),
    }
}

/// The `type` of one block, when it is a non-empty string.
///
/// This is the one structural field every Block Kit block has, and the one `sanitize_blocks` checks
/// before it keeps a block at all (`if not isinstance(block, dict) or not block.get("type"):
/// continue`, block_kit.py:601-603). Slack answers a block without it with `invalid_blocks`, which
/// fails the whole message (block_kit.py:577-580), so the caller is told instead.
fn block_type(block: &Value) -> Option<&str> {
    block
        .get("type")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|block_type| !block_type.is_empty())
}

/// The `text` of one Block Kit text object (`{"type": "plain_text", "text": "…"}`), trimmed.
fn text_object_text(text_object: Option<&Value>) -> Option<&str> {
    text_object
        .and_then(|object| object.get("text"))
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|text| !text.is_empty())
}

/// A plain-text notification fallback derived from the card itself.
///
/// Every Block Kit payload should ship a `text` fallback: Slack uses it for the notification
/// preview, for screen readers and for old clients (block_kit.py:20-22), and the reference always
/// has the rendered mrkdwn string to hand over as "the notification/accessibility fallback"
/// (adapter.py:2289-2298). A card reaches this adapter without one, so the visible text objects
/// inside the blocks are read back out, in reading order:
///
/// - `block.text.text` — the text object a `header` or `section` block carries;
/// - `block.fields[*].text` — a `section`'s field columns;
/// - `block.elements[*].text` — a `context`'s elements.
///
/// Lines are joined with newlines and clamped to [`SLACK_MAX_MESSAGE_LENGTH`] (the same budget
/// `send` splits at), so one huge section cannot bloat the notification. An empty result means the
/// card carries no text at all — dividers, images or input elements only — and the caller posts the
/// blocks without a `text` field rather than inventing copy.
fn card_fallback_text(blocks: &[Value]) -> String {
    let mut lines: Vec<&str> = Vec::new();
    for block in blocks {
        if let Some(text) = text_object_text(block.get("text")) {
            lines.push(text);
        }
        for key in ["fields", "elements"] {
            let Some(items) = block.get(key).and_then(Value::as_array) else {
                continue;
            };
            for item in items {
                if let Some(text) = text_object_text(Some(item)) {
                    lines.push(text);
                }
            }
        }
    }
    let joined = lines.join("\n");
    truncate_message(joined.trim(), SLACK_MAX_MESSAGE_LENGTH, codepoint_len)
        .into_iter()
        .next()
        .unwrap_or_default()
}

/// The JSON type of `value`, so a refusal says what arrived instead of only what was wanted.
fn json_type_name(value: &Value) -> &'static str {
    match value {
        Value::Null => "null",
        Value::Bool(_) => "a boolean",
        Value::Number(_) => "a number",
        Value::String(_) => "a string",
        Value::Array(_) => "an array",
        Value::Object(_) => "an object",
    }
}

/// Slack's card-payload error codes → [`crate::platform::SEND_ERROR_KINDS`].
///
/// A card Slack cannot parse is a payload problem the caller owns and can fix, so the Block Kit
/// validation codes are `bad_format`; `invalid_blocks` is exactly the failure `sanitize_blocks`
/// exists to prevent, because it fails the WHOLE call rather than one block (block_kit.py:577-580).
/// Every other code keeps the text lane's table ([`error_kind`]).
fn card_error_kind(code: &str) -> &'static str {
    match code {
        "invalid_blocks"
        | "invalid_blocks_format"
        | "invalid_arguments"
        | "invalid_arg_name"
        | "invalid_array_arg"
        | "invalid_form_data"
        | "too_many_blocks"
        | "no_text" => "bad_format",
        other => error_kind(other),
    }
}

/// [`interpret`] for the card lane: the `ok` envelope decides, as everywhere else, and a bare HTTP
/// 400 — Slack's answer to a payload it could not parse — is `bad_format` rather than the
/// `transient` the generic status table picks for a 4xx it does not otherwise know (re-sending the
/// same malformed card cannot help).
fn interpret_card(response: &HttpResponse) -> Result<Value, ApiFailure> {
    match interpret_with(response, card_error_kind) {
        Err(mut failure) if response.status == 400 => {
            failure.kind = "bad_format";
            failure.retryable = false;
            failure.retry_after = None;
            Err(failure)
        }
        other => other,
    }
}

// ----- Native draft streaming: cursor glyphs and feature-gate markers -----

/// The trailing glyphs the stream consumer draws as a cursor (`_STREAM_CURSOR_GLYPHS`,
/// adapter.py:2461), read as the reference spells them: `U+2589` (▉), `U+258D` (▍), `U+258C` (▌)
/// and `U+2026` (…).
const STREAM_CURSOR_GLYPHS: [&str; 4] = ["\u{2589}", "\u{258d}", "\u{258c}", "\u{2026}"];

/// Slack error codes that mean this app/workspace cannot use the native stream at all
/// (`_NATIVE_STREAM_UNSUPPORTED_MARKERS`, adapter.py:2462-2464; the reference selects the
/// edit-based transport after the first one).
const NATIVE_STREAM_UNSUPPORTED_MARKERS: [&str; 8] = [
    "not_allowed",
    "missing_scope",
    "feature_not_enabled",
    "invalid_method",
    "unknown_method",
    "method_deprecated",
    "not_authed",
    "streaming_not_allowed",
];

/// The frame without the consumer's trailing cursor glyph (`_strip_stream_cursor`,
/// adapter.py:2477-2481).
///
/// The stream APIs are append-only, so a cursor glyph sent once would stay in the finished message
/// forever: it is stripped, and the whitespace it left behind with it. Only a glyph that is the
/// last non-whitespace character is removed; text that merely contains one travels untouched. When
/// no glyph trails the frame the original string is returned unchanged, exactly as the reference
/// does (it returns `text`, not the stripped copy, in that arm).
fn strip_stream_cursor(text: &str) -> String {
    let stripped = text.trim_end();
    match STREAM_CURSOR_GLYPHS
        .iter()
        .find(|glyph| stripped.ends_with(**glyph))
    {
        Some(glyph) => stripped[..stripped.len() - glyph.len()]
            .trim_end()
            .to_string(),
        None => text.to_string(),
    }
}

// ----- Reactions: emoji-name normalization -----

/// Aliases the reference folds onto a single glyph, read back into Slack's canonical name.
///
/// hermes' `_REACTION_EMOJI_MAP` (adapter.py:3842-3845) maps *names* to glyphs for inbound
/// reactions; two of its entries are two spellings of one glyph (`"thumbsup": "👍", "+1": "👍"`,
/// `"thumbsdown"`/`"-1"`, `"white_check_mark"`/`"heavy_check_mark"`). Outbound, the bot has to name
/// the emoji Slack's API knows, so the table below is that equivalency read backwards: every member
/// of a pair canonicalizes to the name the reference itself uses when it reacts (`white_check_mark`,
/// `x`, `eyes` — adapter.py:3297 / 3284). Names outside the table are not touched.
const SLACK_CANONICAL_EMOJI: &[(&str, &str)] = &[
    ("thumbsup", "thumbsup"),
    ("+1", "thumbsup"),
    ("thumbsdown", "thumbsdown"),
    ("-1", "thumbsdown"),
    ("white_check_mark", "white_check_mark"),
    ("heavy_check_mark", "white_check_mark"),
    ("x", "x"),
    ("no_entry", "no_entry"),
    ("warning", "warning"),
    ("rotating_light", "rotating_light"),
    ("eyes", "eyes"),
    ("rocket", "rocket"),
    ("tada", "tada"),
    ("fire", "fire"),
    ("wave", "wave"),
];

/// The glyph each canonical name renders as, so a caller may pass `👍` where a name is expected.
///
/// This is hermes' `_REACTION_EMOJI_MAP` with duplicates collapsed onto the canonical name; the
/// glyphs are stored without the `U+FE0F` variation selector so either spelling matches
/// ([`glyph_name`] strips it from the input).
const SLACK_EMOJI_GLYPHS: &[(&str, &str)] = &[
    ("thumbsup", "\u{1f44d}"),
    ("thumbsdown", "\u{1f44e}"),
    ("white_check_mark", "\u{2705}"),
    ("x", "\u{274c}"),
    ("no_entry", "\u{26d4}"),
    ("warning", "\u{26a0}"),
    ("rotating_light", "\u{1f6a8}"),
    ("eyes", "\u{1f440}"),
    ("rocket", "\u{1f680}"),
    ("tada", "\u{1f389}"),
    ("fire", "\u{1f525}"),
    ("wave", "\u{1f44b}"),
];

/// The `name` value `reactions.add` / `reactions.remove` should send for a caller's emoji.
///
/// The reference passes its own constant names straight through (`"eyes"`,
/// `"white_check_mark"` — adapter.py:3284 / 3297) and strips colons wherever it compares a name it
/// read back from Slack (adapter.py:3887 / 3975, `adapter_reactions_context.py:35`). This port
/// accepts all three shapes a caller may plausibly hold, in this order:
///
/// 1. a glyph (`👍`, with or without `U+FE0F`) becomes its name — the reference's map read
///    backwards;
/// 2. surrounding colons are stripped (`:eyes:` → `eyes`) and the result is trimmed;
/// 3. an alias collapses to Slack's canonical name ([`SLACK_CANONICAL_EMOJI`]): `+1` → `thumbsup`,
///    `heavy_check_mark` → `white_check_mark`. A skin-tone suffix (`thumbsup::skin-tone-2`) keeps
///    its base name as written — the suffix is part of the name Slack matches, not an alias.
///
/// `None` means "send nothing": blank input, colons only, or a name carrying a character Slack's
/// name grammar has no room for (the reference's own character class for a name is word characters
/// plus `+`, `'`, `-` and the `:` that separates a skin tone —
/// `adapter_reactions_context.py:21`).
pub fn slack_reaction_name(emoji: &str) -> Option<String> {
    let trimmed = emoji.trim();
    if trimmed.is_empty() {
        return None;
    }
    if let Some(name) = glyph_name(trimmed) {
        return Some(name.to_string());
    }
    let name = trimmed.trim_matches(':').trim();
    if name.is_empty() {
        return None;
    }
    let spells_a_name = name
        .chars()
        .all(|ch| ch.is_alphanumeric() || matches!(ch, '+' | '-' | '_' | '\'' | ':'));
    if !spells_a_name {
        return None;
    }
    Some(canonical_emoji_name(name).to_string())
}

/// Slack's canonical name for a glyph (`👍` → `thumbsup`), ignoring a trailing `U+FE0F`.
fn glyph_name(glyph: &str) -> Option<&'static str> {
    let bare = glyph.trim_end_matches('\u{fe0f}');
    SLACK_EMOJI_GLYPHS
        .iter()
        .find(|(_, candidate)| *candidate == bare)
        .map(|(name, _)| *name)
}

/// Slack's canonical name for a name or alias (`+1` → `thumbsup`), else `name` unchanged.
fn canonical_emoji_name(name: &str) -> &str {
    // A skin-tone suffix is a matched part of the name, not an alias of one.
    if name.contains(':') {
        return name;
    }
    let lowered = name.to_ascii_lowercase();
    SLACK_CANONICAL_EMOJI
        .iter()
        .find(|(alias, _)| *alias == lowered)
        .map(|(_, canonical)| *canonical)
        .unwrap_or(name)
}

// ----- Slack Web API reply interpretation -----

/// One failed Web API call, before it becomes a [`SendResult`].
///
/// Kept separate so failures stay small on the stack and the mapping to the send taxonomy lives
/// in exactly one place ([`ApiFailure::into_send_result`]).
struct ApiFailure {
    message: String,
    kind: &'static str,
    retryable: bool,
    retry_after: Option<f64>,
}

impl ApiFailure {
    fn new(message: impl Into<String>, kind: &'static str) -> Self {
        Self {
            message: message.into(),
            kind,
            retryable: false,
            retry_after: None,
        }
    }

    fn retryable(mut self, retry_after: Option<f64>) -> Self {
        self.retryable = true;
        self.retry_after = retry_after;
        self
    }

    fn into_send_result(self) -> SendResult {
        let result = SendResult::failed(self.message, Some(self.kind));
        if self.retryable {
            result.retryable(self.retry_after)
        } else {
            result
        }
    }
}

/// Judge a Slack reply: transport-level status first, then the `ok` field of the envelope.
fn interpret(response: &HttpResponse) -> Result<Value, ApiFailure> {
    interpret_with(response, error_kind)
}

/// [`interpret`] with a caller-supplied error-code table.
///
/// The file endpoints answer with codes the text lane never sees (`file_not_found`,
/// `file_uploads_disabled`, …), so they classify through [`upload_error_kind`] instead of
/// [`error_kind`]; everything else about the envelope is judged identically.
fn interpret_with(
    response: &HttpResponse,
    kinds: fn(&str) -> &'static str,
) -> Result<Value, ApiFailure> {
    let body = match response.json() {
        Ok(body) => body,
        Err(err) => {
            return Err(ApiFailure::new(
                format!("slack reply was not json: {err}"),
                "unknown",
            ));
        }
    };
    if !response.ok() {
        return Err(http_failure(response, &body));
    }
    if body.get("ok").and_then(Value::as_bool) == Some(true) {
        return Ok(body);
    }
    let code = body
        .get("error")
        .and_then(Value::as_str)
        .unwrap_or("unknown_error");
    let failure = ApiFailure::new(format!("slack api error: {code}"), kinds(code));
    if code == "ratelimited" {
        return Err(failure.retryable(response.retry_after));
    }
    Err(failure)
}

/// Map a non-2xx status to the send taxonomy. hermes never sees these as statuses (the SDK raises
/// on them); the classification mirrors how the bot should react.
fn http_failure(response: &HttpResponse, body: &Value) -> ApiFailure {
    let detail = body
        .get("error")
        .and_then(Value::as_str)
        .map(str::to_string)
        .unwrap_or_else(|| format!("http {}", response.status));
    let message = format!("slack api error: {detail}");
    match response.status {
        429 => ApiFailure::new(message, "rate_limited").retryable(response.retry_after),
        401 | 403 => ApiFailure::new(message, "forbidden"),
        404 => ApiFailure::new(message, "not_found"),
        _ => ApiFailure::new(message, "transient").retryable(response.retry_after),
    }
}

/// Slack's `error` codes → [`crate::platform::SEND_ERROR_KINDS`].
fn error_kind(code: &str) -> &'static str {
    match code {
        "ratelimited" => "rate_limited",
        "channel_not_found" => "not_found",
        "not_in_channel" | "is_archived" | "not_authed" | "invalid_auth" | "token_revoked" => {
            "forbidden"
        }
        "msg_too_long" => "too_long",
        _ => "unknown",
    }
}

/// `ts` of a posted message: top-level on `chat.postMessage`, nested under `message` on updates.
fn message_ts(body: &Value) -> Option<String> {
    body.get("ts")
        .and_then(Value::as_str)
        .or_else(|| body.get("message")?.get("ts")?.as_str())
        .map(str::to_string)
}

/// Is `field` present and `true`? (Slack sends `false` for the kinds it does not apply.)
fn flag(object: &Value, field: &str) -> bool {
    object.get(field).and_then(Value::as_bool) == Some(true)
}

/// Percent-encode one query-string value (channel ids are usually alphanumeric, but never assume).
fn encode_query_value(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    for byte in value.bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(byte as char)
            }
            other => out.push_str(&format!("%{other:02X}")),
        }
    }
    out
}

// ----- Markdown → mrkdwn -----

/// Convert standard Markdown to Slack mrkdwn.
///
/// `**bold**` becomes `*bold*`, `__italic__` and `*italic*` become `_italic_`, `[label](url)`
/// becomes `<url|label>` and an ATX heading (`# heading`) becomes `*heading*`. Fenced code blocks
/// are carried through verbatim — conversion never rewrites code.
pub fn markdown_to_slack(text: &str) -> String {
    if text.is_empty() {
        return String::new();
    }
    let mut stash = Stash::default();
    // Order matters: protect code, then headings (which swallow their own emphasis), then links,
    // then emphasis from strongest to weakest so `**` is gone before single `*` runs.
    let protected = protect_fences(text, &mut stash);
    let mut out = convert_headings(&protected, &mut stash);
    out = convert_links(&out, &mut stash);
    out = convert_emphasis(&out, "**", "*", "*", &mut stash);
    out = convert_emphasis(&out, "__", "_", "_", &mut stash);
    out = convert_emphasis(&out, "*", "_", "_", &mut stash);
    stash.restore(&out)
}

/// Converted spans hidden behind `\u{0}SL<n>\u{0}` keys so later passes cannot touch them.
#[derive(Default)]
struct Stash {
    items: Vec<String>,
}

impl Stash {
    /// Hide `value` and return the placeholder that stands in for it.
    fn store(&mut self, value: String) -> String {
        let key = format!("\u{0}SL{}\u{0}", self.items.len());
        self.items.push(value);
        key
    }

    /// Replace every placeholder in `text` with the span it hides.
    fn restore(&self, text: &str) -> String {
        let mut out = String::with_capacity(text.len());
        let mut rest = text;
        while let Some(start) = rest.find('\u{0}') {
            out.push_str(&rest[..start]);
            let after = &rest[start + 1..];
            if let Some(tail) = after.strip_prefix("SL")
                && let Some((index, consumed)) = parse_placeholder_tail(tail)
            {
                match self.items.get(index) {
                    Some(value) => out.push_str(value),
                    None => out.push('\u{0}'),
                }
                rest = &tail[consumed..];
                continue;
            }
            out.push('\u{0}');
            rest = after;
        }
        out.push_str(rest);
        out
    }
}

/// `SL<digits>\u{0}` tail → `(index, bytes consumed from the front of the tail)`.
fn parse_placeholder_tail(tail: &str) -> Option<(usize, usize)> {
    let digits = tail.chars().take_while(char::is_ascii_digit).count();
    if digits == 0 || !tail[digits..].starts_with('\u{0}') {
        return None;
    }
    let index = tail[..digits].parse::<usize>().ok()?;
    Some((index, digits + 1))
}

/// Move every fenced code block behind a placeholder, terminator included.
fn protect_fences(text: &str, stash: &mut Stash) -> String {
    let mut out = String::with_capacity(text.len());
    let mut block = String::new();
    let mut open: Option<(char, usize)> = None;
    for line in text.split_inclusive('\n') {
        let (body, _) = split_line_terminator(line);
        match open {
            Some((marker, run)) => {
                block.push_str(line);
                if is_fence_close(body, marker, run) {
                    out.push_str(&stash.store(std::mem::take(&mut block)));
                    open = None;
                }
            }
            None => match fence_open(body) {
                Some(fence) => {
                    open = Some(fence);
                    block.push_str(line);
                }
                None => out.push_str(line),
            },
        }
    }
    // An unterminated fence is not a code block; leave it exactly as it came in.
    out.push_str(&block);
    out
}

/// Split a line into its content and its `\r?\n` terminator (`""` on the last line).
fn split_line_terminator(line: &str) -> (&str, &str) {
    let content = line.trim_end_matches(&['\n', '\r'][..]);
    (content, &line[content.len()..])
}

/// A fence opener: at least three backticks or tildes, leading whitespace ignored.
fn fence_open(line: &str) -> Option<(char, usize)> {
    let body = line.trim_start();
    let marker = body.chars().next()?;
    if marker != '`' && marker != '~' {
        return None;
    }
    let run = body.chars().take_while(|ch| *ch == marker).count();
    if run < 3 {
        return None;
    }
    // CommonMark forbids a backtick in a backtick fence's info string.
    if marker == '`' && body[run..].contains('`') {
        return None;
    }
    Some((marker, run))
}

/// Is `line` the closing fence of a block opened with `marker` repeated `run` times?
fn is_fence_close(line: &str, marker: char, run: usize) -> bool {
    let body = line.trim_start();
    let closing = body.chars().take_while(|ch| *ch == marker).count();
    closing >= run && body.chars().skip(closing).all(char::is_whitespace)
}

/// Rewrite ATX heading lines (`# Title`) as bold, since mrkdwn has no heading level.
fn convert_headings(text: &str, stash: &mut Stash) -> String {
    let mut out = String::with_capacity(text.len());
    for line in text.split_inclusive('\n') {
        let (content, terminator) = split_line_terminator(line);
        match heading_content(content) {
            Some(heading) => {
                // A heading already renders bold; drop the inner ** so it does not double up.
                let heading = format!("*{}*", heading.replace("**", ""));
                out.push_str(&stash.store(heading));
                out.push_str(terminator);
            }
            None => out.push_str(line),
        }
    }
    out
}

/// Content of an ATX heading line (`#`…`######` followed by a space), or `None`.
fn heading_content(line: &str) -> Option<&str> {
    let hashes = line.chars().take_while(|ch| *ch == '#').count();
    if !(1..=6).contains(&hashes) {
        return None;
    }
    let rest = line.get(hashes..)?.strip_prefix(' ')?;
    let heading = rest.trim();
    if heading.is_empty() {
        None
    } else {
        Some(heading)
    }
}

/// Rewrite `[label](url)` as mrkdwn `<url|label>`.
fn convert_links(text: &str, stash: &mut Stash) -> String {
    let mut out = String::with_capacity(text.len());
    let mut pos = 0usize;
    while pos < text.len() {
        let Some(rel) = text[pos..].find('[') else {
            break;
        };
        let start = pos + rel;
        // `![alt](url)` is an image reference, not a link.
        let is_image = text[..start].ends_with('!');
        let label_start = start + 1;
        let Some(label_rel) = text[label_start..].find(']') else {
            break;
        };
        let label_end = label_start + label_rel;
        let url_start = label_end + 1;
        let target = text[url_start..].strip_prefix('(').and_then(|inner| {
            inner
                .find(')')
                .map(|close| (url_start + 1, url_start + 1 + close))
        });
        if let Some((url_from, url_to)) = target {
            let label = &text[label_start..label_end];
            let url = text[url_from..url_to].trim();
            let usable = !is_image
                && !label.is_empty()
                && !url.is_empty()
                && !label.contains('\n')
                && !url.contains('\n');
            if usable {
                out.push_str(&text[pos..start]);
                out.push_str(&stash.store(format!("<{url}|{label}>")));
                pos = url_to + 1;
                continue;
            }
        }
        out.push_str(&text[pos..=label_end]);
        pos = label_end + 1;
    }
    out.push_str(&text[pos..]);
    out
}

/// Wrap every `delim…delim` span in `open`…`close`, hiding the result behind a placeholder.
///
/// A span only counts when its content is non-empty, stays on one line and has a non-whitespace
/// character touching each delimiter — the same rule hermes uses so `a * b * c` survives.
fn convert_emphasis(text: &str, delim: &str, open: &str, close: &str, stash: &mut Stash) -> String {
    let mut out = String::with_capacity(text.len());
    let mut pos = 0usize;
    while pos < text.len() {
        let Some(rel) = text[pos..].find(delim) else {
            break;
        };
        let start = pos + rel;
        let content_start = start + delim.len();
        let Some(content_rel) = text[content_start..].find(delim) else {
            break;
        };
        let content_end = content_start + content_rel;
        let inner = &text[content_start..content_end];
        out.push_str(&text[pos..start]);
        if is_emphasizable(inner) {
            out.push_str(&stash.store(format!("{open}{inner}{close}")));
            pos = content_end + delim.len();
        } else {
            out.push_str(&text[start..content_start]);
            pos = content_start;
        }
    }
    out.push_str(&text[pos..]);
    out
}

/// Non-empty, single-line content with non-whitespace at both ends.
fn is_emphasizable(inner: &str) -> bool {
    let mut chars = inner.chars();
    let Some(first) = chars.next() else {
        return false;
    };
    let last = inner.chars().next_back().unwrap_or(first);
    !first.is_whitespace() && !last.is_whitespace() && !inner.contains('\n')
}

/// HMAC-SHA-256, hex encoded. Slack signs `v0:{timestamp}:{body}` with its signing secret.
fn hmac_sha256_hex(key: &[u8], message: &[u8]) -> String {
    let mut block = [0u8; 64];
    if key.len() > 64 {
        block[..32].copy_from_slice(&crate::pairing::sha256::digest(key));
    } else {
        block[..key.len()].copy_from_slice(key);
    }
    let mut inner_pad = [0x36u8; 64];
    let mut outer_pad = [0x5cu8; 64];
    for i in 0..64 {
        inner_pad[i] ^= block[i];
        outer_pad[i] ^= block[i];
    }
    let mut inner = inner_pad.to_vec();
    inner.extend_from_slice(message);
    let inner_hash = crate::pairing::sha256::digest(&inner);
    let mut outer = outer_pad.to_vec();
    outer.extend_from_slice(&inner_hash);
    crate::pairing::sha256::digest(&outer)
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    let mut diff = 0u8;
    for (x, y) in a.iter().zip(b.iter()) {
        diff |= x ^ y;
    }
    diff == 0
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::transport::{HttpMethod, Transport, TransportError};
    use async_trait::async_trait;
    use std::sync::{Arc, Mutex};

    /// A transport that records every request and replays canned replies in order.
    #[derive(Default)]
    struct MockTransport {
        requests: Mutex<Vec<HttpRequest>>,
        replies: Mutex<Vec<HttpResponse>>,
    }

    impl MockTransport {
        fn recorded(&self) -> Vec<HttpRequest> {
            self.requests
                .lock()
                .map(|guard| guard.clone())
                .unwrap_or_default()
        }
    }

    #[async_trait]
    impl Transport for MockTransport {
        async fn execute(&self, request: HttpRequest) -> Result<HttpResponse, TransportError> {
            if let Ok(mut requests) = self.requests.lock() {
                requests.push(request);
            }
            let reply = match self.replies.lock() {
                Ok(mut replies) if !replies.is_empty() => replies.remove(0),
                _ => slack_ok(json!({ "ok": true, "ts": "1500000000.000100" })),
            };
            Ok(reply)
        }
    }

    /// A 200 reply with a JSON body.
    fn slack_ok(body: Value) -> HttpResponse {
        HttpResponse {
            status: 200,
            body: body.to_string(),
            retry_after: None,
        }
    }

    /// A 200 reply carrying Slack's failure envelope (`{"ok": false, "error": …}`).
    fn slack_error(code: &str) -> HttpResponse {
        HttpResponse {
            status: 200,
            body: json!({ "ok": false, "error": code }).to_string(),
            retry_after: None,
        }
    }

    /// `(adapter, mock)` with the adapter wired to the recording transport.
    fn adapter_with(replies: Vec<HttpResponse>) -> (SlackAdapter, Arc<MockTransport>) {
        let mock = Arc::new(MockTransport {
            requests: Mutex::new(Vec::new()),
            replies: Mutex::new(replies),
        });
        (SlackAdapter::new(mock.clone(), "xoxb-test"), mock)
    }

    /// The JSON body of a recorded request (`Null` when it has none or is not JSON).
    fn body_of(request: &HttpRequest) -> Value {
        match request.body.as_deref().map(serde_json::from_slice::<Value>) {
            Some(Ok(value)) => value,
            _ => Value::Null,
        }
    }

    fn has_bearer(request: &HttpRequest, token: &str) -> bool {
        request
            .headers
            .iter()
            .any(|(name, value)| name == "Authorization" && value == &format!("Bearer {token}"))
    }

    /// A reply with an explicit status and a raw (non-envelope) body, e.g. the upload receipt.
    fn slack_text(status: u16, body: &str) -> HttpResponse {
        HttpResponse {
            status,
            body: body.to_string(),
            retry_after: None,
        }
    }

    /// `files.getUploadURLExternal` success: the pre-signed URL plus the file id it reserved.
    fn upload_url_reply(url: &str, file_id: &str) -> HttpResponse {
        slack_ok(json!({ "ok": true, "upload_url": url, "file_id": file_id }))
    }

    /// `files.completeUploadExternal` success carrying the shared message `ts` for `channel`.
    fn completion_reply(file_id: &str, title: &str, channel: &str, ts: &str) -> HttpResponse {
        slack_ok(json!({
            "ok": true,
            "files": [{
                "id": file_id,
                "title": title,
                "shares": { "public": { channel: [{ "ts": ts }] } }
            }]
        }))
    }

    /// `Content-Type` of a recorded request (`""` when absent).
    fn content_type(request: &HttpRequest) -> &str {
        request
            .headers
            .iter()
            .find(|(name, _)| name.eq_ignore_ascii_case("content-type"))
            .map(|(_, value)| value.as_str())
            .unwrap_or_default()
    }

    /// Multipart boundary advertised by a recorded request (`""` when it is not multipart).
    fn boundary_of(request: &HttpRequest) -> String {
        content_type(request)
            .strip_prefix("multipart/form-data; boundary=")
            .unwrap_or_default()
            .to_string()
    }

    /// Decode a recorded `application/x-www-form-urlencoded` body into ordered `(name, value)` pairs.
    fn form_pairs(request: &HttpRequest) -> Vec<(String, String)> {
        let Some(body) = request.body.as_deref() else {
            return Vec::new();
        };
        String::from_utf8_lossy(body)
            .split('&')
            .filter(|pair| !pair.is_empty())
            .map(|pair| match pair.split_once('=') {
                Some((name, value)) => (percent_decode(name), percent_decode(value)),
                None => (percent_decode(pair), String::new()),
            })
            .collect()
    }

    /// Percent-decode one form component (`%XX` and `+`), leaving malformed escapes as they are.
    fn percent_decode(value: &str) -> String {
        let bytes = value.as_bytes();
        let mut out: Vec<u8> = Vec::with_capacity(bytes.len());
        let mut index = 0;
        while index < bytes.len() {
            match bytes[index] {
                b'%' if index + 2 < bytes.len() => {
                    let hex = std::str::from_utf8(&bytes[index + 1..index + 3]).unwrap_or_default();
                    match u8::from_str_radix(hex, 16) {
                        Ok(byte) => {
                            out.push(byte);
                            index += 3;
                        }
                        Err(_) => {
                            out.push(bytes[index]);
                            index += 1;
                        }
                    }
                }
                b'+' => {
                    out.push(b' ');
                    index += 1;
                }
                other => {
                    out.push(other);
                    index += 1;
                }
            }
        }
        String::from_utf8_lossy(&out).to_string()
    }

    /// Bytes-backed media with no filename, MIME type or caption.
    fn bytes_media(kind: MediaKind, bytes: &[u8]) -> OutboundMedia {
        OutboundMedia::new(kind, MediaSource::Bytes(bytes.to_vec()))
    }

    #[tokio::test]
    async fn connect_true_only_when_auth_test_says_ok() {
        let (mut adapter, mock) = adapter_with(vec![slack_ok(json!({
            "ok": true,
            "team": "acme",
            "user": "vikram",
            "bot_id": "B1"
        }))]);
        assert!(adapter.connect(false).await);

        let requests = mock.recorded();
        assert_eq!(requests.len(), 1);
        assert_eq!(requests[0].method, HttpMethod::Post);
        assert_eq!(requests[0].url, "https://slack.com/api/auth.test");
        assert!(has_bearer(&requests[0], "xoxb-test"));
    }

    #[tokio::test]
    async fn connect_is_false_when_auth_test_fails() {
        let (mut adapter, _mock) = adapter_with(vec![slack_error("invalid_auth")]);
        assert!(!adapter.connect(false).await);

        let (mut adapter, _mock) = adapter_with(vec![slack_ok(json!({ "ok": false }))]);
        assert!(!adapter.connect(true).await);
    }

    #[tokio::test]
    async fn post_message_request_shape() {
        let (adapter, mock) = adapter_with(vec![slack_ok(json!({ "ok": true, "ts": "1.1" }))]);
        let result = adapter.send("C123", "hello **world**", None, None).await;

        assert!(result.success, "{result:?}");
        assert_eq!(result.message_id.as_deref(), Some("1.1"));
        assert!(result.error_kind.is_none());

        let requests = mock.recorded();
        assert_eq!(requests.len(), 1);
        assert_eq!(requests[0].method, HttpMethod::Post);
        assert_eq!(requests[0].url, "https://slack.com/api/chat.postMessage");
        assert!(has_bearer(&requests[0], "xoxb-test"));
        assert_eq!(
            requests[0]
                .headers
                .iter()
                .find(|(name, _)| name == "Content-Type")
                .map(|(_, value)| value.as_str()),
            Some("application/json")
        );

        let body = body_of(&requests[0]);
        assert_eq!(body["channel"], "C123");
        assert_eq!(body["text"], "hello *world*");
        assert_eq!(body["mrkdwn"], true);
        assert!(
            body.get("thread_ts").is_none(),
            "a top-level send must not open a thread: {body}"
        );
    }

    #[tokio::test]
    async fn reply_to_becomes_thread_ts() {
        let (adapter, mock) = adapter_with(vec![slack_ok(json!({ "ok": true, "ts": "1.2" }))]);
        let result = adapter
            .send("C123", "in thread", Some("1500000000.000100"), None)
            .await;

        assert!(result.success);
        assert_eq!(
            body_of(&mock.recorded()[0])["thread_ts"],
            "1500000000.000100"
        );
    }

    #[tokio::test]
    async fn long_content_is_chunked_at_the_budget() {
        let replies = (1..=3)
            .map(|index| slack_ok(json!({ "ok": true, "ts": format!("ts{index}") })))
            .collect();
        let (adapter, mock) = adapter_with(replies);
        let content = "a".repeat(9000);
        let result = adapter.send("C123", &content, None, None).await;

        assert!(result.success, "{result:?}");
        let requests = mock.recorded();
        assert_eq!(requests.len(), 3, "9000 chars must split into 3 chunks");
        let mut total = 0usize;
        for request in &requests {
            let body = body_of(request);
            let text = body["text"].as_str().unwrap_or_default();
            assert!(
                text.chars().count() <= SLACK_MAX_MESSAGE_LENGTH,
                "chunk exceeded the budget: {} chars",
                text.chars().count()
            );
            total += text.chars().count();
        }
        assert_eq!(total, 9000, "chunking must not lose content");
        assert_eq!(result.message_id.as_deref(), Some("ts3"));
        assert_eq!(result.continuation_message_ids, vec!["ts1", "ts2", "ts3"]);
    }

    #[tokio::test]
    async fn ratelimited_is_retryable_with_retry_after() {
        let reply = HttpResponse {
            status: 200,
            body: json!({ "ok": false, "error": "ratelimited" }).to_string(),
            retry_after: Some(3.0),
        };
        let (adapter, _mock) = adapter_with(vec![reply]);
        let result = adapter.send("C123", "hello", None, None).await;

        assert!(!result.success);
        assert!(result.retryable);
        assert_eq!(result.retry_after, Some(3.0));
        assert_eq!(result.error_kind.as_deref(), Some("rate_limited"));
        assert!(
            result
                .error
                .as_deref()
                .unwrap_or_default()
                .contains("ratelimited")
        );
    }

    #[tokio::test]
    async fn channel_not_found_maps_to_not_found() {
        let (adapter, _mock) = adapter_with(vec![slack_error("channel_not_found")]);
        let result = adapter.send("C404", "hello", None, None).await;

        assert!(!result.success);
        assert!(!result.retryable);
        assert_eq!(result.error_kind.as_deref(), Some("not_found"));
        assert!(result.message_id.is_none());
    }

    #[tokio::test]
    async fn slack_error_codes_map_to_the_send_taxonomy() {
        assert_eq!(error_kind("ratelimited"), "rate_limited");
        assert_eq!(error_kind("channel_not_found"), "not_found");
        assert_eq!(error_kind("not_in_channel"), "forbidden");
        assert_eq!(error_kind("is_archived"), "forbidden");
        assert_eq!(error_kind("not_authed"), "forbidden");
        assert_eq!(error_kind("invalid_auth"), "forbidden");
        assert_eq!(error_kind("token_revoked"), "forbidden");
        assert_eq!(error_kind("msg_too_long"), "too_long");
        assert_eq!(error_kind("something_else"), "unknown");
    }

    #[tokio::test]
    async fn empty_content_is_rejected_without_a_request() {
        let (adapter, mock) = adapter_with(Vec::new());
        let result = adapter.send("C123", "   \n\t ", None, None).await;

        assert!(!result.success);
        assert_eq!(result.error_kind.as_deref(), Some("bad_format"));
        assert!(
            mock.recorded().is_empty(),
            "an empty message must not hit the wire"
        );
    }

    #[test]
    fn markdown_emphasis_conversions() {
        assert_eq!(markdown_to_slack("**bold**"), "*bold*");
        assert_eq!(markdown_to_slack("*italic*"), "_italic_");
        assert_eq!(markdown_to_slack("__italic__"), "_italic_");
        assert_eq!(
            markdown_to_slack("**bold** then *italic* then __also italic__"),
            "*bold* then _italic_ then _also italic_"
        );
        // Whitespace touching a delimiter means it is not emphasis.
        assert_eq!(markdown_to_slack("a * b * c"), "a * b * c");
        assert_eq!(markdown_to_slack("2*3*4"), "2_3_4");
    }

    #[test]
    fn markdown_links_and_headings() {
        assert_eq!(
            markdown_to_slack("[docs](https://example.com/a)"),
            "<https://example.com/a|docs>"
        );
        assert_eq!(
            markdown_to_slack("see [a](http://x.y) now"),
            "see <http://x.y|a> now"
        );
        assert_eq!(markdown_to_slack("# Title"), "*Title*");
        assert_eq!(markdown_to_slack("### **Bold** title"), "*Bold title*");
        assert_eq!(
            markdown_to_slack("# Head\nbody **strong**"),
            "*Head*\nbody *strong*"
        );
        // Not headings: too many hashes, or no space after them.
        assert_eq!(markdown_to_slack("####### nope"), "####### nope");
        assert_eq!(markdown_to_slack("#nope"), "#nope");
        // An image reference is not a link.
        assert_eq!(
            markdown_to_slack("![alt](http://x/y.png)"),
            "![alt](http://x/y.png)"
        );
    }

    #[test]
    fn markdown_leaves_fenced_code_untouched() {
        let fenced = "```\n**not bold** *not italic*\n# not a heading\n```";
        assert_eq!(markdown_to_slack(fenced), fenced);

        let mixed = "before **x**\n```rust\nlet a = **b**;\n```\nafter *y*";
        assert_eq!(
            markdown_to_slack(mixed),
            "before *x*\n```rust\nlet a = **b**;\n```\nafter _y_"
        );

        // An unterminated fence is not a code block, so it is ordinary text.
        assert_eq!(
            markdown_to_slack("```\nunclosed **x**"),
            "```\nunclosed *x*"
        );
    }

    #[tokio::test]
    async fn edit_message_posts_chat_update() {
        let (adapter, mock) = adapter_with(vec![slack_ok(json!({ "ok": true, "ts": "1.5" }))]);
        let result = adapter
            .edit_message("C123", "1.5", "now **final**", true)
            .await;

        assert!(result.success, "{result:?}");
        assert_eq!(result.message_id.as_deref(), Some("1.5"));

        let requests = mock.recorded();
        assert_eq!(requests.len(), 1);
        assert_eq!(requests[0].url, "https://slack.com/api/chat.update");
        assert!(has_bearer(&requests[0], "xoxb-test"));
        let body = body_of(&requests[0]);
        assert_eq!(body["channel"], "C123");
        assert_eq!(body["ts"], "1.5");
        assert_eq!(body["text"], "now *final*");
    }

    #[tokio::test]
    async fn edit_message_maps_api_failures() {
        let (adapter, _mock) = adapter_with(vec![slack_error("msg_too_long")]);
        let result = adapter.edit_message("C123", "1.5", "x", false).await;
        assert!(!result.success);
        assert_eq!(result.error_kind.as_deref(), Some("too_long"));
    }

    #[tokio::test]
    async fn delete_message_ignores_the_result() {
        let (adapter, mock) = adapter_with(vec![slack_error("message_not_found")]);
        adapter.delete_message("C123", "1.9").await;

        let requests = mock.recorded();
        assert_eq!(requests.len(), 1);
        assert_eq!(requests[0].url, "https://slack.com/api/chat.delete");
        let body = body_of(&requests[0]);
        assert_eq!(body["channel"], "C123");
        assert_eq!(body["ts"], "1.9");
    }

    #[tokio::test]
    async fn conversations_info_maps_a_dm() {
        let (adapter, mock) = adapter_with(vec![slack_ok(json!({
            "ok": true,
            "channel": { "id": "D123", "is_im": true, "user": "U1" }
        }))]);
        let info = adapter.get_chat_info("D123").await;

        assert_eq!(info.chat_type, "dm");
        assert_eq!(info.name, "D123");

        let requests = mock.recorded();
        assert_eq!(requests.len(), 1);
        assert_eq!(requests[0].method, HttpMethod::Get);
        assert_eq!(
            requests[0].url,
            "https://slack.com/api/conversations.info?channel=D123"
        );
        assert!(has_bearer(&requests[0], "xoxb-test"));
    }

    #[tokio::test]
    async fn conversations_info_maps_a_channel_and_a_group_dm() {
        let (adapter, _mock) = adapter_with(vec![slack_ok(json!({
            "ok": true,
            "channel": { "id": "C1", "name": "general" }
        }))]);
        let info = adapter.get_chat_info("C1").await;
        assert_eq!(info.name, "#general");
        assert_eq!(info.chat_type, "channel");

        let (adapter, _mock) = adapter_with(vec![slack_ok(json!({
            "ok": true,
            "channel": { "id": "G1", "name": "mpdm-a--b-1", "is_mpim": true }
        }))]);
        let info = adapter.get_chat_info("G1").await;
        assert_eq!(info.chat_type, "group");
        assert_eq!(info.name, "#mpdm-a--b-1");
    }

    #[tokio::test]
    async fn conversations_info_falls_back_on_failure() {
        let (adapter, _mock) = adapter_with(vec![slack_error("channel_not_found")]);
        let info = adapter.get_chat_info("C404").await;
        assert_eq!(info.name, "C404");
        assert_eq!(info.chat_type, "channel");
    }

    #[tokio::test]
    async fn send_typing_is_a_noop() {
        let (adapter, mock) = adapter_with(Vec::new());
        adapter.send_typing("C123", None).await;
        assert!(mock.recorded().is_empty());
    }

    // ----- reactions: reactions.add / reactions.remove -----

    #[test]
    fn slack_supports_reactions() {
        let (adapter, mock) = adapter_with(Vec::new());
        assert!(adapter.supports_reactions());
        assert!(
            mock.recorded().is_empty(),
            "the flag is a capability, not a call"
        );
    }

    #[tokio::test]
    async fn add_reaction_posts_reactions_add_with_the_reference_arguments() {
        let (adapter, mock) = adapter_with(vec![slack_ok(json!({ "ok": true }))]);
        assert!(
            adapter
                .add_reaction("C123", "1500000000.000100", ":white_check_mark:")
                .await
        );

        let requests = mock.recorded();
        assert_eq!(requests.len(), 1);
        assert_eq!(requests[0].method, HttpMethod::Post);
        assert_eq!(requests[0].url, "https://slack.com/api/reactions.add");
        assert!(has_bearer(&requests[0], "xoxb-test"));
        let body = body_of(&requests[0]);
        assert_eq!(body["channel"], "C123");
        assert_eq!(body["timestamp"], "1500000000.000100");
        assert_eq!(body["name"], "white_check_mark");
        assert_eq!(
            body.as_object().map(|object| object.len()),
            Some(3),
            "the reference sends exactly channel/timestamp/name: {body}"
        );
    }

    #[tokio::test]
    async fn remove_reaction_posts_reactions_remove() {
        let (adapter, mock) = adapter_with(vec![slack_ok(json!({ "ok": true }))]);
        assert!(
            adapter
                .remove_reaction("C123", "1500000000.000100", "+1")
                .await
        );

        let requests = mock.recorded();
        assert_eq!(requests.len(), 1);
        assert_eq!(requests[0].method, HttpMethod::Post);
        assert_eq!(requests[0].url, "https://slack.com/api/reactions.remove");
        assert!(has_bearer(&requests[0], "xoxb-test"));
        let body = body_of(&requests[0]);
        assert_eq!(body["channel"], "C123");
        assert_eq!(body["timestamp"], "1500000000.000100");
        assert_eq!(body["name"], "thumbsup");
    }

    #[tokio::test]
    async fn reaction_failures_are_false_and_never_a_silent_success() {
        // Slack answers HTTP 200 with `ok: false` for every reaction failure it knows.
        for reply in [
            slack_error("already_reacted"),
            slack_error("missing_scope"),
            slack_ok(json!({ "ok": false })),
            HttpResponse {
                status: 200,
                body: json!({ "ok": false, "error": "ratelimited" }).to_string(),
                retry_after: Some(2.0),
            },
            slack_text(500, "upstream error"),
        ] {
            let (adapter, mock) = adapter_with(vec![reply]);
            assert!(!adapter.add_reaction("C123", "1.1", "eyes").await);
            assert_eq!(mock.recorded().len(), 1);
        }

        let (adapter, mock) = adapter_with(vec![slack_error("no_reaction")]);
        assert!(!adapter.remove_reaction("C123", "1.1", "eyes").await);
        assert_eq!(mock.recorded().len(), 1);
    }

    #[tokio::test]
    async fn blank_reaction_inputs_make_no_request() {
        let (adapter, mock) = adapter_with(Vec::new());
        for (chat_id, message_id, emoji) in [
            ("", "1.1", "eyes"),
            ("   ", "1.1", "eyes"),
            ("C123", "", "eyes"),
            ("C123", " \t", "eyes"),
            ("C123", "1.1", ""),
            ("C123", "1.1", "   "),
            ("C123", "1.1", "::"),
        ] {
            assert!(
                !adapter.add_reaction(chat_id, message_id, emoji).await,
                "add {chat_id:?}/{message_id:?}/{emoji:?}"
            );
            assert!(
                !adapter.remove_reaction(chat_id, message_id, emoji).await,
                "remove {chat_id:?}/{message_id:?}/{emoji:?}"
            );
        }
        assert!(
            mock.recorded().is_empty(),
            "blank input must not hit the wire"
        );
    }

    #[test]
    fn reaction_names_are_normalized_like_the_reference() {
        // Names the reference itself sends travel unchanged.
        assert_eq!(slack_reaction_name("eyes").as_deref(), Some("eyes"));
        assert_eq!(
            slack_reaction_name("white_check_mark").as_deref(),
            Some("white_check_mark")
        );
        assert_eq!(slack_reaction_name("x").as_deref(), Some("x"));
        // Colons are stripped, as everywhere the reference reads a name back from Slack.
        assert_eq!(slack_reaction_name(" :eyes: ").as_deref(), Some("eyes"));
        assert_eq!(slack_reaction_name("::Eyes::").as_deref(), Some("eyes"));
        // Aliases collapse onto the canonical name of the glyph they share.
        assert_eq!(slack_reaction_name("+1").as_deref(), Some("thumbsup"));
        assert_eq!(
            slack_reaction_name(":thumbsup:").as_deref(),
            Some("thumbsup")
        );
        assert_eq!(slack_reaction_name("-1").as_deref(), Some("thumbsdown"));
        assert_eq!(
            slack_reaction_name("heavy_check_mark").as_deref(),
            Some("white_check_mark")
        );
        // A pasted glyph becomes its name too, with or without the variation selector.
        assert_eq!(
            slack_reaction_name("\u{1f44d}").as_deref(),
            Some("thumbsup")
        );
        assert_eq!(
            slack_reaction_name("\u{26a0}\u{fe0f}").as_deref(),
            Some("warning")
        );
        assert_eq!(
            slack_reaction_name("\u{2705}").as_deref(),
            Some("white_check_mark")
        );
        // A skin-tone suffix is part of the name Slack matches, not an alias.
        assert_eq!(
            slack_reaction_name("thumbsup::skin-tone-2").as_deref(),
            Some("thumbsup::skin-tone-2")
        );
        // A workspace custom emoji is not in the reference table, so it travels as written.
        assert_eq!(
            slack_reaction_name("party_parrot").as_deref(),
            Some("party_parrot")
        );
        // Nothing usable: blank, colons only, or a name Slack's grammar cannot carry.
        assert_eq!(slack_reaction_name(""), None);
        assert_eq!(slack_reaction_name("   "), None);
        assert_eq!(slack_reaction_name("::"), None);
        assert_eq!(slack_reaction_name("eyes eyes"), None);
        assert_eq!(slack_reaction_name("eyes\n").as_deref(), Some("eyes"));
    }

    // ----- outbound media: Slack's three-step external upload -----

    /// A transport that records requests and always fails at the connection level.
    #[derive(Default)]
    struct BrokenTransport {
        requests: Mutex<Vec<HttpRequest>>,
    }

    impl BrokenTransport {
        fn recorded(&self) -> Vec<HttpRequest> {
            self.requests
                .lock()
                .map(|guard| guard.clone())
                .unwrap_or_default()
        }
    }

    #[async_trait]
    impl Transport for BrokenTransport {
        async fn execute(&self, request: HttpRequest) -> Result<HttpResponse, TransportError> {
            if let Ok(mut requests) = self.requests.lock() {
                requests.push(request);
            }
            Err(TransportError::Request("connection reset by peer".into()))
        }
    }

    #[test]
    fn slack_supports_media() {
        let (adapter, _mock) = adapter_with(Vec::new());
        assert!(adapter.supports_media());
    }

    /// The whole transcript: upload URL → multipart POST → completion, with the fields the
    /// reference sends (`channel_id`, `initial_comment`, `thread_ts`, `files[{id,title}]`).
    #[tokio::test]
    async fn media_bytes_run_the_three_step_external_upload() {
        let (adapter, mock) = adapter_with(vec![
            upload_url_reply("https://files.slack.com/upload/v1/abc123", "F0UPLOAD1"),
            slack_text(200, "OK - 5"),
            completion_reply("F0UPLOAD1", "note.txt", "C123", "1500000000.000200"),
        ]);
        let media = bytes_media(MediaKind::Document, b"hello")
            .with_filename("note.txt")
            .with_mime("text/plain")
            .with_caption("**bold** caption");

        let result = adapter
            .send_media("C123", &media, Some("1500000000.000100"))
            .await;
        assert!(result.success, "{result:?}");
        assert_eq!(result.message_id.as_deref(), Some("1500000000.000200"));
        assert!(result.error_kind.is_none());
        assert!(result.continuation_message_ids.is_empty());

        let requests = mock.recorded();
        assert_eq!(requests.len(), 3, "three steps, three requests");

        // Step 1: files.getUploadURLExternal, form-encoded filename + byte length.
        assert_eq!(requests[0].method, HttpMethod::Post);
        assert_eq!(
            requests[0].url,
            "https://slack.com/api/files.getUploadURLExternal"
        );
        assert!(has_bearer(&requests[0], "xoxb-test"));
        assert_eq!(
            content_type(&requests[0]),
            "application/x-www-form-urlencoded"
        );
        assert_eq!(
            form_pairs(&requests[0]),
            vec![
                ("filename".to_string(), "note.txt".to_string()),
                ("length".to_string(), "5".to_string()),
            ]
        );

        // Step 2: multipart POST to the returned URL; the pre-signed hop carries no token.
        assert_eq!(requests[1].method, HttpMethod::Post);
        assert_eq!(requests[1].url, "https://files.slack.com/upload/v1/abc123");
        assert!(
            !requests[1]
                .headers
                .iter()
                .any(|(name, _)| name.eq_ignore_ascii_case("authorization")),
            "the upload URL is already signed; the bot token must not ride along"
        );
        let boundary = boundary_of(&requests[1]);
        assert!(
            !boundary.is_empty(),
            "the upload must be multipart/form-data"
        );
        assert_eq!(
            content_type(&requests[1]),
            format!("multipart/form-data; boundary={boundary}")
        );
        let body =
            String::from_utf8_lossy(requests[1].body.as_deref().unwrap_or_default()).to_string();
        assert!(body.starts_with(&format!("--{boundary}\r\n")), "{body}");
        assert!(
            body.contains(
                "Content-Disposition: form-data; name=\"file\"; filename=\"note.txt\"\r\n"
            ),
            "{body}"
        );
        assert!(
            body.contains("Content-Type: text/plain\r\n\r\nhello\r\n"),
            "{body}"
        );
        assert!(body.ends_with(&format!("--{boundary}--\r\n")), "{body}");

        // Step 3: files.completeUploadExternal shares the file, with caption and thread.
        assert_eq!(requests[2].method, HttpMethod::Post);
        assert_eq!(
            requests[2].url,
            "https://slack.com/api/files.completeUploadExternal"
        );
        assert!(has_bearer(&requests[2], "xoxb-test"));
        assert_eq!(
            content_type(&requests[2]),
            "application/x-www-form-urlencoded"
        );
        assert_eq!(
            form_pairs(&requests[2]),
            vec![
                (
                    "files".to_string(),
                    json!([{ "id": "F0UPLOAD1", "title": "note.txt" }]).to_string()
                ),
                ("channel_id".to_string(), "C123".to_string()),
                ("initial_comment".to_string(), "*bold* caption".to_string()),
                ("thread_ts".to_string(), "1500000000.000100".to_string()),
            ]
        );
    }

    /// No caption and no `reply_to`: the completion body keeps the two required fields only.
    #[tokio::test]
    async fn media_without_caption_or_thread_omits_those_fields() {
        let (adapter, mock) = adapter_with(vec![
            upload_url_reply("https://files.slack.com/upload/v1/abc123", "F1"),
            slack_text(200, "OK - 5"),
            slack_ok(json!({ "ok": true, "files": [{ "id": "F1" }] })),
        ]);
        let result = adapter
            .send_media("C123", &bytes_media(MediaKind::Document, b"hello"), None)
            .await;

        assert!(result.success, "{result:?}");
        // No shares/timestamp: the file id is the only handle Slack returned.
        assert_eq!(result.message_id.as_deref(), Some("F1"));

        let requests = mock.recorded();
        assert_eq!(requests.len(), 3);
        assert_eq!(
            form_pairs(&requests[2]),
            vec![
                (
                    "files".to_string(),
                    json!([{ "id": "F1", "title": "attachment.bin" }]).to_string()
                ),
                ("channel_id".to_string(), "C123".to_string()),
            ]
        );
    }

    #[test]
    fn media_names_are_sanitized_and_defaulted() {
        assert_eq!(
            default_media_attachment(MediaKind::Image),
            ("image.png", "image/png")
        );
        assert_eq!(
            default_media_attachment(MediaKind::Video),
            ("video.mp4", "video/mp4")
        );
        assert_eq!(
            default_media_attachment(MediaKind::Audio),
            ("audio.mp3", "audio/mpeg")
        );
        assert_eq!(
            default_media_attachment(MediaKind::Document),
            ("attachment.bin", "application/octet-stream")
        );

        let bare = bytes_media(MediaKind::Image, b"png");
        assert_eq!(media_filename(&bare), "image.png");
        assert_eq!(media_mime(&bare), "image/png");

        // A CRLF in a filename would inject a part header; a MIME without `/` is not a type.
        let hostile = bytes_media(MediaKind::Document, b"x")
            .with_filename("bad\r\nname\".txt")
            .with_mime("not-a-mime");
        assert_eq!(media_filename(&hostile), "badname.txt");
        assert_eq!(media_mime(&hostile), "application/octet-stream");
    }

    #[tokio::test]
    async fn media_upload_url_ok_false_stops_before_the_upload() {
        let (adapter, mock) = adapter_with(vec![slack_error("invalid_auth")]);
        let result = adapter
            .send_media("C123", &bytes_media(MediaKind::Document, b"hello"), None)
            .await;

        assert!(!result.success);
        assert_eq!(result.error_kind.as_deref(), Some("forbidden"));
        assert!(!result.retryable);
        assert!(result.message_id.is_none());
        assert_eq!(
            mock.recorded().len(),
            1,
            "a failed upload-URL request must stop the flow"
        );
    }

    #[tokio::test]
    async fn media_upload_url_reply_without_fields_is_a_failure() {
        let (adapter, mock) = adapter_with(vec![slack_ok(json!({ "ok": true, "file_id": "F1" }))]);
        let result = adapter
            .send_media("C123", &bytes_media(MediaKind::Document, b"hello"), None)
            .await;
        assert!(!result.success);
        assert_eq!(result.error_kind.as_deref(), Some("unknown"));
        assert!(
            result
                .error
                .as_deref()
                .unwrap_or_default()
                .contains("no upload_url"),
            "{result:?}"
        );
        assert_eq!(mock.recorded().len(), 1);

        let (adapter, mock) = adapter_with(vec![slack_ok(json!({
            "ok": true,
            "upload_url": "https://files.slack.com/upload/v1/abc123"
        }))]);
        let result = adapter
            .send_media("C123", &bytes_media(MediaKind::Document, b"hello"), None)
            .await;
        assert!(!result.success);
        assert!(
            result
                .error
                .as_deref()
                .unwrap_or_default()
                .contains("no file_id"),
            "{result:?}"
        );
        assert_eq!(mock.recorded().len(), 1);
    }

    #[tokio::test]
    async fn media_upload_failure_stops_before_completion() {
        let (adapter, mock) = adapter_with(vec![
            upload_url_reply("https://files.slack.com/upload/v1/abc123", "F1"),
            slack_text(500, "upstream error"),
        ]);
        let result = adapter
            .send_media("C123", &bytes_media(MediaKind::Document, b"hello"), None)
            .await;

        assert!(!result.success);
        assert!(result.retryable, "a 5xx upload is worth another attempt");
        assert_eq!(result.error_kind.as_deref(), Some("transient"));
        assert!(result.error.as_deref().unwrap_or_default().contains("500"));

        let requests = mock.recorded();
        assert_eq!(
            requests.len(),
            2,
            "completion must not run after a failed upload"
        );
        assert!(
            requests
                .iter()
                .all(|request| !request.url.ends_with("files.completeUploadExternal")),
            "an incomplete upload must never be completed"
        );
    }

    #[tokio::test]
    async fn media_upload_429_is_retryable_with_retry_after() {
        let (adapter, _mock) = adapter_with(vec![
            upload_url_reply("https://files.slack.com/upload/v1/abc123", "F1"),
            HttpResponse {
                status: 429,
                body: json!({ "ok": false, "error": "ratelimited" }).to_string(),
                retry_after: Some(7.0),
            },
        ]);
        let result = adapter
            .send_media("C123", &bytes_media(MediaKind::Document, b"hello"), None)
            .await;

        assert!(!result.success);
        assert!(result.retryable);
        assert_eq!(result.retry_after, Some(7.0));
        assert_eq!(result.error_kind.as_deref(), Some("rate_limited"));
    }

    #[tokio::test]
    async fn media_upload_forbidden_not_found_and_server_errors_are_typed() {
        for (status, kind, retryable) in [
            (403, "forbidden", false),
            (404, "not_found", false),
            (503, "transient", true),
        ] {
            let (adapter, mock) = adapter_with(vec![
                upload_url_reply("https://files.slack.com/upload/v1/abc123", "F1"),
                slack_text(status, "nope"),
            ]);
            let result = adapter
                .send_media("C123", &bytes_media(MediaKind::Document, b"hello"), None)
                .await;

            assert!(!result.success, "{status} must not report success");
            assert_eq!(result.error_kind.as_deref(), Some(kind), "{status}");
            assert_eq!(result.retryable, retryable, "{status}");
            assert_eq!(mock.recorded().len(), 2);
        }
    }

    #[tokio::test]
    async fn media_upload_rejects_a_json_error_on_http_200() {
        let (adapter, _mock) = adapter_with(vec![
            upload_url_reply("https://files.slack.com/upload/v1/abc123", "F1"),
            HttpResponse {
                status: 200,
                body: json!({ "ok": false, "error": "ratelimited" }).to_string(),
                retry_after: Some(2.5),
            },
        ]);
        let result = adapter
            .send_media("C123", &bytes_media(MediaKind::Document, b"hello"), None)
            .await;
        assert!(!result.success);
        assert_eq!(result.error_kind.as_deref(), Some("rate_limited"));
        assert_eq!(result.retry_after, Some(2.5));

        let (adapter, _mock) = adapter_with(vec![
            upload_url_reply("https://files.slack.com/upload/v1/abc123", "F1"),
            slack_text(200, r#"{"ok":false,"error":"file_upload_size_restricted"}"#),
        ]);
        let result = adapter
            .send_media("C123", &bytes_media(MediaKind::Document, b"hello"), None)
            .await;
        assert!(!result.success);
        assert_eq!(result.error_kind.as_deref(), Some("too_long"));
        assert!(!result.retryable);
    }

    #[tokio::test]
    async fn media_upload_rejects_an_html_login_page() {
        let (adapter, _mock) = adapter_with(vec![
            upload_url_reply("https://files.slack.com/upload/v1/abc123", "F1"),
            slack_text(200, "<html><body>sign in</body></html>"),
        ]);
        let result = adapter
            .send_media("C123", &bytes_media(MediaKind::Document, b"hello"), None)
            .await;

        assert!(!result.success);
        assert_eq!(result.error_kind.as_deref(), Some("forbidden"));
        assert!(
            result
                .error
                .as_deref()
                .unwrap_or_default()
                .contains("HTML page"),
            "{result:?}"
        );
    }

    #[tokio::test]
    async fn media_completion_failure_is_surfaced() {
        let (adapter, mock) = adapter_with(vec![
            upload_url_reply("https://files.slack.com/upload/v1/abc123", "F1"),
            slack_text(200, "OK - 5"),
            slack_error("channel_not_found"),
        ]);
        let result = adapter
            .send_media("C123", &bytes_media(MediaKind::Document, b"hello"), None)
            .await;

        assert!(!result.success);
        assert_eq!(result.error_kind.as_deref(), Some("not_found"));
        assert!(result.message_id.is_none());
        assert_eq!(mock.recorded().len(), 3);

        // File-endpoint error codes the text lane never sees are classified for uploads only.
        let (adapter, _mock) = adapter_with(vec![
            upload_url_reply("https://files.slack.com/upload/v1/abc123", "F1"),
            slack_text(200, "OK - 5"),
            slack_error("file_uploads_disabled"),
        ]);
        let result = adapter
            .send_media("C123", &bytes_media(MediaKind::Document, b"hello"), None)
            .await;
        assert!(!result.success);
        assert_eq!(result.error_kind.as_deref(), Some("forbidden"));
    }

    #[tokio::test]
    async fn media_transport_error_is_retryable() {
        let transport = Arc::new(BrokenTransport::default());
        let adapter = SlackAdapter::new(transport.clone(), "xoxb-test");
        let result = adapter
            .send_media("C123", &bytes_media(MediaKind::Document, b"hello"), None)
            .await;

        assert!(!result.success);
        assert!(result.retryable);
        assert_eq!(result.error_kind.as_deref(), Some("transient"));
        assert!(
            result
                .error
                .as_deref()
                .unwrap_or_default()
                .contains("transport error"),
            "{result:?}"
        );
        assert_eq!(transport.recorded().len(), 1);
    }

    #[tokio::test]
    async fn media_empty_bytes_are_refused_without_a_request() {
        let (adapter, mock) = adapter_with(Vec::new());
        let result = adapter
            .send_media("C123", &bytes_media(MediaKind::Document, b""), None)
            .await;

        assert!(!result.success);
        assert_eq!(result.error_kind.as_deref(), Some("bad_format"));
        assert!(
            result
                .error
                .as_deref()
                .unwrap_or_default()
                .contains("empty")
        );
        assert!(
            mock.recorded().is_empty(),
            "an empty upload must not hit the wire"
        );
    }

    #[tokio::test]
    async fn media_url_source_is_refused_truthfully() {
        let (adapter, mock) = adapter_with(Vec::new());
        let media = OutboundMedia::image_url("https://example.test/a.png");
        let result = adapter.send_media("C123", &media, None).await;

        assert!(!result.success);
        assert_eq!(result.error_kind.as_deref(), Some("bad_format"));
        let error = result.error.unwrap_or_default();
        assert!(error.contains("MediaSource::Url"), "{error}");
        assert!(
            error.contains("MediaSource::Bytes"),
            "a refusal must name the supported source: {error}"
        );
        assert!(
            mock.recorded().is_empty(),
            "a URL refusal must issue zero requests"
        );
    }

    #[tokio::test]
    async fn media_over_the_size_cap_is_refused_without_a_request() {
        let (adapter, mock) = adapter_with(Vec::new());
        let oversized = bytes_media(MediaKind::Video, &vec![0u8; SLACK_MAX_UPLOAD_BYTES + 1]);
        let result = adapter.send_media("C123", &oversized, None).await;

        assert!(!result.success);
        assert_eq!(result.error_kind.as_deref(), Some("too_long"));
        assert!(
            result
                .error
                .as_deref()
                .unwrap_or_default()
                .contains("per-file upload cap")
        );
        assert!(mock.recorded().is_empty());
    }

    #[test]
    fn completed_upload_id_follows_the_reference_preference_order() {
        let shared = json!({
            "ok": true,
            "files": [{
                "id": "F1",
                "timestamp": 1_700_000_000,
                "shares": { "public": { "C1": [{ "ts": "1.2" }] } }
            }]
        });
        assert_eq!(completed_upload_id(&shared, "F9"), "1.2");

        let timestamped =
            json!({ "ok": true, "files": [{ "id": "F1", "timestamp": 1_700_000_000 }] });
        assert_eq!(completed_upload_id(&timestamped, "F9"), "1700000000");

        let top_level_ts = json!({ "ok": true, "ts": "5.6" });
        assert_eq!(completed_upload_id(&top_level_ts, "F9"), "5.6");

        let bare = json!({ "ok": true, "files": [{ "id": "F1", "title": "a.txt" }] });
        assert_eq!(completed_upload_id(&bare, "F9"), "F1");

        let single_file = json!({
            "ok": true,
            "file": { "id": "F2", "shares": { "private": { "D1": [{ "ts": "3.4" }] } } }
        });
        assert_eq!(completed_upload_id(&single_file, "F2"), "3.4");

        let empty = json!({ "ok": true });
        assert_eq!(completed_upload_id(&empty, "F9"), "F9");
    }

    #[test]
    fn upload_error_codes_map_to_the_send_taxonomy() {
        assert_eq!(upload_error_kind("file_upload_size_restricted"), "too_long");
        assert_eq!(upload_error_kind("snippet_too_large"), "too_long");
        assert_eq!(upload_error_kind("file_uploads_disabled"), "forbidden");
        assert_eq!(upload_error_kind("missing_scope"), "forbidden");
        assert_eq!(upload_error_kind("file_not_found"), "not_found");
        assert_eq!(upload_error_kind("invalid_channel"), "not_found");
        assert_eq!(upload_error_kind("ratelimited"), "rate_limited");
        assert_eq!(upload_error_kind("something_else"), "unknown");
        // The text lane keeps its own table: an upload-only code stays unclassified there.
        assert_eq!(error_kind("file_uploads_disabled"), "unknown");
    }

    // ----- native draft streaming (chat.startStream / chat.appendStream) -----------------------

    /// `chat.startStream` success: the envelope carries the streamed message `ts`.
    fn stream_started(ts: &str) -> HttpResponse {
        slack_ok(json!({ "ok": true, "ts": ts }))
    }

    /// `chat.appendStream` / `chat.stopStream` success: a bare `ok: true`.
    fn stream_accepted() -> HttpResponse {
        slack_ok(json!({ "ok": true }))
    }

    #[test]
    fn slack_advertises_native_draft_streaming() {
        let (adapter, mock) = adapter_with(Vec::new());
        assert!(adapter.supports_draft_streaming());
        assert!(
            mock.recorded().is_empty(),
            "the capability flag is a flag, not a call"
        );
    }

    #[tokio::test]
    async fn first_draft_frame_starts_a_stream_and_later_frames_append_the_delta() {
        let (adapter, mock) =
            adapter_with(vec![stream_started("1500000000.000300"), stream_accepted()]);

        // First frame: chat.startStream, with the consumer's cursor glyph stripped.
        let first = adapter.send_draft("C123", 1, "Hello **world** ▍").await;
        assert!(first.success, "{first:?}");
        assert_eq!(first.message_id.as_deref(), Some("1500000000.000300"));

        // Later frame: only the delta past the streamed text is appended.
        let second = adapter
            .send_draft("C123", 1, "Hello **world** and more")
            .await;
        assert!(second.success, "{second:?}");
        assert_eq!(second.message_id.as_deref(), Some("1500000000.000300"));

        // A frame identical to the streamed text costs no request at all.
        let third = adapter
            .send_draft("C123", 1, "Hello **world** and more")
            .await;
        assert!(third.success, "{third:?}");
        assert_eq!(third.message_id.as_deref(), Some("1500000000.000300"));

        let requests = mock.recorded();
        assert_eq!(
            requests.len(),
            2,
            "the repeated frame must not hit the wire: {requests:?}"
        );

        assert_eq!(requests[0].method, HttpMethod::Post);
        assert_eq!(requests[0].url, "https://slack.com/api/chat.startStream");
        assert!(has_bearer(&requests[0], "xoxb-test"));
        let started = body_of(&requests[0]);
        assert_eq!(started["channel"], "C123");
        assert_eq!(
            started["markdown_text"], "Hello **world**",
            "the cursor glyph must be stripped before it reaches Slack: {started}"
        );
        assert!(
            started.get("thread_ts").is_none(),
            "send_draft is handed no metadata, so no thread anchor is invented: {started}"
        );

        assert_eq!(requests[1].method, HttpMethod::Post);
        assert_eq!(requests[1].url, "https://slack.com/api/chat.appendStream");
        assert!(has_bearer(&requests[1], "xoxb-test"));
        let appended = body_of(&requests[1]);
        assert_eq!(appended["channel"], "C123");
        assert_eq!(
            appended["ts"], "1500000000.000300",
            "the append must address the started message"
        );
        assert_eq!(
            appended["markdown_text"], " and more",
            "chat.appendStream is append-only, so only the delta may travel: {appended}"
        );
    }

    #[tokio::test]
    async fn draft_stream_state_is_isolated_per_channel_and_draft() {
        let (adapter, mock) = adapter_with(vec![
            stream_started("ts.chat1.draft1"),
            stream_started("ts.chat2.draft1"),
            stream_started("ts.chat1.draft2"),
            stream_accepted(),
        ]);

        assert!(adapter.send_draft("C1", 1, "alpha").await.success);
        // A different channel is a different stream.
        assert!(adapter.send_draft("C2", 1, "beta").await.success);
        // A different draft in the first channel opens its own stream instead of reusing C1/1's.
        assert!(adapter.send_draft("C1", 2, "gamma").await.success);
        // Back to the first draft: it appends into its own message, not the newest stream.
        let appended = adapter.send_draft("C1", 1, "alpha delta").await;
        assert!(appended.success, "{appended:?}");
        assert_eq!(appended.message_id.as_deref(), Some("ts.chat1.draft1"));

        let requests = mock.recorded();
        assert_eq!(requests.len(), 4);
        let urls: Vec<&str> = requests
            .iter()
            .map(|request| request.url.as_str())
            .collect();
        assert_eq!(
            urls,
            vec![
                "https://slack.com/api/chat.startStream",
                "https://slack.com/api/chat.startStream",
                "https://slack.com/api/chat.startStream",
                "https://slack.com/api/chat.appendStream",
            ]
        );
        assert_eq!(body_of(&requests[0])["channel"], "C1");
        assert_eq!(body_of(&requests[1])["channel"], "C2");
        assert_eq!(body_of(&requests[2])["channel"], "C1");
        assert_eq!(
            body_of(&requests[3])["ts"],
            "ts.chat1.draft1",
            "the append must land in draft 1's stream"
        );
        assert_eq!(body_of(&requests[3])["markdown_text"], " delta");
    }

    #[test]
    fn the_stream_cursor_glyph_is_stripped_before_the_wire() {
        for glyph in STREAM_CURSOR_GLYPHS {
            assert_eq!(strip_stream_cursor(&format!("working{glyph}")), "working");
            assert_eq!(
                strip_stream_cursor(&format!("working {glyph}  ")),
                "working"
            );
            assert_eq!(
                strip_stream_cursor(glyph),
                "",
                "a frame that is only a cursor carries no text"
            );
        }
        // Only a *trailing* glyph is a cursor: text that carries one keeps it.
        assert_eq!(strip_stream_cursor("a ▍ b"), "a ▍ b");
        assert_eq!(strip_stream_cursor("plain"), "plain");
        assert_eq!(strip_stream_cursor(""), "");
        // An unterminated frame keeps its own trailing whitespace, as the reference returns `text`.
        assert_eq!(strip_stream_cursor("still going  "), "still going  ");
    }

    #[tokio::test]
    async fn a_cursor_only_frame_is_refused_without_a_request() {
        let (adapter, mock) = adapter_with(Vec::new());
        let result = adapter.send_draft("C123", 1, " ▍ ").await;
        assert!(!result.success);
        assert_eq!(result.error_kind.as_deref(), Some("bad_format"));
        assert!(mock.recorded().is_empty());
    }

    #[tokio::test]
    async fn an_unsupported_marker_latches_the_native_stream_off() {
        for code in [
            "unknown_method",
            "missing_scope",
            "not_allowed",
            "feature_not_enabled",
            "method_deprecated",
            "streaming_not_allowed",
            "not_authed",
            "invalid_method",
        ] {
            let (adapter, mock) = adapter_with(vec![slack_error(code)]);
            assert!(adapter.supports_draft_streaming(), "{code}");

            let result = adapter.send_draft("C123", 1, "hello").await;
            assert!(!result.success, "{code}");
            assert_eq!(
                result.error_kind.as_deref(),
                Some("forbidden"),
                "{code}: a capability the app lacks is not retryable"
            );
            assert!(!result.retryable, "{code}");
            assert!(
                result.error.as_deref().unwrap_or_default().contains(code),
                "{code}: {result:?}"
            );

            // The state is remembered, exactly as the reference's `_native_stream_unsupported`.
            assert!(!adapter.supports_draft_streaming(), "{code}");
            let again = adapter.send_draft("C123", 1, "hello again").await;
            assert!(!again.success, "{code}");
            assert_eq!(again.error_kind.as_deref(), Some("forbidden"), "{code}");
            assert_eq!(
                mock.recorded().len(),
                1,
                "{code}: only the first frame may reach the wire"
            );
        }
    }

    #[tokio::test]
    async fn draft_failures_keep_the_send_taxonomy() {
        let (adapter, _mock) = adapter_with(vec![slack_error("channel_not_found")]);
        let result = adapter.send_draft("C404", 1, "hello").await;
        assert!(!result.success);
        assert_eq!(result.error_kind.as_deref(), Some("not_found"));

        let (adapter, _mock) = adapter_with(vec![HttpResponse {
            status: 200,
            body: json!({ "ok": false, "error": "ratelimited" }).to_string(),
            retry_after: Some(5.0),
        }]);
        let result = adapter.send_draft("C123", 1, "hello").await;
        assert!(!result.success);
        assert_eq!(result.error_kind.as_deref(), Some("rate_limited"));
        assert!(result.retryable);
        assert_eq!(result.retry_after, Some(5.0));

        let (adapter, _mock) = adapter_with(vec![slack_text(
            500,
            r#"{"ok":false,"error":"fatal_error"}"#,
        )]);
        let result = adapter.send_draft("C123", 1, "hello").await;
        assert!(!result.success);
        assert_eq!(result.error_kind.as_deref(), Some("transient"));
        assert!(result.retryable);

        // A stream without a `ts` cannot be appended to, so `ok: true` alone is not a success.
        let (adapter, _mock) = adapter_with(vec![slack_ok(json!({ "ok": true }))]);
        let result = adapter.send_draft("C123", 1, "hello").await;
        assert!(
            !result.success,
            "a streamed message without a ts must not report success"
        );
        assert!(result.message_id.is_none());
        assert_eq!(result.error_kind.as_deref(), Some("unknown"));
        assert!(
            result
                .error
                .as_deref()
                .unwrap_or_default()
                .contains("no ts"),
            "{result:?}"
        );
    }

    #[tokio::test]
    async fn a_rewritten_prefix_seals_the_stream_and_fails_the_frame() {
        let (adapter, mock) = adapter_with(vec![
            stream_started("ts.sealed"),
            stream_accepted(),
            stream_started("ts.fresh"),
        ]);
        assert!(adapter.send_draft("C123", 1, "Hello").await.success);

        let rewritten = adapter.send_draft("C123", 1, "Goodbye").await;
        assert!(
            !rewritten.success,
            "an append-only stream cannot be rewritten in place"
        );
        assert_eq!(rewritten.error_kind.as_deref(), Some("bad_format"));
        assert!(
            rewritten
                .error
                .as_deref()
                .unwrap_or_default()
                .contains("rewrote"),
            "{rewritten:?}"
        );

        let requests = mock.recorded();
        assert_eq!(requests.len(), 2);
        assert_eq!(requests[1].url, "https://slack.com/api/chat.stopStream");
        assert_eq!(body_of(&requests[1])["channel"], "C123");
        assert_eq!(
            body_of(&requests[1])["ts"],
            "ts.sealed",
            "the sealed stream is the one that was open"
        );

        // The dead stream is forgotten, so the next frame opens a fresh one.
        let fresh = adapter.send_draft("C123", 1, "Hello again").await;
        assert!(fresh.success, "{fresh:?}");
        assert_eq!(fresh.message_id.as_deref(), Some("ts.fresh"));
        let requests = mock.recorded();
        assert_eq!(requests.len(), 3);
        assert_eq!(requests[2].url, "https://slack.com/api/chat.startStream");
    }

    #[tokio::test]
    async fn a_server_sealed_stream_is_reopened_with_only_the_unsent_tail() {
        let (adapter, mock) = adapter_with(vec![
            stream_started("ts.first"),
            slack_error("message_not_in_streaming_state"),
            stream_started("ts.second"),
            stream_accepted(),
        ]);
        assert!(adapter.send_draft("C123", 7, "Hello").await.success);

        let reopened = adapter.send_draft("C123", 7, "Hello world").await;
        assert!(reopened.success, "{reopened:?}");
        assert_eq!(reopened.message_id.as_deref(), Some("ts.second"));

        let requests = mock.recorded();
        assert_eq!(requests.len(), 3);
        assert_eq!(requests[1].url, "https://slack.com/api/chat.appendStream");
        assert_eq!(body_of(&requests[1])["markdown_text"], " world");
        assert_eq!(requests[2].url, "https://slack.com/api/chat.startStream");
        assert_eq!(body_of(&requests[2])["channel"], "C123");
        assert_eq!(
            body_of(&requests[2])["markdown_text"],
            " world",
            "the reopened stream is seeded with only the text past the sealed message"
        );

        // The full text stays the high-water mark, so the next delta still diffs correctly.
        let extended = adapter.send_draft("C123", 7, "Hello world!").await;
        assert!(extended.success, "{extended:?}");
        assert_eq!(extended.message_id.as_deref(), Some("ts.second"));
        let requests = mock.recorded();
        assert_eq!(requests.len(), 4);
        assert_eq!(requests[3].url, "https://slack.com/api/chat.appendStream");
        assert_eq!(body_of(&requests[3])["ts"], "ts.second");
        assert_eq!(body_of(&requests[3])["markdown_text"], "!");
    }

    #[tokio::test]
    async fn blank_draft_inputs_are_refused_without_a_request() {
        let (adapter, mock) = adapter_with(Vec::new());
        for (chat_id, content) in [
            ("", "hello"),
            ("   ", "hello"),
            ("C123", ""),
            ("C123", " \n\t "),
        ] {
            let result = adapter.send_draft(chat_id, 1, content).await;
            assert!(!result.success, "{chat_id:?}/{content:?}");
            assert_eq!(
                result.error_kind.as_deref(),
                Some("bad_format"),
                "{chat_id:?}/{content:?}"
            );
            assert!(result.message_id.is_none());
        }
        assert!(
            mock.recorded().is_empty(),
            "a blank draft must not hit the wire"
        );
    }

    // ----- rich cards (Block Kit) -------------------------------------------------------------

    /// A transport with no network at all: every call is a connection failure.
    struct DownTransport;

    #[async_trait]
    impl Transport for DownTransport {
        async fn execute(&self, _request: HttpRequest) -> Result<HttpResponse, TransportError> {
            Err(TransportError::Request("connection reset".to_string()))
        }
    }

    /// `chat.postMessage` success for a posted message `ts`.
    fn posted(ts: &str) -> HttpResponse {
        slack_ok(json!({ "ok": true, "channel": "C123", "ts": ts }))
    }

    /// The one request a card send must produce.
    fn the_card_request(mock: &Arc<MockTransport>) -> HttpRequest {
        let requests = mock.recorded();
        assert_eq!(requests.len(), 1, "a card is one chat.postMessage call");
        requests[0].clone()
    }

    #[tokio::test]
    async fn supports_cards_is_true_without_a_request() {
        let (adapter, mock) = adapter_with(Vec::new());
        assert!(adapter.supports_cards());
        assert!(
            mock.recorded().is_empty(),
            "a capability flag must not hit the wire"
        );
    }

    #[tokio::test]
    async fn card_blocks_are_posted_with_a_text_fallback_derived_from_them() {
        let (adapter, mock) = adapter_with(vec![posted("1700000000.000200")]);
        let card = json!([
            {
                "type": "header",
                "text": { "type": "plain_text", "text": "Deploy finished" }
            },
            {
                "type": "section",
                "text": { "type": "mrkdwn", "text": "All tests green" },
                "fields": [
                    { "type": "mrkdwn", "text": "*branch:* main" },
                    { "type": "mrkdwn", "text": "*commit:* abc123" }
                ]
            },
            {
                "type": "context",
                "elements": [{ "type": "mrkdwn", "text": "vikram · 12s" }]
            }
        ]);
        let result = adapter.send_card("C123", &card).await;

        assert!(result.success, "{:?}", result.error);
        assert_eq!(result.message_id.as_deref(), Some("1700000000.000200"));
        assert!(result.error.is_none());
        assert_eq!(result.error_kind, None);
        assert!(!result.retryable);
        // The raw envelope rides along, the way `send` reports the last post.
        assert_eq!(
            result
                .raw_response
                .as_ref()
                .and_then(|raw| raw.get("ok"))
                .and_then(Value::as_bool),
            Some(true)
        );

        let request = the_card_request(&mock);
        assert_eq!(request.method, HttpMethod::Post);
        assert_eq!(request.url, "https://slack.com/api/chat.postMessage");
        assert!(has_bearer(&request, "xoxb-test"));
        let body = body_of(&request);
        assert_eq!(body["channel"], "C123");
        // The payload is passed through unchanged: the caller's blocks are what Slack receives.
        assert_eq!(body["blocks"], card);
        // No `text` was supplied, so it is derived from the card's own text objects in reading
        // order (header, section text, section fields, context elements).
        assert_eq!(
            body["text"],
            "Deploy finished\nAll tests green\n*branch:* main\n*commit:* abc123\nvikram · 12s"
        );
        // A card is not threaded: `send_card` is handed no reply target.
        assert!(body.get("thread_ts").is_none(), "{body}");
    }

    #[tokio::test]
    async fn the_object_form_carries_the_callers_own_text_fallback() {
        let (adapter, mock) = adapter_with(vec![posted("1700000000.000300")]);
        let card = json!({
            "text": "Heads up: build 42 is live",
            "blocks": [{
                "type": "section",
                "text": { "type": "mrkdwn", "text": "The body Slack renders" }
            }]
        });
        let result = adapter.send_card("C123", &card).await;

        assert!(result.success, "{:?}", result.error);
        assert_eq!(result.message_id.as_deref(), Some("1700000000.000300"));
        let body = body_of(&the_card_request(&mock));
        assert_eq!(body["channel"], "C123");
        // The caller's `text` wins over anything derived, and the wrapper is not posted as a block.
        assert_eq!(body["text"], "Heads up: build 42 is live");
        assert_eq!(body["blocks"], card["blocks"]);
        assert_eq!(body["blocks"].as_array().map(Vec::len), Some(1));
        assert!(
            body.get("mrkdwn").is_none(),
            "the card lane sends exactly channel/text/blocks: {body}"
        );
    }

    #[tokio::test]
    async fn a_blank_object_text_is_ignored_and_the_fallback_is_derived() {
        let (adapter, mock) = adapter_with(vec![posted("1700000000.000310")]);
        let card = json!({
            "text": "   ",
            "blocks": [{
                "type": "section",
                "text": { "type": "mrkdwn", "text": "Derived instead" }
            }]
        });
        let result = adapter.send_card("C123", &card).await;

        assert!(result.success, "{:?}", result.error);
        assert_eq!(body_of(&the_card_request(&mock))["text"], "Derived instead");
    }

    #[tokio::test]
    async fn a_card_with_no_text_at_all_posts_without_a_blank_fallback() {
        let (adapter, mock) = adapter_with(vec![posted("1700000000.000400")]);
        let card = json!([
            { "type": "divider" },
            { "type": "image", "image_url": "https://example.test/a.png", "alt_text": "chart" }
        ]);
        let result = adapter.send_card("C123", &card).await;

        assert!(result.success, "{:?}", result.error);
        let body = body_of(&the_card_request(&mock));
        assert_eq!(body["blocks"], card);
        // Nothing readable to derive: no empty `text` is sent (Slack accepts a blocks-only post),
        // and no placeholder copy is invented for the notification.
        assert!(body.get("text").is_none(), "{body}");
    }

    #[tokio::test]
    async fn an_oversized_derived_fallback_is_clamped_to_the_message_budget() {
        let (adapter, mock) = adapter_with(vec![posted("1700000000.000500")]);
        let card = json!([{
            "type": "section",
            "text": { "type": "mrkdwn", "text": "x".repeat(SLACK_MAX_MESSAGE_LENGTH * 2) }
        }]);
        let result = adapter.send_card("C123", &card).await;

        assert!(result.success, "{:?}", result.error);
        let body = body_of(&the_card_request(&mock));
        let text = body["text"].as_str().unwrap_or_default();
        assert_eq!(text.chars().count(), SLACK_MAX_MESSAGE_LENGTH);
        // Only the fallback is clamped: the blocks themselves are posted as given.
        assert_eq!(
            body["blocks"][0]["text"]["text"].as_str().map(str::len),
            Some(SLACK_MAX_MESSAGE_LENGTH * 2)
        );
    }

    #[tokio::test]
    async fn invalid_cards_are_refused_with_zero_requests() {
        let (adapter, mock) = adapter_with(Vec::new());
        let cases: Vec<(&str, Value)> = vec![
            ("a bare string", json!("{\"type\":\"section\"}")),
            ("a bare number", json!(42)),
            ("null", Value::Null),
            (
                "an object without blocks",
                json!({ "text": "no blocks here" }),
            ),
            (
                "an object with non-array blocks",
                json!({ "blocks": "nope" }),
            ),
            ("an empty blocks array", json!([])),
            (
                "a block without a type",
                json!([{ "type": "divider" }, { "text": { "text": "typeless" } }]),
            ),
            ("a block that is not an object", json!(["section"])),
        ];
        for (label, card) in cases {
            let result = adapter.send_card("C123", &card).await;
            assert!(!result.success, "{label} must not report success");
            assert_eq!(
                result.error_kind.as_deref(),
                Some("bad_format"),
                "{label}: {:?}",
                result.error
            );
            assert!(result.message_id.is_none(), "{label}");
            assert!(!result.retryable, "{label}");
            assert!(
                result
                    .error
                    .as_deref()
                    .unwrap_or_default()
                    .starts_with("slack send card:"),
                "{label}: {:?}",
                result.error
            );
        }
        assert!(
            mock.recorded().is_empty(),
            "an invalid card must never reach the wire"
        );

        // The refusals name what was wrong, not just that something was.
        let string_card = adapter.send_card("C123", &json!("nope")).await;
        assert!(
            string_card
                .error
                .as_deref()
                .unwrap_or_default()
                .contains("got a string"),
            "{:?}",
            string_card.error
        );
        let typeless = adapter
            .send_card("C123", &json!([{ "type": "divider" }, { "text": {} }]))
            .await;
        assert!(
            typeless
                .error
                .as_deref()
                .unwrap_or_default()
                .contains("block 1"),
            "{:?}",
            typeless.error
        );

        // A blank channel is refused before the card is even inspected.
        let blank_channel = adapter
            .send_card("  ", &json!([{ "type": "divider" }]))
            .await;
        assert!(!blank_channel.success);
        assert_eq!(blank_channel.error_kind.as_deref(), Some("bad_format"));
        assert!(mock.recorded().is_empty());
    }

    #[tokio::test]
    async fn more_blocks_than_slack_accepts_are_refused_locally() {
        let (adapter, mock) = adapter_with(Vec::new());
        let blocks: Vec<Value> = (0..=SLACK_MAX_BLOCKS)
            .map(|idx| json!({ "type": "divider", "block_id": format!("b{idx}") }))
            .collect();
        let result = adapter.send_card("C123", &Value::Array(blocks)).await;

        assert!(!result.success);
        assert_eq!(result.error_kind.as_deref(), Some("too_long"));
        assert!(
            result
                .error
                .as_deref()
                .unwrap_or_default()
                .contains(&SLACK_MAX_BLOCKS.to_string()),
            "{:?}",
            result.error
        );
        assert!(mock.recorded().is_empty(), "no request may be made");
    }

    #[tokio::test]
    async fn a_card_failure_envelope_is_a_typed_failure() {
        // Slack answers HTTP 200 with `ok: false`; the envelope decides.
        let (adapter, mock) = adapter_with(vec![slack_error("invalid_blocks")]);
        let result = adapter
            .send_card("C123", &json!([{ "type": "section" }]))
            .await;

        assert!(!result.success);
        assert_eq!(result.error_kind.as_deref(), Some("bad_format"));
        assert!(!result.retryable);
        assert!(result.message_id.is_none());
        assert!(
            result
                .error
                .as_deref()
                .unwrap_or_default()
                .contains("invalid_blocks"),
            "{:?}",
            result.error
        );
        assert_eq!(mock.recorded().len(), 1);

        // A permission error keeps the text lane's classification.
        let (adapter, _mock) = adapter_with(vec![slack_error("channel_not_found")]);
        let result = adapter
            .send_card("C404", &json!([{ "type": "divider" }]))
            .await;
        assert_eq!(result.error_kind.as_deref(), Some("not_found"));

        // `ratelimited` on a 200 carries the `Retry-After` through, retryable.
        let (adapter, _mock) = adapter_with(vec![HttpResponse {
            status: 200,
            body: json!({ "ok": false, "error": "ratelimited" }).to_string(),
            retry_after: Some(4.0),
        }]);
        let result = adapter
            .send_card("C123", &json!([{ "type": "divider" }]))
            .await;
        assert_eq!(result.error_kind.as_deref(), Some("rate_limited"));
        assert!(result.retryable);
        assert_eq!(result.retry_after, Some(4.0));
    }

    #[tokio::test]
    async fn a_card_that_slack_accepts_without_a_ts_is_not_a_delivery() {
        let (adapter, _mock) = adapter_with(vec![slack_ok(json!({ "ok": true }))]);
        let result = adapter
            .send_card("C123", &json!([{ "type": "divider" }]))
            .await;

        assert!(
            !result.success,
            "an ok envelope with no ts delivered nothing"
        );
        assert_eq!(result.error_kind.as_deref(), Some("unknown"));
        assert!(result.message_id.is_none());
        assert_eq!(
            result.error.as_deref(),
            Some("slack chat.postMessage reply carried no message ts")
        );
    }

    #[tokio::test]
    async fn card_status_failures_map_onto_the_send_taxonomy() {
        // (status, body, expected kind, retryable)
        let cases = [
            (
                400,
                json!({ "ok": false, "error": "invalid_blocks" }),
                "bad_format",
                false,
            ),
            (
                400,
                json!({ "ok": false, "error": "invalid_arguments" }),
                "bad_format",
                false,
            ),
            (
                403,
                json!({ "ok": false, "error": "not_in_channel" }),
                "forbidden",
                false,
            ),
            (
                404,
                json!({ "ok": false, "error": "channel_not_found" }),
                "not_found",
                false,
            ),
            (
                500,
                json!({ "ok": false, "error": "internal_error" }),
                "transient",
                true,
            ),
            (
                503,
                json!({ "error": "service_unavailable" }),
                "transient",
                true,
            ),
        ];
        for (status, payload, kind, retryable) in cases {
            let (adapter, mock) = adapter_with(vec![slack_text(status, &payload.to_string())]);
            let result = adapter
                .send_card("C123", &json!([{ "type": "divider" }]))
                .await;
            assert!(!result.success, "HTTP {status} must not report success");
            assert_eq!(
                result.error_kind.as_deref(),
                Some(kind),
                "HTTP {status} {payload}"
            );
            assert_eq!(result.retryable, retryable, "HTTP {status} {payload}");
            assert_eq!(mock.recorded().len(), 1, "HTTP {status} {payload}");
        }

        // A 429 is retryable and keeps the server's Retry-After.
        let (adapter, _mock) = adapter_with(vec![HttpResponse {
            status: 429,
            body: json!({ "ok": false, "error": "ratelimited" }).to_string(),
            retry_after: Some(9.0),
        }]);
        let result = adapter
            .send_card("C123", &json!([{ "type": "divider" }]))
            .await;
        assert_eq!(result.error_kind.as_deref(), Some("rate_limited"));
        assert!(result.retryable);
        assert_eq!(result.retry_after, Some(9.0));

        // A reply that is not JSON at all cannot be read as a delivery.
        let (adapter, _mock) = adapter_with(vec![slack_text(200, "<html>proxy</html>")]);
        let result = adapter
            .send_card("C123", &json!([{ "type": "divider" }]))
            .await;
        assert!(!result.success);
        assert_eq!(result.error_kind.as_deref(), Some("unknown"));
    }

    #[tokio::test]
    async fn a_card_transport_error_is_transient_and_retryable() {
        let adapter = SlackAdapter::new(Arc::new(DownTransport) as SharedTransport, "xoxb-test");
        let result = adapter
            .send_card("C123", &json!([{ "type": "divider" }]))
            .await;

        assert!(
            !result.success,
            "a request that never landed is no delivery"
        );
        assert_eq!(result.error_kind.as_deref(), Some("transient"));
        assert!(result.retryable);
        assert!(result.message_id.is_none());
        assert!(
            result
                .error
                .as_deref()
                .unwrap_or_default()
                .contains("connection reset"),
            "{:?}",
            result.error
        );
    }
}

#[cfg(test)]
mod webhook_tests {
    use super::*;
    use crate::transport::{HttpResponse, Transport, TransportError};
    use async_trait::async_trait;
    use std::sync::Arc;

    struct NoTransport;

    #[async_trait]
    impl Transport for NoTransport {
        async fn execute(&self, _request: HttpRequest) -> Result<HttpResponse, TransportError> {
            Err(TransportError::Request("unused".into()))
        }
    }

    fn adapter() -> SlackAdapter {
        SlackAdapter::new(Arc::new(NoTransport) as SharedTransport, "xoxb-test")
            .with_signing_secret("secret")
    }

    #[test]
    fn hmac_matches_the_rfc4231_vector() {
        assert_eq!(
            hmac_sha256_hex(b"Jefe", b"what do ya want for nothing?"),
            "5bdcc146bf60754e6a042426089575c75a003f089d2739839dec58b964ec3843"
        );
    }

    #[test]
    fn signatures_verify_and_tampering_fails() {
        let adapter = adapter();
        let body = br#"{"type":"event_callback"}"#;
        let timestamp = "1700000000";
        let signature = format!(
            "v0={}",
            hmac_sha256_hex(b"secret", b"v0:1700000000:{\"type\":\"event_callback\"}")
        );
        assert!(adapter.verify_webhook(body, timestamp, &signature));
        assert!(!adapter.verify_webhook(
            br#"{"type":"event_callback","x":1}"#,
            timestamp,
            &signature
        ));
        assert!(!adapter.verify_webhook(body, "1700000001", &signature));

        let unsigned = SlackAdapter::new(Arc::new(NoTransport) as SharedTransport, "x");
        assert!(!unsigned.verify_webhook(body, timestamp, &signature));
    }

    #[test]
    fn the_url_verification_challenge_is_answered() {
        let adapter = adapter();
        assert_eq!(
            adapter.webhook_challenge(r#"{"type":"url_verification","challenge":"abc"}"#),
            Some("abc".to_string())
        );
        assert_eq!(
            adapter.webhook_challenge(r#"{"type":"event_callback"}"#),
            None
        );
        assert_eq!(adapter.webhook_challenge("not json"), None);
    }

    #[test]
    fn events_become_messages_and_bots_are_skipped() {
        let adapter = adapter();
        let body = json!({
            "type": "event_callback",
            "event": {
                "type": "message",
                "channel": "C123",
                "user": "U1",
                "text": "hello slack",
                "ts": "1700000000.0001",
                "thread_ts": "1699999999.0001"
            }
        })
        .to_string();
        let events = adapter.parse_webhook(&body).unwrap();
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].text, "hello slack");
        assert_eq!(events[0].source.chat_type, "group");
        assert_eq!(events[0].source.chat_id, "C123");
        assert_eq!(events[0].user_id.as_deref(), Some("U1"));
        assert_eq!(
            events[0].reply_to_message_id.as_deref(),
            Some("1699999999.0001")
        );

        let dm = json!({"type":"event_callback","event":{"type":"message","channel":"D1","user":"U2","text":"dm","ts":"1"}}).to_string();
        assert_eq!(
            adapter.parse_webhook(&dm).unwrap()[0].source.chat_type,
            "dm"
        );

        let bot = json!({"type":"event_callback","event":{"type":"message","channel":"C1","user":"U3","text":"beep","bot_id":"B1"}}).to_string();
        assert!(adapter.parse_webhook(&bot).unwrap().is_empty());
        let edit = json!({"type":"event_callback","event":{"type":"message","subtype":"message_changed","channel":"C1","text":"x"}}).to_string();
        assert!(adapter.parse_webhook(&edit).unwrap().is_empty());
        let blank = json!({"type":"event_callback","event":{"type":"message","channel":"C1","user":"U","text":"  "}}).to_string();
        assert!(adapter.parse_webhook(&blank).unwrap().is_empty());
        assert!(
            adapter
                .parse_webhook(r#"{"type":"app_rate_limited"}"#)
                .unwrap()
                .is_empty()
        );
    }
}
