//! WhatsApp adapter — the local Baileys bridge.
//!
//! The bridge is a small HTTP server on `127.0.0.1:3000` (configurable): `GET /health` reports its
//! state, `GET /messages` drains inbound messages, `POST /send`, `POST /edit` and `POST /send-media`
//! deliver. That is ordinary HTTP, so this adapter needs no socket.
//!
//! Not ported: pairing/QR onboarding and bridge process supervision (the host owns both), and the
//! reference's rich-sent-store indexing of our own attachments
//! (`_send_media_to_bridge`, `adapter.py:939-944`; the gateway has no rich sent store).
//!
//! Outbound media **is** ported: `/send-media` takes a *local file path* (`adapter.py:932-945`), so
//! [`PlatformAdapter::send_media`] stages caller bytes in one uniquely named temp file per call —
//! removed by RAII on every exit path — and posts the reference's payload. URL media is refused:
//! the reference downloads a URL to a local file first (`cache_image_from_url`, `adapter.py:974-981`)
//! and this adapter owns no binary fetcher.
//!
//! Outbound polls **are** ported: `POST send-poll` with `{chatId, question, options,
//! selectableCount}` (`send_poll`, `adapter.py:948-951`). See [`PlatformAdapter::send_poll`].

use async_trait::async_trait;
use serde_json::{Map, Value, json};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use crate::event::{MessageEvent, MessageType, Platform, SessionSource};
use crate::helpers::{codepoint_len, truncate_message};
use crate::platform::{
    ChatInfo, MediaKind, MediaSource, OutboundMedia, PlatformAdapter, SendResult,
};
use crate::transport::{HttpRequest, SharedTransport};

pub const WHATSAPP_MAX_MESSAGE_LENGTH: usize = 4096;

/// WhatsApp's own per-kind media caps, as the repository already encodes them for WhatsApp media
/// (`whatsapp_cloud/adapter.py:70-75`, Meta's documented `/media` limits): image 5 MiB, video 16 MiB,
/// audio 16 MiB, document 100 MiB.
///
/// Note on provenance: the bridge adapter's `_send_media_to_bridge` (`adapter.py:932-945`) carries no
/// size check of its own — it only refuses a missing path — so there is no *bridge-side* number to
/// copy. These are the WhatsApp-service limits, applied locally so oversized bytes are refused before
/// they are written to disk or posted, instead of being rejected by Baileys after a 100 MiB transfer.
pub const WHATSAPP_MAX_IMAGE_BYTES: usize = 5 * 1024 * 1024;
pub const WHATSAPP_MAX_VIDEO_BYTES: usize = 16 * 1024 * 1024;
pub const WHATSAPP_MAX_AUDIO_BYTES: usize = 16 * 1024 * 1024;
pub const WHATSAPP_MAX_DOCUMENT_BYTES: usize = 100 * 1024 * 1024;

/// Longest basename kept from a caller filename, so one absurd string cannot produce an absurd path.
pub const WHATSAPP_MAX_MEDIA_NAME_CHARS: usize = 120;

/// WhatsApp's poll shape: 2–12 options per poll, and the reference's own poll caller only takes the
/// native path for `2 <= len(clean_choices) <= 12` (`send_clarify`, `adapter.py:956-958`).
pub const WHATSAPP_MIN_POLL_OPTIONS: usize = 2;
pub const WHATSAPP_MAX_POLL_OPTIONS: usize = 12;

/// WhatsApp's documented text caps for a poll: a 255-character question and 100-character options.
/// The reference itself validates neither (it forwards whatever it is handed, `adapter.py:950`), so
/// these are the service limits, applied locally the same way the media caps above are
/// ([`WHATSAPP_MAX_IMAGE_BYTES`]) — a poll that could never be created costs no bridge request.
pub const WHATSAPP_MAX_POLL_QUESTION_CHARS: usize = 255;
pub const WHATSAPP_MAX_POLL_OPTION_CHARS: usize = 100;

/// `selectableCount` the reference sends by default: `send_poll(..., selectable_count: int = 1)`
/// (`adapter.py:948`), i.e. a single-choice poll. The trait's
/// [`PlatformAdapter::send_poll`] carries no count parameter, so this adapter always sends 1.
pub const WHATSAPP_POLL_SELECTABLE_COUNT: usize = 1;

pub struct WhatsAppAdapter {
    transport: SharedTransport,
    bridge_base: String,
}

impl WhatsAppAdapter {
    pub fn new(transport: SharedTransport, bridge_base: impl Into<String>) -> Self {
        let base = bridge_base.into();
        let base = if base.trim().is_empty() {
            "http://127.0.0.1:3000".to_string()
        } else {
            base.trim_end_matches('/').to_string()
        };
        Self {
            transport,
            bridge_base: base,
        }
    }

    fn endpoint(&self, path: &str) -> String {
        format!("{}/{path}", self.bridge_base)
    }

    /// Drain `/messages` once and normalize whatever the bridge queued.
    pub async fn poll_once(&self) -> Result<Vec<MessageEvent>, String> {
        let request = HttpRequest::get(self.endpoint("messages"));
        let response = self
            .transport
            .execute(request)
            .await
            .map_err(|err| format!("whatsapp bridge transport error: {err}"))?;
        if !response.ok() {
            return Err(format!(
                "whatsapp bridge returned {}: {}",
                response.status,
                response.body.chars().take(200).collect::<String>()
            ));
        }
        self.parse_messages(&response.body)
    }

    async fn post_json(&self, path: &str, body: &Value) -> Result<Value, (u16, String)> {
        let request = HttpRequest::json(self.endpoint(path), body);
        let response = self
            .transport
            .execute(request)
            .await
            .map_err(|err| (0, err.to_string()))?;
        if !response.ok() {
            return Err((response.status, response.body));
        }
        Ok(response.json().unwrap_or(Value::Null))
    }

    /// One `POST <path>` and the bridge's answer, read the way the reference reads it.
    ///
    /// The reference's `_post_bridge_message` treats HTTP 200 as success and takes the id off
    /// `messageId` (`adapter.py:885-894`), and `_send_media_to_bridge` then insists on
    /// `result.success and result.message_id` before it records the attachment (`adapter.py:939`).
    /// `send_poll` rides the very same envelope (`adapter.py:951`). So a send is reported as
    /// delivered only when the bridge answered 2xx **and** named a non-empty `messageId`; a 2xx
    /// without an id is an unclear delivery, not a success. `label` names the send in the error.
    async fn post_bridge_message(&self, path: &str, payload: &Value, label: &str) -> SendResult {
        let request = HttpRequest::json(self.endpoint(path), payload);
        let response = match self.transport.execute(request).await {
            Ok(response) => response,
            Err(err) => {
                return SendResult::failed(
                    format!("whatsapp bridge transport error: {err}"),
                    Some("transient"),
                )
                .retryable(None);
            }
        };
        if !response.ok() {
            let mut failure = classify_media_failure(response.status, &response.body);
            // The transport already parsed `Retry-After`; keep the bridge's own pacing hint rather
            // than making the delivery ledger guess a backoff.
            if failure.retryable && failure.retry_after.is_none() {
                failure.retry_after = response.retry_after;
            }
            return failure;
        }
        let excerpt: String = response.body.chars().take(200).collect();
        let value = response.json().unwrap_or(Value::Null);
        // An explicit refusal envelope wins over any id it happens to carry.
        if value.get("success").and_then(Value::as_bool) == Some(false)
            || value.get("ok").and_then(Value::as_bool) == Some(false)
        {
            return SendResult::failed(
                format!("whatsapp bridge refused the {label}: {excerpt}"),
                Some("unknown"),
            );
        }
        let message_id = value
            .get("messageId")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|id| !id.is_empty())
            .map(str::to_owned);
        match message_id {
            Some(id) => SendResult {
                raw_response: Some(value),
                ..SendResult::ok(id)
            },
            None => SendResult::failed(
                format!("whatsapp bridge answered /{path} without a message id: {excerpt}"),
                Some("unknown"),
            ),
        }
    }

    /// One `POST /send-media` and the bridge's answer, read the way the reference reads it
    /// (see [`Self::post_bridge_message`]).
    async fn post_media(&self, payload: &Value) -> SendResult {
        self.post_bridge_message("send-media", payload, "media send")
            .await
    }

    /// Drain inbound messages from the bridge. Delivery-status entries produce no events.
    pub fn parse_messages(&self, body: &str) -> Result<Vec<MessageEvent>, String> {
        let root: Value =
            serde_json::from_str(body).map_err(|err| format!("invalid json: {err}"))?;
        let Some(items) = root.as_array() else {
            return Ok(Vec::new());
        };
        let platform = Platform::new("whatsapp").expect("valid platform");
        let mut events = Vec::new();
        for data in items {
            if data.get("event").and_then(Value::as_str) == Some("delivery_status") {
                continue;
            }
            if data.get("fromMe").and_then(Value::as_bool) == Some(true)
                && data.get("fromOwner").and_then(Value::as_bool) != Some(true)
            {
                continue;
            }
            let chat_id = data
                .get("chatId")
                .and_then(Value::as_str)
                .unwrap_or_default();
            if chat_id.is_empty() {
                continue;
            }
            let is_group = data
                .get("isGroup")
                .and_then(Value::as_bool)
                .unwrap_or(false);
            let body_text = data
                .get("body")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string();
            let media = data
                .get("mediaUrls")
                .and_then(Value::as_array)
                .cloned()
                .unwrap_or_default();
            if body_text.trim().is_empty() && media.is_empty() {
                continue;
            }

            let mut source = SessionSource::new(platform.clone(), chat_id.to_string());
            source.chat_type = if is_group { "group" } else { "dm" }.to_string();
            source.chat_name = data
                .get("chatName")
                .and_then(Value::as_str)
                .map(str::to_owned);
            source.user_id = data
                .get("senderId")
                .and_then(Value::as_str)
                .map(str::to_owned);
            source.user_name = data
                .get("senderName")
                .and_then(Value::as_str)
                .map(str::to_owned);
            source.message_id = data
                .get("messageId")
                .and_then(Value::as_str)
                .map(str::to_owned);

            let mut event = MessageEvent::new(body_text, source);
            event.message_id = event.source.message_id.clone();
            event.user_id = event.source.user_id.clone();
            event.user_name = event.source.user_name.clone();
            event.raw_message = Some(data.clone());
            event.message_type = classify_message(data);
            for url in media {
                if let Some(url) = url.as_str() {
                    event.media_urls.push(url.to_string());
                }
            }
            if let Some(mime) = data.get("mime").and_then(Value::as_str) {
                event.media_types.push(mime.to_string());
            }
            if data.get("hasQuotedMessage").and_then(Value::as_bool) == Some(true) {
                event.reply_to_message_id = data
                    .get("quotedMessageId")
                    .and_then(Value::as_str)
                    .map(str::to_owned);
                event.reply_to_text = data
                    .get("quotedText")
                    .and_then(Value::as_str)
                    .map(str::to_owned);
            }
            events.push(event);
        }
        Ok(events)
    }
}

fn classify_message(data: &Value) -> MessageType {
    if data.get("hasMedia").and_then(Value::as_bool) != Some(true) {
        return MessageType::Text;
    }
    let kind = data
        .get("mediaType")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_ascii_lowercase();
    if kind.contains("image") || kind == "photo" {
        MessageType::Photo
    } else if kind.contains("video") {
        MessageType::Video
    } else if kind.contains("ptt") || kind.contains("voice") {
        MessageType::Voice
    } else if kind.contains("audio") {
        MessageType::Audio
    } else {
        MessageType::Document
    }
}

/// WhatsApp addresses are JIDs: `digits@s.whatsapp.net` for a person, `…@g.us` for a group.
fn to_jid(chat_id: &str) -> String {
    let trimmed = chat_id.trim();
    if trimmed.contains('@') {
        return trimmed.to_string();
    }
    if trimmed.starts_with("group:") || trimmed.starts_with("g:") {
        let id = trimmed
            .strip_prefix("group:")
            .or_else(|| trimmed.strip_prefix("g:"))
            .unwrap_or(trimmed);
        return format!("{id}@g.us");
    }
    format!("{trimmed}@s.whatsapp.net")
}

/// The `mediaType` string the bridge expects for a kind: the labels `_MEDIA_INFO` maps to
/// (`adapter.py:399-403`) and `_bridge_media_type` produces (`adapter.py:1216-1218`).
fn media_type_label(kind: MediaKind) -> &'static str {
    match kind {
        MediaKind::Image => "image",
        MediaKind::Video => "video",
        MediaKind::Audio => "audio",
        MediaKind::Document => "document",
    }
}

/// The largest payload this adapter will stage for a kind (see [`WHATSAPP_MAX_IMAGE_BYTES`]).
fn media_size_cap(kind: MediaKind) -> usize {
    match kind {
        MediaKind::Image => WHATSAPP_MAX_IMAGE_BYTES,
        MediaKind::Video => WHATSAPP_MAX_VIDEO_BYTES,
        MediaKind::Audio => WHATSAPP_MAX_AUDIO_BYTES,
        MediaKind::Document => WHATSAPP_MAX_DOCUMENT_BYTES,
    }
}

/// Reduce a caller filename to a safe basename for a temp file and for the wire's `fileName`.
///
/// Only the last path component survives, every character outside `[A-Za-z0-9._-]` folds to `_`, and
/// leading/trailing dots go, so `.`, `..` and `a/b` cannot escape the temp directory or point the
/// bridge at a different file. Over-long names are truncated with the extension kept. `None` when
/// nothing usable is left; the caller then advertises no `fileName` at all rather than a guess.
fn sanitize_media_file_name(raw: &str) -> Option<String> {
    let base = raw.rsplit(['/', '\\']).next()?;
    let folded: String = base
        .chars()
        .map(|ch| {
            if ch.is_ascii_alphanumeric() || matches!(ch, '.' | '_' | '-') {
                ch
            } else {
                '_'
            }
        })
        .collect();
    let trimmed = folded.trim_matches('.').to_string();
    if trimmed.is_empty() || !trimmed.chars().any(|ch| ch.is_ascii_alphanumeric()) {
        return None;
    }
    Some(truncate_media_name(&trimmed))
}

/// Cap a basename at [`WHATSAPP_MAX_MEDIA_NAME_CHARS`], keeping a plausible extension so the name
/// still says what the attachment is.
fn truncate_media_name(name: &str) -> String {
    if name.chars().count() <= WHATSAPP_MAX_MEDIA_NAME_CHARS {
        return name.to_string();
    }
    match name.rsplit_once('.') {
        Some((stem, extension)) if !extension.is_empty() && extension.chars().count() <= 12 => {
            let room = WHATSAPP_MAX_MEDIA_NAME_CHARS - extension.chars().count() - 1;
            let stem: String = stem.chars().take(room).collect();
            format!("{stem}.{extension}")
        }
        _ => name.chars().take(WHATSAPP_MAX_MEDIA_NAME_CHARS).collect(),
    }
}

/// One temp file holding outbound bytes for the duration of a `/send-media` call.
///
/// The bridge reads the file from `filePath` while it uploads, so it must exist for the whole
/// request and must not outlive it: [`Drop`] removes it on every exit path, including the ones where
/// the bridge fails or the future is dropped.
struct TempMediaFile {
    path: PathBuf,
}

impl TempMediaFile {
    /// Write `bytes` into a fresh file whose name ends in the sanitised `base` (when there is one).
    /// `create_new` is what makes the name safe to hand out: a collision is retried, never clobbered.
    fn write(base: Option<&str>, bytes: &[u8]) -> std::io::Result<Self> {
        use std::io::Write as _;

        let mut last_collision: Option<std::io::Error> = None;
        for _ in 0..8 {
            let path = unique_media_path(base);
            match std::fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(&path)
            {
                Ok(mut file) => {
                    if let Err(err) = file.write_all(bytes) {
                        drop(file);
                        let _ = std::fs::remove_file(&path);
                        return Err(err);
                    }
                    return Ok(Self { path });
                }
                Err(err) if err.kind() == std::io::ErrorKind::AlreadyExists => {
                    last_collision = Some(err);
                }
                Err(err) => return Err(err),
            }
        }
        Err(last_collision
            .unwrap_or_else(|| std::io::Error::other("could not create a unique media file")))
    }

    /// The path handed to the bridge as `filePath`.
    fn path(&self) -> &Path {
        &self.path
    }
}

impl Drop for TempMediaFile {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.path);
    }
}

/// `vikram-whatsapp-<pid>-<sequence>-<nanos>[-<sanitised name>]` in the OS temp dir: unique per call
/// without clobbering a name another send (or another process) is using.
fn unique_media_path(base: Option<&str>) -> PathBuf {
    static SEQUENCE: AtomicU64 = AtomicU64::new(0);
    let sequence = SEQUENCE.fetch_add(1, Ordering::Relaxed);
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|elapsed| elapsed.as_nanos())
        .unwrap_or(0);
    let mut name = format!("vikram-whatsapp-{}-{sequence}-{nanos}", std::process::id());
    if let Some(base) = base {
        name.push('-');
        name.push_str(base);
    }
    std::env::temp_dir().join(name)
}

#[async_trait]
impl PlatformAdapter for WhatsAppAdapter {
    async fn connect(&mut self, _is_reconnect: bool) -> bool {
        let request = HttpRequest::get(self.endpoint("health"));
        matches!(self.transport.execute(request).await, Ok(response) if response.ok())
    }

    async fn disconnect(&mut self) {}

    async fn send(
        &self,
        chat_id: &str,
        content: &str,
        reply_to: Option<&str>,
        _metadata: Option<Value>,
    ) -> SendResult {
        if content.trim().is_empty() {
            // The bridge treats a blank reply as "nothing to say".
            return SendResult {
                success: true,
                message_id: None,
                error: None,
                raw_response: None,
                retryable: false,
                retry_after: None,
                continuation_message_ids: Vec::new(),
                error_kind: None,
            };
        }
        let jid = to_jid(chat_id);
        let chunks = truncate_message(content, WHATSAPP_MAX_MESSAGE_LENGTH, codepoint_len);
        let mut ids = Vec::new();
        for (index, chunk) in chunks.iter().enumerate() {
            let mut payload = json!({"chatId": jid, "message": chunk});
            if index == 0
                && let Some(reply_to) = reply_to
            {
                payload["replyTo"] = json!(reply_to);
            }
            match self.post_json("send", &payload).await {
                Ok(value) => {
                    if let Some(id) = value.get("messageId").and_then(Value::as_str) {
                        ids.push(id.to_string());
                    }
                }
                Err((status, body)) => {
                    let mut result = classify_failure(status, &body);
                    result.continuation_message_ids = ids;
                    return result;
                }
            }
        }
        match ids.last() {
            Some(last) => SendResult::ok_split(last.clone(), ids),
            None => SendResult::ok(""),
        }
    }

    async fn edit_message(
        &self,
        chat_id: &str,
        message_id: &str,
        content: &str,
        _finalize: bool,
    ) -> SendResult {
        if content.trim().is_empty() {
            return SendResult::failed("empty message content", Some("bad_format"));
        }
        let payload = json!({
            "chatId": to_jid(chat_id),
            "messageId": message_id,
            "message": content,
        });
        match self.post_json("edit", &payload).await {
            Ok(_) => SendResult::ok(message_id),
            Err((status, body)) => classify_failure(status, &body),
        }
    }

    /// Polls are implemented: the bridge has a native `/send-poll` route (`adapter.py:948-951`).
    fn supports_polls(&self) -> bool {
        true
    }

    /// Send one native WhatsApp poll through the bridge's `/send-poll` route.
    ///
    /// Wire evidence: the reference posts `{chatId, question, options, selectableCount}` to
    /// `send-poll` with a 30 s timeout (`send_poll`, `adapter.py:948-951`) and reads the answer with
    /// `_post_bridge_message` — 2xx plus a non-empty `messageId` is the only success
    /// (`adapter.py:885-894`). So:
    ///
    /// - `chatId` is the same JID mapping text and media use ([`to_jid`]);
    /// - `question` and `options` are trimmed and blank options dropped, which is exactly the
    ///   cleanup the reference's own poll caller performs before it calls `send_poll`
    ///   (`send_clarify`, `adapter.py:956-958`: the question is stripped, choices are stripped and
    ///   blank ones dropped, and only `2 <= len(clean_choices) <= 12` goes down the native path);
    /// - `selectableCount` is the reference's default of 1 — a single-choice poll
    ///   ([`WHATSAPP_POLL_SELECTABLE_COUNT`]) — because [`PlatformAdapter::send_poll`] carries no
    ///   count parameter. The reference's own poll caller also asks for 1 (`adapter.py:958`).
    ///
    /// WhatsApp's documented poll limits are enforced locally, before the wire, so an impossible
    /// poll costs no request: a blank question or fewer than [`WHATSAPP_MIN_POLL_OPTIONS`] usable
    /// options is `bad_format`, more than [`WHATSAPP_MAX_POLL_OPTIONS`] usable options is
    /// `bad_format` too (the poll's shape is wrong, not its size), and a question or option past its
    /// character cap ([`WHATSAPP_MAX_POLL_QUESTION_CHARS`] / [`WHATSAPP_MAX_POLL_OPTION_CHARS`]) is
    /// `too_long`, matching how oversized media is refused.
    ///
    /// Bridge failures reuse the media taxonomy (see [`classify_media_failure`]): 429 rate-limited
    /// and retryable, 403 forbidden, 404 not found, 413 too long, other 4xx bad format, 5xx and
    /// transport transient/retryable. A 2xx that names no `messageId`, or an explicit
    /// `success: false` envelope, is `unknown` — never a success.
    async fn send_poll(&self, chat_id: &str, question: &str, options: &[String]) -> SendResult {
        if chat_id.trim().is_empty() {
            return SendResult::failed(
                "whatsapp needs a chat id to send a poll",
                Some("bad_format"),
            );
        }
        let question = question.trim();
        let question_chars = question.chars().count();
        if question_chars == 0 {
            return SendResult::failed(
                "whatsapp polls need a non-empty question",
                Some("bad_format"),
            );
        }
        if question_chars > WHATSAPP_MAX_POLL_QUESTION_CHARS {
            return SendResult::failed(
                format!(
                    "whatsapp poll question is {question_chars} characters; the WhatsApp cap is \
                     {WHATSAPP_MAX_POLL_QUESTION_CHARS}"
                ),
                Some("too_long"),
            );
        }
        let cleaned: Vec<&str> = options
            .iter()
            .map(|option| option.trim())
            .filter(|option| !option.is_empty())
            .collect();
        if cleaned.len() < WHATSAPP_MIN_POLL_OPTIONS {
            return SendResult::failed(
                format!(
                    "whatsapp polls need at least {WHATSAPP_MIN_POLL_OPTIONS} non-empty options; \
                     got {}",
                    cleaned.len()
                ),
                Some("bad_format"),
            );
        }
        if cleaned.len() > WHATSAPP_MAX_POLL_OPTIONS {
            return SendResult::failed(
                format!(
                    "whatsapp polls take at most {WHATSAPP_MAX_POLL_OPTIONS} options; got {}",
                    cleaned.len()
                ),
                Some("bad_format"),
            );
        }
        if let Some(long) = cleaned
            .iter()
            .find(|option| option.chars().count() > WHATSAPP_MAX_POLL_OPTION_CHARS)
        {
            let chars = long.chars().count();
            let preview: String = long.chars().take(40).collect();
            return SendResult::failed(
                format!(
                    "whatsapp poll option is {chars} characters; the WhatsApp cap is \
                     {WHATSAPP_MAX_POLL_OPTION_CHARS}: {preview}"
                ),
                Some("too_long"),
            );
        }
        let payload = json!({
            "chatId": to_jid(chat_id),
            "question": question,
            "options": cleaned,
            "selectableCount": WHATSAPP_POLL_SELECTABLE_COUNT,
        });
        self.post_bridge_message("send-poll", &payload, "poll send")
            .await
    }

    /// Attachments are implemented: the bridge's `/send-media` takes a local path per file.
    fn supports_media(&self) -> bool {
        true
    }

    /// Send one attachment through the bridge's `/send-media` route.
    ///
    /// The reference's wire body is `{chatId, filePath, mediaType}` plus `caption`/`fileName` only
    /// when they are truthy (`_send_media_to_bridge`, `adapter.py:932-945`): `chatId` is the WhatsApp
    /// JID, `mediaType` is `image`/`video`/`audio`/`document` (`_MEDIA_INFO`, `adapter.py:399-403`),
    /// and there is **no** `replyTo` field — every reference media helper accepts `reply_to` and drops
    /// it (`adapter.py:983-999`), so [`Self::send_media`]'s `reply_to` is not forwarded.
    ///
    /// - [`MediaSource::Bytes`]: written to a uniquely named temp file (the caller's basename
    ///   sanitised to `[A-Za-z0-9._-]`) and sent as `filePath`; [`TempMediaFile`] removes it once the
    ///   request has returned, on success and on every failure path alike.
    /// - [`MediaSource::Url`]: refused with zero requests. The reference downloads URL media to a
    ///   local file before sending it (`cache_image_from_url`, `adapter.py:974-981`, then the
    ///   path-taking helper) and this adapter has no binary fetcher, so there is nothing to post.
    ///
    /// Refusals all happen before the wire is touched: empty bytes are `bad_format`, bytes above the
    /// kind's WhatsApp cap ([`WHATSAPP_MAX_IMAGE_BYTES`] and friends) are `too_long`, and a missing
    /// chat id is `bad_format`. A failed staging write is `transient`/retryable. The reference's local
    /// `File not found` pre-check (`adapter.py:933-934`) cannot trigger here — the file is written by
    /// this call, not handed in by the caller.
    async fn send_media(
        &self,
        chat_id: &str,
        media: &OutboundMedia,
        _reply_to: Option<&str>,
    ) -> SendResult {
        if chat_id.trim().is_empty() {
            return SendResult::failed(
                "whatsapp needs a chat id to send media",
                Some("bad_format"),
            );
        }
        let media_type = media_type_label(media.kind);
        let bytes = match &media.source {
            MediaSource::Bytes(bytes) => bytes,
            MediaSource::Url(url) => {
                return SendResult::failed(
                    format!(
                        "whatsapp media send needs bytes: refusing MediaSource::Url({url}) because the \
                         reference downloads URL media into a local file before sending it \
                         (`cache_image_from_url`, adapter.py:974-981) and this adapter has no binary \
                         fetcher — pass MediaSource::Bytes"
                    ),
                    Some("bad_format"),
                );
            }
        };
        if bytes.is_empty() {
            // The bridge would have a zero-byte file path to hand Baileys: nothing to attach.
            return SendResult::failed(
                "whatsapp media send: refusing to send empty bytes",
                Some("bad_format"),
            );
        }
        let cap = media_size_cap(media.kind);
        if bytes.len() > cap {
            return SendResult::failed(
                format!(
                    "whatsapp {media_type} send is {} bytes; the WhatsApp cap is {cap} bytes",
                    bytes.len()
                ),
                Some("too_long"),
            );
        }
        // Only a usable basename is advertised; an unusable one is dropped rather than echoed.
        let file_name = media.filename.as_deref().and_then(sanitize_media_file_name);
        let staged = match TempMediaFile::write(file_name.as_deref(), bytes) {
            Ok(staged) => staged,
            Err(err) => {
                return SendResult::failed(
                    format!("whatsapp media temp file: {err}"),
                    Some("transient"),
                )
                .retryable(None);
            }
        };
        let mut body = Map::new();
        body.insert("chatId".to_string(), json!(to_jid(chat_id)));
        body.insert(
            "filePath".to_string(),
            json!(staged.path().to_string_lossy().as_ref()),
        );
        body.insert("mediaType".to_string(), json!(media_type));
        if let Some(caption) = media.caption.as_deref().filter(|text| !text.is_empty()) {
            body.insert("caption".to_string(), json!(caption));
        }
        if let Some(file_name) = file_name.as_deref() {
            body.insert("fileName".to_string(), json!(file_name));
        }
        let result = self.post_media(&Value::Object(body)).await;
        // The bridge reads filePath while it answers, so the file lives until here and no longer.
        drop(staged);
        result
    }

    fn splits_long_messages(&self) -> bool {
        true
    }

    fn max_message_length(&self) -> usize {
        WHATSAPP_MAX_MESSAGE_LENGTH
    }

    async fn get_chat_info(&self, chat_id: &str) -> ChatInfo {
        let request = HttpRequest::get(self.endpoint(&format!("chat/{}", to_jid(chat_id))));
        match self.transport.execute(request).await {
            Ok(response) if response.ok() => {
                let value = response.json().unwrap_or(Value::Null);
                ChatInfo {
                    name: value
                        .get("name")
                        .and_then(Value::as_str)
                        .unwrap_or(chat_id)
                        .to_string(),
                    chat_type: if value.get("isGroup").and_then(Value::as_bool) == Some(true) {
                        "group".to_string()
                    } else {
                        "dm".to_string()
                    },
                }
            }
            _ => ChatInfo {
                name: chat_id.to_string(),
                chat_type: "dm".to_string(),
            },
        }
    }

    async fn send_typing(&self, _chat_id: &str, _metadata: Option<Value>) {}
}

/// Failure classification for the media route: the shared text taxonomy plus `413`, which a bridge
/// (or a proxy in front of it) uses for "upload too big" — a size refusal, not a bad request.
fn classify_media_failure(status: u16, body: &str) -> SendResult {
    if status == 413 {
        let excerpt: String = body.chars().take(200).collect();
        return SendResult::failed(
            format!("whatsapp bridge returned 413: {excerpt}"),
            Some("too_long"),
        );
    }
    classify_failure(status, body)
}

/// Text-route classification, unchanged: 429 rate-limited/retryable, 401/403 forbidden, 404 not
/// found, other 4xx bad format, 5xx and transport (`0`) transient/retryable.
fn classify_failure(status: u16, body: &str) -> SendResult {
    let excerpt: String = body.chars().take(200).collect();
    let message = format!("whatsapp bridge returned {status}: {excerpt}");
    match status {
        429 => SendResult::failed(message, Some("rate_limited")).retryable(None),
        401 | 403 => SendResult::failed(message, Some("forbidden")),
        404 => SendResult::failed(message, Some("not_found")),
        400..=499 => SendResult::failed(message, Some("bad_format")),
        500..=599 => SendResult::failed(message, Some("transient")).retryable(None),
        _ => SendResult::failed(message, Some("transient")).retryable(None),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::transport::{HttpResponse, Transport, TransportError};
    use async_trait::async_trait;
    use std::collections::VecDeque;
    use std::sync::{Arc, Mutex};

    struct MockTransport {
        requests: Mutex<Vec<HttpRequest>>,
        responses: Mutex<VecDeque<HttpResponse>>,
        /// Every `filePath` a `/send-media` body named, with whether the file was on disk at the
        /// moment the bridge would have read it.
        media_paths: Mutex<Vec<(String, bool)>>,
    }

    impl MockTransport {
        fn new(responses: Vec<HttpResponse>) -> Arc<Self> {
            Arc::new(Self {
                requests: Mutex::new(Vec::new()),
                responses: Mutex::new(responses.into()),
                media_paths: Mutex::new(Vec::new()),
            })
        }
        fn requests(&self) -> Vec<HttpRequest> {
            self.requests.lock().unwrap().clone()
        }
        fn media_paths(&self) -> Vec<(String, bool)> {
            self.media_paths.lock().unwrap().clone()
        }
        /// Record the `filePath` of a `/send-media` body together with its existence at request time.
        fn observe_media_path(&self, request: &HttpRequest) {
            let path = request
                .body
                .as_deref()
                .and_then(|body| serde_json::from_slice::<Value>(body).ok())
                .and_then(|value| {
                    value
                        .get("filePath")
                        .and_then(Value::as_str)
                        .map(str::to_owned)
                });
            if let Some(path) = path {
                let existed = Path::new(&path).exists();
                self.media_paths.lock().unwrap().push((path, existed));
            }
        }
    }

    #[async_trait]
    impl Transport for MockTransport {
        async fn execute(&self, request: HttpRequest) -> Result<HttpResponse, TransportError> {
            self.observe_media_path(&request);
            self.requests.lock().unwrap().push(request);
            self.responses
                .lock()
                .unwrap()
                .pop_front()
                .ok_or_else(|| TransportError::Request("no scripted response".into()))
        }
    }

    fn build(responses: Vec<HttpResponse>) -> (WhatsAppAdapter, Arc<MockTransport>) {
        let mock = MockTransport::new(responses);
        let adapter = WhatsAppAdapter::new(Arc::clone(&mock) as SharedTransport, "");
        (adapter, mock)
    }

    fn json_response(status: u16, body: Value) -> HttpResponse {
        HttpResponse {
            status,
            body: body.to_string(),
            retry_after: None,
        }
    }

    fn body_of(request: &HttpRequest) -> Value {
        serde_json::from_slice(request.body.as_ref().expect("body")).expect("json")
    }

    #[tokio::test]
    async fn connect_probes_health() {
        let (mut adapter, mock) = build(vec![json_response(200, json!({"status": "connected"}))]);
        assert!(adapter.connect(false).await);
        assert_eq!(mock.requests()[0].url, "http://127.0.0.1:3000/health");
    }

    #[tokio::test]
    async fn send_posts_the_bridge_payload_with_a_jid() {
        let (adapter, mock) = build(vec![json_response(200, json!({"messageId": "m1"}))]);
        let result = adapter.send("15551234567", "hello", Some("q1"), None).await;
        assert!(result.success);
        assert_eq!(result.message_id.as_deref(), Some("m1"));
        let requests = mock.requests();
        assert_eq!(requests[0].url, "http://127.0.0.1:3000/send");
        let body = body_of(&requests[0]);
        assert_eq!(body["chatId"], "15551234567@s.whatsapp.net");
        assert_eq!(body["message"], "hello");
        assert_eq!(body["replyTo"], "q1");
    }

    #[tokio::test]
    async fn a_group_target_becomes_a_group_jid() {
        let (adapter, mock) = build(vec![json_response(200, json!({"messageId": "m1"}))]);
        adapter.send("group:12345", "hi", None, None).await;
        assert_eq!(body_of(&mock.requests()[0])["chatId"], "12345@g.us");
    }

    #[tokio::test]
    async fn long_text_is_chunked_and_quoted_once() {
        let content = "x".repeat(WHATSAPP_MAX_MESSAGE_LENGTH + 1);
        let (adapter, mock) = build(vec![
            json_response(200, json!({"messageId": "a"})),
            json_response(200, json!({"messageId": "b"})),
        ]);
        let result = adapter.send("1", &content, Some("q1"), None).await;
        assert!(result.success);
        assert_eq!(result.continuation_message_ids, vec!["a", "b"]);
        let requests = mock.requests();
        assert_eq!(requests.len(), 2);
        assert!(body_of(&requests[0]).get("replyTo").is_some());
        assert!(body_of(&requests[1]).get("replyTo").is_none());
    }

    #[tokio::test]
    async fn errors_are_classified_and_partial_delivery_kept() {
        let (adapter, _) = build(vec![
            json_response(200, json!({"messageId": "a"})),
            HttpResponse {
                status: 429,
                body: "slow down".into(),
                retry_after: Some(2.0),
            },
        ]);
        let content = "x".repeat(WHATSAPP_MAX_MESSAGE_LENGTH + 1);
        let result = adapter.send("1", &content, None, None).await;
        assert!(!result.success);
        assert!(result.retryable);
        assert_eq!(result.continuation_message_ids, vec!["a"]);
    }

    #[tokio::test]
    async fn edit_posts_to_the_edit_endpoint() {
        let (adapter, mock) = build(vec![json_response(200, json!({}))]);
        let result = adapter.edit_message("1", "m1", "fixed", false).await;
        assert!(result.success);
        assert_eq!(mock.requests()[0].url, "http://127.0.0.1:3000/edit");
        let body = body_of(&mock.requests()[0]);
        assert_eq!(body["messageId"], "m1");
        assert_eq!(body["message"], "fixed");
    }

    #[tokio::test]
    async fn chat_info_reads_the_bridge() {
        let (adapter, _) = build(vec![json_response(
            200,
            json!({"name": "Team", "isGroup": true}),
        )]);
        let info = adapter.get_chat_info("123@g.us").await;
        assert_eq!(info.name, "Team");
        assert_eq!(info.chat_type, "group");
    }

    #[test]
    fn inbound_messages_parse_and_status_entries_are_skipped() {
        let (adapter, _) = build(vec![]);
        let body = json!([
            {"event": "delivery_status", "messageId": "m0"},
            {"chatId": "1555@s.whatsapp.net", "chatName": "Asha", "senderId": "1555",
             "senderName": "Asha", "messageId": "m1", "body": "hello",
             "hasQuotedMessage": true, "quotedMessageId": "q1", "quotedText": "earlier"},
            {"chatId": "9@g.us", "isGroup": true, "senderId": "77", "messageId": "m2",
             "body": "group hi"},
            {"chatId": "9@g.us", "isGroup": true, "senderId": "77", "messageId": "m3",
             "body": "", "hasMedia": true, "mediaType": "image", "mediaUrls": ["/cache/x.jpg"], "mime": "image/jpeg"},
            {"chatId": "9@g.us", "messageId": "m4", "body": "mine", "fromMe": true},
            {"chatId": "", "body": "no chat"}
        ])
        .to_string();
        let events = adapter.parse_messages(&body).unwrap();
        assert_eq!(events.len(), 3);
        assert_eq!(events[0].text, "hello");
        assert_eq!(events[0].source.chat_type, "dm");
        assert_eq!(events[0].reply_to_message_id.as_deref(), Some("q1"));
        assert_eq!(events[1].source.chat_type, "group");
        assert_eq!(events[1].text, "group hi");
        assert_eq!(events[2].message_type, MessageType::Photo);
        assert_eq!(events[2].media_urls, vec!["/cache/x.jpg"]);
        assert_eq!(events[2].media_types, vec!["image/jpeg"]);
    }

    #[tokio::test]
    async fn blank_content_is_a_successful_noop() {
        let (adapter, mock) = build(vec![]);
        let result = adapter.send("1", "  ", None, None).await;
        assert!(result.success);
        assert!(result.message_id.is_none());
        assert!(mock.requests().is_empty());
    }

    // ----- outbound media -----

    /// Read a request body without panicking: an absent or unparsable body reads as `Null`.
    fn media_body(request: &HttpRequest) -> Value {
        request
            .body
            .as_deref()
            .and_then(|body| serde_json::from_slice::<Value>(body).ok())
            .unwrap_or(Value::Null)
    }

    fn bytes_media(kind: MediaKind, bytes: &[u8]) -> OutboundMedia {
        OutboundMedia::new(kind, MediaSource::Bytes(bytes.to_vec()))
    }

    fn sent(id: &str) -> HttpResponse {
        json_response(200, json!({ "messageId": id }))
    }

    #[tokio::test]
    async fn media_sends_are_supported() {
        let (adapter, _) = build(vec![]);
        assert!(adapter.supports_media());
    }

    #[tokio::test]
    async fn media_payload_carries_the_jid_kind_caption_and_file_name() {
        for (kind, expected) in [
            (MediaKind::Image, "image"),
            (MediaKind::Video, "video"),
            (MediaKind::Audio, "audio"),
            (MediaKind::Document, "document"),
        ] {
            let (adapter, mock) = build(vec![sent("wamid-1")]);
            let media = bytes_media(kind, b"payload")
                .with_filename("Quarterly Report.PNG")
                .with_caption("look at this");
            let result = adapter.send_media("15551234567", &media, Some("q1")).await;
            assert!(result.success, "{kind:?}");
            assert_eq!(result.message_id.as_deref(), Some("wamid-1"), "{kind:?}");

            let requests = mock.requests();
            assert_eq!(requests.len(), 1, "{kind:?}");
            assert_eq!(requests[0].url, "http://127.0.0.1:3000/send-media");
            let body = media_body(&requests[0]);
            assert_eq!(body["chatId"], "15551234567@s.whatsapp.net", "{kind:?}");
            assert_eq!(body["mediaType"], expected, "{kind:?}");
            assert_eq!(body["caption"], "look at this", "{kind:?}");
            assert_eq!(body["fileName"], "Quarterly_Report.PNG", "{kind:?}");
            // `send-media` has no reply field: the reference body is chatId/filePath/mediaType plus
            // caption/fileName only (`adapter.py:936-937`).
            assert!(body.get("replyTo").is_none(), "{kind:?}");
            // Exactly the reference's five keys — no client-side extras.
            let mut keys: Vec<&str> = body
                .as_object()
                .map(|object| object.keys().map(String::as_str).collect())
                .unwrap_or_default();
            keys.sort_unstable();
            assert_eq!(
                keys,
                vec!["caption", "chatId", "fileName", "filePath", "mediaType"],
                "{kind:?}"
            );

            let path = body["filePath"].as_str().unwrap_or_default();
            assert!(
                Path::new(path).starts_with(std::env::temp_dir()),
                "staged file must live in the temp dir: {path}"
            );
            assert!(path.ends_with("Quarterly_Report.PNG"), "{kind:?}: {path}");
        }
    }

    #[tokio::test]
    async fn media_to_a_group_uses_the_group_jid_and_omits_absent_optional_keys() {
        let (adapter, mock) = build(vec![sent("wamid-2")]);
        let media = bytes_media(MediaKind::Image, b"png");
        let result = adapter.send_media("group:12345", &media, None).await;
        assert!(result.success);
        let body = media_body(&mock.requests()[0]);
        assert_eq!(body["chatId"], "12345@g.us");
        assert!(body.get("caption").is_none());
        assert!(body.get("fileName").is_none());
        assert_eq!(body.as_object().map(|object| object.len()), Some(3));
    }

    #[tokio::test]
    async fn the_staged_file_exists_while_the_bridge_reads_it_and_is_removed_after_success() {
        let (adapter, mock) = build(vec![sent("wamid-3")]);
        let media = bytes_media(MediaKind::Document, b"%PDF-1.7").with_filename("report.pdf");
        let result = adapter.send_media("1", &media, None).await;
        assert!(result.success);
        let paths = mock.media_paths();
        assert_eq!(paths.len(), 1);
        assert!(
            paths[0].1,
            "the file has to exist while the bridge reads it"
        );
        assert!(paths[0].0.ends_with("report.pdf"), "{}", paths[0].0);
        assert!(
            !Path::new(&paths[0].0).exists(),
            "temp media must be removed after the send: {}",
            paths[0].0
        );
    }

    #[tokio::test]
    async fn the_staged_file_is_removed_after_a_bridge_failure() {
        let (adapter, mock) = build(vec![json_response(500, json!({}))]);
        let media = bytes_media(MediaKind::Image, b"png").with_filename("a.png");
        let result = adapter.send_media("1", &media, None).await;
        assert!(!result.success);
        for (path, existed) in mock.media_paths() {
            assert!(existed, "file missing at request time: {path}");
            assert!(!Path::new(&path).exists(), "leaked temp media: {path}");
        }
    }

    #[tokio::test]
    async fn each_media_send_gets_its_own_temp_file() {
        let (adapter, mock) = build(vec![sent("wamid-4"), sent("wamid-5")]);
        let media = bytes_media(MediaKind::Document, b"%PDF-1.7").with_filename("report.pdf");
        assert!(adapter.send_media("1", &media, None).await.success);
        assert!(adapter.send_media("1", &media, None).await.success);
        let requests = mock.requests();
        let first = media_body(&requests[0])["filePath"]
            .as_str()
            .unwrap_or_default()
            .to_string();
        let second = media_body(&requests[1])["filePath"]
            .as_str()
            .unwrap_or_default()
            .to_string();
        assert_ne!(first, second, "sends must not share a staged file");
        assert!(!Path::new(&first).exists());
        assert!(!Path::new(&second).exists());
    }

    #[tokio::test]
    async fn url_media_is_refused_with_zero_requests_and_names_bytes() {
        let (adapter, mock) = build(vec![]);
        let media = OutboundMedia::image_url("https://example.test/cat.png");
        let result = adapter.send_media("1", &media, None).await;
        assert!(!result.success);
        assert_eq!(result.error_kind.as_deref(), Some("bad_format"));
        assert!(result.message_id.is_none());
        let error = result.error.unwrap_or_default();
        assert!(error.contains("MediaSource::Bytes"), "{error}");
        assert!(error.contains("https://example.test/cat.png"), "{error}");
        assert!(
            mock.requests().is_empty(),
            "a refusal must not touch the wire"
        );
    }

    #[tokio::test]
    async fn empty_bytes_are_refused_before_the_wire() {
        let (adapter, mock) = build(vec![]);
        let result = adapter
            .send_media("1", &bytes_media(MediaKind::Image, b""), None)
            .await;
        assert!(!result.success);
        assert_eq!(result.error_kind.as_deref(), Some("bad_format"));
        assert!(result.message_id.is_none());
        assert!(mock.requests().is_empty());
        assert!(mock.media_paths().is_empty(), "nothing may be staged");
    }

    #[tokio::test]
    async fn oversized_bytes_are_refused_before_the_wire() {
        let (adapter, mock) = build(vec![]);
        let bytes = vec![0u8; WHATSAPP_MAX_IMAGE_BYTES + 1];
        let result = adapter
            .send_media("1", &bytes_media(MediaKind::Image, &bytes), None)
            .await;
        assert!(!result.success);
        assert_eq!(result.error_kind.as_deref(), Some("too_long"));
        let error = result.error.unwrap_or_default();
        assert!(
            error.contains(&WHATSAPP_MAX_IMAGE_BYTES.to_string()),
            "{error}"
        );
        assert!(mock.requests().is_empty());
        assert!(mock.media_paths().is_empty(), "nothing may be staged");
    }

    #[test]
    fn media_caps_match_the_whatsapp_limits() {
        assert_eq!(WHATSAPP_MAX_IMAGE_BYTES, 5 * 1024 * 1024);
        assert_eq!(WHATSAPP_MAX_VIDEO_BYTES, 16 * 1024 * 1024);
        assert_eq!(WHATSAPP_MAX_AUDIO_BYTES, 16 * 1024 * 1024);
        assert_eq!(WHATSAPP_MAX_DOCUMENT_BYTES, 100 * 1024 * 1024);
        assert_eq!(
            media_size_cap(MediaKind::Document),
            WHATSAPP_MAX_DOCUMENT_BYTES
        );
        assert_eq!(media_size_cap(MediaKind::Image), WHATSAPP_MAX_IMAGE_BYTES);
    }

    #[tokio::test]
    async fn a_bridge_answer_without_a_message_id_is_not_a_success() {
        // A bare 200: the bridge said nothing about which message it created.
        let (adapter, mock) = build(vec![json_response(200, json!({}))]);
        let media = bytes_media(MediaKind::Image, b"png").with_filename("a.png");
        let result = adapter.send_media("1", &media, None).await;
        assert!(!result.success);
        assert!(result.message_id.is_none());
        assert!(result.raw_response.is_none());
        assert_eq!(result.error_kind.as_deref(), Some("unknown"));
        for (path, existed) in mock.media_paths() {
            assert!(existed);
            assert!(!Path::new(&path).exists(), "leaked temp media: {path}");
        }

        // An explicit refusal envelope wins even when it carries an id.
        let (adapter, _) = build(vec![json_response(
            200,
            json!({"success": false, "messageId": "m1", "error": "media failed"}),
        )]);
        let result = adapter.send_media("1", &media, None).await;
        assert!(!result.success);
        assert!(result.message_id.is_none());
        assert_eq!(result.error_kind.as_deref(), Some("unknown"));
        assert!(
            result.error.unwrap_or_default().contains("media failed"),
            "the bridge's own words must survive"
        );
    }

    #[tokio::test]
    async fn media_failure_statuses_map_into_the_shared_taxonomy() {
        for (status, kind, retryable) in [
            (429, "rate_limited", true),
            (403, "forbidden", false),
            (404, "not_found", false),
            (413, "too_long", false),
            (400, "bad_format", false),
            (500, "transient", true),
            (503, "transient", true),
        ] {
            let (adapter, mock) = build(vec![HttpResponse {
                status,
                body: "nope".into(),
                retry_after: (status == 429).then_some(7.0),
            }]);
            let media = bytes_media(MediaKind::Image, b"png").with_filename("a.png");
            let result = adapter.send_media("1", &media, None).await;
            assert!(!result.success, "{status} must fail");
            assert_eq!(result.error_kind.as_deref(), Some(kind), "{status}");
            assert_eq!(result.retryable, retryable, "{status}");
            assert_eq!(result.message_id, None, "{status}");
            if status == 429 {
                assert_eq!(result.retry_after, Some(7.0));
            }
            let error = result.error.unwrap_or_default();
            assert!(error.contains(&status.to_string()), "{error}");
            for (path, existed) in mock.media_paths() {
                assert!(existed, "{status}: file missing at request time");
                assert!(!Path::new(&path).exists(), "{status}: leaked {path}");
            }
        }
    }

    #[tokio::test]
    async fn a_transport_error_on_a_media_send_is_retryable_and_cleans_up() {
        // No scripted response: the mock answers with a transport error.
        let (adapter, mock) = build(vec![]);
        let media = bytes_media(MediaKind::Document, b"doc").with_filename("d.pdf");
        let result = adapter.send_media("1", &media, None).await;
        assert!(!result.success);
        assert_eq!(result.error_kind.as_deref(), Some("transient"));
        assert!(result.retryable);
        assert!(
            result.error.unwrap_or_default().contains("transport error"),
            "transport failures must be named"
        );
        for (path, existed) in mock.media_paths() {
            assert!(existed);
            assert!(!Path::new(&path).exists(), "leaked temp media: {path}");
        }
    }

    #[tokio::test]
    async fn a_missing_chat_id_is_refused_before_the_wire() {
        let (adapter, mock) = build(vec![]);
        let result = adapter
            .send_media("  ", &bytes_media(MediaKind::Image, b"png"), None)
            .await;
        assert!(!result.success);
        assert_eq!(result.error_kind.as_deref(), Some("bad_format"));
        assert!(mock.requests().is_empty());
    }

    #[test]
    fn file_names_are_sanitised_into_a_safe_basename() {
        assert_eq!(
            sanitize_media_file_name("a b.png").as_deref(),
            Some("a_b.png")
        );
        assert_eq!(
            sanitize_media_file_name("../../etc/passwd").as_deref(),
            Some("passwd")
        );
        assert_eq!(
            sanitize_media_file_name("dir/sub/na:me?.txt").as_deref(),
            Some("na_me_.txt")
        );
        assert_eq!(sanitize_media_file_name(".."), None);
        assert_eq!(sanitize_media_file_name("   "), None);
        assert_eq!(sanitize_media_file_name(""), None);

        let long = format!("{}.png", "a".repeat(WHATSAPP_MAX_MEDIA_NAME_CHARS + 20));
        let sanitised = sanitize_media_file_name(&long).unwrap_or_default();
        assert!(sanitised.chars().count() <= WHATSAPP_MAX_MEDIA_NAME_CHARS);
        assert!(sanitised.ends_with(".png"), "extension must survive");
    }

    // ----- outbound polls -----

    fn options_of(values: &[&str]) -> Vec<String> {
        values.iter().map(|value| (*value).to_string()).collect()
    }

    /// An adapter with only the required trait surface, i.e. no poll API at all.
    struct TextOnly;

    #[async_trait]
    impl PlatformAdapter for TextOnly {
        async fn connect(&mut self, _is_reconnect: bool) -> bool {
            true
        }
        async fn disconnect(&mut self) {}
        async fn send(
            &self,
            _chat_id: &str,
            _content: &str,
            _reply_to: Option<&str>,
            _metadata: Option<Value>,
        ) -> SendResult {
            SendResult::ok("m1")
        }
        async fn get_chat_info(&self, chat_id: &str) -> ChatInfo {
            ChatInfo {
                name: chat_id.to_string(),
                chat_type: "dm".to_string(),
            }
        }
    }

    #[tokio::test]
    async fn polls_are_supported_and_the_trait_default_still_refuses() {
        let (adapter, _) = build(vec![]);
        assert!(adapter.supports_polls());

        // The trait's default refusal still applies to an adapter without a poll API: a platform
        // cannot claim a poll it has no way to send.
        let text_only = TextOnly;
        assert!(!text_only.supports_polls());
        let result = text_only
            .send_poll("1", "Lunch?", &options_of(&["Pizza", "Sushi"]))
            .await;
        assert!(!result.success);
        assert_eq!(result.error_kind.as_deref(), Some("bad_format"));
        assert!(result.message_id.is_none());
    }

    #[tokio::test]
    async fn poll_posts_the_reference_payload_with_a_jid_and_selectable_count() {
        let (adapter, mock) = build(vec![json_response(200, json!({"messageId": "poll-1"}))]);
        let result = adapter
            .send_poll(
                "15551234567",
                "  Lunch?  ",
                &options_of(&[" Pizza ", "Sushi"]),
            )
            .await;
        assert!(result.success);
        assert_eq!(result.message_id.as_deref(), Some("poll-1"));
        assert!(result.error.is_none());
        assert!(result.error_kind.is_none());
        assert!(!result.retryable);
        // The reference keeps the bridge's whole answer as `raw_response` (adapter.py:891).
        assert_eq!(
            result
                .raw_response
                .as_ref()
                .and_then(|raw| raw.get("messageId"))
                .and_then(Value::as_str),
            Some("poll-1")
        );

        let requests = mock.requests();
        assert_eq!(requests.len(), 1, "one poll is one request");
        assert_eq!(requests[0].method, crate::transport::HttpMethod::Post);
        assert_eq!(requests[0].url, "http://127.0.0.1:3000/send-poll");
        let body = body_of(&requests[0]);
        assert_eq!(body["chatId"], "15551234567@s.whatsapp.net");
        // The question and the options go out stripped, the way the reference's poll caller
        // prepares them (`adapter.py:956-958`).
        assert_eq!(body["question"], "Lunch?");
        assert_eq!(body["options"], json!(["Pizza", "Sushi"]));
        // `selectable_count=1` is the reference's default (`adapter.py:948`).
        assert_eq!(body["selectableCount"], 1);
        // Exactly the reference's four keys — no client-side extras.
        let mut keys: Vec<&str> = body
            .as_object()
            .map(|object| object.keys().map(String::as_str).collect())
            .unwrap_or_default();
        keys.sort_unstable();
        assert_eq!(
            keys,
            vec!["chatId", "options", "question", "selectableCount"]
        );
    }

    #[tokio::test]
    async fn poll_to_a_group_uses_the_group_jid() {
        let (adapter, mock) = build(vec![json_response(200, json!({"messageId": "poll-2"}))]);
        let result = adapter
            .send_poll("group:12345", "Ship it?", &options_of(&["yes", "no"]))
            .await;
        assert!(result.success);
        let body = body_of(&mock.requests()[0]);
        assert_eq!(body["chatId"], "12345@g.us");
    }

    #[tokio::test]
    async fn a_poll_answer_without_a_message_id_is_not_a_success() {
        // A bare 200: the bridge never said which message it created.
        let (adapter, _) = build(vec![json_response(200, json!({}))]);
        let result = adapter
            .send_poll("1", "Lunch?", &options_of(&["Pizza", "Sushi"]))
            .await;
        assert!(!result.success);
        assert!(result.message_id.is_none());
        assert!(result.raw_response.is_none());
        assert_eq!(result.error_kind.as_deref(), Some("unknown"));
        assert!(!result.retryable);
        assert!(
            result
                .error
                .unwrap_or_default()
                .contains("/send-poll without a message id"),
            "the route must be named"
        );

        // An explicit refusal envelope wins even when it carries an id.
        let (adapter, _) = build(vec![json_response(
            200,
            json!({"success": false, "messageId": "poll-3", "error": "poll failed"}),
        )]);
        let result = adapter
            .send_poll("1", "Lunch?", &options_of(&["Pizza", "Sushi"]))
            .await;
        assert!(!result.success);
        assert!(result.message_id.is_none());
        assert_eq!(result.error_kind.as_deref(), Some("unknown"));
        assert!(
            result.error.unwrap_or_default().contains("poll failed"),
            "the bridge's own words must survive"
        );
    }

    #[tokio::test]
    async fn a_blank_question_is_refused_before_the_wire() {
        for question in ["", "   ", "\n\t "] {
            let (adapter, mock) = build(vec![]);
            let result = adapter
                .send_poll("1", question, &options_of(&["Pizza", "Sushi"]))
                .await;
            assert!(!result.success, "{question:?}");
            assert_eq!(
                result.error_kind.as_deref(),
                Some("bad_format"),
                "{question:?}"
            );
            assert!(result.message_id.is_none(), "{question:?}");
            assert!(
                mock.requests().is_empty(),
                "{question:?}: a refusal must not touch the wire"
            );
        }
    }

    #[tokio::test]
    async fn fewer_than_two_usable_options_is_refused_before_the_wire() {
        for options in [
            options_of(&[]),
            options_of(&["only"]),
            // Blank options do not count, exactly as the reference drops them (`adapter.py:956`).
            options_of(&["only", "   "]),
            options_of(&["", ""]),
        ] {
            let (adapter, mock) = build(vec![]);
            let result = adapter.send_poll("1", "Lunch?", &options).await;
            assert!(!result.success, "{options:?}");
            assert_eq!(
                result.error_kind.as_deref(),
                Some("bad_format"),
                "{options:?}"
            );
            assert!(result.message_id.is_none(), "{options:?}");
            assert!(
                mock.requests().is_empty(),
                "{options:?}: a refusal must not touch the wire"
            );
        }
    }

    #[tokio::test]
    async fn option_counts_outside_the_two_to_twelve_window_are_refused() {
        // 12 is the largest allowed count and still goes out.
        let twelve = options_of(&["a", "b", "c", "d", "e", "f", "g", "h", "i", "j", "k", "l"]);
        let (adapter, mock) = build(vec![json_response(200, json!({"messageId": "poll-4"}))]);
        assert!(adapter.send_poll("1", "Pick?", &twelve).await.success);
        assert_eq!(mock.requests().len(), 1);

        // 13 is one too many: refused with zero requests.
        let thirteen = options_of(&[
            "a", "b", "c", "d", "e", "f", "g", "h", "i", "j", "k", "l", "m",
        ]);
        let (adapter, mock) = build(vec![]);
        let result = adapter.send_poll("1", "Pick?", &thirteen).await;
        assert!(!result.success);
        assert_eq!(result.error_kind.as_deref(), Some("bad_format"));
        let error = result.error.unwrap_or_default();
        assert!(
            error.contains(&WHATSAPP_MAX_POLL_OPTIONS.to_string()),
            "{error}"
        );
        assert!(mock.requests().is_empty());
    }

    #[tokio::test]
    async fn over_long_questions_and_options_are_refused_before_the_wire() {
        let (adapter, mock) = build(vec![]);
        let question = "q".repeat(WHATSAPP_MAX_POLL_QUESTION_CHARS + 1);
        let result = adapter
            .send_poll("1", &question, &options_of(&["Pizza", "Sushi"]))
            .await;
        assert!(!result.success);
        assert_eq!(result.error_kind.as_deref(), Some("too_long"));
        let error = result.error.unwrap_or_default();
        assert!(
            error.contains(&WHATSAPP_MAX_POLL_QUESTION_CHARS.to_string()),
            "{error}"
        );
        assert!(mock.requests().is_empty());

        let (adapter, mock) = build(vec![]);
        let long_option = "o".repeat(WHATSAPP_MAX_POLL_OPTION_CHARS + 1);
        let result = adapter
            .send_poll("1", "Pick?", &[long_option, "Sushi".to_string()])
            .await;
        assert!(!result.success);
        assert_eq!(result.error_kind.as_deref(), Some("too_long"));
        let error = result.error.unwrap_or_default();
        assert!(
            error.contains(&WHATSAPP_MAX_POLL_OPTION_CHARS.to_string()),
            "{error}"
        );
        assert!(
            error.chars().count() < 300,
            "the error must not echo a huge option: {error}"
        );
        assert!(mock.requests().is_empty());

        // Both caps are inclusive: a question of exactly 255 characters and options of exactly 100
        // characters still go out.
        let question = "q".repeat(WHATSAPP_MAX_POLL_QUESTION_CHARS);
        let at_cap = vec![
            "o".repeat(WHATSAPP_MAX_POLL_OPTION_CHARS),
            "Sushi".to_string(),
        ];
        let (adapter, mock) = build(vec![json_response(200, json!({"messageId": "poll-5"}))]);
        let result = adapter.send_poll("1", &question, &at_cap).await;
        assert!(result.success);
        assert_eq!(mock.requests().len(), 1);
    }

    #[tokio::test]
    async fn a_missing_chat_id_is_refused_for_polls_too() {
        let (adapter, mock) = build(vec![]);
        let result = adapter
            .send_poll("  ", "Lunch?", &options_of(&["Pizza", "Sushi"]))
            .await;
        assert!(!result.success);
        assert_eq!(result.error_kind.as_deref(), Some("bad_format"));
        assert!(mock.requests().is_empty());
    }

    #[tokio::test]
    async fn poll_failure_statuses_map_into_the_shared_taxonomy() {
        for (status, kind, retryable) in [
            (429, "rate_limited", true),
            (403, "forbidden", false),
            (404, "not_found", false),
            (413, "too_long", false),
            (400, "bad_format", false),
            (500, "transient", true),
            (503, "transient", true),
        ] {
            let (adapter, _) = build(vec![HttpResponse {
                status,
                body: "nope".into(),
                retry_after: (status == 429).then_some(7.0),
            }]);
            let result = adapter
                .send_poll("1", "Lunch?", &options_of(&["Pizza", "Sushi"]))
                .await;
            assert!(!result.success, "{status} must fail");
            assert_eq!(result.error_kind.as_deref(), Some(kind), "{status}");
            assert_eq!(result.retryable, retryable, "{status}");
            assert_eq!(result.message_id, None, "{status}");
            if status == 429 {
                assert_eq!(result.retry_after, Some(7.0));
            }
            let error = result.error.unwrap_or_default();
            assert!(error.contains(&status.to_string()), "{error}");
        }
    }

    #[tokio::test]
    async fn a_transport_error_on_a_poll_is_retryable() {
        // No scripted response: the mock answers with a transport error.
        let (adapter, _) = build(vec![]);
        let result = adapter
            .send_poll("1", "Lunch?", &options_of(&["Pizza", "Sushi"]))
            .await;
        assert!(!result.success);
        assert_eq!(result.error_kind.as_deref(), Some("transient"));
        assert!(result.retryable);
        assert!(result.message_id.is_none());
        assert!(
            result.error.unwrap_or_default().contains("transport error"),
            "transport failures must be named"
        );
    }

    #[test]
    fn poll_limits_match_whatsapp_and_the_reference_default() {
        assert_eq!(WHATSAPP_MIN_POLL_OPTIONS, 2);
        assert_eq!(WHATSAPP_MAX_POLL_OPTIONS, 12);
        assert_eq!(WHATSAPP_MAX_POLL_QUESTION_CHARS, 255);
        assert_eq!(WHATSAPP_MAX_POLL_OPTION_CHARS, 100);
        // `selectable_count: int = 1` (adapter.py:948).
        assert_eq!(WHATSAPP_POLL_SELECTABLE_COUNT, 1);
    }
}
