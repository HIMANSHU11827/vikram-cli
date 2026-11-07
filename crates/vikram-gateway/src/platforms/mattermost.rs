//! Mattermost adapter — the REST half of `gateway/platforms/mattermost` (API v4).
//!
//! Everything here rides on the frozen [`Transport`](crate::transport::Transport): build an
//! [`HttpRequest`], hand it over, classify the response. hermes drives the same endpoints through
//! `aiohttp`; this port talks to the documented REST routes directly, with `Authorization: Bearer`
//! on every call.
//!
//! Ported: the identity probe (`GET /users/me`), post creation with the 4000-character budget
//! (`truncate_message`), thread replies via `root_id`, post editing (`PUT /posts/{post_id}`),
//! channel lookup (`GET /channels/{id}`) with the D/G/O/P type codes, the best-effort typing
//! indicator (`POST /users/me/typing`), REST polling of a channel's posts
//! (`GET /channels/{id}/posts?since=`) and outbound media — one `multipart/form-data` upload
//! (`POST /files`) followed by a post that carries the returned `file_ids`.
//!
//! Fidelity notes (full accounting in `docs/gateway-port-plan.md`):
//! - The transport exposes GET/POST only, so `edit_message` must report the missing PUT verb
//!   instead of pretending the edit happened, and `delete_message` (a unit-returning trait
//!   method) is a documented no-op rather than a fake success — the same honest stance Discord's
//!   adapter takes.
//! - hermes receives inbound messages over a WebSocket (`/api/v4/websocket`, auth challenge,
//!   reconnect backoff, mention gating, dedup, attachment download). None of that is REST, so it
//!   is out of scope here; [`MattermostAdapter::poll_once`] is the REST substitute the runner can
//!   call instead.
//! - `root_id` is attached to *every* chunk of a split reply, matching hermes' per-chunk
//!   `_post_message` call, so the whole answer lands in one thread.
//! - `format_message` (hermes reduces `![alt](url)` to the bare URL so Mattermost inline-previews
//!   it) is not ported: it needs a URL regex this crate has no dependency for, and it changes
//!   bytes the caller wrote. Content is sent verbatim.
//! - hermes disables mentions (`props.disable_mentions`) and retries a broken `root_id` flat for
//!   notify-worthy posts; both are config-driven policies this adapter does not own, so no
//!   `props` are invented and no silent retry happens.
//! - Outbound media ([`PlatformAdapter::send_media`]) follows the reference's upload-then-post
//!   flow (`research/hermes-platforms/mattermost/adapter.py:195-229`): `POST /api/v4/files` with
//!   the `channel_id` and `files` multipart fields, `file_infos[].id` read off the answer, then
//!   `POST /api/v4/posts` with `{"channel_id", "message", "file_ids"}` plus `root_id` for a reply.
//!   Not ported, and refused rather than faked: the URL path (`_send_url_as_file`,
//!   `adapter.py:328-357`) downloads a URL and uploads the bytes — this transport returns text
//!   bodies only, so a fetch-through would corrupt binary content; the multi-image batch
//!   (`send_multiple_images`, `adapter.py:399-434`) and inbound attachment download/cache
//!   (`_download_attachments`, `adapter.py:518-549`) need a receive-side pipeline this REST
//!   adapter does not own. `_resolve_root_id` (`adapter.py:266-271`) is not ported either: it
//!   costs one `GET /posts/{id}` per reply, and `reply_to` is sent as the thread root verbatim,
//!   exactly as `send` already does.
//!
//! A mid-payload failure keeps the chunks that already landed and reports `partial_overflow`,
//! mirroring hermes' overflow-split contract, so the delivery ledger resends only the tail.

use async_trait::async_trait;
use serde_json::{Map, Value, json};
use std::sync::atomic::{AtomicU64, Ordering};

use crate::event::{MessageEvent, Platform, SessionSource, platform_names};
use crate::helpers::{codepoint_len, truncate_message};
use crate::platform::{
    ChatInfo, MediaKind, MediaSource, OutboundMedia, PlatformAdapter, SendResult,
};
use crate::transport::{HttpRequest, HttpResponse, SharedTransport, TransportError};

/// Path prefix every Mattermost REST call hangs off.
pub const MATTERMOST_API_PREFIX: &str = "/api/v4";

/// Budget for one post, measured in characters.
///
/// The server default is 16383, but 4000 is the practical limit for a readable message (hermes
/// `MAX_POST_LENGTH`), and `send` chunks against it.
pub const MAX_MESSAGE_LENGTH: usize = 4000;

/// Posts requested per [`MattermostAdapter::poll_once`] page (the REST maximum useful page).
const POLL_PAGE_SIZE: usize = 60;

/// Cap for one uploaded file, in bytes (50 MiB).
///
/// The reference enforces no client-side cap of its own: `_upload_file` posts whatever bytes it is
/// handed and turns any `>= 400` into a failed upload
/// (`research/hermes-platforms/mattermost/adapter.py:215-229`), and `_standalone_send` does the
/// same (`adapter.py:628-634`), leaving the bound to the server. Mattermost's server-side default
/// is `FileSettings.MaxFileSize = 52428800` (50 MiB), so this port refuses at that same bound
/// before the bytes cross the wire, and still maps a server `413` onto `too_long` for deployments
/// that configure a smaller limit.
pub const MAX_UPLOAD_BYTES: usize = 50 * 1024 * 1024;

/// Longest filename kept from a caller value, so one absurd name cannot bloat a part header.
const MAX_FILENAME_CHARS: usize = 200;

/// Characters of an error body kept in a [`SendResult::error`] message.
const BODY_EXCERPT_CHARS: usize = 200;

/// Mattermost platform adapter speaking REST v4 with a bot or personal-access token.
///
/// Every method is stateless: `connect` only proves the token resolves to a user, so a failed
/// `connect` can be retried with the same instance.
pub struct MattermostAdapter {
    transport: SharedTransport,
    base_url: String,
    token: String,
}

impl MattermostAdapter {
    /// Build an adapter over `transport` for the server at `base_url`.
    ///
    /// A trailing `/` on `base_url` is stripped so endpoints never double up; the token is used
    /// verbatim in an `Authorization: Bearer <token>` header. Nothing is sent anywhere until a
    /// method is called.
    pub fn new(
        transport: SharedTransport,
        base_url: impl Into<String>,
        token: impl Into<String>,
    ) -> Self {
        Self {
            transport,
            base_url: base_url.into().trim_end_matches('/').to_string(),
            token: token.into(),
        }
    }

    /// Absolute URL for an API path such as `/posts`.
    fn endpoint(&self, path: &str) -> String {
        format!("{}{MATTERMOST_API_PREFIX}{path}", self.base_url)
    }

    /// Add the credentials Mattermost needs on every REST call.
    fn authorized(&self, request: HttpRequest) -> HttpRequest {
        request.bearer(&self.token)
    }

    /// Authorized GET for `path`.
    fn get(&self, path: &str) -> HttpRequest {
        self.authorized(HttpRequest::get(self.endpoint(path)))
    }

    /// Authorized JSON POST for `path`.
    fn post_json(&self, path: &str, body: &Value) -> HttpRequest {
        self.authorized(HttpRequest::json(self.endpoint(path), body))
    }

    /// Create one post and return the decoded post object.
    ///
    /// Errors carry the classified [`SendResult`] (boxed: the success path stays small) so the
    /// caller can report the exact refusal.
    async fn post_message(&self, body: &Value) -> Result<Value, Box<SendResult>> {
        self.post_with(body, classify_failure).await
    }

    /// Create one post and decode the answer, classifying a non-2xx response with `classify`.
    ///
    /// Text sends pass [`classify_failure`]; media posts pass [`classify_media_failure`] so a
    /// server-side size refusal is reported as `too_long` instead of `bad_format`. Both paths hit
    /// the same route (`POST /api/v4/posts`, `research/hermes-platforms/mattermost/adapter.py:640`)
    /// and both treat a non-JSON body as an unusable answer rather than a success.
    async fn post_with(
        &self,
        body: &Value,
        classify: fn(&HttpResponse) -> SendResult,
    ) -> Result<Value, Box<SendResult>> {
        let request = self.post_json("/posts", body);
        match self.transport.execute(request).await {
            Ok(response) if response.ok() => response.json().map_err(|err| {
                Box::new(SendResult::failed(
                    format!("mattermost post response was not json: {err}"),
                    Some("unknown"),
                ))
            }),
            Ok(response) => Err(Box::new(classify(&response))),
            Err(err) => Err(Box::new(transport_failure(&err))),
        }
    }

    /// Upload one file to `POST /api/v4/files` and return the server-assigned id.
    ///
    /// The body is `multipart/form-data` (RFC 7578, CRLF framing): a `channel_id` field — "required
    /// so the server can attribute the upload", `adapter.py:625` — then the `files` field with the
    /// filename, the content type and the raw bytes (`adapter.py:219-222`,
    /// `adapter.py:624-627`). The id is read from `file_infos[].id`, which is where the reference
    /// looks (`adapter.py:228-229` takes the first entry; this port takes the first entry that
    /// carries a non-blank id — the same thing for a one-file upload, and honest about a malformed
    /// answer).
    ///
    /// Any failure — transport, non-2xx, unparseable body, no file id — comes back as the
    /// classified [`SendResult`], so the caller stops before posting an unattached message.
    async fn upload_file(
        &self,
        channel_id: &str,
        filename: &str,
        mime: &str,
        bytes: &[u8],
    ) -> Result<String, Box<SendResult>> {
        let boundary = multipart_boundary();
        let request = self
            .authorized(HttpRequest::post(self.endpoint("/files")))
            .header(
                "Content-Type",
                format!("multipart/form-data; boundary={boundary}"),
            )
            .body(encode_upload_multipart(
                &boundary, channel_id, filename, mime, bytes,
            ));

        let response = match self.transport.execute(request).await {
            Ok(response) => response,
            Err(err) => return Err(Box::new(transport_failure(&err))),
        };
        if !response.ok() {
            return Err(Box::new(classify_media_failure(&response)));
        }
        let decoded = response.json().map_err(|err| {
            Box::new(SendResult::failed(
                format!("mattermost file upload response was not json: {err}"),
                Some("unknown"),
            ))
        })?;
        decoded
            .get("file_infos")
            .and_then(Value::as_array)
            .and_then(|infos| {
                infos.iter().find_map(|info| {
                    info.get("id")
                        .and_then(Value::as_str)
                        .filter(|id| !id.trim().is_empty())
                })
            })
            .map(str::to_string)
            .ok_or_else(|| {
                Box::new(SendResult::failed(
                    "mattermost file upload returned no file id",
                    Some("unknown"),
                ))
            })
    }

    /// Fetch the posts of `channel_id` created at or after `since_ms`.
    ///
    /// `GET /channels/{id}/posts?since=<ms>&per_page=60` returns a `posts` map keyed by post id
    /// plus an `order` array. The map is authoritative here and the result is sorted by
    /// `create_at`, because a map has no order of its own.
    ///
    /// Each post becomes a [`MessageEvent`]: `source.chat_id` is the channel, `message_id` the
    /// post id, `platform_update_id` its `create_at` (so `/restart` can advance past it),
    /// `reply_to_message_id` its `root_id` when it is a thread reply, and `raw_message` the
    /// untouched post object. Posts without an `id` cannot be routed and are skipped.
    pub async fn poll_once(
        &self,
        channel_id: &str,
        since_ms: u64,
    ) -> Result<Vec<MessageEvent>, String> {
        let path =
            format!("/channels/{channel_id}/posts?since={since_ms}&per_page={POLL_PAGE_SIZE}");
        let request = self.get(&path);
        let response = self
            .transport
            .execute(request)
            .await
            .map_err(|err| format!("mattermost posts transport error: {err}"))?;
        let decoded = response.json().ok();
        if !response.ok() {
            return Err(format!(
                "mattermost api returned {}: {}",
                response.status,
                error_detail(&response, decoded.as_ref())
            ));
        }
        let body = decoded.ok_or_else(|| "mattermost posts response was not json".to_string())?;
        let Some(posts) = body.get("posts").and_then(Value::as_object) else {
            return Ok(Vec::new());
        };
        let mut events: Vec<(i64, MessageEvent)> = posts
            .values()
            .filter_map(|post| post_to_event(post, channel_id))
            .collect();
        events.sort_by_key(|(create_at, _)| *create_at);
        Ok(events.into_iter().map(|(_, event)| event).collect())
    }
}

/// The gateway platform name as a [`Platform`] value.
///
/// `platform_names::MATTERMOST` is a non-empty lowercase name, so `Platform::new` cannot reject
/// it; the fallback keeps the function total instead of unwrapping.
fn mattermost_platform() -> Platform {
    Platform::new(platform_names::MATTERMOST).unwrap_or_else(Platform::local)
}

/// Turn one REST post object into an event plus its `create_at` sort key.
fn post_to_event(post: &Value, channel_id: &str) -> Option<(i64, MessageEvent)> {
    let id = post
        .get("id")
        .and_then(Value::as_str)
        .filter(|id| !id.trim().is_empty())?;
    let text = post
        .get("message")
        .and_then(Value::as_str)
        .unwrap_or_default();
    let user_id = post
        .get("user_id")
        .and_then(Value::as_str)
        .filter(|user| !user.trim().is_empty());
    let root_id = post
        .get("root_id")
        .and_then(Value::as_str)
        .filter(|root| !root.trim().is_empty());
    let create_at = post.get("create_at").and_then(Value::as_i64).unwrap_or(0);

    let mut source = SessionSource::new(mattermost_platform(), channel_id.to_string());
    source.chat_type = "channel".to_string();
    source.user_id = user_id.map(str::to_string);
    source.message_id = Some(id.to_string());

    let mut event = MessageEvent::new(text, source);
    event.user_id = user_id.map(str::to_string);
    event.message_id = Some(id.to_string());
    event.reply_to_message_id = root_id.map(str::to_string);
    event.platform_update_id = Some(create_at);
    event.raw_message = Some(post.clone());
    Some((create_at, event))
}

/// Mattermost's channel type code (`D`/`G`/`O`/`P`) in [`ChatInfo`] vocabulary.
///
/// hermes folds `P` (private) into `"group"`; this port follows the [`ChatInfo`] contract and
/// keeps `O` and `P` both `"channel"`, since both are channel-shaped for routing.
fn chat_type_for(code: Option<&str>) -> &'static str {
    match code {
        Some("D") => "dm",
        Some("G") => "group",
        Some("O" | "P") => "channel",
        _ => "channel",
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

/// Mattermost's machine-readable error id from a decoded body
/// (`{"id": "api.context.429", "message": "...", "status_code": 429}`).
fn error_id(decoded: Option<&Value>) -> Option<&str> {
    decoded?.get("id")?.as_str()
}

/// The most informative detail an error response offers: its `message` field, else the body.
fn error_detail(response: &HttpResponse, decoded: Option<&Value>) -> String {
    let message = decoded
        .and_then(|body| body.get("message"))
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|message| !message.is_empty());
    match message {
        Some(message) => message.to_string(),
        None => body_excerpt(&response.body),
    }
}

/// Map a non-2xx Mattermost response onto a classified [`SendResult`].
///
/// 429 — or any status whose error `id` contains `rate_limit` — → `rate_limited` (retryable, with
/// the server's `Retry-After`), 401/403 → `forbidden`, 404 → `not_found`, any other 4xx →
/// `bad_format`, 5xx (and anything else non-2xx) → `transient`, retryable.
fn classify_failure(response: &HttpResponse) -> SendResult {
    let decoded = response.json().ok();
    let message = format!(
        "mattermost api returned {}: {}",
        response.status,
        error_detail(response, decoded.as_ref())
    );
    let rate_limited = response.status == 429
        || error_id(decoded.as_ref()).is_some_and(|id| id.contains("rate_limit"));
    if rate_limited {
        let retry_after = decoded
            .as_ref()
            .and_then(|body| body.get("retry_after"))
            .and_then(Value::as_f64)
            .or(response.retry_after);
        return SendResult::failed(message, Some("rate_limited")).retryable(retry_after);
    }
    match response.status {
        401 | 403 => SendResult::failed(message, Some("forbidden")),
        404 => SendResult::failed(message, Some("not_found")),
        400..=499 => SendResult::failed(message, Some("bad_format")),
        _ => SendResult::failed(message, Some("transient")).retryable(None),
    }
}

/// Classify a transport-level failure: the request never produced a response, so the send is
/// retryable (`transient`).
fn transport_failure(err: &TransportError) -> SendResult {
    SendResult::failed(
        format!("mattermost transport error: {err}"),
        Some("transient"),
    )
    .retryable(None)
}

/// Map a failed upload or media-post response onto a classified [`SendResult`].
///
/// Shares [`classify_failure`] with `send` and adds the one status only the attachment path meets:
/// `413` is the server refusing the body size, which is `too_long` (never retryable) rather than
/// the generic 4xx `bad_format` — the reference treats any `>= 400` upload answer as a hard
/// failure and stops (`adapter.py:224-227`), it never retries.
fn classify_media_failure(response: &HttpResponse) -> SendResult {
    if response.status == 413 {
        let decoded = response.json().ok();
        return SendResult::failed(
            format!(
                "mattermost api returned 413: {}",
                error_detail(response, decoded.as_ref())
            ),
            Some("too_long"),
        );
    }
    classify_failure(response)
}

/// Filename and content type Mattermost gets for a media kind the caller did not label.
///
/// The reference guesses from the filename (`mimetypes.guess_type`, `adapter.py:368-369`) and
/// falls back to `"{kind}.png"` for URL media (`adapter.py:356`); without a filename the only
/// workable default is one per kind.
fn default_media_file(kind: MediaKind) -> (&'static str, &'static str) {
    match kind {
        MediaKind::Image => ("image.png", "image/png"),
        MediaKind::Video => ("video.mp4", "video/mp4"),
        MediaKind::Audio => ("audio.mp3", "audio/mpeg"),
        MediaKind::Document => ("attachment.bin", "application/octet-stream"),
    }
}

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

/// Encode the `channel_id` + `files` body `POST /api/v4/files` expects, CRLF-framed per RFC 7578.
///
/// Field order follows the reference's `FormData`: `channel_id` first, then `files` with the
/// filename and content type attached to the part
/// (`research/hermes-platforms/mattermost/adapter.py:219-222`; the standalone sender builds the
/// same body at `adapter.py:624-627`). Nothing here can fail — every part is bytes — so the caller
/// never has to invent a fallback body.
fn encode_upload_multipart(
    boundary: &str,
    channel_id: &str,
    filename: &str,
    mime: &str,
    bytes: &[u8],
) -> Vec<u8> {
    let mut body = Vec::with_capacity(bytes.len() + filename.len() + channel_id.len() + 512);
    body.extend_from_slice(format!("--{boundary}\r\n").as_bytes());
    body.extend_from_slice(b"Content-Disposition: form-data; name=\"channel_id\"\r\n\r\n");
    body.extend_from_slice(channel_id.as_bytes());
    body.extend_from_slice(b"\r\n");
    body.extend_from_slice(format!("--{boundary}\r\n").as_bytes());
    body.extend_from_slice(
        format!("Content-Disposition: form-data; name=\"files\"; filename=\"{filename}\"\r\n")
            .as_bytes(),
    );
    body.extend_from_slice(format!("Content-Type: {mime}\r\n\r\n").as_bytes());
    body.extend_from_slice(bytes);
    body.extend_from_slice(b"\r\n");
    body.extend_from_slice(format!("--{boundary}--\r\n").as_bytes());
    body
}

#[async_trait]
impl PlatformAdapter for MattermostAdapter {
    /// Probe `GET /users/me`; true when Mattermost answers 200 with a non-empty user id.
    async fn connect(&mut self, _is_reconnect: bool) -> bool {
        let request = self.get("/users/me");
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

    /// POST the content to `/posts`, chunked to the 4000-character budget.
    ///
    /// `reply_to` becomes `root_id` on every chunk (hermes threads each chunk of a split reply),
    /// `metadata` is unused — Mattermost has no mention policy this adapter is allowed to invent.
    /// Chunks go out sequentially; a failure after some chunks landed reports success with
    /// `partial_overflow` and the ids already delivered so the ledger resends only the tail.
    async fn send(
        &self,
        chat_id: &str,
        content: &str,
        reply_to: Option<&str>,
        _metadata: Option<Value>,
    ) -> SendResult {
        if content.trim().is_empty() {
            // Caller bug, not a platform problem: refuse before touching the network.
            return SendResult::failed("Refusing to send empty message", Some("bad_format"));
        }
        let chunks = truncate_message(content, MAX_MESSAGE_LENGTH, codepoint_len);
        let total = chunks.len();
        if total == 0 {
            return SendResult::failed("Refusing to send empty message", Some("bad_format"));
        }
        let root_id = reply_to.filter(|root| !root.trim().is_empty());
        let mut ids: Vec<String> = Vec::new();

        for chunk in &chunks {
            let mut body: Map<String, Value> = Map::new();
            body.insert("channel_id".to_string(), Value::String(chat_id.to_string()));
            body.insert("message".to_string(), Value::String(chunk.clone()));
            if let Some(root) = root_id {
                body.insert("root_id".to_string(), Value::String(root.to_string()));
            }

            match self.post_message(&Value::Object(body)).await {
                Ok(post) => match post.get("id").and_then(Value::as_str) {
                    Some(id) if !id.is_empty() => ids.push(id.to_string()),
                    // Delivered but unidentified: report it, never resend blindly.
                    _ => {
                        return SendResult::failed(
                            "mattermost send returned no post id",
                            Some("unknown"),
                        );
                    }
                },
                Err(failure) => {
                    if let Some(last) = ids.last().cloned() {
                        // Keep the head that is already on screen; the ledger resends the tail.
                        let delivered = ids.len() as u32;
                        return SendResult::ok_split(last, ids)
                            .with_partial_overflow(delivered, total as u32);
                    }
                    return *failure;
                }
            }
        }

        match ids.last().cloned() {
            Some(last) => SendResult::ok_split(last, ids),
            None => SendResult::failed("mattermost send produced no posts", Some("bad_format")),
        }
    }

    /// `GET /channels/{chat_id}`: `display_name` (else `name`) plus a `dm` / `group` / `channel`
    /// classification.
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
            .get("display_name")
            .and_then(Value::as_str)
            .or_else(|| body.get("name").and_then(Value::as_str))
            .map(str::trim)
            .filter(|name| !name.is_empty())
            .unwrap_or(chat_id);
        let chat_type = chat_type_for(body.get("type").and_then(Value::as_str));
        ChatInfo {
            name: name.to_string(),
            chat_type: chat_type.to_string(),
        }
    }

    /// Mattermost renders standard Markdown, including fenced code blocks.
    fn supports_code_blocks(&self) -> bool {
        true
    }

    /// Uploads are implemented: one attachment per post, up to [`MAX_UPLOAD_BYTES`].
    fn supports_media(&self) -> bool {
        true
    }

    /// Upload one attachment (`POST /api/v4/files`) then attach it to a new post
    /// (`POST /api/v4/posts`).
    ///
    /// The wire flow is the reference's, step for step:
    /// 1. `POST /api/v4/files` as `multipart/form-data` carrying the `channel_id` field and the
    ///    `files` field (filename, content type, raw bytes) —
    ///    `research/hermes-platforms/mattermost/adapter.py:215-229`, and the standalone sender at
    ///    `adapter.py:624-634`;
    /// 2. read the uploaded file's id from `file_infos[].id` (`adapter.py:228-229`);
    /// 3. `POST /api/v4/posts` with `{"channel_id", "message", "file_ids": [...]}` —
    ///    `_post_message(..., file_ids)` at `adapter.py:195-200` building the payload and
    ///    `_post_with_file` at `adapter.py:210-213` posting it (`adapter.py:640`).
    ///
    /// The caption becomes `message` with no rewriting: the reference passes `caption or ""`
    /// (`adapter.py:212`), so the field is always present and an absent caption sends `""`.
    /// `reply_to` becomes `root_id`, which is where the reference threads its reply
    /// (`adapter.py:202-207`); a blank `reply_to` is the flat-channel case and adds no key, exactly
    /// like `send`. hermes first resolves a reply id to its true thread root
    /// (`_resolve_root_id`, `adapter.py:266-271`); that extra `GET /posts/{id}` is not spent here,
    /// matching `send`'s existing behaviour — see the module fidelity notes.
    ///
    /// Refusals happen before the network is touched: empty bytes are `bad_format`, bytes over
    /// [`MAX_UPLOAD_BYTES`] are `too_long`. A failed upload stops at the upload — the post is
    /// never attempted, so nothing half-attached is ever created — and a failed post is surfaced
    /// with its own classification. Only a post `id` counts as delivered, mirroring `_post_result`
    /// (`adapter.py:65-68`) and `_POST_WITH_FILE_ERROR` (`adapter.py:47`).
    ///
    /// [`MediaSource::Url`] is a typed refusal, not a silent downgrade: the reference downloads
    /// the URL and uploads the bytes (`_send_url_as_file`, `adapter.py:328-357`) and only posts
    /// the bare URL as a *text fallback* when that download or upload fails
    /// (`adapter.py:333-334`). This transport surfaces text bodies only
    /// ([`ReqwestTransport`](crate::transport::ReqwestTransport) reads `response.text()`), so a
    /// fetch-through would corrupt binary content; callers pass [`MediaSource::Bytes`], or call
    /// [`send`](PlatformAdapter::send) themselves for the link fallback.
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
                        "mattermost media upload needs bytes: refusing MediaSource::Url({url}) \
                         because hermes downloads URL media and uploads the bytes \
                         (adapter.py:328-357 `_send_url_as_file`), and this transport only returns \
                         text bodies, so a fetch-through would corrupt binary content — pass \
                         MediaSource::Bytes, or send the URL as text for the reference's fallback"
                    ),
                    Some("bad_format"),
                );
            }
        };
        if bytes.is_empty() {
            return SendResult::failed("refusing to upload empty media bytes", Some("bad_format"));
        }
        if bytes.len() > MAX_UPLOAD_BYTES {
            return SendResult::failed(
                format!(
                    "mattermost upload is {} bytes, over the {} byte limit",
                    bytes.len(),
                    MAX_UPLOAD_BYTES
                ),
                Some("too_long"),
            );
        }

        let (default_name, default_mime) = default_media_file(media.kind);
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

        // Step 1 + 2: the upload is a hard prerequisite. Its failure ends the send here.
        let file_id = match self.upload_file(chat_id, &filename, &mime, bytes).await {
            Ok(file_id) => file_id,
            Err(failure) => return *failure,
        };

        // Step 3: the post that carries the attachment.
        let mut body: Map<String, Value> = Map::new();
        body.insert("channel_id".to_string(), Value::String(chat_id.to_string()));
        body.insert(
            "message".to_string(),
            Value::String(media.caption.as_deref().unwrap_or_default().to_string()),
        );
        body.insert("file_ids".to_string(), json!([file_id]));
        if let Some(root) = reply_to.filter(|root| !root.trim().is_empty()) {
            body.insert("root_id".to_string(), Value::String(root.to_string()));
        }

        match self
            .post_with(&Value::Object(body), classify_media_failure)
            .await
        {
            Ok(post) => match post
                .get("id")
                .and_then(Value::as_str)
                .filter(|id| !id.is_empty())
            {
                Some(id) => SendResult::ok(id),
                None => {
                    SendResult::failed("mattermost media post returned no post id", Some("unknown"))
                }
            },
            Err(failure) => *failure,
        }
    }

    /// `send` chunks with `truncate_message`, so the router must not truncate ahead of it.
    fn splits_long_messages(&self) -> bool {
        true
    }

    fn max_message_length(&self) -> usize {
        MAX_MESSAGE_LENGTH
    }

    /// Best-effort `POST /users/me/typing`; Mattermost clears the dots after a few seconds.
    async fn send_typing(&self, chat_id: &str, _metadata: Option<Value>) {
        let body = json!({ "channel_id": chat_id });
        let request = self.post_json("/users/me/typing", &body);
        // A typing indicator is cosmetic: failures (429 included) are dropped, not propagated.
        let _ = self.transport.execute(request).await;
    }

    /// `PUT /api/v4/posts/{post_id}` with the new message.
    async fn edit_message(
        &self,
        _chat_id: &str,
        message_id: &str,
        content: &str,
        _finalize: bool,
    ) -> SendResult {
        if content.trim().is_empty() {
            return SendResult::failed("empty message content", Some("bad_format"));
        }
        let body = json!({"id": message_id, "message": content});
        let request = self.authorized(HttpRequest::put_json(
            self.endpoint(&format!("/posts/{message_id}")),
            &body,
        ));
        match self.transport.execute(request).await {
            Ok(response) if response.ok() => match response.json() {
                Ok(post) => {
                    let id = post.get("id").and_then(Value::as_str).unwrap_or(message_id);
                    SendResult::ok(id)
                }
                Err(_) => SendResult::ok(message_id),
            },
            Ok(response) => classify_failure(&response),
            Err(err) => transport_failure(&err),
        }
    }

    /// `DELETE /api/v4/posts/{post_id}`. The trait returns no result, so failures are swallowed;
    /// the request itself is real.
    async fn delete_message(&self, _chat_id: &str, message_id: &str) {
        let request = self.authorized(HttpRequest::delete(
            self.endpoint(&format!("/posts/{message_id}")),
        ));
        let _ = self.transport.execute(request).await;
    }
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

    /// Records every request and replays queued responses (200 `{"id": "default"}` once drained).
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

        /// Queue a mixed sequence of answers and transport errors, in call order.
        fn from_results(results: Vec<Result<HttpResponse, TransportError>>) -> Arc<Self> {
            let mock = MockTransport::default();
            lock(&mock.responses).extend(results);
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
                None => Ok(json_response(200, json!({ "id": "default" }))),
            }
        }
    }

    const BASE: &str = "https://mm.example.test";

    fn json_response(status: u16, body: Value) -> HttpResponse {
        HttpResponse {
            status,
            body: body.to_string(),
            retry_after: None,
        }
    }

    fn text_response(status: u16, body: &str) -> HttpResponse {
        HttpResponse {
            status,
            body: body.to_string(),
            retry_after: None,
        }
    }

    fn retry_response(status: u16, body: Value, retry_after: f64) -> HttpResponse {
        HttpResponse {
            status,
            body: body.to_string(),
            retry_after: Some(retry_after),
        }
    }

    fn adapter(mock: &Arc<MockTransport>) -> MattermostAdapter {
        MattermostAdapter::new(mock.clone(), BASE, "test-token")
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

    fn assert_authorized(request: &HttpRequest) {
        assert_eq!(header(request, "Authorization"), Some("Bearer test-token"));
    }

    fn response_ids(count: usize) -> Vec<HttpResponse> {
        (0..count)
            .map(|idx| json_response(200, json!({ "id": format!("p{idx}") })))
            .collect()
    }

    /// A 400 body in the exact shape Mattermost emits.
    fn api_error(status: u16, id: &str, message: &str) -> HttpResponse {
        json_response(
            status,
            json!({ "id": id, "message": message, "status_code": status }),
        )
    }

    /// The upload answer Mattermost returns: the file id lives in `file_infos`.
    fn upload_response(file_id: &str) -> HttpResponse {
        json_response(
            201,
            json!({
                "file_infos": [{
                    "id": file_id,
                    "name": "chart.png",
                    "mime_type": "image/png",
                    "size": 24,
                }],
            }),
        )
    }

    /// A created post — the only answer `send_media` counts as delivered.
    fn post_response(id: &str) -> HttpResponse {
        json_response(
            201,
            json!({ "id": id, "channel_id": "chan1", "message": "hi" }),
        )
    }

    /// The boundary the adapter declared in `Content-Type`.
    fn boundary_of(request: &HttpRequest) -> String {
        header(request, "Content-Type")
            .and_then(|value| value.strip_prefix("multipart/form-data; boundary="))
            .unwrap_or_default()
            .to_string()
    }

    /// The upload body decoded as text (lossy: binary assertions use the byte form below).
    fn body_text(request: &HttpRequest) -> String {
        String::from_utf8_lossy(request.body.as_deref().unwrap_or_default()).to_string()
    }

    /// Whether the raw request body carries `needle` byte for byte.
    fn body_contains(request: &HttpRequest, needle: &[u8]) -> bool {
        let body = request.body.as_deref().unwrap_or_default();
        !needle.is_empty() && body.windows(needle.len()).any(|window| window == needle)
    }

    /// A labelled image attachment so framing assertions stay about framing.
    fn bytes_media(bytes: &[u8]) -> OutboundMedia {
        OutboundMedia::new(MediaKind::Image, MediaSource::Bytes(bytes.to_vec()))
            .with_filename("chart.png")
            .with_mime("image/png")
    }

    #[tokio::test]
    async fn connect_probes_users_me_with_bearer_auth() {
        let mock = MockTransport::with(vec![json_response(
            200,
            json!({ "id": "bot123", "username": "vikram" }),
        )]);
        let mut mattermost = adapter(&mock);
        assert!(mattermost.connect(false).await);

        let requests = mock.requests();
        assert_eq!(requests.len(), 1);
        assert_eq!(requests[0].method, HttpMethod::Get);
        assert_eq!(
            requests[0].url,
            "https://mm.example.test/api/v4/users/me".to_string()
        );
        assert_authorized(&requests[0]);
    }

    #[tokio::test]
    async fn connect_fails_without_a_user_id() {
        let mock = MockTransport::with(vec![
            api_error(
                401,
                "api.context.session_expired.app_error",
                "Invalid token",
            ),
            json_response(200, json!({ "username": "vikram" })),
            json_response(200, json!({ "id": "   " })),
        ]);
        let mut mattermost = adapter(&mock);
        assert!(!mattermost.connect(false).await);
        assert!(!mattermost.connect(true).await);
        assert!(!mattermost.connect(true).await);

        let offline = MockTransport::failing(TransportError::Request("boom".to_string()));
        let mut unreachable = adapter(&offline);
        assert!(!unreachable.connect(false).await);
    }

    #[tokio::test]
    async fn trailing_slashes_are_stripped_from_the_base_url() {
        let mock = MockTransport::with(vec![json_response(200, json!({ "id": "bot123" }))]);
        let mut mattermost =
            MattermostAdapter::new(mock.clone(), "https://mm.example.test///", "t");
        assert!(mattermost.connect(false).await);
        assert_eq!(
            mock.requests()[0].url,
            "https://mm.example.test/api/v4/users/me".to_string()
        );
    }

    #[tokio::test]
    async fn send_posts_json_with_the_channel_and_message() {
        let mock = MockTransport::with(vec![json_response(200, json!({ "id": "post1" }))]);
        let result = adapter(&mock)
            .send("chan1", "hello there", None, None)
            .await;
        assert!(result.success, "{:?}", result.error);
        assert_eq!(result.message_id.as_deref(), Some("post1"));
        assert_eq!(result.continuation_message_ids, vec!["post1"]);

        let requests = mock.requests();
        assert_eq!(requests.len(), 1);
        assert_eq!(requests[0].method, HttpMethod::Post);
        assert_eq!(
            requests[0].url,
            "https://mm.example.test/api/v4/posts".to_string()
        );
        assert_authorized(&requests[0]);
        assert_eq!(
            header(&requests[0], "Content-Type"),
            Some("application/json")
        );
        let body = body_json(&requests[0]);
        assert_eq!(body["channel_id"], "chan1");
        assert_eq!(body["message"], "hello there");
        assert!(body.get("root_id").is_none());
    }

    #[tokio::test]
    async fn send_threads_every_chunk_under_the_reply_root() {
        let mock = MockTransport::with(response_ids(4));
        let result = adapter(&mock)
            .send("chan1", &"x".repeat(9_000), Some("root1"), None)
            .await;
        assert!(result.success, "{:?}", result.error);

        let requests = mock.requests();
        assert!(requests.len() > 1, "expected several chunks");
        for request in &requests {
            let body = body_json(request);
            assert_eq!(body["channel_id"], "chan1");
            assert_eq!(body["root_id"], "root1");
        }
        assert_eq!(result.continuation_message_ids.len(), requests.len());
        assert_eq!(
            result.message_id.as_deref(),
            Some(format!("p{}", requests.len() - 1).as_str())
        );
    }

    #[tokio::test]
    async fn send_chunks_long_content_within_the_four_thousand_cap() {
        let mock = MockTransport::with(response_ids(8));
        let content = "word ".repeat(2_000); // 10_000 characters, needs three chunks
        let result = adapter(&mock).send("chan1", &content, None, None).await;
        assert!(result.success, "{:?}", result.error);

        let requests = mock.requests();
        assert!(requests.len() > 1, "expected several chunks");
        for request in &requests {
            assert_eq!(request.method, HttpMethod::Post);
            assert_eq!(request.url, "https://mm.example.test/api/v4/posts");
            let body = body_json(request);
            let chunk = body["message"].as_str().unwrap_or_default();
            assert!(
                codepoint_len(chunk) <= MAX_MESSAGE_LENGTH,
                "chunk measures {}",
                codepoint_len(chunk)
            );
            assert!(!chunk.trim().is_empty());
        }
        // The ids land in send order and the last one is what later edits would target.
        assert_eq!(result.continuation_message_ids.len(), requests.len());
        assert_eq!(
            result.message_id.as_deref(),
            Some(format!("p{}", requests.len() - 1).as_str())
        );
    }

    #[tokio::test]
    async fn partial_delivery_keeps_the_chunks_that_landed() {
        let mut responses = response_ids(1);
        responses.push(api_error(500, "api.context.500", "Server error"));
        let mock = MockTransport::with(responses);
        let result = adapter(&mock)
            .send("chan1", &"y".repeat(9_000), None, None)
            .await;
        assert!(result.success);
        assert_eq!(result.message_id.as_deref(), Some("p0"));
        assert_eq!(result.continuation_message_ids, vec!["p0"]);
        let (delivered, total) = result.partial_overflow().unwrap_or_default();
        assert_eq!(delivered, 1);
        assert!(total > 1);
    }

    #[tokio::test]
    async fn rate_limit_reads_the_retry_after_header() {
        let mock = MockTransport::with(vec![retry_response(
            429,
            json!({
                "id": "api.context.429",
                "message": "Please wait before retrying",
                "status_code": 429,
            }),
            2.5,
        )]);
        let result = adapter(&mock).send("chan1", "hi", None, None).await;
        assert!(!result.success);
        assert!(result.retryable);
        assert_eq!(result.retry_after, Some(2.5));
        assert_eq!(result.error_kind.as_deref(), Some("rate_limited"));
        assert!(
            result
                .error
                .as_deref()
                .unwrap_or_default()
                .contains("Please wait before retrying"),
            "the api message should surface"
        );
    }

    #[tokio::test]
    async fn rate_limit_error_id_is_retryable_on_any_status() {
        let mock = MockTransport::with(vec![api_error(
            400,
            "api.context.rate_limit.exceeded",
            "Rate limit exceeded",
        )]);
        let result = adapter(&mock).send("chan1", "hi", None, None).await;
        assert!(!result.success);
        assert!(result.retryable);
        assert_eq!(result.retry_after, None);
        assert_eq!(result.error_kind.as_deref(), Some("rate_limited"));
    }

    #[tokio::test]
    async fn unauthorized_and_forbidden_map_to_forbidden() {
        for status in [401u16, 403] {
            let mock = MockTransport::with(vec![api_error(
                status,
                "api.context.permissions.app_error",
                "You do not have the appropriate permissions",
            )]);
            let result = adapter(&mock).send("chan1", "hi", None, None).await;
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
        let mock = MockTransport::with(vec![api_error(
            404,
            "api.context.404.app_error",
            "Unable to find the existing channel.",
        )]);
        let result = adapter(&mock).send("404", "hi", None, None).await;
        assert!(!result.success);
        assert_eq!(result.error_kind.as_deref(), Some("not_found"));
        assert!(!result.retryable);
    }

    #[tokio::test]
    async fn other_client_errors_map_to_bad_format() {
        let mock = MockTransport::with(vec![api_error(
            400,
            "api.post.create_post.channel_id.app_error",
            "Invalid channel_id parameter",
        )]);
        let result = adapter(&mock).send("chan1", "hi", None, None).await;
        assert!(!result.success);
        assert_eq!(result.error_kind.as_deref(), Some("bad_format"));
        assert!(!result.retryable);
    }

    #[tokio::test]
    async fn server_errors_and_transport_failures_are_transient() {
        let server = MockTransport::with(vec![api_error(
            503,
            "api.context.503",
            "Service Unavailable",
        )]);
        let transient = adapter(&server).send("chan1", "hi", None, None).await;
        assert_eq!(transient.error_kind.as_deref(), Some("transient"));
        assert!(transient.retryable);

        let offline = MockTransport::failing(TransportError::Request("connection reset".into()));
        let unreachable = adapter(&offline).send("chan1", "hi", None, None).await;
        assert_eq!(unreachable.error_kind.as_deref(), Some("transient"));
        assert!(unreachable.retryable);
    }

    #[tokio::test]
    async fn empty_content_is_rejected_without_a_request() {
        let mock = MockTransport::with(vec![json_response(200, json!({ "id": "p1" }))]);
        let mattermost = adapter(&mock);
        for content in ["", "   ", "\n\t \r\n"] {
            let result = mattermost.send("chan1", content, None, None).await;
            assert!(!result.success, "content {content:?}");
            assert_eq!(result.error_kind.as_deref(), Some("bad_format"));
        }
        assert_eq!(mock.request_count(), 0);
    }

    #[tokio::test]
    async fn chat_info_prefers_the_display_name() {
        let mock = MockTransport::with(vec![json_response(
            200,
            json!({ "id": "c1", "name": "town-square", "display_name": "Town Square", "type": "O" }),
        )]);
        let info = adapter(&mock).get_chat_info("c1").await;
        assert_eq!(
            info,
            ChatInfo {
                name: "Town Square".to_string(),
                chat_type: "channel".to_string(),
            }
        );
        let requests = mock.requests();
        assert_eq!(requests[0].method, HttpMethod::Get);
        assert_eq!(
            requests[0].url,
            "https://mm.example.test/api/v4/channels/c1".to_string()
        );
        assert_authorized(&requests[0]);
    }

    #[tokio::test]
    async fn chat_info_maps_every_channel_type_code() {
        for (code, expected) in [
            ("D", "dm"),
            ("G", "group"),
            ("O", "channel"),
            ("P", "channel"),
        ] {
            let mock = MockTransport::with(vec![json_response(
                200,
                json!({ "id": "c1", "name": "slug", "type": code }),
            )]);
            let info = adapter(&mock).get_chat_info("c1").await;
            assert_eq!(info.chat_type, expected, "type {code}");
            assert_eq!(info.name, "slug", "type {code} falls back to name");
        }
    }

    #[tokio::test]
    async fn chat_info_degrades_to_the_channel_fallback() {
        let missing = MockTransport::with(vec![api_error(
            404,
            "api.context.404.app_error",
            "Unable to find the existing channel.",
        )]);
        assert_eq!(
            adapter(&missing).get_chat_info("404").await,
            ChatInfo {
                name: "404".to_string(),
                chat_type: "channel".to_string(),
            }
        );

        let broken = MockTransport::with(vec![text_response(200, "not json")]);
        assert_eq!(
            adapter(&broken).get_chat_info("c1").await,
            ChatInfo {
                name: "c1".to_string(),
                chat_type: "channel".to_string(),
            }
        );

        // A channel object without a usable name still routes by id, and an unknown type code
        // is treated as a plain channel.
        let unnamed = MockTransport::with(vec![json_response(
            200,
            json!({ "id": "c1", "display_name": "   ", "type": "X" }),
        )]);
        assert_eq!(
            adapter(&unnamed).get_chat_info("c1").await,
            ChatInfo {
                name: "c1".to_string(),
                chat_type: "channel".to_string(),
            }
        );
    }

    #[tokio::test]
    async fn typing_hits_the_users_me_typing_endpoint() {
        let mock = MockTransport::with(vec![json_response(200, json!({ "status": "OK" }))]);
        adapter(&mock).send_typing("chan1", None).await;

        let requests = mock.requests();
        assert_eq!(requests.len(), 1);
        assert_eq!(requests[0].method, HttpMethod::Post);
        assert_eq!(
            requests[0].url,
            "https://mm.example.test/api/v4/users/me/typing".to_string()
        );
        assert_authorized(&requests[0]);
        assert_eq!(body_json(&requests[0])["channel_id"], "chan1");
    }

    #[tokio::test]
    async fn typing_failures_are_swallowed() {
        let offline = MockTransport::failing(TransportError::Request("down".into()));
        adapter(&offline).send_typing("chan1", None).await;
        assert_eq!(offline.request_count(), 1);
    }

    #[tokio::test]
    async fn edit_message_uses_put_and_reports_failures() {
        let mock = MockTransport::with(vec![
            json_response(200, json!({"id": "post1", "message": "new text"})),
            json_response(
                403,
                json!({"id": "api.context.permissions", "message": "nope"}),
            ),
        ]);
        let adapter = adapter(&mock);

        let result = adapter
            .edit_message("chan1", "post1", "new text", false)
            .await;
        assert!(result.success);
        assert_eq!(result.message_id.as_deref(), Some("post1"));
        {
            let requests = mock.requests();
            assert_eq!(requests.len(), 1);
            assert_eq!(requests[0].method, HttpMethod::Put);
            assert!(requests[0].url.ends_with("/api/v4/posts/post1"));
            let body: Value =
                serde_json::from_slice(requests[0].body.as_ref().expect("body")).expect("json");
            assert_eq!(body["id"], "post1");
            assert_eq!(body["message"], "new text");
        }

        let refused = adapter.edit_message("chan1", "post1", "newer", false).await;
        assert!(!refused.success);
        assert_eq!(refused.error_kind.as_deref(), Some("forbidden"));
    }

    #[tokio::test]
    async fn delete_message_issues_a_delete() {
        let mock = MockTransport::with(vec![json_response(200, json!({"status": "OK"}))]);
        adapter(&mock).delete_message("chan1", "post1").await;
        let requests = mock.requests();
        assert_eq!(requests.len(), 1);
        assert_eq!(requests[0].method, HttpMethod::Delete);
        assert!(requests[0].url.ends_with("/api/v4/posts/post1"));
    }

    #[tokio::test]
    async fn poll_once_parses_posts_in_create_order() {
        let mock = MockTransport::with(vec![json_response(
            200,
            json!({
                "order": ["p2", "p1"],
                "posts": {
                    "p1": {
                        "id": "p1",
                        "user_id": "u1",
                        "message": "first post",
                        "create_at": 2000,
                        "root_id": "",
                    },
                    "p2": {
                        "id": "p2",
                        "user_id": "u2",
                        "message": "a threaded reply",
                        "create_at": 3000,
                        "root_id": "p1",
                    },
                },
            }),
        )]);
        let events = adapter(&mock)
            .poll_once("chan1", 1_500)
            .await
            .expect("poll should succeed");
        assert_eq!(events.len(), 2);

        let requests = mock.requests();
        assert_eq!(requests.len(), 1);
        assert_eq!(requests[0].method, HttpMethod::Get);
        assert_eq!(
            requests[0].url,
            "https://mm.example.test/api/v4/channels/chan1/posts?since=1500&per_page=60"
                .to_string()
        );
        assert_authorized(&requests[0]);

        // Sorted by create_at even though the map arrived out of order.
        assert_eq!(events[0].message_id.as_deref(), Some("p1"));
        assert_eq!(events[0].text, "first post");
        assert_eq!(events[0].user_id.as_deref(), Some("u1"));
        assert_eq!(events[0].platform_update_id, Some(2000));
        assert_eq!(events[0].reply_to_message_id, None);
        assert_eq!(events[0].source.chat_id, "chan1");
        assert_eq!(events[0].source.chat_type, "channel");
        assert_eq!(events[0].source.platform.as_str(), "mattermost");
        assert_eq!(events[0].source.user_id.as_deref(), Some("u1"));
        assert_eq!(events[0].source.message_id.as_deref(), Some("p1"));
        assert_eq!(
            events[0].raw_message.as_ref().and_then(|raw| raw.get("id")),
            Some(&json!("p1"))
        );

        assert_eq!(events[1].message_id.as_deref(), Some("p2"));
        assert_eq!(events[1].reply_to_message_id.as_deref(), Some("p1"));
        assert_eq!(events[1].platform_update_id, Some(3000));
        assert_eq!(events[1].user_id.as_deref(), Some("u2"));
    }

    #[tokio::test]
    async fn poll_once_tolerates_empty_payloads() {
        for body in [
            json!({}),
            json!({ "posts": {} }),
            json!({ "posts": null }),
            json!({ "posts": { "nameless": { "message": "no id" } } }),
        ] {
            let mock = MockTransport::with(vec![json_response(200, body.clone())]);
            let events = adapter(&mock)
                .poll_once("chan1", 0)
                .await
                .unwrap_or_else(|err| panic!("poll failed for {body}: {err}"));
            assert!(events.is_empty(), "unexpected events for {body}");
            assert_eq!(mock.request_count(), 1);
        }
    }

    #[tokio::test]
    async fn poll_once_reports_api_and_wire_failures() {
        let forbidden = MockTransport::with(vec![api_error(
            403,
            "api.context.permissions.app_error",
            "You do not have the appropriate permissions",
        )]);
        let err = adapter(&forbidden)
            .poll_once("chan1", 0)
            .await
            .err()
            .unwrap_or_default();
        assert!(err.contains("403"), "{err}");
        assert!(err.contains("permissions"), "{err}");

        let broken = MockTransport::with(vec![text_response(200, "<html>proxy</html>")]);
        let err = adapter(&broken)
            .poll_once("chan1", 0)
            .await
            .err()
            .unwrap_or_default();
        assert!(err.contains("not json"), "{err}");

        let offline = MockTransport::failing(TransportError::Request("reset".into()));
        let err = adapter(&offline)
            .poll_once("chan1", 0)
            .await
            .err()
            .unwrap_or_default();
        assert!(err.contains("transport error"), "{err}");
    }

    #[tokio::test]
    async fn capability_flags_match_hermes() {
        let mock = MockTransport::with(Vec::new());
        let mut mattermost = adapter(&mock);
        assert!(mattermost.splits_long_messages());
        assert!(mattermost.supports_code_blocks());
        assert_eq!(mattermost.max_message_length(), MAX_MESSAGE_LENGTH);
        assert_eq!(mattermost.message_length("héllo"), 5);
        assert_eq!(mattermost.typed_command_prefix(), "/");
        assert_eq!(MATTERMOST_API_PREFIX, "/api/v4");
        mattermost.disconnect().await;
    }

    // ----- outbound media (`supports_media` / `send_media`) -----

    #[tokio::test]
    async fn media_uploads_multipart_and_attaches_the_file_id() {
        let mock = MockTransport::with(vec![upload_response("file1"), post_response("post1")]);
        let media = bytes_media(b"\x89PNG\r\n\x1a\n-bytes").with_caption("a chart");
        let result = adapter(&mock)
            .send_media("chan1", &media, Some("root1"))
            .await;
        assert!(result.success, "{:?}", result.error);
        assert_eq!(result.message_id.as_deref(), Some("post1"));
        assert_eq!(result.error_kind, None);

        let requests = mock.requests();
        assert_eq!(requests.len(), 2, "one upload, then one post");

        // 1. The upload: POST /api/v4/files, multipart/form-data, bearing the token.
        let upload = &requests[0];
        assert_eq!(upload.method, HttpMethod::Post);
        assert_eq!(
            upload.url,
            "https://mm.example.test/api/v4/files".to_string()
        );
        assert_authorized(upload);
        let boundary = boundary_of(upload);
        assert!(!boundary.is_empty(), "the boundary must be declared");
        let declared = format!("multipart/form-data; boundary={boundary}");
        assert_eq!(header(upload, "Content-Type"), Some(declared.as_str()));

        let body = body_text(upload);
        assert!(body.starts_with(&format!("--{boundary}\r\n")), "{body}");
        assert!(
            body.contains("Content-Disposition: form-data; name=\"channel_id\"\r\n\r\nchan1\r\n"),
            "{body}"
        );
        assert!(
            body.contains(
                "Content-Disposition: form-data; name=\"files\"; filename=\"chart.png\"\r\n"
            ),
            "{body}"
        );
        assert!(body.contains("Content-Type: image/png\r\n\r\n"), "{body}");
        assert!(body.ends_with(&format!("--{boundary}--\r\n")), "{body}");
        assert!(
            body_contains(upload, b"\x89PNG\r\n\x1a\n-bytes"),
            "the raw bytes must travel inside the files part"
        );

        // 2. The post: the uploaded id rides in `file_ids`, the caption in `message`, the reply
        //    anchor in `root_id`.
        let post = &requests[1];
        assert_eq!(post.method, HttpMethod::Post);
        assert_eq!(post.url, "https://mm.example.test/api/v4/posts".to_string());
        assert_authorized(post);
        assert_eq!(header(post, "Content-Type"), Some("application/json"));
        let body = body_json(post);
        assert_eq!(body["channel_id"], "chan1");
        assert_eq!(body["message"], "a chart");
        assert_eq!(body["file_ids"], json!(["file1"]));
        assert_eq!(body["root_id"], "root1");
    }

    #[tokio::test]
    async fn media_without_caption_or_reply_posts_an_empty_message() {
        let mock = MockTransport::with(vec![upload_response("file9"), post_response("post9")]);
        let result = adapter(&mock)
            .send_media("chan1", &bytes_media(b"png-bytes"), None)
            .await;
        assert!(result.success, "{:?}", result.error);
        assert_eq!(result.message_id.as_deref(), Some("post9"));

        let requests = mock.requests();
        assert_eq!(requests.len(), 2);
        let body = body_json(&requests[1]);
        assert_eq!(body["channel_id"], "chan1");
        // hermes posts `caption or ""` (`adapter.py:212`): the field is present and empty.
        assert_eq!(body["message"], "");
        assert_eq!(body["file_ids"], json!(["file9"]));
        assert!(
            body.get("root_id").is_none(),
            "a flat post carries no root_id"
        );
    }

    #[tokio::test]
    async fn media_without_a_filename_or_mime_uses_the_kind_defaults() {
        let mock = MockTransport::with(vec![upload_response("file1"), post_response("post1")]);
        let media = OutboundMedia::new(MediaKind::Document, MediaSource::Bytes(b"pdf".to_vec()));
        let result = adapter(&mock).send_media("chan1", &media, None).await;
        assert!(result.success, "{:?}", result.error);

        let requests = mock.requests();
        assert_eq!(requests.len(), 2);
        let body = body_text(&requests[0]);
        assert!(body.contains("filename=\"attachment.bin\""), "{body}");
        assert!(
            body.contains("Content-Type: application/octet-stream"),
            "{body}"
        );
        assert!(body_contains(&requests[0], b"pdf"));
    }

    #[tokio::test]
    async fn media_upload_failure_stops_before_the_post() {
        let mock = MockTransport::with(vec![api_error(
            401,
            "api.context.session_expired.app_error",
            "Invalid token",
        )]);
        let result = adapter(&mock)
            .send_media("chan1", &bytes_media(b"png"), Some("root1"))
            .await;
        assert!(!result.success);
        assert_eq!(result.error_kind.as_deref(), Some("forbidden"));
        assert_eq!(result.message_id, None);
        assert!(
            result
                .error
                .as_deref()
                .unwrap_or_default()
                .contains("Invalid token")
        );
        assert_eq!(mock.request_count(), 1, "the post must not be attempted");
    }

    #[tokio::test]
    async fn media_upload_without_a_file_id_is_not_a_success() {
        for body in [
            json!({ "file_infos": [{ "name": "chart.png" }] }),
            json!({ "file_infos": [{ "id": "   " }] }),
            json!({ "file_infos": [] }),
            json!({}),
        ] {
            let mock = MockTransport::with(vec![json_response(201, body.clone())]);
            let result = adapter(&mock)
                .send_media("chan1", &bytes_media(b"png"), None)
                .await;
            assert!(!result.success, "{body}");
            assert_eq!(result.error_kind.as_deref(), Some("unknown"), "{body}");
            assert!(
                result
                    .error
                    .as_deref()
                    .unwrap_or_default()
                    .contains("no file id"),
                "{body}"
            );
            assert_eq!(mock.request_count(), 1, "{body} must not post");
        }

        // An upload answer that is not JSON at all is the same hard stop.
        let broken = MockTransport::with(vec![text_response(201, "<html>proxy</html>")]);
        let result = adapter(&broken)
            .send_media("chan1", &bytes_media(b"png"), None)
            .await;
        assert!(!result.success);
        assert_eq!(result.error_kind.as_deref(), Some("unknown"));
        assert_eq!(broken.request_count(), 1);
    }

    #[tokio::test]
    async fn media_post_failure_is_surfaced() {
        let mock = MockTransport::with(vec![
            upload_response("file1"),
            api_error(500, "api.context.500", "Server error"),
        ]);
        let result = adapter(&mock)
            .send_media("chan1", &bytes_media(b"png"), None)
            .await;
        assert!(!result.success);
        assert_eq!(result.error_kind.as_deref(), Some("transient"));
        assert!(result.retryable);
        assert_eq!(result.message_id, None);
        assert!(
            result
                .error
                .as_deref()
                .unwrap_or_default()
                .contains("Server error"),
            "the api message should surface"
        );
        assert_eq!(mock.request_count(), 2, "upload and post both attempted");
    }

    #[tokio::test]
    async fn media_post_without_an_id_is_not_a_success() {
        let mock = MockTransport::with(vec![
            upload_response("file1"),
            json_response(201, json!({ "channel_id": "chan1", "message": "hi" })),
        ]);
        let result = adapter(&mock)
            .send_media("chan1", &bytes_media(b"png"), None)
            .await;
        assert!(!result.success, "only a post id counts as delivered");
        assert_eq!(result.error_kind.as_deref(), Some("unknown"));
        assert_eq!(result.message_id, None);
        assert!(
            result
                .error
                .as_deref()
                .unwrap_or_default()
                .contains("no post id")
        );
    }

    #[tokio::test]
    async fn media_status_codes_map_to_the_standard_kinds() {
        for (status, kind, retryable) in [
            (401u16, "forbidden", false),
            (403, "forbidden", false),
            (404, "not_found", false),
            (413, "too_long", false),
            (400, "bad_format", false),
            (500, "transient", true),
            (503, "transient", true),
            (502, "transient", true),
        ] {
            let mock = MockTransport::with(vec![api_error(status, "api.context", "refused")]);
            let result = adapter(&mock)
                .send_media("chan1", &bytes_media(b"png"), None)
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
                "{status} should surface in the message"
            );
            assert_eq!(mock.request_count(), 1, "{status} must not post");
        }
    }

    #[tokio::test]
    async fn media_post_status_codes_use_the_media_mapping() {
        let too_large = MockTransport::with(vec![
            upload_response("file1"),
            api_error(413, "api.context.413", "Request entity too large"),
        ]);
        let result = adapter(&too_large)
            .send_media("chan1", &bytes_media(b"png"), None)
            .await;
        assert!(!result.success);
        assert_eq!(result.error_kind.as_deref(), Some("too_long"));
        assert!(!result.retryable);

        let forbidden = MockTransport::with(vec![
            upload_response("file1"),
            api_error(403, "api.context.permissions", "nope"),
        ]);
        let result = adapter(&forbidden)
            .send_media("chan1", &bytes_media(b"png"), None)
            .await;
        assert!(!result.success);
        assert_eq!(result.error_kind.as_deref(), Some("forbidden"));
    }

    #[tokio::test]
    async fn media_rate_limits_read_the_retry_after() {
        let upload_limited = MockTransport::with(vec![retry_response(
            429,
            json!({
                "id": "api.context.429",
                "message": "Please wait before retrying",
                "status_code": 429,
            }),
            3.5,
        )]);
        let result = adapter(&upload_limited)
            .send_media("chan1", &bytes_media(b"png"), None)
            .await;
        assert!(!result.success);
        assert_eq!(result.error_kind.as_deref(), Some("rate_limited"));
        assert!(result.retryable);
        assert_eq!(result.retry_after, Some(3.5));
        assert_eq!(upload_limited.request_count(), 1);

        let post_limited = MockTransport::with(vec![
            upload_response("file1"),
            retry_response(
                429,
                json!({ "id": "api.context.429", "message": "slow down", "status_code": 429 }),
                1.25,
            ),
        ]);
        let result = adapter(&post_limited)
            .send_media("chan1", &bytes_media(b"png"), None)
            .await;
        assert!(!result.success);
        assert_eq!(result.error_kind.as_deref(), Some("rate_limited"));
        assert!(result.retryable);
        assert_eq!(result.retry_after, Some(1.25));
        assert_eq!(post_limited.request_count(), 2);
    }

    #[tokio::test]
    async fn media_transport_failures_are_transient_and_stop() {
        let offline = MockTransport::from_results(vec![
            Err(TransportError::Request("connection reset".into())),
            Ok(post_response("post1")),
        ]);
        let result = adapter(&offline)
            .send_media("chan1", &bytes_media(b"png"), None)
            .await;
        assert!(!result.success);
        assert_eq!(result.error_kind.as_deref(), Some("transient"));
        assert!(result.retryable);
        assert_eq!(offline.request_count(), 1, "no post after a failed upload");

        let post_offline = MockTransport::from_results(vec![
            Ok(upload_response("file1")),
            Err(TransportError::Request("reset".into())),
        ]);
        let result = adapter(&post_offline)
            .send_media("chan1", &bytes_media(b"png"), None)
            .await;
        assert!(!result.success);
        assert_eq!(result.error_kind.as_deref(), Some("transient"));
        assert!(result.retryable);
        assert_eq!(post_offline.request_count(), 2);
    }

    #[tokio::test]
    async fn media_empty_bytes_are_refused_without_a_request() {
        let mock = MockTransport::with(vec![upload_response("file1"), post_response("post1")]);
        let result = adapter(&mock)
            .send_media("chan1", &bytes_media(b""), Some("root1"))
            .await;
        assert!(!result.success);
        assert_eq!(result.error_kind.as_deref(), Some("bad_format"));
        assert!(!result.retryable);
        assert_eq!(result.message_id, None);
        assert!(
            result
                .error
                .as_deref()
                .unwrap_or_default()
                .contains("empty")
        );
        assert_eq!(mock.request_count(), 0);
    }

    #[tokio::test]
    async fn media_over_the_upload_cap_is_refused_without_a_request() {
        let mock = MockTransport::with(vec![upload_response("file1")]);
        let oversized = vec![0u8; MAX_UPLOAD_BYTES + 1];
        let result = adapter(&mock)
            .send_media("chan1", &bytes_media(&oversized), None)
            .await;
        assert!(!result.success);
        assert_eq!(result.error_kind.as_deref(), Some("too_long"));
        assert!(!result.retryable);
        assert!(
            result
                .error
                .as_deref()
                .unwrap_or_default()
                .contains("over the")
        );
        assert_eq!(mock.request_count(), 0);
    }

    #[tokio::test]
    async fn media_url_sources_are_refused_truthfully() {
        let mock = MockTransport::with(vec![upload_response("file1"), post_response("post1")]);
        let media = OutboundMedia::image_url("https://example.test/a.png").with_caption("a chart");
        let result = adapter(&mock)
            .send_media("chan1", &media, Some("root1"))
            .await;
        assert!(!result.success, "a URL fetch-through is not ported");
        assert_eq!(result.error_kind.as_deref(), Some("bad_format"));
        assert!(!result.retryable);
        assert_eq!(result.message_id, None);
        let error = result.error.as_deref().unwrap_or_default();
        assert!(error.contains("https://example.test/a.png"), "{error}");
        assert!(error.contains("MediaSource::Bytes"), "{error}");
        assert!(error.contains("adapter.py:328-357"), "{error}");
        assert_eq!(
            mock.request_count(),
            0,
            "a refused URL must not touch the network"
        );
    }

    #[tokio::test]
    async fn media_support_is_declared() {
        let mock = MockTransport::with(Vec::new());
        let mattermost = adapter(&mock);
        assert!(mattermost.supports_media());
        assert_eq!(MAX_UPLOAD_BYTES, 50 * 1024 * 1024);
        // Declaring media support must not disturb the text path.
        assert_eq!(mattermost.max_message_length(), MAX_MESSAGE_LENGTH);
    }
}
