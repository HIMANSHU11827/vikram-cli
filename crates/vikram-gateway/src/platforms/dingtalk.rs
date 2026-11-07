//! DingTalk adapter — custom robot (group webhook) send path plus enterprise-callback parsing.
//!
//! Port of the parts of hermes-agent `plugins/platforms/dingtalk/adapter.py` that a stateless wire
//! adapter can own, retargeted at DingTalk's documented **custom robot** API instead of the
//! Stream-Mode SDK the hermes adapter is built on:
//!
//! - Outbound is one call per chunk: `POST {api_base}/robot/send?access_token=…` with
//!   `{"msgtype": "text", "text": {"content": …}}` (hermes' own `_standalone_send` uses exactly this
//!   payload shape against `DINGTALK_WEBHOOK_URL`).
//! - Signing is DingTalk's documented scheme: `string_to_sign = "{timestamp}\n{secret}"`,
//!   `sign = url_percent_encode(base64(HMAC-SHA256(secret, string_to_sign)))`, sent as
//!   `&timestamp={ms}&sign={sign}` next to the token. Base64 and percent-encoding are implemented
//!   in this file because the crate may not add dependencies; the hash comes from
//!   [`crate::pairing::sha256`].
//! - 20000-character chunking through [`truncate_message`] with [`codepoint_len`]
//!   (`MAX_MESSAGE_LENGTH`), chunks delivered sequentially in order.
//! - Failure taxonomy on `errcode`: token problems → `forbidden`, sign problems → `bad_format`,
//!   frequency limits / HTTP 429 → `rate_limited` (retryable, honoring `Retry-After`), anything
//!   else non-zero → `unknown`, 5xx / transport → `transient` retryable.
//! - `metadata["at"]` is forwarded verbatim as the robot payload's `at` block, so a caller can
//!   @-mention `atMobiles` or the whole group (`isAtAll`).
//! - [`DingTalkAdapter::parse_webhook`] reads the enterprise-bot callback body the roster posts
//!   (`text.content`, `senderStaffId`, `conversationId`, `msgId`, `conversationType`, `createAt`)
//!   into a [`MessageEvent`].
//!
//! Deliberate, documented limitations — each fails honestly rather than pretending:
//! - A custom robot has no message identity: `errcode: 0` is the whole acknowledgement, so every
//!   delivered chunk reports `""` as its id. `edit_message` / `delete_message` therefore cannot
//!   address anything (the trait's `delete_message` returns `()`; `edit_message` reports the
//!   unsupported operation), and `send_typing` is a no-op.
//! - `connect` cannot probe: `/robot/send` has no `getMe`-style endpoint, and the only call that
//!   proves a token works *is* a message delivery (a visible side effect). A non-empty
//!   `access_token` is the strongest cheap statement available.
//! - `get_chat_info` has no endpoint either; a robot webhook is bound to one group, so the chat id
//!   is echoed back as a group.
//! - Callback signature verification is **not** implemented. DingTalk's callback `timestamp`+`sign`
//!   header is computed from the enterprise app's `client_id`/`client_secret`, which a custom robot
//!   token (and this adapter) does not carry; inventing a check from the robot token would reject
//!   every real callback. The listener must authenticate those posts itself.
//!
//! Not ported from hermes (they need runtime state or SDKs this crate does not have): Stream-Mode
//! WebSocket transport (`dingtalk-stream`), per-message `session_webhook` replies and their expiry
//! cache, AI Cards (create/stream/finalize edits), emoji reactions (Thinking→Done), media
//! download-code resolution, `_normalize_markdown` (it exists for the markdown/card renderers; this
//! adapter writes plain `text`), message dedup, reconnect ladders, and the group gating knobs
//! (`require_mention`, `free_response_chats`, `allowed_chats`, `allowed_users`,
//! `mention_patterns`) which are access-control state owned by the caller.

use async_trait::async_trait;
use serde_json::{Value, json};

use crate::event::{MessageEvent, MessageType, Platform, SessionSource, platform_names};
use crate::helpers::{codepoint_len, truncate_message};
use crate::platform::{
    ChatInfo, MediaKind, MediaSource, OutboundMedia, PlatformAdapter, SendResult,
};
use crate::transport::{HttpRequest, SharedTransport};

/// DingTalk OpenAPI origin. The robot webhook lives at `{base}/robot/send`.
pub const DINGTALK_API_BASE: &str = "https://oapi.dingtalk.com";

/// DingTalk's per-message cap, counted in characters (`MAX_MESSAGE_LENGTH`).
pub const MAX_MESSAGE_LENGTH: usize = 20000;

/// `errcode` for "access token is invalid" (robot) — the token does not belong to a live robot.
const ERRCODE_TOKEN_INVALID: i64 = 130101;

/// `errcode` for "invalid access token" on the OpenAPI side (app key / token mismatch).
const ERRCODE_INVALID_TOKEN: i64 = 40014;

/// `errcode` for "sign does not match" — wrong secret, or a clock too far off for the timestamp.
const ERRCODE_SIGN_MISMATCH: i64 = 310000;

/// `errcode` for "send too fast" — DingTalk's per-robot frequency limit.
const ERRCODE_FREQUENCY_LIMITED: i64 = 130102;

/// Key under which an inbound callback's `createAt` is preserved on the event.
const CREATE_AT_KEY: &str = "create_at";

/// DingTalk custom-robot adapter.
///
/// Stateless: one shared [`crate::transport::Transport`] plus the robot's `access_token` and, when
/// the robot has "加签" (signing) enabled, its `secret`. Every call carries its own state, so two
/// calls never share mutable data.
pub struct DingTalkAdapter {
    transport: SharedTransport,
    access_token: String,
    secret: Option<String>,
    api_base: String,
}

impl DingTalkAdapter {
    /// Build an adapter for `access_token` over `transport`; `secret` is the robot's signing secret
    /// (`Some` only when the robot was created with signing enabled).
    pub fn new(
        transport: SharedTransport,
        access_token: impl Into<String>,
        secret: Option<String>,
    ) -> Self {
        Self {
            transport,
            access_token: access_token.into(),
            secret,
            api_base: DINGTALK_API_BASE.to_string(),
        }
    }

    /// Point the adapter at a mirror or a test double; a trailing `/` is dropped.
    pub fn with_api_base(mut self, base: impl Into<String>) -> Self {
        self.api_base = base.into().trim_end_matches('/').to_string();
        self
    }

    /// The signed (or unsigned) `/robot/send` URL for a call made "now".
    fn webhook_url(&self) -> String {
        self.webhook_url_at(now_millis())
    }

    /// The `/robot/send` URL for an explicit `timestamp_ms`.
    ///
    /// The token always rides as a query parameter — that *is* the robot's identity, which is why a
    /// custom robot cannot choose a destination: the group is bound to the token, not to `chat_id`.
    /// With a secret configured, DingTalk's signed-robot parameters are appended:
    /// `&timestamp={ms}&sign={url_percent_encode(base64(HMAC-SHA256(secret, "{ms}\n{secret}")))}`.
    fn webhook_url_at(&self, timestamp_ms: i64) -> String {
        let mut url = format!(
            "{}/robot/send?access_token={}",
            self.api_base,
            percent_encode(self.access_token.trim())
        );
        if let Some(secret) = self.secret.as_deref().filter(|secret| !secret.is_empty()) {
            url.push_str("&timestamp=");
            url.push_str(&timestamp_ms.to_string());
            url.push_str("&sign=");
            url.push_str(&percent_encode(&dingtalk_signature(secret, timestamp_ms)));
        }
        url
    }

    /// POST one robot payload and collapse the ack into `Ok(())` or a classified [`Failure`].
    ///
    /// DingTalk answers `{"errcode": 0, "errmsg": "ok"}` with HTTP 200 for an accepted message and
    /// carries the real verdict in `errcode`; a 2xx with `errcode: 0` (or with no `errcode` at all,
    /// as a bare mirror may answer) is success. Any other shape is a failure, classified once in
    /// [`Failure::into_send_result`].
    async fn call_robot(&self, payload: &Value) -> Result<(), Failure> {
        let request = HttpRequest::json(self.webhook_url(), payload);
        let response = self
            .transport
            .execute(request)
            .await
            .map_err(|err| Failure::Transport(err.to_string()))?;
        let parsed: Value = serde_json::from_str(&response.body).unwrap_or(Value::Null);
        let errcode = parsed.get("errcode").and_then(value_to_i64);
        if response.ok() && errcode.unwrap_or(0) == 0 {
            return Ok(());
        }
        let errmsg = parsed
            .get("errmsg")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|errmsg| !errmsg.is_empty())
            .map(str::to_string);
        let message = errmsg.unwrap_or_else(|| match errcode {
            Some(code) => format!("dingtalk robot returned errcode {code}"),
            None => format!("dingtalk robot returned HTTP {}", response.status),
        });
        Err(Failure::Api {
            status: response.status,
            retry_after: response.retry_after,
            errcode,
            message,
        })
    }

    /// Parse one enterprise-bot callback body into the events it carries.
    ///
    /// Outbound (custom) robots receive nothing, so this parses the body the *enterprise* app
    /// callback posts — the same fields hermes reads off `ChatbotMessage`: `text.content` (the
    /// message), `senderStaffId` / `senderNick` (author), `conversationId` (chat),
    /// `msgId` (message id), `conversationType` (`"1"` = dm, `"2"` = group) and `createAt`.
    ///
    /// One event is produced when `text.content` carries non-blank text, and anything else is
    /// skipped (an empty vector, not an error): DingTalk posts callbacks for events this adapter has
    /// no event shape for. A body that is not JSON at all is an `Err`.
    ///
    /// Callback signature verification is deliberately absent: DingTalk signs those POSTs with the
    /// enterprise app's credentials (its `timestamp` + `sign` header), which a robot token cannot
    /// reproduce. Verifying with the robot token would reject every genuine callback, so the check
    /// stays with whoever owns the listener (and the app secret) instead of being faked here.
    pub fn parse_webhook(&self, body: &str) -> Result<Vec<MessageEvent>, String> {
        let root: Value = serde_json::from_str(body)
            .map_err(|err| format!("dingtalk callback was not json: {err}"))?;
        if !root.is_object() {
            return Ok(Vec::new());
        }
        let content = root
            .get("text")
            .and_then(|text| text.get("content"))
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|content| !content.is_empty())
            .map(str::to_string);
        let Some(content) = content else {
            return Ok(Vec::new());
        };

        // Every callback field is optional in practice; without a conversation (or at least a
        // sender) there is nowhere to route the turn, so the event is dropped instead of invented.
        let field = |key: &str| {
            root.get(key)
                .and_then(Value::as_str)
                .map(str::trim)
                .filter(|value| !value.is_empty())
                .map(str::to_string)
        };
        let chat_id = field("conversationId")
            .or_else(|| field("senderStaffId"))
            .or_else(|| field("senderId"));
        let Some(chat_id) = chat_id else {
            return Ok(Vec::new());
        };

        // `conversationType` is a string on the wire ("1" dm, "2" group); a numeric form from a
        // mirror is accepted, and anything unknown is treated as a dm — hermes' group test is the
        // same single `== "2"` comparison.
        let conversation_type = root.get("conversationType").and_then(value_to_string);
        let is_group = conversation_type.as_deref() == Some("2");
        let user_id = field("senderStaffId").or_else(|| field("senderId"));
        let user_name = field("senderNick").or_else(|| user_id.clone());
        let message_id = field("msgId");

        let mut source = SessionSource::new(dingtalk_platform(), chat_id);
        source.chat_type = if is_group { "group" } else { "dm" }.to_string();
        source.chat_name = field("conversationTitle");
        source.user_id = user_id.clone();
        source.user_name = user_name.clone();
        source.message_id = message_id.clone();

        let mut event = MessageEvent::new(content, source);
        event.message_type = MessageType::Text;
        // hermes mirrors the author onto the event for per-message prompt builders.
        event.user_id = user_id;
        event.user_name = user_name;
        event.message_id = message_id;
        if let Some(create_at) = root.get("createAt") {
            event
                .metadata
                .insert(CREATE_AT_KEY.to_string(), create_at.clone());
        }
        event.raw_message = Some(root);
        Ok(vec![event])
    }
}

#[async_trait]
impl PlatformAdapter for DingTalkAdapter {
    /// `true` only when an `access_token` is configured.
    ///
    /// There is no cheap probe for a custom robot: DingTalk exposes no `getMe`-style endpoint, and
    /// the only request that proves the token works is a real `POST /robot/send` — a message the
    /// group would see. So this reports configuration, not liveness: a wrong-but-present token is
    /// caught on the first send, where the errcode taxonomy classifies it as `forbidden`.
    async fn connect(&mut self, _is_reconnect: bool) -> bool {
        !self.access_token.trim().is_empty()
    }

    /// Nothing is held client-side (no socket, no session), so there is nothing to tear down.
    async fn disconnect(&mut self) {}

    /// Chunk to [`MAX_MESSAGE_LENGTH`] characters and POST each chunk sequentially.
    ///
    /// `chat_id` cannot select a destination: a robot webhook is bound to one group by its token,
    /// so the argument is accepted for the trait contract and unused on the wire. `reply_to` is
    /// equally unusable (DingTalk robots have no reply-anchor parameter) and ignored. Blank content
    /// is refused as `bad_format` with no request, because an empty `text.content` is a DingTalk
    /// error rather than a message.
    ///
    /// A robot ack carries no message id, so every delivered chunk is recorded as `""`: the ids
    /// vector holds one empty string per chunk (its length is the delivered count), and the last of
    /// them — still `""` — is the reported `message_id`.
    async fn send(
        &self,
        _chat_id: &str,
        content: &str,
        _reply_to: Option<&str>,
        metadata: Option<Value>,
    ) -> SendResult {
        if content.trim().is_empty() {
            return SendResult::failed(
                "dingtalk rejects an empty text message body",
                Some("bad_format"),
            );
        }
        let at = at_block(metadata.as_ref());
        let chunks = truncate_message(content, MAX_MESSAGE_LENGTH, codepoint_len);
        let total = chunks.len();
        let mut delivered: Vec<String> = Vec::with_capacity(total);
        for chunk in &chunks {
            let mut payload = json!({
                "msgtype": "text",
                "text": { "content": chunk },
            });
            if let Some(at) = &at {
                payload["at"] = at.clone();
            }
            if let Err(failure) = self.call_robot(&payload).await {
                let mut result = failure.into_send_result();
                if !delivered.is_empty() {
                    // The chunks that landed are reported so a retry can start at the tail.
                    result = result.with_partial_overflow(delivered.len() as u32, total as u32);
                }
                result.continuation_message_ids = delivered;
                return result;
            }
            delivered.push(String::new());
        }
        match delivered.last() {
            Some(last) => SendResult::ok_split(last.clone(), delivered),
            None => SendResult::failed("dingtalk robot send produced no chunks", Some("unknown")),
        }
    }

    /// No endpoint: a robot webhook cannot look a chat up, and it only ever talks to the one group
    /// its token belongs to. The chat id is echoed back as a group so callers never lose routing.
    async fn get_chat_info(&self, chat_id: &str) -> ChatInfo {
        ChatInfo {
            name: chat_id.to_string(),
            chat_type: "group".to_string(),
        }
    }

    /// DingTalk robot webhooks have no attachment upload; the reference renders a remote image as
    /// markdown in the text body (`adapter.py:547-550`) and refuses local files
    /// (`adapter.py:552-558`). Same contract here: URL images only, bytes refused truthfully.
    fn supports_media(&self) -> bool {
        true
    }

    async fn send_media(
        &self,
        chat_id: &str,
        media: &OutboundMedia,
        reply_to: Option<&str>,
    ) -> SendResult {
        let _ = chat_id;
        match (&media.kind, &media.source) {
            (MediaKind::Image, MediaSource::Url(url)) if !url.trim().is_empty() => {
                let block = format!("![image]({})", url.trim());
                let content = match media.caption.as_deref().map(str::trim) {
                    Some(caption) if !caption.is_empty() => format!("{caption}\n\n{block}"),
                    _ => block,
                };
                let _ = reply_to;
                let payload = json!({
                    "msgtype": "markdown",
                    "markdown": {
                        "title": "Vikram",
                        "text": truncate_message(&content, MAX_MESSAGE_LENGTH, codepoint_len).join(""),
                    },
                });
                match self.call_robot(&payload).await {
                    Ok(()) => SendResult::ok(String::new()),
                    Err(failure) => failure.into_send_result(),
                }
            }
            (MediaKind::Image, MediaSource::Url(_)) => SendResult::failed(
                "dingtalk needs a non-empty public image URL",
                Some("bad_format"),
            ),
            (MediaKind::Image, MediaSource::Bytes(_)) => SendResult::failed(
                "DingTalk webhook replies cannot upload local images; provide a public image URL",
                Some("bad_format"),
            ),
            (_, MediaSource::Bytes(_)) => SendResult::failed(
                "DingTalk webhook replies cannot upload local files; provide a public URL",
                Some("bad_format"),
            ),
            (kind, MediaSource::Url(_)) => SendResult::failed(
                format!(
                    "DingTalk webhook replies only render images by URL; {kind:?} is not supported"
                ),
                Some("bad_format"),
            ),
        }
    }

    /// Text messages render verbatim (no markdown), so no formatting pass is applied.
    fn supports_code_blocks(&self) -> bool {
        false
    }

    /// `send` chunks natively; the delivery router must not truncate first.
    fn splits_long_messages(&self) -> bool {
        true
    }

    fn max_message_length(&self) -> usize {
        MAX_MESSAGE_LENGTH
    }

    /// No typing indicator endpoint exists for a robot webhook (hermes' `send_typing` is an empty
    /// method for the same reason), so this is a documented no-op.
    async fn send_typing(&self, _chat_id: &str, _metadata: Option<Value>) {}

    /// A custom robot cannot edit or recall a message it sent: `/robot/send` has no update verb and
    /// the recall/emotion APIs require an enterprise app access token, not a robot webhook token.
    /// This reports the unsupported operation instead of silently dropping the edit.
    async fn edit_message(
        &self,
        _chat_id: &str,
        _message_id: &str,
        _content: &str,
        _finalize: bool,
    ) -> SendResult {
        SendResult::failed(
            "dingtalk custom robots cannot edit a sent message: the robot webhook has no edit endpoint and /robot/recall needs an enterprise app token",
            Some("bad_format"),
        )
    }

    /// Same limitation as [`Self::edit_message`]: recall requires an enterprise app token. The trait
    /// returns `()`, so there is nothing to report — the message simply stays in the group.
    async fn delete_message(&self, _chat_id: &str, _message_id: &str) {}
}

/// How one robot call failed, carrying exactly what the taxonomy needs.
#[derive(Debug)]
enum Failure {
    /// The request never produced a response (connect/TLS/timeout): safe to retry.
    Transport(String),
    /// A response arrived but the call did not succeed.
    Api {
        status: u16,
        retry_after: Option<f64>,
        errcode: Option<i64>,
        message: String,
    },
}

impl Failure {
    /// The `SendResult` failure taxonomy.
    ///
    /// Precedence: a transport failure is `transient`; otherwise DingTalk's `errcode` decides when
    /// the body carried a non-zero one (token problems → `forbidden`, sign problems → `bad_format`,
    /// frequency limits → `rate_limited`, anything else → `unknown`); only when there is no errcode
    /// to consult does the HTTP status decide (429 → `rate_limited` with `Retry-After`, 5xx →
    /// `transient`, any other non-2xx → `unknown`).
    fn into_send_result(self) -> SendResult {
        match self {
            Self::Transport(err) => SendResult::failed(
                format!("dingtalk transport error: {err}"),
                Some("transient"),
            )
            .retryable(None),
            Self::Api {
                status,
                retry_after,
                errcode,
                message,
            } => match errcode.filter(|code| *code != 0) {
                Some(code) => match code {
                    ERRCODE_TOKEN_INVALID | ERRCODE_INVALID_TOKEN => {
                        SendResult::failed(message, Some("forbidden"))
                    }
                    ERRCODE_SIGN_MISMATCH => SendResult::failed(message, Some("bad_format")),
                    ERRCODE_FREQUENCY_LIMITED => {
                        SendResult::failed(message, Some("rate_limited")).retryable(retry_after)
                    }
                    _ => SendResult::failed(message, Some("unknown")),
                },
                None => match status {
                    429 => SendResult::failed(message, Some("rate_limited")).retryable(retry_after),
                    500..=599 => SendResult::failed(message, Some("transient")).retryable(None),
                    _ => SendResult::failed(message, Some("unknown")),
                },
            },
        }
    }
}

/// The robot payload's optional `at` block, forwarded verbatim from `metadata["at"]`.
///
/// Only an object is forwarded: DingTalk documents `{"atMobiles": [...], "isAtAll": bool}`, and a
/// scalar there would make the whole send fail validation.
fn at_block(metadata: Option<&Value>) -> Option<Value> {
    let at = metadata?.get("at")?;
    if at.is_object() {
        Some(at.clone())
    } else {
        None
    }
}

/// DingTalk's request signature: `base64(HMAC-SHA256(secret, "{timestamp_ms}\n{secret}"))`.
///
/// The caller percent-encodes this before putting it in the query string
/// (`sign` must be url-encoded exactly like the token).
pub fn dingtalk_signature(secret: &str, timestamp_ms: i64) -> String {
    let string_to_sign = format!("{timestamp_ms}\n{secret}");
    base64_encode(&hmac_sha256(secret.as_bytes(), string_to_sign.as_bytes()))
}

/// Milliseconds since the Unix epoch, clamped to `i64`; a pre-epoch clock signs as 0.
fn now_millis() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|elapsed| elapsed.as_millis().min(i64::MAX as u128) as i64)
        .unwrap_or(0)
}

/// HMAC-SHA-256 (FIPS 198-1) keyed by `key`. The hash lives in [`crate::pairing::sha256`]; this only
/// adds the padding construction.
fn hmac_sha256(key: &[u8], message: &[u8]) -> [u8; 32] {
    let mut block = [0u8; 64];
    if key.len() > 64 {
        block[..32].copy_from_slice(&crate::pairing::sha256::digest(key));
    } else {
        block[..key.len()].copy_from_slice(key);
    }
    let mut inner = Vec::with_capacity(64 + message.len());
    let mut outer = Vec::with_capacity(64 + 32);
    for byte in block {
        inner.push(byte ^ 0x36);
        outer.push(byte ^ 0x5c);
    }
    inner.extend_from_slice(message);
    let inner_hash = crate::pairing::sha256::digest(&inner);
    outer.extend_from_slice(&inner_hash);
    crate::pairing::sha256::digest(&outer)
}

/// Standard base64 (RFC 4648 §4) with `=` padding, implemented here because the crate may not add
/// dependencies.
fn base64_encode(data: &[u8]) -> String {
    const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(data.len().div_ceil(3) * 4);
    for group in data.chunks(3) {
        let first = group[0];
        let second = group.get(1).copied().unwrap_or(0);
        let third = group.get(2).copied().unwrap_or(0);
        let triple = (u32::from(first) << 16) | (u32::from(second) << 8) | u32::from(third);
        let quad = [
            (triple >> 18) & 0x3f,
            (triple >> 12) & 0x3f,
            (triple >> 6) & 0x3f,
            triple & 0x3f,
        ];
        // A 3-byte group always emits four characters; a short group pads with `=`.
        for (index, value) in quad.iter().enumerate() {
            if index <= group.len() {
                out.push(ALPHABET[*value as usize] as char);
            } else {
                out.push('=');
            }
        }
    }
    out
}

/// Percent-encode one query-string value: unreserved characters are kept, everything else `%XX`.
///
/// `sign` is base64, whose whole alphabet is reserved except alphanumerics — this is why it must be
/// encoded, and why a signed URL shows `%2B`, `%2F` and `%3D` inside the `sign` parameter.
fn percent_encode(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    for byte in value.bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(char::from(byte));
            }
            other => out.push_str(&format!("%{other:02X}")),
        }
    }
    out
}

/// A JSON number or numeric string as `i64` (DingTalk `errcode` is a number; mirrors may quote it).
fn value_to_i64(value: &Value) -> Option<i64> {
    match value {
        Value::Number(number) => number
            .as_i64()
            .or_else(|| number.as_u64().map(|value| value as i64)),
        Value::String(text) => text.trim().parse::<i64>().ok(),
        _ => None,
    }
}

/// A JSON string or number as an owned, trimmed string (DingTalk `conversationType` is `"1"`/`"2"`
/// but a mirror may send `1`/`2`).
fn value_to_string(value: &Value) -> Option<String> {
    match value {
        Value::String(text) => Some(text.trim().to_string()),
        Value::Number(number) => Some(number.to_string()),
        _ => None,
    }
}

/// The gateway's platform handle for this adapter. The constant is a fixed non-empty name, so the
/// fallback arm is unreachable and exists only to keep this function panic-free.
fn dingtalk_platform() -> Platform {
    match Platform::new(platform_names::DINGTALK) {
        Some(platform) => platform,
        None => Platform::local(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::transport::{HttpMethod, HttpResponse, Transport, TransportError};
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

        fn bodies(&self) -> Vec<Value> {
            self.requests().iter().map(request_body).collect()
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

    const TOKEN: &str = "TEST-TOKEN";
    const SECRET: &str = "my-secret";

    fn request_body(request: &HttpRequest) -> Value {
        let bytes = request.body.clone().unwrap_or_default();
        serde_json::from_slice(&bytes).unwrap_or(Value::Null)
    }

    fn chunk_text(request: &HttpRequest) -> String {
        request_body(request)["text"]["content"]
            .as_str()
            .unwrap_or_default()
            .to_string()
    }

    fn response(status: u16, body: Value) -> Result<HttpResponse, TransportError> {
        Ok(HttpResponse {
            status,
            body: body.to_string(),
            retry_after: None,
        })
    }

    fn ack() -> Result<HttpResponse, TransportError> {
        response(200, json!({ "errcode": 0, "errmsg": "ok" }))
    }

    fn errcode(code: i64, errmsg: &str) -> Result<HttpResponse, TransportError> {
        response(200, json!({ "errcode": code, "errmsg": errmsg }))
    }

    fn http_error(status: u16, retry_after: Option<f64>) -> Result<HttpResponse, TransportError> {
        Ok(HttpResponse {
            status,
            body: json!({ "errmsg": "gateway says no" }).to_string(),
            retry_after,
        })
    }

    fn adapter(mock: &Arc<MockTransport>) -> DingTalkAdapter {
        DingTalkAdapter::new(mock.clone(), TOKEN, None)
    }

    fn signed_adapter(mock: &Arc<MockTransport>) -> DingTalkAdapter {
        DingTalkAdapter::new(mock.clone(), TOKEN, Some(SECRET.to_string()))
    }

    /// The value of one query parameter, undecoded.
    fn query_param(url: &str, name: &str) -> Option<String> {
        let query = url.split_once('?')?.1;
        query.split('&').find_map(|pair| {
            let (key, value) = pair.split_once('=')?;
            (key == name).then(|| value.to_string())
        })
    }

    fn non_whitespace(text: &str) -> String {
        text.chars().filter(|ch| !ch.is_whitespace()).collect()
    }

    #[tokio::test]
    async fn send_posts_a_plain_text_message_to_the_robot_endpoint() {
        let mock = MockTransport::new(vec![ack()]);
        let result = adapter(&mock).send("cid-group", "hello", None, None).await;

        // `errcode: 0` is the whole acknowledgement: success, with an empty id.
        assert!(result.success);
        assert_eq!(result.message_id.as_deref(), Some(""));
        assert_eq!(result.continuation_message_ids, vec![String::new()]);
        assert_eq!(result.error, None);
        assert_eq!(result.error_kind, None);

        let requests = mock.requests();
        assert_eq!(requests.len(), 1);
        assert_eq!(requests[0].method, HttpMethod::Post);
        assert_eq!(
            requests[0].url,
            "https://oapi.dingtalk.com/robot/send?access_token=TEST-TOKEN"
        );
        assert!(
            requests[0]
                .headers
                .contains(&("Content-Type".to_string(), "application/json".to_string()))
        );
        assert_eq!(
            request_body(&requests[0]),
            json!({ "msgtype": "text", "text": { "content": "hello" } })
        );
        // No secret → no signed-robot parameters.
        assert!(query_param(&requests[0].url, "timestamp").is_none());
        assert!(query_param(&requests[0].url, "sign").is_none());
    }

    #[test]
    fn signed_url_matches_dingtalks_scheme() {
        // Base64(HMAC-SHA256("my-secret", "1700000000000\nmy-secret")), computed independently.
        let expected_sign = "/MY/oVQIff+ruG9eioj8sjGmwb0lCkY9/Pmw5vfsoM4=";
        assert_eq!(dingtalk_signature(SECRET, 1_700_000_000_000), expected_sign);

        let mock = MockTransport::new(Vec::new());
        let adapter = signed_adapter(&mock);
        assert_eq!(
            adapter.webhook_url_at(1_700_000_000_000),
            concat!(
                "https://oapi.dingtalk.com/robot/send?access_token=TEST-TOKEN",
                "&timestamp=1700000000000",
                "&sign=%2FMY%2FoVQIff%2BruG9eioj8sjGmwb0lCkY9%2FPmw5vfsoM4%3D"
            )
        );
        // The URL builder is pure, so it must not have touched the transport.
        assert!(mock.requests().is_empty());
    }

    #[test]
    fn signing_helpers_match_known_vectors() {
        assert_eq!(base64_encode(b""), "");
        assert_eq!(base64_encode(b"f"), "Zg==");
        assert_eq!(base64_encode(b"fo"), "Zm8=");
        assert_eq!(base64_encode(b"foo"), "Zm9v");
        assert_eq!(base64_encode(b"foob"), "Zm9vYg==");
        assert_eq!(base64_encode(b"fooba"), "Zm9vYmE=");
        assert_eq!(base64_encode(b"foobar"), "Zm9vYmFy");
        assert_eq!(base64_encode(b"hello world"), "aGVsbG8gd29ybGQ=");

        // RFC 4231 test case 2.
        let digest = hmac_sha256(b"Jefe", b"what do ya want for nothing?");
        let hex: String = digest.iter().map(|byte| format!("{byte:02x}")).collect();
        assert_eq!(
            hex,
            "5bdcc146bf60754e6a042426089575c75a003f089d2739839dec58b964ec3843"
        );

        assert_eq!(percent_encode("a-b_c.d~e"), "a-b_c.d~e");
        assert_eq!(percent_encode("a b&c/+/="), "a%20b%26c%2F%2B%2F%3D");
        // A key longer than the HMAC block is hashed first, and still produces a 32-byte tag.
        assert_eq!(hmac_sha256(&[0x0b; 80], b"msg").len(), 32);
    }

    #[tokio::test]
    async fn send_signs_only_when_a_secret_is_configured() {
        let mock = MockTransport::new(vec![ack()]);
        let result = signed_adapter(&mock)
            .send("cid", "signed hello", None, None)
            .await;
        assert!(result.success);
        assert_eq!(result.message_id.as_deref(), Some(""));

        let requests = mock.requests();
        assert_eq!(requests.len(), 1);
        let url = &requests[0].url;
        assert!(
            url.starts_with(
                "https://oapi.dingtalk.com/robot/send?access_token=TEST-TOKEN&timestamp="
            ),
            "{url}"
        );
        let timestamp = query_param(url, "timestamp").expect("timestamp parameter");
        let sign = query_param(url, "sign").expect("sign parameter");
        let timestamp_ms: i64 = timestamp.parse().expect("timestamp is milliseconds");
        // Millisecond resolution: a seconds-resolution stamp would be rejected by DingTalk.
        assert!(timestamp_ms > 1_000_000_000_000, "{timestamp_ms}");

        // The sign is percent-encoded base64: no reserved base64 character survives unencoded, and
        // every escape is a `%XX` triplet. A 32-byte HMAC always encodes to 44 characters ending in
        // one `=` of padding, so the encoded value deterministically ends with `%3D`.
        assert!(!sign.contains('+') && !sign.contains('/') && !sign.contains('='));
        assert!(sign.ends_with("%3D"), "{sign}");
        assert!(
            sign.chars()
                .all(|ch| ch.is_ascii_alphanumeric() || ch == '%')
        );
        assert_eq!(
            sign,
            percent_encode(&dingtalk_signature(SECRET, timestamp_ms))
        );
        // The body is unchanged by signing.
        assert_eq!(chunk_text(&requests[0]), "signed hello");
    }

    #[tokio::test]
    async fn long_content_is_chunked_sequentially() {
        let content: String = (0..600)
            .map(|idx| format!("line {idx} of a long dingtalk answer that has to be split\n"))
            .collect();
        let expected_chunks = truncate_message(&content, MAX_MESSAGE_LENGTH, codepoint_len);
        assert!(expected_chunks.len() > 1, "test content must be split");

        let mock = MockTransport::new(expected_chunks.iter().map(|_| ack()).collect());
        let result = adapter(&mock).send("cid", &content, None, None).await;

        assert!(result.success);
        let requests = mock.requests();
        assert_eq!(requests.len(), expected_chunks.len());
        for request in &requests {
            let text = chunk_text(request);
            assert!(
                codepoint_len(&text) <= MAX_MESSAGE_LENGTH,
                "chunk of {} chars exceeds the cap",
                codepoint_len(&text)
            );
            assert_eq!(request_body(request)["msgtype"], json!("text"));
        }
        // Chunking loses no visible content, and order is preserved.
        let joined: String = requests.iter().map(chunk_text).collect();
        assert_eq!(non_whitespace(&joined), non_whitespace(&content));
        // Every chunk reports the empty id DingTalk gives us; the vector length is the send count.
        assert_eq!(result.message_id.as_deref(), Some(""));
        assert_eq!(result.continuation_message_ids.len(), requests.len());
        assert!(result.continuation_message_ids.iter().all(String::is_empty));
    }

    #[tokio::test]
    async fn token_errcodes_are_forbidden() {
        for code in [ERRCODE_TOKEN_INVALID, ERRCODE_INVALID_TOKEN] {
            let mock = MockTransport::new(vec![errcode(code, "invalid access token")]);
            let result = adapter(&mock).send("cid", "hello", None, None).await;
            assert!(!result.success, "{code}");
            assert_eq!(result.error_kind.as_deref(), Some("forbidden"), "{code}");
            assert!(!result.retryable, "{code}");
            assert!(
                result
                    .error
                    .as_deref()
                    .unwrap_or_default()
                    .contains("token"),
                "{code}: {:?}",
                result.error
            );
        }
    }

    #[tokio::test]
    async fn sign_errcode_is_bad_format() {
        let mock = MockTransport::new(vec![errcode(
            ERRCODE_SIGN_MISMATCH,
            "sign not match, please check your sign",
        )]);
        let result = signed_adapter(&mock).send("cid", "hello", None, None).await;
        assert!(!result.success);
        assert_eq!(result.error_kind.as_deref(), Some("bad_format"));
        assert!(!result.retryable);
    }

    #[tokio::test]
    async fn frequency_and_transport_failures_are_classified() {
        // DingTalk's frequency limit: retryable, and the body carries no retry delay.
        let frequency = MockTransport::new(vec![errcode(
            ERRCODE_FREQUENCY_LIMITED,
            "send too fast, exceed 20 times per minute",
        )]);
        let result = adapter(&frequency).send("cid", "hello", None, None).await;
        assert_eq!(result.error_kind.as_deref(), Some("rate_limited"));
        assert!(result.retryable);
        assert_eq!(result.retry_after, None);

        // HTTP 429 honors the Retry-After the transport parsed.
        let throttled = MockTransport::new(vec![http_error(429, Some(7.0))]);
        let result = adapter(&throttled).send("cid", "hello", None, None).await;
        assert_eq!(result.error_kind.as_deref(), Some("rate_limited"));
        assert!(result.retryable);
        assert_eq!(result.retry_after, Some(7.0));

        // 5xx is transient, and so is a request that never landed.
        let server = MockTransport::new(vec![http_error(503, None)]);
        let result = adapter(&server).send("cid", "hello", None, None).await;
        assert_eq!(result.error_kind.as_deref(), Some("transient"));
        assert!(result.retryable);

        let down = MockTransport::new(vec![Err(TransportError::Request(
            "connection reset".to_string(),
        ))]);
        let result = adapter(&down).send("cid", "hello", None, None).await;
        assert_eq!(result.error_kind.as_deref(), Some("transient"));
        assert!(result.retryable);

        // An errcode this port does not know is reported as-is, not retried blindly.
        let other = MockTransport::new(vec![errcode(330001, "robot not found")]);
        let result = adapter(&other).send("cid", "hello", None, None).await;
        assert_eq!(result.error_kind.as_deref(), Some("unknown"));
        assert!(!result.retryable);
        assert!(
            result
                .error
                .as_deref()
                .unwrap_or_default()
                .contains("robot not found")
        );
    }

    #[tokio::test]
    async fn a_mid_way_failure_reports_the_chunks_already_delivered() {
        let content: String = (0..600)
            .map(|idx| format!("line {idx} of a long dingtalk answer that has to be split\n"))
            .collect();
        let mock = MockTransport::new(vec![ack(), errcode(ERRCODE_TOKEN_INVALID, "bad token")]);
        let result = adapter(&mock).send("cid", &content, None, None).await;

        assert!(!result.success);
        assert_eq!(result.error_kind.as_deref(), Some("forbidden"));
        assert_eq!(result.continuation_message_ids, vec![String::new()]);
        match result.partial_overflow() {
            Some((delivered, total)) => {
                assert_eq!(delivered, 1);
                assert!(total > 1, "the remaining chunks must be counted: {total}");
            }
            None => panic!("a failure after a delivered head must carry the partial marker"),
        }
    }

    #[tokio::test]
    async fn blank_content_is_rejected_without_a_request() {
        let mock = MockTransport::new(Vec::new());
        let result = adapter(&mock).send("cid", "   \n\t ", None, None).await;
        assert!(!result.success);
        assert_eq!(result.error_kind.as_deref(), Some("bad_format"));
        assert!(mock.requests().is_empty());

        let empty = MockTransport::new(Vec::new());
        assert!(!adapter(&empty).send("cid", "", None, None).await.success);
        assert!(empty.requests().is_empty());
    }

    #[tokio::test]
    async fn at_metadata_is_forwarded() {
        let mock = MockTransport::new(vec![ack(), ack()]);
        let adapter = adapter(&mock);
        let at = json!({ "atMobiles": ["13800138000"], "isAtAll": false });

        assert!(
            adapter
                .send("cid", "to one person", None, Some(json!({ "at": at })))
                .await
                .success
        );
        assert!(adapter.send("cid", "plain", None, None).await.success);

        let bodies = mock.bodies();
        assert_eq!(
            bodies[0]["at"],
            json!({ "atMobiles": ["13800138000"], "isAtAll": false })
        );
        assert_eq!(bodies[0]["msgtype"], json!("text"));
        // No at metadata → no `at` key at all (an empty one would be a validation error).
        assert!(bodies[1].get("at").is_none(), "{}", bodies[1]);

        // A non-object `at` is ignored rather than forwarded.
        assert_eq!(at_block(Some(&json!({ "at": "everyone" }))), None);
        assert_eq!(at_block(Some(&json!({}))), None);
        assert_eq!(at_block(None), None);
    }

    #[tokio::test]
    async fn get_chat_info_falls_back_to_the_chat_id() {
        let mock = MockTransport::new(Vec::new());
        let info = adapter(&mock).get_chat_info("cidAbCd==").await;
        assert_eq!(info.name, "cidAbCd==");
        assert_eq!(info.chat_type, "group");
        assert!(mock.requests().is_empty());
    }

    #[tokio::test]
    async fn parse_webhook_reads_dm_and_group_callbacks() {
        let mock = MockTransport::new(Vec::new());
        let adapter = adapter(&mock);

        let dm = json!({
            "conversationId": "cidDM",
            "conversationType": "1",
            "msgId": "msg-1",
            "senderId": "$:LWCP_v1:$abc",
            "senderStaffId": "staff-7",
            "senderNick": "Ann Lee",
            "createAt": 1_690_000_000_000_i64,
            "msgtype": "text",
            "text": { "content": "  hello there  " }
        });
        let events = adapter
            .parse_webhook(&dm.to_string())
            .unwrap_or_else(|error| panic!("dm callback rejected: {error}"));
        assert_eq!(events.len(), 1);
        let event = &events[0];
        assert_eq!(event.text, "hello there");
        assert_eq!(event.message_type, MessageType::Text);
        assert_eq!(event.source.platform.as_str(), "dingtalk");
        assert_eq!(event.source.chat_id, "cidDM");
        assert_eq!(event.source.chat_type, "dm");
        assert_eq!(event.source.user_id.as_deref(), Some("staff-7"));
        assert_eq!(event.source.user_name.as_deref(), Some("Ann Lee"));
        assert_eq!(event.user_id.as_deref(), Some("staff-7"));
        assert_eq!(event.user_name.as_deref(), Some("Ann Lee"));
        assert_eq!(event.message_id.as_deref(), Some("msg-1"));
        assert_eq!(event.source.message_id.as_deref(), Some("msg-1"));
        assert_eq!(
            event.metadata.get(CREATE_AT_KEY),
            Some(&json!(1_690_000_000_000_i64))
        );
        assert!(event.raw_message.is_some());

        let group = json!({
            "conversationId": "cidGroup",
            "conversationType": "2",
            "conversationTitle": "Team",
            "msgId": "msg-2",
            "senderStaffId": "staff-9",
            "text": { "content": "/new" }
        });
        let events = adapter
            .parse_webhook(&group.to_string())
            .unwrap_or_else(|error| panic!("group callback rejected: {error}"));
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].source.chat_type, "group");
        assert_eq!(events[0].source.chat_name.as_deref(), Some("Team"));
        // No `senderNick`: the staff id stands in, and `createAt` is absent rather than invented.
        assert_eq!(events[0].user_name.as_deref(), Some("staff-9"));
        assert!(!events[0].metadata.contains_key(CREATE_AT_KEY));
        // Nothing about a webhook post goes out over the network.
        assert!(mock.requests().is_empty());
    }

    #[tokio::test]
    async fn parse_webhook_skips_unrelated_payloads() {
        let mock = MockTransport::new(Vec::new());
        let adapter = adapter(&mock);

        for body in [
            "{}",
            "[]",
            r#""just a string""#,
            r#"{"msgtype":"text"}"#,
            r#"{"text":{}}"#,
            r#"{"text":{"content":"   "}}"#,
            // Text but no conversation and no sender: nowhere to route it.
            r#"{"text":{"content":"orphan"}}"#,
        ] {
            let events = adapter
                .parse_webhook(body)
                .unwrap_or_else(|error| panic!("{body} must be skipped, not rejected: {error}"));
            assert!(events.is_empty(), "{body} produced {events:?}");
        }

        assert!(adapter.parse_webhook("not json").is_err());
        assert!(mock.requests().is_empty());
    }

    #[tokio::test]
    async fn edit_and_delete_are_documented_no_ops() {
        let mock = MockTransport::new(Vec::new());
        let adapter = adapter(&mock);

        let edited = adapter
            .edit_message("cid", "msg-1", "rewritten", true)
            .await;
        assert!(!edited.success);
        assert_eq!(edited.error_kind.as_deref(), Some("bad_format"));
        assert!(
            edited
                .error
                .as_deref()
                .unwrap_or_default()
                .contains("cannot edit")
        );
        adapter.delete_message("cid", "msg-1").await;
        // Neither operation has a request to make.
        assert!(mock.requests().is_empty());
    }

    #[tokio::test]
    async fn connect_requires_a_non_empty_token() {
        let mock = MockTransport::new(Vec::new());
        let mut missing = DingTalkAdapter::new(mock.clone(), "", None);
        assert!(!missing.connect(false).await);
        let mut blank = DingTalkAdapter::new(mock.clone(), "   ", None);
        assert!(!blank.connect(true).await);
        let mut configured = DingTalkAdapter::new(mock.clone(), TOKEN, None);
        assert!(configured.connect(false).await);
        // No probe is attempted: a robot webhook has nothing cheap to call.
        assert!(mock.requests().is_empty());
    }

    #[tokio::test]
    async fn typing_is_a_no_op() {
        let mock = MockTransport::new(Vec::new());
        let adapter = adapter(&mock);
        adapter.send_typing("cid", None).await;
        adapter.send_typing("cid", Some(json!({ "at": {} }))).await;
        adapter.stop_typing("cid").await;
        assert!(mock.requests().is_empty());
    }

    #[tokio::test]
    async fn capability_flags_describe_a_chunking_text_robot() {
        let mock = MockTransport::new(Vec::new());
        let adapter = adapter(&mock);
        assert_eq!(adapter.max_message_length(), MAX_MESSAGE_LENGTH);
        assert_eq!(adapter.max_message_length(), 20000);
        assert!(adapter.splits_long_messages());
        assert!(!adapter.supports_code_blocks());
        assert!(!adapter.supports_status_text());
        assert!(!adapter.requires_edit_finalize());
        assert!(!adapter.supports_draft_streaming());
        // `format_message` is the identity: a `text` robot sends no markup.
        assert_eq!(adapter.format_message("**bold**"), "**bold**");
    }

    #[tokio::test]
    async fn media_image_url_is_sent_as_markdown() {
        let mock = MockTransport::new(vec![ack()]);
        let media = OutboundMedia::image_url("https://example.test/a.png").with_caption("look");
        let result = adapter(&mock).send_media("cid", &media, None).await;
        assert!(result.success);
        let body = &mock.bodies()[0];
        assert_eq!(body["msgtype"], json!("markdown"));
        assert_eq!(
            body["markdown"]["text"],
            json!("look\n\n![image](https://example.test/a.png)")
        );
    }

    #[tokio::test]
    async fn media_refuses_bytes_and_non_image_kinds_without_a_request() {
        for kind in [MediaKind::Document, MediaKind::Video, MediaKind::Audio] {
            let mock = MockTransport::new(Vec::new());
            let media = OutboundMedia {
                kind,
                source: MediaSource::Url("https://example.test/x".into()),
                filename: None,
                mime: None,
                caption: None,
            };
            let result = adapter(&mock).send_media("cid", &media, None).await;
            assert!(!result.success, "{kind:?} must be refused");
            assert_eq!(result.error_kind.as_deref(), Some("bad_format"));
            assert!(mock.requests().is_empty());
        }

        let mock = MockTransport::new(Vec::new());
        let media = OutboundMedia::new(MediaKind::Image, MediaSource::Bytes(vec![1, 2, 3]));
        let result = adapter(&mock).send_media("cid", &media, None).await;
        assert!(!result.success);
        assert!(mock.requests().is_empty());
    }

    #[tokio::test]
    async fn media_empty_url_is_refused_and_supports_media_is_true() {
        let mock = MockTransport::new(Vec::new());
        let adapter = adapter(&mock);
        assert!(adapter.supports_media());
        let result = adapter
            .send_media("cid", &OutboundMedia::image_url("   "), None)
            .await;
        assert!(!result.success);
        assert!(mock.requests().is_empty());
    }
}
