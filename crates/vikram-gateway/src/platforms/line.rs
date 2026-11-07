//! LINE adapter — the LINE Messaging API (`api.line.me`).
//!
//! Rust port of the wire half of hermes-agent `plugins/platforms/line/adapter.py`: the push and
//! reply endpoints, the `X-Line-Signature` webhook check, chat metadata via the profile /
//! group-summary / room-summary endpoints, the loading indicator and the webhook event parser.
//! Everything goes through [`crate::transport::Transport`], so the adapter is testable with no
//! network, no channel token and no LINE SDK (`line-bot-sdk` is not a dependency and is not
//! needed).
//!
//! Ported behaviour, with the hermes origin:
//! - `connect` → `GET /v2/bot/info` with `Authorization: Bearer <channel access token>`. True only
//!   when the channel answers 2xx **and** the payload carries a `userId`, which is the only way to
//!   learn that the token is live (`_LineClient.get_bot_user_id`).
//! - `send` → text chunked to [`MAX_MESSAGE_LENGTH`] (`LINE_PER_BUBBLE_CHARS`) and pushed through
//!   `POST /v2/bot/message/push`, batched at [`MAX_MESSAGES_PER_REQUEST`] message objects per HTTP
//!   request (`LINE_MAX_MESSAGES_PER_CALL`), sequentially, one request per batch.
//! - Failure taxonomy: `429` → `rate_limited` (retryable, honoring the `Retry-After` header),
//!   `401`/`403` → `forbidden`, `404` → `not_found`, other 4xx → `bad_format`, `5xx` and transport
//!   errors → `transient` (retryable).
//! - `get_chat_info` → `GET /v2/bot/profile/{userId}` for `U…` ids,
//!   `GET /v2/bot/group/{groupId}/summary` for `C…` ids and
//!   `GET /v2/bot/room/{roomId}/summary` for `R…` ids, degrading to `{name: chat_id, chat_type:
//!   "dm"}` when LINE refuses or the prefix is unknown.
//! - `send_typing` → `POST /v2/bot/chat/loading/start` (LINE's loading indicator), best effort.
//! - `verify_webhook` → `X-Line-Signature` is `base64(HMAC-SHA256(channel_secret, raw_body))`,
//!   compared in constant time (`verify_line_signature`).
//! - `parse_webhook` → `events[]` normalized into [`MessageEvent`]s (`_handle_message_event`).
//! - `reply` → `POST /v2/bot/message/reply`, the free single-use path that a webhook's
//!   `replyToken` unlocks. See [`LineAdapter::reply`] for why it bypasses the delivery ledger.
//! - `send_media` → the outbound media half of `send_image_file` / `send_video` / `send_voice`:
//!   LINE media messages are *references*, not uploads. Every object carries a public HTTPS URL
//!   LINE fetches itself (`originalContentUrl`, plus `previewImageUrl` for image and video and
//!   `duration` for audio), so a [`MediaSource::Url`](crate::platform::MediaSource::Url) becomes
//!   one such object and a caption rides a follow-up text message, exactly as
//!   `_send_messages(chat_id, msgs + [_text_message(caption)])` does (adapter.py:828). Byte
//!   sources and documents are refused truthfully — see [`LineAdapter::send_media`].
//!
//! Deliberately not ported (all of them need runtime state, a listener or media serving, none of
//! which belongs to a stateless wire adapter — the host owns them):
//! - the aiohttp webhook server, health probe, media file server and `LINE_PUBLIC_URL` plumbing;
//! - the slow-LLM postback button and its PENDING → READY → DELIVERED cache (`RequestCache`);
//! - the three allowlists, `LINE_ALLOW_ALL_USERS`, message dedup by `webhookEventId`, reply-token
//!   stashing with a TTL, and self-echo filtering through the bot's own `userId`;
//! - inbound media download (`api-data.line.me` → cache) and the outbound paths that need the
//!   reference's own public file server: byte uploads and document sends (see
//!   [`LineAdapter::send_media`]);
//! - `strip_markdown_preserving_urls` / `format_message`, the 4500-character safe-chunk budget and
//!   the ellipsis-on-overflow behaviour of `split_for_line` (this port chunks with
//!   [`truncate_message`] at the full 5000-character limit instead, so nothing is ellipsised);
//! - sticker and location placeholders (`[sticker]`, `[location: …]`): those payload kinds are
//!   skipped rather than turned into invented text.

use async_trait::async_trait;
use serde_json::{Value, json};

use crate::event::{MessageEvent, MessageType, Platform, SessionSource};
use crate::helpers::{codepoint_len, truncate_message};
use crate::platform::{
    ChatInfo, MediaKind, MediaSource, OutboundMedia, PlatformAdapter, SendResult,
};
use crate::transport::{HttpRequest, SharedTransport};

/// LINE Messaging API origin. Every endpoint below is `{base}/v2/bot/…`.
pub const LINE_API_BASE: &str = "https://api.line.me";

/// LINE's hard per-bubble cap in characters (`LINE_PER_BUBBLE_CHARS`).
///
/// hermes chunks at 4500 to keep a safety margin; this port uses the documented hard limit so no
/// content is dropped, and relies on [`truncate_message`] to keep every chunk at or below it.
pub const MAX_MESSAGE_LENGTH: usize = 5000;

/// LINE rejects a request carrying more than five message objects
/// (`LINE_MAX_MESSAGES_PER_CALL`), so a long answer is pushed as several requests.
pub const MAX_MESSAGES_PER_REQUEST: usize = 5;

/// LINE's documented cap, in characters, on the content URLs of a media message object
/// (`originalContentUrl`, `previewImageUrl` and their siblings).
///
/// hermes never checks it: `send_image_file` / `send_video` / `send_voice` hand LINE a URL its own
/// file server minted (`_serve_file` → `_media_url`, adapter.py:751-764), which is always short. A
/// caller-supplied URL is unbounded here, so the documented cap is enforced locally instead of
/// spending a request LINE would answer `400` for. See [`LineAdapter::send_media`].
pub const MAX_MEDIA_URL_CHARS: usize = 2000;

/// The audio `duration` sent when the caller names none.
///
/// The reference's `send_voice(chat_id, audio_path, duration_ms: int = 1000, …)`
/// (adapter.py:831) always writes `"duration": int(duration_ms)` (adapter.py:837), so the field is
/// never omitted from an audio object; [`OutboundMedia`] carries no duration, so this port sends
/// the reference's own 1000 ms default rather than dropping a required field.
pub const DEFAULT_AUDIO_DURATION_MS: u64 = 1000;

/// Channel info: proves the channel access token is live and yields the bot's own `userId`.
const BOT_INFO_PATH: &str = "/v2/bot/info";
/// Metered delivery to a chat (no reply token needed).
const PUSH_PATH: &str = "/v2/bot/message/push";
/// Free delivery that spends a webhook `replyToken`; single use, ~60 s TTL.
const REPLY_PATH: &str = "/v2/bot/message/reply";
/// The "typing" equivalent for 1:1 chats (`LINE_LOADING_URL`).
const LOADING_PATH: &str = "/v2/bot/chat/loading/start";
/// 1:1 chat profile; the id is a `U…` user id.
const PROFILE_PATH_PREFIX: &str = "/v2/bot/profile/";
/// Group summary; the id is a `C…` group id.
const GROUP_SUMMARY_PATH_PREFIX: &str = "/v2/bot/group/";
/// Room summary; the id is an `R…` room id.
const ROOM_SUMMARY_PATH_PREFIX: &str = "/v2/bot/room/";

/// LINE Messaging API adapter.
///
/// Holds one shared [`crate::transport::Transport`] plus the channel credentials; no connection
/// state is kept client-side (LINE talks HTTP only), so `send`/`reply` are usable from any task.
pub struct LineAdapter {
    transport: SharedTransport,
    channel_access_token: String,
    channel_secret: String,
    api_base: String,
}

impl LineAdapter {
    /// Build an adapter over `transport` for one LINE channel.
    ///
    /// `channel_access_token` authorizes every outbound call; `channel_secret` keys the webhook
    /// HMAC. Both come from the LINE console (Messaging API channel → "Channel access token" and
    /// "Channel secret").
    pub fn new(
        transport: SharedTransport,
        channel_access_token: impl Into<String>,
        channel_secret: impl Into<String>,
    ) -> Self {
        Self {
            transport,
            channel_access_token: channel_access_token.into(),
            channel_secret: channel_secret.into(),
            api_base: LINE_API_BASE.to_string(),
        }
    }

    /// Point the adapter at another origin (a regional endpoint or a test double). A trailing `/`
    /// is dropped so paths concatenate cleanly.
    pub fn with_api_base(mut self, base: impl Into<String>) -> Self {
        self.api_base = base.into().trim_end_matches('/').to_string();
        self
    }

    /// Absolute URL of one endpoint, e.g. `https://api.line.me/v2/bot/info`.
    fn api_url(&self, path: &str) -> String {
        format!("{}{path}", self.api_base)
    }

    /// `POST` a JSON body with the bearer token and interpret LINE's reply.
    async fn post_api(&self, path: &str, payload: &Value) -> Result<Value, LineFailure> {
        let request =
            HttpRequest::json(self.api_url(path), payload).bearer(&self.channel_access_token);
        self.execute(request).await
    }

    /// `GET` an endpoint with the bearer token and interpret LINE's reply.
    async fn get_api(&self, path: &str) -> Result<Value, LineFailure> {
        let request = HttpRequest::get(self.api_url(path)).bearer(&self.channel_access_token);
        self.execute(request).await
    }

    /// Run one request and turn the response into either the parsed body or a classified failure.
    ///
    /// A successful push answers `200` with an **empty** body, so an unparseable body is only an
    /// error when the status itself already failed; on success it degrades to `Value::Null`.
    async fn execute(&self, request: HttpRequest) -> Result<Value, LineFailure> {
        match self.transport.execute(request).await {
            Ok(response) => {
                let body: Value = serde_json::from_str(&response.body).unwrap_or(Value::Null);
                if response.ok() {
                    Ok(body)
                } else {
                    Err(failure_from(response.status, &body, response.retry_after))
                }
            }
            // No HTTP status at all: connection reset, timeout, DNS. Worth another attempt.
            Err(err) => Err(
                LineFailure::new(format!("line transport error: {err}"), "transient")
                    .retryable(None),
            ),
        }
    }

    /// Push one batch of already-built message objects (at most [`MAX_MESSAGES_PER_REQUEST`]) and
    /// return the id of every message object, in order.
    ///
    /// LINE answers `{"sentMessages": [{"id": "…"}, …]}`; when it answers with an empty body (a
    /// legal success) or with fewer ids than messages, the missing positions are reported as empty
    /// ids so the caller still sees one entry per delivered message.
    async fn push_messages(
        &self,
        chat_id: &str,
        messages: &[Value],
    ) -> Result<Vec<String>, LineFailure> {
        let payload = json!({ "to": chat_id, "messages": messages });
        let body = self.post_api(PUSH_PATH, &payload).await?;
        let ids = sent_message_ids(&body);
        Ok((0..messages.len())
            .map(|index| ids.get(index).cloned().unwrap_or_default())
            .collect())
    }

    /// Push one batch of already-built text chunks (at most [`MAX_MESSAGES_PER_REQUEST`]) and
    /// return the id of every message object, in order.
    async fn push_batch(
        &self,
        chat_id: &str,
        batch: &[String],
    ) -> Result<Vec<String>, LineFailure> {
        let messages: Vec<Value> = batch
            .iter()
            .map(|chunk| json!({ "type": "text", "text": chunk }))
            .collect();
        self.push_messages(chat_id, &messages).await
    }

    /// Deliver already-built message objects through a webhook `replyToken`.
    ///
    /// The reply endpoint reports no message id, so the caller only learns that LINE accepted the
    /// batch (any size up to [`MAX_MESSAGES_PER_REQUEST`], which is what the reference's
    /// `_send_messages` hands to `client.reply`, adapter.py:878).
    async fn reply_messages(
        &self,
        reply_token: &str,
        messages: &[Value],
    ) -> Result<(), LineFailure> {
        let payload = json!({ "replyToken": reply_token, "messages": messages });
        self.post_api(REPLY_PATH, &payload).await.map(|_| ())
    }

    /// Send `content`, already split into chunks, as sequential push requests.
    async fn push_chunks(&self, chat_id: &str, chunks: &[String]) -> SendResult {
        let mut delivered: Vec<String> = Vec::with_capacity(chunks.len());
        for batch in chunks.chunks(MAX_MESSAGES_PER_REQUEST) {
            match self.push_batch(chat_id, batch).await {
                Ok(ids) => delivered.extend(ids),
                // A later failure keeps what already landed: LINE has no transactional multi-post,
                // and the receipt (`continuation_message_ids` + `partial_overflow`) is what lets a
                // retry send only the tail.
                Err(failure) => {
                    return mark_partial(failure.into_send_result(), &delivered, chunks.len());
                }
            }
        }
        match delivered.last() {
            Some(last) => SendResult::ok_split(last.clone(), delivered),
            // Unreachable for non-empty content (it always yields at least one chunk), but never
            // report a phantom success.
            None => SendResult::failed("line push produced no chunks", Some("bad_format")),
        }
    }

    /// Deliver `content` through a webhook `replyToken`.
    ///
    /// Reply tokens are **free** but single-use and valid for roughly 60 seconds, so this path:
    ///
    /// - does **not** chunk: LINE replies exactly once, so content over [`MAX_MESSAGE_LENGTH`] is
    ///   refused with `bad_format` instead of being split (the caller can fall back to `send`);
    /// - bypasses the normal delivery ledger. The ledger retries, edits and re-sends, and none of
    ///   that can work here: the token is burned by the first attempt and expires on its own, so
    ///   there is nothing left to retry against. Callers that need retry semantics must use `send`.
    ///
    /// On success LINE answers `200` with an empty body and no message id, so the result reports
    /// success with `message_id: None` (LINE's messages cannot be edited or addressed afterwards).
    pub async fn reply(&self, reply_token: &str, content: &str) -> SendResult {
        if reply_token.trim().is_empty() {
            return SendResult::failed("line reply token is empty", Some("bad_format"));
        }
        if content.trim().is_empty() {
            return SendResult::failed("line reply content is empty", Some("bad_format"));
        }
        let length = codepoint_len(content);
        if length > MAX_MESSAGE_LENGTH {
            // No request: the token would be spent on a message LINE rejects, and a reply cannot
            // be split into several bubbles.
            return SendResult::failed(
                format!(
                    "line reply content is {length} characters, over the {MAX_MESSAGE_LENGTH}-character single-use reply limit"
                ),
                Some("bad_format"),
            );
        }
        let messages = [json!({ "type": "text", "text": content })];
        match self.reply_messages(reply_token, &messages).await {
            Ok(()) => success_without_id(),
            Err(failure) => failure.into_send_result(),
        }
    }

    /// Verify the `X-Line-Signature` header of a webhook delivery.
    ///
    /// LINE signs the **raw request body** with the channel secret:
    /// `X-Line-Signature = base64(HMAC-SHA256(channel_secret, raw_body))`. `body` must therefore be
    /// the bytes exactly as received — re-serializing the parsed JSON would change them and fail
    /// the check. The comparison is constant time, and an empty signature or empty secret fails
    /// closed.
    pub fn verify_webhook(&self, body: &[u8], signature: &str) -> bool {
        if signature.is_empty() || self.channel_secret.is_empty() {
            return false;
        }
        let expected = base64_encode(&hmac_sha256(self.channel_secret.as_bytes(), body));
        constant_time_eq(expected.as_bytes(), signature.as_bytes())
    }

    /// Parse one webhook delivery into normalized events.
    ///
    /// Only `type == "message"` events are kept (follow/unfollow/join/leave/postback/beacon carry
    /// no turn), and of those only text payloads plus the four media kinds
    /// (`image`/`video`/`audio`/`file`) — the media kinds carry the LINE content id
    /// (`message.id`) in `media_urls` and the LINE message type in `media_types`, because fetching
    /// the bytes needs a second call to `api-data.line.me` that the host owns. Unparseable JSON is
    /// an `Err` so a caller can answer 400; a body without `events` is an empty batch.
    ///
    /// The chat is derived from the source block: `groupId` → group, `roomId` → group, `userId` →
    /// dm. Events without a resolvable chat id, or whose payload kind this port does not model,
    /// are skipped.
    pub fn parse_webhook(&self, body: &str) -> Result<Vec<MessageEvent>, String> {
        let root: Value = serde_json::from_str(body)
            .map_err(|err| format!("line webhook body is not json: {err}"))?;
        let Some(events) = root.get("events").and_then(Value::as_array) else {
            return Ok(Vec::new());
        };
        Ok(events.iter().filter_map(parse_event).collect())
    }
}

#[async_trait]
impl PlatformAdapter for LineAdapter {
    /// `GET /v2/bot/info`: true only for a 2xx answer carrying the bot's own `userId`.
    ///
    /// `is_reconnect` needs no different handling: the adapter holds no session, queue or offset,
    /// so a reconnect is just another token check.
    async fn connect(&mut self, _is_reconnect: bool) -> bool {
        match self.get_api(BOT_INFO_PATH).await {
            Ok(body) => body
                .get("userId")
                .and_then(Value::as_str)
                .is_some_and(|user_id| !user_id.trim().is_empty()),
            Err(_) => false,
        }
    }

    /// Nothing is held client-side (no listener, no cache, no socket), so there is nothing to tear
    /// down.
    async fn disconnect(&mut self) {}

    /// Chunk, batch and push.
    ///
    /// Blank content is refused locally with `bad_format` and never reaches the wire (LINE rejects
    /// an empty text object). `reply_to` is accepted and ignored: a push cannot quote a message —
    /// only the single-use `replyToken` path can reply to the message that opened the turn, and
    /// that lives in [`LineAdapter::reply`]. `metadata` is likewise unused; hermes reads it only
    /// for the postback cache this port does not have.
    async fn send(
        &self,
        chat_id: &str,
        content: &str,
        _reply_to: Option<&str>,
        _metadata: Option<Value>,
    ) -> SendResult {
        if content.trim().is_empty() {
            return SendResult::failed("line message content is empty", Some("bad_format"));
        }
        let chunks = truncate_message(content, MAX_MESSAGE_LENGTH, codepoint_len);
        self.push_chunks(chat_id, &chunks).await
    }

    /// Media sending is implemented for the three kinds LINE's message objects cover — image,
    /// video and audio.
    ///
    /// `/v2/bot/message/push` and `/v2/bot/message/reply` carry media message objects, so text and
    /// media share one endpoint and one auth path. What LINE accepts is narrower than the name
    /// suggests: every media object references a public HTTPS URL that LINE fetches itself, and
    /// this port has no file server, so only [`MediaSource::Url`] is deliverable and documents are
    /// refused — see [`send_media`](Self::send_media).
    fn supports_media(&self) -> bool {
        true
    }

    /// Deliver one attachment as the LINE media message object its [`MediaKind`] maps to.
    ///
    /// The wire shapes are the reference's, object for object:
    ///
    /// - image → `{"type":"image","originalContentUrl":url,"previewImageUrl":url}` — the same URL
    ///   in both slots (`send_image_file`, adapter.py:827);
    /// - video → `{"type":"video","originalContentUrl":url,"previewImageUrl":url}`
    ///   (`send_video`, adapter.py:860);
    /// - audio → `{"type":"audio","originalContentUrl":url,"duration":1000}`
    ///   (`send_voice`, adapter.py:837; the `1000` is the reference's own `duration_ms` default at
    ///   adapter.py:831 — see [`DEFAULT_AUDIO_DURATION_MS`]);
    /// - document → a refusal. The reference has **no** outbound document path at all:
    ///   `_OUTBOUND_MEDIA` lists `image`/`audio`/`video` only (adapter.py:363-369) and
    ///   `send_document` / `send_file` do not exist in the LINE adapter, so this port refuses
    ///   rather than invent an unvalidated `file` object.
    ///
    /// The video preview is a real compromise the reference papers over with its own file server:
    /// it takes a separate `preview_path`, and without one it writes a stdlib 1×1 PNG to a temp
    /// file and serves it (`send_video`, adapter.py:846-859). This adapter has no public file
    /// server and [`OutboundMedia`] carries no preview reference, so the video URL doubles as the
    /// preview URL and LINE's own validation decides — a URL LINE cannot fetch as an image comes
    /// back as a `400` and surfaces as `bad_format`, never as a fake success.
    ///
    /// **Source rules.** [`MediaSource::Url`] is pushed as-is after the reference's checks: an
    /// empty URL is refused (no request), a non-`https://` URL is refused — hermes rejects the one
    /// it minted with `LINE image URL must be HTTPS: {url}` (adapter.py:825-826) and applies that
    /// to the image path, while LINE's API requires HTTPS for every content URL, so the rule is
    /// enforced uniformly here — and a URL over LINE's documented [`MAX_MEDIA_URL_CHARS`] cap is
    /// refused locally. [`MediaSource::Bytes`] is a truthful refusal naming
    /// [`MediaSource::Url`]: the reference serves local files from its own host
    /// (`_serve_file`/`_media_url`, adapter.py:751-764, gated on `LINE_PUBLIC_URL` at
    /// adapter.py:766-770), which this stateless adapter does not have.
    ///
    /// **Caption.** A media message object has no caption field, so the reference appends the
    /// caption as a *follow-up text message in the same request*
    /// (`msgs + [_text_message(caption)]`, adapter.py:828); this port does the same for every kind,
    /// because the wire mechanism is identical for all four and dropping a caller's caption on the
    /// floor would lose text the caller asked to send. The caption is capped exactly like
    /// `_text_message` (adapter.py:283): at [`MAX_MESSAGE_LENGTH`] with an ellipsis, so it always
    /// fits one bubble. A blank or whitespace-only caption is dropped — LINE rejects an empty text
    /// object, as [`send`](Self::send) already refuses blank content.
    ///
    /// **Reply anchor.** `reply_to` carries a webhook `replyToken`, LINE's only reply mechanism,
    /// and is spent on `POST /v2/bot/message/reply`; a blank or absent anchor is the no-anchor case
    /// and pushes. That is the same token contract as [`LineAdapter::reply`], and matches the
    /// reference's reply-token-first delivery (`_send_messages`, adapter.py:874-883). Unlike the
    /// reference, a rejected token is **not** retried as a push: the token is single-use, and a
    /// push fallback could deliver the same media twice when the reply landed but its response was
    /// lost. The caller can retry through [`send`](Self::send).
    ///
    /// Failures use the shared taxonomy ([`failure_from`]) — `429` → `rate_limited` (retryable,
    /// honoring `Retry-After`), `401`/`403` → `forbidden`, `404` → `not_found`, other `4xx` →
    /// `bad_format`, `5xx`/transport → `transient` — and a refusal never reports delivery.
    async fn send_media(
        &self,
        chat_id: &str,
        media: &OutboundMedia,
        reply_to: Option<&str>,
    ) -> SendResult {
        let messages = match media_messages(media) {
            Ok(messages) => messages,
            Err(refusal) => return *refusal,
        };
        match reply_to.map(str::trim).filter(|token| !token.is_empty()) {
            Some(reply_token) => match self.reply_messages(reply_token, &messages).await {
                Ok(()) => success_without_id(),
                Err(failure) => failure.into_send_result(),
            },
            None => match self.push_messages(chat_id, &messages).await {
                Ok(ids) => {
                    // The last id is the newest object, i.e. the caption when there is one; an empty
                    // body is a legal LINE success and reports the empty id a text push reports.
                    let last = ids.last().cloned().unwrap_or_default();
                    SendResult::ok_split(last, ids)
                }
                Err(failure) => failure.into_send_result(),
            },
        }
    }

    /// Chat metadata from the endpoint the id prefix selects.
    ///
    /// `U…` → `GET /v2/bot/profile/{userId}` (`displayName`), `C…` →
    /// `GET /v2/bot/group/{groupId}/summary` (`groupName`), `R…` →
    /// `GET /v2/bot/room/{roomId}/summary` (`roomName`). Groups and rooms are both reported as
    /// `group`. Any failure — unknown prefix, 404, 403, transport — degrades to
    /// `{name: chat_id, chat_type: "dm"}` so callers never lose routing.
    async fn get_chat_info(&self, chat_id: &str) -> ChatInfo {
        let fallback = ChatInfo {
            name: chat_id.to_string(),
            chat_type: "dm".to_string(),
        };
        let (path, chat_type) = match chat_id.chars().next() {
            Some('U') => (
                format!("{PROFILE_PATH_PREFIX}{}", encode_path_segment(chat_id)),
                "dm",
            ),
            Some('C') => (
                format!(
                    "{GROUP_SUMMARY_PATH_PREFIX}{}/summary",
                    encode_path_segment(chat_id)
                ),
                "group",
            ),
            Some('R') => (
                format!(
                    "{ROOM_SUMMARY_PATH_PREFIX}{}/summary",
                    encode_path_segment(chat_id)
                ),
                "group",
            ),
            _ => return fallback,
        };
        match self.get_api(&path).await {
            Ok(body) => ChatInfo {
                name: summary_name(&body).unwrap_or_else(|| chat_id.to_string()),
                chat_type: chat_type.to_string(),
            },
            Err(_) => fallback,
        }
    }

    /// `send` chunks natively at [`MAX_MESSAGE_LENGTH`], so the router must not truncate first.
    fn splits_long_messages(&self) -> bool {
        true
    }

    fn max_message_length(&self) -> usize {
        MAX_MESSAGE_LENGTH
    }

    /// LINE chat bubbles are plain text: they render `**`, `#` and fences literally. The delivery
    /// layer still gets a valid, correctly measured chunk; only the styling is absent.
    fn supports_code_blocks(&self) -> bool {
        false
    }

    /// `POST /v2/bot/chat/loading/start`, LINE's loading animation.
    ///
    /// It is a UI hint and LINE documents it for 1:1 chats only, so every failure (including the
    /// rejection a group or room id gets) is swallowed. hermes additionally gates on a `U` prefix
    /// and sends a clamped `loadingSeconds`; the port sends the documented minimum body and lets
    /// LINE decide.
    async fn send_typing(&self, chat_id: &str, _metadata: Option<Value>) {
        if chat_id.trim().is_empty() {
            return;
        }
        let payload = json!({ "chatId": chat_id });
        let _ = self.post_api(LOADING_PATH, &payload).await;
    }

    /// LINE has no "stop typing" call; the loading animation clears itself.
    async fn stop_typing(&self, _chat_id: &str) {}
}

// ----- failure taxonomy -----

/// One failed LINE call, carrying exactly what the send taxonomy needs.
struct LineFailure {
    message: String,
    kind: &'static str,
    retryable: bool,
    retry_after: Option<f64>,
}

impl LineFailure {
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

/// Map an HTTP status onto the failure taxonomy, keeping LINE's own error text.
///
/// LINE error bodies look like `{"message": "The request body has 1 error(s)", "details":
/// [{"message": "…", "property": "messages[0].text"}]}`; both halves are folded into one
/// human-readable string.
fn failure_from(status: u16, body: &Value, retry_after: Option<f64>) -> LineFailure {
    let message = error_message(body, status);
    match status {
        // The only status LINE asks us to back off for; it sends `Retry-After` when it knows how
        // long, which the transport already parsed.
        429 => LineFailure::new(message, "rate_limited").retryable(retry_after),
        401 | 403 => LineFailure::new(message, "forbidden"),
        404 => LineFailure::new(message, "not_found"),
        400..=499 => LineFailure::new(message, "bad_format"),
        // 5xx plus anything unexpected: the request is well formed, the far side is not healthy.
        _ => LineFailure::new(message, "transient").retryable(retry_after),
    }
}

/// Human-readable failure text: LINE's `message`, plus the first `details[].message` when present.
fn error_message(body: &Value, status: u16) -> String {
    let summary = text_field(body, "message");
    let detail = body
        .get("details")
        .and_then(Value::as_array)
        .and_then(|details| details.first())
        .and_then(|entry| text_field(entry, "message"));
    match (summary, detail) {
        (Some(summary), Some(detail)) => format!("line api error: {summary} ({detail})"),
        (Some(summary), None) => format!("line api error: {summary}"),
        (None, Some(detail)) => format!("line api error: {detail}"),
        (None, None) => format!("line api returned status {status}"),
    }
}

/// A non-blank string field of `object`, or `None`.
fn text_field(object: &Value, field: &str) -> Option<String> {
    object
        .get(field)
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(str::to_string)
}

/// Ids of the messages LINE accepted, in order (`sentMessages[].id`).
fn sent_message_ids(body: &Value) -> Vec<String> {
    body.get("sentMessages")
        .and_then(Value::as_array)
        .map(|entries| {
            entries
                .iter()
                .map(|entry| {
                    entry
                        .get("id")
                        .and_then(Value::as_str)
                        .unwrap_or_default()
                        .to_string()
                })
                .collect()
        })
        .unwrap_or_default()
}

/// A success that produced no message id (LINE's reply endpoint reports none).
fn success_without_id() -> SendResult {
    SendResult {
        message_id: None,
        ..SendResult::ok(String::new())
    }
}

/// Attach the ids of an already-delivered head to a failure so a retry sends only the tail
/// (`_with_partial_send`).
fn mark_partial(mut result: SendResult, delivered: &[String], total: usize) -> SendResult {
    if delivered.is_empty() {
        return result;
    }
    result = result.with_partial_overflow(delivered.len() as u32, total as u32);
    result.continuation_message_ids = delivered.to_vec();
    result
}

/// The display name of a profile (`displayName`), group (`groupName`) or room (`roomName`) body.
fn summary_name(body: &Value) -> Option<String> {
    ["displayName", "groupName", "roomName"]
        .iter()
        .find_map(|field| text_field(body, field))
}

// ----- outbound media -----

/// Build the LINE message objects for one outbound attachment, or the refusal that ends the send
/// before any request is made.
///
/// Returned order is the reference's: the media object first, then the caption's follow-up text
/// message when there is one (`send_image_file`, adapter.py:827-828). Two objects always fit the
/// [`MAX_MESSAGES_PER_REQUEST`] budget, so the whole attachment rides one request.
///
/// The refusals are the three cases the reference cannot express either:
///
/// - [`MediaSource::Bytes`] — LINE fetches media by URL, and the reference publishes local files
///   through its own HTTPS server (`_serve_file`/`_media_url`, adapter.py:751-764) which does not
///   exist here;
/// - [`MediaKind::Document`] — the reference has no document send at all (`_OUTBOUND_MEDIA`,
///   adapter.py:363-369);
/// - a URL that is empty, not HTTPS (adapter.py:825-826) or longer than LINE's documented
///   [`MAX_MEDIA_URL_CHARS`] cap.
///
/// The refusal is boxed because [`SendResult`] is a wide struct and a `Result` carrying it inline
/// makes every fallible call site larger than the success path needs.
fn media_messages(media: &OutboundMedia) -> Result<Vec<Value>, Box<SendResult>> {
    let url = match &media.source {
        MediaSource::Url(url) => url.trim(),
        MediaSource::Bytes(bytes) => {
            return Err(Box::new(SendResult::failed(
                format!(
                    "line send media: refusing {} bytes of MediaSource::Bytes — LINE media messages \
                     reference a public HTTPS URL (`originalContentUrl`, adapter.py:827/837/860) that \
                     LINE fetches itself, and the reference mints one only from its own file server \
                     (`_serve_file` → `_media_url`, adapter.py:751-764, gated on `LINE_PUBLIC_URL` at \
                     adapter.py:766-770). This adapter has no public file server, so the bytes have \
                     nowhere to be served from: pass a MediaSource::Url that LINE can fetch",
                    bytes.len()
                ),
                Some("bad_format"),
            )));
        }
    };
    if url.is_empty() {
        return Err(Box::new(SendResult::failed(
            "line send media: the media url is empty",
            Some("bad_format"),
        )));
    }
    // The reference's `if not url.lower().startswith("https://")` check (adapter.py:825-826),
    // applied to every kind because LINE's API requires HTTPS for every content URL.
    if !url
        .get(..8)
        .is_some_and(|scheme| scheme.eq_ignore_ascii_case("https://"))
    {
        return Err(Box::new(SendResult::failed(
            format!(
                "line send media: LINE fetches media over HTTPS only, so the url must start with \
                 https:// (adapter.py:825-826); got {url}"
            ),
            Some("bad_format"),
        )));
    }
    let url_chars = codepoint_len(url);
    if url_chars > MAX_MEDIA_URL_CHARS {
        return Err(Box::new(SendResult::failed(
            format!(
                "line send media: the media url is {url_chars} characters, over LINE's \
                 {MAX_MEDIA_URL_CHARS}-character content-url limit"
            ),
            Some("bad_format"),
        )));
    }

    let message = match media.kind {
        // `send_image_file`: LINE renders the preview and the full image from the same URL when
        // only one is known (adapter.py:827).
        MediaKind::Image => json!({
            "type": "image",
            "originalContentUrl": url,
            "previewImageUrl": url,
        }),
        // `send_video`: `originalContentUrl` is the video, `previewImageUrl` the thumbnail
        // (adapter.py:860). The reference prefers a supplied `preview_path` and otherwise serves a
        // 1×1 PNG it writes itself (adapter.py:846-859); with no file server here, the media URL
        // doubles as the preview and LINE validates it.
        MediaKind::Video => json!({
            "type": "video",
            "originalContentUrl": url,
            "previewImageUrl": url,
        }),
        // `send_voice`: always carries `duration` (adapter.py:837), defaulting to 1000 ms.
        MediaKind::Audio => json!({
            "type": "audio",
            "originalContentUrl": url,
            "duration": DEFAULT_AUDIO_DURATION_MS,
        }),
        MediaKind::Document => {
            return Err(Box::new(SendResult::failed(
                "line send media: LINE documents have no implementation to follow — hermes' LINE \
                 adapter sends images, video and audio only (`_OUTBOUND_MEDIA`, adapter.py:363-369) \
                 and defines no send_document/send_file, so this adapter refuses to post an \
                 invented file object; send the document as text, or as a link in a message",
                Some("bad_format"),
            )));
        }
    };

    let mut messages = vec![message];
    if let Some(caption) = media.caption.as_deref().and_then(caption_text) {
        messages.push(json!({ "type": "text", "text": caption }));
    }
    Ok(messages)
}

/// The caption as the reference's follow-up text message (`_text_message`, adapter.py:281-283), or
/// `None` when there is nothing to send.
///
/// A blank or whitespace-only caption is dropped: LINE rejects an empty text object, and
/// [`LineAdapter::send`] already refuses blank content for the same reason. A caption longer than
/// [`MAX_MESSAGE_LENGTH`] is cut to the cap minus one character plus `…`, exactly the truncation
/// `_text_message` applies (`LINE_PER_BUBBLE_CHARS` is the same 5000-character cap).
fn caption_text(caption: &str) -> Option<String> {
    let trimmed = caption.trim();
    if trimmed.is_empty() {
        return None;
    }
    if codepoint_len(trimmed) <= MAX_MESSAGE_LENGTH {
        return Some(trimmed.to_string());
    }
    let mut capped: String = trimmed.chars().take(MAX_MESSAGE_LENGTH - 1).collect();
    capped.push('…');
    Some(capped)
}

// ----- inbound parsing -----

/// Percent-encode one path segment. LINE ids are alphanumeric, but never assume: `%XX` for
/// everything outside the unreserved set keeps a hostile id from rewriting the URL.
fn encode_path_segment(value: &str) -> String {
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

/// The gateway's platform handle for this adapter. The constant is a fixed non-empty name, so the
/// fallback arm is unreachable and exists only to keep this function panic-free.
fn line_platform() -> Platform {
    Platform::new("line").unwrap_or_else(Platform::local)
}

/// `(chat_id, chat_type)` from a LINE source block.
///
/// A source carries exactly one of the three ids: `groupId` (`C…`) for a group, `roomId` (`R…`) for
/// a room, `userId` (`U…`) for a 1:1 chat. Group and room both normalize to `group`; the group and
/// room checks come first because both kinds also name the `userId` that spoke.
fn resolve_chat(source: &Value) -> Option<(String, &'static str)> {
    for (field, chat_type) in [("groupId", "group"), ("roomId", "group")] {
        if let Some(id) = text_field(source, field) {
            return Some((id, chat_type));
        }
    }
    text_field(source, "userId").map(|id| (id, "dm"))
}

/// LINE message `type` → gateway [`MessageType`].
///
/// `audio` maps to [`MessageType::Voice`] because LINE's audio messages are recorded voice clips,
/// the same transcription path Telegram and WhatsApp take (hermes `_LINE_MESSAGE_TYPES`). Every
/// other payload kind returns `None`, which makes [`parse_event`] skip the event.
fn media_message_type(kind: &str) -> Option<MessageType> {
    match kind {
        "text" => Some(MessageType::Text),
        "image" => Some(MessageType::Photo),
        "video" => Some(MessageType::Video),
        "audio" => Some(MessageType::Voice),
        "file" => Some(MessageType::Document),
        _ => None,
    }
}

/// Normalize one webhook `events[]` entry, or `None` when it carries no turn this port models.
///
/// Port of hermes `_handle_message_event` minus media download: the LINE content id becomes the
/// attachment reference and the LINE message type becomes its MIME-ish kind, so a host that can
/// fetch `api-data.line.me` knows exactly what to ask for.
fn parse_event(event: &Value) -> Option<MessageEvent> {
    if event.get("type").and_then(Value::as_str) != Some("message") {
        return None;
    }
    let message = event.get("message").filter(|message| message.is_object())?;
    let kind = message.get("type").and_then(Value::as_str)?;
    let message_type = media_message_type(kind)?;
    let source = event.get("source").filter(|source| source.is_object());
    let (chat_id, chat_type) = resolve_chat(source?)?;

    let message_id = text_field(message, "id");
    let user_id = source.and_then(|source| text_field(source, "userId"));
    let text = match message_type {
        MessageType::Text => message
            .get("text")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string(),
        // A media turn has no text of its own; the placeholder keeps the turn legible in history
        // exactly like hermes' `[image]` / `[file]` markers.
        _ => format!("[{kind}]"),
    };

    let mut session = SessionSource::new(line_platform(), chat_id.clone());
    session.chat_name = Some(chat_id);
    session.chat_type = chat_type.to_string();
    session.user_id = user_id.clone();
    // LINE has no display name on a message event, so the id doubles as the name (hermes
    // `build_source(..., user_name=user_id, chat_name=chat_id)`).
    session.user_name = user_id;
    session.message_id = message_id.clone();

    let mut parsed = MessageEvent::new(text, session);
    parsed.message_type = message_type;
    parsed.user_id = parsed.source.user_id.clone();
    parsed.user_name = parsed.source.user_name.clone();
    parsed.message_id = message_id.clone();
    parsed.platform_update_id = event.get("timestamp").and_then(Value::as_i64);
    parsed.reply_to_message_id = text_field(message, "quotedMessageId");
    // Text turns carry no attachment; a media turn references its LINE content id, which is what a
    // host needs to call `api-data.line.me` for the bytes. A media payload without an id stays
    // attachment-free so `media_urls`/`media_types` remain parallel.
    let (media_urls, media_types) = match (message_type, message_id.as_ref()) {
        (MessageType::Text, _) | (_, None) => (Vec::new(), Vec::new()),
        (_, Some(media_id)) => (vec![media_id.clone()], vec![kind.to_string()]),
    };
    parsed.media_urls = media_urls;
    parsed.media_types = media_types;
    parsed.raw_message = Some(event.clone());
    Some(parsed)
}

// ----- webhook signature primitives -----

/// Base64 alphabet (RFC 4648 §4), standard and padded — what LINE puts in `X-Line-Signature`.
const BASE64_ALPHABET: &[u8; 64] =
    b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";

/// SHA-256 HMAC block size in bytes.
const HMAC_BLOCK_SIZE: usize = 64;

/// HMAC-SHA256 (RFC 2104) over `message` keyed by `key`.
///
/// The SHA-256 compression function itself lives in [`crate::pairing::sha256`]; this is only the
/// HMAC construction around it.
fn hmac_sha256(key: &[u8], message: &[u8]) -> [u8; 32] {
    let mut key_block = [0u8; HMAC_BLOCK_SIZE];
    if key.len() > HMAC_BLOCK_SIZE {
        // A long key is hashed down first (RFC 2104 §2).
        key_block[..32].copy_from_slice(&crate::pairing::sha256::digest(key));
    } else {
        key_block[..key.len()].copy_from_slice(key);
    }
    let mut inner = Vec::with_capacity(HMAC_BLOCK_SIZE + message.len());
    inner.extend(key_block.iter().map(|byte| byte ^ 0x36));
    inner.extend_from_slice(message);
    let inner_hash = crate::pairing::sha256::digest(&inner);
    let mut outer = Vec::with_capacity(HMAC_BLOCK_SIZE + inner_hash.len());
    outer.extend(key_block.iter().map(|byte| byte ^ 0x5c));
    outer.extend_from_slice(&inner_hash);
    crate::pairing::sha256::digest(&outer)
}

/// Standard padded base64 (RFC 4648 §4), implemented here because the crate adds no dependencies.
fn base64_encode(data: &[u8]) -> String {
    let mut out = String::with_capacity(data.len().div_ceil(3) * 4);
    for chunk in data.chunks(3) {
        let b0 = u32::from(chunk[0]);
        let b1 = u32::from(*chunk.get(1).unwrap_or(&0));
        let b2 = u32::from(*chunk.get(2).unwrap_or(&0));
        let triple = (b0 << 16) | (b1 << 8) | b2;
        out.push(char::from(BASE64_ALPHABET[(triple >> 18) as usize & 0x3F]));
        out.push(char::from(BASE64_ALPHABET[(triple >> 12) as usize & 0x3F]));
        if chunk.len() > 1 {
            out.push(char::from(BASE64_ALPHABET[(triple >> 6) as usize & 0x3F]));
        } else {
            out.push('=');
        }
        if chunk.len() > 2 {
            out.push(char::from(BASE64_ALPHABET[triple as usize & 0x3F]));
        } else {
            out.push('=');
        }
    }
    out
}

/// Compare two byte strings without an early exit, so a mismatch position is not observable
/// through timing. Length is compared first (public information for a fixed-size digest).
fn constant_time_eq(left: &[u8], right: &[u8]) -> bool {
    if left.len() != right.len() {
        return false;
    }
    let mut diff = 0u8;
    for (a, b) in left.iter().zip(right.iter()) {
        diff |= a ^ b;
    }
    diff == 0
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::transport::{HttpMethod, HttpResponse, Transport, TransportError};
    use std::collections::VecDeque;
    use std::sync::{Arc, Mutex, MutexGuard};

    const TOKEN: &str = "TEST-CHANNEL-ACCESS-TOKEN";
    const SECRET: &str = "TEST-CHANNEL-SECRET";

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

    fn request_body(request: &HttpRequest) -> Value {
        let bytes = request.body.clone().unwrap_or_default();
        serde_json::from_slice(&bytes).unwrap_or(Value::Null)
    }

    fn has_bearer(request: &HttpRequest, token: &str) -> bool {
        request
            .headers
            .iter()
            .any(|(name, value)| name == "Authorization" && value == &format!("Bearer {token}"))
    }

    fn response(status: u16, body: Value) -> Result<HttpResponse, TransportError> {
        Ok(HttpResponse {
            status,
            body: body.to_string(),
            retry_after: None,
        })
    }

    fn raw_response(status: u16, body: &str) -> Result<HttpResponse, TransportError> {
        Ok(HttpResponse {
            status,
            body: body.to_string(),
            retry_after: None,
        })
    }

    fn api_error(status: u16, message: &str) -> Result<HttpResponse, TransportError> {
        response(status, json!({ "message": message }))
    }

    fn sent_messages(ids: &[&str]) -> Result<HttpResponse, TransportError> {
        let entries: Vec<Value> = ids.iter().map(|id| json!({ "id": id })).collect();
        response(200, json!({ "sentMessages": entries }))
    }

    fn adapter(mock: &Arc<MockTransport>) -> LineAdapter {
        LineAdapter::new(mock.clone(), TOKEN, SECRET)
    }

    #[tokio::test]
    async fn connect_is_true_only_for_bot_info_with_a_user_id() {
        let mock = MockTransport::new(vec![response(
            200,
            json!({ "userId": "U4af4980629", "basicId": "@vikram", "displayName": "Vikram" }),
        )]);
        let mut adapter = adapter(&mock);
        assert!(adapter.connect(false).await);

        let requests = mock.requests();
        assert_eq!(requests.len(), 1);
        assert_eq!(requests[0].method, HttpMethod::Get);
        assert_eq!(requests[0].url, "https://api.line.me/v2/bot/info");
        assert!(has_bearer(&requests[0], TOKEN));
        assert!(requests[0].body.is_none());
    }

    #[tokio::test]
    async fn connect_rejects_every_failure_shape() {
        // LINE's own 401 for a bad or revoked channel access token.
        let unauthorized = MockTransport::new(vec![api_error(401, "Authentication failed")]);
        assert!(!adapter(&unauthorized).connect(false).await);

        // 2xx but no userId: the token works for nothing we can address.
        let anonymous = MockTransport::new(vec![response(200, json!({ "basicId": "@bot" }))]);
        assert!(!adapter(&anonymous).connect(false).await);

        // A blank userId is not an id either.
        let blank = MockTransport::new(vec![response(200, json!({ "userId": "  " }))]);
        assert!(!adapter(&blank).connect(true).await);

        // Transport failure, and a body that is not JSON at all.
        let down = MockTransport::new(vec![Err(TransportError::Request("dns".to_string()))]);
        assert!(!adapter(&down).connect(false).await);
        let garbage = MockTransport::new(vec![raw_response(200, "<html>")]);
        assert!(!adapter(&garbage).connect(false).await);
    }

    #[tokio::test]
    async fn push_carries_the_documented_body_auth_and_path() {
        let mock = MockTransport::new(vec![sent_messages(&["11882510965520"])]);
        let result = adapter(&mock)
            // `reply_to` is accepted but cannot be expressed on a push; it must not appear.
            .send("U4af4980629", "hello", Some("quoted-id"), None)
            .await;

        assert!(result.success, "{result:?}");
        assert_eq!(result.message_id.as_deref(), Some("11882510965520"));
        assert_eq!(result.continuation_message_ids, vec!["11882510965520"]);
        assert!(result.error_kind.is_none());
        assert!(!result.retryable);

        let requests = mock.requests();
        assert_eq!(requests.len(), 1);
        assert_eq!(requests[0].method, HttpMethod::Post);
        assert_eq!(requests[0].url, "https://api.line.me/v2/bot/message/push");
        assert!(has_bearer(&requests[0], TOKEN));
        assert!(
            requests[0]
                .headers
                .iter()
                .any(|(name, value)| name == "Content-Type" && value == "application/json")
        );
        let body = request_body(&requests[0]);
        assert_eq!(
            body,
            json!({
                "to": "U4af4980629",
                "messages": [{ "type": "text", "text": "hello" }],
            })
        );
        assert!(body.get("replyToken").is_none());
    }

    #[tokio::test]
    async fn with_api_base_replaces_the_origin() {
        let mock = MockTransport::new(vec![sent_messages(&["1"])]);
        let adapter = adapter(&mock).with_api_base("https://api.line.test/");
        assert!(adapter.send("U1", "hi", None, None).await.success);
        assert_eq!(
            mock.requests()[0].url,
            "https://api.line.test/v2/bot/message/push"
        );
    }

    #[tokio::test]
    async fn push_with_an_empty_response_body_still_reports_success() {
        // LINE answers a successful push with 200 and no body.
        let mock = MockTransport::new(vec![raw_response(200, "")]);
        let result = adapter(&mock).send("U1", "hello", None, None).await;

        assert!(result.success, "{result:?}");
        assert_eq!(result.message_id.as_deref(), Some(""));
        assert_eq!(result.continuation_message_ids, vec![String::new()]);
    }

    #[tokio::test]
    async fn long_content_is_batched_at_five_messages_per_request() {
        // 26 000 characters split at 5 000 → six chunks → two requests (5 + 1).
        let content = "a".repeat(26_000);
        let mock = MockTransport::new(vec![
            sent_messages(&["m1", "m2", "m3", "m4", "m5"]),
            sent_messages(&["m6"]),
        ]);
        let result = adapter(&mock).send("C123", &content, None, None).await;

        assert!(result.success, "{result:?}");
        let requests = mock.requests();
        assert_eq!(requests.len(), 2, "six chunks fit in two requests");
        let first = request_body(&requests[0]);
        let second = request_body(&requests[1]);
        assert_eq!(first["messages"].as_array().map(Vec::len), Some(5));
        assert_eq!(second["messages"].as_array().map(Vec::len), Some(1));
        assert_eq!(first["to"], "C123");
        assert_eq!(second["to"], "C123");

        let mut total = 0usize;
        for request in &requests {
            for message in request_body(request)["messages"]
                .as_array()
                .cloned()
                .unwrap_or_default()
            {
                assert_eq!(message["type"], "text");
                let text = message["text"].as_str().unwrap_or_default();
                assert!(
                    text.chars().count() <= MAX_MESSAGE_LENGTH,
                    "chunk of {} characters exceeds the cap",
                    text.chars().count()
                );
                total += text.chars().count();
            }
        }
        assert_eq!(total, 26_000, "batching must not lose content");
        assert_eq!(result.message_id.as_deref(), Some("m6"));
        assert_eq!(
            result.continuation_message_ids,
            vec!["m1", "m2", "m3", "m4", "m5", "m6"]
        );
    }

    #[tokio::test]
    async fn mid_way_failure_reports_the_ids_already_delivered() {
        let content = "a".repeat(26_000);
        let mock = MockTransport::new(vec![
            sent_messages(&["m1", "m2", "m3", "m4", "m5"]),
            api_error(403, "Access denied"),
        ]);
        let result = adapter(&mock).send("C123", &content, None, None).await;

        assert!(!result.success);
        assert_eq!(result.error_kind.as_deref(), Some("forbidden"));
        assert_eq!(
            result.continuation_message_ids,
            vec!["m1", "m2", "m3", "m4", "m5"]
        );
        match result.partial_overflow() {
            Some((delivered, total)) => {
                assert_eq!(delivered, 5);
                assert_eq!(total, 6);
            }
            None => panic!("a failure after a delivered head must carry the partial marker"),
        }
    }

    #[tokio::test]
    async fn rate_limit_is_retryable_with_the_retry_after_header() {
        let mock = MockTransport::new(vec![Ok(HttpResponse {
            status: 429,
            body: json!({ "message": "Rate limit exceeded" }).to_string(),
            retry_after: Some(7.0),
        })]);
        let result = adapter(&mock).send("U1", "hello", None, None).await;

        assert!(!result.success);
        assert_eq!(result.error_kind.as_deref(), Some("rate_limited"));
        assert!(result.retryable);
        assert_eq!(result.retry_after, Some(7.0));
        assert!(
            result
                .error
                .as_deref()
                .unwrap_or_default()
                .contains("Rate limit exceeded"),
            "{result:?}"
        );
    }

    #[tokio::test]
    async fn unauthorized_and_forbidden_are_not_retryable() {
        for status in [401, 403] {
            let mock = MockTransport::new(vec![api_error(status, "Authentication failed")]);
            let result = adapter(&mock).send("U1", "hello", None, None).await;
            assert!(!result.success, "{status}");
            assert_eq!(result.error_kind.as_deref(), Some("forbidden"), "{status}");
            assert!(!result.retryable, "{status}");
            assert!(result.message_id.is_none(), "{status}");
        }
    }

    #[tokio::test]
    async fn not_found_bad_request_and_server_errors_map_to_the_taxonomy() {
        let missing = MockTransport::new(vec![api_error(404, "Not found")]);
        let result = adapter(&missing).send("C404", "hello", None, None).await;
        assert_eq!(result.error_kind.as_deref(), Some("not_found"));
        assert!(!result.retryable);

        // LINE reports a body validation problem as 400 with a `details` list.
        let invalid = MockTransport::new(vec![response(
            400,
            json!({
                "message": "The request body has 1 error(s)",
                "details": [{ "message": "may not be empty", "property": "messages[0].text" }],
            }),
        )]);
        let result = adapter(&invalid).send("U1", "hello", None, None).await;
        assert_eq!(result.error_kind.as_deref(), Some("bad_format"));
        assert!(!result.retryable);
        let error = result.error.as_deref().unwrap_or_default();
        assert!(error.contains("The request body has 1 error(s)"), "{error}");
        assert!(error.contains("may not be empty"), "{error}");

        let server = MockTransport::new(vec![api_error(503, "Service Unavailable")]);
        let result = adapter(&server).send("U1", "hello", None, None).await;
        assert_eq!(result.error_kind.as_deref(), Some("transient"));
        assert!(result.retryable);

        let down = MockTransport::new(vec![Err(TransportError::Request(
            "connection reset".to_string(),
        ))]);
        let result = adapter(&down).send("U1", "hello", None, None).await;
        assert_eq!(result.error_kind.as_deref(), Some("transient"));
        assert!(result.retryable);
        assert!(
            result
                .error
                .as_deref()
                .unwrap_or_default()
                .contains("line transport error"),
            "{result:?}"
        );
    }

    #[tokio::test]
    async fn blank_content_is_refused_without_a_request() {
        for content in ["", "   ", "\n\t \r\n"] {
            let mock = MockTransport::new(Vec::new());
            let result = adapter(&mock).send("U1", content, None, None).await;
            assert!(!result.success, "{content:?}");
            assert_eq!(result.error_kind.as_deref(), Some("bad_format"));
            assert!(!result.retryable);
            assert!(
                mock.requests().is_empty(),
                "blank content must not hit the wire: {content:?}"
            );
        }
    }

    #[tokio::test]
    async fn reply_posts_the_reply_endpoint_once() {
        let mock = MockTransport::new(vec![raw_response(200, "")]);
        let result = adapter(&mock).reply("reply-token-1", "hello there").await;

        assert!(result.success, "{result:?}");
        // LINE's reply endpoint reports no message id.
        assert!(result.message_id.is_none());
        assert!(result.error_kind.is_none());

        let requests = mock.requests();
        assert_eq!(requests.len(), 1);
        assert_eq!(requests[0].method, HttpMethod::Post);
        assert_eq!(requests[0].url, "https://api.line.me/v2/bot/message/reply");
        assert!(has_bearer(&requests[0], TOKEN));
        assert_eq!(
            request_body(&requests[0]),
            json!({
                "replyToken": "reply-token-1",
                "messages": [{ "type": "text", "text": "hello there" }],
            })
        );
    }

    #[tokio::test]
    async fn reply_refuses_overlong_and_blank_content_without_a_request() {
        let mock = MockTransport::new(Vec::new());
        let line = adapter(&mock);

        let overlong = "a".repeat(MAX_MESSAGE_LENGTH + 1);
        let result = line.reply("reply-token-1", &overlong).await;
        assert!(!result.success);
        assert_eq!(result.error_kind.as_deref(), Some("bad_format"));
        assert!(
            result
                .error
                .as_deref()
                .unwrap_or_default()
                .contains("single-use reply limit"),
            "{result:?}"
        );

        let blank = line.reply("reply-token-1", "   ").await;
        assert!(!blank.success);
        assert_eq!(blank.error_kind.as_deref(), Some("bad_format"));

        let tokenless = line.reply("  ", "hello").await;
        assert!(!tokenless.success);
        assert_eq!(tokenless.error_kind.as_deref(), Some("bad_format"));

        assert!(
            mock.requests().is_empty(),
            "refused replies must not spend the token"
        );

        // Exactly at the limit is allowed.
        let at_limit = "a".repeat(MAX_MESSAGE_LENGTH);
        let allowed = MockTransport::new(vec![raw_response(200, "")]);
        assert!(adapter(&allowed).reply("t", &at_limit).await.success);
        assert_eq!(allowed.requests().len(), 1);
    }

    #[tokio::test]
    async fn reply_failures_are_classified() {
        let mock = MockTransport::new(vec![api_error(400, "Invalid reply token")]);
        let result = adapter(&mock).reply("spent-token", "hello").await;
        assert!(!result.success);
        assert_eq!(result.error_kind.as_deref(), Some("bad_format"));
        assert!(
            result
                .error
                .as_deref()
                .unwrap_or_default()
                .contains("Invalid reply token"),
            "{result:?}"
        );
    }

    #[tokio::test]
    async fn signature_accepts_the_line_digest_and_rejects_tampering() {
        let adapter = LineAdapter::new(MockTransport::new(Vec::new()), TOKEN, "my-channel-secret");
        // Vector computed independently with `openssl dgst -sha256 -hmac my-channel-secret
        // -binary | base64` over the exact body bytes below.
        let body = br#"{"destination":"U123","events":[]}"#;
        let signature = "lGBbWY5ZCnCTv98GVmVgxGvm9egVhf9y2sWXzaBJbiY=";
        assert!(adapter.verify_webhook(body, signature));

        // Any change to the body invalidates the digest.
        let tampered = br#"{"destination":"U123","events":[{"type":"message"}]}"#;
        assert!(!adapter.verify_webhook(tampered, signature));
        // So does any change to the signature, at either end.
        assert!(!adapter.verify_webhook(body, "lGBbWY5ZCnCTv98GVmVgxGvm9egVhf9y2sWXzaBJbiZ="));
        assert!(!adapter.verify_webhook(body, "XGBbWY5ZCnCTv98GVmVgxGvm9egVhf9y2sWXzaBJbiY="));
        assert!(!adapter.verify_webhook(body, ""));
        assert!(!adapter.verify_webhook(body, "not-a-signature"));

        // A different channel secret produces a different digest.
        let other = LineAdapter::new(MockTransport::new(Vec::new()), TOKEN, "other-secret");
        assert!(!other.verify_webhook(body, signature));

        // Fail closed without a secret.
        let secretless = LineAdapter::new(MockTransport::new(Vec::new()), TOKEN, "");
        assert!(!secretless.verify_webhook(body, signature));
    }

    #[test]
    fn hmac_sha256_matches_the_rfc4231_vector() {
        // RFC 4231 §4.2 (test case 1): 20 bytes of 0x0b, data "Hi There".
        let key = [0x0b_u8; 20];
        let digest = hmac_sha256(&key, b"Hi There");
        let hex: String = digest.iter().map(|byte| format!("{byte:02x}")).collect();
        assert_eq!(
            hex,
            "b0344c61d8db38535ca8afceaf0bf12b881dc200c9833da726e9376c2e32cff7"
        );
        // A key longer than the 64-byte block is hashed down first (RFC 4231 test case 6).
        let long_key = [0xaa_u8; 131];
        let digest = hmac_sha256(
            &long_key,
            b"Test Using Larger Than Block-Size Key - Hash Key First",
        );
        let hex: String = digest.iter().map(|byte| format!("{byte:02x}")).collect();
        assert_eq!(
            hex,
            "60e431591ee0b67f0d8a26aacbf5b77f8e0bc6213728c5140546040f0ee37f54"
        );
    }

    #[test]
    fn base64_matches_the_rfc4648_vectors() {
        for (input, expected) in [
            ("", ""),
            ("f", "Zg=="),
            ("fo", "Zm8="),
            ("foo", "Zm9v"),
            ("foob", "Zm9vYg=="),
            ("fooba", "Zm9vYmE="),
            ("foobar", "Zm9vYmFy"),
        ] {
            assert_eq!(base64_encode(input.as_bytes()), expected, "{input:?}");
        }
        // Every byte value survives the round trip through the alphabet.
        let all: Vec<u8> = (0..=255u8).collect();
        assert_eq!(base64_encode(&all).len(), 344);
    }

    #[tokio::test]
    async fn chat_info_reads_profile_group_and_room_summaries() {
        let cases = [
            (
                "U4af4980629",
                "https://api.line.me/v2/bot/profile/U4af4980629",
                json!({ "userId": "U4af4980629", "displayName": "Ann Lee" }),
                "Ann Lee",
                "dm",
            ),
            (
                "C56b4d1e2",
                "https://api.line.me/v2/bot/group/C56b4d1e2/summary",
                json!({ "groupId": "C56b4d1e2", "groupName": "Team", "pictureUrl": "" }),
                "Team",
                "group",
            ),
            (
                "R1a2b3c4d",
                "https://api.line.me/v2/bot/room/R1a2b3c4d/summary",
                json!({ "roomId": "R1a2b3c4d", "roomName": "War room" }),
                "War room",
                "group",
            ),
        ];
        for (chat_id, url, body, expected_name, expected_type) in cases {
            let mock = MockTransport::new(vec![response(200, body)]);
            let info = adapter(&mock).get_chat_info(chat_id).await;
            assert_eq!(info.name, expected_name, "{chat_id}");
            assert_eq!(info.chat_type, expected_type, "{chat_id}");
            let requests = mock.requests();
            assert_eq!(requests.len(), 1, "{chat_id}");
            assert_eq!(requests[0].method, HttpMethod::Get, "{chat_id}");
            assert_eq!(requests[0].url, url, "{chat_id}");
            assert!(has_bearer(&requests[0], TOKEN), "{chat_id}");
        }
    }

    #[tokio::test]
    async fn chat_info_falls_back_to_the_chat_id() {
        // LINE refuses the lookup.
        let refused = MockTransport::new(vec![api_error(404, "Not found")]);
        let info = adapter(&refused).get_chat_info("C000").await;
        assert_eq!(info.name, "C000");
        assert_eq!(info.chat_type, "dm");

        // A summary body without a usable name keeps the id.
        let nameless = MockTransport::new(vec![response(200, json!({ "groupId": "C001" }))]);
        let info = adapter(&nameless).get_chat_info("C001").await;
        assert_eq!(info.name, "C001");
        assert_eq!(info.chat_type, "group");

        // An unknown id prefix is not even asked about.
        let unknown = MockTransport::new(Vec::new());
        let info = adapter(&unknown).get_chat_info("weird-id").await;
        assert_eq!(info.name, "weird-id");
        assert_eq!(info.chat_type, "dm");
        assert!(unknown.requests().is_empty());
    }

    #[tokio::test]
    async fn typing_posts_the_loading_indicator_and_swallows_failures() {
        let mock = MockTransport::new(vec![response(200, json!([])), api_error(500, "boom")]);
        let adapter = adapter(&mock);

        adapter.send_typing("U4af4980629", None).await;
        // A 1:1-only endpoint rejects group ids; the hint is dropped, never surfaced.
        adapter.send_typing("C56b4d1e2", None).await;
        // A blank chat id is not worth a request.
        adapter.send_typing("   ", None).await;

        let requests = mock.requests();
        assert_eq!(requests.len(), 2);
        assert_eq!(
            requests[0].url,
            "https://api.line.me/v2/bot/chat/loading/start"
        );
        assert_eq!(requests[0].method, HttpMethod::Post);
        assert!(has_bearer(&requests[0], TOKEN));
        assert_eq!(
            request_body(&requests[0]),
            json!({ "chatId": "U4af4980629" })
        );
    }

    #[tokio::test]
    async fn typing_swallows_a_transport_failure() {
        let mock = MockTransport::new(vec![Err(TransportError::Request("down".to_string()))]);
        adapter(&mock).send_typing("U1", None).await;
        assert_eq!(mock.requests().len(), 1);
    }

    #[test]
    fn webhook_parses_text_media_group_and_room_events() {
        let adapter = LineAdapter::new(MockTransport::new(Vec::new()), TOKEN, SECRET);
        let body = json!({
            "destination": "U123",
            "events": [
                {
                    "type": "message",
                    "mode": "active",
                    "timestamp": 1_700_000_000_123_i64,
                    "webhookEventId": "01H",
                    "source": { "type": "user", "userId": "U4af4980629" },
                    "message": {
                        "type": "text",
                        "id": "325708",
                        "text": "hello bot",
                        "quotedMessageId": "325700",
                    },
                },
                {
                    "type": "message",
                    "timestamp": 1_700_000_001_000_i64,
                    "source": { "type": "group", "groupId": "C56b4d1e2", "userId": "U4af4980629" },
                    "message": { "type": "text", "id": "325709", "text": "group hi" },
                },
                {
                    "type": "message",
                    "timestamp": 1_700_000_002_000_i64,
                    "source": { "type": "room", "roomId": "R1a2b3c4d", "userId": "U4af4980629" },
                    "message": { "type": "image", "id": "325710", "contentProvider": { "type": "line" } },
                },
                // Not a message event: no turn.
                { "type": "follow", "timestamp": 1_700_000_003_000_i64, "source": { "type": "user", "userId": "U9" } },
                { "type": "postback", "timestamp": 1_700_000_004_000_i64, "source": { "type": "user", "userId": "U9" } },
                // A message kind this port does not model is skipped rather than invented.
                {
                    "type": "message",
                    "timestamp": 1_700_000_005_000_i64,
                    "source": { "type": "user", "userId": "U9" },
                    "message": { "type": "sticker", "id": "325711" },
                },
                // No resolvable chat id.
                { "type": "message", "timestamp": 1_700_000_006_000_i64, "message": { "type": "text", "id": "1", "text": "x" } },
            ],
        })
        .to_string();

        let events = adapter.parse_webhook(&body).expect("valid webhook");
        assert_eq!(events.len(), 3, "{events:#?}");

        let dm = &events[0];
        assert_eq!(dm.text, "hello bot");
        assert_eq!(dm.message_type, MessageType::Text);
        assert_eq!(dm.source.platform.as_str(), "line");
        assert_eq!(dm.source.chat_id, "U4af4980629");
        assert_eq!(dm.source.chat_type, "dm");
        assert_eq!(dm.source.chat_name.as_deref(), Some("U4af4980629"));
        assert_eq!(dm.user_id.as_deref(), Some("U4af4980629"));
        assert_eq!(dm.user_name.as_deref(), Some("U4af4980629"));
        assert_eq!(dm.message_id.as_deref(), Some("325708"));
        assert_eq!(dm.source.message_id.as_deref(), Some("325708"));
        assert_eq!(dm.platform_update_id, Some(1_700_000_000_123));
        assert_eq!(dm.reply_to_message_id.as_deref(), Some("325700"));
        assert!(dm.media_urls.is_empty());
        assert!(dm.raw_message.is_some());

        let group = &events[1];
        assert_eq!(group.source.chat_id, "C56b4d1e2");
        assert_eq!(group.source.chat_type, "group");
        assert_eq!(group.user_id.as_deref(), Some("U4af4980629"));
        assert_eq!(group.text, "group hi");

        let room = &events[2];
        assert_eq!(room.source.chat_id, "R1a2b3c4d");
        assert_eq!(room.source.chat_type, "group");
        assert_eq!(room.message_type, MessageType::Photo);
        assert_eq!(room.text, "[image]");
        assert_eq!(room.media_urls, vec!["325710"]);
        assert_eq!(room.media_types, vec!["image"]);
        assert_eq!(room.reply_to_message_id, None);
    }

    #[test]
    fn webhook_maps_every_media_kind() {
        let adapter = LineAdapter::new(MockTransport::new(Vec::new()), TOKEN, SECRET);
        let body = json!({
            "events": [
                { "type": "message", "timestamp": 1, "source": { "type": "user", "userId": "U1" },
                  "message": { "type": "video", "id": "v1" } },
                { "type": "message", "timestamp": 2, "source": { "type": "user", "userId": "U1" },
                  "message": { "type": "audio", "id": "a1" } },
                { "type": "message", "timestamp": 3, "source": { "type": "user", "userId": "U1" },
                  "message": { "type": "file", "id": "f1", "fileName": "notes.pdf" } },
            ],
        })
        .to_string();

        let events = adapter.parse_webhook(&body).expect("valid webhook");
        assert_eq!(events.len(), 3);
        assert_eq!(events[0].message_type, MessageType::Video);
        assert_eq!(events[0].media_urls, vec!["v1"]);
        assert_eq!(events[0].media_types, vec!["video"]);
        assert_eq!(events[0].text, "[video]");
        // LINE audio is a recorded voice clip, so it takes the transcription path.
        assert_eq!(events[1].message_type, MessageType::Voice);
        assert_eq!(events[1].media_types, vec!["audio"]);
        assert_eq!(events[2].message_type, MessageType::Document);
        assert_eq!(events[2].media_urls, vec!["f1"]);
        assert_eq!(events[2].media_types, vec!["file"]);
    }

    #[test]
    fn webhook_handles_empty_and_unparseable_bodies() {
        let adapter = LineAdapter::new(MockTransport::new(Vec::new()), TOKEN, SECRET);
        assert!(
            adapter
                .parse_webhook(r#"{"destination":"U1"}"#)
                .expect("no events key is an empty batch")
                .is_empty()
        );
        assert!(
            adapter
                .parse_webhook(r#"{"events":[]}"#)
                .expect("empty array")
                .is_empty()
        );

        let error = adapter
            .parse_webhook("not json")
            .expect_err("invalid json must fail");
        assert!(error.contains("not json"), "{error}");
    }

    #[test]
    fn platform_handle_and_path_encoding() {
        assert_eq!(line_platform().as_str(), "line");
        assert_eq!(encode_path_segment("U4af4980629"), "U4af4980629");
        // A hostile id cannot rewrite the URL.
        assert_eq!(encode_path_segment("../a b"), "..%2Fa%20b");
    }

    #[test]
    fn taxonomy_covers_the_documented_statuses() {
        assert_eq!(
            failure_from(429, &Value::Null, Some(3.0))
                .into_send_result()
                .error_kind
                .as_deref(),
            Some("rate_limited")
        );
        assert_eq!(
            failure_from(403, &Value::Null, None)
                .into_send_result()
                .error_kind
                .as_deref(),
            Some("forbidden")
        );
        assert_eq!(
            failure_from(404, &Value::Null, None)
                .into_send_result()
                .error_kind
                .as_deref(),
            Some("not_found")
        );
        assert_eq!(
            failure_from(400, &Value::Null, None)
                .into_send_result()
                .error_kind
                .as_deref(),
            Some("bad_format")
        );
        let transient = failure_from(500, &Value::Null, None).into_send_result();
        assert_eq!(transient.error_kind.as_deref(), Some("transient"));
        assert!(transient.retryable);
        // A status LINE does not document still gets a definite answer.
        assert_eq!(
            failure_from(418, &Value::Null, None)
                .into_send_result()
                .error_kind
                .as_deref(),
            Some("bad_format")
        );
        // No message at all falls back to the status.
        assert!(
            error_message(&Value::Null, 502).contains("502"),
            "{:?}",
            error_message(&Value::Null, 502)
        );
    }

    // ----- outbound media (`supports_media` / `send_media`) -----

    const IMAGE_URL: &str = "https://cdn.example.test/cat.png";

    fn media(kind: MediaKind, url: &str) -> OutboundMedia {
        OutboundMedia::new(kind, MediaSource::Url(url.to_string()))
    }

    #[tokio::test]
    async fn supports_media_is_true() {
        assert!(adapter(&MockTransport::new(Vec::new())).supports_media());
    }

    #[tokio::test]
    async fn send_media_pushes_the_image_object_with_the_url_as_preview() {
        let mock = MockTransport::new(vec![sent_messages(&["11882510965520"])]);
        let result = adapter(&mock)
            .send_media("U4af4980629", &media(MediaKind::Image, IMAGE_URL), None)
            .await;

        assert!(result.success, "{result:?}");
        assert_eq!(result.message_id.as_deref(), Some("11882510965520"));
        assert_eq!(result.continuation_message_ids, vec!["11882510965520"]);
        assert!(result.error_kind.is_none());
        assert!(!result.retryable);

        let requests = mock.requests();
        assert_eq!(requests.len(), 1);
        assert_eq!(requests[0].method, HttpMethod::Post);
        assert_eq!(requests[0].url, "https://api.line.me/v2/bot/message/push");
        assert!(has_bearer(&requests[0], TOKEN));
        assert!(
            requests[0]
                .headers
                .iter()
                .any(|(name, value)| name == "Content-Type" && value == "application/json")
        );
        assert_eq!(
            request_body(&requests[0]),
            json!({
                "to": "U4af4980629",
                "messages": [{
                    "type": "image",
                    "originalContentUrl": IMAGE_URL,
                    // `send_image_file` (adapter.py:827) reuses the image URL as the preview.
                    "previewImageUrl": IMAGE_URL,
                }],
            })
        );
    }

    #[tokio::test]
    async fn send_media_pushes_the_video_object_with_a_preview_url() {
        let url = "https://cdn.example.test/clip.mp4";
        let mock = MockTransport::new(vec![sent_messages(&["m-video"])]);
        let result = adapter(&mock)
            .send_media("C56b4d1e2", &media(MediaKind::Video, url), None)
            .await;

        assert!(result.success, "{result:?}");
        assert_eq!(result.message_id.as_deref(), Some("m-video"));
        let body = request_body(&mock.requests()[0]);
        assert_eq!(body["to"], "C56b4d1e2");
        assert_eq!(
            body["messages"],
            json!([{
                "type": "video",
                "originalContentUrl": url,
                // `send_video` requires `previewImageUrl` (adapter.py:860). The reference serves a
                // supplied preview file or a 1x1 PNG from its own host; with no file server here the
                // media URL doubles as the preview and LINE validates it.
                "previewImageUrl": url,
            }])
        );
    }

    #[tokio::test]
    async fn send_media_pushes_the_audio_object_with_the_reference_duration() {
        let url = "https://cdn.example.test/clip.m4a";
        let mock = MockTransport::new(vec![sent_messages(&["m-audio"])]);
        let result = adapter(&mock)
            .send_media("U1", &media(MediaKind::Audio, url), None)
            .await;

        assert!(result.success, "{result:?}");
        assert_eq!(
            request_body(&mock.requests()[0])["messages"],
            json!([{
                "type": "audio",
                "originalContentUrl": url,
                // `send_voice(..., duration_ms: int = 1000)` always writes `duration`
                // (adapter.py:831/837).
                "duration": DEFAULT_AUDIO_DURATION_MS,
            }])
        );
    }

    #[tokio::test]
    async fn send_media_document_is_refused_without_a_request() {
        let mock = MockTransport::new(Vec::new());
        let document = media(MediaKind::Document, "https://cdn.example.test/report.pdf")
            .with_filename("report.pdf")
            .with_mime("application/pdf")
            .with_caption("the report");
        let result = adapter(&mock).send_media("U1", &document, None).await;

        assert!(!result.success);
        assert_eq!(result.error_kind.as_deref(), Some("bad_format"));
        assert!(result.message_id.is_none());
        assert!(result.continuation_message_ids.is_empty());
        let error = result.error.as_deref().unwrap_or_default();
        assert!(error.contains("send_document"), "{error}");
        assert!(error.contains("adapter.py:363-369"), "{error}");
        assert!(
            mock.requests().is_empty(),
            "a refused document must not hit the wire"
        );
    }

    #[tokio::test]
    async fn send_media_caption_rides_a_follow_up_text_message() {
        let mock = MockTransport::new(vec![sent_messages(&["m-media", "m-caption"])]);
        let image = media(MediaKind::Image, IMAGE_URL).with_caption("look at this");
        let result = adapter(&mock).send_media("U1", &image, None).await;

        assert!(result.success, "{result:?}");
        // Both objects ride one request, media first then the caption (`send_image_file`,
        // adapter.py:827-828); the newest id is the caption's.
        assert_eq!(result.message_id.as_deref(), Some("m-caption"));
        assert_eq!(
            result.continuation_message_ids,
            vec!["m-media", "m-caption"]
        );
        let requests = mock.requests();
        assert_eq!(requests.len(), 1);
        assert_eq!(
            request_body(&requests[0])["messages"],
            json!([
                { "type": "image", "originalContentUrl": IMAGE_URL, "previewImageUrl": IMAGE_URL },
                { "type": "text", "text": "look at this" },
            ])
        );

        // Video and audio carry no caption field either, so the follow-up mechanism is the same.
        let video =
            media(MediaKind::Video, "https://cdn.example.test/clip.mp4").with_caption("clip");
        let mock = MockTransport::new(vec![sent_messages(&["v", "c"])]);
        assert!(adapter(&mock).send_media("U1", &video, None).await.success);
        let messages = request_body(&mock.requests()[0])["messages"]
            .as_array()
            .cloned()
            .unwrap_or_default();
        assert_eq!(messages.len(), 2);
        assert_eq!(messages[1], json!({ "type": "text", "text": "clip" }));
    }

    #[tokio::test]
    async fn send_media_blank_caption_is_dropped_and_a_long_one_is_capped() {
        // A whitespace-only caption would be an empty text object, which LINE rejects.
        let blank = MockTransport::new(vec![sent_messages(&["m1"])]);
        let image = media(MediaKind::Image, IMAGE_URL).with_caption("   \n\t ");
        assert!(adapter(&blank).send_media("U1", &image, None).await.success);
        assert_eq!(
            request_body(&blank.requests()[0])["messages"]
                .as_array()
                .map(Vec::len),
            Some(1)
        );

        // Over the per-bubble cap: cut at 4999 characters plus an ellipsis, like `_text_message`
        // (adapter.py:283).
        let long = "z".repeat(MAX_MESSAGE_LENGTH + 500);
        let capped = MockTransport::new(vec![sent_messages(&["a", "b"])]);
        let image = media(MediaKind::Image, IMAGE_URL).with_caption(long);
        assert!(
            adapter(&capped)
                .send_media("U1", &image, None)
                .await
                .success
        );
        let messages = request_body(&capped.requests()[0])["messages"]
            .as_array()
            .cloned()
            .unwrap_or_default();
        assert_eq!(messages.len(), 2);
        let text = messages[1]["text"].as_str().unwrap_or_default();
        assert_eq!(text.chars().count(), MAX_MESSAGE_LENGTH);
        assert!(text.ends_with('…'), "{text:?}");
    }

    #[tokio::test]
    async fn send_media_reply_token_uses_the_reply_endpoint() {
        let mock = MockTransport::new(vec![raw_response(200, "")]);
        let image = media(MediaKind::Image, IMAGE_URL).with_caption("hi");
        let result = adapter(&mock)
            .send_media("U1", &image, Some("reply-token-1"))
            .await;

        assert!(result.success, "{result:?}");
        // The reply endpoint reports no message id and the token is single-use.
        assert!(result.message_id.is_none());
        assert!(result.error_kind.is_none());
        let requests = mock.requests();
        assert_eq!(requests.len(), 1);
        assert_eq!(requests[0].url, "https://api.line.me/v2/bot/message/reply");
        assert_eq!(requests[0].method, HttpMethod::Post);
        assert!(has_bearer(&requests[0], TOKEN));
        let body = request_body(&requests[0]);
        assert_eq!(body["replyToken"], "reply-token-1");
        assert!(body.get("to").is_none(), "a reply names no push target");
        assert_eq!(body["messages"].as_array().map(Vec::len), Some(2));

        // A blank anchor is the no-anchor case and pushes, the way `send` accepts a blank
        // `reply_to`.
        let pushed = MockTransport::new(vec![sent_messages(&["m1"])]);
        assert!(
            adapter(&pushed)
                .send_media("U1", &media(MediaKind::Image, IMAGE_URL), Some("   "))
                .await
                .success
        );
        assert_eq!(
            pushed.requests()[0].url,
            "https://api.line.me/v2/bot/message/push"
        );
        assert_eq!(request_body(&pushed.requests()[0])["to"], "U1");
    }

    #[tokio::test]
    async fn send_media_reply_token_failure_does_not_fall_back_to_push() {
        // The token is spent or expired: LINE answers 400 and nothing was delivered.
        let mock = MockTransport::new(vec![api_error(400, "Invalid reply token")]);
        let result = adapter(&mock)
            .send_media("U1", &media(MediaKind::Image, IMAGE_URL), Some("spent"))
            .await;

        assert!(!result.success);
        assert_eq!(result.error_kind.as_deref(), Some("bad_format"));
        assert!(
            result
                .error
                .as_deref()
                .unwrap_or_default()
                .contains("Invalid reply token"),
            "{result:?}"
        );
        // No push retry: a duplicate delivery is worse than a classified failure.
        assert_eq!(mock.requests().len(), 1);
        assert_eq!(
            mock.requests()[0].url,
            "https://api.line.me/v2/bot/message/reply"
        );
    }

    #[tokio::test]
    async fn send_media_rate_limit_is_retryable_with_the_retry_after_header() {
        let mock = MockTransport::new(vec![Ok(HttpResponse {
            status: 429,
            body: json!({ "message": "Rate limit exceeded" }).to_string(),
            retry_after: Some(4.0),
        })]);
        let result = adapter(&mock)
            .send_media("U1", &media(MediaKind::Image, IMAGE_URL), None)
            .await;

        assert!(!result.success);
        assert_eq!(result.error_kind.as_deref(), Some("rate_limited"));
        assert!(result.retryable);
        assert_eq!(result.retry_after, Some(4.0));
        assert!(
            result
                .error
                .as_deref()
                .unwrap_or_default()
                .contains("Rate limit exceeded"),
            "{result:?}"
        );
    }

    #[tokio::test]
    async fn send_media_failures_map_to_the_taxonomy() {
        for (status, kind, retryable) in [
            (400, "bad_format", false),
            (401, "forbidden", false),
            (403, "forbidden", false),
            (404, "not_found", false),
            (500, "transient", true),
            (503, "transient", true),
        ] {
            let mock = MockTransport::new(vec![api_error(status, "line says no")]);
            let result = adapter(&mock)
                .send_media("U1", &media(MediaKind::Image, IMAGE_URL), None)
                .await;
            assert!(!result.success, "{status}");
            assert_eq!(result.error_kind.as_deref(), Some(kind), "{status}");
            assert_eq!(result.retryable, retryable, "{status}");
            assert!(result.message_id.is_none(), "{status}");
            assert!(
                result
                    .error
                    .as_deref()
                    .unwrap_or_default()
                    .contains("line says no"),
                "{status}: {result:?}"
            );
        }

        let down = MockTransport::new(vec![Err(TransportError::Request(
            "connection reset".to_string(),
        ))]);
        let result = adapter(&down)
            .send_media("U1", &media(MediaKind::Image, IMAGE_URL), None)
            .await;
        assert_eq!(result.error_kind.as_deref(), Some("transient"));
        assert!(result.retryable);
        assert!(
            result
                .error
                .as_deref()
                .unwrap_or_default()
                .contains("line transport error"),
            "{result:?}"
        );
    }

    #[tokio::test]
    async fn send_media_bytes_are_refused_without_a_request() {
        let mock = MockTransport::new(Vec::new());
        let bytes = OutboundMedia::new(MediaKind::Image, MediaSource::Bytes(vec![1, 2, 3]));
        let result = adapter(&mock).send_media("U1", &bytes, None).await;

        assert!(!result.success);
        assert_eq!(result.error_kind.as_deref(), Some("bad_format"));
        assert!(result.message_id.is_none());
        let error = result.error.as_deref().unwrap_or_default();
        assert!(error.contains("MediaSource::Url"), "{error}");
        assert!(error.contains("3 bytes"), "{error}");
        assert!(
            error.contains("LINE_PUBLIC_URL"),
            "the refusal must say why bytes cannot be served: {error}"
        );
        assert!(
            mock.requests().is_empty(),
            "bytes cannot be served, so nothing is sent"
        );

        // Empty bytes are refused the same way, never reported as a delivery.
        let empty = OutboundMedia::new(MediaKind::Audio, MediaSource::Bytes(Vec::new()));
        let mock = MockTransport::new(Vec::new());
        assert!(!adapter(&mock).send_media("U1", &empty, None).await.success);
        assert!(mock.requests().is_empty());
    }

    #[tokio::test]
    async fn send_media_empty_and_non_https_urls_are_refused_without_a_request() {
        for url in [
            "",
            "   ",
            "http://cdn.example.test/a.png",
            "ftp://cdn.example.test/a.png",
            "cdn.example.test/a.png",
        ] {
            let mock = MockTransport::new(Vec::new());
            let result = adapter(&mock)
                .send_media("U1", &media(MediaKind::Image, url), None)
                .await;
            assert!(!result.success, "{url:?}");
            assert_eq!(result.error_kind.as_deref(), Some("bad_format"), "{url:?}");
            assert!(result.message_id.is_none(), "{url:?}");
            assert!(mock.requests().is_empty(), "{url:?} must not hit the wire");
        }

        let mock = MockTransport::new(Vec::new());
        let empty = adapter(&mock)
            .send_media("U1", &media(MediaKind::Image, "  "), None)
            .await;
        assert!(
            empty
                .error
                .as_deref()
                .unwrap_or_default()
                .contains("url is empty"),
            "{empty:?}"
        );

        let insecure = adapter(&mock)
            .send_media("U1", &media(MediaKind::Image, "http://x.test/a.png"), None)
            .await;
        let error = insecure.error.as_deref().unwrap_or_default().to_string();
        assert!(error.contains("adapter.py:825-826"), "{error}");

        // Surrounding whitespace is dropped, so a padded URL is still delivered.
        let padded = MockTransport::new(vec![sent_messages(&["m1"])]);
        let url = format!("  {IMAGE_URL}  ");
        assert!(
            adapter(&padded)
                .send_media("U1", &media(MediaKind::Image, &url), None)
                .await
                .success
        );
        assert_eq!(
            request_body(&padded.requests()[0])["messages"][0]["originalContentUrl"],
            IMAGE_URL
        );
    }

    #[tokio::test]
    async fn send_media_url_cap_is_the_line_documented_length() {
        let prefix = "https://cdn.example.test/";
        let at_cap = format!("{prefix}{}", "a".repeat(MAX_MEDIA_URL_CHARS - prefix.len()));
        assert_eq!(at_cap.chars().count(), MAX_MEDIA_URL_CHARS);

        let allowed = MockTransport::new(vec![sent_messages(&["m1"])]);
        assert!(
            adapter(&allowed)
                .send_media("U1", &media(MediaKind::Image, &at_cap), None)
                .await
                .success
        );
        assert_eq!(
            request_body(&allowed.requests()[0])["messages"][0]["originalContentUrl"],
            at_cap.as_str()
        );

        let over = format!("{at_cap}a");
        let refused = MockTransport::new(Vec::new());
        let result = adapter(&refused)
            .send_media("U1", &media(MediaKind::Image, &over), None)
            .await;
        assert!(!result.success);
        assert_eq!(result.error_kind.as_deref(), Some("bad_format"));
        assert!(
            result
                .error
                .as_deref()
                .unwrap_or_default()
                .contains("2001 characters"),
            "{result:?}"
        );
        assert!(refused.requests().is_empty());
    }
}
