//! WeCom (Enterprise WeChat) adapter — the `openws` AI-bot gateway.
//!
//! The bot holds one WebSocket to `wss://openws.work.weixin.qq.com`. It subscribes with
//! `aibot_subscribe`, receives `aibot_msg_callback` frames, answers inside the same stream with
//! `aibot_respond_msg` (or starts a new message with `aibot_send_msg` when there is no inbound
//! `req_id`). The socket is injected as a [`LineSocket`](crate::socket::LineSocket); production
//! uses [`WsSocket`](crate::socket::WsSocket), which now carries `wss://`.
//!
//! Outbound media follows `research/hermes-platforms/wecom/media.py`: one
//! `aibot_upload_media_init`, then one `aibot_upload_media_chunk` per 512 KiB of payload
//! (0-based `chunk_index`, standard base64 body), then `aibot_upload_media_finish`; the returned
//! `media_id` rides an ordinary `aibot_send_msg` / `aibot_respond_msg` body of
//! `{msgtype, <msgtype>: {media_id}}`. Every phase is a request/response round trip over the same
//! socket (`_checked_request` in `adapter.py`) with the reference's 15 s deadline, and a failure at
//! any phase stops the flow before the next frame goes out.
//!
//! Not ported: streaming frames (`aibot_respond_msg` with `finish=false`), AI cards, reactions,
//! dedup, the per-chat send queue and reconnection. URL media sources are refused: the reference
//! downloads them through an SSRF-guarded HTTP client and re-uploads the bytes, and this adapter
//! has no HTTP client — [`PlatformAdapter::send_image`] therefore falls back to the reference's own
//! text send for a URL.

use async_trait::async_trait;
use serde_json::{Value, json};
use std::collections::{HashMap, VecDeque};
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use crate::event::{MessageEvent, Platform, SessionSource};
use crate::helpers::{codepoint_len, truncate_message};
use crate::platform::{
    ChatInfo, MediaKind, MediaSource, OutboundMedia, PlatformAdapter, SendResult,
};
use crate::socket::SharedLineSocket;

pub const WECOM_MAX_MESSAGE_LENGTH: usize = 4000;
pub const WECOM_WS_URL: &str = "wss://openws.work.weixin.qq.com";

const CMD_SUBSCRIBE: &str = "aibot_subscribe";
const CMD_CALLBACK: &str = "aibot_msg_callback";
const CMD_LEGACY_CALLBACK: &str = "aibot_callback";
const CMD_EVENT_CALLBACK: &str = "aibot_event_callback";
const CMD_SEND: &str = "aibot_send_msg";
const CMD_RESPOND: &str = "aibot_respond_msg";
const CMD_PING: &str = "ping";

// ----- media upload protocol (media.py) -----

/// Phase one: announce the upload (`_upload_media_bytes`).
const CMD_UPLOAD_INIT: &str = "aibot_upload_media_init";
/// Phase two: one frame per chunk, repeated until the whole payload is on the gateway.
const CMD_UPLOAD_CHUNK: &str = "aibot_upload_media_chunk";
/// Phase three: close the upload and take the `media_id`.
const CMD_UPLOAD_FINISH: &str = "aibot_upload_media_finish";

/// `UPLOAD_CHUNK_SIZE` — 512 KiB of raw bytes per chunk frame.
pub const WECOM_UPLOAD_CHUNK_SIZE: usize = 512 * 1024;
/// `MAX_UPLOAD_CHUNKS` — the gateway refuses more chunks than this.
pub const WECOM_MAX_UPLOAD_CHUNKS: usize = 100;
/// `IMAGE_MAX_BYTES`.
pub const WECOM_IMAGE_MAX_BYTES: usize = 10 * 1024 * 1024;
/// `VIDEO_MAX_BYTES`.
pub const WECOM_VIDEO_MAX_BYTES: usize = 10 * 1024 * 1024;
/// `VOICE_MAX_BYTES`.
pub const WECOM_VOICE_MAX_BYTES: usize = 2 * 1024 * 1024;
/// `FILE_MAX_BYTES`.
pub const WECOM_FILE_MAX_BYTES: usize = 20 * 1024 * 1024;
/// `ABSOLUTE_MAX_BYTES` — one upload can never exceed this.
pub const WECOM_ABSOLUTE_MAX_BYTES: usize = WECOM_FILE_MAX_BYTES;
/// `VOICE_SUPPORTED_MIMES` — the only audio the gateway plays as a voice message.
pub const WECOM_VOICE_SUPPORTED_MIMES: &[&str] = &["audio/amr"];
/// `REQUEST_TIMEOUT_SECONDS` — the deadline for one command round trip.
pub const WECOM_REQUEST_TIMEOUT_SECS: f64 = 15.0;

/// How many `_reply_req_ids` entries the reference keeps (`DEDUP_MAX_SIZE`).
const REPLY_REQ_ID_CACHE_SIZE: usize = 1000;

static REQ_SEQ: AtomicU64 = AtomicU64::new(0);

pub struct WeComAdapter {
    socket: SharedLineSocket,
    bot_id: String,
    secret: String,
    device_id: String,
    /// Last inbound `req_id` per chat. WeCom groups only accept passive replies.
    last_req_ids: Mutex<HashMap<String, String>>,
    /// Inbound message id → its `req_id` (reference `_reply_req_ids`), so `send_media` can answer a
    /// specific message in-stream. Bounded to [`REPLY_REQ_ID_CACHE_SIZE`], newest first.
    message_req_ids: Mutex<Vec<(String, String)>>,
    /// Frames read while a media request owned the socket reader, handed back to [`Self::next_event`]
    /// so an inbound message is never swallowed by an upload.
    pending_inbound: Mutex<VecDeque<String>>,
}

/// The WeCom media type that goes on the wire as the upload `type` and the message `msgtype`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum WeComMediaType {
    Image,
    Video,
    Voice,
    File,
}

impl WeComMediaType {
    fn as_str(self) -> &'static str {
        match self {
            Self::Image => "image",
            Self::Video => "video",
            Self::Voice => "voice",
            Self::File => "file",
        }
    }

    /// `_TYPE_LIMITS`: the per-type upload cap, plus the label and human cap the downgrade notice
    /// uses. `file` has no separate cap (the absolute cap covers it).
    fn type_limit(self) -> Option<(usize, &'static str, &'static str)> {
        match self {
            Self::Image => Some((WECOM_IMAGE_MAX_BYTES, "image", "10MB")),
            Self::Video => Some((WECOM_VIDEO_MAX_BYTES, "video", "10MB")),
            Self::Voice => Some((WECOM_VOICE_MAX_BYTES, "voice", "2MB")),
            Self::File => None,
        }
    }
}

/// `_apply_file_size_limits` verdict: the type that will actually be sent, the rejection reason
/// when the payload cannot go at all, and the notice when the type had to be downgraded.
#[derive(Debug, Clone, PartialEq, Eq)]
struct SizeVerdict {
    final_type: WeComMediaType,
    rejected: Option<String>,
    downgrade_note: Option<String>,
}

/// `_prepare_outbound_media` result: the bytes plus everything the wire frames need.
struct PreparedMedia<'a> {
    data: &'a [u8],
    filename: String,
    media_type: WeComMediaType,
    rejected: Option<String>,
    downgrade_note: Option<String>,
}

/// What `aibot_upload_media_finish` handed back.
struct UploadedMedia {
    /// `body.type` from the finish response, else the type that was uploaded (the reference keeps
    /// whatever the gateway calls it and uses it as the message `msgtype`).
    media_type: String,
    media_id: String,
    created_at: Option<Value>,
}

/// A media send failure, typed so [`PlatformAdapter::send_media`] can report a
/// [`crate::platform::SEND_ERROR_KINDS`] kind plus retryability instead of raw provider text.
#[derive(Debug, Clone, PartialEq, Eq)]
enum MediaError {
    /// The frame never reached the gateway: the socket refused it or closed mid-request.
    Transport(String),
    /// No correlated response frame arrived inside [`WECOM_REQUEST_TIMEOUT_SECS`].
    Timeout(String),
    /// The gateway answered with a non-zero `errcode`.
    Refused {
        operation: String,
        errcode: i64,
        errmsg: String,
    },
    /// The gateway accepted the frame but the response carried no `upload_id` / `media_id`, so the
    /// flow cannot continue — the reference raises here too, which is why it never fakes a send.
    Malformed(String),
    /// Refused locally, before any frame went out.
    Rejected { message: String, kind: &'static str },
}

impl MediaError {
    /// Failure code as a [`SendResult`] carrying the shared taxonomy.
    fn into_result(self) -> SendResult {
        match self {
            Self::Transport(message) | Self::Timeout(message) | Self::Malformed(message) => {
                SendResult::failed(message, Some("transient")).retryable(None)
            }
            Self::Refused {
                operation,
                errcode,
                errmsg,
            } => {
                let kind = errcode_kind(errcode);
                let result = SendResult::failed(
                    format!("{operation} failed: WeCom errcode {errcode}: {errmsg}"),
                    Some(kind),
                );
                if is_retryable_kind(kind) {
                    result.retryable(None)
                } else {
                    result
                }
            }
            Self::Rejected { message, kind } => SendResult::failed(message, Some(kind)),
        }
    }
}

impl WeComAdapter {
    pub fn new(
        socket: SharedLineSocket,
        bot_id: impl Into<String>,
        secret: impl Into<String>,
        device_id: impl Into<String>,
    ) -> Self {
        Self {
            socket: Arc::clone(&socket),
            bot_id: bot_id.into(),
            secret: secret.into(),
            device_id: device_id.into(),
            last_req_ids: Mutex::new(HashMap::new()),
            message_req_ids: Mutex::new(Vec::new()),
            pending_inbound: Mutex::new(VecDeque::new()),
        }
    }

    fn req_id(prefix: &str) -> String {
        format!(
            "{prefix}-{}-{}",
            std::process::id(),
            REQ_SEQ.fetch_add(1, Ordering::Relaxed)
        )
    }

    async fn send_json(&self, value: &Value) -> Result<(), String> {
        let line = serde_json::to_string(value).map_err(|err| err.to_string())?;
        self.socket
            .send_line(&line)
            .await
            .map_err(|err| err.to_string())
    }

    /// Remember the inbound `req_id` so a later reply to this chat can answer in-stream.
    pub fn remember_req_id(&self, chat_id: &str, req_id: &str) {
        if let Ok(mut map) = self.last_req_ids.lock() {
            map.insert(chat_id.to_string(), req_id.to_string());
        }
    }

    fn cached_req_id(&self, chat_id: &str) -> Option<String> {
        self.last_req_ids
            .lock()
            .ok()
            .and_then(|map| map.get(chat_id).cloned())
    }

    /// Remember `message id → req_id` so `send_media(reply_to = Some(message_id))` can answer that
    /// message in-stream. The reference bounds this cache (`bounded_put`); the oldest entry goes
    /// first here.
    fn remember_message_req_id(&self, message_id: &str, req_id: &str) {
        if message_id.is_empty() || req_id.is_empty() {
            return;
        }
        let Ok(mut cache) = self.message_req_ids.lock() else {
            return;
        };
        cache.retain(|(cached, _)| cached != message_id);
        cache.insert(0, (message_id.to_string(), req_id.to_string()));
        cache.truncate(REPLY_REQ_ID_CACHE_SIZE);
    }

    /// `_reply_req_id_for_message`: a `quote:`-prefixed target is not a message id (the reference
    /// treats it as a quote request), so it never resolves to a `req_id`.
    fn reply_req_id_for_message(&self, reply_to: Option<&str>) -> Option<String> {
        let reply_to = reply_to?.trim();
        if reply_to.is_empty() || reply_to.starts_with("quote:") {
            return None;
        }
        self.message_req_ids.lock().ok().and_then(|cache| {
            cache
                .iter()
                .find(|(message_id, _)| message_id == reply_to)
                .map(|(_, req_id)| req_id.clone())
        })
    }

    /// `_cached_reply_req_id`: an explicit `reply_to` mapping first, else the chat's last inbound
    /// `req_id`. The reference additionally drops the `req_id` when a live or expired stream owns
    /// it; this adapter has no stream registry, so the cached id is always usable.
    fn cached_reply_req_id(&self, chat_id: &str, reply_to: Option<&str>) -> Option<String> {
        self.reply_req_id_for_message(reply_to)
            .or_else(|| self.cached_req_id(chat_id))
    }

    /// A frame that arrived while a request owned the reader; queued for [`Self::next_event`].
    fn stash_inbound(&self, line: String) {
        if let Ok(mut queue) = self.pending_inbound.lock()
            && queue.len() < 64
        {
            queue.push_back(line);
        }
    }

    fn take_stashed_inbound(&self) -> Option<String> {
        self.pending_inbound
            .lock()
            .ok()
            .and_then(|mut queue| queue.pop_front())
    }

    /// Read the next inbound callback. Pings, acks and unknown frames are skipped.
    pub async fn next_event(&self) -> Result<Option<MessageEvent>, String> {
        loop {
            let line = match self.take_stashed_inbound() {
                Some(line) => line,
                None => match self
                    .socket
                    .next_line()
                    .await
                    .map_err(|err| err.to_string())?
                {
                    Some(line) => line,
                    None => return Ok(None),
                },
            };
            let Ok(payload) = serde_json::from_str::<Value>(&line) else {
                continue;
            };
            let cmd = payload.get("cmd").and_then(Value::as_str).unwrap_or("");
            if cmd == CMD_PING || cmd.is_empty() {
                continue;
            }
            if cmd != CMD_CALLBACK && cmd != CMD_LEGACY_CALLBACK {
                continue;
            }
            let req_id = payload
                .get("headers")
                .and_then(|headers| headers.get("req_id"))
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string();
            let Some(body) = payload.get("body") else {
                continue;
            };
            let sender = body.get("from").and_then(|from| from.get("userid"));
            let sender_id = sender.and_then(Value::as_str).unwrap_or_default();
            let chat_id = body
                .get("chatid")
                .and_then(Value::as_str)
                .filter(|value| !value.is_empty())
                .unwrap_or(sender_id)
                .to_string();
            if chat_id.is_empty() {
                continue;
            }
            let is_group = body.get("chattype").and_then(Value::as_str) == Some("group");
            let mut text = body
                .get("text")
                .and_then(|text| text.get("content"))
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string();
            if is_group {
                // "(@Bot) /approve" → "/approve"
                if let Some(rest) = text.strip_prefix('@')
                    && let Some(space) = rest.find(char::is_whitespace)
                {
                    text = rest[space..].trim_start().to_string();
                }
            }
            if text.trim().is_empty() {
                continue;
            }
            if !req_id.is_empty() {
                self.remember_req_id(&chat_id, &req_id);
            }

            let msg_id = body
                .get("msgid")
                .and_then(Value::as_str)
                .unwrap_or(&req_id)
                .to_string();
            self.remember_message_req_id(&msg_id, &req_id);
            let platform = Platform::new("wecom").expect("valid platform");
            let mut source = SessionSource::new(platform, chat_id.clone());
            source.chat_type = if is_group { "group" } else { "dm" }.to_string();
            source.user_id = Some(sender_id.to_string());
            source.user_name = Some(sender_id.to_string());
            source.message_id = Some(msg_id.clone());

            let mut event = MessageEvent::new(text, source);
            event.user_id = Some(sender_id.to_string());
            event.user_name = Some(sender_id.to_string());
            event.message_id = Some(msg_id);
            event.raw_message = Some(payload);
            event.metadata.insert("req_id".to_string(), json!(req_id));
            event.metadata.insert(
                "chat_type".to_string(),
                json!(if is_group { "group" } else { "dm" }),
            );
            return Ok(Some(event));
        }
    }

    fn markdown_body(content: &str) -> Value {
        json!({ "msgtype": "markdown", "markdown": { "content": content } })
    }

    // ----- outbound media (media.py) -----

    /// One command round trip: send the frame, then wait for the frame whose `headers.req_id`
    /// matches (`adapter.py::_request`). Pings are skipped; inbound callbacks that arrive while the
    /// request owns the reader are queued for [`Self::next_event`] instead of being dropped.
    ///
    /// The deadline is `REQUEST_TIMEOUT_SECONDS` per phase, exactly like the reference: a gateway
    /// that never answers fails the send rather than hanging it.
    async fn request_frame(
        &self,
        cmd: &str,
        req_id: &str,
        body: Value,
    ) -> Result<Value, MediaError> {
        let payload = json!({
            "cmd": cmd,
            "headers": { "req_id": req_id },
            "body": body,
        });
        self.send_json(&payload)
            .await
            .map_err(|err| MediaError::Transport(format!("wecom {cmd} failed: {err}")))?;
        let deadline =
            tokio::time::Instant::now() + Duration::from_secs_f64(WECOM_REQUEST_TIMEOUT_SECS);
        loop {
            let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
            if remaining.is_zero() {
                return Err(MediaError::Timeout(format!(
                    "wecom {cmd} timed out after {WECOM_REQUEST_TIMEOUT_SECS}s"
                )));
            }
            let frame = match tokio::time::timeout(remaining, self.socket.next_line()).await {
                Err(_) => {
                    return Err(MediaError::Timeout(format!(
                        "wecom {cmd} timed out after {WECOM_REQUEST_TIMEOUT_SECS}s"
                    )));
                }
                Ok(Err(err)) => {
                    return Err(MediaError::Transport(format!("wecom {cmd} failed: {err}")));
                }
                Ok(Ok(None)) => {
                    return Err(MediaError::Transport(format!(
                        "wecom socket closed while waiting for {cmd}"
                    )));
                }
                Ok(Ok(Some(line))) => line,
            };
            let Ok(payload) = serde_json::from_str::<Value>(&frame) else {
                continue;
            };
            let frame_cmd = payload.get("cmd").and_then(Value::as_str).unwrap_or("");
            if frame_cmd == CMD_PING {
                continue;
            }
            if response_req_id(&payload) == Some(req_id) {
                return Ok(payload);
            }
            if matches!(
                frame_cmd,
                CMD_CALLBACK | CMD_LEGACY_CALLBACK | CMD_EVENT_CALLBACK
            ) {
                self.stash_inbound(frame);
            }
        }
    }

    /// A round trip whose `req_id` the adapter mints, as `_send_request` does.
    async fn request(&self, cmd: &str, body: Value) -> Result<Value, MediaError> {
        let req_id = Self::req_id(cmd);
        self.request_frame(cmd, &req_id, body).await
    }

    /// The reference's three-phase upload: `init` → one `chunk` per 512 KiB → `finish`.
    ///
    /// Field names, chunking and ordering come straight from `_upload_media_bytes`:
    /// `{type, filename, total_size, total_chunks, md5}`, then
    /// `{upload_id, chunk_index, base64_data}` with a 0-based `chunk_index` (the official SDK's
    /// numbering, as the reference notes), then `{upload_id}`. A failed phase returns early, so no
    /// later frame is emitted.
    async fn upload_media_bytes(
        &self,
        data: &[u8],
        media_type: WeComMediaType,
        filename: &str,
    ) -> Result<UploadedMedia, MediaError> {
        if data.is_empty() {
            return Err(MediaError::Rejected {
                message: "cannot upload empty media".to_string(),
                kind: "bad_format",
            });
        }
        let total_size = data.len();
        let total_chunks = total_size.div_ceil(WECOM_UPLOAD_CHUNK_SIZE);
        if total_chunks > WECOM_MAX_UPLOAD_CHUNKS {
            return Err(MediaError::Rejected {
                message: format!(
                    "file too large: {total_chunks} chunks exceeds WeCom's maximum of {WECOM_MAX_UPLOAD_CHUNKS} chunks"
                ),
                kind: "too_long",
            });
        }

        let init = self
            .request(
                CMD_UPLOAD_INIT,
                json!({
                    "type": media_type.as_str(),
                    "filename": filename,
                    "total_size": total_size,
                    "total_chunks": total_chunks,
                    "md5": md5_hex(data),
                }),
            )
            .await?;
        ensure_accepted(&init, "media upload init")?;
        let Some(upload_id) = body_text(&init, "upload_id") else {
            return Err(MediaError::Malformed(format!(
                "media upload init returned no upload_id: {}",
                summarise(&init)
            )));
        };
        let upload_id = upload_id.to_string();

        for (chunk_index, chunk) in data.chunks(WECOM_UPLOAD_CHUNK_SIZE).enumerate() {
            let ack = self
                .request(
                    CMD_UPLOAD_CHUNK,
                    json!({
                        "upload_id": &upload_id,
                        "chunk_index": chunk_index,
                        "base64_data": base64_encode(chunk),
                    }),
                )
                .await?;
            ensure_accepted(&ack, &format!("media upload chunk {chunk_index}"))?;
        }

        let finish = self
            .request(CMD_UPLOAD_FINISH, json!({ "upload_id": &upload_id }))
            .await?;
        ensure_accepted(&finish, "media upload finish")?;
        let Some(media_id) = body_text(&finish, "media_id") else {
            return Err(MediaError::Malformed(format!(
                "media upload finish returned no media_id: {}",
                summarise(&finish)
            )));
        };
        Ok(UploadedMedia {
            media_type: body_text(&finish, "type")
                .map(str::to_string)
                .unwrap_or_else(|| media_type.as_str().to_string()),
            media_id: media_id.to_string(),
            created_at: finish
                .get("body")
                .and_then(|body| body.get("created_at"))
                .cloned(),
        })
    }

    /// The reference's `_media_body` in both shapes: the passive reply carries the inbound `req_id`
    /// and no `chatid`, the proactive one is `aibot_send_msg` with `chatid`.
    async fn send_media_message(
        &self,
        chat_id: &str,
        reply_req_id: Option<&str>,
        media_type: &str,
        media_id: &str,
    ) -> Result<Value, MediaError> {
        let mut body = serde_json::Map::new();
        body.insert("msgtype".to_string(), json!(media_type));
        body.insert(media_type.to_string(), json!({ "media_id": media_id }));
        match reply_req_id {
            Some(req_id) => {
                let response = self
                    .request_frame(CMD_RESPOND, req_id, Value::Object(body))
                    .await?;
                ensure_accepted(&response, "send reply media message")?;
                Ok(response)
            }
            None => {
                body.insert("chatid".to_string(), json!(chat_id));
                let response = self.request(CMD_SEND, Value::Object(body)).await?;
                ensure_accepted(&response, "send media message")?;
                Ok(response)
            }
        }
    }

    /// `_send_media_source`: prepare, cap, upload, send, then the caption (and any downgrade
    /// notice) as follow-up markdown — WeCom media messages carry no caption field, which is why
    /// the reference splits it out.
    async fn deliver_media(
        &self,
        chat_id: &str,
        media: &OutboundMedia,
        reply_to: Option<&str>,
    ) -> Result<SendResult, MediaError> {
        let bytes = match &media.source {
            MediaSource::Bytes(bytes) => bytes.as_slice(),
            MediaSource::Url(url) => {
                return Err(MediaError::Rejected {
                    message: format!(
                        "wecom cannot send media by URL ({url}): the reference downloads the file \
                         and uploads the bytes, and this adapter has no HTTP client — hand over \
                         the bytes instead"
                    ),
                    kind: "bad_format",
                });
            }
        };
        let prepared = prepare_outbound_media(
            media.kind,
            bytes,
            media.filename.as_deref(),
            media.mime.as_deref(),
        );
        if let Some(reason) = prepared.rejected.as_deref() {
            // the reference sends the rejection as follow-up markdown (caption included) first
            let notice = join_notice(&format!("⚠️ {reason}"), media.caption.as_deref());
            if !notice.is_empty() {
                let _ = self.send(chat_id, &notice, reply_to, None).await;
            }
            return Err(MediaError::Rejected {
                message: reason.to_string(),
                kind: "too_long",
            });
        }

        let reply_req_id = self.cached_reply_req_id(chat_id, reply_to);
        let uploaded = self
            .upload_media_bytes(prepared.data, prepared.media_type, &prepared.filename)
            .await?;
        let response = self
            .send_media_message(
                chat_id,
                reply_req_id.as_deref(),
                &uploaded.media_type,
                &uploaded.media_id,
            )
            .await?;

        // `_payload_req_id(media_response) or uuid4().hex[:12]`: the gateway's req_id when the
        // response carries one, else a fresh opaque id.
        let message_id = response_req_id(&response)
            .filter(|req_id| !req_id.is_empty())
            .map(str::to_string)
            .unwrap_or_else(|| Self::req_id("media"));

        let mut raw = serde_json::Map::new();
        raw.insert(
            "upload".to_string(),
            json!({
                "type": uploaded.media_type,
                "media_id": uploaded.media_id,
                "created_at": uploaded.created_at,
            }),
        );
        raw.insert("media".to_string(), response);

        let caption_result = match media
            .caption
            .as_deref()
            .map(str::trim)
            .filter(|caption| !caption.is_empty())
        {
            Some(caption) => Some(self.send(chat_id, caption, reply_to, None).await),
            None => None,
        };
        raw.insert(
            "caption".to_string(),
            caption_result
                .as_ref()
                .and_then(|result| result.raw_response.clone())
                .unwrap_or(Value::Null),
        );
        raw.insert(
            "caption_error".to_string(),
            match &caption_result {
                Some(result) => result.error.clone().map_or(Value::Null, Value::String),
                None => Value::Null,
            },
        );

        let downgrade_result = match prepared
            .downgrade_note
            .as_deref()
            .filter(|note| !note.is_empty())
        {
            Some(note) => Some(
                self.send(chat_id, &format!("ℹ️ {note}"), reply_to, None)
                    .await,
            ),
            None => None,
        };
        raw.insert(
            "downgrade".to_string(),
            downgrade_result
                .as_ref()
                .and_then(|result| result.raw_response.clone())
                .unwrap_or(Value::Null),
        );
        raw.insert(
            "downgrade_error".to_string(),
            match &downgrade_result {
                Some(result) => result.error.clone().map_or(Value::Null, Value::String),
                None => Value::Null,
            },
        );

        let mut result = SendResult::ok(message_id);
        result.raw_response = Some(Value::Object(raw));
        Ok(result)
    }
}

#[async_trait]
impl PlatformAdapter for WeComAdapter {
    async fn connect(&mut self, _is_reconnect: bool) -> bool {
        let payload = json!({
            "cmd": CMD_SUBSCRIBE,
            "headers": { "req_id": Self::req_id("subscribe") },
            "body": {
                "bot_id": self.bot_id,
                "secret": self.secret,
                "device_id": self.device_id,
            },
        });
        self.send_json(&payload).await.is_ok()
    }

    async fn disconnect(&mut self) {}

    async fn send(
        &self,
        chat_id: &str,
        content: &str,
        _reply_to: Option<&str>,
        metadata: Option<Value>,
    ) -> SendResult {
        if chat_id.trim().is_empty() {
            return SendResult::failed("wecom needs a chat id", Some("bad_format"));
        }
        if content.trim().is_empty() {
            return SendResult::failed("empty message content", Some("bad_format"));
        }
        // A caller may pass the inbound req_id explicitly; otherwise the cached one for this chat
        // is used, which is what a group requires.
        let reply_req_id = metadata
            .as_ref()
            .and_then(|meta| meta.get("reply_req_id"))
            .and_then(Value::as_str)
            .map(str::to_owned)
            .or_else(|| self.cached_req_id(chat_id));

        let chunks = truncate_message(content, WECOM_MAX_MESSAGE_LENGTH, codepoint_len);
        let mut ids = Vec::new();
        for chunk in chunks {
            let (cmd, req_id, body) = match &reply_req_id {
                Some(req_id) => (CMD_RESPOND, req_id.clone(), Self::markdown_body(&chunk)),
                None => (CMD_SEND, Self::req_id("send"), {
                    let mut body = Self::markdown_body(&chunk);
                    body["chatid"] = json!(chat_id);
                    body
                }),
            };
            let payload = json!({
                "cmd": cmd,
                "headers": { "req_id": req_id },
                "body": body,
            });
            if let Err(err) = self.send_json(&payload).await {
                return SendResult::failed(format!("wecom send failed: {err}"), Some("transient"))
                    .retryable(None);
            }
            ids.push(req_id);
        }
        match ids.last() {
            Some(last) => SendResult::ok_split(last.clone(), ids),
            None => SendResult::ok(""),
        }
    }

    fn splits_long_messages(&self) -> bool {
        true
    }

    fn max_message_length(&self) -> usize {
        WECOM_MAX_MESSAGE_LENGTH
    }

    async fn get_chat_info(&self, chat_id: &str) -> ChatInfo {
        ChatInfo {
            name: chat_id.to_string(),
            chat_type: if chat_id.to_lowercase().starts_with("group") {
                "group".to_string()
            } else {
                "dm".to_string()
            },
        }
    }

    async fn send_typing(&self, _chat_id: &str, _metadata: Option<Value>) {}

    /// Uploads are implemented, so the router may hand media to this adapter.
    fn supports_media(&self) -> bool {
        true
    }

    /// Send one attachment through the reference's upload protocol.
    ///
    /// The frame sequence (media.py `_upload_media_bytes` + `_send_media_message`), each phase a
    /// correlated round trip with a 15 s deadline and an `errcode` check:
    /// 1. `aibot_upload_media_init` — `{type, filename, total_size, total_chunks, md5}`;
    /// 2. `aibot_upload_media_chunk` once per 512 KiB —
    ///    `{upload_id, chunk_index, base64_data}`, `chunk_index` starting at 0;
    /// 3. `aibot_upload_media_finish` — `{upload_id}` → `body.media_id` (`body.type` renames the
    ///    type when the gateway says so);
    /// 4. `aibot_respond_msg` with the inbound `req_id` when one is known, else `aibot_send_msg`
    ///    with `chatid`; both bodies are `{msgtype, <msgtype>: {media_id}}`.
    ///
    /// Any failing phase ends the send with a typed failure and no further frame, so a partial
    /// upload can never be mistaken for a delivered message. Empty payloads are refused, the
    /// reference's caps are enforced (image/video 10 MB, voice 2 MB, file 20 MB absolute, 100
    /// chunks) and an over-cap image/video/voice is downgraded to `file` with the reference's
    /// notice. The caption travels as a follow-up markdown message; the kind comes from
    /// [`MediaKind`] plus the filename/MIME, and `reply_to` resolves through the message-id cache
    /// (`quote:` targets deliberately do not). A URL source is refused: uploading one needs an
    /// HTTP client this adapter does not have.
    async fn send_media(
        &self,
        chat_id: &str,
        media: &OutboundMedia,
        reply_to: Option<&str>,
    ) -> SendResult {
        if chat_id.trim().is_empty() {
            return SendResult::failed("wecom needs a chat id", Some("bad_format"));
        }
        match self.deliver_media(chat_id, media, reply_to).await {
            Ok(result) => result,
            Err(err) => err.into_result(),
        }
    }

    /// `send_image` with the reference's fallback: try the media path, then send the URL as text
    /// (`WeComMediaMixin.send_image`). Since URL sources are refused above, a URL ends up as a
    /// plain text send carrying the link plus the caption — exactly the reference's own fallback.
    async fn send_image(
        &self,
        chat_id: &str,
        url: &str,
        caption: Option<&str>,
        _metadata: Option<Value>,
    ) -> SendResult {
        let mut media = OutboundMedia::image_url(url);
        if let Some(caption) = caption {
            media = media.with_caption(caption);
        }
        let result = self.send_media(chat_id, &media, None).await;
        if result.success {
            return result;
        }
        let text = match caption {
            Some(caption) => format!("{caption}\n{url}"),
            None => url.to_string(),
        };
        self.send(chat_id, &text, None, None).await
    }
}

// ----- protocol helpers -----

/// The `req_id` a frame carries, if any (`_payload_req_id`).
fn response_req_id(payload: &Value) -> Option<&str> {
    payload
        .get("headers")?
        .get("req_id")?
        .as_str()
        .map(str::trim)
        .filter(|req_id| !req_id.is_empty())
}

/// A `body` field as trimmed text, treating an empty string as absent (the reference's
/// `str(... or "")` + `strip()`).
fn body_text<'a>(payload: &'a Value, key: &str) -> Option<&'a str> {
    payload
        .get("body")?
        .get(key)?
        .as_str()
        .map(str::trim)
        .filter(|value| !value.is_empty())
}

/// Raise when a response reports a non-zero `errcode` (`_checked_request`).
fn ensure_accepted(payload: &Value, operation: &str) -> Result<(), MediaError> {
    match response_errcode(payload) {
        None => Ok(()),
        Some((errcode, errmsg)) => Err(MediaError::Refused {
            operation: operation.to_string(),
            errcode,
            errmsg,
        }),
    }
}

/// The failure code a response frame reports; `None` means success (errcode 0 or absent).
///
/// `_response_error` reads the top-level `errcode`. When the top level carries none the port also
/// looks inside `body`, which can only turn a would-be success into a truthful failure — never the
/// reverse.
fn response_errcode(payload: &Value) -> Option<(i64, String)> {
    if payload.get("errcode").is_some() {
        return errcode_of(payload);
    }
    payload.get("body").and_then(errcode_of)
}

/// `(errcode, errmsg)` from one object, or `None` when the code is 0 or unusable.
fn errcode_of(container: &Value) -> Option<(i64, String)> {
    let raw = container.get("errcode")?;
    let errcode = raw
        .as_i64()
        .or_else(|| raw.as_str().and_then(|code| code.trim().parse().ok()))?;
    if errcode == 0 {
        return None;
    }
    let errmsg = container
        .get("errmsg")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|errmsg| !errmsg.is_empty())
        .unwrap_or("unknown error");
    Some((errcode, errmsg.to_string()))
}

/// WeCom `errcode` → [`crate::platform::SEND_ERROR_KINDS`].
///
/// WeCom answers on the bot socket with a numeric `errcode` rather than an HTTP status, so both
/// vocabularies are mapped here: the HTTP-shaped codes the taxonomy names (400/403/404/413/429/5xx)
/// and the codes the gateway actually uses — 45009/45033/846607 are the frequency limits (846607 is
/// the 30 messages/min/chat cap), 40001/42001 a rejected or expired credential, 846604/846608 an
/// expired reply window and 846609 a lost subscription.
fn errcode_kind(errcode: i64) -> &'static str {
    match errcode {
        429 | 45009 | 45033 | 846607 => "rate_limited",
        401 | 403 | 40001 | 40013 | 40014 | 42001 | 48001 => "forbidden",
        404 | 40007 => "not_found",
        413 | 45002 => "too_long",
        400 | 40003 | 40006 | 40009 | 40035 | 44002 | 6000 => "bad_format",
        846604 | 846608 | 846609 => "transient",
        500..=599 => "transient",
        _ => "unknown",
    }
}

/// Kinds the delivery ledger may retry as-is.
fn is_retryable_kind(kind: &str) -> bool {
    matches!(kind, "rate_limited" | "transient")
}

/// A single-line, length-capped rendering of a frame for error text.
fn summarise(payload: &Value) -> String {
    let text = payload.to_string();
    if text.chars().count() <= 200 {
        return text;
    }
    let mut out: String = text.chars().take(200).collect();
    out.push('…');
    out
}

/// `warning_text` (base.py): the notice and the caption on separate lines, empty parts dropped.
fn join_notice(notice: &str, caption: Option<&str>) -> String {
    let caption = caption.map(str::trim).filter(|caption| !caption.is_empty());
    match caption {
        Some(caption) if !notice.is_empty() => format!("{notice}\n{caption}"),
        Some(caption) => caption.to_string(),
        None => notice.to_string(),
    }
}

// ----- MIME / media type mapping -----

/// `_prepare_outbound_media`: resolve the MIME type from what the caller gave us, detect the WeCom
/// media type, then apply the size verdict.
fn prepare_outbound_media<'a>(
    kind: MediaKind,
    data: &'a [u8],
    filename: Option<&str>,
    mime: Option<&str>,
) -> PreparedMedia<'a> {
    let content_type = normalize_content_type(mime, filename.unwrap_or_default());
    let filename = resolved_filename(kind, filename, &content_type);
    let detected = detect_wecom_media_type(kind, &content_type);
    let verdict = apply_file_size_limits(data.len(), detected, &content_type);
    PreparedMedia {
        data,
        filename,
        media_type: verdict.final_type,
        rejected: verdict.rejected,
        downgrade_note: verdict.downgrade_note,
    }
}

/// The wire `filename`. The reference always resolves one from the source path; a caller here may
/// pass none, so derive one from the kind plus the extension the MIME type implies.
fn resolved_filename(kind: MediaKind, filename: Option<&str>, mime: &str) -> String {
    if let Some(name) = filename.map(str::trim).filter(|name| !name.is_empty()) {
        // the wire field is a name, not a path
        return name.rsplit(['/', '\\']).next().unwrap_or(name).to_string();
    }
    let base = match kind {
        MediaKind::Image => "image",
        MediaKind::Video => "video",
        MediaKind::Audio => "voice",
        MediaKind::Document => "file",
    };
    match extension_for_mime(mime) {
        Some(extension) => format!("wecom_{base}.{extension}"),
        None => format!("wecom_{base}"),
    }
}

/// `_normalize_content_type`: the caller's MIME type unless it is missing or too generic to mean
/// anything, in which case the filename decides.
fn normalize_content_type(mime: Option<&str>, filename: &str) -> String {
    let normalized = mime
        .unwrap_or_default()
        .split(';')
        .next()
        .unwrap_or_default()
        .trim()
        .to_lowercase();
    if !normalized.is_empty()
        && normalized != "application/octet-stream"
        && normalized != "text/plain"
    {
        return normalized;
    }
    guess_mime_type(filename)
}

/// `_guess_mime_type` over the extension table; unknown extensions are `application/octet-stream`.
fn guess_mime_type(filename: &str) -> String {
    mime_for_extension(&extension_of(filename))
        .unwrap_or("application/octet-stream")
        .to_string()
}

/// Lowercased extension without the dot, `""` when the name has none.
fn extension_of(filename: &str) -> String {
    let name = filename.rsplit(['/', '\\']).next().unwrap_or(filename);
    match name.rsplit_once('.') {
        Some((stem, extension)) if !stem.is_empty() && !extension.is_empty() => {
            extension.to_lowercase()
        }
        _ => String::new(),
    }
}

/// `_detect_wecom_media_type`, plus the trait's [`MediaKind`] as the fallback when the MIME type is
/// generic: `application/ogg` is the gateway's voice container, then the image/video/audio prefixes
/// decide, exactly as the reference does.
fn detect_wecom_media_type(kind: MediaKind, content_type: &str) -> WeComMediaType {
    if content_type == "application/ogg" {
        return WeComMediaType::Voice;
    }
    if content_type.starts_with("image/") {
        return WeComMediaType::Image;
    }
    if content_type.starts_with("video/") {
        return WeComMediaType::Video;
    }
    if content_type.starts_with("audio/") {
        return WeComMediaType::Voice;
    }
    match kind {
        MediaKind::Image => WeComMediaType::Image,
        MediaKind::Video => WeComMediaType::Video,
        MediaKind::Audio => WeComMediaType::Voice,
        MediaKind::Document => WeComMediaType::File,
    }
}

/// `_apply_file_size_limits`: over the absolute cap is a rejection (noticed, then failed), an
/// over-cap or non-AMR voice/image/video is downgraded to `file` with a notice, everything else
/// keeps its type.
fn apply_file_size_limits(
    size: usize,
    detected: WeComMediaType,
    content_type: &str,
) -> SizeVerdict {
    let size_mb = size as f64 / (1024.0 * 1024.0);
    if size > WECOM_ABSOLUTE_MAX_BYTES {
        return SizeVerdict {
            final_type: detected,
            rejected: Some(format!(
                "file too large ({size_mb:.2}MB): WeCom uploads at most {}MB",
                WECOM_ABSOLUTE_MAX_BYTES / (1024 * 1024)
            )),
            downgrade_note: None,
        };
    }
    if detected == WeComMediaType::Voice
        && !content_type.is_empty()
        && !WECOM_VOICE_SUPPORTED_MIMES.contains(&content_type)
    {
        return SizeVerdict {
            final_type: WeComMediaType::File,
            rejected: None,
            downgrade_note: Some(format!(
                "{content_type} cannot be played as a WeCom voice message (only audio/amr can); sent as a file"
            )),
        };
    }
    if let Some((cap, label, human)) = detected.type_limit()
        && size > cap
    {
        return SizeVerdict {
            final_type: WeComMediaType::File,
            rejected: None,
            downgrade_note: Some(format!(
                "{label} is {size_mb:.2}MB, over WeCom's {human} {label} cap; sent as a file"
            )),
        };
    }
    SizeVerdict {
        final_type: detected,
        rejected: None,
        downgrade_note: None,
    }
}

/// The part of Python's `mimetypes` this adapter needs: the extensions WeCom can be handed.
fn mime_for_extension(extension: &str) -> Option<&'static str> {
    let mime = match extension {
        "png" => "image/png",
        "jpg" | "jpeg" | "jpe" => "image/jpeg",
        "gif" => "image/gif",
        "webp" => "image/webp",
        "bmp" => "image/bmp",
        "tif" | "tiff" => "image/tiff",
        "heic" => "image/heic",
        "mp4" | "m4v" => "video/mp4",
        "mov" => "video/quicktime",
        "avi" => "video/x-msvideo",
        "mkv" => "video/x-matroska",
        "webm" => "video/webm",
        "mpg" | "mpeg" => "video/mpeg",
        "amr" => "audio/amr",
        "mp3" => "audio/mpeg",
        "wav" => "audio/wav",
        "m4a" => "audio/mp4",
        "aac" => "audio/aac",
        "ogg" | "oga" | "opus" => "audio/ogg",
        "flac" => "audio/flac",
        "pdf" => "application/pdf",
        "doc" => "application/msword",
        "docx" => "application/vnd.openxmlformats-officedocument.wordprocessingml.document",
        "xls" => "application/vnd.ms-excel",
        "xlsx" => "application/vnd.openxmlformats-officedocument.spreadsheetml.sheet",
        "ppt" => "application/vnd.ms-powerpoint",
        "pptx" => "application/vnd.openxmlformats-officedocument.presentationml.presentation",
        "txt" | "log" => "text/plain",
        "csv" => "text/csv",
        "md" => "text/markdown",
        "json" => "application/json",
        "xml" => "application/xml",
        "zip" => "application/zip",
        "gz" => "application/gzip",
        "tar" => "application/x-tar",
        "7z" => "application/x-7z-compressed",
        "rar" => "application/vnd.rar",
        _ => return None,
    };
    Some(mime)
}

/// The reverse direction, for naming a file the caller did not name.
fn extension_for_mime(mime: &str) -> Option<&'static str> {
    let extension = match mime {
        "image/png" => "png",
        "image/jpeg" => "jpg",
        "image/gif" => "gif",
        "image/webp" => "webp",
        "image/bmp" => "bmp",
        "video/mp4" => "mp4",
        "video/quicktime" => "mov",
        "video/webm" => "webm",
        "audio/amr" => "amr",
        "audio/mpeg" => "mp3",
        "audio/wav" => "wav",
        "audio/ogg" => "ogg",
        "application/pdf" => "pdf",
        "text/plain" => "txt",
        "text/csv" => "csv",
        _ => return None,
    };
    Some(extension)
}

// ----- encoders (the crate may not add dependencies) -----

/// Lowercase hex MD5 of `data` — the init frame's `md5` field (`hashlib.md5(data).hexdigest()`).
fn md5_hex(data: &[u8]) -> String {
    md5::digest(data)
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

/// Standard-alphabet Base64 with `=` padding (RFC 4648 §4) for the chunk frames.
fn base64_encode(data: &[u8]) -> String {
    const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(data.len().div_ceil(3) * 4);
    for chunk in data.chunks(3) {
        let first = chunk.first().copied().unwrap_or(0) as u32;
        let second = chunk.get(1).copied().unwrap_or(0) as u32;
        let third = chunk.get(2).copied().unwrap_or(0) as u32;
        let triple = (first << 16) | (second << 8) | third;
        out.push(ALPHABET[(triple >> 18) as usize & 0x3f] as char);
        out.push(ALPHABET[(triple >> 12) as usize & 0x3f] as char);
        if chunk.len() > 1 {
            out.push(ALPHABET[(triple >> 6) as usize & 0x3f] as char);
        } else {
            out.push('=');
        }
        if chunk.len() > 2 {
            out.push(ALPHABET[triple as usize & 0x3f] as char);
        } else {
            out.push('=');
        }
    }
    out
}

/// MD5 (RFC 1321), implemented here because the crate may not add dependencies and the SHA-2
/// digests available elsewhere cannot stand in for the `md5` field WeCom expects.
mod md5 {
    /// Per-round left-rotation amounts.
    const SHIFTS: [u32; 64] = [
        7, 12, 17, 22, 7, 12, 17, 22, 7, 12, 17, 22, 7, 12, 17, 22, 5, 9, 14, 20, 5, 9, 14, 20, 5,
        9, 14, 20, 5, 9, 14, 20, 4, 11, 16, 23, 4, 11, 16, 23, 4, 11, 16, 23, 4, 11, 16, 23, 6, 10,
        15, 21, 6, 10, 15, 21, 6, 10, 15, 21, 6, 10, 15, 21,
    ];

    /// `floor(abs(sin(i + 1)) * 2^32)`, the RFC's table.
    const TABLE: [u32; 64] = [
        0xd76a_a478,
        0xe8c7_b756,
        0x2420_70db,
        0xc1bd_ceee,
        0xf57c_0faf,
        0x4787_c62a,
        0xa830_4613,
        0xfd46_9501,
        0x6980_98d8,
        0x8b44_f7af,
        0xffff_5bb1,
        0x895c_d7be,
        0x6b90_1122,
        0xfd98_7193,
        0xa679_438e,
        0x49b4_0821,
        0xf61e_2562,
        0xc040_b340,
        0x265e_5a51,
        0xe9b6_c7aa,
        0xd62f_105d,
        0x0244_1453,
        0xd8a1_e681,
        0xe7d3_fbc8,
        0x21e1_cde6,
        0xc337_07d6,
        0xf4d5_0d87,
        0x455a_14ed,
        0xa9e3_e905,
        0xfcef_a3f8,
        0x676f_02d9,
        0x8d2a_4c8a,
        0xfffa_3942,
        0x8771_f681,
        0x6d9d_6122,
        0xfde5_380c,
        0xa4be_ea44,
        0x4bde_cfa9,
        0xf6bb_4b60,
        0xbebf_bc70,
        0x289b_7ec6,
        0xeaa1_27fa,
        0xd4ef_3085,
        0x0488_1d05,
        0xd9d4_d039,
        0xe6db_99e5,
        0x1fa2_7cf8,
        0xc4ac_5665,
        0xf429_2244,
        0x432a_ff97,
        0xab94_23a7,
        0xfc93_a039,
        0x655b_59c3,
        0x8f0c_cc92,
        0xffef_f47d,
        0x8584_5dd1,
        0x6fa8_7e4f,
        0xfe2c_e6e0,
        0xa301_4314,
        0x4e08_11a1,
        0xf753_7e82,
        0xbd3a_f235,
        0x2ad7_d2bb,
        0xeb86_d391,
    ];

    /// Raw 16-byte digest, little-endian words as the RFC lays them out.
    pub(super) fn digest(data: &[u8]) -> [u8; 16] {
        let mut message = data.to_vec();
        let bit_len = (data.len() as u64).wrapping_mul(8);
        message.push(0x80);
        while message.len() % 64 != 56 {
            message.push(0);
        }
        message.extend_from_slice(&bit_len.to_le_bytes());

        let mut state = [0x6745_2301u32, 0xefcd_ab89, 0x98ba_dcfe, 0x1032_5476];
        for block in message.as_chunks::<64>().0 {
            let mut words = [0u32; 16];
            for (word, chunk) in words.iter_mut().zip(block.as_chunks::<4>().0) {
                *word = u32::from_le_bytes(*chunk);
            }
            let (mut a, mut b, mut c, mut d) = (state[0], state[1], state[2], state[3]);
            for round in 0..64 {
                let (mixed, index) = match round / 16 {
                    0 => ((b & c) | (!b & d), round),
                    1 => ((d & b) | (!d & c), (5 * round + 1) % 16),
                    2 => (b ^ c ^ d, (3 * round + 5) % 16),
                    _ => (c ^ (b | !d), (7 * round) % 16),
                };
                let sum = a
                    .wrapping_add(mixed)
                    .wrapping_add(TABLE[round])
                    .wrapping_add(words[index]);
                a = d;
                d = c;
                c = b;
                b = b.wrapping_add(sum.rotate_left(SHIFTS[round]));
            }
            state[0] = state[0].wrapping_add(a);
            state[1] = state[1].wrapping_add(b);
            state[2] = state[2].wrapping_add(c);
            state[3] = state[3].wrapping_add(d);
        }

        let mut out = [0u8; 16];
        for (chunk, word) in out.as_chunks_mut::<4>().0.iter_mut().zip(state) {
            *chunk = word.to_le_bytes();
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::socket::{LineSocket, SocketError};
    use async_trait::async_trait;
    use std::sync::Mutex as StdMutex;

    struct MockSocket {
        sent: StdMutex<Vec<String>>,
        incoming: StdMutex<VecDeque<String>>,
        /// `req_id`s the gateway has not answered yet, oldest first.
        awaiting: StdMutex<VecDeque<String>>,
    }

    impl MockSocket {
        fn new(incoming: Vec<String>) -> Arc<Self> {
            Arc::new(Self {
                sent: StdMutex::new(Vec::new()),
                incoming: StdMutex::new(incoming.into()),
                awaiting: StdMutex::new(VecDeque::new()),
            })
        }
        fn sent(&self) -> Vec<String> {
            self.sent
                .lock()
                .map(|sent| sent.clone())
                .unwrap_or_default()
        }
    }

    #[async_trait]
    impl LineSocket for MockSocket {
        async fn send_line(&self, line: &str) -> Result<(), SocketError> {
            if let Ok(mut sent) = self.sent.lock() {
                sent.push(line.to_string());
            }
            if let Ok(payload) = serde_json::from_str::<Value>(line)
                && let Some(req_id) = payload
                    .get("headers")
                    .and_then(|headers| headers.get("req_id"))
                    .and_then(Value::as_str)
                && let Ok(mut awaiting) = self.awaiting.lock()
            {
                awaiting.push_back(req_id.to_string());
            }
            Ok(())
        }

        async fn next_line(&self) -> Result<Option<String>, SocketError> {
            let Some(line) = self
                .incoming
                .lock()
                .ok()
                .and_then(|mut queue| queue.pop_front())
            else {
                return Ok(None);
            };
            let Ok(mut payload) = serde_json::from_str::<Value>(&line) else {
                return Ok(Some(line));
            };
            // The adapter mints `req_id`s internally, so a scripted reply marks itself `@echo` and
            // the mock copies the `req_id` of the oldest unanswered request — the correlation the
            // gateway itself performs.
            if payload.get("@echo").and_then(Value::as_bool) == Some(true) {
                let echoed = self
                    .awaiting
                    .lock()
                    .ok()
                    .and_then(|mut queue| queue.pop_front());
                if let Some(echoed) = echoed
                    && let Some(object) = payload.as_object_mut()
                {
                    object.remove("@echo");
                    object.insert("headers".to_string(), json!({ "req_id": echoed }));
                }
            }
            Ok(Some(payload.to_string()))
        }
    }

    fn build(incoming: Vec<String>) -> (WeComAdapter, Arc<MockSocket>) {
        let socket = MockSocket::new(incoming);
        let adapter = WeComAdapter::new(
            Arc::clone(&socket) as SharedLineSocket,
            "bot-1",
            "secret-1",
            "device-1",
        );
        (adapter, socket)
    }

    fn sent_json(socket: &MockSocket) -> Vec<Value> {
        socket
            .sent()
            .iter()
            .map(|line| serde_json::from_str(line).unwrap())
            .collect()
    }

    fn cmds(socket: &MockSocket) -> Vec<String> {
        sent_json(socket)
            .iter()
            .map(|frame| frame["cmd"].as_str().unwrap_or_default().to_string())
            .collect()
    }

    /// A success reply for the pending request.
    fn ack(body: Value) -> String {
        json!({ "@echo": true, "errcode": 0, "errmsg": "ok", "body": body }).to_string()
    }

    /// A refusal for the pending request.
    fn refuse(errcode: i64, errmsg: &str) -> String {
        json!({ "@echo": true, "errcode": errcode, "errmsg": errmsg, "body": {} }).to_string()
    }

    /// init + `chunks` chunk acks + finish, i.e. everything up to the media message.
    fn upload_script(media_id: &str, chunks: usize, media_type: &str) -> Vec<String> {
        let mut script = vec![ack(json!({ "upload_id": "up-1" }))];
        for _ in 0..chunks {
            script.push(ack(json!({})));
        }
        script.push(ack(json!({ "media_id": media_id, "type": media_type })));
        script
    }

    fn inbound_callback(chat_id: &str, msg_id: &str, req_id: &str, text: &str) -> String {
        json!({
            "cmd": CMD_CALLBACK,
            "headers": {"req_id": req_id},
            "body": {"msgtype": "text", "chatid": chat_id, "chattype": "single",
                     "from": {"userid": chat_id}, "msgid": msg_id,
                     "text": {"content": text}}
        })
        .to_string()
    }

    fn image_bytes(data: &[u8]) -> OutboundMedia {
        OutboundMedia::new(MediaKind::Image, MediaSource::Bytes(data.to_vec()))
            .with_filename("photo.png")
            .with_mime("image/png")
    }

    #[tokio::test]
    async fn connect_subscribes_with_the_bot_credentials() {
        let (mut adapter, socket) = build(vec![]);
        assert!(adapter.connect(false).await);
        let sent = sent_json(&socket);
        assert_eq!(sent[0]["cmd"], CMD_SUBSCRIBE);
        assert_eq!(sent[0]["body"]["bot_id"], "bot-1");
        assert_eq!(sent[0]["body"]["secret"], "secret-1");
        assert_eq!(sent[0]["body"]["device_id"], "device-1");
    }

    #[tokio::test]
    async fn send_starts_a_new_message_without_a_req_id() {
        let (adapter, socket) = build(vec![]);
        let result = adapter.send("chat-1", "hello", None, None).await;
        assert!(result.success);
        let sent = sent_json(&socket);
        assert_eq!(sent[0]["cmd"], CMD_SEND);
        assert_eq!(sent[0]["body"]["chatid"], "chat-1");
        assert_eq!(sent[0]["body"]["msgtype"], "markdown");
        assert_eq!(sent[0]["body"]["markdown"]["content"], "hello");
    }

    #[tokio::test]
    async fn send_answers_in_stream_when_a_req_id_is_known() {
        let (adapter, socket) = build(vec![]);
        adapter.remember_req_id("group-1", "inbound-req");
        let result = adapter.send("group-1", "reply", None, None).await;
        assert!(result.success);
        let sent = sent_json(&socket);
        assert_eq!(sent[0]["cmd"], CMD_RESPOND);
        assert_eq!(sent[0]["headers"]["req_id"], "inbound-req");
        assert!(sent[0]["body"].get("chatid").is_none());
    }

    #[tokio::test]
    async fn long_text_is_chunked() {
        let content = "x".repeat(WECOM_MAX_MESSAGE_LENGTH + 1);
        let (adapter, socket) = build(vec![]);
        assert!(adapter.send("chat-1", &content, None, None).await.success);
        let sent = sent_json(&socket);
        assert_eq!(sent.len(), 2);
        for item in sent {
            assert!(
                item["body"]["markdown"]["content"]
                    .as_str()
                    .unwrap()
                    .chars()
                    .count()
                    <= WECOM_MAX_MESSAGE_LENGTH
            );
        }
    }

    #[tokio::test]
    async fn inbound_dm_and_group_callbacks_parse() {
        let dm = json!({
            "cmd": CMD_CALLBACK,
            "headers": {"req_id": "r1"},
            "body": {"msgtype": "text", "chatid": "u1", "chattype": "single",
                     "from": {"userid": "u1"}, "msgid": "m1",
                     "text": {"content": "hello"}}
        })
        .to_string();
        let group = json!({
            "cmd": CMD_CALLBACK,
            "headers": {"req_id": "r2"},
            "body": {"msgtype": "text", "chatid": "group-9", "chattype": "group",
                     "from": {"userid": "u2"}, "msgid": "m2",
                     "text": {"content": "@vikram /status"}}
        })
        .to_string();
        let ping = json!({"cmd": CMD_PING, "headers": {"req_id": "p"}}).to_string();
        let (adapter, _) = build(vec![ping, dm, group]);
        let first = adapter.next_event().await.unwrap().expect("dm");
        assert_eq!(first.text, "hello");
        assert_eq!(first.source.chat_type, "dm");
        assert_eq!(first.user_id.as_deref(), Some("u1"));
        let second = adapter.next_event().await.unwrap().expect("group");
        assert_eq!(second.text, "/status");
        assert_eq!(second.source.chat_type, "group");
        assert_eq!(second.metadata["req_id"], "r2");
    }

    #[tokio::test]
    async fn blank_inputs_do_not_touch_the_socket() {
        let (adapter, socket) = build(vec![]);
        assert!(!adapter.send("", "hi", None, None).await.success);
        assert!(!adapter.send("chat", "  ", None, None).await.success);
        assert!(socket.sent().is_empty());
    }

    #[tokio::test]
    async fn end_of_stream_is_none() {
        let (adapter, _) = build(vec![]);
        assert!(adapter.next_event().await.unwrap().is_none());
    }

    // ----- media: capability -----

    #[tokio::test]
    async fn supports_media_is_advertised() {
        let (adapter, _) = build(vec![]);
        assert!(adapter.supports_media());
    }

    // ----- media: the three-phase upload -----

    #[tokio::test]
    async fn media_upload_walks_init_chunks_finish_then_sends() {
        let data = b"hello media".to_vec();
        let mut script = upload_script("mid-1", 1, "image");
        script.push(ack(json!({ "msgid": "m-out" })));
        let (adapter, socket) = build(script);

        let result = adapter
            .send_media("chat-1", &image_bytes(&data), None)
            .await;

        assert!(result.success, "{:?}", result.error);
        assert_eq!(
            cmds(&socket),
            vec![
                CMD_UPLOAD_INIT,
                CMD_UPLOAD_CHUNK,
                CMD_UPLOAD_FINISH,
                CMD_SEND
            ]
        );
        let sent = sent_json(&socket);

        // phase 1: the reference's exact field set
        let init = &sent[0]["body"];
        assert_eq!(
            sent[0]["headers"]["req_id"]
                .as_str()
                .map(|id| id.starts_with(CMD_UPLOAD_INIT)),
            Some(true)
        );
        assert_eq!(init["type"], "image");
        assert_eq!(init["filename"], "photo.png");
        assert_eq!(init["total_size"], data.len());
        assert_eq!(init["total_chunks"], 1);
        assert_eq!(init["md5"], md5_hex(&data));

        // phase 2: body shape and 0-based chunk index
        let chunk = &sent[1]["body"];
        assert_eq!(chunk["upload_id"], "up-1");
        assert_eq!(chunk["chunk_index"], 0);
        assert_eq!(chunk["base64_data"], "aGVsbG8gbWVkaWE=");

        // phase 3: finish carries the upload id only
        assert_eq!(sent[2]["body"], json!({ "upload_id": "up-1" }));

        // the media message: the reference's `_media_body` plus `chatid`
        assert_eq!(sent[3]["body"]["msgtype"], "image");
        assert_eq!(sent[3]["body"]["image"]["media_id"], "mid-1");
        assert_eq!(sent[3]["body"]["chatid"], "chat-1");

        // reported success carries the gateway's req_id and the frames it came from
        assert!(
            result
                .message_id
                .as_deref()
                .is_some_and(|id| id.starts_with(CMD_SEND))
        );
        let raw = result.raw_response.as_ref().expect("raw response");
        assert_eq!(raw["upload"]["media_id"], "mid-1");
        assert_eq!(raw["media"]["body"]["msgid"], "m-out");
        assert_eq!(raw["caption"], Value::Null);
        assert_eq!(raw["downgrade"], Value::Null);
    }

    #[tokio::test]
    async fn upload_chunk_boundaries_split_on_512_kib() {
        let data = vec![7u8; WECOM_UPLOAD_CHUNK_SIZE + 1];
        let mut script = upload_script("mid-2", 2, "file");
        script.push(ack(json!({})));
        let (adapter, socket) = build(script);

        let media = OutboundMedia::new(MediaKind::Document, MediaSource::Bytes(data.clone()))
            .with_filename("report.pdf")
            .with_mime("application/pdf");
        assert!(adapter.send_media("chat-1", &media, None).await.success);

        let sent = sent_json(&socket);
        assert_eq!(
            cmds(&socket),
            vec![
                CMD_UPLOAD_INIT,
                CMD_UPLOAD_CHUNK,
                CMD_UPLOAD_CHUNK,
                CMD_UPLOAD_FINISH,
                CMD_SEND
            ]
        );
        assert_eq!(sent[0]["body"]["total_size"], WECOM_UPLOAD_CHUNK_SIZE + 1);
        assert_eq!(sent[0]["body"]["total_chunks"], 2);
        assert_eq!(sent[1]["body"]["chunk_index"], 0);
        assert_eq!(sent[2]["body"]["chunk_index"], 1);
        // 512 KiB encodes to 174_763 groups of four characters; the trailing byte to one group
        assert_eq!(
            sent[1]["body"]["base64_data"].as_str().map(str::len),
            Some(699_052)
        );
        assert_eq!(
            sent[2]["body"]["base64_data"].as_str().map(str::len),
            Some(4)
        );
        assert_eq!(sent[4]["body"]["msgtype"], "file");
        assert_eq!(sent[4]["body"]["file"]["media_id"], "mid-2");
    }

    #[tokio::test]
    async fn media_send_uses_the_cached_req_id_for_a_passive_reply() {
        let mut script = upload_script("mid-3", 1, "image");
        script.push(ack(json!({})));
        let (adapter, socket) = build(script);
        adapter.remember_req_id("group-1", "inbound-req");

        let result = adapter
            .send_media("group-1", &image_bytes(b"png-bytes"), None)
            .await;

        assert!(result.success, "{:?}", result.error);
        let sent = sent_json(&socket);
        let reply = sent.last().expect("reply frame");
        assert_eq!(reply["cmd"], CMD_RESPOND);
        assert_eq!(reply["headers"]["req_id"], "inbound-req");
        assert_eq!(reply["body"]["image"]["media_id"], "mid-3");
        assert!(reply["body"].get("chatid").is_none());
    }

    #[tokio::test]
    async fn media_send_resolves_reply_to_through_the_message_cache() {
        let mut script = vec![inbound_callback("chat-a", "m-in", "r-in", "hello")];
        script.extend(upload_script("mid-4", 1, "image"));
        script.push(ack(json!({})));
        // a second media send for a chat with no cache: `quote:` never resolves to a req_id
        script.extend(upload_script("mid-5", 1, "image"));
        script.push(ack(json!({})));
        let (adapter, socket) = build(script);

        let event = adapter.next_event().await.unwrap().expect("event");
        assert_eq!(event.message_id.as_deref(), Some("m-in"));

        // reply_to names the inbound message, so the reply goes to *its* req_id even though the
        // send targets another chat
        let first = adapter
            .send_media("other-chat", &image_bytes(b"a"), Some("m-in"))
            .await;
        assert!(first.success, "{:?}", first.error);
        let after_first = sent_json(&socket);
        let reply = &after_first[after_first.len() - 1];
        assert_eq!(reply["cmd"], CMD_RESPOND);
        assert_eq!(reply["headers"]["req_id"], "r-in");

        let second = adapter
            .send_media("other-chat", &image_bytes(b"b"), Some("quote:m-in"))
            .await;
        assert!(second.success, "{:?}", second.error);
        let after_second = sent_json(&socket);
        let proactive = &after_second[after_second.len() - 1];
        assert_eq!(proactive["cmd"], CMD_SEND);
        assert_eq!(proactive["body"]["chatid"], "other-chat");
    }

    #[tokio::test]
    async fn media_caption_is_sent_as_a_followup_markdown() {
        let mut script = upload_script("mid-6", 1, "image");
        script.push(ack(json!({})));
        let (adapter, socket) = build(script);

        let media = image_bytes(b"cap").with_caption("here is the chart");
        let result = adapter.send_media("chat-1", &media, None).await;

        assert!(result.success, "{:?}", result.error);
        let sent = sent_json(&socket);
        assert_eq!(sent.len(), 5);
        assert_eq!(sent[4]["cmd"], CMD_SEND);
        assert_eq!(sent[4]["body"]["msgtype"], "markdown");
        assert_eq!(sent[4]["body"]["markdown"]["content"], "here is the chart");
        let raw = result.raw_response.as_ref().expect("raw");
        assert_eq!(raw["caption_error"], Value::Null);
    }

    #[tokio::test]
    async fn media_without_caption_or_downgrade_sends_no_followup() {
        let mut script = upload_script("mid-7", 1, "image");
        script.push(ack(json!({})));
        let (adapter, socket) = build(script);
        let media = image_bytes(b"x").with_caption("   ");
        assert!(adapter.send_media("chat-1", &media, None).await.success);
        assert_eq!(sent_json(&socket).len(), 4);
    }

    // ----- media: a failure at each phase stops the flow -----

    #[tokio::test]
    async fn init_failure_stops_before_any_chunk() {
        let (adapter, socket) = build(vec![refuse(45009, "api freq out of limit")]);
        let result = adapter
            .send_media("chat-1", &image_bytes(b"bytes"), None)
            .await;

        assert!(!result.success);
        assert_eq!(result.error_kind.as_deref(), Some("rate_limited"));
        assert!(result.retryable);
        assert_eq!(cmds(&socket), vec![CMD_UPLOAD_INIT]);
        assert!(
            result
                .error
                .as_deref()
                .is_some_and(|error| error.contains("media upload init failed"))
        );
    }

    #[tokio::test]
    async fn chunk_failure_stops_before_finish_and_send() {
        let data = vec![1u8; WECOM_UPLOAD_CHUNK_SIZE + 1];
        let script = vec![
            ack(json!({ "upload_id": "up-9" })),
            refuse(40006, "invalid filesize"),
        ];
        let (adapter, socket) = build(script);
        let media = OutboundMedia::new(MediaKind::Document, MediaSource::Bytes(data));
        let result = adapter.send_media("chat-1", &media, None).await;

        assert!(!result.success);
        assert_eq!(result.error_kind.as_deref(), Some("bad_format"));
        assert!(!result.retryable);
        assert_eq!(cmds(&socket), vec![CMD_UPLOAD_INIT, CMD_UPLOAD_CHUNK]);
        assert!(
            result
                .error
                .as_deref()
                .is_some_and(|error| error.contains("media upload chunk 0 failed"))
        );
    }

    #[tokio::test]
    async fn finish_failure_stops_before_the_media_send() {
        let script = vec![
            ack(json!({ "upload_id": "up-1" })),
            ack(json!({})),
            refuse(40001, "invalid credential"),
        ];
        let (adapter, socket) = build(script);
        let result = adapter
            .send_media("chat-1", &image_bytes(b"bytes"), None)
            .await;

        assert!(!result.success);
        assert_eq!(result.error_kind.as_deref(), Some("forbidden"));
        assert_eq!(
            cmds(&socket),
            vec![CMD_UPLOAD_INIT, CMD_UPLOAD_CHUNK, CMD_UPLOAD_FINISH]
        );
    }

    #[tokio::test]
    async fn media_message_failure_is_reported_not_faked() {
        let mut script = upload_script("mid-8", 1, "image");
        script.push(refuse(846609, "subscription lost"));
        let (adapter, socket) = build(script);
        let result = adapter
            .send_media("chat-1", &image_bytes(b"bytes"), None)
            .await;

        assert!(!result.success);
        assert_eq!(result.error_kind.as_deref(), Some("transient"));
        assert!(result.retryable);
        assert_eq!(
            cmds(&socket),
            vec![
                CMD_UPLOAD_INIT,
                CMD_UPLOAD_CHUNK,
                CMD_UPLOAD_FINISH,
                CMD_SEND
            ]
        );
        assert!(
            result
                .error
                .as_deref()
                .is_some_and(|error| error.contains("send media message failed"))
        );
    }

    #[tokio::test]
    async fn missing_upload_id_never_reaches_a_chunk() {
        let (adapter, socket) = build(vec![ack(json!({ "total_size": 3 }))]);
        let result = adapter
            .send_media("chat-1", &image_bytes(b"abc"), None)
            .await;

        assert!(!result.success);
        assert_eq!(result.error_kind.as_deref(), Some("transient"));
        assert_eq!(cmds(&socket), vec![CMD_UPLOAD_INIT]);
        assert!(
            result
                .error
                .as_deref()
                .is_some_and(|error| error.contains("no upload_id"))
        );
    }

    #[tokio::test]
    async fn missing_media_id_never_sends_the_message() {
        let script = vec![
            ack(json!({ "upload_id": "up-1" })),
            ack(json!({})),
            ack(json!({ "type": "image" })),
        ];
        let (adapter, socket) = build(script);
        let result = adapter
            .send_media("chat-1", &image_bytes(b"abc"), None)
            .await;

        assert!(!result.success);
        assert_eq!(result.error_kind.as_deref(), Some("transient"));
        assert_eq!(
            cmds(&socket),
            vec![CMD_UPLOAD_INIT, CMD_UPLOAD_CHUNK, CMD_UPLOAD_FINISH]
        );
        assert!(
            result
                .error
                .as_deref()
                .is_some_and(|error| error.contains("no media_id"))
        );
    }

    #[tokio::test]
    async fn closed_socket_maps_to_transient() {
        let (adapter, socket) = build(vec![]);
        let result = adapter
            .send_media("chat-1", &image_bytes(b"abc"), None)
            .await;

        assert!(!result.success);
        assert_eq!(result.error_kind.as_deref(), Some("transient"));
        assert!(result.retryable);
        assert_eq!(cmds(&socket), vec![CMD_UPLOAD_INIT]);
    }

    #[tokio::test]
    async fn a_body_level_errcode_is_not_mistaken_for_success() {
        let (adapter, socket) = build(vec![
            json!({ "@echo": true, "body": { "errcode": 45009, "errmsg": "too many" } })
                .to_string(),
        ]);
        let result = adapter
            .send_media("chat-1", &image_bytes(b"abc"), None)
            .await;

        assert!(!result.success);
        assert_eq!(result.error_kind.as_deref(), Some("rate_limited"));
        assert_eq!(cmds(&socket), vec![CMD_UPLOAD_INIT]);
    }

    // ----- media: local refusals -----

    #[tokio::test]
    async fn empty_bytes_are_refused_without_touching_the_socket() {
        let (adapter, socket) = build(vec![]);
        let result = adapter.send_media("chat-1", &image_bytes(b""), None).await;

        assert!(!result.success);
        assert_eq!(result.error_kind.as_deref(), Some("bad_format"));
        assert!(
            result
                .error
                .as_deref()
                .is_some_and(|error| error.contains("empty media"))
        );
        assert!(socket.sent().is_empty());
    }

    #[tokio::test]
    async fn url_sources_are_refused_truthfully() {
        let (adapter, socket) = build(vec![]);
        let media = OutboundMedia::image_url("https://example.test/a.png");
        let result = adapter.send_media("chat-1", &media, None).await;

        assert!(!result.success);
        assert_eq!(result.error_kind.as_deref(), Some("bad_format"));
        assert!(
            result
                .error
                .as_deref()
                .is_some_and(|error| error.contains("no HTTP client"))
        );
        assert!(socket.sent().is_empty());
    }

    #[tokio::test]
    async fn blank_chat_id_is_refused() {
        let (adapter, socket) = build(vec![]);
        let result = adapter.send_media("  ", &image_bytes(b"abc"), None).await;
        assert!(!result.success);
        assert_eq!(result.error_kind.as_deref(), Some("bad_format"));
        assert!(socket.sent().is_empty());
    }

    #[tokio::test]
    async fn send_image_falls_back_to_a_text_send_for_a_url() {
        let (adapter, socket) = build(vec![]);
        let result = adapter
            .send_image(
                "chat-1",
                "https://example.test/a.png",
                Some("see this"),
                None,
            )
            .await;

        assert!(result.success);
        let sent = sent_json(&socket);
        assert_eq!(sent.len(), 1);
        assert_eq!(sent[0]["cmd"], CMD_SEND);
        assert_eq!(
            sent[0]["body"]["markdown"]["content"],
            "see this\nhttps://example.test/a.png"
        );
    }

    #[tokio::test]
    async fn oversize_payload_is_rejected_after_a_notice() {
        let data = vec![0u8; WECOM_ABSOLUTE_MAX_BYTES + 1];
        let (adapter, socket) = build(vec![]);
        let media = OutboundMedia::new(MediaKind::Document, MediaSource::Bytes(data))
            .with_filename("huge.bin");
        let result = adapter.send_media("chat-1", &media, None).await;

        assert!(!result.success);
        assert_eq!(result.error_kind.as_deref(), Some("too_long"));
        // the rejection goes out as follow-up markdown instead of an upload attempt
        let sent = sent_json(&socket);
        assert_eq!(sent.len(), 1);
        assert_eq!(sent[0]["cmd"], CMD_SEND);
        let notice = sent[0]["body"]["markdown"]["content"]
            .as_str()
            .unwrap_or_default();
        assert!(notice.contains("file too large"), "{notice}");
    }

    #[tokio::test]
    async fn non_amr_audio_is_downgraded_to_a_file() {
        let data = vec![3u8; 1024];
        let mut script = upload_script("mid-9", 1, "file");
        script.push(ack(json!({})));
        let (adapter, socket) = build(script);
        let media = OutboundMedia::new(MediaKind::Audio, MediaSource::Bytes(data))
            .with_filename("voice.mp3")
            .with_mime("audio/mpeg");

        let result = adapter.send_media("chat-1", &media, None).await;

        assert!(result.success, "{:?}", result.error);
        let sent = sent_json(&socket);
        assert_eq!(sent[0]["body"]["type"], "file");
        assert_eq!(sent[3]["body"]["msgtype"], "file");
        assert_eq!(sent[3]["body"]["file"]["media_id"], "mid-9");
        // the downgrade rides a second follow-up markdown (no caption was given)
        assert_eq!(sent.len(), 5);
        let notice = sent[4]["body"]["markdown"]["content"]
            .as_str()
            .unwrap_or_default();
        assert!(notice.contains("sent as a file"), "{notice}");
        let raw = result.raw_response.as_ref().expect("raw");
        assert_eq!(raw["downgrade_error"], Value::Null);
    }

    #[tokio::test]
    async fn amr_audio_keeps_the_voice_type() {
        let mut script = upload_script("mid-10", 1, "voice");
        script.push(ack(json!({})));
        let (adapter, socket) = build(script);
        let media = OutboundMedia::new(MediaKind::Audio, MediaSource::Bytes(vec![4u8; 64]))
            .with_filename("clip.amr");

        assert!(adapter.send_media("chat-1", &media, None).await.success);
        let sent = sent_json(&socket);
        assert_eq!(sent[0]["body"]["type"], "voice");
        assert_eq!(sent[0]["body"]["filename"], "clip.amr");
        assert_eq!(sent.len(), 4);
    }

    // ----- media: sizing, MIME mapping and the taxonomy -----

    #[test]
    fn size_caps_match_the_reference() {
        let mb = 1024 * 1024;
        // within the per-type caps: type survives
        assert_eq!(
            apply_file_size_limits(10 * mb, WeComMediaType::Image, "image/png"),
            SizeVerdict {
                final_type: WeComMediaType::Image,
                rejected: None,
                downgrade_note: None
            }
        );
        // over the image cap: downgraded to a file with a notice
        let image = apply_file_size_limits(10 * mb + 1, WeComMediaType::Image, "image/png");
        assert_eq!(image.final_type, WeComMediaType::File);
        assert!(image.rejected.is_none());
        assert!(
            image
                .downgrade_note
                .as_deref()
                .is_some_and(|note| note.contains("10MB"))
        );
        // over the video cap
        assert_eq!(
            apply_file_size_limits(10 * mb + 1, WeComMediaType::Video, "video/mp4").final_type,
            WeComMediaType::File
        );
        // over the voice cap
        let voice = apply_file_size_limits(2 * mb + 1, WeComMediaType::Voice, "audio/amr");
        assert_eq!(voice.final_type, WeComMediaType::File);
        assert!(
            voice
                .downgrade_note
                .as_deref()
                .is_some_and(|note| note.contains("2MB"))
        );
        // a file has no type cap of its own, only the absolute one
        assert_eq!(
            apply_file_size_limits(20 * mb, WeComMediaType::File, "application/pdf").final_type,
            WeComMediaType::File
        );
        // over the absolute cap: rejected outright
        let oversize = apply_file_size_limits(20 * mb + 1, WeComMediaType::File, "application/pdf");
        assert_eq!(oversize.final_type, WeComMediaType::File);
        assert!(
            oversize
                .rejected
                .as_deref()
                .is_some_and(|reason| reason.contains("file too large"))
        );
        assert!(oversize.downgrade_note.is_none());
        // a non-AMR voice is a file no matter how small
        let downgraded = apply_file_size_limits(1024, WeComMediaType::Voice, "audio/mpeg");
        assert_eq!(downgraded.final_type, WeComMediaType::File);
        assert!(
            downgraded
                .downgrade_note
                .as_deref()
                .is_some_and(|note| note.contains("audio/amr"))
        );
    }

    #[test]
    fn chunk_count_cap_is_enforced() {
        // 100 chunks of 512 KiB is the largest upload the gateway accepts; the absolute byte cap
        // (20 MB) is reached first, so this guard is a backstop, not the primary one.
        let largest_upload_chunks = WECOM_ABSOLUTE_MAX_BYTES.div_ceil(WECOM_UPLOAD_CHUNK_SIZE);
        assert_eq!(largest_upload_chunks, 40);
        assert!(largest_upload_chunks < WECOM_MAX_UPLOAD_CHUNKS);
    }

    #[test]
    fn media_type_detection_matches_the_reference() {
        // the reference's first rule: application/ogg is a voice message
        assert_eq!(
            detect_wecom_media_type(MediaKind::Document, "application/ogg"),
            WeComMediaType::Voice
        );
        // then the MIME prefixes decide, whatever the caller's kind says
        assert_eq!(
            detect_wecom_media_type(MediaKind::Document, "image/png"),
            WeComMediaType::Image
        );
        assert_eq!(
            detect_wecom_media_type(MediaKind::Image, "video/mp4"),
            WeComMediaType::Video
        );
        assert_eq!(
            detect_wecom_media_type(MediaKind::Document, "audio/amr"),
            WeComMediaType::Voice
        );
        // a generic MIME falls back to the trait's kind
        assert_eq!(
            detect_wecom_media_type(MediaKind::Document, "application/pdf"),
            WeComMediaType::File
        );
        assert_eq!(
            detect_wecom_media_type(MediaKind::Image, "application/octet-stream"),
            WeComMediaType::Image
        );
    }

    #[test]
    fn mime_types_are_normalized_and_guessed_like_the_reference() {
        assert_eq!(
            normalize_content_type(Some("image/png; charset=x"), ""),
            "image/png"
        );
        assert_eq!(normalize_content_type(Some("IMAGE/JPEG"), ""), "image/jpeg");
        // generic labels defer to the filename
        assert_eq!(
            normalize_content_type(Some("application/octet-stream"), "clip.amr"),
            "audio/amr"
        );
        assert_eq!(
            normalize_content_type(Some("text/plain"), "notes.pdf"),
            "application/pdf"
        );
        assert_eq!(normalize_content_type(None, "photo.png"), "image/png");
        assert_eq!(
            normalize_content_type(None, "mystery.bin"),
            "application/octet-stream"
        );
        // a nameless payload keeps the generic type and a derived name
        assert_eq!(normalize_content_type(None, ""), "application/octet-stream");
        let prepared = prepare_outbound_media(MediaKind::Audio, b"abc", None, Some("audio/mpeg"));
        assert_eq!(prepared.filename, "wecom_voice.mp3");
        // path-ish names are reduced to a bare name
        let prepared =
            prepare_outbound_media(MediaKind::Document, b"abc", Some("/tmp/x/report.pdf"), None);
        assert_eq!(prepared.filename, "report.pdf");
        assert_eq!(prepared.media_type, WeComMediaType::File);
    }

    #[test]
    fn errcode_taxonomy_covers_the_reference_codes() {
        for (errcode, kind) in [
            (429, "rate_limited"),
            (45009, "rate_limited"),
            (45033, "rate_limited"),
            (846607, "rate_limited"),
            (400, "bad_format"),
            (403, "forbidden"),
            (40001, "forbidden"),
            (42001, "forbidden"),
            (404, "not_found"),
            (40007, "not_found"),
            (413, "too_long"),
            (45002, "too_long"),
            (846609, "transient"),
            (846604, "transient"),
            (500, "transient"),
            (503, "transient"),
            (123456, "unknown"),
        ] {
            assert_eq!(errcode_kind(errcode), kind, "errcode {errcode}");
        }
        assert!(is_retryable_kind("rate_limited"));
        assert!(is_retryable_kind("transient"));
        assert!(!is_retryable_kind("bad_format"));
        assert!(!is_retryable_kind("forbidden"));
    }

    #[test]
    fn refusals_report_the_shared_kinds() {
        let refused = MediaError::Refused {
            operation: "media upload init".to_string(),
            errcode: 846607,
            errmsg: "too many messages".to_string(),
        };
        let result = refused.into_result();
        assert!(!result.success);
        assert_eq!(result.error_kind.as_deref(), Some("rate_limited"));
        assert!(result.retryable);

        let rejected = MediaError::Rejected {
            message: "cannot upload empty media".to_string(),
            kind: "bad_format",
        };
        let result = rejected.into_result();
        assert_eq!(result.error_kind.as_deref(), Some("bad_format"));
        assert!(!result.retryable);
    }

    // ----- media: the reader must not swallow inbound frames -----

    #[tokio::test]
    async fn callbacks_arriving_during_an_upload_are_delivered_to_next_event() {
        let mut script = vec![inbound_callback(
            "chat-a",
            "m-in",
            "r-in",
            "while uploading",
        )];
        script.extend(upload_script("mid-11", 1, "image"));
        script.push(ack(json!({})));
        let (adapter, socket) = build(script);

        let result = adapter
            .send_media("chat-b", &image_bytes(b"abc"), None)
            .await;
        assert!(result.success, "{:?}", result.error);
        assert_eq!(
            cmds(&socket),
            vec![
                CMD_UPLOAD_INIT,
                CMD_UPLOAD_CHUNK,
                CMD_UPLOAD_FINISH,
                CMD_SEND
            ]
        );

        let event = adapter.next_event().await.unwrap().expect("stashed event");
        assert_eq!(event.text, "while uploading");
        assert_eq!(event.metadata["req_id"], "r-in");
        assert!(adapter.next_event().await.unwrap().is_none());
    }

    // ----- encoders -----

    #[test]
    fn md5_matches_the_rfc_1321_vectors() {
        assert_eq!(md5_hex(b""), "d41d8cd98f00b204e9800998ecf8427e");
        assert_eq!(md5_hex(b"a"), "0cc175b9c0f1b6a831c399e269772661");
        assert_eq!(md5_hex(b"abc"), "900150983cd24fb0d6963f7d28e17f72");
        assert_eq!(
            md5_hex(b"message digest"),
            "f96b697d7cb7938d525a2f31aaf161d0"
        );
        assert_eq!(
            md5_hex(b"abcdefghijklmnopqrstuvwxyz"),
            "c3fcd3d76192e4007dfb496cca67e13b"
        );
        assert_eq!(
            md5_hex(b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789"),
            "d174ab98d277d9f5a5611c2c9f419d9f"
        );
        assert_eq!(
            md5_hex(
                b"12345678901234567890123456789012345678901234567890123456789012345678901234567890"
            ),
            "57edf4a22be3c955ac49da2e2107b67a"
        );
    }

    #[test]
    fn base64_matches_the_rfc_4648_vectors() {
        assert_eq!(base64_encode(b""), "");
        assert_eq!(base64_encode(b"f"), "Zg==");
        assert_eq!(base64_encode(b"fo"), "Zm8=");
        assert_eq!(base64_encode(b"foo"), "Zm9v");
        assert_eq!(base64_encode(b"foob"), "Zm9vYg==");
        assert_eq!(base64_encode(b"fooba"), "Zm9vYmE=");
        assert_eq!(base64_encode(b"foobar"), "Zm9vYmFy");
        assert_eq!(base64_encode(b"hello media"), "aGVsbG8gbWVkaWE=");
        assert_eq!(base64_encode(&[0xff, 0x00, 0x7f]), "/wB/");
    }
}
