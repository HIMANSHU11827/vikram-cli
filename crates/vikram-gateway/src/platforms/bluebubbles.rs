//! BlueBubbles adapter — iMessage through a self-hosted BlueBubbles macOS server.
//!
//! Outbound is one REST call per message (`POST /api/v1/message/text`) — plus, for an attachment,
//! one `multipart/form-data` POST to `/api/v1/message/attachment` and optionally one follow-up text
//! call carrying the caption. Every call authenticates with the server password in the query
//! string, which is how BlueBubbles documents its API. The password rides in a URL rather than a
//! header because BlueBubbles has no token scheme.
//!
//! Inbound arrives as a webhook: BlueBubbles `POST`s `{"type": "new-message", "data": {...}}` to a
//! listener the host owns. [`BlueBubblesAdapter::parse_webhook`] only decodes such a body — this
//! module starts no server, so the listener, its 1 MiB body cap and the shared-ingress routing stay
//! with the host.
//!
//! Port of the REST + webhook half of `hermes-platforms/bluebubbles/adapter.py`. Deliberate
//! differences (full accounting in `docs/gateway-port-plan.md`):
//! - No webhook server, no webhook registration/unregistration (`/api/v1/webhook`) and no read
//!   receipts: all of that needs the listener and the Private API helper bundle, which this
//!   adapter does not own.
//! - No chat-GUID resolution (`POST /api/v1/chat/query` plus an LRU cache) and no
//!   "create a chat from an address" fallback: the caller passes a resolved `chatGuid`, so a chat
//!   the server does not know is reported as `not_found` instead of being silently created. The
//!   attachment path keeps the reference's pass-through branch of `_resolve_chat_guid`
//!   (`adapter.py:325-335`: a target containing `;` is already a chat GUID) and refuses anything
//!   else before touching the wire rather than querying for it.
//! - Outbound attachments are ported for [`MediaSource::Bytes`] only (see
//!   [`BlueBubblesAdapter`]'s `send_media`): `multipart/form-data` to
//!   `/api/v1/message/attachment` with the reference's `chatGuid` / `name` / `tempGuid` /
//!   `isAudioMessage` fields and one `attachment` file part. [`MediaSource::Url`] is refused: the
//!   reference first downloads the URL into the local cache (`cache_image_from_url`), and this
//!   adapter has no binary fetcher, so it makes zero requests instead of uploading nothing.
//! - No inbound attachment download: inbound media is reported as the BlueBubbles `guid`s in
//!   `MessageEvent::media_urls` for the host to fetch, since caching files is outside this
//!   adapter's surface.
//! - `send` never splits. BlueBubbles documents no text limit, so one request carries the whole
//!   message and chunking is left to the server; `splits_long_messages()` therefore stays `false`
//!   and `max_message_length()` keeps the trait default (the reference chunks at 4000 code points
//!   and turns blank-line paragraphs into separate bubbles).
//! - `reply_to` is not honored: anchoring a reply needs `method: "private-api"` plus
//!   `selectedMessageGuid`, which only works with the Private API helper installed, and this
//!   adapter keeps no server-capability state.
//! - The reference drops tapback reactions (`associatedMessageType` 2000-2005 / 3000-3005) and
//!   strips markdown with `strip_markdown(keep_link_targets=True)`. Neither is ported: `format_message`
//!   stays identity and tapbacks surface as ordinary messages.

use std::sync::atomic::{AtomicU64, Ordering};

use async_trait::async_trait;
use serde_json::{Value, json};

use crate::event::{MessageEvent, MessageType, Platform, SessionSource, platform_names};
use crate::platform::{
    ChatInfo, MediaKind, MediaSource, OutboundMedia, PlatformAdapter, SendResult,
};
use crate::transport::{HttpRequest, HttpResponse, SharedTransport, TransportError};

/// Characters of an error body kept in a [`SendResult::error`] message.
const BODY_EXCERPT_CHARS: usize = 200;

/// Largest attachment this adapter uploads.
///
/// The reference puts no size cap on `_send_attachment` — it only bounds the request with
/// `timeout=120` (`adapter.py:406`) — so the binding limit is iMessage's own: the service refuses
/// a single attachment over 100 MB. Refusing locally keeps a doomed upload off the wire and gives
/// the caller a `too_long` instead of an opaque server error.
const MAX_ATTACHMENT_BYTES: usize = 100 * 1024 * 1024;

/// Longest filename or content type kept from a caller value, so one absurd string cannot bloat a
/// multipart part header.
const MAX_PART_TOKEN_CHARS: usize = 200;

/// The BlueBubbles form field carrying the uploaded file (`files={"attachment": ...}`,
/// `adapter.py:407`).
const ATTACHMENT_FIELD: &str = "attachment";

/// Webhook event types that carry a user message. `updated-message` is an edit of one.
const MESSAGE_EVENT_TYPES: [&str; 2] = ["new-message", "updated-message"];

/// `data.style` value iMessage uses for a group chat.
const GROUP_CHAT_STYLE: i64 = 43;

/// The BlueBubbles send method: the macOS AppleScript bridge, available without the helper bundle.
const SEND_METHOD: &str = "apple-script";

/// Source of the per-call suffix in `tempGuid`. BlueBubbles uses the temporary GUID to correlate
/// its own echo of a send, so two sends must never share one.
static TEMP_GUID_SEQ: AtomicU64 = AtomicU64::new(0);

/// BlueBubbles platform adapter speaking the server's HTTP API with a server password.
///
/// The adapter is stateless: `connect` only proves the server answers, so a failed `connect` can be
/// retried with the same instance.
pub struct BlueBubblesAdapter {
    transport: SharedTransport,
    server: String,
    password: String,
}

impl BlueBubblesAdapter {
    /// Build an adapter over `transport` for the BlueBubbles server at `server`.
    ///
    /// `server` may omit the scheme (`http://` is assumed, as the reference does) and a trailing
    /// slash is dropped, so both `192.168.1.10:1234` and `http://192.168.1.10:1234/` work. Nothing
    /// is sent anywhere until a trait method is called.
    pub fn new(
        transport: SharedTransport,
        server: impl Into<String>,
        password: impl Into<String>,
    ) -> Self {
        Self {
            transport,
            server: normalize_server_url(&server.into()),
            password: password.into(),
        }
    }

    /// Absolute URL for an API path, with the password appended as the query parameter every
    /// BlueBubbles endpoint requires.
    ///
    /// None of the paths used here carries a query of its own, so the separator is always `?`
    /// (the reference switches to `&` for the `?with=participants` chat lookup).
    fn endpoint(&self, path: &str) -> String {
        format!(
            "{}{path}?password={}",
            self.server,
            encode_uri_component(&self.password)
        )
    }

    /// Upload one attachment and return its BlueBubbles `guid` plus the decoded answer body.
    ///
    /// Wire shape (`_send_attachment`, `adapter.py:388-418`): `POST /api/v1/message/attachment`
    /// as `multipart/form-data` with the reference's form fields in its order — `chatGuid`,
    /// `name`, `tempGuid`, and `isAudioMessage=true` only for a voice memo — followed by the file
    /// part `attachment` carrying the filename and content type. httpx emits `data` before
    /// `files`, so field-then-file is also the reference's byte order.
    ///
    /// Success is exactly "HTTP 200, envelope `status == 200`, `data.guid` present", a tightening
    /// of the reference's `result.get("status") == 200` (`adapter.py:412`) check: the reference
    /// still reports `success=True` with `message_id=None` when the envelope carries no `data`
    /// object, which the delivery ledger cannot tell apart from a delivered message. Everything
    /// else is a classified failure rather than an invented id.
    ///
    /// The failure is boxed so the success path (`String` + body) stays small.
    async fn post_attachment(
        &self,
        chat_guid: &str,
        filename: &str,
        mime: &str,
        bytes: &[u8],
        as_audio_message: bool,
    ) -> Result<(String, Value), Box<SendResult>> {
        let boundary = multipart_boundary();
        let temp_guid = attachment_temp_guid();
        let mut fields: Vec<(&str, &str)> = vec![
            ("chatGuid", chat_guid),
            ("name", filename),
            ("tempGuid", temp_guid.as_str()),
        ];
        if as_audio_message {
            fields.push(("isAudioMessage", "true"));
        }
        let request = HttpRequest::post(self.endpoint("/api/v1/message/attachment"))
            .header(
                "Content-Type",
                format!("multipart/form-data; boundary={boundary}"),
            )
            .body(encode_attachment_multipart(
                &boundary, &fields, filename, mime, bytes,
            ));
        let response = self
            .transport
            .execute(request)
            .await
            .map_err(|err| Box::new(transport_failure(&err)))?;
        // The reference raises on any non-2xx before reading the body (`adapter.py:408`).
        if !(200..300).contains(&response.status) {
            return Err(Box::new(classify_attachment_failure(&response)));
        }
        let Ok(body) = response.json() else {
            // A 2xx with an unreadable body cannot confirm the upload, so it is not a success.
            return Err(Box::new(SendResult::failed(
                format!(
                    "bluebubbles attachment upload returned a non-json body: {}",
                    body_excerpt(&response.body)
                ),
                Some("unknown"),
            )));
        };
        match body.get("status").and_then(Value::as_i64) {
            Some(200) => {}
            Some(status) => return Err(Box::new(failure_from_envelope(status, &body))),
            None => {
                return Err(Box::new(SendResult::failed(
                    format!(
                        "bluebubbles attachment upload returned no numeric status: {}",
                        body_excerpt(&response.body)
                    ),
                    Some("unknown"),
                )));
            }
        }
        match non_empty_str(body.get("data").and_then(|data| data.get("guid"))) {
            Some(guid) => Ok((guid.to_string(), body)),
            None => Err(Box::new(SendResult::failed(
                "bluebubbles attachment upload was accepted without a data.guid",
                Some("unknown"),
            ))),
        }
    }

    /// Parse one webhook delivery into the events it carries.
    ///
    /// BlueBubbles posts `{"type": "new-message", "data": {...}}`; `updated-message` carries an
    /// edited copy of the same shape. Everything else (typing indicators, read receipts, contact
    /// updates) and any message the account itself sent (`data.isFromMe`) yields no event, so the
    /// caller can acknowledge the delivery either way. A body that is not JSON is the only error:
    /// valid JSON lacking a routable message is simply quiet.
    ///
    /// Never panics: missing or mistyped fields degrade to empty values, and a message with no chat
    /// GUID yields no event (there would be nowhere to reply).
    pub fn parse_webhook(&self, body: &str) -> Result<Vec<MessageEvent>, String> {
        let payload: Value =
            serde_json::from_str(body).map_err(|err| format!("invalid json: {err}"))?;
        let event_type = payload
            .get("type")
            .and_then(Value::as_str)
            .unwrap_or_default();
        if !MESSAGE_EVENT_TYPES.contains(&event_type) {
            return Ok(Vec::new());
        }
        let Some(data) = payload.get("data").filter(|data| data.is_object()) else {
            return Ok(Vec::new());
        };
        if data.get("isFromMe").and_then(Value::as_bool) == Some(true) {
            return Ok(Vec::new());
        }

        // BlueBubbles v1.9+ nests the chat under `data.chats[0]`; older payloads carry `chatGuid`.
        let chat = data
            .get("chats")
            .and_then(Value::as_array)
            .and_then(|chats| chats.first());
        let chat_id = non_empty_str(chat.and_then(|chat| chat.get("guid")))
            .or_else(|| non_empty_str(data.get("chatGuid")));
        let Some(chat_id) = chat_id else {
            return Ok(Vec::new());
        };
        let chat_type = if chat.is_some_and(is_group_chat) || is_group_chat(data) {
            "group"
        } else {
            "dm"
        };

        let handle = data.get("handle");
        let user_id =
            non_empty_str(handle.and_then(|handle| handle.get("address"))).map(str::to_string);
        let user_name = handle
            .map(handle_display_name)
            .filter(|name| !name.is_empty());
        let message_id = non_empty_str(data.get("guid")).map(str::to_string);
        let media = collect_media(data);

        let mut source = SessionSource::new(bluebubbles_platform(), chat_id);
        source.chat_type = chat_type.to_string();
        source.user_id = user_id.clone();
        source.user_name = user_name.clone();
        source.message_id = message_id.clone();

        let mut event = MessageEvent::new(
            data.get("text").and_then(Value::as_str).unwrap_or_default(),
            source,
        );
        event.message_type = media.message_type;
        event.user_id = user_id;
        event.user_name = user_name;
        event.message_id = message_id;
        // `dateCreated` is the server's millisecond timestamp: the monotonic-enough update marker
        // `/restart` records so the next gateway advances past this delivery.
        event.platform_update_id = data.get("dateCreated").and_then(Value::as_i64);
        event.reply_to_message_id = non_empty_str(data.get("associatedMessageGuid"))
            .or_else(|| non_empty_str(data.get("threadOriginatorGuid")))
            .map(str::to_string);
        event.media_urls = media.urls;
        event.media_types = media.types;
        event.media_filenames = media.filenames;
        event.raw_message = Some(payload);
        Ok(vec![event])
    }
}

/// Media extracted from one webhook record.
#[derive(Default)]
struct Media {
    /// BlueBubbles attachment GUIDs, in payload order.
    urls: Vec<String>,
    /// MIME type per attachment (`""` when the server did not say).
    types: Vec<String>,
    /// Original transfer name per attachment (`""` = unknown), parallel to `urls`.
    filenames: Vec<String>,
    /// Classification derived from the first recognized MIME type.
    message_type: MessageType,
}

/// Collect `data.attachments[]` into parallel url/type/filename lists.
///
/// An attachment without a GUID cannot be fetched, so it is dropped. Only a GUID is reported as the
/// "url": downloading it needs the same password-bearing REST call this adapter leaves to the host.
fn collect_media(data: &Value) -> Media {
    let mut media = Media::default();
    let Some(attachments) = data.get("attachments").and_then(Value::as_array) else {
        return media;
    };
    for attachment in attachments {
        let Some(guid) = non_empty_str(attachment.get("guid")) else {
            continue;
        };
        let mime = attachment
            .get("mimeType")
            .and_then(Value::as_str)
            .unwrap_or_default();
        media.urls.push(guid.to_string());
        media.types.push(mime.to_string());
        media.filenames.push(
            attachment
                .get("transferName")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string(),
        );
        if media.message_type == MessageType::Text {
            media.message_type = message_type_for(mime);
        }
    }
    media
}

/// iMessage messages are text unless an attachment says otherwise.
fn message_type_for(mime: &str) -> MessageType {
    let mime = mime.to_ascii_lowercase();
    if mime.starts_with("image/") {
        MessageType::Photo
    } else if mime.starts_with("audio/") {
        MessageType::Audio
    } else if mime.starts_with("video/") {
        MessageType::Video
    } else if mime.is_empty() {
        MessageType::Text
    } else {
        MessageType::Document
    }
}

/// Is this payload node a group chat? `style == 43` is iMessage's group marker; `isGroup` is the
/// BlueBubbles flag that carries the same fact.
fn is_group_chat(node: &Value) -> bool {
    node.get("isGroup").and_then(Value::as_bool) == Some(true)
        || node.get("style").and_then(Value::as_i64) == Some(GROUP_CHAT_STYLE)
}

/// A handle's display name: `firstName` + `lastName`, joined when both are present.
fn handle_display_name(handle: &Value) -> String {
    let part = |key: &str| {
        handle
            .get(key)
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .map(str::to_string)
    };
    [part("firstName"), part("lastName")]
        .into_iter()
        .flatten()
        .collect::<Vec<String>>()
        .join(" ")
}

/// A non-blank string from a JSON node, or `None`.
fn non_empty_str(node: Option<&Value>) -> Option<&str> {
    node.and_then(Value::as_str)
        .filter(|value| !value.trim().is_empty())
}

/// Add `http://` when `raw` carries no scheme and drop trailing slashes.
fn normalize_server_url(raw: &str) -> String {
    let trimmed = raw.trim().trim_end_matches('/');
    let has_scheme = trimmed
        .get(..8)
        .is_some_and(|head| head.eq_ignore_ascii_case("https://"))
        || trimmed
            .get(..7)
            .is_some_and(|head| head.eq_ignore_ascii_case("http://"));
    if trimmed.is_empty() || has_scheme {
        return trimmed.to_string();
    }
    format!("http://{trimmed}")
}

/// Percent-encode one URI component. Unreserved characters are kept; everything else is escaped,
/// so a password containing `&`, `+` or spaces survives the query string intact.
fn encode_uri_component(value: &str) -> String {
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

/// A `tempGuid` unique to this call: process id plus a monotonic counter.
fn temp_guid() -> String {
    let sequence = TEMP_GUID_SEQ.fetch_add(1, Ordering::Relaxed);
    format!("vikram-{}-{sequence}", std::process::id())
}

/// The platform identity every event from this adapter carries.
fn bluebubbles_platform() -> Platform {
    // A compile-time constant, never wire input: `Platform::new` cannot refuse it.
    Platform::new(platform_names::BLUEBUBBLES).expect("bluebubbles is a known platform name")
}

/// A short single-line excerpt of a response body for error messages.
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

/// A send the server accepted but did not identify. Reported as delivered without a message id so
/// the delivery ledger never resends it blind.
fn delivered_without_id() -> SendResult {
    SendResult {
        success: true,
        message_id: None,
        error: None,
        raw_response: None,
        retryable: false,
        retry_after: None,
        continuation_message_ids: Vec::new(),
        error_kind: None,
    }
}

/// Map a non-2xx BlueBubbles response onto a classified [`SendResult`].
///
/// 429 → `rate_limited` (retryable, server delay preferred), 401/403 → `forbidden`, 404 →
/// `not_found`, 400 → `bad_format`, 5xx → `transient` (retryable), anything else → `unknown`.
fn classify_failure(response: &HttpResponse) -> SendResult {
    let decoded = response.json().ok();
    let message = format!(
        "bluebubbles api returned {}: {}",
        response.status,
        body_excerpt(&response.body)
    );
    match response.status {
        429 => {
            let retry_after = response.retry_after.or_else(|| {
                decoded
                    .as_ref()
                    .and_then(|body| body.get("retryAfter"))
                    .and_then(|value| {
                        value
                            .as_f64()
                            .or_else(|| value.as_str().and_then(|text| text.trim().parse().ok()))
                    })
            });
            SendResult::failed(message, Some("rate_limited")).retryable(retry_after)
        }
        401 | 403 => SendResult::failed(message, Some("forbidden")),
        404 => SendResult::failed(message, Some("not_found")),
        400 => SendResult::failed(message, Some("bad_format")),
        500..=599 => SendResult::failed(message, Some("transient")).retryable(None),
        _ => SendResult::failed(message, Some("unknown")),
    }
}

/// Classify a transport-level failure: no response arrived, so the send is worth retrying.
fn transport_failure(err: &TransportError) -> SendResult {
    SendResult::failed(
        format!("bluebubbles transport error: {err}"),
        Some("transient"),
    )
    .retryable(None)
}

// ----- Outbound media: chat-GUID resolution, naming, multipart framing, receipts -----

/// The reference's `_resolve_chat_guid` pass-through branch (`adapter.py:325-335`).
///
/// A target that already contains `;` is a BlueBubbles chat GUID (`iMessage;-;alice`) and is used
/// verbatim. A bare address or email would need `POST /api/v1/chat/query`, which this adapter does
/// not make (see the module notes), so it resolves to `None` and the caller refuses the send.
fn resolve_chat_guid(chat_id: &str) -> Option<&str> {
    let target = chat_id.trim();
    (!target.is_empty() && target.contains(';')).then_some(target)
}

/// Filename and content type used when the caller labels neither.
///
/// The reference takes the filename from `filename or os.path.basename(file_path)`
/// (`adapter.py:398`) and always posts `application/octet-stream` as the part's content type
/// (`adapter.py:407`). Held bytes have no path, so a per-kind default name stands in for the
/// basename, and the per-kind content type is used rather than octet-stream: BlueBubbles and
/// iMessage both sniff the payload, and the honest type is what `isAudioMessage` already promises
/// for a voice memo.
fn default_media_file(kind: MediaKind) -> (&'static str, &'static str) {
    match kind {
        MediaKind::Image => ("image.jpg", "image/jpeg"),
        MediaKind::Video => ("video.mp4", "video/mp4"),
        MediaKind::Audio => ("audio.mp3", "audio/mpeg"),
        MediaKind::Document => ("attachment.bin", "application/octet-stream"),
    }
}

/// The `name` field and the file part's filename: the caller's basename when it names one, else
/// the kind's default.
fn attachment_filename(media: &OutboundMedia) -> String {
    let (default_name, _) = default_media_file(media.kind);
    media
        .filename
        .as_deref()
        .map(sanitize_part_token)
        .map(|name| last_path_segment(&name).to_string())
        .filter(|name| !name.is_empty())
        .unwrap_or_else(|| default_name.to_string())
}

/// The file part's content type: the caller's MIME when it looks like one, else the kind default.
fn attachment_mime(media: &OutboundMedia) -> String {
    let (_, default_mime) = default_media_file(media.kind);
    media
        .mime
        .as_deref()
        .map(sanitize_part_token)
        .filter(|mime| mime.contains('/'))
        .unwrap_or_else(|| default_mime.to_string())
}

/// The last segment of an already-single-line path (`/tmp/x.png` → `x.png`).
fn last_path_segment(raw: &str) -> &str {
    raw.rsplit(['/', '\\']).next().unwrap_or(raw)
}

/// Make a caller string safe to interpolate into a `Content-Disposition` / `Content-Type` part
/// header: no CR/LF (header injection), no quote or backslash (parameter breakout), no other
/// control character, and no unbounded length.
fn sanitize_part_token(raw: &str) -> String {
    let cleaned: String = raw
        .chars()
        .filter(|ch| !ch.is_control() && *ch != '"' && *ch != '\\')
        .take(MAX_PART_TOKEN_CHARS)
        .collect();
    cleaned.trim().to_string()
}

/// Whole seconds since the Unix epoch in nanoseconds; `0` when the clock is unavailable or
/// pre-epoch, which only costs the nonce its clock component.
fn epoch_nanos() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|elapsed| elapsed.as_nanos() as u64)
        .unwrap_or(0)
}

/// A fresh `multipart/form-data` boundary for one upload.
///
/// The delimiter must not occur inside the parts, so it mixes the clock with a process counter
/// rather than being a constant.
fn multipart_boundary() -> String {
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let sequence = COUNTER.fetch_add(1, Ordering::Relaxed);
    format!(
        "----VIKRAMBlueBubblesBoundary{:016x}{sequence:08x}",
        epoch_nanos()
    )
}

/// Source of the per-call suffix in an attachment `tempGuid`, kept apart from the text-send
/// sequence so neither send kind perturbs the other.
static ATTACHMENT_TEMP_GUID_SEQ: AtomicU64 = AtomicU64::new(0);

/// The attachment `tempGuid`: 32 lowercase hex characters, the shape of the reference's
/// `uuid.uuid4().hex` (`adapter.py:403`), built from the clock, the process id and a counter so two
/// uploads in one process can never collide.
fn attachment_temp_guid() -> String {
    let sequence = ATTACHMENT_TEMP_GUID_SEQ.fetch_add(1, Ordering::Relaxed);
    format!(
        "{:016x}{:08x}{sequence:08x}",
        epoch_nanos(),
        std::process::id()
    )
}

/// Encode the body `POST /api/v1/message/attachment` expects, CRLF-framed per RFC 7578.
///
/// `fields` are the plain form fields in the reference's `data` order (`adapter.py:403-405`), then
/// one file part named [`ATTACHMENT_FIELD`] with its filename and content type, exactly the
/// `files={"attachment": (fname, payload, mime)}` mapping httpx emits after them
/// (`adapter.py:407`). Nothing here can fail — every part is bytes — so the caller never has to
/// invent a body.
fn encode_attachment_multipart(
    boundary: &str,
    fields: &[(&str, &str)],
    filename: &str,
    mime: &str,
    bytes: &[u8],
) -> Vec<u8> {
    let mut body = Vec::with_capacity(bytes.len() + filename.len() + mime.len() + 512);
    for (name, value) in fields {
        body.extend_from_slice(format!("--{boundary}\r\n").as_bytes());
        body.extend_from_slice(
            format!("Content-Disposition: form-data; name=\"{name}\"\r\n\r\n").as_bytes(),
        );
        body.extend_from_slice(value.as_bytes());
        body.extend_from_slice(b"\r\n");
    }
    body.extend_from_slice(format!("--{boundary}\r\n").as_bytes());
    body.extend_from_slice(
        format!(
            "Content-Disposition: form-data; name=\"{ATTACHMENT_FIELD}\"; filename=\"{filename}\"\r\n"
        )
        .as_bytes(),
    );
    body.extend_from_slice(format!("Content-Type: {mime}\r\n\r\n").as_bytes());
    body.extend_from_slice(bytes);
    body.extend_from_slice(b"\r\n");
    body.extend_from_slice(format!("--{boundary}--\r\n").as_bytes());
    body
}

/// Map a non-2xx attachment response onto a classified [`SendResult`].
///
/// Same taxonomy as a text send ([`classify_failure`]) plus `413 Payload Too Large` →
/// `too_long`: an oversized upload is a caller problem the ledger must not retry, and it is the
/// only status the attachment path adds.
fn classify_attachment_failure(response: &HttpResponse) -> SendResult {
    if response.status == 413 {
        return SendResult::failed(
            format!(
                "bluebubbles api returned 413: {}",
                body_excerpt(&response.body)
            ),
            Some("too_long"),
        );
    }
    classify_failure(response)
}

/// The `error_kind` for a status reported inside a BlueBubbles JSON envelope.
fn failure_kind_for_status(status: i64) -> &'static str {
    match status {
        400 => "bad_format",
        401 | 403 => "forbidden",
        404 => "not_found",
        413 => "too_long",
        429 => "rate_limited",
        500..=599 => "transient",
        _ => "unknown",
    }
}

/// Turn an HTTP-200 answer whose envelope `status` is not 200 into a classified failure, using the
/// envelope's `message` (`adapter.py:416`) when it carries one.
fn failure_from_envelope(status: i64, body: &Value) -> SendResult {
    let message = non_empty_str(body.get("message"))
        .map(str::to_string)
        .unwrap_or_else(|| format!("bluebubbles attachment upload returned status {status}"));
    let kind = failure_kind_for_status(status);
    let result = SendResult::failed(message, Some(kind));
    if kind == "rate_limited" || kind == "transient" {
        result.retryable(None)
    } else {
        result
    }
}

/// Refuse an upload that cannot succeed, before any request is framed.
///
/// Empty bytes are a caller bug (`bad_format`); bytes past [`MAX_ATTACHMENT_BYTES`] are `too_long`
/// and never retryable.
fn attachment_size_refusal(len: usize) -> Option<SendResult> {
    if len == 0 {
        return Some(SendResult::failed(
            "refusing to upload empty attachment bytes",
            Some("bad_format"),
        ));
    }
    (len > MAX_ATTACHMENT_BYTES).then(|| {
        SendResult::failed(
            format!(
                "bluebubbles attachment is {len} bytes; the iMessage cap is {MAX_ATTACHMENT_BYTES} bytes"
            ),
            Some("too_long"),
        )
    })
}

/// The result for an attachment that landed but whose caption did not.
///
/// The reference ignores the caption's outcome entirely (`await self.send(chat_id, caption)`,
/// `adapter.py:410-411`) and reports the attachment as a clean success. That reads as "media and
/// caption delivered" to the caller, so this adapter reports `success = false` while keeping the
/// delivered attachment's `guid` in `message_id` and the caption failure's kind in `error_kind`:
/// the caller learns exactly what is on iMessage, and `retryable = false` stops the ledger from
/// resending the attachment and duplicating it.
fn caption_failure(attachment_guid: &str, body: Value, caption: SendResult) -> SendResult {
    let detail = caption
        .error
        .unwrap_or_else(|| "the caption send reported no error detail".to_string());
    SendResult {
        success: false,
        message_id: Some(attachment_guid.to_string()),
        error: Some(format!(
            "bluebubbles attachment {attachment_guid} was delivered but its caption failed: {detail}"
        )),
        raw_response: Some(body),
        retryable: false,
        retry_after: None,
        continuation_message_ids: Vec::new(),
        error_kind: Some(caption.error_kind.unwrap_or_else(|| "unknown".to_string())),
    }
}

#[async_trait]
impl PlatformAdapter for BlueBubblesAdapter {
    /// Probe `GET /api/v1/server/info`; true when the server answers 200 with a `data` object.
    ///
    /// The reference also pings `/api/v1/ping` and records `private_api` / `helper_connected`; this
    /// adapter has no capabilities to gate on that, so one call is enough.
    async fn connect(&mut self, _is_reconnect: bool) -> bool {
        let request = HttpRequest::get(self.endpoint("/api/v1/server/info"));
        match self.transport.execute(request).await {
            Ok(response) if response.status == 200 => response
                .json()
                .ok()
                .as_ref()
                .and_then(|body| body.get("data"))
                .is_some_and(Value::is_object),
            _ => false,
        }
    }

    /// Nothing to tear down: the adapter holds no connection, only a transport handle and no
    /// registered webhook.
    async fn disconnect(&mut self) {}

    /// POST the whole message to `/api/v1/message/text` as one iMessage bubble.
    ///
    /// BlueBubbles documents no text limit, so the message is never chunked here — one request
    /// carries it and the server decides how to split it. `reply_to` is not used (see the module
    /// notes); `metadata` has no wire equivalent on this endpoint. Blank content is refused before
    /// any request is made.
    async fn send(
        &self,
        chat_id: &str,
        content: &str,
        _reply_to: Option<&str>,
        _metadata: Option<Value>,
    ) -> SendResult {
        if content.trim().is_empty() {
            // Caller bug, not a platform problem: refuse before touching the network.
            return SendResult::failed("Refusing to send empty message", Some("bad_format"));
        }
        let payload = json!({
            "chatGuid": chat_id,
            "tempGuid": temp_guid(),
            "message": content,
            "method": SEND_METHOD,
        });
        let request = HttpRequest::json(self.endpoint("/api/v1/message/text"), &payload);
        match self.transport.execute(request).await {
            Ok(response) if response.ok() => match response.json() {
                Ok(body) => match non_empty_str(body.get("data").and_then(|data| data.get("guid")))
                {
                    Some(guid) => SendResult::ok(guid),
                    None => delivered_without_id(),
                },
                // Accepted, but the body was not JSON: the message is on its way regardless.
                Err(_) => delivered_without_id(),
            },
            Ok(response) => classify_failure(&response),
            Err(err) => transport_failure(&err),
        }
    }

    /// BlueBubbles accepts file uploads, so the media path is real: [`Self::send_media`].
    fn supports_media(&self) -> bool {
        true
    }

    /// Upload one attachment, then send its caption as a follow-up bubble.
    ///
    /// Port of `_send_attachment` (`adapter.py:388-418`), reached by the reference's
    /// `send_image_file` / `send_voice` / `send_video` / `send_document`:
    ///
    /// 1. Resolve the chat GUID ([`resolve_chat_guid`]: the reference's pass-through branch only —
    ///    a bare address is refused here rather than looked up, see the module notes).
    /// 2. Refuse locally, with zero requests, for empty bytes (`bad_format`), bytes past
    ///    [`MAX_ATTACHMENT_BYTES`] (`too_long`), and [`MediaSource::Url`] (`bad_format`).
    /// 3. `POST /api/v1/message/attachment` as `multipart/form-data`: `chatGuid`, `name`,
    ///    `tempGuid`, `isAudioMessage=true` for [`MediaKind::Audio`], then the `attachment` file
    ///    part. Success requires HTTP 200, envelope `status == 200` and a `data.guid`
    ///    ([`BlueBubblesAdapter::post_attachment`]).
    /// 4. Send the caption through [`Self::send`] — one plain text bubble, which is what the
    ///    reference's `await self.send(chat_id, caption)` produces — and report the attachment's
    ///    `guid` as the message id. A caption failure is surfaced as a failure with the attachment
    ///    guid kept ([`caption_failure`]): half the send landing is not a success, and a caption
    ///    is only attempted once the attachment is confirmed, so a failed upload never leaves a
    ///    stray bubble behind.
    ///
    /// `reply_to` is not honored, for the same Private-API reason text sends ignore it. Every
    /// failure carries an [`crate::platform::SEND_ERROR_KINDS`] kind: 429 → `rate_limited`, 401/403
    /// → `forbidden`, 404 → `not_found`, 413 → `too_long`, 5xx and transport errors →
    /// `transient`.
    async fn send_media(
        &self,
        chat_id: &str,
        media: &OutboundMedia,
        _reply_to: Option<&str>,
    ) -> SendResult {
        let Some(chat_guid) = resolve_chat_guid(chat_id) else {
            return SendResult::failed(
                format!(
                    "bluebubbles chat id `{chat_id}` is not a resolved chat GUID (no `;`); resolve \
                     it with /api/v1/chat/query before sending media"
                ),
                Some("not_found"),
            );
        };
        let bytes = match &media.source {
            MediaSource::Bytes(bytes) => bytes.as_slice(),
            MediaSource::Url(url) => {
                // The reference downloads the URL into the media cache first; there is no binary
                // fetcher here, so refuse instead of uploading nothing.
                return SendResult::failed(
                    format!(
                        "bluebubbles uploads held bytes only; `{}` is a URL this adapter cannot \
                         fetch",
                        url.trim()
                    ),
                    Some("bad_format"),
                );
            }
        };
        if let Some(refusal) = attachment_size_refusal(bytes.len()) {
            return refusal;
        }
        let filename = attachment_filename(media);
        let mime = attachment_mime(media);
        let as_audio_message = media.kind == MediaKind::Audio;
        let (attachment_guid, body) = match self
            .post_attachment(chat_guid, &filename, &mime, bytes, as_audio_message)
            .await
        {
            Ok(uploaded) => uploaded,
            Err(failure) => return *failure,
        };
        if let Some(caption) = media
            .caption
            .as_deref()
            .map(str::trim)
            .filter(|caption| !caption.is_empty())
        {
            let caption_result = self.send(chat_id, caption, None, None).await;
            if !caption_result.success {
                return caption_failure(&attachment_guid, body, caption_result);
            }
        }
        SendResult {
            success: true,
            message_id: Some(attachment_guid),
            error: None,
            raw_response: Some(body),
            retryable: false,
            retry_after: None,
            continuation_message_ids: Vec::new(),
            error_kind: None,
        }
    }

    /// `GET /api/v1/chat/{guid}`: display name plus a `dm` / `group` classification.
    ///
    /// The name falls back from `data.displayName` to `data.chatIdentifier` to the chat id, and any
    /// failure degrades to `{name: chat_id, chat_type: "dm"}` so routing still works.
    async fn get_chat_info(&self, chat_id: &str) -> ChatInfo {
        let fallback = ChatInfo {
            name: chat_id.to_string(),
            chat_type: "dm".to_string(),
        };
        let request = HttpRequest::get(
            self.endpoint(&format!("/api/v1/chat/{}", encode_uri_component(chat_id))),
        );
        let Ok(response) = self.transport.execute(request).await else {
            return fallback;
        };
        if !response.ok() {
            return fallback;
        }
        let Ok(body) = response.json() else {
            return fallback;
        };
        let data = body.get("data");
        let name = non_empty_str(data.and_then(|data| data.get("displayName")))
            .or_else(|| non_empty_str(data.and_then(|data| data.get("chatIdentifier"))))
            .unwrap_or(chat_id);
        let chat_type = if data.is_some_and(is_group_chat) {
            "group"
        } else {
            "dm"
        };
        ChatInfo {
            name: name.to_string(),
            chat_type: chat_type.to_string(),
        }
    }

    /// No-op: BlueBubbles has no typing endpoint.
    ///
    /// The reference fakes it with `POST /api/v1/chat/{guid}/typing`, which only exists when the
    /// Private API helper bundle is installed and connected — state this adapter does not track, so
    /// it never fires a request that would 404 on a plain server.
    async fn send_typing(&self, _chat_id: &str, _metadata: Option<Value>) {}
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::transport::{HttpMethod, Transport};
    use std::collections::VecDeque;
    use std::sync::{Arc, Mutex, MutexGuard};

    /// Unpoison a lock: a panicking test thread must not cascade into confusing failures.
    fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
        mutex
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    /// Records every request and replays queued responses (a 200 send acknowledgement once drained).
    #[derive(Default)]
    struct MockTransport {
        requests: Mutex<Vec<HttpRequest>>,
        responses: Mutex<VecDeque<Result<HttpResponse, TransportError>>>,
    }

    impl MockTransport {
        fn with(responses: Vec<HttpResponse>) -> Arc<Self> {
            let mock = MockTransport::default();
            lock(&mock.responses).extend(responses.into_iter().map(Ok));
            Arc::new(mock)
        }

        fn failing(err: TransportError) -> Arc<Self> {
            let mock = MockTransport::default();
            lock(&mock.responses).push_back(Err(err));
            Arc::new(mock)
        }

        fn requests(&self) -> Vec<HttpRequest> {
            lock(&self.requests).clone()
        }

        fn request_count(&self) -> usize {
            lock(&self.requests).len()
        }
    }

    #[async_trait]
    impl Transport for MockTransport {
        async fn execute(&self, request: HttpRequest) -> Result<HttpResponse, TransportError> {
            lock(&self.requests).push(request);
            match lock(&self.responses).pop_front() {
                Some(result) => result,
                None => Ok(json_response(
                    200,
                    json!({"status": 200, "data": {"guid": "sent-guid"}}),
                )),
            }
        }
    }

    fn json_response(status: u16, body: Value) -> HttpResponse {
        HttpResponse {
            status,
            body: body.to_string(),
            retry_after: None,
        }
    }

    fn plain_response(status: u16, body: &str) -> HttpResponse {
        HttpResponse {
            status,
            body: body.to_string(),
            retry_after: None,
        }
    }

    fn adapter(mock: &Arc<MockTransport>) -> BlueBubblesAdapter {
        BlueBubblesAdapter::new(mock.clone(), "http://bb.local:1234", "s3cr3t")
    }

    fn body_json(request: &HttpRequest) -> Value {
        serde_json::from_slice(request.body.as_deref().unwrap_or_default()).unwrap_or(Value::Null)
    }

    fn header<'a>(request: &'a HttpRequest, name: &str) -> Option<&'a str> {
        request
            .headers
            .iter()
            .find(|(key, _)| key.eq_ignore_ascii_case(name))
            .map(|(_, value)| value.as_str())
    }

    #[tokio::test]
    async fn connect_probes_server_info_with_the_password() {
        let mock = MockTransport::with(vec![json_response(
            200,
            json!({"status": 200, "data": {"private_api": true, "helper_connected": false}}),
        )]);
        let mut bluebubbles = adapter(&mock);
        assert!(bluebubbles.connect(false).await);

        let requests = mock.requests();
        assert_eq!(requests.len(), 1);
        assert_eq!(requests[0].method, HttpMethod::Get);
        assert_eq!(
            requests[0].url,
            "http://bb.local:1234/api/v1/server/info?password=s3cr3t"
        );
        assert!(requests[0].body.is_none());
    }

    #[tokio::test]
    async fn connect_percent_encodes_the_password_and_tolerates_a_scheme_less_server() {
        let mock = MockTransport::with(vec![json_response(200, json!({"data": {}}))]);
        let mut bluebubbles =
            BlueBubblesAdapter::new(mock.clone(), "192.168.1.10:1234/", "p@ss word/+");
        assert!(bluebubbles.connect(false).await);
        assert_eq!(
            mock.requests()[0].url,
            "http://192.168.1.10:1234/api/v1/server/info?password=p%40ss%20word%2F%2B"
        );

        let secure = MockTransport::with(vec![json_response(200, json!({"data": {}}))]);
        let mut bluebubbles = BlueBubblesAdapter::new(secure.clone(), "HTTPS://bb.example", "pw");
        assert!(bluebubbles.connect(false).await);
        assert!(secure.requests()[0].url.starts_with("HTTPS://bb.example/"));
    }

    #[tokio::test]
    async fn connect_fails_without_a_server_data_object() {
        let mock = MockTransport::with(vec![
            json_response(401, json!({"message": "unauthorized"})),
            json_response(200, json!({"status": 200})),
            json_response(200, json!({"data": []})),
        ]);
        let mut bluebubbles = adapter(&mock);
        assert!(!bluebubbles.connect(false).await);
        assert!(!bluebubbles.connect(true).await);
        assert!(!bluebubbles.connect(true).await);

        let offline = MockTransport::failing(TransportError::Request("boom".to_string()));
        assert!(!adapter(&offline).connect(false).await);
    }

    #[tokio::test]
    async fn send_posts_the_documented_json_body() {
        let mock = MockTransport::with(vec![json_response(
            200,
            json!({"status": 200, "data": {"guid": "iMessage;-;chat-1:abc"}}),
        )]);
        let result = adapter(&mock)
            .send("iMessage;-;alice", "hello there", None, None)
            .await;
        assert!(result.success, "{:?}", result.error);
        assert_eq!(result.message_id.as_deref(), Some("iMessage;-;chat-1:abc"));
        assert!(!result.retryable);

        let requests = mock.requests();
        assert_eq!(requests.len(), 1);
        assert_eq!(requests[0].method, HttpMethod::Post);
        assert_eq!(
            requests[0].url,
            "http://bb.local:1234/api/v1/message/text?password=s3cr3t"
        );
        assert_eq!(
            header(&requests[0], "Content-Type"),
            Some("application/json")
        );
        let body = body_json(&requests[0]);
        assert_eq!(body["chatGuid"], "iMessage;-;alice");
        assert_eq!(body["message"], "hello there");
        assert_eq!(body["method"], "apple-script");
        assert!(
            body["tempGuid"]
                .as_str()
                .is_some_and(|guid| !guid.is_empty()),
            "a tempGuid must be sent: {body}"
        );
    }

    #[tokio::test]
    async fn temp_guid_is_unique_across_sends() {
        let mock = MockTransport::with(vec![
            json_response(200, json!({"data": {"guid": "g1"}})),
            json_response(200, json!({"data": {"guid": "g2"}})),
        ]);
        let bluebubbles = adapter(&mock);
        assert!(bluebubbles.send("chat", "one", None, None).await.success);
        assert!(bluebubbles.send("chat", "two", None, None).await.success);

        let requests = mock.requests();
        assert_eq!(requests.len(), 2);
        let first = body_json(&requests[0]);
        let second = body_json(&requests[1]);
        assert_ne!(first["tempGuid"], second["tempGuid"]);
        assert_ne!(first["message"], second["message"]);
    }

    #[tokio::test]
    async fn send_without_a_guid_in_the_response_reports_delivery_without_an_id() {
        let mock = MockTransport::with(vec![json_response(200, json!({"status": 200}))]);
        let result = adapter(&mock).send("chat", "hi", None, None).await;
        assert!(result.success);
        assert_eq!(result.message_id, None);

        let unreadable = MockTransport::with(vec![plain_response(200, "ok")]);
        let result = adapter(&unreadable).send("chat", "hi", None, None).await;
        assert!(result.success);
        assert_eq!(result.message_id, None);
    }

    #[tokio::test]
    async fn rate_limit_is_retryable_and_keeps_the_retry_after() {
        let mock = MockTransport::with(vec![HttpResponse {
            status: 429,
            body: "{}".to_string(),
            retry_after: Some(2.5),
        }]);
        let result = adapter(&mock).send("chat", "hi", None, None).await;
        assert!(!result.success);
        assert!(result.retryable);
        assert_eq!(result.retry_after, Some(2.5));
        assert_eq!(result.error_kind.as_deref(), Some("rate_limited"));

        let from_body = MockTransport::with(vec![json_response(
            429,
            json!({"retryAfter": "1.25", "message": "too many requests"}),
        )]);
        let result = adapter(&from_body).send("chat", "hi", None, None).await;
        assert!(result.retryable);
        assert_eq!(result.retry_after, Some(1.25));
        assert_eq!(result.error_kind.as_deref(), Some("rate_limited"));
    }

    #[tokio::test]
    async fn unauthorized_and_forbidden_map_to_forbidden() {
        for status in [401u16, 403] {
            let mock = MockTransport::with(vec![json_response(
                status,
                json!({"message": "bad password"}),
            )]);
            let result = adapter(&mock).send("chat", "hi", None, None).await;
            assert!(!result.success, "{status}");
            assert_eq!(result.error_kind.as_deref(), Some("forbidden"), "{status}");
            assert!(!result.retryable, "{status}");
            assert!(
                result
                    .error
                    .as_deref()
                    .unwrap_or_default()
                    .contains(&status.to_string()),
                "the status should surface in the message"
            );
        }
    }

    #[tokio::test]
    async fn unknown_chat_is_not_found_and_a_blank_message_is_bad_format() {
        let missing = MockTransport::with(vec![json_response(
            404,
            json!({"message": "Chat not found"}),
        )]);
        let not_found = adapter(&missing).send("chat", "hi", None, None).await;
        assert_eq!(not_found.error_kind.as_deref(), Some("not_found"));
        assert!(!not_found.retryable);

        let invalid = MockTransport::with(vec![json_response(
            400,
            json!({"message": "message is required"}),
        )]);
        let bad_format = adapter(&invalid).send("chat", "hi", None, None).await;
        assert_eq!(bad_format.error_kind.as_deref(), Some("bad_format"));
        assert!(!bad_format.retryable);
    }

    #[tokio::test]
    async fn server_and_transport_errors_are_transient() {
        let server = MockTransport::with(vec![json_response(
            503,
            json!({"message": "Service Unavailable"}),
        )]);
        let transient = adapter(&server).send("chat", "hi", None, None).await;
        assert_eq!(transient.error_kind.as_deref(), Some("transient"));
        assert!(transient.retryable);

        let offline = MockTransport::failing(TransportError::Request("connection reset".into()));
        let unreachable = adapter(&offline).send("chat", "hi", None, None).await;
        assert_eq!(unreachable.error_kind.as_deref(), Some("transient"));
        assert!(unreachable.retryable);
    }

    #[tokio::test]
    async fn blank_content_is_rejected_without_a_request() {
        let mock = MockTransport::with(Vec::new());
        let bluebubbles = adapter(&mock);
        for content in ["", "   ", "\n\t \r\n"] {
            let result = bluebubbles.send("chat", content, None, None).await;
            assert!(!result.success, "content {content:?}");
            assert_eq!(result.error_kind.as_deref(), Some("bad_format"));
        }
        assert_eq!(mock.request_count(), 0);
    }

    #[tokio::test]
    async fn chat_info_reads_the_display_name_and_group_style() {
        let group = MockTransport::with(vec![json_response(
            200,
            json!({"data": {"guid": "chat-1", "displayName": "Family", "style": 43}}),
        )]);
        assert_eq!(
            adapter(&group).get_chat_info("chat-1").await,
            ChatInfo {
                name: "Family".to_string(),
                chat_type: "group".to_string(),
            }
        );
        let requests = group.requests();
        assert_eq!(requests[0].method, HttpMethod::Get);
        assert_eq!(
            requests[0].url,
            "http://bb.local:1234/api/v1/chat/chat-1?password=s3cr3t"
        );

        let flagged = MockTransport::with(vec![json_response(
            200,
            json!({"data": {"displayName": "Crew", "isGroup": true}}),
        )]);
        let info = adapter(&flagged).get_chat_info("chat-2").await;
        assert_eq!(info.chat_type, "group");
        assert_eq!(info.name, "Crew");

        // No display name: the identifier names the chat, and style 45 is a 1:1 thread.
        let direct = MockTransport::with(vec![json_response(
            200,
            json!({"data": {"chatIdentifier": "alice@example.com", "style": 45}}),
        )]);
        assert_eq!(
            adapter(&direct).get_chat_info("chat-3").await,
            ChatInfo {
                name: "alice@example.com".to_string(),
                chat_type: "dm".to_string(),
            }
        );
    }

    #[tokio::test]
    async fn chat_info_percent_encodes_the_guid() {
        let mock = MockTransport::with(vec![json_response(200, json!({"data": {}}))]);
        let info = adapter(&mock)
            .get_chat_info("iMessage;+;alice@example.com")
            .await;
        assert_eq!(info.name, "iMessage;+;alice@example.com");
        assert_eq!(info.chat_type, "dm");
        assert_eq!(
            mock.requests()[0].url,
            "http://bb.local:1234/api/v1/chat/iMessage%3B%2B%3Balice%40example.com?password=s3cr3t"
        );
    }

    #[tokio::test]
    async fn chat_info_degrades_to_the_chat_id() {
        let missing = MockTransport::with(vec![json_response(
            404,
            json!({"message": "Chat not found"}),
        )]);
        assert_eq!(
            adapter(&missing).get_chat_info("chat-9").await,
            ChatInfo {
                name: "chat-9".to_string(),
                chat_type: "dm".to_string(),
            }
        );

        let broken = MockTransport::with(vec![plain_response(200, "not json")]);
        assert_eq!(
            adapter(&broken).get_chat_info("chat-9").await.name,
            "chat-9"
        );

        let offline = MockTransport::failing(TransportError::Request("down".into()));
        assert_eq!(
            adapter(&offline).get_chat_info("chat-9").await.chat_type,
            "dm"
        );

        // A blank displayName must not win over the chat id.
        let blank = MockTransport::with(vec![json_response(
            200,
            json!({"data": {"displayName": "  "}}),
        )]);
        assert_eq!(adapter(&blank).get_chat_info("chat-9").await.name, "chat-9");
    }

    #[tokio::test]
    async fn typing_is_a_documented_no_op() {
        let mock = MockTransport::with(Vec::new());
        let bluebubbles = adapter(&mock);
        bluebubbles.send_typing("chat", None).await;
        bluebubbles.stop_typing("chat").await;
        assert_eq!(mock.request_count(), 0);
    }

    fn dm_webhook() -> String {
        json!({
            "type": "new-message",
            "data": {
                "guid": "msg-1",
                "text": "hello from imessage",
                "dateCreated": 1_712_345_678_901i64,
                "isFromMe": false,
                "chatGuid": "iMessage;-;+15551234567",
                "chats": [{"guid": "iMessage;-;+15551234567", "style": 45}],
                "handle": {"address": "+15551234567"},
                "associatedMessageGuid": "msg-0"
            }
        })
        .to_string()
    }

    #[tokio::test]
    async fn webhook_new_message_from_a_dm_becomes_one_event() {
        let mock = MockTransport::with(Vec::new());
        let events = adapter(&mock).parse_webhook(&dm_webhook()).expect("parsed");
        assert_eq!(events.len(), 1);
        let event = &events[0];
        assert_eq!(event.text, "hello from imessage");
        assert_eq!(event.message_type, MessageType::Text);
        assert_eq!(event.source.platform.as_str(), "bluebubbles");
        assert_eq!(event.source.chat_id, "iMessage;-;+15551234567");
        assert_eq!(event.source.chat_type, "dm");
        assert_eq!(event.user_id.as_deref(), Some("+15551234567"));
        assert_eq!(event.source.user_id.as_deref(), Some("+15551234567"));
        assert_eq!(event.message_id.as_deref(), Some("msg-1"));
        assert_eq!(event.source.message_id.as_deref(), Some("msg-1"));
        assert_eq!(event.platform_update_id, Some(1_712_345_678_901));
        assert_eq!(event.reply_to_message_id.as_deref(), Some("msg-0"));
        assert!(event.raw_message.is_some());
        assert_eq!(
            mock.request_count(),
            0,
            "parsing must not touch the network"
        );
    }

    #[tokio::test]
    async fn webhook_group_message_carries_the_sender_name_and_attachment() {
        let body = json!({
            "type": "updated-message",
            "data": {
                "guid": "msg-2",
                "text": "look at this",
                "isFromMe": false,
                "chats": [{"guid": "iMessage;+;group-42", "style": 43, "isGroup": true}],
                "handle": {"address": "ada@example.com", "firstName": "Ada", "lastName": "Lovelace"},
                "threadOriginatorGuid": "msg-1",
                "attachments": [
                    {
                        "guid": "att-1",
                        "mimeType": "image/jpeg",
                        "transferName": "photo.jpg"
                    },
                    {"mimeType": "image/png"},
                    {"guid": "att-2", "mimeType": "application/pdf", "transferName": "notes.pdf"}
                ]
            }
        })
        .to_string();

        let mock = MockTransport::with(Vec::new());
        let events = adapter(&mock).parse_webhook(&body).expect("parsed");
        assert_eq!(events.len(), 1);
        let event = &events[0];
        assert_eq!(event.source.chat_id, "iMessage;+;group-42");
        assert_eq!(event.source.chat_type, "group");
        assert_eq!(event.user_id.as_deref(), Some("ada@example.com"));
        assert_eq!(event.user_name.as_deref(), Some("Ada Lovelace"));
        assert_eq!(event.source.user_name.as_deref(), Some("Ada Lovelace"));
        assert_eq!(event.reply_to_message_id.as_deref(), Some("msg-1"));
        assert_eq!(event.message_type, MessageType::Photo);
        // The GUID-less attachment is dropped; names and types stay parallel.
        assert_eq!(event.media_urls, vec!["att-1", "att-2"]);
        assert_eq!(event.media_types, vec!["image/jpeg", "application/pdf"]);
        assert_eq!(event.media_filenames, vec!["photo.jpg", "notes.pdf"]);
    }

    #[tokio::test]
    async fn webhook_chat_id_falls_back_to_the_data_chat_guid() {
        let body = json!({
            "type": "new-message",
            "data": {
                "guid": "msg-3",
                "text": "no chats array",
                "chatGuid": "iMessage;-;fallback",
                "handle": {"address": "bob@example.com"}
            }
        })
        .to_string();
        let mock = MockTransport::with(Vec::new());
        let events = adapter(&mock).parse_webhook(&body).expect("parsed");
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].source.chat_id, "iMessage;-;fallback");
        assert_eq!(events[0].source.chat_type, "dm");
        assert_eq!(events[0].user_name, None);
    }

    #[tokio::test]
    async fn webhook_skips_messages_the_account_sent() {
        let body = json!({
            "type": "new-message",
            "data": {"guid": "msg-4", "text": "from me", "isFromMe": true, "chatGuid": "c"}
        })
        .to_string();
        let mock = MockTransport::with(Vec::new());
        assert!(adapter(&mock).parse_webhook(&body).expect("ok").is_empty());
    }

    #[tokio::test]
    async fn webhook_ignores_other_event_types_and_unroutable_bodies() {
        let mock = MockTransport::with(Vec::new());
        let bluebubbles = adapter(&mock);
        for body in [
            json!({"type": "typing-indicator", "data": {"guid": "m", "text": "hi"}}).to_string(),
            json!({"type": "read-receipt", "data": {}}).to_string(),
            json!({"data": {"text": "no type at all"}}).to_string(),
            json!({"type": "new-message"}).to_string(),
            json!({"type": "new-message", "data": {"text": "no chat"}}).to_string(),
        ] {
            assert!(
                bluebubbles
                    .parse_webhook(&body)
                    .expect("valid json")
                    .is_empty(),
                "body {body} should yield no event"
            );
        }

        assert!(bluebubbles.parse_webhook("not json").is_err());
        assert!(bluebubbles.parse_webhook("").is_err());
    }

    #[tokio::test]
    async fn capability_flags_stay_conservative() {
        let mock = MockTransport::with(Vec::new());
        let mut bluebubbles = adapter(&mock);
        // BlueBubbles documents no text limit, so the adapter never claims to split.
        assert!(!bluebubbles.splits_long_messages());
        assert_eq!(bluebubbles.max_message_length(), 4096);
        assert_eq!(bluebubbles.message_length("héllo"), 5);
        assert_eq!(bluebubbles.typed_command_prefix(), "/");
        // iMessage strips markdown; the reference overrides format_message to do that itself.
        assert!(!bluebubbles.supports_code_blocks());
        assert_eq!(bluebubbles.format_message("**bold**"), "**bold**");
        // The default media path degrades to a text send, which this endpoint supports.
        assert!(
            bluebubbles
                .send_image("chat", "https://example.test/a.png", Some("cap"), None)
                .await
                .success
        );
        bluebubbles.disconnect().await;
    }

    // ----- Outbound media -----

    /// One decoded part of a recorded `multipart/form-data` body.
    struct Part {
        name: String,
        filename: Option<String>,
        content_type: Option<String>,
        body: String,
    }

    /// The boundary a recorded request advertises (`""` when it is not multipart).
    fn boundary_of(request: &HttpRequest) -> String {
        header(request, "Content-Type")
            .and_then(|value| value.strip_prefix("multipart/form-data; boundary="))
            .unwrap_or_default()
            .to_string()
    }

    /// Decode a recorded multipart body. Segments are read as lossy UTF-8: the fixtures below are
    /// ASCII, so no payload byte is lost.
    fn multipart_parts(request: &HttpRequest) -> Vec<Part> {
        let boundary = boundary_of(request);
        let body: String =
            String::from_utf8_lossy(request.body.as_deref().unwrap_or_default()).into_owned();
        let delimiter = format!("--{boundary}");
        let mut parts = Vec::new();
        for segment in body.split(&delimiter) {
            let segment = segment.strip_prefix("\r\n").unwrap_or(segment);
            // Every real part is CRLF-terminated; the preamble and the closing `--` are not.
            let Some(segment) = segment.strip_suffix("\r\n") else {
                continue;
            };
            let Some((head, content)) = segment.split_once("\r\n\r\n") else {
                continue;
            };
            let mut part = Part {
                name: String::new(),
                filename: None,
                content_type: None,
                body: content.to_string(),
            };
            for line in head.split("\r\n") {
                if let Some(rest) = line.strip_prefix("Content-Disposition:") {
                    for param in rest.split(';').map(str::trim) {
                        if let Some(value) = param.strip_prefix("name=\"") {
                            part.name = value.trim_end_matches('"').to_string();
                        } else if let Some(value) = param.strip_prefix("filename=\"") {
                            part.filename = Some(value.trim_end_matches('"').to_string());
                        }
                    }
                } else if let Some(value) = line.strip_prefix("Content-Type:") {
                    part.content_type = Some(value.trim().to_string());
                }
            }
            parts.push(part);
        }
        parts
    }

    /// The value of one plain (non-file) form field.
    fn field(parts: &[Part], name: &str) -> Option<String> {
        parts
            .iter()
            .find(|part| part.name == name && part.filename.is_none())
            .map(|part| part.body.clone())
    }

    /// The single file part of an upload.
    fn file_part(parts: &[Part]) -> Option<&Part> {
        parts.iter().find(|part| part.filename.is_some())
    }

    fn image_media(bytes: &[u8]) -> OutboundMedia {
        OutboundMedia::new(MediaKind::Image, MediaSource::Bytes(bytes.to_vec()))
    }

    /// A 200 answer shaped like BlueBubbles' attachment acknowledgement.
    fn attachment_ok(guid: &str) -> HttpResponse {
        json_response(200, json!({"status": 200, "data": {"guid": guid}}))
    }

    #[tokio::test]
    async fn supports_media_and_a_url_source_is_refused_without_a_request() {
        let mock = MockTransport::with(Vec::new());
        let bluebubbles = adapter(&mock);
        assert!(bluebubbles.supports_media());

        let result = bluebubbles
            .send_media(
                "iMessage;-;alice",
                &OutboundMedia::image_url("https://example.test/a.png"),
                None,
            )
            .await;
        assert!(!result.success);
        assert_eq!(result.error_kind.as_deref(), Some("bad_format"));
        assert!(!result.retryable);
        let error = result.error.as_deref().unwrap_or_default();
        assert!(error.contains("https://example.test/a.png"), "{error}");
        assert!(error.contains("fetch"), "{error}");
        assert_eq!(mock.request_count(), 0, "a URL must not reach the wire");
    }

    #[tokio::test]
    async fn unresolved_chat_ids_are_refused_without_a_request() {
        let mock = MockTransport::with(Vec::new());
        let bluebubbles = adapter(&mock);
        for chat_id in ["", "   ", "+15551234567", "alice@example.com"] {
            let result = bluebubbles
                .send_media(chat_id, &image_media(b"PNG"), None)
                .await;
            assert!(!result.success, "chat id {chat_id:?}");
            assert_eq!(
                result.error_kind.as_deref(),
                Some("not_found"),
                "{chat_id:?}"
            );
            assert!(!result.retryable, "{chat_id:?}");
        }
        assert_eq!(mock.request_count(), 0);

        // A resolved GUID — with surrounding whitespace — passes through untouched.
        let mock = MockTransport::with(vec![attachment_ok("att-1")]);
        assert!(
            adapter(&mock)
                .send_media("  iMessage;-;alice  ", &image_media(b"PNG"), None)
                .await
                .success
        );
        let parts = multipart_parts(&mock.requests()[0]);
        assert_eq!(
            field(&parts, "chatGuid").as_deref(),
            Some("iMessage;-;alice")
        );
    }

    #[tokio::test]
    async fn empty_bytes_are_refused_without_a_request() {
        let mock = MockTransport::with(Vec::new());
        let result = adapter(&mock)
            .send_media("iMessage;-;alice", &image_media(b""), None)
            .await;
        assert!(!result.success);
        assert_eq!(result.error_kind.as_deref(), Some("bad_format"));
        assert!(!result.retryable);
        assert_eq!(mock.request_count(), 0);
    }

    #[test]
    fn an_upload_past_the_attachment_cap_is_too_long() {
        let empty = attachment_size_refusal(0).expect("empty bytes are refused");
        assert_eq!(empty.error_kind.as_deref(), Some("bad_format"));
        assert!(!empty.retryable);

        assert!(attachment_size_refusal(1).is_none());
        assert!(attachment_size_refusal(MAX_ATTACHMENT_BYTES).is_none());

        let over = attachment_size_refusal(MAX_ATTACHMENT_BYTES + 1).expect("over the cap");
        assert_eq!(over.error_kind.as_deref(), Some("too_long"));
        assert!(!over.retryable);
        let error = over.error.as_deref().unwrap_or_default();
        assert!(
            error.contains(&(MAX_ATTACHMENT_BYTES + 1).to_string()),
            "{error}"
        );

        // iMessage caps one attachment at 100 MB.
        assert_eq!(MAX_ATTACHMENT_BYTES, 100 * 1024 * 1024);
    }

    #[tokio::test]
    async fn attachment_posts_the_documented_multipart_body() {
        let mock = MockTransport::with(vec![attachment_ok("att-1")]);
        let media = image_media(b"PNGDATA")
            .with_filename("photo.png")
            .with_mime("image/png");
        let result = adapter(&mock)
            .send_media("iMessage;-;alice", &media, None)
            .await;
        assert!(result.success, "{:?}", result.error);
        assert_eq!(result.message_id.as_deref(), Some("att-1"));
        assert_eq!(result.error_kind, None);
        assert!(!result.retryable);
        let raw = result.raw_response.as_ref().expect("raw response");
        assert_eq!(raw["status"], 200);

        let requests = mock.requests();
        assert_eq!(requests.len(), 1);
        assert_eq!(requests[0].method, HttpMethod::Post);
        assert_eq!(
            requests[0].url,
            "http://bb.local:1234/api/v1/message/attachment?password=s3cr3t"
        );
        assert!(!boundary_of(&requests[0]).is_empty());

        let parts = multipart_parts(&requests[0]);
        // The reference's `data` fields, then the `files` part — httpx emits them in that order.
        assert_eq!(
            parts
                .iter()
                .map(|part| part.name.as_str())
                .collect::<Vec<_>>(),
            vec!["chatGuid", "name", "tempGuid", "attachment"]
        );
        assert_eq!(
            field(&parts, "chatGuid").as_deref(),
            Some("iMessage;-;alice")
        );
        assert_eq!(field(&parts, "name").as_deref(), Some("photo.png"));
        assert_uuid4_hex(&field(&parts, "tempGuid").expect("tempGuid"));
        assert_eq!(field(&parts, "isAudioMessage"), None);

        let file = file_part(&parts).expect("a file part");
        assert_eq!(file.name, "attachment");
        assert_eq!(file.filename.as_deref(), Some("photo.png"));
        assert_eq!(file.content_type.as_deref(), Some("image/png"));
        assert_eq!(file.body, "PNGDATA");
    }

    /// `uuid.uuid4().hex` (`adapter.py:403`) is 32 lowercase hex characters.
    fn assert_uuid4_hex(guid: &str) {
        assert_eq!(guid.len(), 32, "{guid}");
        assert!(
            guid.chars()
                .all(|ch| ch.is_ascii_digit() || ('a'..='f').contains(&ch)),
            "{guid}"
        );
    }

    #[tokio::test]
    async fn attachment_filenames_and_mime_types_default_per_kind() {
        for (kind, expected_name, expected_mime) in [
            (MediaKind::Image, "image.jpg", "image/jpeg"),
            (MediaKind::Video, "video.mp4", "video/mp4"),
            (MediaKind::Audio, "audio.mp3", "audio/mpeg"),
            (
                MediaKind::Document,
                "attachment.bin",
                "application/octet-stream",
            ),
        ] {
            let mock = MockTransport::with(vec![attachment_ok("att-x")]);
            let media = OutboundMedia::new(kind, MediaSource::Bytes(b"DATA".to_vec()));
            let result = adapter(&mock)
                .send_media("iMessage;+;chat-1", &media, None)
                .await;
            assert!(result.success, "{kind:?}: {:?}", result.error);
            let parts = multipart_parts(&mock.requests()[0]);
            assert_eq!(
                field(&parts, "name").as_deref(),
                Some(expected_name),
                "{kind:?}"
            );
            let file = file_part(&parts).expect("a file part");
            assert_eq!(file.filename.as_deref(), Some(expected_name), "{kind:?}");
            assert_eq!(
                file.content_type.as_deref(),
                Some(expected_mime),
                "{kind:?}"
            );
        }
    }

    #[tokio::test]
    async fn a_caller_filename_is_reduced_to_a_safe_basename() {
        let mock = MockTransport::with(vec![attachment_ok("att-1")]);
        let media = image_media(b"DATA")
            .with_filename("/tmp/../dir/pho\"to\r\nX.png")
            .with_mime("not-a-mime");
        assert!(
            adapter(&mock)
                .send_media("iMessage;-;alice", &media, None)
                .await
                .success
        );
        let parts = multipart_parts(&mock.requests()[0]);
        // Quotes and CRLF are dropped (no header injection), the directory part is stripped.
        assert_eq!(field(&parts, "name").as_deref(), Some("photoX.png"));
        let file = file_part(&parts).expect("a file part");
        assert_eq!(file.filename.as_deref(), Some("photoX.png"));
        // A value that is not a content type falls back to the kind's default.
        assert_eq!(file.content_type.as_deref(), Some("image/jpeg"));

        // A blank caller filename also falls back to the default.
        let blank = MockTransport::with(vec![attachment_ok("att-2")]);
        assert!(
            adapter(&blank)
                .send_media(
                    "iMessage;-;alice",
                    &image_media(b"D").with_filename("   "),
                    None
                )
                .await
                .success
        );
        let parts = multipart_parts(&blank.requests()[0]);
        assert_eq!(field(&parts, "name").as_deref(), Some("image.jpg"));
    }

    #[tokio::test]
    async fn audio_attachments_carry_the_voice_memo_flag() {
        let mock = MockTransport::with(vec![attachment_ok("att-a")]);
        let media = OutboundMedia::new(MediaKind::Audio, MediaSource::Bytes(b"CAFDATA".to_vec()));
        let result = adapter(&mock)
            .send_media("iMessage;+;group-42", &media, None)
            .await;
        assert!(result.success, "{:?}", result.error);

        let parts = multipart_parts(&mock.requests()[0]);
        assert_eq!(
            parts
                .iter()
                .map(|part| part.name.as_str())
                .collect::<Vec<_>>(),
            vec![
                "chatGuid",
                "name",
                "tempGuid",
                "isAudioMessage",
                "attachment"
            ]
        );
        assert_eq!(field(&parts, "isAudioMessage").as_deref(), Some("true"));
        let file = file_part(&parts).expect("a file part");
        assert_eq!(file.content_type.as_deref(), Some("audio/mpeg"));
    }

    #[tokio::test]
    async fn every_upload_carries_its_own_temp_guid() {
        let mock = MockTransport::with(vec![attachment_ok("a1"), attachment_ok("a2")]);
        let bluebubbles = adapter(&mock);
        assert!(
            bluebubbles
                .send_media("iMessage;-;alice", &image_media(b"one"), None)
                .await
                .success
        );
        assert!(
            bluebubbles
                .send_media("iMessage;-;alice", &image_media(b"two"), None)
                .await
                .success
        );
        let requests = mock.requests();
        let first = field(&multipart_parts(&requests[0]), "tempGuid").expect("tempGuid");
        let second = field(&multipart_parts(&requests[1]), "tempGuid").expect("tempGuid");
        assert_uuid4_hex(&first);
        assert_uuid4_hex(&second);
        assert_ne!(first, second);
    }

    #[tokio::test]
    async fn caption_is_sent_as_a_follow_up_text_message() {
        let mock = MockTransport::with(vec![
            attachment_ok("att-1"),
            json_response(200, json!({"status": 200, "data": {"guid": "text-1"}})),
        ]);
        let media = image_media(b"PNGDATA").with_caption("look at this");
        let result = adapter(&mock)
            .send_media("iMessage;-;alice", &media, None)
            .await;
        assert!(result.success, "{:?}", result.error);
        // The attachment's guid is the reported id, not the caption's.
        assert_eq!(result.message_id.as_deref(), Some("att-1"));

        let requests = mock.requests();
        assert_eq!(requests.len(), 2);
        assert_eq!(requests[1].method, HttpMethod::Post);
        assert_eq!(
            requests[1].url,
            "http://bb.local:1234/api/v1/message/text?password=s3cr3t"
        );
        assert_eq!(
            header(&requests[1], "Content-Type"),
            Some("application/json")
        );
        let body = body_json(&requests[1]);
        assert_eq!(body["message"], "look at this");
        assert_eq!(body["chatGuid"], "iMessage;-;alice");

        // A blank caption is no caption: one request only.
        let quiet = MockTransport::with(vec![attachment_ok("att-2")]);
        let result = adapter(&quiet)
            .send_media(
                "iMessage;-;alice",
                &image_media(b"PNG").with_caption("   "),
                None,
            )
            .await;
        assert!(result.success, "{:?}", result.error);
        assert_eq!(result.message_id.as_deref(), Some("att-2"));
        assert_eq!(quiet.request_count(), 1);
    }

    #[tokio::test]
    async fn a_failed_caption_is_surfaced_without_claiming_success() {
        let mock = MockTransport::with(vec![
            attachment_ok("att-1"),
            json_response(403, json!({"message": "forbidden"})),
        ]);
        let media = image_media(b"PNGDATA").with_caption("look at this");
        let result = adapter(&mock)
            .send_media("iMessage;-;alice", &media, None)
            .await;
        assert!(!result.success);
        // The attachment really did land, so its guid is kept for the ledger.
        assert_eq!(result.message_id.as_deref(), Some("att-1"));
        assert_eq!(result.error_kind.as_deref(), Some("forbidden"));
        assert!(
            !result.retryable,
            "a retry would upload the attachment a second time"
        );
        assert!(result.raw_response.is_some());
        let error = result.error.as_deref().unwrap_or_default();
        assert!(error.contains("caption"), "{error}");
        assert!(error.contains("att-1"), "{error}");
        assert!(error.contains("forbidden"), "{error}");
        assert_eq!(mock.request_count(), 2);
    }

    #[tokio::test]
    async fn attachment_http_errors_are_classified() {
        for (status, kind, retryable) in [
            (400u16, "bad_format", false),
            (403, "forbidden", false),
            (404, "not_found", false),
            (413, "too_long", false),
            (429, "rate_limited", true),
            (500, "transient", true),
            (503, "transient", true),
        ] {
            let mock =
                MockTransport::with(vec![json_response(status, json!({"message": "rejected"}))]);
            let media = image_media(b"PNG").with_caption("never sent");
            let result = adapter(&mock)
                .send_media("iMessage;-;alice", &media, None)
                .await;
            assert!(!result.success, "{status}");
            assert_eq!(result.error_kind.as_deref(), Some(kind), "{status}");
            assert_eq!(result.retryable, retryable, "{status}");
            assert_eq!(result.message_id, None, "{status}");
            let error = result.error.as_deref().unwrap_or_default();
            assert!(error.contains(&status.to_string()), "{status}: {error}");
            // The upload is a hard prerequisite: no caption bubble after a failed one.
            assert_eq!(mock.request_count(), 1, "{status}");
        }

        let limited = MockTransport::with(vec![HttpResponse {
            status: 429,
            body: "{}".to_string(),
            retry_after: Some(4.0),
        }]);
        let result = adapter(&limited)
            .send_media("iMessage;-;alice", &image_media(b"PNG"), None)
            .await;
        assert_eq!(result.retry_after, Some(4.0));

        let offline = MockTransport::failing(TransportError::Request("connection reset".into()));
        let result = adapter(&offline)
            .send_media("iMessage;-;alice", &image_media(b"PNG"), None)
            .await;
        assert!(!result.success);
        assert_eq!(result.error_kind.as_deref(), Some("transient"));
        assert!(result.retryable);
    }

    #[tokio::test]
    async fn attachment_body_level_failures_never_report_success() {
        // HTTP 200, envelope status 400: the reference's `result["message"]` surfaced verbatim.
        let failed = MockTransport::with(vec![json_response(
            200,
            json!({"status": 400, "message": "Attachment upload failed"}),
        )]);
        let result = adapter(&failed)
            .send_media("iMessage;-;alice", &image_media(b"PNG"), None)
            .await;
        assert!(!result.success);
        assert_eq!(result.error.as_deref(), Some("Attachment upload failed"));
        assert_eq!(result.error_kind.as_deref(), Some("bad_format"));
        assert_eq!(result.message_id, None);

        // Envelope status 200 without a usable data.guid is not a delivery.
        for body in [
            json!({"status": 200}),
            json!({"status": 200, "data": {}}),
            json!({"status": 200, "data": {"guid": "   "}}),
            json!({"status": 200, "data": []}),
        ] {
            let mock = MockTransport::with(vec![json_response(200, body.clone())]);
            let result = adapter(&mock)
                .send_media("iMessage;-;alice", &image_media(b"PNG"), None)
                .await;
            assert!(!result.success, "{body}");
            assert_eq!(result.error_kind.as_deref(), Some("unknown"), "{body}");
            assert_eq!(result.message_id, None, "{body}");
        }

        // A string status is not the numeric 200 the reference compares against.
        let stringy = MockTransport::with(vec![json_response(
            200,
            json!({"status": "200", "data": {"guid": "att-1"}}),
        )]);
        let result = adapter(&stringy)
            .send_media("iMessage;-;alice", &image_media(b"PNG"), None)
            .await;
        assert!(!result.success);
        assert_eq!(result.error_kind.as_deref(), Some("unknown"));

        // A 2xx with an unreadable body cannot confirm anything either.
        let garbled = MockTransport::with(vec![plain_response(200, "Attachment stored")]);
        let result = adapter(&garbled)
            .send_media("iMessage;-;alice", &image_media(b"PNG"), None)
            .await;
        assert!(!result.success);
        assert_eq!(result.error_kind.as_deref(), Some("unknown"));
        assert!(
            result
                .error
                .as_deref()
                .unwrap_or_default()
                .contains("Attachment stored")
        );
    }
}
