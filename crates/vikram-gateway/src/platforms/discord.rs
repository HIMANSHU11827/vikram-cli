//! Discord adapter — the REST half of `gateway/platforms/discord` (API v10).
//!
//! Everything here rides on the frozen [`Transport`](crate::transport::Transport): build an
//! [`HttpRequest`], hand it over, classify the response. No discord.py equivalent exists in this
//! port, so the adapter talks to the documented REST endpoints directly.
//!
//! Ported: bot identity probe (`GET /users/@me`), message create with the 2000-code-point budget
//! (`truncate_message`), reply references, `allowed_mentions` passthrough, channel lookup
//! (`GET /channels/{id}`), the best-effort typing indicator, single-attachment uploads
//! (`multipart/form-data` with `payload_json` + `files[0]`) and the bot's own reactions
//! (`PUT`/`DELETE /channels/{id}/messages/{id}/reactions/{emoji}/@me`), plus rich cards posted as
//! `embeds` with a plain `content` mirror.
//!
//! Fidelity notes (full accounting in `docs/gateway-port-plan.md`):
//! - Edits and deletes are real `PATCH`/`DELETE` requests; the transport exposes the full verb set.
//! - hermes synthesizes a default `AllowedMentions` (`@everyone` / roles denied, users allowed).
//!   That policy needs config plumbing this adapter does not own, so only an explicitly supplied
//!   `metadata["allowed_mentions"]` is forwarded; nothing is invented.
//! - hermes hops to `metadata["thread_id"]`, opens forum posts, syncs slash commands, joins voice
//!   channels and runs a persistent typing loop. All of that needs the
//!   gateway WebSocket client, which is outside this REST adapter's surface.
//! - Reactions are REST-only (`Message.add_reaction` / `remove_reaction` in discord.py), so they are
//!   ported even though hermes only calls them from gateway-side lifecycle hooks: `PUT`/`DELETE`
//!   `/channels/{channel_id}/messages/{message_id}/reactions/{emoji}/@me`, success only on the
//!   documented `204 No Content`, best-effort otherwise.
//! - hermes labels threads `"thread"`; this port folds thread channel types (10/11/12) into
//!   `"group"`, the vocabulary [`ChatInfo`] documents.
//! - A mid-payload failure keeps the chunks that already landed and reports
//!   `partial_overflow`, mirroring hermes' overflow-split contract, so the delivery ledger resends
//!   only the tail.
//! - Uploads follow `adapter_media.py`: `POST /channels/{id}/messages` with `multipart/form-data`
//!   parts `payload_json` (caption, plus `message_reference` for replies) and `files[0]`
//!   (filename + content type). That is the exact shape hermes hand-builds for native voice
//!   messages (`_post_voice_message`, adapter_media.py:405-412) and the shape discord.py's
//!   `channel.send(files=[...])` produces for every other attachment path.
//! - `MediaSource::Url` is refused, truthfully: hermes downloads URL media and uploads the bytes
//!   (`_send_url_media`, adapter_media.py:438-475) — it never hands a bare URL to the API — and
//!   this transport only surfaces text bodies (`response.text()` in `transport.rs`), so a
//!   fetch-through would corrupt binary content. Callers pass [`MediaSource::Bytes`].
//! - The upload cap is the flat [`DISCORD_MAX_ATTACHMENT_BYTES`] this port documents; hermes
//!   floors a per-guild `filesize_limit` at a 20 MiB platform default
//!   (`_DISCORD_DEFAULT_UPLOAD_LIMIT_BYTES`, adapter_media.py:18-24), which needs guild state
//!   this adapter does not hold.
//! - A `200` whose message object carries no `attachments` is a failure, mirroring hermes'
//!   "accepted the message but attached no files" guard (adapter_media.py:146-160) instead of
//!   reporting a silent drop as delivered.
//! - Cards are one embed object or `{"embeds": [...], "content": …}`, posted as
//!   `{"embeds": [...], "content": <mirror>}`. The reference always carries the payload as plain
//!   `content` next to the embed (adapter.py:5621-5622, 5667, 5724) because embeds can be
//!   invisible or detached on some clients; this port derives that mirror from the embeds'
//!   `title`/`description` when the caller supplies none. Interactive `View`s (buttons, selects,
//!   modals) are not ported: they are driven by the interaction runtime, which sits above this
//!   REST adapter.

use async_trait::async_trait;
use serde_json::{Map, Value, json};
use std::sync::Mutex;
use std::sync::atomic::{AtomicI64, AtomicU64, Ordering};
use std::time::Duration;

use crate::event::{MessageEvent, Platform, SessionSource};
use crate::helpers::{codepoint_len, truncate_message};
use crate::platform::{
    ChatInfo, MediaKind, MediaSource, OutboundMedia, PlatformAdapter, SendResult,
};
use crate::socket::SharedLineSocket;
use crate::transport::{HttpRequest, HttpResponse, SharedTransport, TransportError};

/// Base URL of the Discord REST API.
pub const DISCORD_API_BASE: &str = "https://discord.com/api/v10";

/// Discord's hard limit on message content, measured in code points.
pub const DISCORD_MAX_MESSAGE_LENGTH: usize = 2000;

/// Discord's per-attachment upload cap, in bytes (25 MiB).
///
/// Applied to `MediaSource::Bytes` before anything is sent, so a doomed upload never reaches the
/// network. hermes varies the cap per guild boost tier; see the module fidelity notes.
pub const DISCORD_MAX_ATTACHMENT_BYTES: usize = 25 * 1024 * 1024;

/// Discord's documented cap on the embeds one message may carry.
///
/// A create-message body holds at most ten embeds; an eleventh is refused locally so a caller-sized
/// card never costs a round trip Discord is certain to reject.
pub const DISCORD_MAX_EMBEDS: usize = 10;

/// Discord rejects bot traffic without a user agent in the `DiscordBot (url, version)` shape.
const USER_AGENT: &str = concat!(
    "DiscordBot (https://github.com/VIKRAM-CLI/vikram, ",
    env!("CARGO_PKG_VERSION"),
    ")"
);

/// Characters of an error body kept in a [`SendResult::error`] message.
const BODY_EXCERPT_CHARS: usize = 200;

/// Discord's documented answer for both reaction routes: `204 No Content`, with no body.
///
/// Nothing else counts. A `200` is not what the API documents for `PUT`/`DELETE …/reactions/{emoji}/@me`,
/// so treating it as success would be claiming a reaction that was never confirmed.
pub const DISCORD_REACTION_SUCCESS_STATUS: u16 = 204;

/// Discord platform adapter speaking REST v10 with a bot token.
///
/// Every method is stateless: `connect` only proves the token resolves to a bot, so a failed
/// `connect` can be retried with the same instance.
pub struct DiscordAdapter {
    transport: SharedTransport,
    bot_token: String,
    api_base: String,
}

impl DiscordAdapter {
    /// Build an adapter over `transport` that authenticates as `bot_token`.
    ///
    /// The token is used verbatim in an `Authorization: Bot <token>` header; nothing is sent
    /// anywhere until a trait method is called.
    pub fn new(transport: SharedTransport, bot_token: impl Into<String>) -> Self {
        Self {
            transport,
            bot_token: bot_token.into(),
            api_base: DISCORD_API_BASE.to_string(),
        }
    }

    /// Point the adapter at a mirror or a local test double.
    pub fn with_api_base(mut self, base: impl Into<String>) -> Self {
        self.api_base = base.into().trim_end_matches('/').to_string();
        self
    }

    /// Absolute URL for an API path such as `/users/@me`.
    fn endpoint(&self, path: &str) -> String {
        format!("{}{path}", self.api_base)
    }

    /// Add the bot credentials Discord needs on every REST call.
    fn authorized(&self, request: HttpRequest) -> HttpRequest {
        request
            .header("Authorization", format!("Bot {}", self.bot_token))
            .header("User-Agent", USER_AGENT)
    }

    /// Authorized GET for `path`.
    fn get(&self, path: &str) -> HttpRequest {
        self.authorized(HttpRequest::get(self.endpoint(path)))
    }

    /// Authorized JSON POST for `path`.
    fn post_json(&self, path: &str, body: &Value) -> HttpRequest {
        self.authorized(HttpRequest::json(self.endpoint(path), body))
    }

    /// Absolute URL of the bot's own reaction on `message_id`, or `None` when the caller's input
    /// cannot address one.
    ///
    /// The route is discord.py's `HTTPClient.add_reaction` / `remove_reaction` target,
    /// `{base}/channels/{channel_id}/messages/{message_id}/reactions/{emoji}/@me`, where `@me`
    /// selects the bot's own reaction (the only one it may manage). Ids are path segments too, so
    /// they are encoded with the emoji — a slash can never escape its segment.
    fn reaction_url(&self, chat_id: &str, message_id: &str, emoji: &str) -> Option<String> {
        let chat_id = chat_id.trim();
        let message_id = message_id.trim();
        if chat_id.is_empty() || message_id.is_empty() {
            return None;
        }
        let emoji = reaction_path_segment(emoji)?;
        Some(self.endpoint(&format!(
            "/channels/{}/messages/{}/reactions/{emoji}/@me",
            encode_path_segment(chat_id, ""),
            encode_path_segment(message_id, ""),
        )))
    }

    /// Create one message and return the decoded message object.
    ///
    /// Errors carry the classified [`SendResult`] (boxed: the success path stays small) so the
    /// caller can report the exact refusal.
    async fn post_message(&self, chat_id: &str, body: &Value) -> Result<Value, Box<SendResult>> {
        self.post_message_classified(chat_id, body, classify_failure)
            .await
    }

    /// [`post_message`](Self::post_message) with the non-2xx classifier chosen by the caller.
    ///
    /// Every create-message path shares this route and the transport's error handling; only the
    /// refinements differ, because a body Discord rejects outright (`400`) means `bad_format` for a
    /// card just as it does for an upload, while a text body is judged by its machine-readable code.
    async fn post_message_classified(
        &self,
        chat_id: &str,
        body: &Value,
        classify: fn(&HttpResponse) -> SendResult,
    ) -> Result<Value, Box<SendResult>> {
        let request = self.post_json(&format!("/channels/{chat_id}/messages"), body);
        match self.transport.execute(request).await {
            Ok(response) if response.ok() => response.json().map_err(|err| {
                Box::new(SendResult::failed(
                    format!("discord send response was not json: {err}"),
                    Some("unknown"),
                ))
            }),
            Ok(response) => Err(Box::new(classify(&response))),
            Err(err) => Err(Box::new(transport_failure(&err))),
        }
    }
}

/// `allowed_mentions` supplied by the caller, forwarded verbatim (hermes reads it from config).
fn allowed_mentions_from(metadata: Option<&Value>) -> Option<Value> {
    let value = metadata?.get("allowed_mentions")?;
    if value.is_null() {
        return None;
    }
    Some(value.clone())
}

/// Percent-encode `value` for one URL path segment, keeping the RFC 3986 unreserved characters and
/// whatever `safe` adds literal.
///
/// Every other byte — including each byte of a multi-byte UTF-8 code point — becomes `%XX`, which is
/// what discord.py's route formatter (`urllib.parse.quote` over the `{emoji}` parameter) puts on the
/// wire. Encoding is done on bytes, not chars, so a UTF-8 emoji needs no special case.
fn encode_path_segment(value: &str, safe: &str) -> String {
    let mut out = String::with_capacity(value.len());
    for byte in value.bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(char::from(byte));
            }
            other if safe.as_bytes().contains(&other) => out.push(char::from(other)),
            other => out.push_str(&format!("%{other:02X}")),
        }
    }
    out
}

/// `name:id` for a custom-emoji reference, mirroring discord.py's `PartialEmoji._as_reaction()`.
///
/// Accepts the message-wire forms `<:name:id>` and `<a:name:id>` as well as the already reduced
/// `name:id`; the `a` marker (animated) is dropped because the REST route never carries it, and `id`
/// must be all digits (a Discord snowflake). `None` means the string is not a custom-emoji
/// reference — a unicode emoji, for instance.
fn custom_emoji_reference(raw: &str) -> Option<String> {
    let inner = match raw
        .strip_prefix('<')
        .and_then(|rest| rest.strip_suffix('>'))
    {
        Some(inner) => inner,
        None => raw,
    };
    // `<a:name:id>` marks an animated emoji; only that `a:` marker is dropped, so a name that merely
    // starts with `a` (`<:art:id>`) keeps it. `<:name:id>` then leaves a leading separator colon.
    let inner = inner.strip_prefix("a:").unwrap_or(inner);
    let inner = inner.strip_prefix(':').unwrap_or(inner);
    let (name, id) = inner.split_once(':')?;
    if name.is_empty() || id.is_empty() || !id.bytes().all(|byte| byte.is_ascii_digit()) {
        return None;
    }
    Some(format!("{name}:{id}"))
}

/// The `{emoji}` path segment of a reaction route, or `None` when the caller supplied no emoji.
///
/// Two shapes reach Discord, exactly as discord.py produces them:
/// - a unicode emoji is fully percent-encoded, so `👍` becomes `%F0%9F%91%8D`;
/// - a custom emoji becomes `name:id` and keeps its colon literal, because the documented route is
///   `…/reactions/{emoji}/@me` with `name:id` standing in for `{emoji}`.
///
/// Anything else is sent verbatim (percent-encoded): a platform-defined reaction name is not this
/// port's to reinterpret.
fn reaction_path_segment(emoji: &str) -> Option<String> {
    let emoji = emoji.trim();
    if emoji.is_empty() {
        return None;
    }
    match custom_emoji_reference(emoji) {
        Some(reference) => Some(encode_path_segment(&reference, ":")),
        None => Some(encode_path_segment(emoji, "")),
    }
}

/// Whether a reaction request got Discord's documented confirmation.
///
/// Both routes answer `204 No Content` on success and nothing at all on failure, so the only honest
/// success is that status: a transport error, any non-2xx status, and even a `2xx` that is not the
/// documented `204` are all `false`.
fn reaction_succeeded(result: Result<HttpResponse, TransportError>) -> bool {
    matches!(result, Ok(response) if response.status == DISCORD_REACTION_SUCCESS_STATUS)
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

/// Discord's machine-readable error code from a decoded body (`50035` = invalid form body).
fn error_code(body: Option<&Value>) -> Option<i64> {
    body?.get("code")?.as_i64()
}

/// Map a non-2xx Discord response onto a classified [`SendResult`].
///
/// 429 → `rate_limited` (retryable, server delay preferred over any timestamp), 401/403 →
/// `forbidden`, 404 → `not_found`, 400 + code 50035 → `bad_format`, 5xx → `transient`
/// (retryable), anything else → `unknown`.
fn classify_failure(response: &HttpResponse) -> SendResult {
    let decoded = response.json().ok();
    let message = format!(
        "discord api returned {}: {}",
        response.status,
        body_excerpt(&response.body)
    );
    match response.status {
        429 => {
            let retry_after = decoded
                .as_ref()
                .and_then(|body| body.get("retry_after"))
                .and_then(Value::as_f64)
                .or(response.retry_after);
            SendResult::failed(message, Some("rate_limited")).retryable(retry_after)
        }
        401 | 403 => SendResult::failed(message, Some("forbidden")),
        404 => SendResult::failed(message, Some("not_found")),
        500..=599 => SendResult::failed(message, Some("transient")).retryable(None),
        _ => {
            if error_code(decoded.as_ref()) == Some(50035) {
                SendResult::failed(message, Some("bad_format"))
            } else {
                SendResult::failed(message, Some("unknown"))
            }
        }
    }
}

/// Classify a transport-level failure: the request never produced a response, so the send is
/// retryable (`transient`).
fn transport_failure(err: &TransportError) -> SendResult {
    SendResult::failed(format!("discord transport error: {err}"), Some("transient")).retryable(None)
}

/// Map a failed upload response onto a classified [`SendResult`].
///
/// Shares [`classify_failure`] with `send`, plus two upload-only refinements: a `400` means
/// Discord rejected the multipart body even when the machine-readable `50035` code is absent
/// (`bad_format`, never retryable), and `413` is the server-side size refusal the preflight
/// exists to avoid (`too_long`).
fn classify_media_failure(response: &HttpResponse) -> SendResult {
    let kind = match response.status {
        400 => Some("bad_format"),
        413 => Some("too_long"),
        _ => None,
    };
    match kind {
        Some(kind) => SendResult::failed(
            format!(
                "discord api returned {}: {}",
                response.status,
                body_excerpt(&response.body)
            ),
            Some(kind),
        ),
        None => classify_failure(response),
    }
}

/// Map a failed card response onto a classified [`SendResult`].
///
/// The card path reads the same taxonomy as text and media — `429` (server delay preferred over any
/// timestamp), `401`/`403`, `404`, `400`, `5xx`, transport — with the two refinements the media path
/// already applies to a body Discord rejects outright: a `400` is `bad_format` (a malformed embed is
/// a caller bug, never a retry) even without the machine-readable `50035` code, and `413` is
/// `too_long`.
fn classify_card_failure(response: &HttpResponse) -> SendResult {
    match response.status {
        400 | 413 => classify_media_failure(response),
        _ => classify_failure(response),
    }
}

/// Filename and content type Discord gets for a media kind the caller did not label.
fn default_media_attachment(kind: MediaKind) -> (&'static str, &'static str) {
    match kind {
        MediaKind::Image => ("image.png", "image/png"),
        MediaKind::Video => ("video.mp4", "video/mp4"),
        MediaKind::Audio => ("audio.mp3", "audio/mpeg"),
        MediaKind::Document => ("attachment.bin", "application/octet-stream"),
    }
}

/// Longest filename kept from a caller value, so one absurd name cannot bloat a part header.
const MAX_FILENAME_CHARS: usize = 200;

/// Make a caller string safe to interpolate into a `Content-Disposition` / `Content-Type` part
/// header: no CR/LF (header injection), no quote or backslash (parameter breakout), no other
/// control characters, and no unbounded length.
fn sanitize_part_token(raw: &str) -> String {
    let cleaned: String = raw
        .chars()
        .filter(|ch| !ch.is_control() && *ch != '"' && *ch != '\\')
        .take(MAX_FILENAME_CHARS)
        .collect();
    cleaned.trim().to_string()
}

/// A fresh `multipart/form-data` boundary for one upload.
///
/// The delimiter must not occur inside the parts, so it mixes the clock with a process counter
/// rather than being a constant. The clock read cannot fail a send: an unavailable or pre-epoch
/// system clock contributes a zero nonce and the counter still makes the token unique.
fn multipart_boundary() -> String {
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let seq = COUNTER.fetch_add(1, Ordering::Relaxed);
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|elapsed| elapsed.as_nanos() as u64)
        .unwrap_or(0);
    format!("----VIKRAMFormBoundary{nanos:016x}{seq:08x}")
}

/// `payload_json` for an upload: the caption as `content`, plus the reply anchor.
///
/// The reply shape is identical to [`PlatformAdapter::send`]'s and to the reference's
/// (`message_reference` with `fail_if_not_exists: false`): Discord v10 addresses DM channels and
/// guild channels through the same route, so neither gains a `guild_id`. A caption longer than
/// the 2000-code-point budget is cut to it — the attachment message carries one body, and `send`
/// remains the path for text that needs chunks. Blank captions are omitted rather than sent.
fn media_payload(media: &OutboundMedia, reply_to: Option<&str>) -> Value {
    let mut payload = Map::new();
    if let Some(caption) = media
        .caption
        .as_deref()
        .map(str::trim)
        .filter(|caption| !caption.is_empty())
        && let Some(head) = truncate_message(caption, DISCORD_MAX_MESSAGE_LENGTH, codepoint_len)
            .into_iter()
            .next()
    {
        payload.insert("content".to_string(), Value::String(head));
    }
    if let Some(message_id) = reply_to {
        payload.insert(
            "message_reference".to_string(),
            json!({ "message_id": message_id, "fail_if_not_exists": false }),
        );
    }
    Value::Object(payload)
}

/// Encode the `payload_json` + `files[0]` body Discord expects, CRLF-framed per RFC 7578.
///
/// `None` means the payload could not be serialized (never a panic): the caller turns that into a
/// `bad_format` refusal instead of sending a truncated body.
fn encode_media_multipart(
    boundary: &str,
    payload: &Value,
    filename: &str,
    mime: &str,
    bytes: &[u8],
) -> Option<Vec<u8>> {
    let payload_json = serde_json::to_vec(payload).ok()?;
    let mut body = Vec::with_capacity(payload_json.len() + bytes.len() + 512);
    body.extend_from_slice(format!("--{boundary}\r\n").as_bytes());
    body.extend_from_slice(b"Content-Disposition: form-data; name=\"payload_json\"\r\n");
    body.extend_from_slice(b"Content-Type: application/json\r\n\r\n");
    body.extend_from_slice(&payload_json);
    body.extend_from_slice(b"\r\n");
    body.extend_from_slice(format!("--{boundary}\r\n").as_bytes());
    body.extend_from_slice(
        format!("Content-Disposition: form-data; name=\"files[0]\"; filename=\"{filename}\"\r\n")
            .as_bytes(),
    );
    body.extend_from_slice(format!("Content-Type: {mime}\r\n\r\n").as_bytes());
    body.extend_from_slice(bytes);
    body.extend_from_slice(b"\r\n");
    body.extend_from_slice(format!("--{boundary}--\r\n").as_bytes());
    Some(body)
}

/// A validated outbound card: the embeds to post plus the plain `content` mirror.
struct CardPayload {
    /// One to [`DISCORD_MAX_EMBEDS`] embed objects, forwarded to Discord verbatim.
    embeds: Vec<Value>,
    /// Plain-text mirror of the embeds; empty when nothing in the card can be mirrored.
    content: String,
}

/// Whether a JSON value counts as a populated card field.
///
/// `null`, a blank string and an empty array or object say nothing, so an object built only from
/// those is the "empty object" Discord refuses: it would put an embed on the wire that renders as
/// nothing at all.
fn is_present_field(value: &Value) -> bool {
    match value {
        Value::Null => false,
        Value::String(text) => !text.trim().is_empty(),
        Value::Array(items) => !items.is_empty(),
        Value::Object(fields) => fields.values().any(is_present_field),
        Value::Bool(_) | Value::Number(_) => true,
    }
}

/// Read `card` as one embed object or the `{"embeds": [...], "content": …}` wrapper.
///
/// `Err` carries the refusal reason, and validation finishes before any request exists, so an
/// invalid card never reaches the network:
/// - the card must be a JSON object;
/// - an object carrying `embeds` or `content` is the wrapper form: `embeds` must be a non-empty
///   array of at most [`DISCORD_MAX_EMBEDS`] entries and `content` a string when present (a blank
///   one is treated as absent, because a blank mirror is not a fallback);
/// - any other object is one embed and must carry at least one non-empty field;
/// - every embed must be an object carrying at least one non-empty field.
///
/// The embeds themselves are not rewritten — a caller-sized card is posted as given, and Discord's
/// own `400` is reported as `bad_format`.
fn card_payload(card: &Value) -> Result<CardPayload, String> {
    let fields = card
        .as_object()
        .ok_or_else(|| "the card must be a JSON object".to_string())?;
    let wrapped = fields.contains_key("embeds") || fields.contains_key("content");
    let (embeds, supplied) = if wrapped {
        let embeds = match fields.get("embeds") {
            Some(Value::Array(embeds)) => embeds.clone(),
            Some(_) => return Err("`embeds` must be an array of embed objects".to_string()),
            None => return Err("the card carries no `embeds` array".to_string()),
        };
        let supplied = match fields.get("content") {
            None | Some(Value::Null) => None,
            Some(Value::String(text)) => Some(text.trim().to_string()).filter(|t| !t.is_empty()),
            Some(_) => return Err("`content` must be a string".to_string()),
        };
        (embeds, supplied)
    } else {
        (vec![card.clone()], None)
    };
    if embeds.is_empty() {
        return Err("the card carries no embeds".to_string());
    }
    if embeds.len() > DISCORD_MAX_EMBEDS {
        return Err(format!(
            "the card carries {} embeds, over Discord's {DISCORD_MAX_EMBEDS} embed per-message cap",
            embeds.len()
        ));
    }
    for (index, embed) in embeds.iter().enumerate() {
        let embed_fields = embed
            .as_object()
            .ok_or_else(|| format!("embed {index} is not a JSON object"))?;
        if !embed_fields.values().any(is_present_field) {
            return Err(format!("embed {index} carries no non-empty field"));
        }
    }
    let content = supplied.unwrap_or_else(|| card_fallback_content(&embeds));
    Ok(CardPayload { embeds, content })
}

/// The plain-text mirror sent alongside `embeds`: their `title`s and `description`s.
///
/// hermes never sends an embed alone. Every card it builds travels with the payload as plain
/// `content` — `send_kwargs = {"content": content, "embed": embed, …}` (adapter.py:5724) — because
/// "embeds can be invisible/detached on web/mobile, so ``content`` carries the payload"
/// (adapter.py:5621-5622, 5667). This port mirrors that rule for any card it is handed: the same
/// reader that loses the embed still sees the text.
///
/// Titles and descriptions are taken per embed in order and joined with a blank line; the whole
/// mirror is then capped to the 2000-code-point message budget with [`truncate_message`], exactly
/// the budget `send` enforces, so the mirror is always a message Discord accepts. A card whose
/// embeds carry only images, colours or fields derives an empty mirror rather than invented words.
fn card_fallback_content(embeds: &[Value]) -> String {
    let mut parts: Vec<&str> = Vec::new();
    for embed in embeds {
        for key in ["title", "description"] {
            if let Some(text) = embed.get(key).and_then(Value::as_str).map(str::trim)
                && !text.is_empty()
            {
                parts.push(text);
            }
        }
    }
    let joined = parts.join("\n\n");
    truncate_message(&joined, DISCORD_MAX_MESSAGE_LENGTH, codepoint_len)
        .into_iter()
        .next()
        .unwrap_or_default()
}

#[async_trait]
impl PlatformAdapter for DiscordAdapter {
    /// Probe `GET /users/@me`; true when Discord answers 200 with a bot id.
    async fn connect(&mut self, _is_reconnect: bool) -> bool {
        let request = self.get("/users/@me");
        match self.transport.execute(request).await {
            Ok(response) if response.status == 200 => response
                .json()
                .ok()
                .as_ref()
                .and_then(|body| body.get("id"))
                .and_then(Value::as_str)
                .is_some_and(|id| !id.trim().is_empty()),
            _ => false,
        }
    }

    /// Nothing to tear down: the adapter holds no connection, only a transport handle.
    async fn disconnect(&mut self) {}

    /// POST the content to `/channels/{chat_id}/messages`, chunked to the 2000-code-point budget.
    ///
    /// `reply_to` becomes a `message_reference` with `fail_if_not_exists: false` on the first
    /// chunk only (hermes' default `reply_to_mode = "first"`). `metadata["allowed_mentions"]` is
    /// forwarded verbatim. Chunks go out sequentially; a failure after some chunks landed reports
    /// success with `partial_overflow` and the ids already delivered.
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
        let chunks = truncate_message(content, DISCORD_MAX_MESSAGE_LENGTH, codepoint_len);
        let total = chunks.len();
        if total == 0 {
            return SendResult::failed("Refusing to send empty message", Some("bad_format"));
        }
        let allowed_mentions = allowed_mentions_from(metadata.as_ref());
        let reference = reply_to
            .map(|message_id| json!({ "message_id": message_id, "fail_if_not_exists": false }));
        let mut ids: Vec<String> = Vec::new();

        for (index, chunk) in chunks.iter().enumerate() {
            let mut body: Map<String, Value> = Map::new();
            body.insert("content".to_string(), Value::String(chunk.clone()));
            if index == 0
                && let Some(reference) = &reference
            {
                body.insert("message_reference".to_string(), reference.clone());
            }
            if let Some(allowed_mentions) = &allowed_mentions {
                body.insert("allowed_mentions".to_string(), allowed_mentions.clone());
            }

            match self.post_message(chat_id, &Value::Object(body)).await {
                Ok(message) => {
                    match message.get("id").and_then(Value::as_str) {
                        Some(id) if !id.is_empty() => ids.push(id.to_string()),
                        // Delivered but unidentified: report it, never resend blindly.
                        _ => {
                            return SendResult::failed(
                                "discord send returned no message id",
                                Some("unknown"),
                            );
                        }
                    }
                }
                Err(failure) => {
                    if let Some(last) = ids.last() {
                        // Keep the head that is already on screen; the ledger resends the tail.
                        let delivered = ids.len() as u32;
                        return SendResult::ok_split(last.clone(), ids)
                            .with_partial_overflow(delivered, total as u32);
                    }
                    return *failure;
                }
            }
        }

        match ids.last() {
            Some(last) => SendResult::ok_split(last.clone(), ids),
            None => SendResult::failed("discord send produced no messages", Some("bad_format")),
        }
    }

    /// Rich cards are implemented: a caller's embed payload rides on
    /// `POST /channels/{channel_id}/messages` as `{"embeds": [...], "content": …}`.
    fn supports_cards(&self) -> bool {
        true
    }

    /// Post rich cards as Discord embeds.
    ///
    /// `card` is either one embed object (`{"title": …, "description": …, "color": …}`) or the
    /// `{"embeds": [...], "content": …}` wrapper, and both are posted as Discord's create-message
    /// body:
    ///
    /// ```json
    /// {"embeds": [{"title": "…", "description": "…"}], "content": "<plain fallback>"}
    /// ```
    ///
    /// That is hermes' wire shape. Every interactive card it sends goes through
    /// `_send_prompt` → `channel.send(**send_kwargs)` with
    /// `send_kwargs = {"content": content, "embed": embed, "view": view}`
    /// (adapter.py:5658, 5724, 5770), and discord.py turns `embed=` into the `embeds` array on
    /// `POST /channels/{channel_id}/messages`. `content` is not decoration: the reference carries
    /// the payload as plain text because "embeds can be invisible/detached on web/mobile, so
    /// ``content`` carries the payload" (adapter.py:5621-5622, 5667). So when the caller supplies
    /// no `content` it is derived from the embeds' `title`/`description` by
    /// [`card_fallback_content`]; a card nothing can be derived from (images only) posts without a
    /// `content` key rather than with invented words.
    ///
    /// Route and auth are the adapter's existing pair: one route for DM channels and guild channels
    /// alike (`POST /channels/{chat_id}/messages`, no `guild_id`), authorized with
    /// `Authorization: Bot <token>` and the bot user agent, exactly as [`send`](Self::send) is.
    ///
    /// Refused locally, with zero requests: a blank channel id, a card that is not a JSON object,
    /// an empty card, a wrapper without an `embeds` array (or with a non-array `embeds`, a
    /// non-string `content`), more than [`DISCORD_MAX_EMBEDS`] embeds, an embed that is not an
    /// object, and an embed with no non-empty field.
    ///
    /// Success requires the created message's `id`, which is what the reference reports
    /// (`SendResult(success=True, message_id=str(msg.id))`, adapter.py:5661); the decoded message
    /// rides along in [`SendResult::raw_response`]. A `2xx` without an id is a typed failure rather
    /// than a claimed delivery, and every non-2xx goes through [`classify_card_failure`] — the
    /// `send`/media taxonomy with `400`/`413` refinements. Neither views/components nor embeds
    /// editing are ported here: components need the interaction runtime the reference drives through
    /// its `View` classes, which is outside this REST adapter's surface.
    async fn send_card(&self, chat_id: &str, card: &Value) -> SendResult {
        let chat_id = chat_id.trim();
        if chat_id.is_empty() {
            return SendResult::failed(
                "discord send card: channel id is empty",
                Some("bad_format"),
            );
        }
        let payload = match card_payload(card) {
            Ok(payload) => payload,
            Err(reason) => {
                return SendResult::failed(
                    format!("discord send card: {reason}"),
                    Some("bad_format"),
                );
            }
        };

        let mut body: Map<String, Value> = Map::new();
        body.insert("embeds".to_string(), Value::Array(payload.embeds));
        if !payload.content.trim().is_empty() {
            body.insert("content".to_string(), Value::String(payload.content));
        }

        match self
            .post_message_classified(chat_id, &Value::Object(body), classify_card_failure)
            .await
        {
            Ok(message) => match message.get("id").and_then(Value::as_str) {
                Some(id) if !id.is_empty() => {
                    let mut result = SendResult::ok(id);
                    result.raw_response = Some(message);
                    result
                }
                // Accepted but unidentified: report it, never claim a delivery.
                _ => {
                    SendResult::failed("discord send card returned no message id", Some("unknown"))
                }
            },
            Err(failure) => *failure,
        }
    }

    /// Uploads are implemented: one attachment per message, up to
    /// [`DISCORD_MAX_ATTACHMENT_BYTES`].
    fn supports_media(&self) -> bool {
        true
    }

    /// Discord's REST API exposes the reaction routes, so [`add_reaction`](Self::add_reaction) is
    /// implemented.
    fn supports_reactions(&self) -> bool {
        true
    }

    /// React to `message_id` with `PUT /channels/{channel_id}/messages/{message_id}/reactions/{emoji}/@me`.
    ///
    /// That route is what hermes reaches through `message.add_reaction(emoji)`
    /// (adapter.py:3163-3172) once discord.py turns the emoji into a URL segment:
    /// - a unicode emoji is percent-encoded byte by byte (`👍` → `%F0%9F%91%8D`);
    /// - a custom emoji becomes `name:id` — `<:name:id>`, `<a:name:id>` and a bare `name:id` are all
    ///   accepted and reduced the way `PartialEmoji._as_reaction()` reduces them.
    ///
    /// The request is refused before the network when the chat id, the message id or the emoji is
    /// blank. Reactions are best effort, but they are never a fake success: `true` requires Discord's
    /// documented `204 No Content`, so a `403`, a `404` (message gone), a `429` and a transport error
    /// are all `false`, and so is an unexpected `200`.
    async fn add_reaction(&self, chat_id: &str, message_id: &str, emoji: &str) -> bool {
        let Some(url) = self.reaction_url(chat_id, message_id, emoji) else {
            return false;
        };
        reaction_succeeded(
            self.transport
                .execute(self.authorized(HttpRequest::put(url)))
                .await,
        )
    }

    /// Remove the bot's own `emoji` reaction with
    /// `DELETE /channels/{channel_id}/messages/{message_id}/reactions/{emoji}/@me`.
    ///
    /// hermes reaches this through `message.remove_reaction(emoji, self._client.user)`
    /// (adapter.py:3174-3183); `/@me` is the REST spelling of "the bot's own reaction", so no user id
    /// travels in the URL. Emoji encoding and validation are identical to
    /// [`add_reaction`](Self::add_reaction), and so is the success rule: only `204 No Content` is
    /// `true`. Discord answers `404` when the reaction is not there, which is a truthful `false`.
    async fn remove_reaction(&self, chat_id: &str, message_id: &str, emoji: &str) -> bool {
        let Some(url) = self.reaction_url(chat_id, message_id, emoji) else {
            return false;
        };
        reaction_succeeded(
            self.transport
                .execute(self.authorized(HttpRequest::delete(url)))
                .await,
        )
    }

    /// Upload one attachment to `POST /channels/{chat_id}/messages` as `multipart/form-data`.
    ///
    /// The body carries exactly two parts, as hermes builds them (adapter_media.py:405-412):
    /// `payload_json` with `content` (the caption, when non-blank) and `message_reference` (when
    /// `reply_to` is set, same shape as `send`), then `files[0]` with the filename, the MIME type
    /// and the raw bytes. DM channels and guild channels use this one route — Discord v10 has no
    /// separate DM upload endpoint — so the reference behaves identically in both lanes.
    ///
    /// Refusals happen before the network is touched: empty bytes and files over the cap are
    /// `bad_format` / `too_long`. [`MediaSource::Url`] is a typed refusal rather than a fake
    /// success, because hermes downloads URL media first and this transport cannot return binary
    /// bodies.
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
                        "discord media upload needs bytes: refusing MediaSource::Url({url}) because \
                         the reference downloads URL media before posting it (adapter_media.py \
                         `_send_url_media`) and this transport only returns text bodies, so a \
                         fetch-through would corrupt binary content — pass MediaSource::Bytes"
                    ),
                    Some("bad_format"),
                );
            }
        };
        if bytes.is_empty() {
            return SendResult::failed("refusing to upload empty media bytes", Some("bad_format"));
        }
        if bytes.len() > DISCORD_MAX_ATTACHMENT_BYTES {
            return SendResult::failed(
                format!(
                    "discord attachment is {} bytes, over the {} byte limit",
                    bytes.len(),
                    DISCORD_MAX_ATTACHMENT_BYTES
                ),
                Some("too_long"),
            );
        }

        let (default_name, default_mime) = default_media_attachment(media.kind);
        let filename = media
            .filename
            .as_deref()
            .map(sanitize_part_token)
            .filter(|name| !name.is_empty())
            .unwrap_or_else(|| default_name.to_string());
        let mime = media
            .mime
            .as_deref()
            .map(sanitize_part_token)
            .filter(|mime| mime.contains('/'))
            .unwrap_or_else(|| default_mime.to_string());

        let payload = media_payload(media, reply_to);
        let boundary = multipart_boundary();
        let Some(body) = encode_media_multipart(&boundary, &payload, &filename, &mime, bytes)
        else {
            return SendResult::failed(
                "discord media payload_json could not be encoded",
                Some("bad_format"),
            );
        };
        let request = self
            .authorized(HttpRequest::post(
                self.endpoint(&format!("/channels/{chat_id}/messages")),
            ))
            .header(
                "Content-Type",
                format!("multipart/form-data; boundary={boundary}"),
            )
            .body(body);

        match self.transport.execute(request).await {
            Ok(response) if response.ok() => {
                let decoded = match response.json() {
                    Ok(decoded) => decoded,
                    Err(err) => {
                        return SendResult::failed(
                            format!("discord media send response was not json: {err}"),
                            Some("unknown"),
                        );
                    }
                };
                // hermes fails loud when Discord accepted the message but attached nothing: a
                // delivery ledger must not record a silent drop as delivered.
                let attached = decoded
                    .get("attachments")
                    .and_then(Value::as_array)
                    .is_some_and(|attachments| !attachments.is_empty());
                if !attached {
                    return SendResult::failed(
                        format!("discord accepted the message but attached no files ({filename})"),
                        Some("unknown"),
                    );
                }
                match decoded.get("id").and_then(Value::as_str) {
                    Some(id) if !id.is_empty() => SendResult::ok(id),
                    _ => SendResult::failed(
                        "discord media send returned no message id",
                        Some("unknown"),
                    ),
                }
            }
            Ok(response) => classify_media_failure(&response),
            Err(err) => transport_failure(&err),
        }
    }

    /// `GET /channels/{chat_id}`: name plus a `dm` / `group` / `channel` classification.
    ///
    /// Any failure degrades to `{name: chat_id, chat_type: "channel"}` so routing still works.
    async fn get_chat_info(&self, chat_id: &str) -> ChatInfo {
        let fallback = ChatInfo {
            name: chat_id.to_string(),
            chat_type: "channel".to_string(),
        };
        let request = self.get(&format!("/channels/{chat_id}"));
        let Ok(response) = self.transport.execute(request).await else {
            return fallback;
        };
        if !response.ok() {
            return fallback;
        }
        let Ok(body) = response.json() else {
            return fallback;
        };
        let name = body
            .get("name")
            .and_then(Value::as_str)
            .filter(|name| !name.trim().is_empty())
            .or_else(|| body.get("id").and_then(Value::as_str))
            .unwrap_or(chat_id);
        let chat_type = match body.get("type").and_then(Value::as_u64) {
            Some(1) => "dm",
            Some(10..=12) => "group",
            _ => "channel",
        };
        ChatInfo {
            name: name.to_string(),
            chat_type: chat_type.to_string(),
        }
    }

    fn supports_code_blocks(&self) -> bool {
        true
    }

    /// `send` chunks with `truncate_message`, so the router must not truncate ahead of it.
    fn splits_long_messages(&self) -> bool {
        true
    }

    fn max_message_length(&self) -> usize {
        DISCORD_MAX_MESSAGE_LENGTH
    }

    /// Best-effort `POST /channels/{chat_id}/typing`; Discord shows the dots for ~10s.
    async fn send_typing(&self, chat_id: &str, _metadata: Option<Value>) {
        let request = self.authorized(HttpRequest::post(
            self.endpoint(&format!("/channels/{chat_id}/typing")),
        ));
        // A typing indicator is cosmetic: failures (429 included) are dropped, not propagated.
        let _ = self.transport.execute(request).await;
    }

    /// `PATCH /channels/{id}/messages/{id}` with the new content.
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
        let request = self.authorized(HttpRequest::patch_json(
            self.endpoint(&format!("/channels/{chat_id}/messages/{message_id}")),
            &json!({"content": content}),
        ));
        match self.transport.execute(request).await {
            Ok(response) if response.ok() => match response.json() {
                Ok(body) => {
                    let id = body.get("id").and_then(Value::as_str).unwrap_or(message_id);
                    SendResult::ok(id)
                }
                Err(_) => SendResult::ok(message_id),
            },
            Ok(response) => classify_failure(&response),
            Err(err) => transport_failure(&err),
        }
    }

    /// `DELETE /channels/{id}/messages/{id}`. The trait returns no result, so failures can only be
    /// swallowed; the request itself is real.
    async fn delete_message(&self, chat_id: &str, message_id: &str) {
        let request = self.authorized(HttpRequest::delete(
            self.endpoint(&format!("/channels/{chat_id}/messages/{message_id}")),
        ));
        let _ = self.transport.execute(request).await;
    }
}

/// Discord gateway endpoint used when the caller does not override it.
pub const DISCORD_GATEWAY_URL: &str = "wss://gateway.discord.gg/?v=10&encoding=json";
/// GUILDS | GUILD_MESSAGES | DIRECT_MESSAGES | MESSAGE_CONTENT.
pub const DISCORD_INTENTS: u64 = 1 | 512 | 4096 | 32768;

/// The gateway's inbound half: identify once, then yield `MESSAGE_CREATE` events.
///
/// The socket is a [`LineSocket`](crate::socket::LineSocket); production uses
/// [`WsSocket`](crate::socket::WsSocket). Heartbeats are driven by the read timeout, so an idle
/// gateway never blocks and never misses its interval.
pub struct DiscordGateway {
    socket: SharedLineSocket,
    token: String,
    heartbeat_ms: Mutex<u64>,
    seq: AtomicI64,
}

impl DiscordGateway {
    pub fn new(socket: SharedLineSocket, token: impl Into<String>) -> Self {
        Self {
            socket,
            token: token.into(),
            heartbeat_ms: Mutex::new(41_250),
            seq: AtomicI64::new(0),
        }
    }

    fn heartbeat_interval(&self) -> u64 {
        *self
            .heartbeat_ms
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    async fn send(&self, payload: Value) -> Result<(), String> {
        let line = serde_json::to_string(&payload).map_err(|err| err.to_string())?;
        self.socket
            .send_line(&line)
            .await
            .map_err(|err| err.to_string())
    }

    async fn heartbeat(&self) -> Result<(), String> {
        let seq = self.seq.load(Ordering::Relaxed);
        let data = if seq <= 0 { Value::Null } else { json!(seq) };
        self.send(json!({ "op": 1, "d": data })).await
    }

    /// Read Hello, send Identify, and wait for READY.
    pub async fn identify(&self) -> Result<(), String> {
        loop {
            let Some(line) = self
                .socket
                .next_line()
                .await
                .map_err(|err| err.to_string())?
            else {
                return Err("discord gateway closed during hello".into());
            };
            let Ok(value) = serde_json::from_str::<Value>(&line) else {
                continue;
            };
            if value.get("op").and_then(Value::as_i64) == Some(10) {
                if let Some(interval) = value
                    .get("d")
                    .and_then(|d| d.get("heartbeat_interval"))
                    .and_then(Value::as_u64)
                {
                    *self
                        .heartbeat_ms
                        .lock()
                        .unwrap_or_else(|poisoned| poisoned.into_inner()) = interval.max(1000);
                }
                break;
            }
        }
        self.send(json!({
            "op": 2,
            "d": {
                "token": self.token,
                "intents": DISCORD_INTENTS,
                "properties": { "os": "linux", "browser": "vikram", "device": "vikram" },
            }
        }))
        .await?;

        loop {
            let interval = self.heartbeat_interval();
            let line = match tokio::time::timeout(
                Duration::from_millis(interval),
                self.socket.next_line(),
            )
            .await
            {
                Err(_) => {
                    self.heartbeat().await?;
                    continue;
                }
                Ok(Ok(Some(line))) => line,
                Ok(Ok(None)) => return Err("discord gateway closed before READY".into()),
                Ok(Err(err)) => return Err(err.to_string()),
            };
            let Ok(value) = serde_json::from_str::<Value>(&line) else {
                continue;
            };
            if let Some(seq) = value.get("s").and_then(Value::as_i64) {
                self.seq.store(seq, Ordering::Relaxed);
            }
            match value.get("op").and_then(Value::as_i64).unwrap_or(0) {
                0 if value.get("t").and_then(Value::as_str) == Some("READY") => return Ok(()),
                1 => self.heartbeat().await?,
                9 => return Err("discord invalid session".into()),
                _ => {}
            }
        }
    }

    /// Next inbound message. Bot messages, edits and other dispatches are skipped.
    pub async fn next_event(&self) -> Result<Option<MessageEvent>, String> {
        loop {
            let interval = self.heartbeat_interval();
            let line = match tokio::time::timeout(
                Duration::from_millis(interval),
                self.socket.next_line(),
            )
            .await
            {
                Err(_) => {
                    self.heartbeat().await?;
                    continue;
                }
                Ok(Ok(Some(line))) => line,
                Ok(Ok(None)) => return Ok(None),
                Ok(Err(err)) => return Err(err.to_string()),
            };
            let Ok(value) = serde_json::from_str::<Value>(&line) else {
                continue;
            };
            if let Some(seq) = value.get("s").and_then(Value::as_i64) {
                self.seq.store(seq, Ordering::Relaxed);
            }
            match value.get("op").and_then(Value::as_i64).unwrap_or(0) {
                1 => {
                    self.heartbeat().await?;
                    continue;
                }
                7 => return Err("discord requested a reconnect".into()),
                9 => return Err("discord invalid session".into()),
                0 => {}
                _ => continue,
            }
            if value.get("t").and_then(Value::as_str) != Some("MESSAGE_CREATE") {
                continue;
            }
            if let Some(event) = parse_discord_message(&value) {
                return Ok(Some(event));
            }
        }
    }
}

fn parse_discord_message(payload: &Value) -> Option<MessageEvent> {
    let data = payload.get("d")?;
    let author = data.get("author")?;
    if author.get("bot").and_then(Value::as_bool).unwrap_or(false) {
        return None;
    }
    let content = data
        .get("content")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .trim()
        .to_string();
    let attachments = data
        .get("attachments")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    if content.is_empty() && attachments.is_empty() {
        return None;
    }
    let channel = data.get("channel_id")?.as_str()?.to_string();
    let guild = data.get("guild_id").and_then(Value::as_str);
    let platform = Platform::new("discord")?;
    let mut source = SessionSource::new(platform, channel);
    source.chat_type = if guild.is_some() { "channel" } else { "dm" }.to_string();
    source.user_id = author.get("id").and_then(Value::as_str).map(str::to_owned);
    source.user_name = author
        .get("username")
        .and_then(Value::as_str)
        .map(str::to_owned);
    source.message_id = data.get("id").and_then(Value::as_str).map(str::to_owned);

    let mut event = MessageEvent::new(content, source);
    event.user_id = event.source.user_id.clone();
    event.user_name = event.source.user_name.clone();
    event.message_id = event.source.message_id.clone();
    event.reply_to_message_id = data
        .get("referenced_message")
        .and_then(|message| message.get("id"))
        .and_then(Value::as_str)
        .map(str::to_owned);
    event.raw_message = Some(payload.clone());
    for attachment in attachments {
        if let Some(url) = attachment.get("url").and_then(Value::as_str) {
            event.media_urls.push(url.to_string());
        }
        if let Some(kind) = attachment.get("content_type").and_then(Value::as_str) {
            event.media_types.push(kind.to_string());
        }
        if let Some(name) = attachment.get("filename").and_then(Value::as_str) {
            event.media_filenames.push(name.to_string());
        }
    }
    Some(event)
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

    /// Records every request and replays queued responses (200 `{"id": "0"}` once drained).
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
                None => Ok(json_response(200, json!({"id": "0"}))),
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

    fn adapter(mock: &Arc<MockTransport>) -> DiscordAdapter {
        DiscordAdapter::new(mock.clone(), "test-token")
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

    fn response_ids(count: usize) -> Vec<HttpResponse> {
        (0..count)
            .map(|idx| json_response(200, json!({ "id": format!("m{idx}") })))
            .collect()
    }

    /// A create-message response carrying one attachment — the shape Discord returns for an
    /// upload, and the only shape `send_media` counts as delivered.
    fn media_response(id: &str) -> HttpResponse {
        json_response(
            200,
            json!({
                "id": id,
                "attachments": [{"id": "a1", "filename": "chart.png", "size": 24}],
            }),
        )
    }

    /// The boundary the adapter declared in `Content-Type`.
    fn boundary_of(request: &HttpRequest) -> String {
        header(request, "Content-Type")
            .and_then(|value| value.strip_prefix("multipart/form-data; boundary="))
            .unwrap_or_default()
            .to_string()
    }

    /// The upload body as text (file bytes included; use the byte form for binary assertions).
    fn body_text(request: &HttpRequest) -> String {
        String::from_utf8_lossy(request.body.as_deref().unwrap_or_default()).to_string()
    }

    /// The decoded `payload_json` part of a multipart upload body.
    fn payload_json_of(request: &HttpRequest) -> Value {
        let boundary = boundary_of(request);
        let body = body_text(request);
        for part in body.split(&format!("--{boundary}\r\n")) {
            if let Some(rest) = part.strip_prefix(
                "Content-Disposition: form-data; name=\"payload_json\"\r\n\
                 Content-Type: application/json\r\n\r\n",
            ) {
                return serde_json::from_str(rest.strip_suffix("\r\n").unwrap_or(rest))
                    .unwrap_or(Value::Null);
            }
        }
        Value::Null
    }

    /// A named, typed image attachment so the framing assertions stay about framing.
    fn bytes_media(bytes: &[u8]) -> OutboundMedia {
        OutboundMedia::new(MediaKind::Image, MediaSource::Bytes(bytes.to_vec()))
            .with_filename("chart.png")
            .with_mime("image/png")
    }

    #[tokio::test]
    async fn connect_probes_users_me_with_bot_auth() {
        let mock = MockTransport::with(vec![json_response(200, json!({"id": "42", "bot": true}))]);
        let mut discord = adapter(&mock);
        assert!(discord.connect(false).await);

        let requests = mock.requests();
        assert_eq!(requests.len(), 1);
        assert_eq!(requests[0].method, HttpMethod::Get);
        assert_eq!(
            requests[0].url,
            "https://discord.com/api/v10/users/@me".to_string()
        );
        assert_eq!(
            header(&requests[0], "Authorization"),
            Some("Bot test-token")
        );
    }

    #[tokio::test]
    async fn connect_fails_without_a_bot_id() {
        let mock = MockTransport::with(vec![
            json_response(401, json!({"message": "401: Unauthorized", "code": 0})),
            json_response(200, json!({"username": "bot"})),
            json_response(200, Value::Null),
        ]);
        let mut discord = adapter(&mock);
        assert!(!discord.connect(false).await);
        assert!(!discord.connect(true).await);
        assert!(!discord.connect(true).await);

        let failing = MockTransport::failing(TransportError::Request("boom".to_string()));
        let mut offline = adapter(&failing);
        assert!(!offline.connect(false).await);
    }

    #[tokio::test]
    async fn send_posts_json_with_bot_auth() {
        let mock = MockTransport::with(vec![json_response(200, json!({"id": "m1"}))]);
        let discord = adapter(&mock);
        let result = discord.send("100", "hello there", None, None).await;
        assert!(result.success);
        assert_eq!(result.message_id.as_deref(), Some("m1"));

        let requests = mock.requests();
        assert_eq!(requests.len(), 1);
        assert_eq!(requests[0].method, HttpMethod::Post);
        assert_eq!(
            requests[0].url,
            "https://discord.com/api/v10/channels/100/messages".to_string()
        );
        assert_eq!(
            header(&requests[0], "Authorization"),
            Some("Bot test-token")
        );
        assert_eq!(
            header(&requests[0], "Content-Type"),
            Some("application/json")
        );
        assert_eq!(body_json(&requests[0])["content"], "hello there");
        assert!(body_json(&requests[0]).get("message_reference").is_none());
    }

    #[tokio::test]
    async fn send_chunks_long_content_within_the_two_thousand_cap() {
        let mock = MockTransport::with(response_ids(8));
        let discord = adapter(&mock);
        let content = "word ".repeat(2_000); // 10_000 characters, needs six chunks
        let result = discord.send("100", &content, None, None).await;
        assert!(result.success, "{:?}", result.error);

        let requests = mock.requests();
        assert!(requests.len() > 1, "expected several chunks");
        for request in &requests {
            let body = body_json(request);
            let chunk = body["content"].as_str().unwrap_or_default();
            assert!(
                codepoint_len(chunk) <= DISCORD_MAX_MESSAGE_LENGTH,
                "chunk measures {}",
                codepoint_len(chunk)
            );
            assert!(!chunk.trim().is_empty());
        }
        // Every chunk is addressed to the same channel and the last id is the reported one.
        for request in &requests {
            assert!(request.url.ends_with("/channels/100/messages"));
        }
        let last = requests.len() - 1;
        assert_eq!(
            result.message_id.as_deref(),
            Some(format!("m{last}").as_str())
        );
        assert_eq!(result.continuation_message_ids.len(), requests.len());
    }

    #[tokio::test]
    async fn send_attaches_reply_reference_to_the_first_chunk() {
        let mock = MockTransport::with(response_ids(4));
        let discord = adapter(&mock);
        let content = "x".repeat(2_500);
        assert!(
            discord
                .send("100", &content, Some("99"), None)
                .await
                .success
        );

        let requests = mock.requests();
        assert!(requests.len() > 1);
        for (index, request) in requests.iter().enumerate() {
            let body = body_json(request);
            match body.get("message_reference") {
                Some(reference) => {
                    assert_eq!(index, 0, "only the first chunk anchors the reply");
                    assert_eq!(reference["message_id"], "99");
                    assert_eq!(reference["fail_if_not_exists"], false);
                }
                None => assert_ne!(index, 0),
            }
        }
    }

    #[tokio::test]
    async fn send_passes_allowed_mentions_through_verbatim() {
        let mock = MockTransport::with(vec![json_response(200, json!({"id": "m1"}))]);
        let discord = adapter(&mock);
        let metadata = json!({"allowed_mentions": {"parse": ["users"], "replied_user": false}});
        assert!(
            discord
                .send("100", "hi <@1>", None, Some(metadata.clone()))
                .await
                .success
        );
        let body = body_json(&mock.requests()[0]);
        assert_eq!(body["allowed_mentions"], metadata["allowed_mentions"]);

        // Absent metadata never grows an implied policy.
        let plain = MockTransport::with(vec![json_response(200, json!({"id": "m2"}))]);
        assert!(adapter(&plain).send("100", "hi", None, None).await.success);
        assert!(
            body_json(&plain.requests()[0])
                .get("allowed_mentions")
                .is_none()
        );
    }

    #[tokio::test]
    async fn rate_limit_reads_retry_after_from_the_body() {
        let mock = MockTransport::with(vec![json_response(
            429,
            json!({"message": "You are being rate limited.", "retry_after": 2.75, "global": false}),
        )]);
        let result = adapter(&mock).send("100", "hi", None, None).await;
        assert!(!result.success);
        assert!(result.retryable);
        assert_eq!(result.retry_after, Some(2.75));
        assert_eq!(result.error_kind.as_deref(), Some("rate_limited"));
    }

    #[tokio::test]
    async fn rate_limit_falls_back_to_the_header_delay() {
        let mock = MockTransport::with(vec![HttpResponse {
            status: 429,
            body: "{}".to_string(),
            retry_after: Some(1.5),
        }]);
        let result = adapter(&mock).send("100", "hi", None, None).await;
        assert!(result.retryable);
        assert_eq!(result.retry_after, Some(1.5));
        assert_eq!(result.error_kind.as_deref(), Some("rate_limited"));
    }

    #[tokio::test]
    async fn unauthorized_and_forbidden_map_to_forbidden() {
        for status in [401u16, 403] {
            let mock = MockTransport::with(vec![json_response(status, json!({"message": "nope"}))]);
            let result = adapter(&mock).send("100", "hi", None, None).await;
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
    async fn unknown_channel_maps_to_not_found() {
        let mock = MockTransport::with(vec![json_response(
            404,
            json!({"message": "Unknown Channel", "code": 10003}),
        )]);
        let result = adapter(&mock).send("404", "hi", None, None).await;
        assert!(!result.success);
        assert_eq!(result.error_kind.as_deref(), Some("not_found"));
        assert!(!result.retryable);
    }

    #[tokio::test]
    async fn validation_error_maps_to_bad_format_but_server_errors_are_transient() {
        let mock = MockTransport::with(vec![json_response(
            400,
            json!({"message": "Invalid Form Body", "code": 50035}),
        )]);
        let invalid = adapter(&mock).send("100", "hi", None, None).await;
        assert_eq!(invalid.error_kind.as_deref(), Some("bad_format"));
        assert!(!invalid.retryable);

        let server = MockTransport::with(vec![json_response(
            503,
            json!({"message": "Service Unavailable"}),
        )]);
        let transient = adapter(&server).send("100", "hi", None, None).await;
        assert_eq!(transient.error_kind.as_deref(), Some("transient"));
        assert!(transient.retryable);

        let offline = MockTransport::failing(TransportError::Request("connection reset".into()));
        let unreachable = adapter(&offline).send("100", "hi", None, None).await;
        assert_eq!(unreachable.error_kind.as_deref(), Some("transient"));
        assert!(unreachable.retryable);
    }

    #[tokio::test]
    async fn empty_content_is_rejected_without_a_request() {
        let mock = MockTransport::with(vec![json_response(200, json!({"id": "m1"}))]);
        let discord = adapter(&mock);
        for content in ["", "   ", "\n\t \r\n"] {
            let result = discord.send("100", content, None, None).await;
            assert!(!result.success, "content {content:?}");
            assert_eq!(result.error_kind.as_deref(), Some("bad_format"));
        }
        assert_eq!(mock.request_count(), 0);
    }

    #[tokio::test]
    async fn partial_delivery_keeps_the_chunks_that_landed() {
        let mut responses = response_ids(2);
        responses.push(json_response(500, json!({"message": "boom"})));
        let mock = MockTransport::with(responses);
        let result = adapter(&mock)
            .send("100", &"y".repeat(5_000), None, None)
            .await;
        assert!(result.success);
        assert_eq!(result.message_id.as_deref(), Some("m1"));
        assert_eq!(result.continuation_message_ids, vec!["m0", "m1"]);
        let (delivered, total) = result.partial_overflow().unwrap_or_default();
        assert_eq!(delivered, 2);
        assert!(total > 2);
    }

    #[tokio::test]
    async fn media_upload_frames_payload_json_and_the_file_part_exactly() {
        let mock = MockTransport::with(vec![media_response("m42")]);
        let bytes: Vec<u8> = b"\x89PNG\r\n\x1a\n\x00binary-tail".to_vec();
        let media = bytes_media(&bytes).with_caption("daily chart");

        let result = adapter(&mock).send_media("100", &media, None).await;
        assert!(result.success, "{:?}", result.error);
        assert_eq!(result.message_id.as_deref(), Some("m42"));

        let requests = mock.requests();
        assert_eq!(requests.len(), 1);
        assert_eq!(requests[0].method, HttpMethod::Post);
        assert_eq!(
            requests[0].url,
            "https://discord.com/api/v10/channels/100/messages".to_string()
        );
        assert_eq!(
            header(&requests[0], "Authorization"),
            Some("Bot test-token")
        );
        assert_eq!(header(&requests[0], "User-Agent"), Some(USER_AGENT));

        // The two-part body, byte for byte: payload_json first, then files[0] with the raw bytes.
        let boundary = boundary_of(&requests[0]);
        assert!(!boundary.is_empty(), "no boundary in the content type");
        let mut expected = format!(
            "--{boundary}\r\n\
             Content-Disposition: form-data; name=\"payload_json\"\r\n\
             Content-Type: application/json\r\n\
             \r\n\
             {{\"content\":\"daily chart\"}}\r\n\
             --{boundary}\r\n\
             Content-Disposition: form-data; name=\"files[0]\"; filename=\"chart.png\"\r\n\
             Content-Type: image/png\r\n\
             \r\n"
        )
        .into_bytes();
        expected.extend_from_slice(&bytes);
        expected.extend_from_slice(format!("\r\n--{boundary}--\r\n").as_bytes());
        assert_eq!(requests[0].body.as_deref(), Some(expected.as_slice()));
    }

    #[tokio::test]
    async fn media_upload_omits_content_when_the_caption_is_absent() {
        let mock = MockTransport::with(vec![media_response("m1"), media_response("m2")]);
        let discord = adapter(&mock);
        assert!(
            discord
                .send_media("100", &bytes_media(b"png"), None)
                .await
                .success
        );
        assert!(
            discord
                .send_media("100", &bytes_media(b"png").with_caption("   \n "), None)
                .await
                .success
        );

        for request in mock.requests() {
            let payload = payload_json_of(&request);
            assert_eq!(payload, json!({}), "blank captions are not sent as content");
            assert!(!body_text(&request).contains("\"content\""));
        }
    }

    #[tokio::test]
    async fn media_upload_carries_the_caption_as_content() {
        let mock = MockTransport::with(vec![media_response("m1")]);
        let media = bytes_media(b"png").with_caption("  quarterly numbers  ");
        assert!(adapter(&mock).send_media("100", &media, None).await.success);
        let payload = payload_json_of(&mock.requests()[0]);
        assert_eq!(payload["content"], "quarterly numbers");
        assert!(payload.get("message_reference").is_none());
    }

    #[tokio::test]
    async fn media_upload_replies_with_the_same_reference_shape_as_send() {
        let mock = MockTransport::with(vec![media_response("m1")]);
        let media = bytes_media(b"png").with_caption("chart");
        assert!(
            adapter(&mock)
                .send_media("100", &media, Some("99"))
                .await
                .success
        );
        let payload = payload_json_of(&mock.requests()[0]);
        assert_eq!(payload["message_reference"]["message_id"], "99");
        assert_eq!(payload["message_reference"]["fail_if_not_exists"], false);
        assert_eq!(payload["content"], "chart");
    }

    #[tokio::test]
    async fn media_uploads_address_dms_and_guild_channels_through_one_route() {
        // Discord v10 has no DM-specific upload endpoint: a DM channel id and a guild channel id
        // both resolve to `POST /channels/{id}/messages`, and the reply anchor gains no `guild_id`.
        let mut urls: Vec<String> = Vec::new();
        let mut payloads: Vec<Value> = Vec::new();
        for chat_id in ["dm-77", "1234567890123456789"] {
            let mock = MockTransport::with(vec![media_response("m1")]);
            let media = bytes_media(b"png").with_caption("hi");
            assert!(
                adapter(&mock)
                    .send_media(chat_id, &media, Some("99"))
                    .await
                    .success
            );
            let requests = mock.requests();
            urls.push(requests[0].url.clone());
            payloads.push(payload_json_of(&requests[0]));
        }
        assert_eq!(
            urls[0],
            "https://discord.com/api/v10/channels/dm-77/messages"
        );
        assert_eq!(
            urls[1],
            "https://discord.com/api/v10/channels/1234567890123456789/messages"
        );
        assert_eq!(payloads[0], payloads[1]);
        assert!(payloads[0]["message_reference"].get("guild_id").is_none());
    }

    #[tokio::test]
    async fn media_part_headers_sanitize_the_caller_filename_and_mime() {
        let mock = MockTransport::with(vec![media_response("m1")]);
        let media = OutboundMedia::new(
            MediaKind::Document,
            MediaSource::Bytes(b"%PDF-1.7".to_vec()),
        )
        .with_filename("inject\"\r\nContent-Disposition: form-data; name=\"files[9]\"\r\n\r\nx.txt")
        .with_mime("text/plain\r\nX-Evil: 1");
        assert!(adapter(&mock).send_media("100", &media, None).await.success);

        let body = body_text(&mock.requests()[0]);
        // Only the two real part headers start a line; the injected text lands inside the quoted
        // filename parameter, with its CR/LF and quotes stripped.
        assert_eq!(body.matches("\r\nContent-Disposition").count(), 2);
        assert!(
            !body.contains("\r\nX-Evil"),
            "mime must not inject a header"
        );
        assert!(
            body.contains("filename=\"injectContent-Disposition: form-data; name=files[9]x.txt\""),
            "sanitized filename missing:\n{body}"
        );
        assert!(body.contains("Content-Type: text/plainX-Evil: 1\r\n\r\n"));
    }

    #[tokio::test]
    async fn media_defaults_filename_and_mime_from_the_media_kind() {
        let cases = [
            (MediaKind::Image, "image.png", "image/png"),
            (MediaKind::Video, "video.mp4", "video/mp4"),
            (MediaKind::Audio, "audio.mp3", "audio/mpeg"),
            (
                MediaKind::Document,
                "attachment.bin",
                "application/octet-stream",
            ),
        ];
        for (kind, filename, mime) in cases {
            let mock = MockTransport::with(vec![media_response("m1")]);
            let media = OutboundMedia::new(kind, MediaSource::Bytes(b"x".to_vec()));
            assert!(adapter(&mock).send_media("100", &media, None).await.success);
            let body = body_text(&mock.requests()[0]);
            assert!(
                body.contains(&format!("filename=\"{filename}\"")),
                "{kind:?} default filename missing:\n{body}"
            );
            assert!(
                body.contains(&format!("Content-Type: {mime}\r\n\r\n")),
                "{kind:?} default mime missing:\n{body}"
            );
        }
    }

    #[tokio::test]
    async fn empty_media_bytes_are_rejected_without_a_request() {
        let mock = MockTransport::with(vec![media_response("m1")]);
        let media = OutboundMedia::new(MediaKind::Image, MediaSource::Bytes(Vec::new()));
        let result = adapter(&mock).send_media("100", &media, None).await;
        assert!(!result.success);
        assert_eq!(result.error_kind.as_deref(), Some("bad_format"));
        assert!(!result.retryable);
        assert_eq!(mock.request_count(), 0, "nothing should hit the wire");
    }

    #[tokio::test]
    async fn media_over_the_twenty_five_mib_cap_is_rejected_before_upload() {
        let mock = MockTransport::with(vec![media_response("m1")]);
        let media = OutboundMedia::new(
            MediaKind::Document,
            MediaSource::Bytes(vec![0u8; DISCORD_MAX_ATTACHMENT_BYTES + 1]),
        );
        let result = adapter(&mock).send_media("100", &media, None).await;
        assert!(!result.success);
        assert_eq!(result.error_kind.as_deref(), Some("too_long"));
        assert!(!result.retryable);
        assert_eq!(mock.request_count(), 0);

        let message = result.error.as_deref().unwrap_or_default();
        assert!(
            message.contains(&(DISCORD_MAX_ATTACHMENT_BYTES + 1).to_string()),
            "{message}"
        );
        assert!(
            message.contains(&DISCORD_MAX_ATTACHMENT_BYTES.to_string()),
            "{message}"
        );
    }

    #[tokio::test]
    async fn media_rate_limit_reads_retry_after_from_the_body() {
        let mock = MockTransport::with(vec![json_response(
            429,
            json!({"message": "You are being rate limited.", "retry_after": 4.25, "global": false}),
        )]);
        let result = adapter(&mock)
            .send_media("100", &bytes_media(b"png"), None)
            .await;
        assert!(!result.success);
        assert_eq!(result.error_kind.as_deref(), Some("rate_limited"));
        assert!(result.retryable);
        assert_eq!(result.retry_after, Some(4.25));
    }

    #[tokio::test]
    async fn media_refusals_map_to_the_standard_kinds() {
        let cases = [
            (
                403u16,
                json!({"message": "Missing Access", "code": 50001}),
                "forbidden",
                false,
            ),
            (
                404,
                json!({"message": "Unknown Channel", "code": 10003}),
                "not_found",
                false,
            ),
            (
                400,
                json!({"message": "Invalid Form Body"}),
                "bad_format",
                false,
            ),
            (
                413,
                json!({"message": "Request entity too large"}),
                "too_long",
                false,
            ),
            (500, json!({"message": "boom"}), "transient", true),
            (
                503,
                json!({"message": "Service Unavailable"}),
                "transient",
                true,
            ),
        ];
        for (status, body, kind, retryable) in cases {
            let mock = MockTransport::with(vec![json_response(status, body)]);
            let result = adapter(&mock)
                .send_media("100", &bytes_media(b"png"), None)
                .await;
            assert!(!result.success, "{status}");
            assert_eq!(result.error_kind.as_deref(), Some(kind), "{status}");
            assert_eq!(result.retryable, retryable, "{status}");
            assert!(
                result
                    .error
                    .as_deref()
                    .unwrap_or_default()
                    .contains(&status.to_string()),
                "{status}: status should surface in the message"
            );
        }
    }

    #[tokio::test]
    async fn media_transport_errors_are_transient() {
        let offline = MockTransport::failing(TransportError::Request("connection reset".into()));
        let result = adapter(&offline)
            .send_media("100", &bytes_media(b"png"), None)
            .await;
        assert!(!result.success);
        assert_eq!(result.error_kind.as_deref(), Some("transient"));
        assert!(result.retryable);
        assert_eq!(offline.request_count(), 1);
    }

    #[tokio::test]
    async fn a_message_without_attachments_is_never_reported_as_delivered() {
        let empty = MockTransport::with(vec![json_response(200, json!({"id": "m1"}))]);
        let result = adapter(&empty)
            .send_media("100", &bytes_media(b"png"), None)
            .await;
        assert!(!result.success);
        assert_eq!(result.error_kind.as_deref(), Some("unknown"));
        assert!(
            result
                .error
                .as_deref()
                .unwrap_or_default()
                .contains("attached no files")
        );

        let unnamed = MockTransport::with(vec![json_response(
            200,
            json!({"attachments": [{"id": "a1"}]}),
        )]);
        let no_id = adapter(&unnamed)
            .send_media("100", &bytes_media(b"png"), None)
            .await;
        assert!(!no_id.success);
        assert_eq!(no_id.error_kind.as_deref(), Some("unknown"));
        assert!(
            no_id
                .error
                .as_deref()
                .unwrap_or_default()
                .contains("no message id")
        );
    }

    #[tokio::test]
    async fn url_media_is_refused_truthfully() {
        let mock = MockTransport::with(Vec::new());
        let discord = adapter(&mock);
        assert!(discord.supports_media());

        let media = OutboundMedia::image_url("https://cdn.example.test/a.png").with_caption("look");
        let result = discord.send_media("100", &media, None).await;
        assert!(!result.success, "a URL source is not a delivery");
        assert_eq!(result.error_kind.as_deref(), Some("bad_format"));
        assert_eq!(mock.request_count(), 0, "no upload may be attempted");

        let message = result.error.as_deref().unwrap_or_default();
        assert!(message.contains("MediaSource::Bytes"), "{message}");
        assert!(
            message.contains("https://cdn.example.test/a.png"),
            "{message}"
        );
        assert!(message.contains("needs bytes"), "{message}");
    }

    #[tokio::test]
    async fn an_overlong_caption_is_capped_to_the_message_budget() {
        let mock = MockTransport::with(vec![media_response("m1")]);
        let caption = "y".repeat(4_000);
        let media = bytes_media(b"png").with_caption(&caption);
        assert!(adapter(&mock).send_media("100", &media, None).await.success);
        let payload = payload_json_of(&mock.requests()[0]);
        let sent = payload["content"].as_str().unwrap_or_default();
        assert!(!sent.is_empty());
        assert!(codepoint_len(sent) <= DISCORD_MAX_MESSAGE_LENGTH);
    }

    #[tokio::test]
    async fn chat_info_reports_guild_channel_metadata() {
        let mock = MockTransport::with(vec![json_response(
            200,
            json!({"id": "55", "name": "general", "type": 0, "guild_id": "9"}),
        )]);
        let info = adapter(&mock).get_chat_info("55").await;
        assert_eq!(
            info,
            ChatInfo {
                name: "general".to_string(),
                chat_type: "channel".to_string(),
            }
        );
        let requests = mock.requests();
        assert_eq!(requests[0].method, HttpMethod::Get);
        assert_eq!(
            requests[0].url,
            "https://discord.com/api/v10/channels/55".to_string()
        );
        assert_eq!(
            header(&requests[0], "Authorization"),
            Some("Bot test-token")
        );
    }

    #[tokio::test]
    async fn chat_info_falls_back_to_the_id_for_dms_and_groups_threads() {
        let mock = MockTransport::with(vec![json_response(200, json!({"id": "77", "type": 1}))]);
        let dm = adapter(&mock).get_chat_info("77").await;
        assert_eq!(
            dm,
            ChatInfo {
                name: "77".to_string(),
                chat_type: "dm".to_string(),
            }
        );

        for channel_type in [10u64, 11, 12] {
            let group = MockTransport::with(vec![json_response(
                200,
                json!({"id": "88", "name": "planning", "type": channel_type}),
            )]);
            let info = adapter(&group).get_chat_info("88").await;
            assert_eq!(info.name, "planning");
            assert_eq!(info.chat_type, "group", "type {channel_type}");
        }
    }

    #[tokio::test]
    async fn chat_info_degrades_to_the_channel_fallback() {
        let missing = MockTransport::with(vec![json_response(
            404,
            json!({"message": "Unknown Channel", "code": 10003}),
        )]);
        assert_eq!(
            adapter(&missing).get_chat_info("404").await,
            ChatInfo {
                name: "404".to_string(),
                chat_type: "channel".to_string(),
            }
        );

        let broken = MockTransport::with(vec![HttpResponse {
            status: 200,
            body: "not json".to_string(),
            retry_after: None,
        }]);
        assert_eq!(
            adapter(&broken).get_chat_info("12").await.chat_type,
            "channel"
        );
    }

    #[tokio::test]
    async fn typing_hits_the_typing_endpoint() {
        let mock = MockTransport::with(vec![HttpResponse {
            status: 204,
            body: String::new(),
            retry_after: None,
        }]);
        adapter(&mock).send_typing("100", None).await;
        let requests = mock.requests();
        assert_eq!(requests.len(), 1);
        assert_eq!(requests[0].method, HttpMethod::Post);
        assert_eq!(
            requests[0].url,
            "https://discord.com/api/v10/channels/100/typing".to_string()
        );
        assert_eq!(
            header(&requests[0], "Authorization"),
            Some("Bot test-token")
        );
    }

    #[tokio::test]
    async fn typing_failures_are_swallowed() {
        let offline = MockTransport::failing(TransportError::Request("down".into()));
        adapter(&offline).send_typing("100", None).await;
        assert_eq!(offline.request_count(), 1);
    }

    #[tokio::test]
    async fn edit_message_uses_patch_and_reports_failures() {
        let mock = MockTransport::with(vec![
            json_response(200, json!({"id": "m1", "content": "new text"})),
            json_response(
                403,
                json!({"message": "Missing Permissions", "code": 50013}),
            ),
        ]);
        let adapter = adapter(&mock);

        let result = adapter.edit_message("100", "m1", "new text", false).await;
        assert!(result.success);
        assert_eq!(result.message_id.as_deref(), Some("m1"));
        {
            let requests = mock.requests();
            assert_eq!(requests.len(), 1);
            assert_eq!(requests[0].method, HttpMethod::Patch);
            assert!(requests[0].url.ends_with("/channels/100/messages/m1"));
        }

        let refused = adapter.edit_message("100", "m1", "newer", false).await;
        assert!(!refused.success);
        assert_eq!(refused.error_kind.as_deref(), Some("forbidden"));
    }

    #[tokio::test]
    async fn delete_message_issues_a_delete() {
        let mock = MockTransport::with(vec![json_response(204, json!({}))]);
        adapter(&mock).delete_message("100", "m1").await;
        let requests = mock.requests();
        assert_eq!(requests.len(), 1);
        assert_eq!(requests[0].method, HttpMethod::Delete);
        assert!(requests[0].url.ends_with("/channels/100/messages/m1"));
    }

    // ----- rich cards (embeds) -----------------------------------------------------------------

    #[tokio::test]
    async fn card_posts_one_embed_with_a_derived_content_mirror() {
        let mock = MockTransport::with(vec![json_response(
            200,
            json!({"id": "m7", "channel_id": "100", "embeds": [{"title": "Daily report"}]}),
        )]);
        let card = json!({
            "title": "Daily report",
            "description": "Close 42.00, up 1.2%",
            "color": 3_447_003,
        });

        let result = adapter(&mock).send_card("100", &card).await;
        assert!(result.success, "{:?}", result.error);
        assert_eq!(result.message_id.as_deref(), Some("m7"));
        assert_eq!(
            result.raw_response.as_ref().and_then(|raw| raw.get("id")),
            Some(&json!("m7"))
        );

        let requests = mock.requests();
        assert_eq!(requests.len(), 1);
        assert_eq!(requests[0].method, HttpMethod::Post);
        assert_eq!(
            requests[0].url,
            "https://discord.com/api/v10/channels/100/messages".to_string()
        );
        assert_eq!(
            header(&requests[0], "Authorization"),
            Some("Bot test-token")
        );
        assert_eq!(header(&requests[0], "User-Agent"), Some(USER_AGENT));
        assert_eq!(
            header(&requests[0], "Content-Type"),
            Some("application/json")
        );

        let body = body_json(&requests[0]);
        // The embed travels verbatim, wrapped in the `embeds` array discord.py builds from `embed=`.
        assert_eq!(body["embeds"], json!([card.clone()]));
        // The plain mirror the reference always carries, derived from title + description.
        assert_eq!(body["content"], "Daily report\n\nClose 42.00, up 1.2%");
    }

    #[tokio::test]
    async fn card_honours_the_caller_content_fallback() {
        let mock = MockTransport::with(vec![json_response(200, json!({"id": "m1"}))]);
        let card = json!({
            "embeds": [{"title": "Approval needed", "description": "rm -rf /tmp/x"}],
            "content": "  **Approval needed**\n\nrm -rf /tmp/x  ",
        });
        assert!(adapter(&mock).send_card("100", &card).await.success);

        let body = body_json(&mock.requests()[0]);
        assert_eq!(body["content"], "**Approval needed**\n\nrm -rf /tmp/x");
        assert_eq!(
            body["embeds"],
            json!([{"title": "Approval needed", "description": "rm -rf /tmp/x"}])
        );
        // The wrapper keys never leak into the embed itself.
        assert!(body["embeds"][0].get("content").is_none());
    }

    #[tokio::test]
    async fn card_posts_a_multi_embed_array_in_order() {
        let mock = MockTransport::with(vec![json_response(200, json!({"id": "m2"}))]);
        let card = json!({
            "embeds": [
                {"title": "First", "description": "one"},
                {"title": "Second", "description": "two"},
                {"description": "third only"},
            ],
        });
        assert!(adapter(&mock).send_card("100", &card).await.success);

        let body = body_json(&mock.requests()[0]);
        assert_eq!(body["embeds"].as_array().map(Vec::len), Some(3));
        assert_eq!(body["embeds"][0]["title"], "First");
        assert_eq!(body["embeds"][2]["description"], "third only");
        assert_eq!(
            body["content"],
            "First\n\none\n\nSecond\n\ntwo\n\nthird only"
        );
    }

    #[tokio::test]
    async fn card_without_mirrorable_text_posts_embeds_only() {
        let mock = MockTransport::with(vec![json_response(200, json!({"id": "m3"}))]);
        let card = json!({"image": {"url": "https://cdn.example.test/chart.png"}});
        assert!(adapter(&mock).send_card("100", &card).await.success);

        let body = body_json(&mock.requests()[0]);
        assert_eq!(
            body["embeds"][0]["image"]["url"],
            "https://cdn.example.test/chart.png"
        );
        // Nothing to mirror, so no `content` key at all — never an invented placeholder.
        assert!(body.get("content").is_none(), "{body}");
    }

    #[tokio::test]
    async fn card_refuses_invalid_payloads_without_a_request() {
        let mock = MockTransport::with(Vec::new());
        let discord = adapter(&mock);
        let over_cap = json!({
            "embeds": (0..DISCORD_MAX_EMBEDS + 1)
                .map(|idx| json!({"title": format!("embed {idx}")}))
                .collect::<Vec<Value>>(),
        });
        let cases = vec![
            ("non-object", json!("just a string")),
            ("null", Value::Null),
            ("array of embeds at the top", json!([{"title": "t"}])),
            ("empty object", json!({})),
            ("blank field only", json!({"title": "   \n "})),
            ("blank nested object", json!({"author": {"name": ""}})),
            ("null field only", json!({"image": Value::Null})),
            ("empty embeds array", json!({"embeds": []})),
            ("embeds not an array", json!({"embeds": {"title": "t"}})),
            ("wrapper without embeds", json!({"content": "text"})),
            (
                "content not a string",
                json!({"embeds": [{"title": "t"}], "content": 42}),
            ),
            (
                "non-object embed",
                json!({"embeds": [{"title": "t"}, "nope"]}),
            ),
            ("empty embed in the array", json!({"embeds": [{}]})),
            ("over the embed cap", over_cap),
        ];
        for (label, card) in cases {
            let result = discord.send_card("100", &card).await;
            assert!(!result.success, "{label}: {card}");
            assert_eq!(result.error_kind.as_deref(), Some("bad_format"), "{label}");
            assert!(!result.retryable, "{label}");
            assert!(result.message_id.is_none(), "{label}");
        }

        // A blank channel id cannot address a message either.
        let blank = discord.send_card("   ", &json!({"title": "t"})).await;
        assert!(!blank.success);
        assert_eq!(blank.error_kind.as_deref(), Some("bad_format"));

        assert_eq!(mock.request_count(), 0, "nothing may hit the wire");
    }

    #[tokio::test]
    async fn card_success_requires_the_message_id() {
        let empty = MockTransport::with(vec![json_response(200, json!({"embeds": [{}]}))]);
        let missing = adapter(&empty)
            .send_card("100", &json!({"title": "t"}))
            .await;
        assert!(!missing.success);
        assert_eq!(missing.error_kind.as_deref(), Some("unknown"));
        assert!(
            missing
                .error
                .as_deref()
                .unwrap_or_default()
                .contains("no message id")
        );

        let blank_id = MockTransport::with(vec![json_response(200, json!({"id": ""}))]);
        let unnamed = adapter(&blank_id)
            .send_card("100", &json!({"title": "t"}))
            .await;
        assert!(!unnamed.success);
        assert_eq!(unnamed.error_kind.as_deref(), Some("unknown"));

        // A 2xx whose body is not JSON cannot name the message either.
        let garbage = MockTransport::with(vec![HttpResponse {
            status: 200,
            body: "not json".to_string(),
            retry_after: None,
        }]);
        let undecodable = adapter(&garbage)
            .send_card("100", &json!({"title": "t"}))
            .await;
        assert!(!undecodable.success);
        assert_eq!(undecodable.error_kind.as_deref(), Some("unknown"));
    }

    #[tokio::test]
    async fn card_failures_use_the_shared_taxonomy() {
        let cases = [
            (
                403u16,
                json!({"message": "Missing Access", "code": 50001}),
                "forbidden",
                false,
            ),
            (
                404,
                json!({"message": "Unknown Channel", "code": 10003}),
                "not_found",
                false,
            ),
            (
                400,
                json!({"message": "Invalid Form Body", "code": 50035}),
                "bad_format",
                false,
            ),
            // A bare 400 is still a malformed card, never a retry.
            (400, json!({"message": "Bad Request"}), "bad_format", false),
            (
                413,
                json!({"message": "Payload too large"}),
                "too_long",
                false,
            ),
            (500, json!({"message": "boom"}), "transient", true),
            (
                503,
                json!({"message": "Service Unavailable"}),
                "transient",
                true,
            ),
        ];
        for (status, body, kind, retryable) in cases {
            let mock = MockTransport::with(vec![json_response(status, body)]);
            let result = adapter(&mock)
                .send_card("100", &json!({"title": "t"}))
                .await;
            assert!(!result.success, "{status}");
            assert_eq!(result.error_kind.as_deref(), Some(kind), "{status}");
            assert_eq!(result.retryable, retryable, "{status}");
            assert!(
                result
                    .error
                    .as_deref()
                    .unwrap_or_default()
                    .contains(&status.to_string()),
                "{status}: status should surface in the message"
            );
        }
    }

    #[tokio::test]
    async fn card_rate_limit_carries_the_server_delay() {
        let mock = MockTransport::with(vec![json_response(
            429,
            json!({"message": "You are being rate limited.", "retry_after": 3.5, "global": false}),
        )]);
        let result = adapter(&mock)
            .send_card("100", &json!({"title": "t"}))
            .await;
        assert!(!result.success);
        assert_eq!(result.error_kind.as_deref(), Some("rate_limited"));
        assert!(result.retryable);
        assert_eq!(result.retry_after, Some(3.5));

        // Without a body delay the header's answer is used, as on the text path.
        let header_only = MockTransport::with(vec![HttpResponse {
            status: 429,
            body: "{}".to_string(),
            retry_after: Some(0.75),
        }]);
        let by_header = adapter(&header_only)
            .send_card("100", &json!({"title": "t"}))
            .await;
        assert_eq!(by_header.error_kind.as_deref(), Some("rate_limited"));
        assert_eq!(by_header.retry_after, Some(0.75));
    }

    #[tokio::test]
    async fn card_transport_errors_are_transient() {
        let offline = MockTransport::failing(TransportError::Request("connection reset".into()));
        let result = adapter(&offline)
            .send_card("100", &json!({"title": "t"}))
            .await;
        assert!(!result.success);
        assert_eq!(result.error_kind.as_deref(), Some("transient"));
        assert!(result.retryable);
        assert_eq!(offline.request_count(), 1);
    }

    #[tokio::test]
    async fn cards_address_dms_and_guild_channels_through_one_route() {
        // Like text and uploads: a DM channel id and a guild channel id share the create-message
        // route, and neither the route nor the body grows a `guild_id`.
        let mut urls: Vec<String> = Vec::new();
        let mut bodies: Vec<Value> = Vec::new();
        for chat_id in ["dm-77", "1234567890123456789"] {
            let mock = MockTransport::with(vec![json_response(200, json!({"id": "m1"}))]);
            let card = json!({"title": "hi", "description": "there"});
            assert!(adapter(&mock).send_card(chat_id, &card).await.success);
            let requests = mock.requests();
            urls.push(requests[0].url.clone());
            bodies.push(body_json(&requests[0]));
        }
        assert_eq!(
            urls[0],
            "https://discord.com/api/v10/channels/dm-77/messages"
        );
        assert_eq!(
            urls[1],
            "https://discord.com/api/v10/channels/1234567890123456789/messages"
        );
        assert_eq!(bodies[0], bodies[1]);
        assert!(bodies[0].get("guild_id").is_none());
    }

    #[tokio::test]
    async fn a_long_derived_mirror_is_capped_to_the_message_budget() {
        let mock = MockTransport::with(vec![json_response(200, json!({"id": "m1"}))]);
        let card = json!({"title": "Report", "description": "y".repeat(6_000)});
        assert!(adapter(&mock).send_card("100", &card).await.success);

        let body = body_json(&mock.requests()[0]);
        let mirror = body["content"].as_str().unwrap_or_default();
        assert!(!mirror.is_empty());
        assert!(
            codepoint_len(mirror) <= DISCORD_MAX_MESSAGE_LENGTH,
            "mirror measures {}",
            codepoint_len(mirror)
        );
        assert!(mirror.starts_with("Report"), "{mirror}");
    }

    #[test]
    fn card_payload_shape_rules_are_local() {
        // One embed object needs one populated field; the wrapper needs an `embeds` array.
        assert!(card_payload(&json!({"title": "t"})).is_ok());
        assert!(card_payload(&json!({"embeds": [{"title": "t"}]})).is_ok());
        assert!(card_payload(&json!({"embeds": [{"title": "t"}], "content": null})).is_ok());

        let single = card_payload(&json!({"title": " t ", "description": "d"}))
            .map(|payload| (payload.embeds.len(), payload.content));
        assert_eq!(single, Ok((1, "t\n\nd".to_string())));

        // A blank caller fallback is absent, so the mirror is derived.
        let blank = card_payload(&json!({"embeds": [{"title": "t"}], "content": "  "}))
            .map(|payload| payload.content);
        assert_eq!(blank, Ok("t".to_string()));

        assert!(card_payload(&json!({"embeds": [], "content": "x"})).is_err());
        assert!(card_payload(&json!([])).is_err());
        assert!(card_payload(&json!(true)).is_err());
        assert!(!is_present_field(&Value::Null));
        assert!(!is_present_field(&json!("  ")));
        assert!(!is_present_field(&json!([])));
        assert!(!is_present_field(&json!({})));
        assert!(is_present_field(&json!(0)));
        assert!(is_present_field(&json!(false)));
        assert!(is_present_field(&json!("x")));
    }

    // ----- reactions ---------------------------------------------------------------------------

    /// Discord's documented reaction answer: `204 No Content` with an empty body.
    fn no_content() -> HttpResponse {
        HttpResponse {
            status: DISCORD_REACTION_SUCCESS_STATUS,
            body: String::new(),
            retry_after: None,
        }
    }

    #[tokio::test]
    async fn add_reaction_puts_the_reaction_route_with_bot_auth() {
        let mock = MockTransport::with(vec![no_content()]);
        assert!(adapter(&mock).add_reaction("100", "m1", "👍").await);

        let requests = mock.requests();
        assert_eq!(requests.len(), 1);
        assert_eq!(requests[0].method, HttpMethod::Put);
        assert_eq!(
            requests[0].url,
            "https://discord.com/api/v10/channels/100/messages/m1/reactions/%F0%9F%91%8D/@me"
        );
        assert_eq!(
            header(&requests[0], "Authorization"),
            Some("Bot test-token")
        );
        assert_eq!(header(&requests[0], "User-Agent"), Some(USER_AGENT));
        // A reaction is a route, not a payload: no body is sent.
        assert!(requests[0].body.is_none(), "{:?}", requests[0].body);
    }

    #[tokio::test]
    async fn remove_reaction_deletes_the_same_route_for_self() {
        let mock = MockTransport::with(vec![no_content()]);
        assert!(adapter(&mock).remove_reaction("100", "m1", "👀").await);

        let requests = mock.requests();
        assert_eq!(requests.len(), 1);
        assert_eq!(requests[0].method, HttpMethod::Delete);
        // `/@me` is the bot's own reaction, so no user id appears in the route.
        assert_eq!(
            requests[0].url,
            "https://discord.com/api/v10/channels/100/messages/m1/reactions/%F0%9F%91%80/@me"
        );
        assert_eq!(
            header(&requests[0], "Authorization"),
            Some("Bot test-token")
        );
        assert!(requests[0].body.is_none());
    }

    #[tokio::test]
    async fn blank_reaction_inputs_never_reach_the_wire() {
        let mock = MockTransport::with(Vec::new());
        let discord = adapter(&mock);
        let cases = [
            ("100", "m1", ""),
            ("100", "m1", "   "),
            ("100", "m1", "\n\t"),
            ("", "m1", "👍"),
            ("   ", "m1", "👍"),
            ("100", "", "👍"),
            ("100", "  ", "👍"),
        ];
        for (chat_id, message_id, emoji) in cases {
            assert!(
                !discord.add_reaction(chat_id, message_id, emoji).await,
                "add {chat_id:?}/{message_id:?}/{emoji:?}"
            );
            assert!(
                !discord.remove_reaction(chat_id, message_id, emoji).await,
                "remove {chat_id:?}/{message_id:?}/{emoji:?}"
            );
        }
        assert_eq!(mock.request_count(), 0, "nothing may hit the wire");
    }

    #[tokio::test]
    async fn unicode_emoji_is_percent_encoded_into_the_path_segment() {
        // Multi-byte UTF-8: one `%XX` per byte, exactly what urllib.parse.quote produces.
        let cases = [
            ("👍", "%F0%9F%91%8D"),
            ("👀", "%F0%9F%91%80"),
            ("✅", "%E2%9C%85"),
            ("❤️", "%E2%9D%A4%EF%B8%8F"),
        ];
        for (emoji, encoded) in cases {
            let mock = MockTransport::with(vec![no_content()]);
            assert!(
                adapter(&mock).add_reaction("100", "m1", emoji).await,
                "{emoji}"
            );
            let url = mock.requests()[0].url.clone();
            assert!(
                url.ends_with(&format!("/reactions/{encoded}/@me")),
                "{emoji}: {url}"
            );
            // The raw emoji never lands in the URL unencoded.
            assert!(!url.contains(emoji), "{emoji}: {url}");
        }
    }

    #[tokio::test]
    async fn custom_emoji_is_sent_as_name_colon_id() {
        // discord.py's PartialEmoji._as_reaction(): `<:name:id>` / `<a:name:id>` → `name:id`, and
        // the colon stays literal because Discord documents `name:id` as the `{emoji}` value.
        let cases = [
            ("<:blob:123456789012345678>", "blob:123456789012345678"),
            ("<a:party:987654321098765432>", "party:987654321098765432"),
            ("blob:123456789012345678", "blob:123456789012345678"),
        ];
        for (emoji, segment) in cases {
            let mock = MockTransport::with(vec![no_content()]);
            assert!(
                adapter(&mock).add_reaction("100", "m1", emoji).await,
                "{emoji}"
            );
            let url = mock.requests()[0].url.clone();
            assert!(
                url.ends_with(&format!("/reactions/{segment}/@me")),
                "{emoji}: {url}"
            );
        }

        // A name that merely starts with `a` is not animated; the marker is only dropped as `a:`.
        let mock = MockTransport::with(vec![no_content()]);
        assert!(
            adapter(&mock)
                .add_reaction("100", "m1", "<:art:111111111111111111>")
                .await
        );
        assert!(
            mock.requests()[0]
                .url
                .ends_with("/reactions/art:111111111111111111/@me"),
            "{}",
            mock.requests()[0].url
        );

        // An id that is not a snowflake is not a custom emoji: it goes out verbatim.
        let mock = MockTransport::with(vec![no_content()]);
        assert!(
            adapter(&mock)
                .add_reaction("100", "m1", "name:not-an-id")
                .await
        );
        assert!(
            mock.requests()[0]
                .url
                .ends_with("/reactions/name%3Anot-an-id/@me"),
            "{}",
            mock.requests()[0].url
        );
    }

    #[tokio::test]
    async fn reaction_success_requires_the_documented_204() {
        // A 2xx that is not the documented answer is not a confirmation.
        let ok200 = MockTransport::with(vec![json_response(200, json!({}))]);
        assert!(!adapter(&ok200).add_reaction("100", "m1", "👍").await);
        assert!(!adapter(&ok200).remove_reaction("100", "m1", "👍").await);
        assert_eq!(ok200.request_count(), 2);
    }

    #[tokio::test]
    async fn reaction_failures_are_false() {
        let cases = [
            (
                400u16,
                json!({"message": "Invalid Form Body", "code": 50035}),
            ),
            (
                403,
                json!({"message": "Missing Permissions", "code": 50013}),
            ),
            (404, json!({"message": "Unknown Message", "code": 10008})),
            (
                429,
                json!({"message": "You are being rate limited.", "retry_after": 1.5}),
            ),
            (500, json!({"message": "boom"})),
        ];
        for (status, body) in cases {
            let mock = MockTransport::with(vec![
                json_response(status, body.clone()),
                json_response(status, body),
            ]);
            let discord = adapter(&mock);
            assert!(
                !discord.add_reaction("100", "m1", "👍").await,
                "add status {status}"
            );
            assert!(
                !discord.remove_reaction("100", "m1", "👍").await,
                "remove status {status}"
            );
        }

        // A reaction that never reached Discord is not a reaction.
        let offline = MockTransport::failing(TransportError::Request("connection reset".into()));
        let discord = adapter(&offline);
        assert!(!discord.add_reaction("100", "m1", "👍").await);
        assert!(!discord.remove_reaction("100", "m1", "👍").await);
        assert_eq!(offline.request_count(), 2, "each call really attempted");
    }

    #[test]
    fn reaction_encoding_helpers_shape_the_route_segment() {
        assert_eq!(encode_path_segment("👍", ""), "%F0%9F%91%8D");
        assert_eq!(encode_path_segment("a-b_c.d~e", ""), "a-b_c.d~e");
        assert_eq!(encode_path_segment("a/b:c", ""), "a%2Fb%3Ac");
        assert_eq!(encode_path_segment("a/b:c", ":"), "a%2Fb:c");
        assert_eq!(
            custom_emoji_reference("<:blob:123456789012345678>").as_deref(),
            Some("blob:123456789012345678")
        );
        assert_eq!(
            custom_emoji_reference("<a:party:123456789012345678>").as_deref(),
            Some("party:123456789012345678")
        );
        assert_eq!(custom_emoji_reference("👍"), None);
        assert_eq!(custom_emoji_reference("name:id"), None, "id must be digits");
        assert_eq!(reaction_path_segment("  "), None);
        assert_eq!(
            reaction_path_segment(" 👍 ").as_deref(),
            Some("%F0%9F%91%8D")
        );
    }

    #[tokio::test]
    async fn capability_flags_match_hermes() {
        let mock = MockTransport::with(Vec::new());
        let mut discord = adapter(&mock);
        assert!(discord.splits_long_messages());
        assert!(discord.supports_code_blocks());
        assert!(discord.supports_media());
        assert!(discord.supports_reactions());
        assert!(discord.supports_cards());
        assert_eq!(discord.max_message_length(), DISCORD_MAX_MESSAGE_LENGTH);
        assert_eq!(discord.message_length("héllo"), 5);
        assert_eq!(discord.typed_command_prefix(), "/");
        discord.disconnect().await;
    }
}

#[cfg(test)]
mod gateway_tests {
    use super::*;
    use crate::socket::{LineSocket, SocketError};
    use async_trait::async_trait;
    use std::collections::VecDeque;
    use std::sync::{Arc, Mutex as StdMutex};

    struct MockSocket {
        sent: StdMutex<Vec<String>>,
        incoming: StdMutex<VecDeque<String>>,
    }

    impl MockSocket {
        fn new(incoming: Vec<String>) -> Arc<Self> {
            Arc::new(Self {
                sent: StdMutex::new(Vec::new()),
                incoming: StdMutex::new(incoming.into()),
            })
        }
        fn sent(&self) -> Vec<Value> {
            self.sent
                .lock()
                .unwrap()
                .iter()
                .map(|line| serde_json::from_str(line).unwrap())
                .collect()
        }
    }

    #[async_trait]
    impl LineSocket for MockSocket {
        async fn send_line(&self, line: &str) -> Result<(), SocketError> {
            self.sent.lock().unwrap().push(line.to_string());
            Ok(())
        }
        async fn next_line(&self) -> Result<Option<String>, SocketError> {
            Ok(self.incoming.lock().unwrap().pop_front())
        }
    }

    fn gateway(incoming: Vec<Value>) -> (DiscordGateway, Arc<MockSocket>) {
        let lines = incoming.into_iter().map(|v| v.to_string()).collect();
        let socket = MockSocket::new(lines);
        (
            DiscordGateway::new(Arc::clone(&socket) as SharedLineSocket, "bot-token"),
            socket,
        )
    }

    #[tokio::test]
    async fn identify_reads_hello_and_sends_the_identify_frame() {
        let (gateway, socket) = gateway(vec![
            json!({"op": 10, "d": {"heartbeat_interval": 45000}}),
            json!({"op": 0, "t": "READY", "s": 1}),
        ]);
        gateway.identify().await.unwrap();
        let sent = socket.sent();
        assert_eq!(sent[0]["op"], 2);
        assert_eq!(sent[0]["d"]["token"], "bot-token");
        assert_eq!(sent[0]["d"]["intents"], DISCORD_INTENTS);
        assert_eq!(gateway.heartbeat_interval(), 45_000);
    }

    #[tokio::test]
    async fn message_create_becomes_an_event_and_bots_are_skipped() {
        let (gateway, _) = gateway(vec![
            json!({"op": 0, "t": "MESSAGE_CREATE", "s": 5, "d": {
                "id": "m1", "channel_id": "C1", "guild_id": "G1", "content": "hello discord",
                "author": {"id": "U1", "username": "Asha"},
                "referenced_message": {"id": "m0"},
                "attachments": [{"url": "https://cdn/x.png", "content_type": "image/png", "filename": "x.png"}]
            }}),
            json!({"op": 0, "t": "MESSAGE_CREATE", "s": 6, "d": {
                "id": "m2", "channel_id": "C1", "content": "beep",
                "author": {"id": "B1", "username": "bot", "bot": true}
            }}),
            json!({"op": 0, "t": "MESSAGE_CREATE", "s": 7, "d": {
                "id": "m3", "channel_id": "D1", "content": "dm text",
                "author": {"id": "U2", "username": "Bob"}
            }}),
        ]);
        let first = gateway.next_event().await.unwrap().expect("event");
        assert_eq!(first.text, "hello discord");
        assert_eq!(first.source.chat_type, "channel");
        assert_eq!(first.user_id.as_deref(), Some("U1"));
        assert_eq!(first.reply_to_message_id.as_deref(), Some("m0"));
        assert_eq!(first.media_types, vec!["image/png"]);

        let second = gateway.next_event().await.unwrap().expect("dm");
        assert_eq!(second.source.chat_type, "dm");
        assert_eq!(second.text, "dm text");
        assert_eq!(gateway.seq.load(Ordering::Relaxed), 7);
    }

    #[tokio::test]
    async fn a_heartbeat_request_is_answered_with_the_sequence() {
        let (gateway, socket) = gateway(vec![json!({"op": 1})]);
        // Seed the sequence so the heartbeat carries it.
        gateway.seq.store(9, Ordering::Relaxed);
        {
            let mut incoming = socket.incoming.lock().unwrap();
            incoming.push_back(
                json!({"op": 0, "t": "MESSAGE_CREATE", "s": 10, "d": {
                    "id": "m2", "channel_id": "C", "content": "after",
                    "author": {"id": "U", "username": "u"}
                }})
                .to_string(),
            );
        }
        let event = gateway.next_event().await.unwrap().expect("event");
        assert_eq!(event.text, "after");
        let sent = socket.sent();
        assert!(sent.iter().any(|value| value["op"] == 1 && value["d"] == 9));
    }
}
