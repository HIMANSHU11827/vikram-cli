//! Signal adapter — `signal-cli-rest-api` over HTTP.
//!
//! Outbound is one REST call per message: `POST {base}/v2/send`. Inbound is long polling:
//! `GET {base}/v1/receive/{account}?timeout=<secs>` returns a JSON array of envelopes. Both ride on
//! the frozen [`Transport`](crate::transport::Transport), so the whole wire surface is testable with
//! no daemon, no phone number and no linked account.
//!
//! Port of `plugins/platforms/signal/adapter.py`, retargeted to the REST surface: hermes talks the
//! signal-cli daemon's JSON-RPC 2.0 + SSE protocol (`POST /api/v1/rpc`, `GET /api/v1/events`), while
//! `signal-cli-rest-api` — what the deployment actually runs — exposes `v2/send` and `v1/receive`.
//! The envelope model is the same, so parsing follows hermes one field at a time.
//!
//! Ported behaviour, with the hermes origin:
//! - 8000-unit chunking through [`truncate_message`] with [`utf16_len`] (`MAX_MESSAGE_LENGTH`,
//!   `_split_signal_formatted_message`); chunks are delivered sequentially and in order.
//! - Outbound media ([`SignalAdapter::send_media`]): one attachment per `v2/send`, the hermes
//!   `attachments=[…]` semantics (`_send_file`, `_send_attachment_batch`) expressed in the shape
//!   this bridge actually parses, with the 100 MB per-attachment cap (`SIGNAL_MAX_ATTACHMENT_SIZE`),
//!   the caption as `message` and the reply anchor as `quote_timestamp`.
//! - Envelope parsing (`_handle_envelope`): `dataMessage.message`, the `sourceNumber` →
//!   `sourceUuid` → `source` sender preference, `groupInfo.groupId` group routing, `quote.id` reply
//!   anchors and `attachments[]` facts.
//! - Send failure taxonomy: 429 → `rate_limited` with the server's `Retry-After`, 401/403 →
//!   `forbidden`, 404 → `not_found`, 400 → `bad_format`, 413 → `too_long`, 5xx / transport →
//!   `transient`.
//!
//! Deliberately not ported (full accounting in `docs/gateway-port-plan.md`): the `markdown_to_signal`
//! renderer and its `textStyles` range translation, the SSE listener with its health monitor and
//! jittered reconnect ladder, the per-account platform lock, the rate-limit scheduler and batch
//! pacing, attachment download/caching (media bytes are fetched separately, see
//! [`SignalAdapter::poll_once`]), the multi-attachment batch sender (`send_multiple_images` and
//! `SIGNAL_MAX_ATTACHMENTS_PER_MSG`) with the URL/`file://` download path of `send_image`, reactions
//! (`sendReaction`, the 👀/✅ lifecycle hooks), mention
//! rendering and `require_mention` gating, DM/group allowlists, echo-back filtering (`syncMessage`
//! promotion and the sent-timestamp caches), the `listContacts` number↔UUID recipient cache, and
//! typing indicators (the REST API has none). Those need runtime state — locks, background tasks,
//! stores — that does not belong in a stateless wire adapter.

use async_trait::async_trait;
use serde_json::{Value, json};
use std::io::{ErrorKind, Write};
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use crate::event::{MessageEvent, MessageType, Platform, SessionSource, platform_names};
use crate::helpers::truncate_message;
use crate::platform::{
    ChatInfo, MediaKind, MediaSource, OutboundMedia, PlatformAdapter, SendResult, utf16_len,
};
use crate::transport::{HttpRequest, HttpResponse, SharedTransport, TransportError};

/// Signal's per-message cap, counted in UTF-16 code units (hermes `MAX_MESSAGE_LENGTH`).
pub const SIGNAL_MAX_MESSAGE_LENGTH: usize = 8000;

/// Signal's per-attachment ceiling in bytes (hermes `SIGNAL_MAX_ATTACHMENT_SIZE`, "100 MB"). A
/// larger attachment is refused locally: Signal will not deliver it, and uploading it only wastes a
/// request and the caller's wait. The cap counts raw attachment bytes, which is what hermes measures
/// (`_resolve_image_path`, `_send_attachment`); base64 transport inflates the JSON body on top of it.
pub const SIGNAL_MAX_ATTACHMENT_SIZE: usize = 100 * 1024 * 1024;

/// Signal's per-message attachment cap (hermes `SIGNAL_MAX_ATTACHMENTS_PER_MSG`, 32, from the
/// Signal-Android/Desktop source). [`SignalAdapter::send_media`] posts exactly **one** attachment per
/// `v2/send`, so this bound is never approached; the constant is recorded so the batched sender
/// (`send_multiple_images`) keeps a faithful value when it is ported.
pub const SIGNAL_MAX_ATTACHMENTS_PER_MSG: usize = 32;

/// Characters of an error body kept in a [`SendResult::error`] message.
const BODY_EXCERPT_CHARS: usize = 200;

/// Longest attachment filename kept after sanitising, in characters. Long enough for a real name,
/// short enough that the staging path stays well inside every common `PATH_MAX`.
const MAX_FILENAME_CHARS: usize = 96;

/// MIME type used in a `data:` attachment when the caller supplied a filename but no usable type.
const DEFAULT_ATTACHMENT_MIME: &str = "application/octet-stream";

/// How many times [`TempAttachment::create`] retries a name collision before giving up.
const TEMP_FILE_ATTEMPTS: u8 = 3;

/// RFC 4648 `base64` alphabet — the encoding `signal-cli-rest-api` decodes attachments from.
const BASE64_ALPHABET: &[u8; 64] =
    b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";

/// A Signal timestamp is a millisecond epoch — 13 digits today. The accepted digit range is wider so
/// the check survives a unit change, narrow enough that arbitrary ids (`12`, `abc`, a UUID) never
/// qualify as a quote target.
const TIMESTAMP_MIN_DIGITS: usize = 10;
const TIMESTAMP_MAX_DIGITS: usize = 19;

/// Keys a Signal `quote` object may carry the quoted author under, in preference order
/// (hermes `_QUOTE_AUTHOR_KEYS`).
const QUOTE_AUTHOR_KEYS: [&str; 6] = [
    "author",
    "authorNumber",
    "authorUuid",
    "authorAci",
    "authorServiceId",
    "authorServiceIdString",
];

/// How [`SignalAdapter::send_media`] hands attachment bytes to the bridge.
///
/// The reference (`plugins/platforms/signal/adapter.py`) sends `attachments=[<local path>]` in the
/// signal-cli daemon's JSON-RPC `send` params (`_send_file`, `_send_attachment_batch`) because it
/// runs on the same host as the daemon and hands it filesystem paths.
/// `signal-cli-rest-api`'s `v2/send` — what this adapter targets — carries attachments **only** as
/// `base64_attachments`: an array of base64 strings, each either bare or in the bridge's documented
/// data-URL forms `data:<mime>;base64,<data>` / `data:<mime>;filename=<name>;base64,<data>`. That
/// makes [`Self::Base64`] the only shape this bridge is known to parse, so it is the default.
///
/// [`Self::FilePath`] exists for a bridge that reads daemon-local paths the way hermes does (a
/// `v2/send` front end that grew the daemon's `attachments` field, or the daemon RPC behind a
/// translation layer). It is strictly opt-in: `v2/send`'s decoder ignores unknown fields, so sending
/// `attachments: [path]` to the REST API as-is would deliver the *text* and silently drop the file —
/// a fake success, which this crate forbids. Callers must know their bridge reads paths.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum AttachmentMode {
    /// Inline `base64_attachments` (the `signal-cli-rest-api` `v2/send` schema). No disk writes.
    #[default]
    Base64,
    /// Hermes-style `attachments: [<path>]`, with the bytes staged in `std::env::temp_dir()` and
    /// removed by [`TempAttachment`]'s `Drop` as soon as the request completes.
    FilePath,
}

/// Signal adapter speaking the `signal-cli-rest-api` REST surface.
///
/// Stateless apart from its transport handle, the daemon base URL, the linked account and the
/// attachment shape: `connect` only probes the daemon, so a failed connect is retried on the same
/// instance. `base_url` is stored without a trailing `/` so every endpoint is a plain concatenation.
pub struct SignalAdapter {
    transport: SharedTransport,
    base_url: String,
    account: String,
    attachment_mode: AttachmentMode,
}

impl SignalAdapter {
    /// Build an adapter for `account` (E.164 number, e.g. `+15551234567`) against the daemon at
    /// `base_url` (e.g. `http://127.0.0.1:8080`).
    pub fn new(
        transport: SharedTransport,
        base_url: impl Into<String>,
        account: impl Into<String>,
    ) -> Self {
        Self {
            transport,
            base_url: base_url.into().trim_end_matches('/').to_string(),
            account: account.into(),
            attachment_mode: AttachmentMode::default(),
        }
    }

    /// Choose how [`send_media`](PlatformAdapter::send_media) hands attachment bytes to the bridge.
    ///
    /// Defaults to [`AttachmentMode::Base64`], the shape `signal-cli-rest-api` parses; see
    /// [`AttachmentMode`] before switching to [`AttachmentMode::FilePath`].
    pub fn with_attachment_mode(mut self, mode: AttachmentMode) -> Self {
        self.attachment_mode = mode;
        self
    }

    /// The daemon base URL plus an API path (`/v1/about`, `/v2/send`, ...).
    fn endpoint(&self, path: &str) -> String {
        format!("{}{path}", self.base_url)
    }

    /// POST a `v2/send` payload and return the decoded response body.
    ///
    /// A non-2xx response and a transport failure become a classified [`SendResult`] in the `Err`
    /// arm; a 2xx whose body is not JSON decodes to [`Value::Null`], which the caller reports as a
    /// response without a timestamp rather than pretending the send was identified.
    async fn post_send(&self, payload: &Value) -> Result<Value, Box<SendResult>> {
        let request = HttpRequest::json(self.endpoint("/v2/send"), payload);
        match self.transport.execute(request).await {
            Ok(response) if response.ok() => Ok(response.json().unwrap_or(Value::Null)),
            Ok(response) => Err(Box::new(classify_failure(&response))),
            Err(err) => Err(Box::new(transport_failure(&err))),
        }
    }

    /// Long-poll `GET /v1/receive/{account}` once and normalize whatever came back.
    ///
    /// `timeout_secs` is the daemon-side hold: it returns as soon as an envelope arrives or the
    /// timeout expires. The body is a JSON array whose entries wrap an `envelope`; for every entry
    /// carrying an `envelope.dataMessage` one [`MessageEvent`] is produced. Entries without a
    /// `dataMessage` — delivery/read receipts, typing indicators, sync traffic, stories — are
    /// skipped, as are `dataMessage`s with neither text nor an attachment.
    ///
    /// `message_id` and `platform_update_id` both come from the envelope's millisecond timestamp,
    /// which is what Signal quotes by, so a reply can always be anchored. `reply_to_message_id`
    /// comes from `quote.id`.
    ///
    /// Media bytes are **not** downloaded here: `media_urls` carries the attachment `id` (or its
    /// `filename` when the daemon omits the id), which a caller resolves separately through
    /// `GET /v1/attachments/{id}`. That keeps this adapter free of a cache and of the 100 MB
    /// attachment-size policy hermes enforces at download time.
    ///
    /// Transport errors, non-2xx statuses, unparseable JSON and a non-array body are reported as
    /// `Err` so the caller can back off; an empty batch is `Ok(vec![])`.
    pub async fn poll_once(&self, timeout_secs: u32) -> Result<Vec<MessageEvent>, String> {
        let url = format!(
            "{}/v1/receive/{}?timeout={timeout_secs}",
            self.base_url, self.account
        );
        let response = self
            .transport
            .execute(HttpRequest::get(url))
            .await
            .map_err(|err| format!("signal receive transport error: {err}"))?;
        if !response.ok() {
            return Err(format!(
                "signal receive failed with status {}: {}",
                response.status,
                body_excerpt(&response.body)
            ));
        }
        let parsed: Value = serde_json::from_str(&response.body)
            .map_err(|err| format!("signal receive returned invalid json: {err}"))?;
        let entries = match parsed.as_array() {
            Some(entries) => entries,
            None => {
                return Err(format!(
                    "signal receive returned a non-array body: {}",
                    body_excerpt(&response.body)
                ));
            }
        };
        Ok(entries.iter().filter_map(parse_entry).collect())
    }
}

#[async_trait]
impl PlatformAdapter for SignalAdapter {
    /// Probe `GET /v1/about`. True only for a 200 whose body is a JSON object (the daemon's version
    /// report); every other status, a non-object body, and a transport failure are `false`.
    ///
    /// `is_reconnect` needs no different handling: nothing is held client-side, and the receive
    /// cursor is the daemon's, not ours.
    async fn connect(&mut self, _is_reconnect: bool) -> bool {
        let request = HttpRequest::get(self.endpoint("/v1/about"));
        let response = match self.transport.execute(request).await {
            Ok(response) => response,
            Err(_) => return false,
        };
        if response.status != 200 {
            return false;
        }
        match response.json() {
            Ok(body) => body.is_object(),
            Err(_) => false,
        }
    }

    /// Nothing to tear down: no socket, no polling task. The caller simply stops calling
    /// [`SignalAdapter::poll_once`].
    async fn disconnect(&mut self) {}

    /// Chunk `content` to [`SIGNAL_MAX_MESSAGE_LENGTH`] UTF-16 units and POST the chunks in order to
    /// `/v2/send`.
    ///
    /// Each request carries `number` (the linked account), `recipients: [chat_id]` and `message`.
    /// A timestamp-shaped `reply_to` adds `quote_timestamp` on the opening chunk; `quote_author` is
    /// added only when `metadata["quote_author"]` supplies the quoted sender — never invented.
    /// Blank content is refused before any request is made.
    ///
    /// Every chunk's response timestamp is collected in send order and the last one becomes
    /// `message_id` (Signal has no separate message id). A failure after some chunks landed keeps
    /// those timestamps in `continuation_message_ids` behind the `partial_overflow` marker so the
    /// delivery ledger resends only the tail.
    async fn send(
        &self,
        chat_id: &str,
        content: &str,
        reply_to: Option<&str>,
        metadata: Option<Value>,
    ) -> SendResult {
        if content.trim().is_empty() {
            // Caller bug, not a platform problem: refuse before touching the network.
            return SendResult::failed("Refusing to send empty message", Some("bad_format"));
        }
        let chunks = truncate_message(content, SIGNAL_MAX_MESSAGE_LENGTH, utf16_len);
        let total = chunks.len();
        if total == 0 {
            return SendResult::failed("Refusing to send empty message", Some("bad_format"));
        }
        let quote = quote_fields(reply_to, metadata.as_ref());
        let mut timestamps: Vec<String> = Vec::with_capacity(total);

        for (index, chunk) in chunks.iter().enumerate() {
            let mut payload = json!({
                "number": self.account,
                "recipients": [chat_id],
                "message": chunk,
            });
            // hermes' default `reply_to_mode` is "first": only the opening chunk quotes, so one
            // answer never produces N quotes of the same message.
            if index == 0
                && let Some((timestamp, author)) = &quote
            {
                payload["quote_timestamp"] = json!(timestamp);
                if let Some(author) = author {
                    payload["quote_author"] = json!(author);
                }
            }
            match self.post_send(&payload).await {
                Ok(response) => match id_field(&response, "timestamp") {
                    Some(timestamp) => timestamps.push(timestamp),
                    // Delivered but unidentified: report it, never resend blindly.
                    None => {
                        return mark_partial(
                            SendResult::failed(
                                "signal send response carried no timestamp",
                                Some("unknown"),
                            ),
                            &timestamps,
                            total,
                        );
                    }
                },
                Err(failure) => return mark_partial(*failure, &timestamps, total),
            }
        }

        match timestamps.last() {
            Some(last) => SendResult::ok_split(last.clone(), timestamps),
            None => SendResult::failed("signal send delivered no chunks", Some("bad_format")),
        }
    }

    /// Signal delivers attachments as first-class messages, so [`send_media`](Self::send_media) is
    /// implemented.
    fn supports_media(&self) -> bool {
        true
    }

    /// Send one attachment with the caption as the message body.
    ///
    /// Reference semantics, adapted to this bridge:
    /// - hermes sends `attachments=[<path>]` in the `send` RPC (`_send_file`, adapter.py:880-885, and
    ///   `_send_attachment_batch`, adapter.py:823-857) with `message: caption or ""`; the same
    ///   `message`/caption pairing and the same one-attachment-per-call shape are kept here.
    /// - `signal-cli-rest-api`'s `v2/send` parses attachments only as `base64_attachments`, so
    ///   [`AttachmentMode::Base64`] (the default, see [`AttachmentMode`]) sends the bytes inline: bare
    ///   base64, or the bridge's `data:<mime>;filename=<name>;base64,<data>` form when a filename is
    ///   supplied so the recipient sees a real name instead of an opaque blob.
    ///   [`AttachmentMode::FilePath`] stages the bytes in [`std::env::temp_dir()`] and posts the
    ///   hermes-shaped `attachments: [<path>]`, which is only correct against a bridge that reads
    ///   daemon-local paths.
    /// - `reply_to` uses the adapter's existing quote parameter: a timestamp-shaped anchor becomes
    ///   `quote_timestamp`. No `quote_author` is added — `send_media` has no metadata to take one
    ///   from, and the adapter never invents one.
    ///
    /// Refusals happen before any request: [`MediaSource::Url`] (this adapter has no binary fetcher
    /// and will not claim a download it cannot do), empty bytes (nothing to deliver), and anything
    /// over [`SIGNAL_MAX_ATTACHMENT_SIZE`]. A successful send is one the bridge accepted (`2xx`),
    /// identified by the timestamp it returned; a `2xx` without a timestamp is a failure (`unknown`),
    /// matching [`send`](Self::send) so no success is ever reported for an unidentified delivery.
    /// Failures carry the shared taxonomy, 429 keeping the server's `Retry-After`.
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
                        "signal send media cannot fetch {url:?}: this adapter has no binary \
                         fetcher, so the bytes must be supplied as MediaSource::Bytes"
                    ),
                    Some("bad_format"),
                );
            }
        };
        if bytes.is_empty() {
            return SendResult::failed("Refusing to send empty media", Some("bad_format"));
        }
        if let Some(oversize) = oversize_failure(bytes.len()) {
            return oversize;
        }

        let mut payload = json!({
            "number": self.account,
            "recipients": [chat_id],
            // hermes: `message: caption or ""` — the caption is the message, and a captionless
            // attachment still carries an (empty) message field the bridge expects.
            "message": media.caption.as_deref().unwrap_or(""),
        });
        // The adapter's existing reply parameter, reused verbatim: only a timestamp-shaped anchor can
        // be a Signal quote, and only the quoted timestamp is known here.
        if let Some((timestamp, _author)) = quote_fields(reply_to, None) {
            payload["quote_timestamp"] = json!(timestamp);
        }

        // `_staged` is deliberately a named binding rather than `_`: it owns the temp file for the
        // whole request and its `Drop` deletes the file when this function returns — on success, on
        // a classified failure and on a panic — while a bare `_` would drop it immediately.
        let _staged = match self.attachment_mode {
            AttachmentMode::Base64 => {
                payload["base64_attachments"] = json!([base64_attachment_value(media, bytes)]);
                None
            }
            AttachmentMode::FilePath => {
                let filename = attachment_filename(media.filename.as_deref(), media.kind);
                match TempAttachment::create(&filename, bytes) {
                    Ok(staged) => {
                        payload["attachments"] = json!([staged.wire_path()]);
                        Some(staged)
                    }
                    Err(err) => {
                        return SendResult::failed(
                            format!(
                                "signal send media could not stage a temp attachment in {}: {err}",
                                std::env::temp_dir().display()
                            ),
                            Some("unknown"),
                        );
                    }
                }
            }
        };

        match self.post_send(&payload).await {
            Ok(response) => match id_field(&response, "timestamp") {
                Some(timestamp) => SendResult::ok(timestamp),
                None => SendResult::failed(
                    "signal media send response carried no timestamp",
                    Some("unknown"),
                ),
            },
            Err(failure) => *failure,
        }
    }

    /// No endpoint exists for this in `signal-cli-rest-api`: the API exposes neither a contact
    /// lookup nor a group query, so the chat is reported as the id itself, classified as a DM.
    /// Callers keep a usable routing answer instead of losing the chat to an error.
    async fn get_chat_info(&self, chat_id: &str) -> ChatInfo {
        ChatInfo {
            name: chat_id.to_string(),
            chat_type: "dm".to_string(),
        }
    }

    /// Typing indicators do not exist in `signal-cli-rest-api` (the daemon's `sendTyping` RPC was not
    /// carried over to the REST surface), so this is an explicit no-op: the turn proceeds, the
    /// recipient simply sees no dots.
    async fn send_typing(&self, _chat_id: &str, _metadata: Option<Value>) {}

    /// `send` chunks natively through [`truncate_message`], so the router must not truncate first.
    fn splits_long_messages(&self) -> bool {
        true
    }

    fn max_message_length(&self) -> usize {
        SIGNAL_MAX_MESSAGE_LENGTH
    }

    /// Signal's limit is counted in UTF-16 code units, like hermes' `message_len_fn` for this
    /// platform.
    fn message_length(&self, text: &str) -> usize {
        utf16_len(text)
    }
}

/// The refusal for an attachment above [`SIGNAL_MAX_ATTACHMENT_SIZE`], or `None` when it fits.
///
/// The measure is the raw byte count, as in hermes (`_resolve_image_path` compares
/// `Path.stat().st_size`, `_send_attachment` the same). `too_long` is the taxonomy's kind for "this
/// payload is bigger than the platform accepts"; it is not retryable, and shrinking or re-encoding
/// the file is the caller's decision.
fn oversize_failure(len: usize) -> Option<SendResult> {
    if len <= SIGNAL_MAX_ATTACHMENT_SIZE {
        return None;
    }
    Some(SendResult::failed(
        format!(
            "signal send media: attachment is {len} bytes, over the {SIGNAL_MAX_ATTACHMENT_SIZE}-byte \
             per-attachment cap"
        ),
        Some("too_long"),
    ))
}

/// The `base64_attachments` entry for `bytes`.
///
/// The bridge accepts three shapes (see [`AttachmentMode`]): bare base64, `data:<mime>;base64,<data>`,
/// and `data:<mime>;filename=<name>;base64,<data>`. A filename the recipient should see selects the
/// third — that is the only way this bridge carries a name, since `v2/send` has no separate filename
/// field — and the first is used when the caller gave nothing but bytes. Everything interpolated here
/// is sanitised ([`safe_filename`], [`mime_token`]), so neither the `;`/`,` delimiters nor a path
/// separator can appear inside the value.
fn base64_attachment_value(media: &OutboundMedia, bytes: &[u8]) -> String {
    let encoded = base64_encode(bytes);
    let filename = media
        .filename
        .as_deref()
        .map(|name| safe_filename(name, media.kind));
    let mime = media.mime.as_deref().map(mime_token).unwrap_or_default();
    match filename {
        Some(filename) => {
            let mime = if mime.is_empty() {
                DEFAULT_ATTACHMENT_MIME
            } else {
                mime.as_str()
            };
            format!("data:{mime};filename={filename};base64,{encoded}")
        }
        None if !mime.is_empty() => format!("data:{mime};base64,{encoded}"),
        None => encoded,
    }
}

/// A MIME type reduced to a safe `data:` URL token, or `""` when it is empty or unusable.
///
/// Only `type/subtype` characters survive validation; anything carrying a `;`, `,`, whitespace or
/// non-ASCII (all of which would break the data-URL grammar) is rejected outright rather than
/// rewritten into something that looks like a type but is not one.
fn mime_token(mime: &str) -> String {
    let trimmed = mime.trim();
    if trimmed.is_empty() || trimmed.len() > 128 {
        return String::new();
    }
    let usable = trimmed
        .chars()
        .all(|ch| ch.is_ascii_alphanumeric() || matches!(ch, '/' | '+' | '.' | '-' | '_'));
    if usable {
        trimmed.to_ascii_lowercase()
    } else {
        String::new()
    }
}

/// The filename to put on the wire: exactly one path component, no traversal, never empty.
///
/// Only ASCII alphanumerics and `.`, `-`, `_` survive; every other character (path separators, the
/// data-URL delimiters `;` and `,`, control characters, non-ASCII, spaces) becomes `_`. Leading dots
/// are dropped so the name cannot be `..` or a hidden file, an empty result falls back to
/// [`default_attachment_name`], and the length is capped at [`MAX_FILENAME_CHARS`]. The result is
/// therefore safe both as a `filename=` value in a data URL and as the final component of a staging
/// path in [`std::env::temp_dir()`].
fn safe_filename(filename: &str, kind: MediaKind) -> String {
    let base = filename
        .rsplit(['/', '\\'])
        .next()
        .unwrap_or_default()
        .to_string();
    let mut name: String = base
        .chars()
        .map(|ch| {
            if ch.is_ascii_alphanumeric() || matches!(ch, '.' | '-' | '_') {
                ch
            } else {
                '_'
            }
        })
        .collect();
    while name.starts_with('.') {
        name.remove(0);
    }
    if name.is_empty() {
        return default_attachment_name(kind);
    }
    if name.chars().count() > MAX_FILENAME_CHARS {
        name = name.chars().take(MAX_FILENAME_CHARS).collect();
    }
    name
}

/// The filename used when the caller supplied none (or one that sanitised to nothing).
///
/// The extension is a container Signal and the recipient's client both understand for the kind; the
/// content, not the name, is what decides how the message renders.
fn default_attachment_name(kind: MediaKind) -> String {
    let extension = match kind {
        MediaKind::Image => "png",
        MediaKind::Video => "mp4",
        MediaKind::Audio => "m4a",
        MediaKind::Document => "bin",
    };
    format!("attachment.{extension}")
}

/// The wire filename for `filename`, falling back to [`default_attachment_name`].
fn attachment_filename(filename: Option<&str>, kind: MediaKind) -> String {
    match filename {
        Some(filename) => safe_filename(filename, kind),
        None => default_attachment_name(kind),
    }
}

/// Standard base64 with padding (RFC 4648 §4), the encoding `base64_attachments` decodes.
///
/// Hand-rolled because this crate has no base64 dependency and the attachment path is the only place
/// that needs one; it is `O(n)`, allocation-light, and cannot panic (every index is masked into the
/// 64-character alphabet and the group loop never indexes past the chunk it was given).
fn base64_encode(bytes: &[u8]) -> String {
    let mut out = String::with_capacity(bytes.len().div_ceil(3) * 4);
    for chunk in bytes.chunks(3) {
        let mut triple: u32 = 0;
        for (index, byte) in chunk.iter().enumerate() {
            triple |= u32::from(*byte) << (16 - 8 * index);
        }
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

/// A staged attachment file that deletes itself when dropped.
///
/// This is the RAII half of [`AttachmentMode::FilePath`]: the bridge reads the file while the
/// request is in flight, and `Drop` removes it however `send_media` returns — accepted, refused,
/// transport error, or a panic unwinding through it.
struct TempAttachment {
    path: PathBuf,
}

impl TempAttachment {
    /// Stage `bytes` in [`std::env::temp_dir()`] under a unique name ending in `filename`.
    ///
    /// The name is `vikram-signal-<pid>-<nanos>-<sequence>-<filename>`: one path component, so a
    /// hostile `filename` cannot escape the temp directory ([`safe_filename`] already reduced it to a
    /// bare name, and this prefix is added on top). `create_new` never reuses an existing path, so a
    /// pre-planted file or symlink at the chosen name fails the call instead of being followed or
    /// overwritten; pid + clock + sequence make a collision in practice impossible, and a genuine one
    /// is retried a few times before it is reported.
    fn create(filename: &str, bytes: &[u8]) -> std::io::Result<Self> {
        static SEQUENCE: AtomicU64 = AtomicU64::new(0);
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|elapsed| elapsed.as_nanos())
            .unwrap_or_default();
        let mut last_error = None;
        for attempt in 0..TEMP_FILE_ATTEMPTS {
            let sequence = SEQUENCE.fetch_add(1, Ordering::Relaxed);
            let name = format!(
                "vikram-signal-{}-{nanos}-{sequence}-{filename}",
                std::process::id()
            );
            let path = std::env::temp_dir().join(name);
            match std::fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(&path)
            {
                Ok(mut file) => {
                    // The guard owns the path before a byte is written: a short write still deletes
                    // the partial file when the error unwinds through it.
                    let guard = Self { path };
                    file.write_all(bytes)?;
                    return Ok(guard);
                }
                Err(err)
                    if err.kind() == ErrorKind::AlreadyExists
                        && attempt + 1 < TEMP_FILE_ATTEMPTS =>
                {
                    last_error = Some(err);
                }
                Err(err) => return Err(err),
            }
        }
        Err(last_error.unwrap_or_else(|| {
            std::io::Error::other("could not pick an unused temp attachment name")
        }))
    }

    /// The staged path as the JSON string the bridge receives.
    ///
    /// Lossy on purpose: a non-UTF-8 `TMPDIR` must not turn a sendable attachment into a panic, and a
    /// request carrying a lossily-rendered path fails at the bridge (path not found), which is
    /// reported as a real failure rather than a success.
    fn wire_path(&self) -> String {
        self.path.to_string_lossy().into_owned()
    }
}

impl Drop for TempAttachment {
    /// Best-effort removal. A leftover file in the OS temp directory is not worth turning a sent
    /// message into an error.
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.path);
    }
}

/// The `(quote_timestamp, quote_author)` pair for a reply, or `None` when the anchor cannot be a
/// Signal quote.
///
/// Signal quotes by the quoted message's millisecond timestamp, which is exactly what this adapter
/// puts in `message_id`, so only a timestamp-shaped `reply_to` qualifies — a channel-style id is
/// dropped rather than guessed at. The author is never invented: it is taken from
/// `metadata["quote_author"]` when the caller supplies it and omitted otherwise.
fn quote_fields(
    reply_to: Option<&str>,
    metadata: Option<&Value>,
) -> Option<(String, Option<String>)> {
    let timestamp = reply_to?.trim();
    if !is_timestamp_like(timestamp) {
        return None;
    }
    let author = metadata.and_then(|metadata| string_field(metadata, "quote_author"));
    Some((timestamp.to_string(), author))
}

/// Does `value` look like a Signal millisecond timestamp (all ASCII digits, epoch-sized)?
fn is_timestamp_like(value: &str) -> bool {
    (TIMESTAMP_MIN_DIGITS..=TIMESTAMP_MAX_DIGITS).contains(&value.len())
        && value.bytes().all(|byte| byte.is_ascii_digit())
}

/// Parse one `GET /v1/receive` array entry into a normalized event.
///
/// The daemon wraps each event in `envelope`; a bare envelope is tolerated as well (hermes does the
/// same with `envelope.get("envelope", envelope)`). `dataMessage` is the only message shape this
/// adapter models, so receipts, typing, sync traffic and stories fall out here.
fn parse_entry(entry: &Value) -> Option<MessageEvent> {
    let envelope = entry
        .get("envelope")
        .filter(|value| value.is_object())
        .unwrap_or(entry);
    let data = envelope
        .get("dataMessage")
        .filter(|value| value.is_object())?;
    let sender = sender_of(envelope)?;
    let timestamp = timestamp_of(envelope, data);
    let group = data.get("groupInfo").filter(|value| value.is_object());
    let group_id = group.and_then(|group| id_field(group, "groupId"));
    let is_group = group_id.is_some();
    let source_name = string_field(envelope, "sourceName");
    let chat_id = match &group_id {
        Some(group_id) => group_id.clone(),
        None => sender.clone(),
    };
    let chat_name = if is_group {
        group.and_then(|group| string_field(group, "groupName"))
    } else {
        source_name.clone().or_else(|| Some(sender.clone()))
    };

    let mut source = SessionSource::new(signal_platform(), chat_id);
    source.chat_name = chat_name;
    source.chat_type = if is_group { "group" } else { "dm" }.to_string();
    source.user_id = Some(sender.clone());
    source.user_name = source_name.or_else(|| Some(sender.clone()));
    source.user_id_alt = id_field(envelope, "sourceUuid");
    source.chat_id_alt = group_id;
    source.message_id = timestamp.map(|timestamp| timestamp.to_string());

    let media = media_from(data);
    let text = string_field(data, "message").unwrap_or_default();
    // Contentless envelopes still carry a `dataMessage` wrapper (profile-key updates, empty
    // sends); hermes skips them so an empty message cannot open a full agent turn.
    if text.trim().is_empty() && media.urls.is_empty() {
        return None;
    }

    let quote = data.get("quote").filter(|value| value.is_object());
    let mut event = MessageEvent::new(text, source);
    event.message_type = media.message_type;
    // hermes mirrors the author onto the event for per-message prompt builders.
    event.user_id = event.source.user_id.clone();
    event.user_name = event.source.user_name.clone();
    event.raw_message = Some(entry.clone());
    event.message_id = timestamp.map(|timestamp| timestamp.to_string());
    event.platform_update_id = timestamp;
    event.reply_to_message_id = quote.and_then(|quote| id_field(quote, "id"));
    event.reply_to_text = quote.and_then(|quote| string_field(quote, "text"));
    event.reply_to_author_id = quote.and_then(quote_author_id);
    event.media_urls = media.urls;
    event.media_types = media.types;
    event.media_filenames = media.filenames;
    Some(event)
}

/// Inbound attachment facts, every vector parallel to `media_urls`.
#[derive(Debug, Default)]
struct InboundMedia {
    message_type: MessageType,
    urls: Vec<String>,
    types: Vec<String>,
    filenames: Vec<String>,
}

/// Collect `attachments[]` into parallel url/type/name vectors plus the derived message type.
///
/// Each entry's reference is its `id`, falling back to its `filename`; an attachment with neither
/// has nothing a caller could resolve, so it is dropped instead of recorded as an empty url. Types
/// and filenames keep the vectors parallel, `""` meaning the daemon did not say.
fn media_from(data: &Value) -> InboundMedia {
    let mut media = InboundMedia::default();
    let attachments = data.get("attachments").and_then(Value::as_array);
    for attachment in attachments.into_iter().flatten() {
        let Some(reference) =
            id_field(attachment, "id").or_else(|| string_field(attachment, "filename"))
        else {
            continue;
        };
        media
            .types
            .push(string_field(attachment, "contentType").unwrap_or_default());
        media
            .filenames
            .push(string_field(attachment, "filename").unwrap_or_default());
        media.urls.push(reference);
    }
    media.message_type = if media.urls.is_empty() {
        MessageType::Text
    } else {
        // First matching MIME prefix wins; anything else is a document, so a caller can still
        // surface the bytes as a file attachment.
        media
            .types
            .iter()
            .find_map(|content_type| media_type_for(content_type))
            .unwrap_or(MessageType::Document)
    };
    media
}

/// Signal's MIME-prefix classification (hermes `_MEDIA_TYPE_BY_MIME_PREFIX`), `None` when the type
/// is unknown or absent.
fn media_type_for(content_type: &str) -> Option<MessageType> {
    let content_type = content_type.trim().to_ascii_lowercase();
    if content_type.starts_with("audio/") {
        Some(MessageType::Voice)
    } else if content_type.starts_with("image/") {
        Some(MessageType::Photo)
    } else if content_type.starts_with("video/") {
        Some(MessageType::Video)
    } else {
        None
    }
}

/// The sender's best identifier: `sourceNumber`, then `sourceUuid`, then the legacy `source`
/// (hermes reads the same three in that order). Without any of them the envelope cannot be routed.
fn sender_of(envelope: &Value) -> Option<String> {
    id_field(envelope, "sourceNumber")
        .or_else(|| id_field(envelope, "sourceUuid"))
        .or_else(|| id_field(envelope, "source"))
}

/// Envelope timestamp in milliseconds. signal-cli keeps it on the envelope; the `dataMessage` copy
/// is the fallback for payloads that drop the wrapper field.
fn timestamp_of(envelope: &Value, data: &Value) -> Option<i64> {
    number_field(envelope, "timestamp").or_else(|| number_field(data, "timestamp"))
}

/// The quoted author's identifier, first key present in [`QUOTE_AUTHOR_KEYS`] order.
fn quote_author_id(quote: &Value) -> Option<String> {
    QUOTE_AUTHOR_KEYS
        .iter()
        .find_map(|key| string_field(quote, key))
}

/// A non-blank string field, trimmed.
fn string_field(value: &Value, key: &str) -> Option<String> {
    let text = value.get(key)?.as_str()?.trim();
    if text.is_empty() {
        None
    } else {
        Some(text.to_string())
    }
}

/// A Signal identifier as a string. phone numbers, UUIDs, group ids and timestamps arrive as strings
/// in signal-cli output, but mocked or newer payloads may carry them as numbers, so both are read.
fn id_field(value: &Value, key: &str) -> Option<String> {
    match value.get(key)? {
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

/// An integer field, accepted as a JSON number or a numeric string.
fn number_field(value: &Value, key: &str) -> Option<i64> {
    match value.get(key)? {
        Value::Number(number) => number.as_i64(),
        Value::String(text) => text.trim().parse::<i64>().ok(),
        _ => None,
    }
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

/// Map a non-2xx `signal-cli-rest-api` response onto the send failure taxonomy.
///
/// 429 → `rate_limited` (retryable, carrying the server's `Retry-After`), 401/403 → `forbidden`,
/// 404 → `not_found`, 400 → `bad_format`, 413 → `too_long` (the only status a media send can hit
/// that a text send cannot: an upload the bridge refuses to read), 5xx → `transient` (retryable).
/// Statuses outside the taxonomy degrade to `unknown`, which no consumer retries.
fn classify_failure(response: &HttpResponse) -> SendResult {
    let message = format!(
        "signal api returned {}: {}",
        response.status,
        body_excerpt(&response.body)
    );
    match response.status {
        429 => SendResult::failed(message, Some("rate_limited")).retryable(response.retry_after),
        401 | 403 => SendResult::failed(message, Some("forbidden")),
        404 => SendResult::failed(message, Some("not_found")),
        400 => SendResult::failed(message, Some("bad_format")),
        413 => SendResult::failed(message, Some("too_long")),
        500..=599 => SendResult::failed(message, Some("transient")).retryable(None),
        _ => SendResult::failed(message, Some("unknown")),
    }
}

/// Classify a transport-level failure: the request never produced a response, so the send is
/// retryable (`transient`).
fn transport_failure(err: &TransportError) -> SendResult {
    SendResult::failed(format!("signal transport error: {err}"), Some("transient")).retryable(None)
}

/// Attach the timestamps of already-delivered chunks to a failure so a retry sends only the tail
/// (`_with_partial_send` in the Telegram adapter uses the same marker).
fn mark_partial(mut result: SendResult, delivered: &[String], total: usize) -> SendResult {
    if delivered.is_empty() {
        return result;
    }
    result = result.with_partial_overflow(delivered.len() as u32, total as u32);
    result.continuation_message_ids = delivered.to_vec();
    result
}

/// The gateway's platform handle for this adapter. The constant is a fixed non-empty name, so the
/// fallback arm is unreachable and exists only to keep this function panic-free.
fn signal_platform() -> Platform {
    match Platform::new(platform_names::SIGNAL) {
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
        /// For every path listed in a request's `attachments` array: did that file exist at the
        /// moment the request was executed? Proves a staged temp attachment was on disk while the
        /// send was in flight (and, with a later check, that it has been removed since).
        staged_presence: Mutex<Vec<bool>>,
    }

    impl MockTransport {
        fn new(responses: Vec<Result<HttpResponse, TransportError>>) -> Arc<Self> {
            Arc::new(Self {
                requests: Mutex::new(Vec::new()),
                responses: Mutex::new(responses.into()),
                staged_presence: Mutex::new(Vec::new()),
            })
        }

        fn requests(&self) -> Vec<HttpRequest> {
            lock(&self.requests).clone()
        }

        fn staged_presence(&self) -> Vec<bool> {
            lock(&self.staged_presence).clone()
        }
    }

    #[async_trait]
    impl Transport for MockTransport {
        async fn execute(&self, request: HttpRequest) -> Result<HttpResponse, TransportError> {
            if let Some(bytes) = request.body.as_ref()
                && let Ok(body) = serde_json::from_slice::<Value>(bytes)
                && let Some(paths) = body.get("attachments").and_then(Value::as_array)
            {
                for path in paths.iter().filter_map(Value::as_str) {
                    lock(&self.staged_presence).push(std::path::Path::new(path).exists());
                }
            }
            lock(&self.requests).push(request);
            match lock(&self.responses).pop_front() {
                Some(response) => response,
                None => Err(TransportError::Request(
                    "mock transport ran out of scripted responses".to_string(),
                )),
            }
        }
    }

    /// Poison-free guard: a panic in one test must not cascade into an unreadable failure.
    fn lock<T>(guard: &Mutex<T>) -> MutexGuard<'_, T> {
        guard
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    fn response(status: u16, body: Value) -> HttpResponse {
        HttpResponse {
            status,
            body: body.to_string(),
            retry_after: None,
        }
    }

    fn json_response(status: u16, body: Value) -> Result<HttpResponse, TransportError> {
        Ok(response(status, body))
    }

    fn raw_response(status: u16, body: &str) -> Result<HttpResponse, TransportError> {
        Ok(HttpResponse {
            status,
            body: body.to_string(),
            retry_after: None,
        })
    }

    fn body_json(request: &HttpRequest) -> Value {
        let bytes = request.body.clone().unwrap_or_default();
        serde_json::from_slice(&bytes).unwrap_or(Value::Null)
    }

    /// An adapter pointed at a loopback daemon. The trailing slash proves `new` normalizes it.
    fn adapter(mock: &Arc<MockTransport>) -> SignalAdapter {
        SignalAdapter::new(mock.clone(), "http://127.0.0.1:8080/", "+15551234567")
    }

    fn sent(timestamp: u64) -> Result<HttpResponse, TransportError> {
        json_response(200, json!({ "timestamp": timestamp.to_string() }))
    }

    /// Unwrap a poll result, failing the test with the adapter's own error text.
    fn polled(result: Result<Vec<MessageEvent>, String>) -> Vec<MessageEvent> {
        match result {
            Ok(events) => events,
            Err(error) => panic!("poll failed: {error}"),
        }
    }

    fn envelope(timestamp: i64, data: Value) -> Value {
        json!({
            "envelope": {
                "sourceNumber": "+15557654321",
                "sourceUuid": "uuid-sender",
                "sourceName": "Ann",
                "timestamp": timestamp,
                "dataMessage": data,
            }
        })
    }

    #[tokio::test]
    async fn connect_is_true_only_for_a_json_object_from_about() {
        let mock = MockTransport::new(vec![json_response(
            200,
            json!({ "versions": ["0.1"], "build": 42 }),
        )]);
        let mut adapter = adapter(&mock);
        assert!(adapter.connect(false).await);
        let requests = mock.requests();
        assert_eq!(requests.len(), 1);
        assert_eq!(requests[0].method, HttpMethod::Get);
        assert_eq!(requests[0].url, "http://127.0.0.1:8080/v1/about");
    }

    #[tokio::test]
    async fn connect_rejects_failures() {
        // A non-200 answer from a daemon that is up but unhappy.
        let unavailable = MockTransport::new(vec![json_response(503, json!({ "error": "down" }))]);
        assert!(!adapter(&unavailable).connect(false).await);

        // 200 with a JSON array or a JSON scalar: not the daemon's version report.
        let array = MockTransport::new(vec![json_response(200, json!([1, 2, 3]))]);
        assert!(!adapter(&array).connect(false).await);
        let scalar = MockTransport::new(vec![json_response(200, json!("ok"))]);
        assert!(!adapter(&scalar).connect(false).await);

        // 200 with a body that is not JSON at all.
        let garbage = MockTransport::new(vec![raw_response(200, "<html>nope</html>")]);
        assert!(!adapter(&garbage).connect(false).await);

        // No daemon listening.
        let down = MockTransport::new(vec![Err(TransportError::Request("dns".to_string()))]);
        assert!(!adapter(&down).connect(false).await);
    }

    #[tokio::test]
    async fn send_posts_number_recipient_and_message() {
        let mock = MockTransport::new(vec![sent(1_700_000_000_123)]);
        let result = adapter(&mock)
            .send("+15557654321", "hello", None, None)
            .await;

        assert!(result.success, "{result:?}");
        assert_eq!(result.message_id.as_deref(), Some("1700000000123"));
        assert_eq!(result.error_kind, None);
        assert_eq!(
            result.continuation_message_ids,
            vec!["1700000000123".to_string()]
        );

        let requests = mock.requests();
        assert_eq!(requests.len(), 1);
        assert_eq!(requests[0].method, HttpMethod::Post);
        assert_eq!(requests[0].url, "http://127.0.0.1:8080/v2/send");
        let body = body_json(&requests[0]);
        assert_eq!(body["number"], json!("+15551234567"));
        assert_eq!(body["recipients"], json!(["+15557654321"]));
        assert_eq!(body["message"], json!("hello"));
        // Nothing the caller did not supply is invented.
        assert!(body.get("quote_timestamp").is_none(), "{body}");
        assert!(body.get("quote_author").is_none(), "{body}");
        assert!(body.get("attachments").is_none(), "{body}");
    }

    #[tokio::test]
    async fn quote_fields_ride_only_when_the_anchor_is_a_timestamp() {
        // A timestamp anchor plus an author the caller supplied.
        let quoted = MockTransport::new(vec![sent(1)]);
        let metadata = json!({ "quote_author": "+15551234567" });
        assert!(
            adapter(&quoted)
                .send("+15557654321", "hi", Some("1700000000000"), Some(metadata))
                .await
                .success
        );
        let body = body_json(&quoted.requests()[0]);
        assert_eq!(body["quote_timestamp"], json!("1700000000000"));
        assert_eq!(body["quote_author"], json!("+15551234567"));

        // The same anchor without metadata: the timestamp quotes, no author is invented.
        let anonymous = MockTransport::new(vec![sent(2)]);
        assert!(
            adapter(&anonymous)
                .send("+15557654321", "hi", Some("1700000000000"), None)
                .await
                .success
        );
        let body = body_json(&anonymous.requests()[0]);
        assert_eq!(body["quote_timestamp"], json!("1700000000000"));
        assert!(body.get("quote_author").is_none(), "{body}");

        // A non-timestamp anchor cannot be a Signal quote, so no quote fields at all.
        let opaque = MockTransport::new(vec![sent(3)]);
        let metadata = json!({ "quote_author": "+15551234567" });
        assert!(
            adapter(&opaque)
                .send("+15557654321", "hi", Some("msg-17"), Some(metadata))
                .await
                .success
        );
        let body = body_json(&opaque.requests()[0]);
        assert!(body.get("quote_timestamp").is_none(), "{body}");
        assert!(body.get("quote_author").is_none(), "{body}");
    }

    #[tokio::test]
    async fn long_messages_are_chunked_within_the_unit_cap() {
        let content: String = (0..400)
            .map(|idx| format!("line {idx} of a signal reply that has to be split somewhere\n"))
            .collect();
        assert!(
            utf16_len(&content) > SIGNAL_MAX_MESSAGE_LENGTH,
            "test content must exceed one message"
        );
        let expected = truncate_message(&content, SIGNAL_MAX_MESSAGE_LENGTH, utf16_len);
        assert!(expected.len() > 1, "content should need several chunks");
        let mock = MockTransport::new(
            (0..expected.len())
                .map(|idx| sent(1_700_000_000_000 + idx as u64))
                .collect(),
        );

        let result = adapter(&mock)
            .send("+15557654321", &content, None, None)
            .await;

        assert!(result.success, "{result:?}");
        let requests = mock.requests();
        assert_eq!(requests.len(), expected.len());
        for (request, chunk) in requests.iter().zip(&expected) {
            assert_eq!(request.url, "http://127.0.0.1:8080/v2/send");
            let text = body_json(request)["message"]
                .as_str()
                .unwrap_or_default()
                .to_string();
            assert_eq!(&text, chunk);
            assert!(
                utf16_len(&text) <= SIGNAL_MAX_MESSAGE_LENGTH,
                "chunk of {} units exceeds the cap: {text:?}",
                utf16_len(&text)
            );
            assert_eq!(body_json(request)["recipients"], json!(["+15557654321"]));
        }
        // Every id is reported in send order and the last chunk's timestamp is the message id.
        assert_eq!(result.continuation_message_ids.len(), expected.len());
        let last = format!("{}", 1_700_000_000_000u64 + expected.len() as u64 - 1);
        assert_eq!(result.message_id.as_deref(), Some(last.as_str()));
    }

    #[tokio::test]
    async fn rate_limit_is_rate_limited_with_the_retry_after() {
        let mock = MockTransport::new(vec![Ok(HttpResponse {
            status: 429,
            body: json!({ "error": "Too many requests" }).to_string(),
            retry_after: Some(12.0),
        })]);
        let result = adapter(&mock)
            .send("+15557654321", "hello", None, None)
            .await;

        assert!(!result.success);
        assert_eq!(result.error_kind.as_deref(), Some("rate_limited"));
        assert!(result.retryable);
        assert_eq!(result.retry_after, Some(12.0));
        assert!(
            result
                .error
                .as_deref()
                .unwrap_or_default()
                .contains("Too many"),
            "{result:?}"
        );
    }

    #[tokio::test]
    async fn send_failures_map_to_the_taxonomy() {
        let cases: [(u16, &str, bool); 4] = [
            (401, "forbidden", false),
            (403, "forbidden", false),
            (404, "not_found", false),
            (400, "bad_format", false),
        ];
        for (status, expected, retryable) in cases {
            let mock = MockTransport::new(vec![json_response(status, json!({ "error": "nope" }))]);
            let result = adapter(&mock)
                .send("+15557654321", "hello", None, None)
                .await;
            assert!(!result.success, "{status}");
            assert_eq!(result.error_kind.as_deref(), Some(expected), "{status}");
            assert_eq!(result.retryable, retryable, "{status}");
        }

        // 5xx and a dead daemon are worth retrying.
        let server =
            MockTransport::new(vec![json_response(502, json!({ "error": "bad gateway" }))]);
        let result = adapter(&server)
            .send("+15557654321", "hi", None, None)
            .await;
        assert_eq!(result.error_kind.as_deref(), Some("transient"));
        assert!(result.retryable);

        let down = MockTransport::new(vec![Err(TransportError::Request("reset".to_string()))]);
        let result = adapter(&down).send("+15557654321", "hi", None, None).await;
        assert_eq!(result.error_kind.as_deref(), Some("transient"));
        assert!(result.retryable);
        assert!(
            result
                .error
                .as_deref()
                .unwrap_or_default()
                .contains("signal transport error"),
            "{result:?}"
        );
    }

    #[tokio::test]
    async fn mid_way_failure_reports_the_chunks_already_delivered() {
        let content: String = (0..400)
            .map(|idx| format!("line {idx} of a signal reply that has to be split somewhere\n"))
            .collect();
        let expected = truncate_message(&content, SIGNAL_MAX_MESSAGE_LENGTH, utf16_len);
        assert!(expected.len() > 2, "need at least three chunks");
        // Two chunks land, the third is refused.
        let mock = MockTransport::new(vec![
            sent(11),
            sent(12),
            json_response(403, json!({ "error": "blocked" })),
        ]);
        let result = adapter(&mock)
            .send("+15557654321", &content, None, None)
            .await;

        assert!(!result.success);
        assert_eq!(result.error_kind.as_deref(), Some("forbidden"));
        assert_eq!(result.continuation_message_ids, vec!["11", "12"]);
        match result.partial_overflow() {
            Some((delivered, total)) => {
                assert_eq!(delivered, 2);
                assert_eq!(total as usize, expected.len());
            }
            None => panic!("a failure after delivered chunks must carry the partial marker"),
        }
    }

    #[tokio::test]
    async fn blank_content_is_refused_without_a_request() {
        let mock = MockTransport::new(Vec::new());
        let result = adapter(&mock)
            .send("+15557654321", "  \n\t ", None, None)
            .await;
        assert!(!result.success);
        assert_eq!(result.error_kind.as_deref(), Some("bad_format"));
        assert!(mock.requests().is_empty());
    }

    #[tokio::test]
    async fn supports_media_is_true() {
        let mock = MockTransport::new(Vec::new());
        assert!(adapter(&mock).supports_media());
        assert!(mock.requests().is_empty());
    }

    /// A tiny attachment with the fields a caller usually carries.
    fn image_media(bytes: &[u8]) -> OutboundMedia {
        OutboundMedia::new(MediaKind::Image, MediaSource::Bytes(bytes.to_vec()))
    }

    #[tokio::test]
    async fn send_media_posts_an_inline_base64_attachment_with_caption_and_quote() {
        let mock = MockTransport::new(vec![sent(1_700_000_000_999)]);
        let media = image_media(b"hello")
            .with_filename("cat.png")
            .with_mime("image/png")
            .with_caption("look at this");

        let result = adapter(&mock)
            .send_media("+15557654321", &media, Some("1700000000000"))
            .await;

        assert!(result.success, "{result:?}");
        assert_eq!(result.message_id.as_deref(), Some("1700000000999"));
        assert_eq!(result.error_kind, None);
        assert!(result.continuation_message_ids.is_empty());

        let requests = mock.requests();
        assert_eq!(requests.len(), 1);
        assert_eq!(requests[0].method, HttpMethod::Post);
        assert_eq!(requests[0].url, "http://127.0.0.1:8080/v2/send");
        let body = body_json(&requests[0]);
        assert_eq!(body["number"], json!("+15551234567"));
        assert_eq!(body["recipients"], json!(["+15557654321"]));
        // The caption is the message, exactly as hermes' `_send_file` passes `caption or ""`.
        assert_eq!(body["message"], json!("look at this"));
        // The adapter's existing quote parameter: a timestamp anchor quotes, no author is invented.
        assert_eq!(body["quote_timestamp"], json!("1700000000000"));
        assert!(body.get("quote_author").is_none(), "{body}");
        // The bridge's named data-URL attachment form, carrying the sanitised filename.
        assert_eq!(
            body["base64_attachments"],
            json!(["data:image/png;filename=cat.png;base64,aGVsbG8="])
        );
        // Base64 mode writes nothing to disk.
        assert!(body.get("attachments").is_none(), "{body}");
        assert!(mock.staged_presence().is_empty());
    }

    #[tokio::test]
    async fn send_media_without_caption_or_name_posts_bare_base64() {
        let mock = MockTransport::new(vec![sent(7)]);
        let result = adapter(&mock)
            .send_media("+15557654321", &image_media(b"hello"), None)
            .await;

        assert!(result.success, "{result:?}");
        let body = body_json(&mock.requests()[0]);
        // hermes' `message: caption or ""`: the field is present and empty, never absent.
        assert_eq!(body["message"], json!(""));
        assert_eq!(body["base64_attachments"], json!(["aGVsbG8="]));
        assert!(body.get("quote_timestamp").is_none(), "{body}");

        // A MIME type alone selects the mime-only data-URL form; an unusable one is dropped rather
        // than smuggled into the URL grammar.
        let mime_only = MockTransport::new(vec![sent(8)]);
        let media = image_media(b"hello").with_mime("IMAGE/PNG");
        assert!(
            adapter(&mime_only)
                .send_media("+15557654321", &media, None)
                .await
                .success
        );
        let body = body_json(&mime_only.requests()[0]);
        assert_eq!(
            body["base64_attachments"],
            json!(["data:image/png;base64,aGVsbG8="])
        );

        let hostile_mime = MockTransport::new(vec![sent(9)]);
        let media = image_media(b"hello").with_mime("image/png;filename=evil.sh");
        assert!(
            adapter(&hostile_mime)
                .send_media("+15557654321", &media, None)
                .await
                .success
        );
        let body = body_json(&hostile_mime.requests()[0]);
        assert_eq!(body["base64_attachments"], json!(["aGVsbG8="]));
    }

    #[tokio::test]
    async fn send_media_sanitises_the_filename_into_one_safe_component() {
        let mock = MockTransport::new(vec![sent(11)]);
        let media = image_media(b"hello")
            .with_filename("../../etc/passwd")
            .with_mime("image/png");
        assert!(
            adapter(&mock)
                .send_media("+15557654321", &media, None)
                .await
                .success
        );
        let value = body_json(&mock.requests()[0])["base64_attachments"][0]
            .as_str()
            .unwrap_or_default()
            .to_string();
        assert_eq!(value, "data:image/png;filename=passwd;base64,aGVsbG8=");
        // Only the `image/png` MIME keeps its slash; the name carries no path at all.
        assert_eq!(value.matches('/').count(), 1, "{value}");
        assert!(!value.contains(".."), "{value}");
    }

    #[tokio::test]
    async fn send_media_file_path_mode_stages_a_temp_file_and_removes_it() {
        let mock = MockTransport::new(vec![sent(21)]);
        let media = OutboundMedia::new(MediaKind::Document, MediaSource::Bytes(b"pdf".to_vec()))
            .with_filename("notes.pdf")
            .with_caption("the notes");
        let result = adapter(&mock)
            .with_attachment_mode(AttachmentMode::FilePath)
            .send_media("+15557654321", &media, None)
            .await;

        assert!(result.success, "{result:?}");
        let requests = mock.requests();
        assert_eq!(requests.len(), 1);
        assert_eq!(requests[0].url, "http://127.0.0.1:8080/v2/send");
        let body = body_json(&requests[0]);
        assert_eq!(body["message"], json!("the notes"));
        assert!(body.get("base64_attachments").is_none(), "{body}");
        let path = body["attachments"][0]
            .as_str()
            .unwrap_or_default()
            .to_string();

        // The file existed while the request was in flight …
        assert_eq!(mock.staged_presence(), vec![true]);
        // … one path component, under the OS temp directory, uniquely named, ending in the name …
        let file = std::path::Path::new(&path);
        assert!(
            file.starts_with(std::env::temp_dir()),
            "{path} is not under the temp dir"
        );
        let name = file
            .file_name()
            .and_then(|name| name.to_str())
            .unwrap_or("");
        assert!(name.starts_with("vikram-signal-"), "{name}");
        assert!(name.ends_with("-notes.pdf"), "{name}");
        // … and it is gone once the send returns (the RAII guard ran).
        assert!(!file.exists(), "temp attachment left behind: {path}");
    }

    #[tokio::test]
    async fn send_media_file_path_mode_removes_the_file_after_a_failure() {
        let mock = MockTransport::new(vec![json_response(403, json!({ "error": "blocked" }))]);
        let media = image_media(b"hello").with_filename("cat.png");
        let result = adapter(&mock)
            .with_attachment_mode(AttachmentMode::FilePath)
            .send_media("+15557654321", &media, None)
            .await;

        assert!(!result.success);
        assert_eq!(result.error_kind.as_deref(), Some("forbidden"));
        assert_eq!(mock.staged_presence(), vec![true]);
        let path = body_json(&mock.requests()[0])["attachments"][0]
            .as_str()
            .unwrap_or_default()
            .to_string();
        assert!(
            !std::path::Path::new(&path).exists(),
            "a refused send must still clean up: {path}"
        );
    }

    #[tokio::test]
    async fn send_media_refuses_urls_without_a_request() {
        let mock = MockTransport::new(Vec::new());
        let media = OutboundMedia::image_url("https://example.test/cat.png");
        let result = adapter(&mock)
            .send_media("+15557654321", &media, None)
            .await;

        assert!(!result.success);
        assert_eq!(result.error_kind.as_deref(), Some("bad_format"));
        assert!(!result.retryable);
        let error = result.error.as_deref().unwrap_or_default();
        assert!(error.contains("MediaSource::Bytes"), "{error}");
        assert!(error.contains("https://example.test/cat.png"), "{error}");
        assert!(
            mock.requests().is_empty(),
            "a refusal must not hit the wire"
        );
    }

    #[tokio::test]
    async fn send_media_refuses_empty_bytes_without_a_request() {
        let mock = MockTransport::new(Vec::new());
        let result = adapter(&mock)
            .send_media("+15557654321", &image_media(b""), None)
            .await;

        assert!(!result.success);
        assert_eq!(result.error_kind.as_deref(), Some("bad_format"));
        assert!(
            result
                .error
                .as_deref()
                .unwrap_or_default()
                .contains("empty media"),
            "{result:?}"
        );
        assert!(mock.requests().is_empty());
    }

    #[tokio::test]
    async fn send_media_enforces_the_reference_size_cap() {
        // The cap is hermes' `SIGNAL_MAX_ATTACHMENT_SIZE`, verbatim.
        assert_eq!(SIGNAL_MAX_ATTACHMENT_SIZE, 100 * 1024 * 1024);
        assert_eq!(SIGNAL_MAX_ATTACHMENTS_PER_MSG, 32);
        // At the cap: no refusal. One byte over: refused, before any request.
        assert!(oversize_failure(SIGNAL_MAX_ATTACHMENT_SIZE).is_none());
        let over = oversize_failure(SIGNAL_MAX_ATTACHMENT_SIZE + 1)
            .as_ref()
            .map(|result| (result.success, result.error_kind.clone(), result.retryable));
        assert_eq!(over, Some((false, Some("too_long".to_string()), false)));
        assert!(oversize_failure(usize::MAX).is_some());
    }

    #[tokio::test]
    async fn send_media_rate_limit_carries_the_retry_after() {
        let mock = MockTransport::new(vec![Ok(HttpResponse {
            status: 429,
            body: json!({ "error": "Too many requests" }).to_string(),
            retry_after: Some(9.5),
        })]);
        let result = adapter(&mock)
            .send_media("+15557654321", &image_media(b"hello"), None)
            .await;

        assert!(!result.success);
        assert_eq!(result.error_kind.as_deref(), Some("rate_limited"));
        assert!(result.retryable);
        assert_eq!(result.retry_after, Some(9.5));
        assert!(
            result
                .error
                .as_deref()
                .unwrap_or_default()
                .contains("Too many"),
            "{result:?}"
        );
    }

    #[tokio::test]
    async fn send_media_failures_map_to_the_taxonomy() {
        let cases: [(u16, &str, bool); 4] = [
            (400, "bad_format", false),
            // 413 is the media-only arm: a body the bridge refuses as too large.
            (413, "too_long", false),
            (403, "forbidden", false),
            (500, "transient", true),
        ];
        for (status, expected, retryable) in cases {
            let mock = MockTransport::new(vec![json_response(status, json!({ "error": "nope" }))]);
            let result = adapter(&mock)
                .send_media("+15557654321", &image_media(b"hello"), None)
                .await;
            assert!(!result.success, "{status}");
            assert_eq!(result.error_kind.as_deref(), Some(expected), "{status}");
            assert_eq!(result.retryable, retryable, "{status}");
            assert_eq!(result.message_id, None, "{status}");

            let down = MockTransport::new(vec![Err(TransportError::Request("reset".to_string()))]);
            let transport = adapter(&down)
                .send_media("+15557654321", &image_media(b"hello"), None)
                .await;
            assert!(!transport.success, "{status}");
            assert_eq!(
                transport.error_kind.as_deref(),
                Some("transient"),
                "{status}"
            );
            assert!(transport.retryable, "{status}");
            assert!(
                transport
                    .error
                    .as_deref()
                    .unwrap_or_default()
                    .contains("signal transport error"),
                "{transport:?}"
            );
        }
    }

    #[tokio::test]
    async fn send_media_without_a_timestamp_is_not_a_reported_success() {
        // The bridge accepted the request but identified nothing: reported as a failure, like `send`.
        let mock = MockTransport::new(vec![json_response(200, json!({ "ok": true }))]);
        let result = adapter(&mock)
            .send_media("+15557654321", &image_media(b"hello"), None)
            .await;

        assert!(!result.success);
        assert_eq!(result.error_kind.as_deref(), Some("unknown"));
        assert!(result.message_id.is_none());
        assert!(
            result
                .error
                .as_deref()
                .unwrap_or_default()
                .contains("no timestamp"),
            "{result:?}"
        );
    }

    #[test]
    fn base64_encoding_matches_rfc_4648() {
        let vectors: [(&[u8], &str); 7] = [
            (b"", ""),
            (b"f", "Zg=="),
            (b"fo", "Zm8="),
            (b"foo", "Zm9v"),
            (b"foob", "Zm9vYg=="),
            (b"fooba", "Zm9vYmE="),
            (b"foobar", "Zm9vYmFy"),
        ];
        for (bytes, expected) in vectors {
            assert_eq!(base64_encode(bytes), expected);
        }
        // Every byte value round-trips through the alphabet with the expected expansion.
        let all_bytes: Vec<u8> = (0..=255).collect();
        assert_eq!(
            base64_encode(&all_bytes).len(),
            all_bytes.len().div_ceil(3) * 4
        );
        assert!(
            base64_encode(&all_bytes).ends_with("==") || base64_encode(&all_bytes).ends_with('=')
        );
    }

    #[test]
    fn filenames_are_reduced_to_one_safe_component() {
        assert_eq!(safe_filename("cat.png", MediaKind::Image), "cat.png");
        assert_eq!(safe_filename("/etc/passwd", MediaKind::Document), "passwd");
        assert_eq!(
            safe_filename("..\\..\\win.ini", MediaKind::Document),
            "win.ini"
        );
        assert_eq!(safe_filename("../../", MediaKind::Image), "attachment.png");
        assert_eq!(safe_filename("..", MediaKind::Video), "attachment.mp4");
        assert_eq!(safe_filename("", MediaKind::Audio), "attachment.m4a");
        // The data-URL delimiters and control characters never survive.
        assert_eq!(
            safe_filename("a;b,c\n d.png", MediaKind::Image),
            "a_b_c__d.png"
        );
        assert_eq!(safe_filename("héllo.png", MediaKind::Image), "h_llo.png");
        // Length is capped; the name is still a single component.
        let long = safe_filename(&"x".repeat(500), MediaKind::Image);
        assert_eq!(long.chars().count(), MAX_FILENAME_CHARS);
        assert!(!long.contains('/'));

        assert_eq!(
            attachment_filename(None, MediaKind::Document),
            "attachment.bin"
        );
        assert_eq!(
            attachment_filename(Some("a.pdf"), MediaKind::Document),
            "a.pdf"
        );
    }

    #[test]
    fn mime_tokens_are_validated_not_rewritten() {
        assert_eq!(mime_token("image/png"), "image/png");
        assert_eq!(mime_token(" IMAGE/PNG "), "image/png");
        assert_eq!(
            mime_token("application/vnd.oasis+xml"),
            "application/vnd.oasis+xml"
        );
        assert_eq!(mime_token(""), "");
        assert_eq!(mime_token("image/png;x=1"), "");
        assert_eq!(mime_token("image/png,evil"), "");
        assert_eq!(mime_token(&"a".repeat(200)), "");
    }

    #[tokio::test]
    async fn get_chat_info_falls_back_to_the_chat_id() {
        let mock = MockTransport::new(Vec::new());
        let adapter = adapter(&mock);

        let dm = adapter.get_chat_info("+15557654321").await;
        assert_eq!(dm.name, "+15557654321");
        assert_eq!(dm.chat_type, "dm");

        // The API has no lookup, so even a group id is reported as-is rather than guessed at.
        let group = adapter.get_chat_info("Z3JvdXA=").await;
        assert_eq!(group.name, "Z3JvdXA=");
        assert_eq!(group.chat_type, "dm");
        assert!(
            mock.requests().is_empty(),
            "get_chat_info must not hit the wire"
        );
    }

    #[tokio::test]
    async fn send_typing_is_a_silent_no_op() {
        let mock = MockTransport::new(Vec::new());
        adapter(&mock).send_typing("+15557654321", None).await;
        assert!(mock.requests().is_empty());
    }

    #[tokio::test]
    async fn poll_once_parses_a_dm_text_message() {
        let mock = MockTransport::new(vec![json_response(
            200,
            json!([envelope(
                1_700_000_000_123,
                json!({ "message": "hello there", "timestamp": 1_700_000_000_123i64 }),
            )]),
        )]);
        let events = polled(adapter(&mock).poll_once(5).await);

        assert_eq!(events.len(), 1);
        let event = &events[0];
        assert_eq!(event.text, "hello there");
        assert_eq!(event.message_type, MessageType::Text);
        assert_eq!(event.message_id.as_deref(), Some("1700000000123"));
        assert_eq!(event.platform_update_id, Some(1_700_000_000_123));
        assert_eq!(event.user_id.as_deref(), Some("+15557654321"));
        assert_eq!(event.user_name.as_deref(), Some("Ann"));
        assert_eq!(event.source.platform.as_str(), "signal");
        assert_eq!(event.source.chat_id, "+15557654321");
        assert_eq!(event.source.chat_type, "dm");
        assert_eq!(event.source.chat_name.as_deref(), Some("Ann"));
        assert_eq!(event.source.user_id_alt.as_deref(), Some("uuid-sender"));
        assert_eq!(event.source.chat_id_alt, None);
        assert!(event.raw_message.is_some());
        assert!(event.media_urls.is_empty());
        assert_eq!(event.reply_to_message_id, None);

        let request = &mock.requests()[0];
        assert_eq!(request.method, HttpMethod::Get);
        assert_eq!(
            request.url,
            "http://127.0.0.1:8080/v1/receive/+15551234567?timeout=5"
        );
    }

    #[tokio::test]
    async fn poll_once_parses_a_group_message_and_its_quote() {
        let mock = MockTransport::new(vec![json_response(
            200,
            json!([envelope(
                1_700_000_000_456,
                json!({
                    "message": "yes, that one",
                    "groupInfo": { "groupId": "Z3JvdXA=", "groupName": "Team" },
                    "quote": {
                        "id": 1_700_000_000_400i64,
                        "author": "+15559998888",
                        "authorNumber": "+15559998888",
                        "text": "did you mean it?",
                    },
                }),
            )]),
        )]);
        let events = polled(adapter(&mock).poll_once(0).await);

        assert_eq!(events.len(), 1);
        let event = &events[0];
        assert_eq!(event.text, "yes, that one");
        // Group messages route to the group id, with the sender kept as the author.
        assert_eq!(event.source.chat_id, "Z3JvdXA=");
        assert_eq!(event.source.chat_type, "group");
        assert_eq!(event.source.chat_name.as_deref(), Some("Team"));
        assert_eq!(event.source.chat_id_alt.as_deref(), Some("Z3JvdXA="));
        assert_eq!(event.user_id.as_deref(), Some("+15557654321"));
        assert_eq!(event.source.user_id.as_deref(), Some("+15557654321"));
        // Signal's reply anchor is the quoted message's timestamp.
        assert_eq!(event.reply_to_message_id.as_deref(), Some("1700000000400"));
        assert_eq!(event.reply_to_author_id.as_deref(), Some("+15559998888"));
        assert_eq!(event.reply_to_text.as_deref(), Some("did you mean it?"));
        // No timeout parameter when the caller asks for none.
        assert_eq!(
            mock.requests()[0].url,
            "http://127.0.0.1:8080/v1/receive/+15551234567?timeout=0"
        );
    }

    #[tokio::test]
    async fn poll_once_records_attachments() {
        let mock = MockTransport::new(vec![json_response(
            200,
            json!([
                envelope(
                    1_700_000_000_500,
                    json!({
                        "message": "",
                        "attachments": [
                            {
                                "id": "att-1",
                                "contentType": "image/png",
                                "filename": "cat.png",
                                "size": 1234,
                            },
                            { "contentType": "application/pdf", "filename": "notes.pdf" },
                        ],
                    }),
                ),
                // An attachment with neither an id nor a name references nothing: dropped, and the
                // envelope has no content left, so it is skipped entirely.
                envelope(
                    1_700_000_000_600,
                    json!({ "attachments": [{ "contentType": "image/jpeg" }] }),
                ),
            ]),
        )]);
        let events = polled(adapter(&mock).poll_once(30).await);

        assert_eq!(events.len(), 1);
        let event = &events[0];
        assert_eq!(event.message_type, MessageType::Photo);
        assert_eq!(event.text, "");
        // The first attachment is referenced by id, the second falls back to its filename.
        assert_eq!(event.media_urls, vec!["att-1", "notes.pdf"]);
        assert_eq!(
            event.media_types,
            vec!["image/png", "application/pdf"],
            "types stay parallel to media_urls"
        );
        assert_eq!(event.media_filenames, vec!["cat.png", "notes.pdf"]);
    }

    #[tokio::test]
    async fn poll_once_skips_receipts_typings_and_contentless_envelopes() {
        let mock = MockTransport::new(vec![json_response(
            200,
            json!([
                {
                    "envelope": {
                        "sourceNumber": "+15557654321",
                        "timestamp": 1,
                        "receiptMessage": { "isDelivery": true, "timestamps": [1] },
                    }
                },
                {
                    "envelope": {
                        "sourceNumber": "+15557654321",
                        "timestamp": 2,
                        "typingMessage": { "action": "STARTED" },
                    }
                },
                // A dataMessage with no text and no media cannot open a turn.
                envelope(3, json!({ "message": "   " })),
                envelope(4, json!({ "message": "the only real message" })),
            ]),
        )]);
        let events = polled(adapter(&mock).poll_once(1).await);

        assert_eq!(events.len(), 1);
        assert_eq!(events[0].text, "the only real message");
        assert_eq!(events[0].message_id.as_deref(), Some("4"));
    }

    #[tokio::test]
    async fn poll_once_reports_transport_and_protocol_failures() {
        let rejected = MockTransport::new(vec![json_response(500, json!({ "error": "boom" }))]);
        let error = adapter(&rejected)
            .poll_once(10)
            .await
            .expect_err("500 must fail");
        assert!(error.contains("500"), "{error}");

        let garbage = MockTransport::new(vec![raw_response(200, "not json")]);
        let error = adapter(&garbage)
            .poll_once(10)
            .await
            .expect_err("invalid json must fail");
        assert!(error.contains("invalid json"), "{error}");

        let object = MockTransport::new(vec![json_response(200, json!({ "error": "nope" }))]);
        let error = adapter(&object)
            .poll_once(10)
            .await
            .expect_err("a non-array body must fail");
        assert!(error.contains("non-array"), "{error}");

        let down = MockTransport::new(vec![Err(TransportError::Request("timeout".to_string()))]);
        let error = adapter(&down)
            .poll_once(10)
            .await
            .expect_err("a transport error must fail");
        assert!(error.contains("timeout"), "{error}");
    }

    #[tokio::test]
    async fn poll_once_returns_nothing_for_an_empty_batch() {
        let mock = MockTransport::new(vec![json_response(200, json!([]))]);
        assert!(polled(adapter(&mock).poll_once(20).await).is_empty());
    }

    #[test]
    fn helper_parsing_is_defensive() {
        // Timestamps: epoch-sized digits only.
        assert!(is_timestamp_like("1700000000123"));
        assert!(!is_timestamp_like("170000000"));
        assert!(!is_timestamp_like("+15551234567"));
        assert!(!is_timestamp_like(""));
        assert!(!is_timestamp_like("17000000001a3"));

        // Sender preference: sourceNumber, then sourceUuid, then source.
        assert_eq!(
            sender_of(&json!({ "sourceNumber": "+1", "sourceUuid": "u", "source": "s" }))
                .as_deref(),
            Some("+1")
        );
        assert_eq!(
            sender_of(&json!({ "sourceUuid": "u", "source": "s" })).as_deref(),
            Some("u")
        );
        assert_eq!(sender_of(&json!({ "source": "s" })).as_deref(), Some("s"));
        assert_eq!(sender_of(&json!({ "sourceNumber": "  " })), None);
        assert_eq!(sender_of(&json!({})), None);

        // Numbers are accepted as ids and timestamps, strings too.
        assert_eq!(id_field(&json!({ "id": 7 }), "id").as_deref(), Some("7"));
        assert_eq!(
            id_field(&json!({ "id": " x " }), "id").as_deref(),
            Some("x")
        );
        assert_eq!(id_field(&json!({ "id": "" }), "id"), None);
        assert_eq!(
            number_field(&json!({ "t": 1_700_000_000_123u64 }), "t"),
            Some(1_700_000_000_123)
        );
        assert_eq!(number_field(&json!({ "t": "42" }), "t"), Some(42));
        assert_eq!(number_field(&json!({ "t": "abc" }), "t"), None);

        // A quote author is read from the first known key.
        assert_eq!(
            quote_author_id(&json!({ "authorNumber": "+1555", "authorUuid": "u" })).as_deref(),
            Some("+1555")
        );
        assert_eq!(quote_author_id(&json!({})), None);

        // MIME prefixes classify media, unknown types do not.
        assert_eq!(media_type_for("image/png"), Some(MessageType::Photo));
        assert_eq!(media_type_for("AUDIO/ogg"), Some(MessageType::Voice));
        assert_eq!(media_type_for("video/mp4"), Some(MessageType::Video));
        assert_eq!(media_type_for("application/pdf"), None);
        assert_eq!(media_type_for(""), None);

        // Long error bodies are excerpted, never dumped whole.
        assert_eq!(body_excerpt("  "), "<empty body>");
        assert!(body_excerpt(&"x".repeat(400)).chars().count() <= BODY_EXCERPT_CHARS + 1);
    }
}
