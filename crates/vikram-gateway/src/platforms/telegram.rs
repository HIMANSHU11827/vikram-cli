//! Telegram adapter — Bot API HTTP calls and long polling.
//!
//! Rust port of hermes-agent `plugins/platforms/telegram/adapter.py` (the wire half: `send`,
//! `edit_message`, `delete_message`, `send_typing`, `get_chat_info`, message/update parsing and the
//! MarkdownV2 renderer). Everything the hermes file builds on python-telegram-bot is re-implemented
//! here directly against the Bot API over [`crate::transport::Transport`], so the adapter stays
//! testable with no network, no token and no SDK.
//!
//! Ported behaviour, with the hermes origin:
//! - MarkdownV2 rendering: `format_message` (protect code, convert constructs, escape the rest),
//!   built on `_escape_mdv2`'s special-character set.
//! - 4096-UTF-16 chunking through [`truncate_message`] with [`utf16_len`] (`MAX_MESSAGE_LENGTH`,
//!   `message_len_fn`), chunks delivered sequentially in order.
//! - MarkdownV2 rejection fallback: a 400 that mentions a parse/entity failure is retried once with
//!   `parse_mode` removed (`_send_chunk_markdown_or_plain`).
//! - Send failure taxonomy: FloodWait (`429` + `parameters.retry_after`) → `rate_limited` with the
//!   requested delay, `403` → `forbidden`, other 4xx → `bad_format`, 5xx / transport → `transient`.
//! - Partial send: message ids that already landed ride on the failure as
//!   `continuation_message_ids` plus the `partial_overflow` marker (`_with_partial_send`).
//! - Outbound media ([`PlatformAdapter::send_media`]): `sendPhoto` / `sendVideo` / `sendAudio` /
//!   `sendDocument` chosen per [`MediaKind`], matching hermes' `send_photo` / `send_video` /
//!   `send_audio` / `send_document` helpers. A [`MediaSource::Url`] rides as the media field's
//!   value (Telegram fetches it, no download here); held bytes are uploaded as
//!   `multipart/form-data` with a boundary built in this file, so no new dependency.
//!
//! - Reactions: `setMessageReaction` carries one `ReactionType` array,
//!   `[{"type": "emoji", "emoji": …}]`, to react and an empty array to clear, exactly what
//!   python-telegram-bot builds for hermes' `_set_reaction` / `_clear_reactions`
//!   (adapter.py:7175-7191).
//! - Native draft streaming ([`PlatformAdapter::send_draft`]): `sendMessageDraft` with `chat_id`,
//!   `draft_id` and `text`, the MarkdownV2 render with its one plain-text retry, and the UTF-16 cut
//!   to [`MAX_MESSAGE_LENGTH`] (`send_draft`, adapter.py:4092-4133). The rich variant
//!   (`sendRichMessageDraft`) is not ported; see [`PlatformAdapter::send_draft`].
//!
//! Deliberately not ported (see the module report in the port plan): python-telegram-bot's rich
//! messages, inline keyboards/callbacks, media download/caching, albums (`sendMediaGroup`), GIF
//! animations (`sendAnimation`), voice bubbles (`sendVoice`), video thumbnail/geometry probing,
//! image pre-compression, DM topics/forum routing, per-chat flood cooldowns, reconnect ladders,
//! webhook mode and allowlist gating. Those need runtime state (locks, background tasks, stores,
//! ffmpeg/ffprobe) that does not belong to a stateless wire adapter.

use async_trait::async_trait;
use serde_json::{Value, json};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use crate::event::{MessageEvent, MessageType, Platform, SessionSource, platform_names};
use crate::helpers::truncate_message;
use crate::platform::{
    ChatInfo, MediaKind, MediaSource, OutboundMedia, PlatformAdapter, SendResult, utf16_len,
};
use crate::transport::{HttpRequest, SharedTransport};

/// Bot API origin. Every method URL is `{base}/bot{token}/{method}`.
pub const TELEGRAM_API_BASE: &str = "https://api.telegram.org";

/// Telegram's per-message cap, counted in UTF-16 code units (`MAX_MESSAGE_LENGTH`).
pub const MAX_MESSAGE_LENGTH: usize = 4096;

/// Only these update kinds are requested from `getUpdates`; everything else (callbacks, chat
/// member churn, …) has no event shape here yet.
const ALLOWED_UPDATES: &str = r#"["message","edited_message"]"#;

/// MarkdownV2 is the only parse mode this adapter writes.
const MARKDOWN_V2: &str = "MarkdownV2";

const GET_ME: &str = "getMe";
const GET_UPDATES: &str = "getUpdates";
const SEND_MESSAGE: &str = "sendMessage";
const EDIT_MESSAGE_TEXT: &str = "editMessageText";
const DELETE_MESSAGE: &str = "deleteMessage";
const SEND_CHAT_ACTION: &str = "sendChatAction";
const GET_CHAT: &str = "getChat";
const SEND_PHOTO: &str = "sendPhoto";
const SEND_VIDEO: &str = "sendVideo";
const SEND_AUDIO: &str = "sendAudio";
const SEND_DOCUMENT: &str = "sendDocument";
const SET_MESSAGE_REACTION: &str = "setMessageReaction";
const SEND_MESSAGE_DRAFT: &str = "sendMessageDraft";

/// Longest media URL the Bot API accepts for `photo`/`video`/`audio`/`document`; Telegram answers a
/// longer one with `400 Bad Request: wrong file identifier/HTTP URL specified`.
pub const MAX_MEDIA_URL_LEN: usize = 2000;

/// Bot API caption cap for a media message (`_caption_1024`); a longer caption is cut, exactly as
/// hermes does with `caption[:1024]`.
pub const MAX_MEDIA_CAPTION_CHARS: usize = 1024;

/// Process-wide counter that keeps two uploads in one process from framing with the same boundary.
static MULTIPART_SEQUENCE: AtomicU64 = AtomicU64::new(0);

/// Characters MarkdownV2 requires escaped outside `pre`/`code` entities (`_MDV2_ESCAPE_RE` minus
/// the backslash, which this port leaves alone; see [`escape_markdown_v2`]).
const MARKDOWN_V2_SPECIALS: [char; 18] = [
    '_', '*', '[', ']', '(', ')', '~', '`', '>', '#', '+', '-', '=', '|', '{', '}', '.', '!',
];

/// One parsed `getUpdates` entry: the normalized event plus the update id that produced it.
///
/// Mirrors hermes' `(event, update.update_id)` pair, which `/restart` records so the next gateway
/// process can advance past it.
#[derive(Debug, Clone, PartialEq)]
pub struct ParsedUpdate {
    /// The normalized inbound event.
    pub event: MessageEvent,
    /// Telegram `update_id`; also stored on the event as `platform_update_id`.
    pub update_id: i64,
}

/// Telegram Bot API adapter.
///
/// Holds one shared [`crate::transport::Transport`] and the bot token; all state a hermes
/// adapter keeps in `self._bot`/PTB is passed in or returned here, so two calls never share
/// mutable state.
pub struct TelegramAdapter {
    transport: SharedTransport,
    token: String,
    api_base: String,
}

impl TelegramAdapter {
    /// Build an adapter for `token` over `transport`.
    pub fn new(transport: SharedTransport, token: impl Into<String>) -> Self {
        Self {
            transport,
            token: token.into(),
            api_base: TELEGRAM_API_BASE.to_string(),
        }
    }

    /// Point the adapter at a mirror or a local test double.
    pub fn with_api_base(mut self, base: impl Into<String>) -> Self {
        self.api_base = base.into().trim_end_matches('/').to_string();
        self
    }

    /// `{base}/bot{token}/{method}`, the Bot API URL shape.
    fn api_url(&self, method: &str) -> String {
        format!("{}/bot{}/{method}", self.api_base, self.token)
    }

    /// POST a JSON method and unwrap `result`, mapping every failure to [`ApiFailure`].
    async fn call_api(&self, method: &str, payload: &Value) -> Result<Value, ApiFailure> {
        self.call_request(HttpRequest::json(self.api_url(method), payload))
            .await
    }

    /// Perform one already-built request (JSON, form or multipart) and unwrap `result`, mapping
    /// every failure to [`ApiFailure`] so both the text and the media paths share one taxonomy.
    async fn call_request(&self, request: HttpRequest) -> Result<Value, ApiFailure> {
        let response = match self.transport.execute(request).await {
            Ok(response) => response,
            Err(err) => return Err(ApiFailure::Transport(err.to_string())),
        };
        let parsed: Value = serde_json::from_str(&response.body).unwrap_or(Value::Null);
        if !response.ok() {
            return Err(ApiFailure::Status {
                status: response.status,
                description: api_description(&parsed, response.status),
                body: response.body,
                retry_after: api_retry_after(&parsed, response.retry_after),
            });
        }
        if parsed.get("ok").and_then(Value::as_bool) == Some(true) {
            return Ok(parsed.get("result").cloned().unwrap_or(Value::Null));
        }
        Err(ApiFailure::Status {
            status: response.status,
            description: api_description(&parsed, response.status),
            body: response.body,
            retry_after: api_retry_after(&parsed, response.retry_after),
        })
    }

    /// Deliver already-rendered `chunks` in order, returning the shared [`SendResult`] contract.
    async fn send_rendered_chunks(
        &self,
        chat_id: &str,
        chunks: &[String],
        reply_to: Option<&str>,
    ) -> SendResult {
        let total = chunks.len();
        // hermes `_reply_to_mode` defaults to "first": only the opening chunk carries the anchor.
        let reply_anchor = reply_to.and_then(|value| value.trim().parse::<i64>().ok());
        let mut delivered: Vec<String> = Vec::with_capacity(total);
        for (index, chunk) in chunks.iter().enumerate() {
            let mut payload = json!({
                "chat_id": chat_id,
                "text": chunk,
                "parse_mode": MARKDOWN_V2,
            });
            if index == 0
                && let Some(message_id) = reply_anchor
            {
                payload["reply_parameters"] = json!({
                    "message_id": message_id,
                    "allow_sending_without_reply": true,
                });
            }
            let outcome = match self.call_api(SEND_MESSAGE, &payload).await {
                Err(failure) if failure.is_parse_rejection() => {
                    let mut plain = payload.clone();
                    if let Some(object) = plain.as_object_mut() {
                        object.remove("parse_mode");
                    }
                    self.call_api(SEND_MESSAGE, &plain).await
                }
                other => other,
            };
            match outcome {
                Ok(result) => match message_id_from(&result) {
                    Some(message_id) => delivered.push(message_id),
                    None => {
                        return mark_partial(
                            SendResult::failed(
                                "telegram sendMessage response carried no message_id",
                                Some("unknown"),
                            ),
                            &delivered,
                            total,
                        );
                    }
                },
                Err(failure) => {
                    return mark_partial(failure.into_send_result(), &delivered, total);
                }
            }
        }
        match delivered.last() {
            Some(last) => SendResult::ok_split(last.clone(), delivered),
            None => SendResult::failed("telegram sendMessage delivered no chunks", Some("unknown")),
        }
    }

    /// POST `setMessageReaction` and report whether the Bot API confirmed the change.
    ///
    /// `reaction` is the whole `reaction` field: `[{"type": "emoji", "emoji": …}]` to react and `[]`
    /// to clear every reaction the bot left on the message (`setMessageReaction` replaces the bot's
    /// reaction list rather than appending to it). The Bot API documents `True` as the result, so a
    /// transport failure, a non-2xx status, an `ok: false` envelope and a 200 that does not carry
    /// `result: true` all report `false`.
    async fn set_reaction(&self, chat_id: &str, message_id: i64, reaction: Value) -> bool {
        let payload = json!({
            "chat_id": chat_id,
            "message_id": message_id,
            "reaction": reaction,
        });
        match self.call_api(SET_MESSAGE_REACTION, &payload).await {
            Ok(result) => result.as_bool() == Some(true),
            Err(_) => false,
        }
    }
}

#[async_trait]
impl PlatformAdapter for TelegramAdapter {
    /// `getMe`. True only when the Bot API answers `ok: true` with a `result.id`, which is also the
    /// only way to know the token is live; `is_reconnect` needs no different handling because
    /// long-poll offsets live with the caller (`poll_once`).
    async fn connect(&mut self, _is_reconnect: bool) -> bool {
        let request = HttpRequest::get(self.api_url(GET_ME));
        let response = match self.transport.execute(request).await {
            Ok(response) => response,
            Err(_) => return false,
        };
        if !response.ok() {
            return false;
        }
        match response.json() {
            Ok(body) => {
                body.get("ok").and_then(Value::as_bool) == Some(true)
                    && body
                        .get("result")
                        .and_then(|result| result.get("id"))
                        .is_some_and(|id| !id.is_null())
            }
            Err(_) => false,
        }
    }

    /// Nothing is held client-side (no PTB application, no polling task), so there is nothing to tear
    /// down; the caller simply stops calling `poll_once`.
    async fn disconnect(&mut self) {}

    /// Render to MarkdownV2, chunk to [`MAX_MESSAGE_LENGTH`] UTF-16 units, send sequentially.
    ///
    /// Empty or whitespace-only content short-circuits to a successful no-op (hermes skips it to
    /// avoid Telegram's empty-text 400).
    async fn send(
        &self,
        chat_id: &str,
        content: &str,
        reply_to: Option<&str>,
        _metadata: Option<Value>,
    ) -> SendResult {
        if content.trim().is_empty() {
            return empty_send_result();
        }
        let escaped = markdown_to_telegram(content);
        let chunks = truncate_message(&escaped, MAX_MESSAGE_LENGTH, utf16_len);
        if chunks.is_empty() {
            return empty_send_result();
        }
        self.send_rendered_chunks(chat_id, &chunks, reply_to).await
    }

    /// Telegram delivers photos, videos, audio and documents as first-class messages, so
    /// [`send_media`](Self::send_media) is implemented.
    fn supports_media(&self) -> bool {
        true
    }

    /// Upload one attachment through the Bot API method its [`MediaKind`] maps to.
    ///
    /// Method selection mirrors hermes' typed python-telegram-bot helpers — `send_photo` for an
    /// image (adapter.py:5442), `send_video` for a video (`{"video": f, …}`, adapter.py:5412/5420),
    /// `send_audio` for audio (`audio=…`, adapter.py:5229) and `send_document` for a document
    /// (`{"document": f, "filename": …}`, adapter.py:5388) — so the media field is respectively
    /// `photo`, `video`, `audio` and `document`.
    ///
    /// - [`MediaSource::Url`] travels as the media field's value; Telegram fetches it, this adapter
    ///   never downloads it (hermes does the same on its `photo=image_url` path). A URL longer than
    ///   [`MAX_MEDIA_URL_LEN`] is refused here rather than sent to be rejected.
    /// - [`MediaSource::Bytes`] frames one `multipart/form-data` body (see [`multipart_body`]) with
    ///   the `chat_id`, optional `caption` and optional `reply_to_message_id` parts ahead of the
    ///   file part. Empty bytes are refused: Telegram cannot deliver a zero-length upload.
    ///
    /// `caption` rides the media message (cut at [`MAX_MEDIA_CAPTION_CHARS`], as hermes'
    /// `_caption_1024` does); `reply_to` becomes the `reply_to_message_id` part, the anchor name
    /// hermes' shared media funnel sets (`_media_send_kwargs`, adapter.py:5150). A reply anchor that
    /// is not a Telegram message id is dropped, exactly as `_reply_to_message_id_for_send` does,
    /// because the Bot API would otherwise reject the whole upload.
    ///
    /// Failures use the shared taxonomy ([`ApiFailure::into_send_result`]); a refusal never reports
    /// delivery, and a result without `message_id` is a failure rather than a fake success.
    async fn send_media(
        &self,
        chat_id: &str,
        media: &OutboundMedia,
        reply_to: Option<&str>,
    ) -> SendResult {
        let spec = MediaSpec::for_kind(media.kind);
        let caption = media.caption.as_deref().map(caption_within_cap);
        let reply_anchor = reply_to.and_then(|anchor| anchor.trim().parse::<i64>().ok());
        let request = match &media.source {
            MediaSource::Url(url) => {
                if url.trim().is_empty() {
                    return SendResult::failed(
                        "telegram send media: media url is empty",
                        Some("bad_format"),
                    );
                }
                if url.chars().count() > MAX_MEDIA_URL_LEN {
                    return SendResult::failed(
                        format!(
                            "telegram send media: media url is longer than the Bot API limit of \
                             {MAX_MEDIA_URL_LEN} characters"
                        ),
                        Some("bad_format"),
                    );
                }
                let mut fields: Vec<(&str, String)> = Vec::with_capacity(4);
                fields.push(("chat_id", chat_id.to_string()));
                fields.push((spec.field, url.clone()));
                if let Some(caption) = caption {
                    fields.push(("caption", caption));
                }
                if let Some(anchor) = reply_anchor {
                    fields.push(("reply_to_message_id", anchor.to_string()));
                }
                HttpRequest::form(self.api_url(spec.method), &fields)
            }
            MediaSource::Bytes(bytes) => {
                if bytes.is_empty() {
                    return SendResult::failed(
                        "telegram send media: refusing to upload an empty file",
                        Some("bad_format"),
                    );
                }
                let mut fields: Vec<(&str, String)> = Vec::with_capacity(3);
                fields.push(("chat_id", chat_id.to_string()));
                if let Some(caption) = caption {
                    fields.push(("caption", caption));
                }
                if let Some(anchor) = reply_anchor {
                    fields.push(("reply_to_message_id", anchor.to_string()));
                }
                let filename = media_filename(media.filename.as_deref(), spec.default_filename);
                let content_type = media
                    .mime
                    .as_deref()
                    .map(header_safe)
                    .filter(|mime| !mime.is_empty())
                    .unwrap_or_else(|| spec.default_mime.to_string());
                let MultipartBody { boundary, body } = multipart_body(
                    &fields,
                    MediaPart {
                        field: spec.field,
                        filename: &filename,
                        content_type: &content_type,
                        bytes,
                    },
                );
                HttpRequest::post(self.api_url(spec.method))
                    .header(
                        "Content-Type",
                        format!("multipart/form-data; boundary={boundary}"),
                    )
                    .body(body)
            }
        };
        match self.call_request(request).await {
            Ok(result) => match message_id_from(&result) {
                Some(message_id) => SendResult::ok(message_id),
                None => SendResult::failed(
                    format!("telegram {} response carried no message_id", spec.method),
                    Some("unknown"),
                ),
            },
            Err(failure) => failure.into_send_result(),
        }
    }

    /// `sendMessageDraft` is available to every bot on Bot API 9.5, so the native preview is
    /// advertised.
    ///
    /// hermes gates the capability on the chat class: `sendMessageDraft` works for private chats
    /// only, so `supports_draft_streaming(chat_type)` answers false for groups, which keep the
    /// edit-based preview (`adapter.py:4085-4090`). This trait method is asked without a chat, and
    /// the adapter cannot know the class of a chat it has not seen, so the flag reports the API's
    /// availability; the private-chat condition belongs to the caller that knows the chat.
    fn supports_draft_streaming(&self) -> bool {
        true
    }

    /// POST `sendMessageDraft` to animate one preview frame, the plain half of hermes' `send_draft`
    /// (`adapter.py:4092-4133`).
    ///
    /// The reference first tries a *rich* draft (`sendRichMessageDraft`, adapter.py:4097-4099) and
    /// degrades to `sendMessageDraft` whenever that fails. The rich call is a separate Bot API
    /// method (`_try_send_rich_draft`) that this port does not implement and cannot verify here, so
    /// every frame takes the plain path: the rich draft is a formatting upgrade, never the only way
    /// to stream, and the reference's own fallback produces exactly this request.
    ///
    /// Wire shape, per `adapter.py:4112-4120`: `chat_id` (trimmed), `draft_id` (an integer; the
    /// same id animates the same preview, a new id starts a new one) and `text`, plus `parse_mode:
    /// MarkdownV2` on the first attempt.
    ///
    /// - The text is the content cut to [`MAX_MESSAGE_LENGTH`] UTF-16 units *before* rendering, as
    ///   the reference truncates with `len_fn=utf16_len` ahead of `format_message`
    ///   (adapter.py:4102-4114). Rendering escapes characters and can therefore push a frame that
    ///   fit by a few units back over the cap; the reference has the same edge and lets the API
    ///   answer it, so an over-cap frame surfaces as the typed failure below rather than a silently
    ///   shortened preview.
    /// - A 400 blaming markdown parsing is retried once with the raw text and no `parse_mode`,
    ///   the same fallback `send` and `edit_message` take (adapter.py:4111-4130).
    /// - Success requires the Bot API's `ok: true` answer. Drafts are not messages, so the result
    ///   carries no `message_id`; every other answer — a 403, a 429 FloodWait, a 5xx, a transport
    ///   failure — maps through [`ApiFailure::into_send_result`] and never reports delivery.
    /// - Blank content or a blank chat id is refused before any request: the Bot API rejects empty
    ///   `text`, and a draft addressed to nowhere is not a preview.
    async fn send_draft(&self, chat_id: &str, draft_id: u64, content: &str) -> SendResult {
        let chat_id = chat_id.trim();
        if chat_id.is_empty() {
            return SendResult::failed("telegram send draft: chat id is empty", Some("bad_format"));
        }
        if content.trim().is_empty() {
            return SendResult::failed("telegram send draft: content is empty", Some("bad_format"));
        }
        let text = match draft_text_within_cap(content) {
            Some(text) => text,
            None => {
                return SendResult::failed(
                    "telegram send draft: content produced no text within the message cap",
                    Some("too_long"),
                );
            }
        };
        let mut payload = json!({
            "chat_id": chat_id,
            "draft_id": draft_id,
            "text": markdown_to_telegram(&text),
            "parse_mode": MARKDOWN_V2,
        });
        let outcome = match self.call_api(SEND_MESSAGE_DRAFT, &payload).await {
            Err(failure) if failure.is_parse_rejection() => {
                // MarkdownV2 rejected: retry once with the raw text, exactly as the reference does.
                if let Some(object) = payload.as_object_mut() {
                    object.remove("parse_mode");
                    object.insert("text".to_string(), json!(text));
                }
                self.call_api(SEND_MESSAGE_DRAFT, &payload).await
            }
            other => other,
        };
        match outcome {
            Ok(_) => draft_sent_result(),
            Err(failure) => failure.into_send_result(),
        }
    }

    /// Telegram has `setMessageReaction`, so [`add_reaction`](Self::add_reaction) is implemented.
    fn supports_reactions(&self) -> bool {
        true
    }

    /// React to `message_id` with one emoji through `setMessageReaction`.
    ///
    /// hermes `_set_reaction` (adapter.py:7175-7187) passes the emoji string to
    /// `set_message_reaction(chat_id=…, message_id=…, reaction=emoji)`; python-telegram-bot
    /// serializes that as the single-element `ReactionType` array the Bot API documents,
    /// `{"reaction": [{"type": "emoji", "emoji": "👍"}]}`, alongside the chat and message ids.
    ///
    /// Best effort, but never a fake success: `true` requires the documented
    /// `{"ok": true, "result": true}` answer, and every other outcome — a transport failure, a
    /// non-2xx status, an error envelope, a 200 without `result: true` — is `false`.
    ///
    /// Nothing is sent when the request could not be valid: a blank chat id, a blank emoji, or a
    /// message id that is not a Bot API integer (the reference casts with `int(message_id)` and its
    /// `except` arm turns a cast failure into `False`). A chat id may be numeric (`-1001234567890`)
    /// or an `@username` target, matching `normalize_telegram_chat_id`.
    async fn add_reaction(&self, chat_id: &str, message_id: &str, emoji: &str) -> bool {
        let Some((chat_id, message_id)) = reaction_target(chat_id, message_id) else {
            return false;
        };
        let Some(reaction) = reaction_types(emoji) else {
            return false;
        };
        self.set_reaction(&chat_id, message_id, reaction).await
    }

    /// Clear the bot's reaction on `message_id`.
    ///
    /// The Bot API has no per-emoji removal on this path: `setMessageReaction` replaces the bot's
    /// reaction list, and an empty list is the documented way to clear it — exactly what hermes
    /// `_clear_reactions` does (`_set_reaction(…, None)`, adapter.py:7189-7191). `emoji` is
    /// therefore validated but not transmitted; a blank emoji or an unusable id/chat id is refused
    /// before the request, and only the documented `result: true` answer reports success.
    async fn remove_reaction(&self, chat_id: &str, message_id: &str, emoji: &str) -> bool {
        let Some((chat_id, message_id)) = reaction_target(chat_id, message_id) else {
            return false;
        };
        if emoji.trim().is_empty() {
            return false;
        }
        self.set_reaction(&chat_id, message_id, json!([])).await
    }

    /// `getChat`, mapped onto the gateway's three chat classes.
    ///
    /// `private` → `dm`, `group`/`supergroup` → `group`, `channel` → `channel`. Any failure degrades to
    /// `{name: chat_id, chat_type: "dm"}` instead of erroring, so callers never lose routing.
    async fn get_chat_info(&self, chat_id: &str) -> ChatInfo {
        let fallback = ChatInfo {
            name: chat_id.to_string(),
            chat_type: "dm".to_string(),
        };
        let payload = json!({ "chat_id": chat_id });
        let result = match self.call_api(GET_CHAT, &payload).await {
            Ok(result) => result,
            Err(_) => return fallback,
        };
        ChatInfo {
            chat_type: chat_type_from_telegram(
                result
                    .get("type")
                    .and_then(Value::as_str)
                    .unwrap_or("private"),
            )
            .to_string(),
            name: display_name(&result).unwrap_or_else(|| chat_id.to_string()),
        }
    }

    /// Renders `pre`/`code` blocks, so fenced code survives.
    fn supports_code_blocks(&self) -> bool {
        true
    }

    /// `send` chunks natively; the delivery router must not truncate first.
    fn splits_long_messages(&self) -> bool {
        true
    }

    fn max_message_length(&self) -> usize {
        MAX_MESSAGE_LENGTH
    }

    /// Telegram counts UTF-16 code units, not code points (`message_len_fn`).
    fn message_length(&self, text: &str) -> usize {
        utf16_len(text)
    }

    /// Standard markdown → MarkdownV2 (`format_message`).
    fn format_message(&self, content: &str) -> String {
        markdown_to_telegram(content)
    }

    /// `sendChatAction` with `action: "typing"`; the indicator expires on its own, so failures are
    /// ignored (it is a UI hint, never a delivery).
    async fn send_typing(&self, chat_id: &str, _metadata: Option<Value>) {
        let payload = json!({ "chat_id": chat_id, "action": "typing" });
        let _ = self.call_api(SEND_CHAT_ACTION, &payload).await;
    }

    /// Telegram typing state is server-timed and cannot be cancelled through the Bot API.
    async fn stop_typing(&self, _chat_id: &str) {}

    /// `editMessageText` with MarkdownV2, falling back once to plain text on a parse rejection.
    ///
    /// A `not modified` 400 is reported as success: the message already shows that exact text.
    /// `finalize` is a DingTalk-style contract; Telegram has no final-edit notion, so it is accepted
    /// and ignored. The text is not chunked here (an edit targets one message); an oversized edit is
    /// rejected by Telegram and surfaces as a failure.
    async fn edit_message(
        &self,
        chat_id: &str,
        message_id: &str,
        content: &str,
        _finalize: bool,
    ) -> SendResult {
        let payload = json!({
            "chat_id": chat_id,
            "message_id": message_id_value(message_id),
            "text": markdown_to_telegram(content),
            "parse_mode": MARKDOWN_V2,
        });
        let outcome = match self.call_api(EDIT_MESSAGE_TEXT, &payload).await {
            Err(failure) if failure.is_parse_rejection() => {
                let mut plain = payload.clone();
                if let Some(object) = plain.as_object_mut() {
                    object.remove("parse_mode");
                }
                self.call_api(EDIT_MESSAGE_TEXT, &plain).await
            }
            other => other,
        };
        match outcome {
            Ok(result) => {
                SendResult::ok(message_id_from(&result).unwrap_or_else(|| message_id.to_string()))
            }
            Err(failure) if failure.is_not_modified() => SendResult::ok(message_id),
            Err(failure) => failure.into_send_result(),
        }
    }

    /// `deleteMessage`, best effort (the Bot API only allows the bot's own messages, ~48 h back).
    async fn delete_message(&self, chat_id: &str, message_id: &str) {
        let payload = json!({
            "chat_id": chat_id,
            "message_id": message_id_value(message_id),
        });
        let _ = self.call_api(DELETE_MESSAGE, &payload).await;
    }
}

impl TelegramAdapter {
    /// Long-poll `getUpdates` once and normalize whatever came back.
    ///
    /// `offset` is the standard Telegram acknowledgement cursor (highest handled `update_id + 1`);
    /// `timeout_secs` is the server-side long-poll hold. Only `message` and `edited_message` are
    /// requested. Transport errors, non-2xx statuses, `ok: false` bodies and unparseable JSON are
    /// reported as `Err`, so a caller can back off; updates this adapter has no event shape for are
    /// skipped rather than faked.
    pub async fn poll_once(
        &self,
        offset: Option<i64>,
        timeout_secs: u32,
    ) -> Result<Vec<ParsedUpdate>, String> {
        let mut url = format!("{}/bot{}/{GET_UPDATES}?", self.api_base, self.token);
        if let Some(offset) = offset {
            url.push_str("offset=");
            url.push_str(&offset.to_string());
            url.push('&');
        }
        url.push_str("timeout=");
        url.push_str(&timeout_secs.to_string());
        url.push_str("&allowed_updates=");
        url.push_str(&encode_query_value(ALLOWED_UPDATES));

        let response = self
            .transport
            .execute(HttpRequest::get(url))
            .await
            .map_err(|err| format!("telegram getUpdates transport error: {err}"))?;
        if !response.ok() {
            return Err(format!(
                "telegram getUpdates failed with status {}: {}",
                response.status,
                shortened(&response.body, 200)
            ));
        }
        let parsed: Value = serde_json::from_str(&response.body)
            .map_err(|err| format!("telegram getUpdates returned invalid json: {err}"))?;
        if parsed.get("ok").and_then(Value::as_bool) != Some(true) {
            return Err(format!(
                "telegram getUpdates was rejected: {}",
                api_description(&parsed, response.status)
            ));
        }
        let updates = match parsed.get("result").and_then(Value::as_array) {
            Some(updates) => updates,
            None => return Ok(Vec::new()),
        };
        Ok(updates.iter().filter_map(parse_update).collect())
    }
}

/// How one Bot API call failed, carrying exactly what the taxonomy needs.
#[derive(Debug)]
enum ApiFailure {
    /// The request never produced a response (connect/TLS/timeout): safe to retry.
    Transport(String),
    /// A response arrived but the call did not succeed.
    Status {
        status: u16,
        description: String,
        body: String,
        retry_after: Option<f64>,
    },
}

impl ApiFailure {
    /// The `SendResult` failure taxonomy: FloodWait, forbidden, bad format, transient.
    fn into_send_result(self) -> SendResult {
        match self {
            Self::Transport(err) => SendResult::failed(
                format!("telegram transport error: {err}"),
                Some("transient"),
            )
            .retryable(None),
            Self::Status {
                status,
                description,
                body,
                retry_after,
            } => {
                let message = if description.is_empty() {
                    body
                } else {
                    description
                };
                match status {
                    429 => SendResult::failed(message, Some("rate_limited")).retryable(retry_after),
                    403 => SendResult::failed(message, Some("forbidden")),
                    500..=599 => SendResult::failed(message, Some("transient")).retryable(None),
                    _ => SendResult::failed(message, Some("bad_format")),
                }
            }
        }
    }

    /// A 400 whose body blames markdown parsing: Telegram could not read our entities, so the same
    /// text is worth one retry with `parse_mode` removed (`_send_chunk_markdown_or_plain`).
    fn is_parse_rejection(&self) -> bool {
        match self {
            Self::Status {
                status: 400, body, ..
            } => {
                let body = body.to_lowercase();
                body.contains("parse") || body.contains("entities")
            }
            _ => false,
        }
    }

    /// Telegram's "the edit changed nothing" 400, which is a success from the caller's point of view.
    fn is_not_modified(&self) -> bool {
        match self {
            Self::Status {
                status: 400,
                description,
                body,
                ..
            } => format!("{description} {body}")
                .to_lowercase()
                .contains("not modified"),
            _ => false,
        }
    }
}

/// The Bot API upload endpoint for one [`MediaKind`], with the form field and defaults it needs.
///
/// hermes reaches the same four endpoints through python-telegram-bot's typed helpers:
/// `send_photo(photo=…)` (adapter.py:5442), `send_video(video=…)` (adapter.py:5412/5420),
/// `send_audio(audio=…)` (adapter.py:5229) and `send_document(document=…, filename=…)`
/// (adapter.py:5388).
#[derive(Debug, Clone, Copy)]
struct MediaSpec {
    /// Bot API method appended to `{base}/bot{token}/`.
    method: &'static str,
    /// Name of the form field carrying the media itself.
    field: &'static str,
    /// Filename used when the caller names none.
    default_filename: &'static str,
    /// Content type used when the caller supplies no `mime`.
    default_mime: &'static str,
}

impl MediaSpec {
    /// The method/field/defaults triple for `kind`.
    fn for_kind(kind: MediaKind) -> Self {
        match kind {
            MediaKind::Image => Self {
                method: SEND_PHOTO,
                field: "photo",
                default_filename: "photo.jpg",
                default_mime: "image/jpeg",
            },
            MediaKind::Video => Self {
                method: SEND_VIDEO,
                field: "video",
                default_filename: "video.mp4",
                default_mime: "video/mp4",
            },
            MediaKind::Audio => Self {
                method: SEND_AUDIO,
                field: "audio",
                default_filename: "audio.mp3",
                default_mime: "audio/mpeg",
            },
            MediaKind::Document => Self {
                method: SEND_DOCUMENT,
                field: "document",
                default_filename: "document.bin",
                default_mime: "application/octet-stream",
            },
        }
    }
}

/// The single file part of an upload: the form field it fills, its filename, content type and bytes.
struct MediaPart<'a> {
    field: &'a str,
    filename: &'a str,
    content_type: &'a str,
    bytes: &'a [u8],
}

/// A framed `multipart/form-data` body: the boundary token and the bytes to POST.
struct MultipartBody {
    boundary: String,
    body: Vec<u8>,
}

/// Frame `fields` (short text parts) plus one file part as `multipart/form-data` (RFC 7578).
///
/// Every part opens with `--{boundary}\r\n`, carries its headers, one blank line, its value and a
/// trailing CRLF; the body closes with `--{boundary}--\r\n`. Only CRLF ends a line — a bare LF would
/// be read as part of the value.
fn multipart_body(fields: &[(&str, String)], file: MediaPart<'_>) -> MultipartBody {
    // A boundary that occurs inside the payload would split the body early, so it is chosen
    // against the bytes being uploaded.
    let boundary = multipart_boundary(file.bytes);
    let mut body = Vec::with_capacity(file.bytes.len() + 512);
    for (name, value) in fields {
        body.extend_from_slice(b"--");
        body.extend_from_slice(boundary.as_bytes());
        body.extend_from_slice(b"\r\nContent-Disposition: form-data; name=\"");
        body.extend_from_slice(name.as_bytes());
        body.extend_from_slice(b"\"\r\n\r\n");
        body.extend_from_slice(value.as_bytes());
        body.extend_from_slice(b"\r\n");
    }
    body.extend_from_slice(b"--");
    body.extend_from_slice(boundary.as_bytes());
    body.extend_from_slice(b"\r\nContent-Disposition: form-data; name=\"");
    body.extend_from_slice(file.field.as_bytes());
    body.extend_from_slice(b"\"; filename=\"");
    body.extend_from_slice(file.filename.as_bytes());
    body.extend_from_slice(b"\"\r\nContent-Type: ");
    body.extend_from_slice(file.content_type.as_bytes());
    body.extend_from_slice(b"\r\n\r\n");
    body.extend_from_slice(file.bytes);
    body.extend_from_slice(b"\r\n--");
    body.extend_from_slice(boundary.as_bytes());
    body.extend_from_slice(b"--\r\n");
    MultipartBody { boundary, body }
}

/// A fresh boundary token: the process counter plus wall-clock nanoseconds, both hex.
fn fresh_boundary() -> String {
    let sequence = MULTIPART_SEQUENCE.fetch_add(1, Ordering::Relaxed);
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |elapsed| elapsed.as_nanos());
    format!("vikramFormBoundary{sequence:016x}{nanos:032x}")
}

/// A boundary token that does not occur inside `payload`; regenerated (bounded) when it does.
///
/// After eight distinct candidates the last one is returned anyway: a token that long appearing
/// verbatim inside a real upload is not a payload this adapter has to survive.
fn multipart_boundary(payload: &[u8]) -> String {
    let mut candidate = fresh_boundary();
    let mut attempt = 0u32;
    while attempt < 8 && contains_bytes(payload, candidate.as_bytes()) {
        candidate = fresh_boundary();
        attempt += 1;
    }
    candidate
}

/// Whether `needle` occurs inside `haystack`; an empty needle never matches.
fn contains_bytes(haystack: &[u8], needle: &[u8]) -> bool {
    !needle.is_empty()
        && needle.len() <= haystack.len()
        && haystack
            .windows(needle.len())
            .any(|window| window == needle)
}

/// The filename a part header may carry: the last path component of `raw`, stripped of quotes and
/// control characters (a raw CRLF would end the header line), else `default`.
fn media_filename(raw: Option<&str>, default: &'static str) -> String {
    let basename = raw
        .and_then(|value| value.rsplit(['/', '\\']).next())
        .unwrap_or_default();
    let cleaned = header_safe(basename);
    if cleaned.is_empty() {
        default.to_string()
    } else {
        cleaned
    }
}

/// Strip what must never reach a header line (`"`, `\r`, `\n`, other control characters) and trim.
fn header_safe(value: &str) -> String {
    let mut cleaned = String::with_capacity(value.len());
    for ch in value.chars() {
        if !ch.is_control() && ch != '"' {
            cleaned.push(ch);
        }
    }
    cleaned.trim().to_string()
}

/// A caption cut to the Bot API cap (`_caption_1024`, adapter.py:5161: `caption[:1024]`).
fn caption_within_cap(caption: &str) -> String {
    caption.chars().take(MAX_MEDIA_CAPTION_CHARS).collect()
}

/// Render `content` for `parse_mode: MarkdownV2`.
///
/// Port of hermes `format_message`: fenced blocks are kept byte-for-byte (Telegram renders them as
/// `pre`), `**bold**` becomes `*bold*`, `__italic__` becomes `_italic_`, inline code is preserved,
/// and everything else is escaped with [`escape_markdown_v2`]'s rule set.
///
/// Not ported from hermes' `format_message`: GFM pipe-table rewriting, header→bold, `*italic*` →
/// `_italic_`, `~~strike~~`, `||spoiler||`, blockquote and link handling. Those need nested-regex
/// passes this dependency-free port does not attempt; their characters are escaped as literals
/// instead, which always renders (never a parse error) even when styling is lost.
pub fn markdown_to_telegram(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut in_fence = false;
    for line in text.split_inclusive('\n') {
        if in_fence {
            out.push_str(line);
            if is_fence_line(line) {
                in_fence = false;
            }
            continue;
        }
        if is_fence_line(line) {
            in_fence = true;
            out.push_str(line);
            continue;
        }
        convert_prose_line(line, &mut out);
    }
    out
}

/// Escape every MarkdownV2 special character outside fenced blocks and inline code spans.
///
/// The special set is Telegram's: `_` `*` `[` `]` `(` `)` `~` `` ` `` `>` `#` `+` `-` `=` `|` `{`
/// `}` `.` `!`. Content inside a fenced block or an inline code span is copied verbatim, because
/// MarkdownV2 treats it as a `pre`/`code` entity rather than text; an unmatched backtick run is
/// escaped like any other character instead of swallowing the rest of the line.
///
/// The backslash itself is not escaped: this port only ever escapes text it produced, and Telegram
/// reading a doubled backslash would surface as a visible `\` in the message.
pub fn escape_markdown_v2(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut in_fence = false;
    for line in text.split_inclusive('\n') {
        if in_fence {
            out.push_str(line);
            if is_fence_line(line) {
                in_fence = false;
            }
            continue;
        }
        if is_fence_line(line) {
            in_fence = true;
            out.push_str(line);
            continue;
        }
        escape_inline(line, &mut out);
    }
    out
}

/// A successful send that produced no message (empty content), keeping `message_id` empty rather
/// than inventing one.
fn empty_send_result() -> SendResult {
    let mut result = SendResult::ok(String::new());
    result.message_id = None;
    result
}

/// A successful draft frame: a draft is not a message, so no id is reported.
///
/// hermes returns `SendResult(success=True, message_id=None)` for the same reason — "Drafts have no
/// message_id" (adapter.py:4097) — and the caller sends the final text through `send`. Inventing an
/// id here would point a later edit at a message that does not exist.
fn draft_sent_result() -> SendResult {
    let mut result = SendResult::ok(String::new());
    result.message_id = None;
    result
}

/// One draft frame's text: the content cut to [`MAX_MESSAGE_LENGTH`] UTF-16 code units.
///
/// The reference keeps the content when it fits and otherwise truncates through the regular send
/// contract's UTF-16 measure, taking the first chunk (adapter.py:4102-4104). `None` means the
/// truncation produced nothing at all, which is never sent as a draft.
fn draft_text_within_cap(content: &str) -> Option<String> {
    if utf16_len(content) <= MAX_MESSAGE_LENGTH {
        return Some(content.to_string());
    }
    truncate_message(content, MAX_MESSAGE_LENGTH, utf16_len)
        .into_iter()
        .next()
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

/// `message_id` as a number when it parses (the Bot API documents integers), else as the string the
/// caller handed us so an opaque id is still forwarded.
fn message_id_value(message_id: &str) -> Value {
    match message_id.trim().parse::<i64>() {
        Ok(value) => json!(value),
        Err(_) => json!(message_id),
    }
}

/// The `(chat_id, message_id)` pair a reaction call addresses, or `None` when it cannot address one.
///
/// A reaction needs a chat and an integer message id. The chat id keeps its string form — the Bot
/// API takes numeric ids (`-1001234567890`) and `@username` targets alike, which is what
/// `normalize_telegram_chat_id` guards upstream — while the message id is parsed, because hermes'
/// `int(message_id)` raises on anything else and its `except` arm reports the reaction as failed.
fn reaction_target(chat_id: &str, message_id: &str) -> Option<(String, i64)> {
    let chat_id = chat_id.trim();
    if chat_id.is_empty() {
        return None;
    }
    let message_id = message_id.trim().parse::<i64>().ok()?;
    Some((chat_id.to_string(), message_id))
}

/// The Bot API `reaction` field that sets one emoji, or `None` when there is no emoji to set.
///
/// `ReactionType` is a tagged union; the reference only ever produces the `emoji` variant
/// (`[{"type": "emoji", "emoji": …}]`) because python-telegram-bot builds it from the plain string
/// hermes passes to `set_message_reaction`. A custom-emoji id — the `custom_emoji` variant, keyed by
/// `custom_emoji_id` — is therefore *not* supported here: the caller's string cannot say which
/// variant it means, and guessing would send the wrong reaction. Surrounding whitespace is dropped
/// (a padded emoji is never what a caller meant); the emoji itself travels verbatim as UTF-8.
fn reaction_types(emoji: &str) -> Option<Value> {
    let emoji = emoji.trim();
    if emoji.is_empty() {
        return None;
    }
    Some(json!([{ "type": "emoji", "emoji": emoji }]))
}

/// Pull `result.message_id` out of a Bot API response body.
fn message_id_from(result: &Value) -> Option<String> {
    result.get("message_id").and_then(value_to_id)
}

/// Human-readable error text: the Bot API `description`, else the HTTP status.
fn api_description(body: &Value, status: u16) -> String {
    match body.get("description").and_then(Value::as_str) {
        Some(description) if !description.trim().is_empty() => description.to_string(),
        _ => format!("telegram api returned status {status}"),
    }
}

/// Seconds Telegram asked us to wait: `parameters.retry_after` first (FloodWait), then the
/// `Retry-After` header the transport already parsed.
fn api_retry_after(body: &Value, header: Option<f64>) -> Option<f64> {
    body.get("parameters")
        .and_then(|parameters| parameters.get("retry_after"))
        .and_then(Value::as_f64)
        .or(header)
}

/// First `limit` characters plus an ellipsis, for error text.
fn shortened(text: &str, limit: usize) -> String {
    if text.chars().count() <= limit {
        return text.to_string();
    }
    let mut out: String = text.chars().take(limit).collect();
    out.push('…');
    out
}

/// Percent-encode a query-string value (unreserved characters kept, everything else `%XX`), used for
/// the JSON-array `allowed_updates` parameter.
fn encode_query_value(value: &str) -> String {
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
fn telegram_platform() -> Platform {
    match Platform::new(platform_names::TELEGRAM) {
        Some(platform) => platform,
        None => Platform::local(),
    }
}

/// Telegram `chat.type` → gateway chat class. Unknown types are treated as DMs (the tooling treats
/// unknown classes leniently, and a DM is the safest routing assumption).
fn chat_type_from_telegram(raw: &str) -> &'static str {
    match raw {
        "group" | "supergroup" => "group",
        "channel" => "channel",
        _ => "dm",
    }
}

/// A Telegram id as a string: integer ids arrive as JSON numbers, but channel ids and mocked
/// payloads may arrive as strings.
fn value_to_id(value: &Value) -> Option<String> {
    match value {
        Value::Number(number) => number
            .as_i64()
            .map(|id| id.to_string())
            .or_else(|| number.as_u64().map(|id| id.to_string())),
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

/// `first_name` + `last_name`, either optional; `None` when both are missing or blank.
fn joined_name(object: &Value) -> Option<String> {
    let first = object
        .get("first_name")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .trim();
    let last = object
        .get("last_name")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .trim();
    let name = match (first.is_empty(), last.is_empty()) {
        (true, true) => String::new(),
        (true, false) => last.to_string(),
        (false, true) => first.to_string(),
        (false, false) => format!("{first} {last}"),
    };
    if name.is_empty() { None } else { Some(name) }
}

/// Best display name for a chat or user object: `title`, then full name, then `@username`.
fn display_name(object: &Value) -> Option<String> {
    let title = object
        .get("title")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|title| !title.is_empty());
    let username = object
        .get("username")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|username| !username.is_empty());
    title
        .map(str::to_string)
        .or_else(|| joined_name(object))
        .or_else(|| username.map(str::to_string))
}

/// Message text: `text`, else the media `caption` (hermes reads both).
fn message_text_opt(message: &Value) -> Option<String> {
    message
        .get("text")
        .and_then(Value::as_str)
        .or_else(|| message.get("caption").and_then(Value::as_str))
        .filter(|text| !text.is_empty())
        .map(str::to_string)
}

/// Inbound attachment facts: kind, Bot API `file_id`s, MIME types and original names, all parallel.
#[derive(Debug, Clone, Default)]
struct InboundMedia {
    message_type: MessageType,
    file_ids: Vec<String>,
    mime_types: Vec<String>,
    filenames: Vec<String>,
}

impl InboundMedia {
    /// A message with no attachment keeps `message_type` at its text default.
    fn none() -> Self {
        Self {
            message_type: MessageType::Text,
            file_ids: Vec::new(),
            mime_types: Vec::new(),
            filenames: Vec::new(),
        }
    }
}

/// One attachment entry from a Telegram media object (`file_id`, `mime_type`, `file_name`).
fn media_entry(message_type: MessageType, file: &Value, fallback_mime: &str) -> InboundMedia {
    let file_id = match file.get("file_id").and_then(Value::as_str) {
        Some(file_id) if !file_id.is_empty() => file_id.to_string(),
        _ => return InboundMedia::none(),
    };
    let mime_type = file
        .get("mime_type")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|mime_type| !mime_type.is_empty())
        .unwrap_or(fallback_mime)
        .to_string();
    let filename = file
        .get("file_name")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_string();
    InboundMedia {
        message_type,
        file_ids: vec![file_id],
        mime_types: vec![mime_type],
        filenames: vec![filename],
    }
}

/// Classify a message's attachment the way hermes `_media_message_type` does: the first present
/// attachment wins, in sticker → photo → video → video_note → audio → voice → document order.
///
/// `file_id`s land in `media_urls` because download/caching is not part of this port: a `file_id` is
/// what the Bot API hands out, and a fetcher can resolve it later with `getFile`.
fn media_from_message(message: &Value) -> InboundMedia {
    if let Some(sticker) = message.get("sticker").filter(|sticker| sticker.is_object()) {
        return media_entry(MessageType::Sticker, sticker, "image/webp");
    }
    if let Some(photos) = message.get("photo").and_then(Value::as_array) {
        // Telegram sends every size, ascending; the last entry is the largest.
        if let Some(best) = photos.last().filter(|photo| photo.is_object()) {
            return media_entry(MessageType::Photo, best, "image/jpeg");
        }
    }
    if let Some(video) = message.get("video").filter(|video| video.is_object()) {
        return media_entry(MessageType::Video, video, "video/mp4");
    }
    if let Some(note) = message.get("video_note").filter(|note| note.is_object()) {
        return media_entry(MessageType::Video, note, "video/mp4");
    }
    if let Some(audio) = message.get("audio").filter(|audio| audio.is_object()) {
        return media_entry(MessageType::Audio, audio, "audio/mpeg");
    }
    if let Some(voice) = message.get("voice").filter(|voice| voice.is_object()) {
        return media_entry(MessageType::Voice, voice, "audio/ogg");
    }
    if let Some(document) = message
        .get("document")
        .filter(|document| document.is_object())
    {
        return media_entry(MessageType::Document, document, "application/octet-stream");
    }
    InboundMedia::none()
}

/// Forum/DM topic thread id, when the message really belongs to a topic (`is_topic_message`); plain
/// reply anchors are not durable threads and are dropped (hermes `_effective_message_thread_id`).
fn message_thread_id(message: &Value) -> Option<String> {
    if message.get("is_topic_message").and_then(Value::as_bool) != Some(true) {
        return None;
    }
    message.get("message_thread_id").and_then(value_to_id)
}

/// Parse one `getUpdates` entry into a [`ParsedUpdate`], or `None` for update kinds without an event
/// shape here (or payloads missing a chat/id).
fn parse_update(update: &Value) -> Option<ParsedUpdate> {
    let update_id = update.get("update_id").and_then(Value::as_i64)?;
    if let Some(message) = update.get("message").filter(|message| message.is_object()) {
        return parse_message(message, update_id, false);
    }
    let message = update
        .get("edited_message")
        .filter(|message| message.is_object())?;
    parse_message(message, update_id, true)
}

/// Build the normalized [`MessageEvent`] from a Telegram `Message` object.
///
/// Port of hermes `_build_message_event` plus `_media_message_type`. An edited message carries
/// `edit_revision = "u{update_id}"` (hermes `_edit_revision`'s update-id form): `edit_date` has
/// whole-second resolution, so the update id is the stable identity of one edit.
fn parse_message(message: &Value, update_id: i64, edited: bool) -> Option<ParsedUpdate> {
    let chat = message.get("chat")?;
    let chat_id = value_to_id(chat.get("id")?)?;
    let chat_type = chat_type_from_telegram(
        chat.get("type")
            .and_then(Value::as_str)
            .unwrap_or("private"),
    );
    let chat_name = display_name(chat);
    let user = message.get("from").filter(|user| user.is_object());
    let message_id = message.get("message_id").and_then(value_to_id);
    let media = media_from_message(message);

    let mut source = SessionSource::new(telegram_platform(), chat_id.clone());
    source.chat_name = chat_name.clone();
    source.chat_type = chat_type.to_string();
    source.user_id = user.and_then(|user| user.get("id")).and_then(value_to_id);
    source.user_name = user.and_then(joined_name).or_else(|| {
        if chat_type == "dm" {
            chat_name.clone()
        } else {
            None
        }
    });
    source.is_bot = user
        .and_then(|user| user.get("is_bot"))
        .and_then(Value::as_bool)
        .unwrap_or(false);
    source.message_id = message_id.clone();
    source.thread_id = message_thread_id(message);

    let mut event = MessageEvent::new(message_text_opt(message).unwrap_or_default(), source);
    event.message_type = media.message_type;
    // hermes mirrors the author onto the event for per-message prompt builders.
    event.user_id = event.source.user_id.clone();
    event.user_name = event.source.user_name.clone();
    event.raw_message = Some(message.clone());
    event.message_id = message_id;
    event.platform_update_id = Some(update_id);
    let reply = message
        .get("reply_to_message")
        .filter(|reply| reply.is_object());
    event.reply_to_message_id = reply
        .and_then(|reply| reply.get("message_id"))
        .and_then(value_to_id);
    event.reply_to_text = reply.and_then(message_text_opt);
    event.media_urls = media.file_ids;
    event.media_types = media.mime_types;
    event.media_filenames = media.filenames;
    if edited {
        event.edit_revision = Some(format!("u{update_id}"));
    }
    Some(ParsedUpdate { event, update_id })
}

/// A line that opens or closes a fenced code block: three or more backticks or tildes, optionally
/// indented, at the start of the line (`parse_fence_open` in `helpers.rs`).
fn is_fence_line(line: &str) -> bool {
    let body = line.trim_start_matches(' ');
    let body = body.strip_suffix('\n').unwrap_or(body);
    let body = body.strip_suffix('\r').unwrap_or(body);
    let marker = match body.chars().next() {
        Some(marker @ ('`' | '~')) => marker,
        _ => return false,
    };
    body.chars().take_while(|ch| *ch == marker).count() >= 3
}

/// Escape one prose line, copying inline code spans through untouched.
fn escape_inline(line: &str, out: &mut String) {
    let chars: Vec<char> = line.chars().collect();
    let mut idx = 0;
    while idx < chars.len() {
        if chars[idx] == '`' {
            let run = run_length(&chars, idx, '`');
            match find_closing_run(&chars, idx + run, '`', run) {
                Some(end) => {
                    out.extend(chars[idx..end].iter());
                    idx = end;
                }
                None => {
                    // Unmatched backtick: a literal, so escape the whole run.
                    for _ in 0..run {
                        out.push_str("\\`");
                    }
                    idx += run;
                }
            }
            continue;
        }
        push_escaped(chars[idx], out);
        idx += 1;
    }
}

/// Convert one prose line to MarkdownV2: stash code, convert bold/italic, escape the rest.
fn convert_prose_line(line: &str, out: &mut String) {
    let chars: Vec<char> = line.chars().collect();
    let mut idx = 0;
    while idx < chars.len() {
        let ch = chars[idx];
        if ch == '`' {
            let run = run_length(&chars, idx, '`');
            match find_closing_run(&chars, idx + run, '`', run) {
                Some(end) => {
                    out.extend(chars[idx..end].iter());
                    idx = end;
                }
                None => {
                    for _ in 0..run {
                        out.push_str("\\`");
                    }
                    idx += run;
                }
            }
            continue;
        }
        if ch == '*' && chars.get(idx + 1) == Some(&'*') {
            if let Some((close, next)) = find_emphasis_close(&chars, idx + 2, '*')
                && close > idx + 2
            {
                out.push('*');
                push_escaped_all(&chars[idx + 2..close], out);
                out.push('*');
                idx = next;
                continue;
            }
            out.push_str("\\*");
            idx += 1;
            continue;
        }
        if ch == '_'
            && chars.get(idx + 1) == Some(&'_')
            && (idx == 0 || !chars[idx - 1].is_alphanumeric())
        {
            if let Some((close, next)) = find_emphasis_close(&chars, idx + 2, '_') {
                let word_bounded = next >= chars.len() || !chars[next].is_alphanumeric();
                if close > idx + 2 && word_bounded {
                    out.push('_');
                    push_escaped_all(&chars[idx + 2..close], out);
                    out.push('_');
                    idx = next;
                    continue;
                }
            }
            out.push_str("\\_");
            idx += 1;
            continue;
        }
        push_escaped(ch, out);
        idx += 1;
    }
}

/// Length of the run of `marker` starting at `from`.
fn run_length(chars: &[char], from: usize, marker: char) -> usize {
    chars[from..].iter().take_while(|ch| **ch == marker).count()
}

/// Index just past the next run of exactly `run` `marker` characters at or after `from`.
fn find_closing_run(chars: &[char], from: usize, marker: char, run: usize) -> Option<usize> {
    let mut idx = from;
    while idx < chars.len() {
        if chars[idx] == marker {
            let length = run_length(chars, idx, marker);
            if length == run {
                return Some(idx + length);
            }
            idx += length;
        } else {
            idx += 1;
        }
    }
    None
}

/// Find the closing `**`/`__` pair for an emphasis span opening just before `from`: the inner span's
/// start, the closer's start and the index just past the closer.
fn find_emphasis_close(chars: &[char], from: usize, marker: char) -> Option<(usize, usize)> {
    let mut idx = from;
    while idx < chars.len() {
        if chars[idx] == marker && chars.get(idx + 1) == Some(&marker) {
            return Some((idx, idx + 2));
        }
        idx += 1;
    }
    None
}

/// Push one character, escaping it when MarkdownV2 reserves it.
fn push_escaped(ch: char, out: &mut String) {
    if MARKDOWN_V2_SPECIALS.contains(&ch) {
        out.push('\\');
    }
    out.push(ch);
}

/// Escape a run of characters that is known to be plain text (no code, no formatting markers).
fn push_escaped_all(chars: &[char], out: &mut String) {
    for ch in chars {
        push_escaped(*ch, out);
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

    fn request_body(request: &HttpRequest) -> Value {
        let bytes = request.body.clone().unwrap_or_default();
        serde_json::from_slice(&bytes).unwrap_or(Value::Null)
    }

    fn request_text(request: &HttpRequest) -> String {
        request_body(request)
            .get("text")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string()
    }

    /// Poll and unwrap, failing the test with the adapter's own error text instead of a panic on a
    /// `Result` the test never inspected.
    fn polled(result: Result<Vec<ParsedUpdate>, String>) -> Vec<ParsedUpdate> {
        match result {
            Ok(updates) => updates,
            Err(error) => panic!("poll failed: {error}"),
        }
    }

    fn response(status: u16, body: Value) -> Result<HttpResponse, TransportError> {
        Ok(HttpResponse {
            status,
            body: body.to_string(),
            retry_after: None,
        })
    }

    fn api_ok(result: Value) -> Result<HttpResponse, TransportError> {
        response(200, json!({ "ok": true, "result": result }))
    }

    fn api_error(status: u16, description: &str) -> Result<HttpResponse, TransportError> {
        response(
            status,
            json!({ "ok": false, "error_code": status, "description": description }),
        )
    }

    fn sent_message(message_id: i64) -> Result<HttpResponse, TransportError> {
        api_ok(json!({
            "message_id": message_id,
            "chat": { "id": 4242, "type": "private" },
            "text": "ok",
        }))
    }

    fn adapter(mock: &Arc<MockTransport>) -> TelegramAdapter {
        TelegramAdapter::new(mock.clone(), "TEST-TOKEN")
    }

    fn private_update(update_id: i64, message: Value) -> Value {
        json!({ "update_id": update_id, "message": message })
    }

    fn non_whitespace(text: &str) -> String {
        text.chars().filter(|ch| !ch.is_whitespace()).collect()
    }

    #[tokio::test]
    async fn connect_is_true_only_for_ok_get_me_with_an_id() {
        let mock = MockTransport::new(vec![api_ok(json!({
            "id": 424242,
            "is_bot": true,
            "first_name": "Vikram",
            "username": "vikram_bot",
        }))]);
        let mut adapter = adapter(&mock);
        assert!(adapter.connect(false).await);
        let requests = mock.requests();
        assert_eq!(requests.len(), 1);
        assert_eq!(requests[0].method, HttpMethod::Get);
        assert_eq!(
            requests[0].url,
            "https://api.telegram.org/botTEST-TOKEN/getMe"
        );
    }

    #[tokio::test]
    async fn connect_rejects_failures() {
        // HTTP 401 from the Bot API.
        let unauthorized = MockTransport::new(vec![api_error(401, "Unauthorized")]);
        assert!(!adapter(&unauthorized).connect(false).await);

        // ok: true but no result.id.
        let idless = MockTransport::new(vec![api_ok(json!({ "is_bot": true }))]);
        assert!(!adapter(&idless).connect(false).await);

        // ok: false inside a 200 envelope.
        let envelope = MockTransport::new(vec![response(
            200,
            json!({ "ok": false, "description": "Unauthorized" }),
        )]);
        assert!(!adapter(&envelope).connect(false).await);

        // A transport failure, and a body that is not JSON at all.
        let down = MockTransport::new(vec![Err(TransportError::Request("dns".to_string()))]);
        assert!(!adapter(&down).connect(false).await);
        let garbage = MockTransport::new(vec![Ok(HttpResponse {
            status: 200,
            body: "<html>".to_string(),
            retry_after: None,
        })]);
        assert!(!adapter(&garbage).connect(false).await);
    }

    #[tokio::test]
    async fn send_message_body_carries_chat_id_parse_mode_and_reply_parameters() {
        let mock = MockTransport::new(vec![sent_message(11)]);
        let result = adapter(&mock).send("4242", "hello", Some("7"), None).await;

        assert!(result.success);
        assert_eq!(result.message_id.as_deref(), Some("11"));
        assert_eq!(result.error_kind, None);

        let requests = mock.requests();
        assert_eq!(requests.len(), 1);
        assert_eq!(requests[0].method, HttpMethod::Post);
        assert_eq!(
            requests[0].url,
            "https://api.telegram.org/botTEST-TOKEN/sendMessage"
        );
        let body = request_body(&requests[0]);
        assert_eq!(body["chat_id"], json!("4242"));
        assert_eq!(body["text"], json!("hello"));
        assert_eq!(body["parse_mode"], json!("MarkdownV2"));
        assert_eq!(
            body["reply_parameters"],
            json!({ "message_id": 7, "allow_sending_without_reply": true })
        );
    }

    #[tokio::test]
    async fn send_omits_reply_parameters_without_a_numeric_anchor() {
        let mock = MockTransport::new(vec![sent_message(12), sent_message(13)]);
        let adapter = adapter(&mock);
        assert!(adapter.send("4242", "no anchor", None, None).await.success);
        // An opaque anchor cannot be a Telegram reply target, so it is dropped rather than guessed.
        assert!(
            adapter
                .send("4242", "opaque", Some("abc"), None)
                .await
                .success
        );
        for body in mock.bodies() {
            assert!(body.get("reply_parameters").is_none(), "{body}");
        }
    }

    #[tokio::test]
    async fn long_messages_are_chunked_within_the_utf16_cap() {
        let content: String = (0..160)
            .map(|idx| format!("sentence {idx} with a few words to fill the message\n"))
            .collect();
        let escaped = markdown_to_telegram(&content);
        assert!(
            utf16_len(&escaped) > MAX_MESSAGE_LENGTH,
            "test content must exceed one message"
        );

        let ok_count = escaped.len() / 512 + 4;
        let mock = MockTransport::new((0..ok_count).map(|_| sent_message(100)).collect());
        let result = adapter(&mock).send("4242", &content, Some("9"), None).await;

        assert!(result.success);
        let requests = mock.requests();
        assert!(requests.len() > 1, "content should have been split");
        for request in &requests {
            let text = request_text(request);
            assert!(
                utf16_len(&text) <= MAX_MESSAGE_LENGTH,
                "chunk of {} units exceeds the cap: {text:?}",
                utf16_len(&text)
            );
            assert_eq!(request_body(request)["parse_mode"], json!("MarkdownV2"));
        }
        // Only the leading chunk replies to the anchor.
        assert_eq!(
            request_body(&requests[0])["reply_parameters"]["message_id"],
            json!(9)
        );
        for request in &requests[1..] {
            assert!(request_body(request).get("reply_parameters").is_none());
        }
        // The last chunk id is the message id, and every id is reported in send order.
        assert_eq!(result.message_id.as_deref(), Some("100"));
        assert_eq!(result.continuation_message_ids.len(), requests.len());
        // Chunking loses no visible content.
        let joined: String = requests.iter().map(request_text).collect();
        assert_eq!(non_whitespace(&joined), non_whitespace(&escaped));
    }

    #[tokio::test]
    async fn entity_parse_failure_retries_once_without_parse_mode() {
        let mock = MockTransport::new(vec![
            api_error(
                400,
                "Bad Request: can't parse entities: Can't find end of Bold entity",
            ),
            sent_message(21),
        ]);
        let result = adapter(&mock)
            .send("4242", "**unbalanced", None, None)
            .await;

        assert!(result.success);
        assert_eq!(result.message_id.as_deref(), Some("21"));
        let bodies = mock.bodies();
        assert_eq!(bodies.len(), 2);
        assert_eq!(bodies[0]["parse_mode"], json!("MarkdownV2"));
        assert!(bodies[1].get("parse_mode").is_none(), "{}", bodies[1]);
        // The retry resends the same chunk, not a re-render.
        assert_eq!(bodies[0]["text"], bodies[1]["text"]);
    }

    #[tokio::test]
    async fn non_parse_400_does_not_trigger_the_fallback() {
        let mock = MockTransport::new(vec![api_error(400, "Bad Request: message text is empty")]);
        let result = adapter(&mock).send("4242", "hi", None, None).await;
        assert!(!result.success);
        assert_eq!(result.error_kind.as_deref(), Some("bad_format"));
        assert_eq!(mock.requests().len(), 1);
    }

    #[tokio::test]
    async fn flood_wait_is_rate_limited_with_the_requested_delay() {
        let mock = MockTransport::new(vec![response(
            429,
            json!({
                "ok": false,
                "error_code": 429,
                "description": "Too Many Requests: retry after 12",
                "parameters": { "retry_after": 12 },
            }),
        )]);
        let result = adapter(&mock).send("4242", "hello", None, None).await;

        assert!(!result.success);
        assert_eq!(result.error_kind.as_deref(), Some("rate_limited"));
        assert!(result.retryable);
        assert_eq!(result.retry_after, Some(12.0));
        assert!(
            result
                .error
                .as_deref()
                .unwrap_or_default()
                .contains("Too Many Requests")
        );
    }

    #[tokio::test]
    async fn forbidden_and_transient_failures_are_classified() {
        let blocked = MockTransport::new(vec![api_error(
            403,
            "Forbidden: bot was blocked by the user",
        )]);
        let result = adapter(&blocked).send("4242", "hello", None, None).await;
        assert!(!result.success);
        assert_eq!(result.error_kind.as_deref(), Some("forbidden"));
        assert!(!result.retryable);

        let server = MockTransport::new(vec![api_error(502, "Bad Gateway")]);
        let result = adapter(&server).send("4242", "hello", None, None).await;
        assert_eq!(result.error_kind.as_deref(), Some("transient"));
        assert!(result.retryable);

        let down = MockTransport::new(vec![Err(TransportError::Request(
            "connection reset".to_string(),
        ))]);
        let result = adapter(&down).send("4242", "hello", None, None).await;
        assert_eq!(result.error_kind.as_deref(), Some("transient"));
        assert!(result.retryable);
    }

    #[tokio::test]
    async fn mid_way_failure_reports_the_ids_already_delivered() {
        let content: String = (0..400)
            .map(|idx| format!("sentence {idx} with a few words to fill the message\n"))
            .collect();
        let mock = MockTransport::new(vec![
            sent_message(31),
            sent_message(32),
            api_error(403, "Forbidden: bot was kicked from the group chat"),
        ]);
        let result = adapter(&mock).send("4242", &content, None, None).await;

        assert!(!result.success);
        assert_eq!(result.error_kind.as_deref(), Some("forbidden"));
        // The two chunks that landed are reported so a retry sends only the tail.
        assert_eq!(result.continuation_message_ids, vec!["31", "32"]);
        match result.partial_overflow() {
            Some((delivered, total)) => {
                assert_eq!(delivered, 2);
                assert!(
                    total > 2,
                    "chunks after the failure must be counted: {total}"
                );
            }
            None => panic!("failure after a delivered head must carry the partial marker"),
        }
    }

    #[tokio::test]
    async fn get_chat_info_maps_every_telegram_chat_type() {
        let cases = [
            (
                "private",
                json!({ "id": 7, "type": "private", "first_name": "Ann", "last_name": "Lee" }),
                "Ann Lee",
                "dm",
            ),
            (
                "group",
                json!({ "id": -100, "type": "group", "title": "Team" }),
                "Team",
                "group",
            ),
            (
                "supergroup",
                json!({ "id": -200, "type": "supergroup", "title": "Forum" }),
                "Forum",
                "group",
            ),
            (
                "channel",
                json!({ "id": -300, "type": "channel", "title": "News" }),
                "News",
                "channel",
            ),
        ];
        for (label, chat, expected_name, expected_type) in cases {
            let mock = MockTransport::new(vec![api_ok(chat)]);
            let info = adapter(&mock).get_chat_info("-1").await;
            assert_eq!(info.name, expected_name, "{label}");
            assert_eq!(info.chat_type, expected_type, "{label}");
            let request = &mock.requests()[0];
            assert_eq!(
                request.url,
                "https://api.telegram.org/botTEST-TOKEN/getChat"
            );
            assert_eq!(request_body(request)["chat_id"], json!("-1"));
        }
    }

    #[tokio::test]
    async fn get_chat_info_falls_back_to_username_then_chat_id() {
        let named = MockTransport::new(vec![api_ok(json!({
            "id": 7, "type": "private", "username": "vikram_bot"
        }))]);
        assert_eq!(adapter(&named).get_chat_info("7").await.name, "vikram_bot");

        let failed = MockTransport::new(vec![api_error(400, "Bad Request: chat not found")]);
        let info = adapter(&failed).get_chat_info("-100999").await;
        assert_eq!(info.name, "-100999");
        assert_eq!(info.chat_type, "dm");
    }

    #[tokio::test]
    async fn send_typing_posts_send_chat_action() {
        let mock = MockTransport::new(vec![api_ok(json!(true))]);
        adapter(&mock).send_typing("4242", None).await;
        let requests = mock.requests();
        assert_eq!(requests.len(), 1);
        assert_eq!(
            requests[0].url,
            "https://api.telegram.org/botTEST-TOKEN/sendChatAction"
        );
        let body = request_body(&requests[0]);
        assert_eq!(body["chat_id"], json!("4242"));
        assert_eq!(body["action"], json!("typing"));
    }

    #[tokio::test]
    async fn typing_failures_are_swallowed() {
        let mock = MockTransport::new(vec![Err(TransportError::Request("down".to_string()))]);
        adapter(&mock).send_typing("4242", None).await;
        assert_eq!(mock.requests().len(), 1);
    }

    #[tokio::test]
    async fn edit_and_delete_use_the_message_endpoints() {
        let mock = MockTransport::new(vec![
            api_ok(json!({ "message_id": 55, "text": "fixed!" })),
            api_ok(json!(true)),
        ]);
        let adapter = adapter(&mock);

        let edited = adapter.edit_message("4242", "55", "fixed!", true).await;
        assert!(edited.success);
        assert_eq!(edited.message_id.as_deref(), Some("55"));

        adapter.delete_message("4242", "55").await;

        let requests = mock.requests();
        assert_eq!(requests.len(), 2);
        assert_eq!(
            requests[0].url,
            "https://api.telegram.org/botTEST-TOKEN/editMessageText"
        );
        let body = request_body(&requests[0]);
        assert_eq!(body["chat_id"], json!("4242"));
        assert_eq!(body["message_id"], json!(55));
        assert_eq!(body["text"], json!("fixed\\!"));
        assert_eq!(body["parse_mode"], json!("MarkdownV2"));

        assert_eq!(
            requests[1].url,
            "https://api.telegram.org/botTEST-TOKEN/deleteMessage"
        );
        let body = request_body(&requests[1]);
        assert_eq!(body["message_id"], json!(55));
    }

    #[tokio::test]
    async fn edit_reports_not_modified_as_success_and_failures_as_sent_results() {
        let unchanged = MockTransport::new(vec![api_error(
            400,
            "Bad Request: message is not modified: specified new message content and reply markup are exactly the same",
        )]);
        let result = adapter(&unchanged)
            .edit_message("4242", "55", "same", false)
            .await;
        assert!(result.success);
        assert_eq!(result.message_id.as_deref(), Some("55"));

        let too_old =
            MockTransport::new(vec![api_error(400, "Bad Request: message can't be edited")]);
        let result = adapter(&too_old)
            .edit_message("4242", "55", "nope", false)
            .await;
        assert!(!result.success);
        assert_eq!(result.error_kind.as_deref(), Some("bad_format"));
    }

    #[tokio::test]
    async fn poll_once_parses_a_text_update() {
        let mock = MockTransport::new(vec![api_ok(json!([private_update(
            700,
            json!({
                "message_id": 11,
                "date": 1_700_000_000,
                "chat": { "id": 4242, "type": "private", "first_name": "Ann", "last_name": "Lee" },
                "from": { "id": 4242, "is_bot": false, "first_name": "Ann", "last_name": "Lee" },
                "text": "hello there",
            }),
        )]))]);
        let updates = polled(adapter(&mock).poll_once(Some(699), 25).await);

        assert_eq!(updates.len(), 1);
        let parsed = &updates[0];
        assert_eq!(parsed.update_id, 700);
        assert_eq!(parsed.event.text, "hello there");
        assert_eq!(parsed.event.message_type, MessageType::Text);
        assert_eq!(parsed.event.message_id.as_deref(), Some("11"));
        assert_eq!(parsed.event.platform_update_id, Some(700));
        assert_eq!(parsed.event.user_id.as_deref(), Some("4242"));
        assert_eq!(parsed.event.user_name.as_deref(), Some("Ann Lee"));
        assert_eq!(parsed.event.edit_revision, None);
        assert_eq!(parsed.event.source.platform.as_str(), "telegram");
        assert_eq!(parsed.event.source.chat_id, "4242");
        assert_eq!(parsed.event.source.chat_type, "dm");
        assert_eq!(parsed.event.source.chat_name.as_deref(), Some("Ann Lee"));
        assert!(parsed.event.raw_message.is_some());
        assert!(parsed.event.media_urls.is_empty());

        let request = &mock.requests()[0];
        assert_eq!(request.method, HttpMethod::Get);
        assert!(request.url.starts_with(
            "https://api.telegram.org/botTEST-TOKEN/getUpdates?offset=699&timeout=25&allowed_updates="
        ));
        // `offset`, `timeout` and the JSON-array `allowed_updates` ride as query parameters.
        assert_eq!(
            request.url,
            "https://api.telegram.org/botTEST-TOKEN/getUpdates?offset=699&timeout=25&allowed_updates=%5B%22message%22%2C%22edited_message%22%5D"
        );
    }

    #[tokio::test]
    async fn poll_once_parses_a_photo_update() {
        let mock = MockTransport::new(vec![api_ok(json!([private_update(
            701,
            json!({
                "message_id": 12,
                "date": 1_700_000_100,
                "chat": { "id": -100123, "type": "supergroup", "title": "Team" },
                "from": { "id": 9, "is_bot": false, "first_name": "Bob" },
                "caption": "look at this",
                "photo": [
                    { "file_id": "small", "file_unique_id": "a", "width": 90, "height": 90 },
                    { "file_id": "large", "file_unique_id": "b", "width": 1280, "height": 1280 },
                ],
            }),
        )]))]);
        let updates = polled(adapter(&mock).poll_once(None, 0).await);

        assert_eq!(updates.len(), 1);
        let event = &updates[0].event;
        assert_eq!(event.message_type, MessageType::Photo);
        assert_eq!(event.text, "look at this");
        assert_eq!(event.media_urls, vec!["large"]);
        assert_eq!(event.media_types, vec!["image/jpeg"]);
        assert_eq!(event.media_filenames, vec![String::new()]);
        assert_eq!(event.source.chat_type, "group");
        assert_eq!(event.source.chat_name.as_deref(), Some("Team"));
        // No offset means no offset parameter.
        assert!(!mock.requests()[0].url.contains("offset="));
    }

    #[tokio::test]
    async fn poll_once_maps_document_voice_and_sticker_media() {
        let mock = MockTransport::new(vec![api_ok(json!([
            private_update(
                702,
                json!({
                    "message_id": 13,
                    "chat": { "id": 4242, "type": "private" },
                    "from": { "id": 4242, "is_bot": false, "first_name": "Ann" },
                    "document": {
                        "file_id": "doc-1",
                        "file_name": "notes.pdf",
                        "mime_type": "application/pdf",
                    },
                }),
            ),
            private_update(
                703,
                json!({
                    "message_id": 14,
                    "chat": { "id": 4242, "type": "private" },
                    "from": { "id": 4242, "is_bot": false, "first_name": "Ann" },
                    "voice": { "file_id": "voice-1", "duration": 3 },
                }),
            ),
            private_update(
                704,
                json!({
                    "message_id": 15,
                    "chat": { "id": 4242, "type": "private" },
                    "from": { "id": 4242, "is_bot": false, "first_name": "Ann" },
                    "sticker": { "file_id": "sticker-1" },
                }),
            ),
        ]))]);
        let updates = polled(adapter(&mock).poll_once(None, 0).await);

        assert_eq!(updates.len(), 3);
        assert_eq!(updates[0].event.message_type, MessageType::Document);
        assert_eq!(updates[0].event.media_urls, vec!["doc-1"]);
        assert_eq!(updates[0].event.media_types, vec!["application/pdf"]);
        assert_eq!(updates[0].event.media_filenames, vec!["notes.pdf"]);
        assert_eq!(updates[1].event.message_type, MessageType::Voice);
        assert_eq!(updates[1].event.media_types, vec!["audio/ogg"]);
        assert_eq!(updates[2].event.message_type, MessageType::Sticker);
        assert_eq!(updates[2].event.media_types, vec!["image/webp"]);
    }

    #[tokio::test]
    async fn poll_once_marks_edited_updates_with_a_revision() {
        let mock = MockTransport::new(vec![api_ok(json!([json!({
            "update_id": 705,
            "edited_message": {
                "message_id": 16,
                "edit_date": 1_700_000_200,
                "chat": { "id": 4242, "type": "private" },
                "from": { "id": 4242, "is_bot": false, "first_name": "Ann" },
                "text": "corrected",
            },
        })]))]);
        let updates = polled(adapter(&mock).poll_once(None, 0).await);

        assert_eq!(updates.len(), 1);
        assert_eq!(updates[0].event.text, "corrected");
        assert_eq!(updates[0].event.edit_revision.as_deref(), Some("u705"));
    }

    #[tokio::test]
    async fn poll_once_carries_reply_context_and_skips_unknown_updates() {
        let mock = MockTransport::new(vec![api_ok(json!([
            { "update_id": 706, "callback_query": { "id": "cb" } },
            private_update(
                707,
                json!({
                    "message_id": 17,
                    "chat": { "id": -500, "type": "group", "title": "Team" },
                    "from": { "id": 3, "is_bot": false, "first_name": "Cy" },
                    "text": "yes",
                    "reply_to_message": { "message_id": 16, "text": "did you mean it?" },
                }),
            ),
            { "update_id": 708 },
        ]))]);
        let updates = polled(adapter(&mock).poll_once(None, 0).await);

        assert_eq!(updates.len(), 1);
        let event = &updates[0].event;
        assert_eq!(event.reply_to_message_id.as_deref(), Some("16"));
        assert_eq!(event.reply_to_text.as_deref(), Some("did you mean it?"));
        assert_eq!(event.source.chat_type, "group");
        // No user_name fallback from the chat title in a group.
        assert_eq!(event.user_name.as_deref(), Some("Cy"));
    }

    #[tokio::test]
    async fn poll_once_reports_api_and_transport_failures() {
        let rejected = MockTransport::new(vec![api_error(401, "Unauthorized")]);
        let error = adapter(&rejected)
            .poll_once(None, 10)
            .await
            .expect_err("401 must fail");
        assert!(error.contains("401"), "{error}");

        let down = MockTransport::new(vec![Err(TransportError::Request("timeout".to_string()))]);
        let error = adapter(&down)
            .poll_once(None, 10)
            .await
            .expect_err("transport error must fail");
        assert!(error.contains("timeout"), "{error}");

        let garbage = MockTransport::new(vec![Ok(HttpResponse {
            status: 200,
            body: "not json".to_string(),
            retry_after: None,
        })]);
        let error = adapter(&garbage)
            .poll_once(None, 10)
            .await
            .expect_err("invalid json must fail");
        assert!(error.contains("invalid json"), "{error}");
    }

    #[tokio::test]
    async fn empty_content_is_a_successful_no_op() {
        let mock = MockTransport::new(Vec::new());
        let result = adapter(&mock).send("4242", "   \n", None, None).await;
        assert!(result.success);
        assert!(result.message_id.is_none());
        assert!(mock.requests().is_empty());
    }

    #[test]
    fn markdown_to_telegram_converts_bold_and_italic() {
        assert_eq!(markdown_to_telegram("**bold**"), "*bold*");
        assert_eq!(markdown_to_telegram("__italic__"), "_italic_");
        assert_eq!(
            markdown_to_telegram("plain prose"),
            "plain prose",
            "nothing to escape"
        );
        assert_eq!(
            markdown_to_telegram("**bold** and __italic__"),
            "*bold* and _italic_"
        );
        // Specials outside a construct are escaped.
        assert_eq!(markdown_to_telegram("a.b!"), "a\\.b\\!");
        // Inline code is preserved verbatim, including would-be specials.
        assert_eq!(markdown_to_telegram("run `a.b!` now"), "run `a.b!` now");
        // An unclosed `**` degrades to a literal instead of a broken entity.
        assert_eq!(markdown_to_telegram("**oops"), "\\*\\*oops");
        // `snake__case` is not an italic span.
        assert_eq!(
            markdown_to_telegram("snake__case__x"),
            "snake\\_\\_case\\_\\_x"
        );
    }

    #[test]
    fn markdown_to_telegram_keeps_fenced_blocks_intact() {
        let text = "```rust\nlet a = 1.0; // **not bold**\n```\n**hi**";
        assert_eq!(
            markdown_to_telegram(text),
            "```rust\nlet a = 1.0; // **not bold**\n```\n*hi*"
        );
        // Unterminated fence swallows the rest verbatim rather than escaping it.
        assert_eq!(markdown_to_telegram("```\na.b"), "```\na.b");
    }

    #[test]
    fn escape_markdown_v2_escapes_prose_and_skips_code() {
        assert_eq!(escape_markdown_v2("a_b"), "a\\_b");
        assert_eq!(
            escape_markdown_v2("1.2-3+4=5|6{7}~8>9#"),
            "1\\.2\\-3\\+4\\=5\\|6\\{7\\}\\~8\\>9\\#"
        );
        assert_eq!(escape_markdown_v2("(x) [y]!"), "\\(x\\) \\[y\\]\\!");
        // Inline code and fenced blocks keep their characters.
        assert_eq!(escape_markdown_v2("`a.b!`"), "`a.b!`");
        assert_eq!(escape_markdown_v2("```\na.b!\n```"), "```\na.b!\n```");
        assert_eq!(escape_markdown_v2("a.b `c.d` e.f"), "a\\.b `c.d` e\\.f");
        // An unmatched backtick cannot swallow the rest of the line.
        assert_eq!(escape_markdown_v2("`a.b"), "\\`a\\.b");
        assert_eq!(escape_markdown_v2(""), "");
    }

    #[test]
    fn telegram_helpers_map_ids_and_chat_types() {
        assert_eq!(chat_type_from_telegram("private"), "dm");
        assert_eq!(chat_type_from_telegram("group"), "group");
        assert_eq!(chat_type_from_telegram("supergroup"), "group");
        assert_eq!(chat_type_from_telegram("channel"), "channel");
        assert_eq!(chat_type_from_telegram("something_new"), "dm");
        assert_eq!(value_to_id(&json!(42)).as_deref(), Some("42"));
        assert_eq!(value_to_id(&json!("-100")).as_deref(), Some("-100"));
        assert_eq!(value_to_id(&json!("  ")), None);
        assert_eq!(value_to_id(&json!(null)), None);
        assert_eq!(message_id_value("55"), json!(55));
        assert_eq!(message_id_value("abc"), json!("abc"));
        assert_eq!(encode_query_value("[\"a\"]"), "%5B%22a%22%5D");
        assert_eq!(shortened("abcdef", 3), "abc…");
        assert_eq!(shortened("ab", 3), "ab");
    }

    // ----- outbound media fixtures ------------------------------------------------------------

    /// The boundary token a multipart request declared in its `Content-Type` header.
    fn multipart_boundary_of(request: &HttpRequest) -> String {
        let header = request
            .headers
            .iter()
            .find(|(name, _)| name.eq_ignore_ascii_case("Content-Type"))
            .map(|(_, value)| value.clone())
            .unwrap_or_default();
        match header.strip_prefix("multipart/form-data; boundary=") {
            Some(boundary) if !boundary.is_empty() => boundary.to_string(),
            _ => panic!("request is not multipart/form-data: {header:?}"),
        }
    }

    /// One parsed `multipart/form-data` part.
    struct MultipartPart {
        name: String,
        filename: Option<String>,
        content_type: Option<String>,
        value: Vec<u8>,
    }

    /// Split `body` on every occurrence of `needle`, keeping the segments between matches.
    fn split_on<'a>(body: &'a [u8], needle: &[u8]) -> Vec<&'a [u8]> {
        assert!(!needle.is_empty(), "an empty delimiter cannot split a body");
        let mut segments = Vec::new();
        let mut start = 0;
        let mut cursor = 0;
        while cursor + needle.len() <= body.len() {
            if &body[cursor..cursor + needle.len()] == needle {
                segments.push(&body[start..cursor]);
                cursor += needle.len();
                start = cursor;
            } else {
                cursor += 1;
            }
        }
        segments.push(&body[start..]);
        segments
    }

    /// Offset of the first occurrence of `needle` in `haystack`.
    fn find_bytes(haystack: &[u8], needle: &[u8]) -> Option<usize> {
        if needle.is_empty() || needle.len() > haystack.len() {
            return None;
        }
        (0..=haystack.len() - needle.len())
            .find(|start| &haystack[*start..*start + needle.len()] == needle)
    }

    /// Drop the CRLF that frames one part segment off a multipart body.
    fn trim_crlf(segment: &[u8]) -> &[u8] {
        let start = if segment.starts_with(b"\r\n") { 2 } else { 0 };
        let end = if segment.len() >= start + 2 && segment.ends_with(b"\r\n") {
            segment.len() - 2
        } else {
            segment.len()
        };
        &segment[start..end]
    }

    /// The value of `attribute="…"` inside a `Content-Disposition` line.
    fn disposition_attribute(line: &str, attribute: &str) -> Option<String> {
        let marker = format!("{attribute}=\"");
        let start = line.find(&marker)? + marker.len();
        let rest = line.get(start..)?;
        let end = rest.find('"')?;
        rest.get(..end).map(str::to_string)
    }

    /// Parse a multipart body back into its parts, asserting CRLF-only framing on the way.
    fn multipart_parts(request: &HttpRequest) -> Vec<MultipartPart> {
        let boundary = multipart_boundary_of(request);
        let body = request.body.clone().unwrap_or_default();
        for (index, byte) in body.iter().enumerate() {
            if *byte == b'\n' {
                assert!(
                    index > 0 && body[index - 1] == b'\r',
                    "bare LF at byte {index}: multipart framing must be CRLF"
                );
            }
        }
        let delimiter = format!("--{boundary}");
        let segments = split_on(&body, delimiter.as_bytes());
        assert_eq!(
            segments.first().map(|segment| segment.len()),
            Some(0),
            "nothing may precede the first boundary"
        );
        assert_eq!(
            segments.last().copied(),
            Some(b"--\r\n".as_slice()),
            "the body must close with --boundary--CRLF"
        );
        let mut parts = Vec::new();
        let last = segments.len().saturating_sub(2);
        for segment in segments.iter().skip(1).take(last) {
            let segment = trim_crlf(segment);
            let split = match find_bytes(segment, b"\r\n\r\n") {
                Some(split) => split,
                None => panic!("multipart part without a header terminator"),
            };
            let head = String::from_utf8_lossy(&segment[..split]).to_string();
            let mut name = None;
            let mut filename = None;
            let mut content_type = None;
            for (line_index, line) in head.split("\r\n").enumerate() {
                if line_index == 0 {
                    assert!(
                        line.starts_with("Content-Disposition: form-data;"),
                        "unexpected part header: {line}"
                    );
                    name = disposition_attribute(line, "name");
                    filename = disposition_attribute(line, "filename");
                } else if let Some(value) = line.strip_prefix("Content-Type: ") {
                    content_type = Some(value.to_string());
                }
            }
            parts.push(MultipartPart {
                name: name.unwrap_or_default(),
                filename,
                content_type,
                value: segment[split + 4..].to_vec(),
            });
        }
        parts
    }

    /// The part called `name`, if the adapter wrote one.
    fn part<'a>(parts: &'a [MultipartPart], name: &str) -> Option<&'a MultipartPart> {
        parts.iter().find(|part| part.name == name)
    }

    /// The part called `name`, failing the test when the adapter omitted it.
    fn required_part<'a>(parts: &'a [MultipartPart], name: &str) -> &'a MultipartPart {
        match part(parts, name) {
            Some(part) => part,
            None => panic!(
                "missing multipart part {name:?}; got {:?}",
                parts.iter().map(|part| &part.name).collect::<Vec<_>>()
            ),
        }
    }

    /// A text part's value as a string.
    fn part_value(parts: &[MultipartPart], name: &str) -> Option<String> {
        part(parts, name).map(|part| String::from_utf8_lossy(&part.value).to_string())
    }

    /// Decode one `%XX` escape in a form value.
    fn percent_decode(value: &str) -> String {
        let bytes = value.as_bytes();
        let mut out: Vec<u8> = Vec::with_capacity(bytes.len());
        let mut index = 0;
        while index < bytes.len() {
            if bytes[index] == b'%'
                && let Some(hex) = value.get(index + 1..index + 3)
                && let Ok(byte) = u8::from_str_radix(hex, 16)
            {
                out.push(byte);
                index += 3;
                continue;
            }
            out.push(bytes[index]);
            index += 1;
        }
        String::from_utf8_lossy(&out).to_string()
    }

    /// Decode an `application/x-www-form-urlencoded` body into its pairs.
    fn form_pairs(request: &HttpRequest) -> Vec<(String, String)> {
        let body = String::from_utf8_lossy(&request.body.clone().unwrap_or_default()).to_string();
        body.split('&')
            .filter(|pair| !pair.is_empty())
            .map(|pair| {
                let mut split = pair.splitn(2, '=');
                let key = split.next().unwrap_or_default();
                let value = split.next().unwrap_or_default();
                (percent_decode(key), percent_decode(value))
            })
            .collect()
    }

    /// A form value by name.
    fn form_value(pairs: &[(String, String)], key: &str) -> Option<String> {
        pairs
            .iter()
            .find(|(name, _)| name == key)
            .map(|(_, value)| value.clone())
    }

    #[test]
    fn telegram_supports_media() {
        let mock = MockTransport::new(Vec::new());
        assert!(adapter(&mock).supports_media());
    }

    #[tokio::test]
    async fn media_url_uses_the_bot_api_method_for_each_kind() {
        let cases = [
            (MediaKind::Image, "sendPhoto", "photo"),
            (MediaKind::Video, "sendVideo", "video"),
            (MediaKind::Audio, "sendAudio", "audio"),
            (MediaKind::Document, "sendDocument", "document"),
        ];
        for (kind, method, field) in cases {
            let mock = MockTransport::new(vec![sent_message(90)]);
            let media = OutboundMedia::new(
                kind,
                MediaSource::Url("https://example.test/a.bin".to_string()),
            );
            let result = adapter(&mock).send_media("-100500", &media, None).await;

            assert!(
                result.success,
                "{method}: {}",
                result.error.unwrap_or_default()
            );
            assert_eq!(result.message_id.as_deref(), Some("90"), "{method}");
            let requests = mock.requests();
            assert_eq!(requests.len(), 1, "{method}");
            assert_eq!(requests[0].method, HttpMethod::Post, "{method}");
            assert_eq!(
                requests[0].url,
                format!("https://api.telegram.org/botTEST-TOKEN/{method}")
            );
            let pairs = form_pairs(&requests[0]);
            assert_eq!(form_value(&pairs, "chat_id").as_deref(), Some("-100500"));
            assert_eq!(
                form_value(&pairs, field).as_deref(),
                Some("https://example.test/a.bin"),
                "{method}"
            );
        }
    }

    #[tokio::test]
    async fn bytes_upload_is_multipart_form_data_with_crlf_framing() {
        let mock = MockTransport::new(vec![sent_message(77)]);
        let media = OutboundMedia::new(
            MediaKind::Image,
            MediaSource::Bytes(b"\x00\x01binary-payload".to_vec()),
        );
        let result = adapter(&mock).send_media("4242", &media, None).await;

        assert!(result.success, "{}", result.error.unwrap_or_default());
        assert_eq!(result.message_id.as_deref(), Some("77"));

        let requests = mock.requests();
        assert_eq!(requests.len(), 1);
        assert_eq!(requests[0].method, HttpMethod::Post);
        assert_eq!(
            requests[0].url,
            "https://api.telegram.org/botTEST-TOKEN/sendPhoto"
        );
        let boundary = multipart_boundary_of(&requests[0]);
        assert!(
            requests[0].headers.contains(&(
                "Content-Type".to_string(),
                format!("multipart/form-data; boundary={boundary}")
            )),
            "the declared boundary must match the framed body"
        );

        let body = requests[0].body.clone().unwrap_or_default();
        let framed = String::from_utf8_lossy(&body).to_string();
        assert!(
            framed.starts_with(&format!("--{boundary}\r\n")),
            "the body must open with the boundary delimiter"
        );
        assert!(
            framed.ends_with(&format!("\r\n--{boundary}--\r\n")),
            "the body must close with the terminator and a CRLF"
        );

        // `multipart_parts` also asserts every LF is part of a CRLF pair and that the framing ends
        // with `--boundary--CRLF`.
        let parts = multipart_parts(&requests[0]);
        let names: Vec<&str> = parts.iter().map(|part| part.name.as_str()).collect();
        assert_eq!(names, vec!["chat_id", "photo"]);
        assert_eq!(part_value(&parts, "chat_id").as_deref(), Some("4242"));
        let photo = required_part(&parts, "photo");
        assert_eq!(photo.filename.as_deref(), Some("photo.jpg"));
        assert_eq!(photo.content_type.as_deref(), Some("image/jpeg"));
        assert_eq!(photo.value, b"\x00\x01binary-payload");
    }

    #[tokio::test]
    async fn bytes_upload_carries_filename_mime_caption_and_reply() {
        let mock = MockTransport::new(vec![sent_message(78)]);
        let media = OutboundMedia::new(
            MediaKind::Document,
            MediaSource::Bytes(b"%PDF-1.7".to_vec()),
        )
        .with_filename("quarterly report.pdf")
        .with_mime("application/pdf")
        .with_caption("here it is");
        let result = adapter(&mock)
            .send_media("-100500", &media, Some("17"))
            .await;

        assert!(result.success, "{}", result.error.unwrap_or_default());
        let requests = mock.requests();
        assert_eq!(
            requests[0].url,
            "https://api.telegram.org/botTEST-TOKEN/sendDocument"
        );
        let parts = multipart_parts(&requests[0]);
        let document = required_part(&parts, "document");
        assert_eq!(document.filename.as_deref(), Some("quarterly report.pdf"));
        assert_eq!(document.content_type.as_deref(), Some("application/pdf"));
        assert_eq!(document.value, b"%PDF-1.7");
        assert_eq!(part_value(&parts, "chat_id").as_deref(), Some("-100500"));
        assert_eq!(part_value(&parts, "caption").as_deref(), Some("here it is"));
        assert_eq!(
            part_value(&parts, "reply_to_message_id").as_deref(),
            Some("17")
        );
    }

    #[tokio::test]
    async fn bytes_uploads_default_the_filename_and_content_type_per_kind() {
        let cases = [
            (
                MediaKind::Image,
                "sendPhoto",
                "photo",
                "photo.jpg",
                "image/jpeg",
            ),
            (
                MediaKind::Video,
                "sendVideo",
                "video",
                "video.mp4",
                "video/mp4",
            ),
            (
                MediaKind::Audio,
                "sendAudio",
                "audio",
                "audio.mp3",
                "audio/mpeg",
            ),
            (
                MediaKind::Document,
                "sendDocument",
                "document",
                "document.bin",
                "application/octet-stream",
            ),
        ];
        for (kind, method, field, filename, mime) in cases {
            let mock = MockTransport::new(vec![sent_message(79)]);
            let media = OutboundMedia::new(kind, MediaSource::Bytes(b"payload".to_vec()));
            let result = adapter(&mock).send_media("4242", &media, None).await;
            assert!(
                result.success,
                "{method}: {}",
                result.error.unwrap_or_default()
            );
            let requests = mock.requests();
            assert_eq!(
                requests[0].url,
                format!("https://api.telegram.org/botTEST-TOKEN/{method}")
            );
            let parts = multipart_parts(&requests[0]);
            let file = required_part(&parts, field);
            assert_eq!(file.filename.as_deref(), Some(filename), "{method}");
            assert_eq!(file.content_type.as_deref(), Some(mime), "{method}");
            assert_eq!(file.value, b"payload", "{method}");
        }
    }

    #[tokio::test]
    async fn filenames_cannot_break_out_of_the_header_and_captions_are_capped() {
        let mock = MockTransport::new(vec![sent_message(80)]);
        let media = OutboundMedia::new(MediaKind::Document, MediaSource::Bytes(b"x".to_vec()))
            .with_filename("../../etc/evil\r\nX-Injected: 1.png")
            .with_mime("image/png\r\nX-Injected: 1")
            .with_caption("c".repeat(MAX_MEDIA_CAPTION_CHARS + 5));
        assert!(
            adapter(&mock)
                .send_media("4242", &media, None)
                .await
                .success
        );

        let parts = multipart_parts(&mock.requests()[0]);
        let document = required_part(&parts, "document");
        // Path components and CRLF are gone, so the header stays one line and one filename.
        assert_eq!(document.filename.as_deref(), Some("evilX-Injected: 1.png"));
        assert_eq!(
            document.content_type.as_deref(),
            Some("image/pngX-Injected: 1")
        );
        let caption = part_value(&parts, "caption").unwrap_or_default();
        assert_eq!(caption.chars().count(), MAX_MEDIA_CAPTION_CHARS);
    }

    #[tokio::test]
    async fn url_media_is_passed_verbatim_without_a_download() {
        let url = "https://example.test/a.png?w=1&h=2#frag";
        let mock = MockTransport::new(vec![sent_message(81)]);
        assert!(
            adapter(&mock)
                .send_media("4242", &OutboundMedia::image_url(url), None)
                .await
                .success
        );

        let requests = mock.requests();
        // Exactly one request: the adapter never fetches the URL itself.
        assert_eq!(requests.len(), 1);
        assert_eq!(
            requests[0].url,
            "https://api.telegram.org/botTEST-TOKEN/sendPhoto"
        );
        let pairs = form_pairs(&requests[0]);
        assert_eq!(form_value(&pairs, "photo").as_deref(), Some(url));
    }

    #[tokio::test]
    async fn caption_and_reply_are_omitted_when_absent() {
        let mock = MockTransport::new(vec![sent_message(82), sent_message(83), sent_message(84)]);
        let adapter = adapter(&mock);

        let url_media = OutboundMedia::image_url("https://example.test/a.png");
        assert!(adapter.send_media("4242", &url_media, None).await.success);
        let bytes_media = OutboundMedia::new(MediaKind::Image, MediaSource::Bytes(b"p".to_vec()));
        assert!(adapter.send_media("4242", &bytes_media, None).await.success);
        // An opaque anchor is not a Telegram message id, so it is dropped rather than sent to a
        // rejection that would lose the media (`_reply_to_message_id_for_send`).
        assert!(
            adapter
                .send_media("4242", &url_media, Some("abc"))
                .await
                .success
        );

        let requests = mock.requests();
        let pairs = form_pairs(&requests[0]);
        assert!(form_value(&pairs, "caption").is_none());
        assert!(form_value(&pairs, "reply_to_message_id").is_none());

        let parts = multipart_parts(&requests[1]);
        assert!(part(&parts, "caption").is_none());
        assert!(part(&parts, "reply_to_message_id").is_none());

        let pairs = form_pairs(&requests[2]);
        assert!(form_value(&pairs, "reply_to_message_id").is_none());
    }

    #[tokio::test]
    async fn media_flood_wait_reports_rate_limited_with_retry_after() {
        let mock = MockTransport::new(vec![response(
            429,
            json!({
                "ok": false,
                "error_code": 429,
                "description": "Too Many Requests: retry after 9",
                "parameters": { "retry_after": 9 },
            }),
        )]);
        let result = adapter(&mock)
            .send_media(
                "4242",
                &OutboundMedia::image_url("https://example.test/a.png"),
                None,
            )
            .await;

        assert!(!result.success);
        assert_eq!(result.error_kind.as_deref(), Some("rate_limited"));
        assert!(result.retryable);
        assert_eq!(result.retry_after, Some(9.0));
        assert!(
            result
                .error
                .as_deref()
                .unwrap_or_default()
                .contains("Too Many Requests")
        );
    }

    #[tokio::test]
    async fn media_bad_request_is_bad_format() {
        let mock = MockTransport::new(vec![api_error(
            400,
            "Bad Request: PHOTO_INVALID_DIMENSIONS",
        )]);
        let media = OutboundMedia::new(MediaKind::Image, MediaSource::Bytes(b"pixels".to_vec()));
        let result = adapter(&mock).send_media("4242", &media, None).await;

        assert!(!result.success);
        assert_eq!(result.error_kind.as_deref(), Some("bad_format"));
        assert!(!result.retryable);
        assert!(
            result
                .error
                .as_deref()
                .unwrap_or_default()
                .contains("PHOTO_INVALID_DIMENSIONS")
        );
    }

    #[tokio::test]
    async fn media_forbidden_is_forbidden() {
        let mock = MockTransport::new(vec![api_error(
            403,
            "Forbidden: bot was blocked by the user",
        )]);
        let result = adapter(&mock)
            .send_media(
                "4242",
                &OutboundMedia::image_url("https://example.test/a.png"),
                None,
            )
            .await;

        assert!(!result.success);
        assert_eq!(result.error_kind.as_deref(), Some("forbidden"));
        assert!(!result.retryable);
        assert!(result.message_id.is_none());
    }

    #[tokio::test]
    async fn media_transport_failure_is_a_retryable_transient_error() {
        let mock = MockTransport::new(vec![Err(TransportError::Request(
            "connection reset".to_string(),
        ))]);
        let media = OutboundMedia::new(MediaKind::Video, MediaSource::Bytes(b"frames".to_vec()));
        let result = adapter(&mock).send_media("4242", &media, None).await;

        assert!(!result.success);
        assert_eq!(result.error_kind.as_deref(), Some("transient"));
        assert!(result.retryable);
        assert!(
            result
                .error
                .as_deref()
                .unwrap_or_default()
                .contains("connection reset")
        );
    }

    #[tokio::test]
    async fn empty_bytes_and_oversized_urls_are_refused_without_a_request() {
        let mock = MockTransport::new(Vec::new());
        let adapter = adapter(&mock);

        let empty = OutboundMedia::new(MediaKind::Image, MediaSource::Bytes(Vec::new()));
        let result = adapter.send_media("4242", &empty, None).await;
        assert!(!result.success);
        assert_eq!(result.error_kind.as_deref(), Some("bad_format"));
        assert!(
            result
                .error
                .as_deref()
                .unwrap_or_default()
                .contains("empty")
        );

        let oversized_url = format!("https://example.test/{}", "a".repeat(MAX_MEDIA_URL_LEN + 1));
        let oversized = OutboundMedia::image_url(&oversized_url);
        let result = adapter.send_media("4242", &oversized, None).await;
        assert!(!result.success);
        assert_eq!(result.error_kind.as_deref(), Some("bad_format"));
        assert!(
            result
                .error
                .as_deref()
                .unwrap_or_default()
                .contains(&MAX_MEDIA_URL_LEN.to_string())
        );

        // Neither refusal touched the wire.
        assert!(mock.requests().is_empty());
    }

    #[tokio::test]
    async fn a_url_at_the_bot_api_limit_is_still_accepted() {
        let prefix = "https://example.test/";
        let url = format!("{prefix}{}", "a".repeat(MAX_MEDIA_URL_LEN - prefix.len()));
        assert_eq!(url.chars().count(), MAX_MEDIA_URL_LEN);

        let mock = MockTransport::new(vec![sent_message(85)]);
        assert!(
            adapter(&mock)
                .send_media("4242", &OutboundMedia::image_url(&url), None)
                .await
                .success
        );
        let pairs = form_pairs(&mock.requests()[0]);
        assert_eq!(form_value(&pairs, "photo").as_deref(), Some(url.as_str()));
    }

    #[tokio::test]
    async fn ok_false_responses_surface_the_description_and_never_report_delivery() {
        let mock = MockTransport::new(vec![response(
            200,
            json!({
                "ok": false,
                "error_code": 400,
                "description": "Bad Request: file must be non-empty",
            }),
        )]);
        let media = OutboundMedia::new(MediaKind::Document, MediaSource::Bytes(b"x".to_vec()));
        let result = adapter(&mock).send_media("4242", &media, None).await;

        assert!(!result.success, "a refused upload must not report delivery");
        assert!(result.message_id.is_none());
        assert_eq!(
            result.error.as_deref(),
            Some("Bad Request: file must be non-empty")
        );
        // A 200 envelope carrying `ok: false` is a request-format rejection.
        assert_eq!(result.error_kind.as_deref(), Some("bad_format"));
    }

    #[tokio::test]
    async fn a_result_without_a_message_id_is_not_reported_as_delivered() {
        let mock = MockTransport::new(vec![api_ok(json!({ "photo": [] }))]);
        let result = adapter(&mock)
            .send_media(
                "4242",
                &OutboundMedia::image_url("https://example.test/a.png"),
                None,
            )
            .await;

        assert!(!result.success);
        assert!(result.message_id.is_none());
        assert_eq!(result.error_kind.as_deref(), Some("unknown"));
    }

    // ----- reactions ---------------------------------------------------------------------------

    /// The raw request body as text, so an assertion can see the emoji's UTF-8 bytes.
    fn request_text_raw(request: &HttpRequest) -> String {
        String::from_utf8_lossy(request.body.as_deref().unwrap_or_default()).to_string()
    }

    #[test]
    fn telegram_supports_reactions() {
        let mock = MockTransport::new(Vec::new());
        assert!(adapter(&mock).supports_reactions());
    }

    #[tokio::test]
    async fn add_reaction_posts_set_message_reaction_with_an_emoji_reaction_type() {
        let mock = MockTransport::new(vec![api_ok(json!(true))]);
        let reacted = adapter(&mock)
            .add_reaction("-1001234567890", "11", "👍")
            .await;
        assert!(reacted, "the documented result: true must report success");

        let requests = mock.requests();
        assert_eq!(requests.len(), 1);
        assert_eq!(requests[0].method, HttpMethod::Post);
        assert_eq!(
            requests[0].url,
            "https://api.telegram.org/botTEST-TOKEN/setMessageReaction"
        );
        let body = request_body(&requests[0]);
        assert_eq!(body["chat_id"], json!("-1001234567890"));
        assert_eq!(
            body["message_id"],
            json!(11),
            "the Bot API takes an integer"
        );
        assert_eq!(
            body["reaction"],
            json!([{ "type": "emoji", "emoji": "👍" }])
        );

        // The emoji travels as UTF-8 in the JSON body, never as a `\u` escape.
        let raw = request_text_raw(&requests[0]);
        assert!(raw.contains('👍'), "{raw}");
        assert!(!raw.contains("\\u"), "{raw}");
    }

    #[tokio::test]
    async fn add_reaction_accepts_a_username_chat_target_and_a_padded_emoji() {
        let mock = MockTransport::new(vec![api_ok(json!(true))]);
        assert!(
            adapter(&mock)
                .add_reaction(" @vikram_news ", " 12 ", " 🎉 ")
                .await
        );
        let body = request_body(&mock.requests()[0]);
        // `normalize_telegram_chat_id` keeps an @username as a stripped string.
        assert_eq!(body["chat_id"], json!("@vikram_news"));
        assert_eq!(body["message_id"], json!(12));
        assert_eq!(
            body["reaction"],
            json!([{ "type": "emoji", "emoji": "🎉" }])
        );
    }

    #[tokio::test]
    async fn remove_reaction_clears_the_bot_reactions_with_an_empty_array() {
        let mock = MockTransport::new(vec![api_ok(json!(true))]);
        let cleared = adapter(&mock).remove_reaction("4242", "11", "👀").await;
        assert!(cleared);

        let requests = mock.requests();
        assert_eq!(requests.len(), 1);
        assert_eq!(requests[0].method, HttpMethod::Post);
        assert_eq!(
            requests[0].url,
            "https://api.telegram.org/botTEST-TOKEN/setMessageReaction"
        );
        let body = request_body(&requests[0]);
        assert_eq!(body["chat_id"], json!("4242"));
        assert_eq!(body["message_id"], json!(11));
        // `setMessageReaction` replaces the bot's list; an empty array is the documented clear, and
        // the emoji argument is validated but never transmitted.
        assert_eq!(body["reaction"], json!([]));
    }

    #[tokio::test]
    async fn blank_reaction_inputs_never_reach_the_wire() {
        let mock = MockTransport::new(Vec::new());
        let adapter = adapter(&mock);
        let cases = [
            ("4242", "11", ""),
            ("4242", "11", "   "),
            ("4242", "11", "\n\t"),
            ("", "11", "👍"),
            ("   ", "11", "👍"),
            ("4242", "", "👍"),
            ("4242", "  ", "👍"),
        ];
        for (chat_id, message_id, emoji) in cases {
            assert!(
                !adapter.add_reaction(chat_id, message_id, emoji).await,
                "add {chat_id:?}/{message_id:?}/{emoji:?}"
            );
            assert!(
                !adapter.remove_reaction(chat_id, message_id, emoji).await,
                "remove {chat_id:?}/{message_id:?}/{emoji:?}"
            );
        }
        assert_eq!(mock.requests().len(), 0, "nothing may hit the wire");
    }

    #[tokio::test]
    async fn a_non_integer_message_id_is_refused_without_a_request() {
        // hermes casts with `int(message_id)`; its except arm reports the reaction as failed.
        let mock = MockTransport::new(Vec::new());
        let adapter = adapter(&mock);
        for message_id in ["abc", "11a", "1.5", "m-1"] {
            assert!(!adapter.add_reaction("4242", message_id, "👍").await);
            assert!(!adapter.remove_reaction("4242", message_id, "👍").await);
        }
        assert_eq!(mock.requests().len(), 0);
    }

    #[tokio::test]
    async fn reaction_failures_are_false() {
        // A transport failure never reached Telegram.
        let down = MockTransport::new(vec![Err(TransportError::Request("dns failure".into()))]);
        let offline = adapter(&down);
        assert!(!offline.add_reaction("4242", "11", "👍").await);
        assert!(!offline.remove_reaction("4242", "11", "👀").await);

        // Every non-2xx status is a failure, FloodWait included (reactions are best effort).
        for status in [400u16, 403, 404, 429, 500, 502] {
            let mock = MockTransport::new(vec![
                api_error(status, "reaction refused"),
                api_error(status, "reaction refused"),
            ]);
            let refusing = adapter(&mock);
            assert!(
                !refusing.add_reaction("4242", "11", "👍").await,
                "add status {status}"
            );
            assert!(
                !refusing.remove_reaction("4242", "11", "👍").await,
                "remove status {status}"
            );
        }

        // A 200 envelope that says ok: false.
        let rejected = MockTransport::new(vec![response(
            200,
            json!({ "ok": false, "error_code": 400, "description": "Bad Request: reaction_invalid" }),
        )]);
        assert!(!adapter(&rejected).add_reaction("4242", "11", "👍").await);

        // A body that is not JSON at all.
        let garbage = MockTransport::new(vec![Ok(HttpResponse {
            status: 200,
            body: "<html>".to_string(),
            retry_after: None,
        })]);
        assert!(!adapter(&garbage).add_reaction("4242", "11", "👍").await);
    }

    #[tokio::test]
    async fn a_success_envelope_without_result_true_is_not_a_claimed_reaction() {
        // The Bot API documents `Returns True on success`; anything else is not that success.
        for body in [
            json!({ "ok": true }),
            json!({ "ok": true, "result": false }),
            json!({ "ok": true, "result": {} }),
            json!({ "ok": true, "result": null }),
        ] {
            let mock = MockTransport::new(vec![response(200, body.clone())]);
            let adapter = adapter(&mock);
            assert!(
                !adapter.add_reaction("4242", "11", "👍").await,
                "add {body}"
            );
            assert_eq!(mock.requests().len(), 1, "the request itself is still made");
        }
    }

    #[test]
    fn reaction_payload_helpers_shape_the_bot_api_fields() {
        assert_eq!(
            reaction_target(" 4242 ", " 11 "),
            Some(("4242".to_string(), 11))
        );
        assert_eq!(reaction_target("", "11"), None);
        assert_eq!(reaction_target("4242", "nope"), None);
        assert_eq!(
            reaction_types("\u{1F44D}"),
            Some(json!([{ "type": "emoji", "emoji": "👍" }]))
        );
        assert_eq!(reaction_types("  "), None);
    }

    // ----- native draft streaming (sendMessageDraft) -------------------------------------------

    /// The Bot API answers a draft with `{"ok": true, "result": true}`; a draft carries no message.
    fn draft_ok() -> Result<HttpResponse, TransportError> {
        api_ok(json!(true))
    }

    #[test]
    fn telegram_advertises_native_draft_streaming() {
        let mock = MockTransport::new(Vec::new());
        assert!(adapter(&mock).supports_draft_streaming());
        assert!(
            mock.requests().is_empty(),
            "the capability flag is not a call"
        );
    }

    #[tokio::test]
    async fn send_draft_posts_send_message_draft_with_the_reference_fields() {
        let mock = MockTransport::new(vec![draft_ok()]);
        let result = adapter(&mock)
            .send_draft("4242", 77, "hello **world**")
            .await;

        assert!(result.success, "{}", result.error.unwrap_or_default());
        assert!(
            result.message_id.is_none(),
            "a draft is not a message, so there is no id to report"
        );
        assert!(result.error_kind.is_none());
        assert!(result.continuation_message_ids.is_empty());

        let requests = mock.requests();
        assert_eq!(requests.len(), 1);
        assert_eq!(requests[0].method, HttpMethod::Post);
        assert_eq!(
            requests[0].url,
            "https://api.telegram.org/botTEST-TOKEN/sendMessageDraft"
        );
        let body = request_body(&requests[0]);
        assert_eq!(body["chat_id"], json!("4242"));
        assert_eq!(body["draft_id"], json!(77));
        // Same MarkdownV2 render `send` uses, so the preview does not snap when the final lands.
        assert_eq!(body["text"], json!("hello *world*"));
        assert_eq!(body["parse_mode"], json!("MarkdownV2"));
        assert_eq!(
            body.as_object().map(|object| object.len()),
            Some(4),
            "the reference sends chat_id/draft_id/text plus parse_mode: {body}"
        );
    }

    #[tokio::test]
    async fn reusing_a_draft_id_animates_the_same_preview() {
        let mock = MockTransport::new(vec![draft_ok(), draft_ok(), draft_ok()]);
        let streaming = adapter(&mock);
        for frame in ["one", "one two", "one two three"] {
            assert!(
                streaming.send_draft("4242", 9, frame).await.success,
                "frame {frame}"
            );
        }

        let bodies = mock.bodies();
        assert_eq!(bodies.len(), 3, "every frame is its own request");
        for body in &bodies {
            assert_eq!(body["chat_id"], json!("4242"));
            assert_eq!(body["draft_id"], json!(9), "the id must be reused: {body}");
        }
        let texts: Vec<&str> = bodies
            .iter()
            .map(|body| body["text"].as_str().unwrap_or_default())
            .collect();
        assert_eq!(texts, vec!["one", "one two", "one two three"]);

        // A different id is a different preview: the adapter never rewrites it.
        let other = MockTransport::new(vec![draft_ok()]);
        let other_adapter = adapter(&other);
        assert!(other_adapter.send_draft("4242", 10, "next").await.success);
        assert_eq!(request_body(&other.requests()[0])["draft_id"], json!(10));
    }

    #[tokio::test]
    async fn a_draft_frame_is_cut_to_the_utf16_cap() {
        // Each emoji is one code point but two UTF-16 units, so the cap counts 2048 of them.
        let content = "😀".repeat(MAX_MESSAGE_LENGTH);
        assert!(utf16_len(&content) > MAX_MESSAGE_LENGTH);

        let mock = MockTransport::new(vec![draft_ok()]);
        assert!(adapter(&mock).send_draft("4242", 5, &content).await.success);

        let text = request_text(&mock.requests()[0]);
        assert_eq!(
            utf16_len(&text),
            MAX_MESSAGE_LENGTH,
            "the frame must fill but not exceed the UTF-16 cap"
        );
        assert_eq!(text, "😀".repeat(MAX_MESSAGE_LENGTH / 2));
        assert!(
            content.starts_with(&text),
            "the preview must be a prefix of the frame it streams"
        );
        // The cap is on UTF-16 units, not code points: half the code points fit.
        assert_eq!(text.chars().count(), MAX_MESSAGE_LENGTH / 2);
    }

    #[tokio::test]
    async fn a_draft_parse_rejection_retries_once_as_plain_text() {
        let mock = MockTransport::new(vec![
            api_error(
                400,
                "Bad Request: can't parse entities: Can't find end of Bold entity",
            ),
            draft_ok(),
        ]);
        let result = adapter(&mock).send_draft("4242", 3, "**unbalanced").await;

        assert!(result.success, "{}", result.error.unwrap_or_default());
        let bodies = mock.bodies();
        assert_eq!(bodies.len(), 2);
        assert_eq!(bodies[0]["parse_mode"], json!("MarkdownV2"));
        assert!(
            bodies[1].get("parse_mode").is_none(),
            "the retry drops parse_mode: {}",
            bodies[1]
        );
        // The retry sends the raw text — the reference's `use_markdown=False` arm.
        assert_eq!(bodies[1]["text"], json!("**unbalanced"));
        assert_eq!(bodies[1]["draft_id"], json!(3));
        assert_eq!(bodies[1]["chat_id"], json!("4242"));
    }

    #[tokio::test]
    async fn draft_failures_are_typed_and_never_report_delivery() {
        // FloodWait: rate limited with the delay the API asked for.
        let flood = MockTransport::new(vec![response(
            429,
            json!({
                "ok": false,
                "error_code": 429,
                "description": "Too Many Requests: retry after 4",
                "parameters": { "retry_after": 4 },
            }),
        )]);
        let result = adapter(&flood).send_draft("4242", 1, "hi").await;
        assert!(!result.success);
        assert_eq!(result.error_kind.as_deref(), Some("rate_limited"));
        assert!(result.retryable);
        assert_eq!(result.retry_after, Some(4.0));

        // A 5xx is transient.
        let server = MockTransport::new(vec![api_error(500, "Internal Server Error")]);
        let result = adapter(&server).send_draft("4242", 1, "hi").await;
        assert!(!result.success);
        assert_eq!(result.error_kind.as_deref(), Some("transient"));
        assert!(result.retryable);

        // A 200 envelope that says `ok: false` is a rejection, not a draft on screen.
        let refused = MockTransport::new(vec![response(
            200,
            json!({
                "ok": false,
                "error_code": 400,
                "description": "Bad Request: draft rejected",
            }),
        )]);
        let result = adapter(&refused).send_draft("4242", 1, "hi").await;
        assert!(
            !result.success,
            "an ok:false answer must not report delivery"
        );
        assert!(result.message_id.is_none());
        assert_eq!(result.error_kind.as_deref(), Some("bad_format"));
        assert_eq!(
            result.error.as_deref(),
            Some("Bad Request: draft rejected"),
            "the API's own description must reach the caller"
        );

        // A transport failure never reached Telegram.
        let down = MockTransport::new(vec![Err(TransportError::Request(
            "connection reset".to_string(),
        ))]);
        let result = adapter(&down).send_draft("4242", 1, "hi").await;
        assert!(!result.success);
        assert_eq!(result.error_kind.as_deref(), Some("transient"));
        assert!(result.retryable);
    }

    #[tokio::test]
    async fn blank_draft_inputs_are_refused_without_a_request() {
        let mock = MockTransport::new(Vec::new());
        let adapter = adapter(&mock);
        for (chat_id, content) in [
            ("", "hello"),
            ("   ", "hello"),
            ("\n\t", "hello"),
            ("4242", ""),
            ("4242", "   "),
            ("4242", " \n\t "),
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
            mock.requests().is_empty(),
            "a blank draft must not hit the wire"
        );
    }

    #[test]
    fn draft_text_helpers_mirror_the_reference_contract() {
        assert_eq!(draft_text_within_cap("short").as_deref(), Some("short"));
        let exactly = "a".repeat(MAX_MESSAGE_LENGTH);
        assert_eq!(
            draft_text_within_cap(&exactly).as_deref(),
            Some(exactly.as_str())
        );
        let emoji = "😀".repeat(MAX_MESSAGE_LENGTH);
        assert_eq!(
            draft_text_within_cap(&emoji),
            Some("😀".repeat(MAX_MESSAGE_LENGTH / 2))
        );
        // A draft result is a success without a message id, never an invented one.
        let result = draft_sent_result();
        assert!(result.success);
        assert!(result.message_id.is_none());
        assert!(result.error.is_none());
    }
}
