//! Photon adapter — local sidecar HTTP API.
//!
//! Photon's sidecar (default `127.0.0.1:8789`) owns the iMessage bridge and exposes a small JSON
//! API. This adapter talks to it over the shared [`Transport`](crate::transport::Transport):
//! `GET /health` to prove the sidecar is up and `POST /send` to deliver text, authenticated with
//! `X-Hermes-Sidecar-Token`. Inbound events arrive on the sidecar's SSE stream, parsed here into
//! [`MessageEvent`]s; the host supplies the stream through [`SseSource`].
//!
//! **Outbound media** is one `POST /send-attachment` per file ([`PhotonAdapter::send_media`]). The
//! sidecar takes a **local path**, not uploaded bytes — `spectrum-ts` reads the file from disk
//! itself (`sidecar/index.mjs:1100-1119`, `_attachment_body` at `adapter.py:557-564`) — so bytes
//! handed to this adapter are written to a uniquely named, sanitised temp file for the duration of
//! the call and removed afterwards. Audio goes out as an iMessage voice note (`kind="voice"`).
//!
//! Not ported: inbound attachments beyond a marker, polls and rich links, and process supervision
//! of the sidecar itself.

use async_trait::async_trait;
use serde_json::{Map, Value, json};
use std::path::{Path, PathBuf};
use std::sync::Mutex as StdMutex;
use std::sync::atomic::{AtomicU64, Ordering};
use tokio::sync::Mutex as AsyncMutex;

use crate::event::{MessageEvent, Platform, SessionSource};
use crate::helpers::{codepoint_len, truncate_message};
use crate::platform::{
    ChatInfo, MediaKind, MediaSource, OutboundMedia, PlatformAdapter, SendResult,
};
use crate::transport::SharedTransport;

/// iMessage caps practical message size around 16 KB; stay well under it.
pub const PHOTON_MAX_MESSAGE_LENGTH: usize = 8000;

/// Ceiling for the `/send-attachment` control body, mirroring the sidecar's own `MAX_BODY_BYTES`
/// (`sidecar/index.mjs:826-848`): above it the sidecar destroys the request instead of answering,
/// so a body that big has to be refused locally rather than earned as a dead connection.
///
/// This bounds the JSON we send, **not** the attachment: the reference transfers a path
/// (`adapter.py:557-564`) and zero file bytes, so an attachment of any size is delivered by the
/// sidecar reading it from disk. The reference declares no outbound attachment size cap.
pub const PHOTON_MAX_ATTACHMENT_BODY_BYTES: usize = 2 * 1024 * 1024;

/// Stem used when the caller supplies no `filename`.
const PHOTON_DEFAULT_ATTACHMENT_STEM: &str = "attachment";

/// Longest caller-supplied basename kept; longer names are truncated (extension preserved).
const PHOTON_MAX_ATTACHMENT_NAME_CHARS: usize = 120;

pub struct PhotonAdapter {
    transport: SharedTransport,
    sidecar_base: String,
    token: String,
}

impl PhotonAdapter {
    pub fn new(
        transport: SharedTransport,
        sidecar_base: impl Into<String>,
        token: impl Into<String>,
    ) -> Self {
        Self {
            transport,
            sidecar_base: sidecar_base.into().trim_end_matches('/').to_string(),
            token: token.into(),
        }
    }

    fn endpoint(&self, path: &str) -> String {
        format!("{}{path}", self.sidecar_base)
    }

    fn authorized(&self, request: crate::transport::HttpRequest) -> crate::transport::HttpRequest {
        request.header("X-Hermes-Sidecar-Token", self.token.clone())
    }

    /// `POST /send` with the text classifier.
    async fn post_send(&self, body: &Value) -> SendResult {
        self.post_json("/send", body, classify_failure).await
    }

    /// One authenticated JSON POST to the sidecar, success only on its success envelope.
    ///
    /// The sidecar answers every control call with `{"ok": true, "messageId": …}` (`ok(res, …)`,
    /// `sidecar/index.mjs:923-927`) and a refusal with `{"ok": false, "error": …}`
    /// (`sidecar/index.mjs:850-921`) — the envelope `_post_send` checks in the reference
    /// (`adapter.py:1534-1545`, via `_sidecar_call`, `adapter.py:1547-1559`). `classify` maps a
    /// non-2xx status, so the media path can treat `413` as `too_long` without changing text sends.
    async fn post_json(
        &self,
        path: &str,
        body: &Value,
        classify: fn(&crate::transport::HttpResponse) -> SendResult,
    ) -> SendResult {
        let request = self.authorized(crate::transport::HttpRequest::json(
            self.endpoint(path),
            body,
        ));
        let response = match self.transport.execute(request).await {
            Ok(response) => response,
            Err(err) => {
                return SendResult::failed(
                    format!("photon sidecar transport error: {err}"),
                    Some("transient"),
                )
                .retryable(None);
            }
        };
        if !response.ok() {
            return classify(&response);
        }
        match response.json() {
            Ok(value) if value.get("ok").and_then(Value::as_bool).unwrap_or(false) => {
                match value.get("messageId").and_then(Value::as_str) {
                    Some(id) => SendResult::ok(id),
                    None => SendResult::ok(""),
                }
            }
            Ok(value) => {
                let message = value
                    .get("error")
                    .and_then(Value::as_str)
                    .unwrap_or("photon sidecar rejected the send");
                SendResult::failed(message.to_string(), Some("unknown"))
            }
            Err(err) => SendResult::failed(format!("photon sidecar reply: {err}"), Some("unknown")),
        }
    }

    /// One `POST /send-attachment` for bytes already in hand.
    ///
    /// The sidecar's handler reads `{spaceId, path, kind, name, mimeType, caption}` and drives
    /// `spectrum-ts` from the **path** (`sidecar/index.mjs:1100-1119`) — exactly the body the
    /// reference builds (`_attachment_body`, `adapter.py:557-564`). There is no base64/`data` field
    /// in that body and the sidecar refuses one without `path` (`"spaceId and path are required"`,
    /// `index.mjs:1103-1105`), so the bytes must be on disk first; hence [`TempAttachment`].
    ///
    /// `name`/`mimeType`/`caption` are optional keys in the reference ("only sent when Hermes
    /// supplied them", `adapter.py:563`) and the sidecar likewise only forwards them when truthy
    /// (`index.mjs:1108-1113`), so absent values are left out rather than sent empty.
    async fn post_attachment(
        &self,
        chat_id: &str,
        media: &OutboundMedia,
        bytes: &[u8],
        name: &str,
        mime: Option<String>,
    ) -> SendResult {
        let extension = extension_of(name).or_else(|| {
            mime.as_deref()
                .and_then(extension_for_mime)
                .map(str::to_owned)
        });
        let attachment = match TempAttachment::write(extension.as_deref(), bytes) {
            Ok(attachment) => attachment,
            Err(err) => {
                return SendResult::failed(
                    format!("photon attachment temp file: {err}"),
                    Some("transient"),
                )
                .retryable(None);
            }
        };
        let mut body = Map::new();
        body.insert("spaceId".to_string(), json!(chat_id));
        body.insert(
            "path".to_string(),
            json!(attachment.path().to_string_lossy().to_string()),
        );
        body.insert("kind".to_string(), json!(attachment_kind(media.kind)));
        body.insert("name".to_string(), json!(name));
        if let Some(mime) = mime {
            body.insert("mimeType".to_string(), json!(mime));
        }
        if let Some(caption) = media_caption(media) {
            body.insert("caption".to_string(), json!(caption));
        }
        let body = Value::Object(body);
        match serde_json::to_vec(&body) {
            Ok(encoded) if encoded.len() > PHOTON_MAX_ATTACHMENT_BODY_BYTES => SendResult::failed(
                format!(
                    "photon attachment body is {} bytes, above the sidecar's \
                     {PHOTON_MAX_ATTACHMENT_BODY_BYTES} byte control-body cap \
                     (sidecar/index.mjs:826-848): shorten the caption or the filename",
                    encoded.len()
                ),
                Some("too_long"),
            ),
            Ok(_) => {
                self.post_json("/send-attachment", &body, classify_media_failure)
                    .await
            }
            Err(err) => {
                SendResult::failed(format!("photon attachment body: {err}"), Some("unknown"))
            }
        }
    }
}

#[async_trait]
impl PlatformAdapter for PhotonAdapter {
    async fn connect(&mut self, _is_reconnect: bool) -> bool {
        let request = self.authorized(crate::transport::HttpRequest::get(self.endpoint("/health")));
        matches!(self.transport.execute(request).await, Ok(response) if response.ok())
    }

    async fn disconnect(&mut self) {}

    async fn send(
        &self,
        chat_id: &str,
        content: &str,
        _reply_to: Option<&str>,
        _metadata: Option<Value>,
    ) -> SendResult {
        if chat_id.trim().is_empty() {
            return SendResult::failed("photon needs a space id", Some("bad_format"));
        }
        if content.trim().is_empty() {
            return SendResult::failed("empty message content", Some("bad_format"));
        }

        let chunks = truncate_message(content, PHOTON_MAX_MESSAGE_LENGTH, codepoint_len);
        let mut ids: Vec<String> = Vec::new();
        for chunk in chunks {
            let body = json!({
                "spaceId": chat_id,
                "text": chunk,
                "format": "markdown",
            });
            let result = self.post_send(&body).await;
            if !result.success {
                return result;
            }
            if let Some(id) = result.message_id {
                ids.push(id);
            }
        }
        match ids.last() {
            Some(last) => SendResult::ok_split(last.clone(), ids),
            None => SendResult::ok(""),
        }
    }

    fn supports_code_blocks(&self) -> bool {
        true
    }

    /// Attachments are implemented: one `/send-attachment` per file.
    fn supports_media(&self) -> bool {
        true
    }

    /// Send one attachment as the local path the sidecar reads (see
    /// [`post_attachment`](PhotonAdapter::post_attachment)).
    ///
    /// - [`MediaSource::Bytes`]: written to a uniquely named temp file — caller basename sanitised to
    ///   `[A-Za-z0-9._-]`, removed by RAII once the call returns — and sent as `path`. The reference
    ///   body carries a path, never inline base64 (`_attachment_body`, `adapter.py:557-564`;
    ///   `_sidecar_send_attachment`, `adapter.py:1534-1545`), and the sidecar's handler reads the file
    ///   from disk (`sidecar/index.mjs:1100-1119`), so no `data` field is available to send instead.
    /// - [`MediaSource::Url`]: refused with zero requests. hermes downloads URL media into a local
    ///   file before sending it (`cache_image_from_url`, `adapter.py:1300-1307`; the file-based
    ///   helpers at `adapter.py:1309-1327` take a path), and this adapter owns no binary fetcher, so
    ///   a fetch-through is impossible — the refusal names [`MediaSource::Bytes`].
    /// - `kind`: [`MediaKind::Audio`] → `"voice"`, an iMessage voice note (the reference's
    ///   `send_voice` passes `kind="voice"`, `adapter.py:1314-1317`, and `_attachment_body` keeps
    ///   only that value, `adapter.py:562`); every other kind → `"attachment"`.
    /// - `reply_to` is **not** forwarded: `/send-attachment` has no reply field — its handler
    ///   destructures only `spaceId, path, name, mimeType, caption, kind` (`sidecar/index.mjs:1100-1102`)
    ///   — and every reference media helper accepts `reply_to` and drops it (`adapter.py:1312`,
    ///   `:1317`, `:1322`, `:1327`). The caption is delivered by the sidecar as its own follow-up
    ///   bubble after the media (`index.mjs:1121-1132`), so an attachment send is unthreaded.
    async fn send_media(
        &self,
        chat_id: &str,
        media: &OutboundMedia,
        _reply_to: Option<&str>,
    ) -> SendResult {
        if chat_id.trim().is_empty() {
            return SendResult::failed("photon needs a space id", Some("bad_format"));
        }
        let bytes = match &media.source {
            MediaSource::Bytes(bytes) => bytes,
            MediaSource::Url(url) => {
                return SendResult::failed(
                    format!(
                        "photon media send needs bytes: refusing MediaSource::Url({url}) because \
                         the reference downloads URL media into a local file before sending it \
                         (`cache_image_from_url`, adapter.py:1300-1307) and this adapter has no \
                         binary fetcher — pass MediaSource::Bytes"
                    ),
                    Some("bad_format"),
                );
            }
        };
        if bytes.is_empty() {
            // `spectrum-ts` has nothing to upload for a zero-byte file; refuse instead of asking
            // the sidecar for a delivery that cannot happen.
            return SendResult::failed(
                "photon media send: refusing to send empty bytes",
                Some("bad_format"),
            );
        }
        let name = attachment_name(media);
        let mime = attachment_mime(media, &name);
        self.post_attachment(chat_id, media, bytes, &name, mime)
            .await
    }

    fn splits_long_messages(&self) -> bool {
        true
    }

    fn max_message_length(&self) -> usize {
        PHOTON_MAX_MESSAGE_LENGTH
    }

    async fn get_chat_info(&self, chat_id: &str) -> ChatInfo {
        ChatInfo {
            name: chat_id.to_string(),
            chat_type: "dm".to_string(),
        }
    }

    async fn send_typing(&self, _chat_id: &str, _metadata: Option<Value>) {}
}

fn classify_failure(response: &crate::transport::HttpResponse) -> SendResult {
    let excerpt: String = response.body.chars().take(200).collect();
    let message = format!("photon sidecar returned {}: {excerpt}", response.status);
    match response.status {
        429 => SendResult::failed(message, Some("rate_limited")).retryable(response.retry_after),
        401 | 403 => SendResult::failed(message, Some("forbidden")),
        404 => SendResult::failed(message, Some("not_found")),
        400..=499 => SendResult::failed(message, Some("bad_format")),
        500..=599 => SendResult::failed(message, Some("transient")).retryable(None),
        _ => SendResult::failed(message, Some("unknown")),
    }
}

/// [`classify_failure`] plus the upload-specific `413`: an attachment the upstream rejects as too
/// large is a permanent verdict for that payload, so it maps to `too_long` rather than the generic
/// `bad_format` a text send would get. Kept separate so text sends stay byte-for-byte unchanged.
fn classify_media_failure(response: &crate::transport::HttpResponse) -> SendResult {
    if response.status == 413 {
        let excerpt: String = response.body.chars().take(200).collect();
        return SendResult::failed(
            format!("photon sidecar returned 413: {excerpt}"),
            Some("too_long"),
        );
    }
    classify_failure(response)
}

/// `/send-attachment` `kind`: audio goes as a voice note, everything else as a plain file
/// (`"voice" if kind == "voice" else "attachment"`, `adapter.py:562`).
fn attachment_kind(kind: MediaKind) -> &'static str {
    match kind {
        MediaKind::Audio => "voice",
        MediaKind::Image | MediaKind::Video | MediaKind::Document => "attachment",
    }
}

/// Trimmed caption, or `None` when blank: the reference drops a falsy caption from the body
/// (`adapter.py:563`) and the sidecar only sends one when it is a non-empty string
/// (`sidecar/index.mjs:1123`).
fn media_caption(media: &OutboundMedia) -> Option<String> {
    media
        .caption
        .as_deref()
        .map(str::trim)
        .filter(|caption| !caption.is_empty())
        .map(str::to_owned)
}

/// The `name` the recipient should see.
///
/// A caller filename is reduced to a safe basename; without one, a kind-based stem plus the
/// extension implied by the caller's mime keeps spectrum's extension inference working. The temp
/// file's unique suffix never leaks here (`adapter.py:1544` labels an attachment by
/// `name or basename(path)`).
fn attachment_name(media: &OutboundMedia) -> String {
    if let Some(name) = media.filename.as_deref().and_then(sanitize_attachment_name) {
        return name;
    }
    let stem = match media.kind {
        MediaKind::Image => "image",
        MediaKind::Video => "video",
        MediaKind::Audio => "audio",
        MediaKind::Document => PHOTON_DEFAULT_ATTACHMENT_STEM,
    };
    match media.mime.as_deref().and_then(extension_for_mime) {
        Some(extension) => format!("{stem}.{extension}"),
        None => stem.to_string(),
    }
}

/// The `mimeType` to advertise: the caller's, else guessed from the filename the way the reference
/// guesses from the path (`_guess_mime`, `adapter.py:567-569`, applied at `adapter.py:1543`).
/// `None` leaves the key out and lets `spectrum-ts` infer the type from the extension
/// (`sidecar/index.mjs:1108-1113`).
fn attachment_mime(media: &OutboundMedia, name: &str) -> Option<String> {
    match media
        .mime
        .as_deref()
        .map(str::trim)
        .filter(|mime| !mime.is_empty())
    {
        Some(mime) => Some(mime.to_string()),
        None => extension_of(name)
            .and_then(|extension| mime_for_extension(&extension))
            .map(str::to_owned),
    }
}

/// MIME types for the extensions the reference would recognise through Python's
/// `mimetypes.guess_type` (`adapter.py:567-569`), for the kinds this adapter sends.
const PHOTON_MIME_BY_EXTENSION: &[(&str, &str)] = &[
    ("png", "image/png"),
    ("jpg", "image/jpeg"),
    ("jpeg", "image/jpeg"),
    ("gif", "image/gif"),
    ("webp", "image/webp"),
    ("heic", "image/heic"),
    ("heif", "image/heif"),
    ("tiff", "image/tiff"),
    ("mp4", "video/mp4"),
    ("mov", "video/quicktime"),
    ("mp3", "audio/mpeg"),
    ("m4a", "audio/mp4"),
    ("aac", "audio/aac"),
    ("ogg", "audio/ogg"),
    ("wav", "audio/wav"),
    ("caf", "audio/x-caf"),
    ("pdf", "application/pdf"),
    ("txt", "text/plain"),
    ("csv", "text/csv"),
    ("json", "application/json"),
    ("zip", "application/zip"),
];

fn mime_for_extension(extension: &str) -> Option<&'static str> {
    PHOTON_MIME_BY_EXTENSION
        .iter()
        .find(|(candidate, _)| *candidate == extension)
        .map(|(_, mime)| *mime)
}

fn extension_for_mime(mime: &str) -> Option<&'static str> {
    PHOTON_MIME_BY_EXTENSION
        .iter()
        .find(|(_, candidate)| *candidate == mime.trim())
        .map(|(extension, _)| *extension)
}

/// Lowercased extension after the last dot, when it looks like one (`1..=12` alphanumerics).
fn extension_of(name: &str) -> Option<String> {
    let (_, extension) = name.rsplit_once('.')?;
    if extension.is_empty()
        || extension.len() > 12
        || !extension.chars().all(|ch| ch.is_ascii_alphanumeric())
    {
        return None;
    }
    Some(extension.to_ascii_lowercase())
}

/// Reduce a caller filename to a safe basename.
///
/// Only the last path component survives, every character outside `[A-Za-z0-9._-]` folds to `_`, and
/// leading/trailing dots go, so `.`/`..`/`a/b` cannot escape the temp directory or turn the sidecar's
/// path into a different file. Over-long names are truncated with the extension kept. `None` when
/// nothing usable is left (the caller then gets a kind-based default name).
fn sanitize_attachment_name(raw: &str) -> Option<String> {
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
    if trimmed.is_empty() {
        return None;
    }
    Some(truncate_attachment_name(&trimmed))
}

/// Cap a basename at [`PHOTON_MAX_ATTACHMENT_NAME_CHARS`], keeping a plausible extension.
fn truncate_attachment_name(name: &str) -> String {
    if name.chars().count() <= PHOTON_MAX_ATTACHMENT_NAME_CHARS {
        return name.to_string();
    }
    match name.rsplit_once('.') {
        Some((stem, extension)) if extension.chars().count() <= 12 => {
            let room = PHOTON_MAX_ATTACHMENT_NAME_CHARS - extension.chars().count() - 1;
            let stem: String = stem.chars().take(room).collect();
            format!("{stem}.{extension}")
        }
        _ => name
            .chars()
            .take(PHOTON_MAX_ATTACHMENT_NAME_CHARS)
            .collect(),
    }
}

/// One temp file holding outbound bytes for the duration of a `/send-attachment` call.
///
/// The sidecar reads the file from disk while it uploads (`sidecar/index.mjs:1100-1119`), so it must
/// exist for the whole request and must not outlive it: [`Drop`] removes it on every exit path.
struct TempAttachment {
    path: PathBuf,
}

impl TempAttachment {
    /// Write `bytes` into a fresh, uniquely named file whose extension is `extension` — the one the
    /// advertised `name` carries, else the one the caller's mime implies — because `spectrum-ts`
    /// infers the attachment's type from the file extension (`sidecar/index.mjs:1108-1113`).
    fn write(extension: Option<&str>, bytes: &[u8]) -> std::io::Result<Self> {
        use std::io::Write as _;

        let mut last_collision: Option<std::io::Error> = None;
        for _ in 0..8 {
            let path = unique_attachment_path(extension);
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
            .unwrap_or_else(|| std::io::Error::other("could not create a unique attachment file")))
    }

    /// The path handed to the sidecar.
    fn path(&self) -> &Path {
        &self.path
    }
}

impl Drop for TempAttachment {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.path);
    }
}

/// `vikram-photon-<pid>-<sequence>-<nanos>[.ext]` in the OS temp dir: unique per call without
/// clobbering a name another send (or another process) is using.
fn unique_attachment_path(extension: Option<&str>) -> PathBuf {
    static SEQUENCE: AtomicU64 = AtomicU64::new(0);
    let sequence = SEQUENCE.fetch_add(1, Ordering::Relaxed);
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|elapsed| elapsed.as_nanos())
        .unwrap_or(0);
    let mut name = format!("vikram-photon-{}-{sequence}-{nanos}", std::process::id());
    if let Some(extension) = extension {
        name.push('.');
        name.push_str(extension);
    }
    std::env::temp_dir().join(name)
}

/// Where inbound sidecar events come from. The host supplies the real stream; tests supply a
/// scripted one.
#[async_trait]
pub trait SseSource: Send + Sync {
    /// One `data:` payload, or `None` when the stream ended.
    async fn next_event(&self) -> Result<Option<String>, String>;
}

/// The sidecar's SSE stream over HTTP. One `GET`, streamed chunk by chunk.
pub struct ReqwestSse {
    client: reqwest::Client,
    url: String,
    token: String,
    response: AsyncMutex<Option<reqwest::Response>>,
    buffer: StdMutex<String>,
}

impl ReqwestSse {
    pub fn connect(url: impl Into<String>, token: impl Into<String>) -> Result<Self, String> {
        let client = reqwest::Client::builder()
            .build()
            .map_err(|err| format!("photon sse client: {err}"))?;
        Ok(Self {
            client,
            url: url.into(),
            token: token.into(),
            response: AsyncMutex::new(None),
            buffer: StdMutex::new(String::new()),
        })
    }

    async fn ensure_open(&self) -> Result<(), String> {
        let mut slot = self.response.lock().await;
        if slot.is_none() {
            let response = self
                .client
                .get(&self.url)
                .header("X-Hermes-Sidecar-Token", &self.token)
                .header("Accept", "text/event-stream")
                .send()
                .await
                .map_err(|err| format!("photon sse connect: {err}"))?;
            if !response.status().is_success() {
                return Err(format!("photon sse returned {}", response.status()));
            }
            *slot = Some(response);
        }
        Ok(())
    }
}

/// Take one line off the buffer. Photon's sidecar streams **NDJSON** on `/inbound` (one JSON
/// object per line); the same reader also tolerates SSE `data:` framing so a sidecar that switches
/// to SSE does not break the adapter.
fn take_payload(buffer: &StdMutex<String>) -> Option<String> {
    let mut buffer = buffer
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    let boundary = buffer.find('\n')?;
    let line = buffer[..boundary].trim_end_matches('\r').to_string();
    buffer.drain(..boundary + 1);
    if line.is_empty()
        || line.starts_with(':')
        || line.starts_with("event:")
        || line.starts_with("id:")
        || line.starts_with("retry:")
    {
        return Some(String::new());
    }
    if let Some(value) = line.strip_prefix("data:") {
        return Some(value.trim_start().to_string());
    }
    Some(line)
}

#[async_trait]
impl SseSource for ReqwestSse {
    async fn next_event(&self) -> Result<Option<String>, String> {
        loop {
            if let Some(payload) = take_payload(&self.buffer) {
                return Ok(Some(payload));
            }
            self.ensure_open().await?;
            let mut slot = self.response.lock().await;
            let Some(response) = slot.as_mut() else {
                return Ok(None);
            };
            match response
                .chunk()
                .await
                .map_err(|err| format!("photon sse read: {err}"))?
            {
                Some(bytes) => {
                    let text = String::from_utf8_lossy(&bytes);
                    self.buffer
                        .lock()
                        .unwrap_or_else(|poisoned| poisoned.into_inner())
                        .push_str(&text);
                }
                None => {
                    *slot = None;
                    return Ok(None);
                }
            }
        }
    }
}

impl PhotonAdapter {
    /// Normalize one sidecar inbound event. Non-message events yield no event.
    pub fn parse_sidecar_event(&self, payload: &str) -> Result<Vec<MessageEvent>, String> {
        if payload.trim().is_empty() {
            return Ok(Vec::new());
        }
        let event: Value =
            serde_json::from_str(payload).map_err(|err| format!("invalid sidecar event: {err}"))?;
        let space = event.get("space");
        let Some(space_id) = space
            .and_then(|space| space.get("id"))
            .and_then(Value::as_str)
            .filter(|id| !id.is_empty())
        else {
            return Ok(Vec::new());
        };
        let chat_type =
            if space.and_then(|s| s.get("type")).and_then(Value::as_str) == Some("group") {
                "group"
            } else {
                "dm"
            };
        let mut content = event.get("content").cloned().unwrap_or(Value::Null);
        let sender_id = event
            .get("sender")
            .and_then(|sender| sender.get("id"))
            .and_then(Value::as_str)
            .filter(|id| !id.is_empty())
            .or_else(|| space.and_then(|s| s.get("phone")).and_then(Value::as_str))
            .unwrap_or(space_id)
            .to_string();
        let message_id = event
            .get("messageId")
            .and_then(Value::as_str)
            .map(str::to_owned);

        // A threaded reply wraps the real content; keep the quoted id and unwrap once.
        let mut reply_to = None;
        if content.get("type").and_then(Value::as_str) == Some("reply") {
            reply_to = content
                .get("targetMessageId")
                .and_then(Value::as_str)
                .map(str::to_owned);
            if let Some(inner) = content.get("content").cloned() {
                content = inner;
            }
        }

        let kind = content
            .get("type")
            .and_then(Value::as_str)
            .unwrap_or_default();
        let text = match kind {
            "read" | "read_receipt" => return Ok(Vec::new()),
            "reaction" => {
                // Only a tapback on something we sent is addressed to the bot.
                if content.get("targetDirection").and_then(Value::as_str) != Some("outbound") {
                    return Ok(Vec::new());
                }
                let emoji = content
                    .get("emoji")
                    .and_then(Value::as_str)
                    .unwrap_or_default();
                format!("reaction:added:{emoji}")
            }
            "text" => content
                .get("text")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string(),
            "attachment" | "voice" => content
                .get("text")
                .and_then(Value::as_str)
                .map(str::to_owned)
                .unwrap_or_else(|| format!("[{kind}]")),
            "richlink" | "group" | "poll_option" => content
                .get("text")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string(),
            _ => return Ok(Vec::new()),
        };
        if text.trim().is_empty() {
            return Ok(Vec::new());
        }

        let platform = Platform::new("photon").expect("valid platform");
        let mut source = SessionSource::new(platform, space_id.to_string());
        source.chat_type = chat_type.to_string();
        source.user_id = Some(sender_id.clone());
        source.user_name = Some(sender_id.clone());
        source.message_id = message_id.clone();

        let mut message = MessageEvent::new(text, source);
        message.user_id = Some(sender_id);
        message.message_id = message_id;
        message.reply_to_message_id = reply_to;
        message.raw_message = Some(event.clone());
        if let Some(mime) = content.get("mime").and_then(Value::as_str) {
            message.media_types.push(mime.to_string());
        }
        if kind == "voice" {
            message.message_type = crate::event::MessageType::Voice;
        } else if kind == "attachment" && message.media_types.is_empty() {
            message.message_type = crate::event::MessageType::Document;
        }
        Ok(vec![message])
    }

    /// Pull the stream until one payload carries a message.
    pub async fn next_event(&self, source: &dyn SseSource) -> Result<Option<MessageEvent>, String> {
        loop {
            let Some(payload) = source.next_event().await? else {
                return Ok(None);
            };
            let events = self.parse_sidecar_event(&payload)?;
            if let Some(event) = events.into_iter().next() {
                return Ok(Some(event));
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::transport::{HttpMethod, HttpRequest, HttpResponse, Transport, TransportError};
    use async_trait::async_trait;
    use std::collections::VecDeque;
    use std::sync::{Arc, Mutex};

    struct MockTransport {
        requests: Mutex<Vec<HttpRequest>>,
        responses: Mutex<VecDeque<HttpResponse>>,
        /// `(path, existed)` for every request whose body carried a `/send-attachment` `path`,
        /// sampled at request time: proves the temp file was on disk for the call and is gone after.
        attachment_paths: Mutex<Vec<(String, bool)>>,
    }

    impl MockTransport {
        fn new(responses: Vec<HttpResponse>) -> Arc<Self> {
            Arc::new(Self {
                requests: Mutex::new(Vec::new()),
                responses: Mutex::new(responses.into()),
                attachment_paths: Mutex::new(Vec::new()),
            })
        }
        fn requests(&self) -> Vec<HttpRequest> {
            self.requests.lock().unwrap().clone()
        }
        fn attachment_paths(&self) -> Vec<(String, bool)> {
            self.attachment_paths.lock().unwrap().clone()
        }
    }

    #[async_trait]
    impl Transport for MockTransport {
        async fn execute(&self, request: HttpRequest) -> Result<HttpResponse, TransportError> {
            let attachment = request
                .body
                .as_deref()
                .and_then(|body| serde_json::from_slice::<Value>(body).ok())
                .and_then(|value| value.get("path").and_then(Value::as_str).map(str::to_owned));
            if let Some(path) = attachment {
                let existed = Path::new(&path).exists();
                self.attachment_paths.lock().unwrap().push((path, existed));
            }
            self.requests.lock().unwrap().push(request);
            self.responses
                .lock()
                .unwrap()
                .pop_front()
                .ok_or_else(|| TransportError::Request("no scripted response".into()))
        }
    }

    fn build(responses: Vec<HttpResponse>) -> (PhotonAdapter, Arc<MockTransport>) {
        let mock = MockTransport::new(responses);
        let adapter = PhotonAdapter::new(
            Arc::clone(&mock) as SharedTransport,
            "http://127.0.0.1:8789",
            "tok",
        );
        (adapter, mock)
    }

    fn sent(id: &str) -> HttpResponse {
        HttpResponse {
            status: 200,
            body: json!({"ok": true, "messageId": id}).to_string(),
            retry_after: None,
        }
    }

    fn body_of(request: &HttpRequest) -> Value {
        serde_json::from_slice(request.body.as_ref().expect("body")).expect("json")
    }

    #[tokio::test]
    async fn connect_probes_health_with_the_sidecar_token() {
        let (adapter, mock) = build(vec![HttpResponse {
            status: 200,
            body: "{}".into(),
            retry_after: None,
        }]);
        let mut adapter = adapter;
        assert!(adapter.connect(false).await);
        let requests = mock.requests();
        assert_eq!(requests[0].url, "http://127.0.0.1:8789/health");
        assert!(
            requests[0]
                .headers
                .iter()
                .any(|(name, value)| name == "X-Hermes-Sidecar-Token" && value == "tok")
        );
    }

    #[tokio::test]
    async fn send_posts_the_space_and_markdown_text() {
        let (adapter, mock) = build(vec![sent("m1")]);
        let result = adapter.send("space-1", "hello", None, None).await;
        assert!(result.success);
        assert_eq!(result.message_id.as_deref(), Some("m1"));
        let requests = mock.requests();
        assert_eq!(requests[0].url, "http://127.0.0.1:8789/send");
        let body = body_of(&requests[0]);
        assert_eq!(body["spaceId"], "space-1");
        assert_eq!(body["text"], "hello");
        assert_eq!(body["format"], "markdown");
    }

    #[tokio::test]
    async fn long_text_is_chunked_within_the_cap() {
        let content = "x".repeat(PHOTON_MAX_MESSAGE_LENGTH + 50);
        let (adapter, mock) = build(vec![sent("a"), sent("b")]);
        let result = adapter.send("s", &content, None, None).await;
        assert!(result.success);
        assert_eq!(result.continuation_message_ids, vec!["a", "b"]);
        for request in mock.requests() {
            assert!(
                body_of(&request)["text"].as_str().unwrap().chars().count()
                    <= PHOTON_MAX_MESSAGE_LENGTH
            );
        }
    }

    #[tokio::test]
    async fn a_sidecar_refusal_is_a_failure() {
        let (adapter, _) = build(vec![HttpResponse {
            status: 200,
            body: json!({"ok": false, "error": "not connected"}).to_string(),
            retry_after: None,
        }]);
        let result = adapter.send("s", "hi", None, None).await;
        assert!(!result.success);
        assert_eq!(result.error.as_deref(), Some("not connected"));
    }

    #[tokio::test]
    async fn rate_limits_and_auth_failures_are_classified() {
        let (adapter, _) = build(vec![HttpResponse {
            status: 429,
            body: String::new(),
            retry_after: Some(5.0),
        }]);
        let limited = adapter.send("s", "hi", None, None).await;
        assert!(limited.retryable);
        assert_eq!(limited.retry_after, Some(5.0));

        let (adapter, _) = build(vec![HttpResponse {
            status: 401,
            body: "no".into(),
            retry_after: None,
        }]);
        assert_eq!(
            adapter
                .send("s", "hi", None, None)
                .await
                .error_kind
                .as_deref(),
            Some("forbidden")
        );
    }

    #[tokio::test]
    async fn blank_inputs_make_no_request() {
        let (adapter, mock) = build(vec![]);
        assert!(!adapter.send("", "hi", None, None).await.success);
        assert!(!adapter.send("s", "  ", None, None).await.success);
        assert!(mock.requests().is_empty());
    }

    #[tokio::test]
    async fn chat_info_and_typing_are_local() {
        let (adapter, mock) = build(vec![]);
        let info = adapter.get_chat_info("space-1").await;
        assert_eq!(info.chat_type, "dm");
        adapter.send_typing("space-1", None).await;
        assert!(mock.requests().is_empty());
    }

    // ----- outbound media (`supports_media` / `send_media`) -----

    fn bytes_media(kind: MediaKind, bytes: &[u8]) -> OutboundMedia {
        OutboundMedia::new(kind, MediaSource::Bytes(bytes.to_vec()))
    }

    fn attachment_body(mock: &MockTransport) -> Value {
        let requests = mock.requests();
        assert_eq!(requests.len(), 1, "expected exactly one request");
        assert_eq!(requests[0].method, HttpMethod::Post);
        assert_eq!(requests[0].url, "http://127.0.0.1:8789/send-attachment");
        assert!(
            requests[0]
                .headers
                .iter()
                .any(|(name, value)| name == "X-Hermes-Sidecar-Token" && value == "tok"),
            "sidecar token header missing"
        );
        assert!(
            requests[0]
                .headers
                .iter()
                .any(|(name, value)| name == "Content-Type" && value == "application/json"),
            "json content type missing"
        );
        body_of(&requests[0])
    }

    #[tokio::test]
    async fn media_is_supported() {
        let (adapter, mock) = build(vec![]);
        assert!(adapter.supports_media());
        assert!(mock.requests().is_empty());
    }

    #[tokio::test]
    async fn image_bytes_travel_as_a_sanitised_temp_path_that_is_cleaned_up() {
        let (adapter, mock) = build(vec![sent("att-1")]);
        let media = bytes_media(MediaKind::Image, b"png-bytes")
            .with_filename("../../etc/passwd name.png")
            .with_mime("image/png")
            .with_caption("  look at this  ");
        let result = adapter.send_media("space-1", &media, None).await;
        assert!(result.success);
        assert_eq!(result.message_id.as_deref(), Some("att-1"));

        let body = attachment_body(&mock);
        assert_eq!(body["spaceId"], "space-1");
        assert_eq!(body["kind"], "attachment");
        // The caller's path components and spaces never reach the sidecar path or the name.
        assert_eq!(body["name"], "passwd_name.png");
        assert_eq!(body["mimeType"], "image/png");
        assert_eq!(body["caption"], "look at this");
        // The reference body carries a local path, never inline base64 (`adapter.py:557-564`).
        assert!(body.get("data").is_none());
        let path = body["path"].as_str().expect("path");
        assert!(
            path.ends_with(".png"),
            "extension kept for spectrum's mime inference: {path}"
        );
        assert!(
            Path::new(path).starts_with(std::env::temp_dir()),
            "attachment must live in the temp dir: {path}"
        );
        assert!(!path.contains("passwd name"), "unsanitised name: {path}");

        let recorded = mock.attachment_paths();
        assert_eq!(recorded.len(), 1);
        assert!(
            recorded[0].1,
            "the file has to exist while the sidecar reads it"
        );
        assert!(
            !Path::new(path).exists(),
            "temp attachment must be removed after the send: {path}"
        );
    }

    #[tokio::test]
    async fn each_call_gets_its_own_temp_file() {
        let (adapter, mock) = build(vec![sent("att-1"), sent("att-2")]);
        let media = bytes_media(MediaKind::Document, b"%PDF-1.7").with_filename("report.pdf");
        assert!(adapter.send_media("space-1", &media, None).await.success);
        assert!(adapter.send_media("space-1", &media, None).await.success);
        let requests = mock.requests();
        let first = body_of(&requests[0])["path"]
            .as_str()
            .map(str::to_owned)
            .expect("path");
        let second = body_of(&requests[1])["path"]
            .as_str()
            .map(str::to_owned)
            .expect("path");
        assert_ne!(first, second, "concurrent sends must not share a file");
        assert!(!Path::new(&first).exists());
        assert!(!Path::new(&second).exists());
    }

    #[tokio::test]
    async fn audio_is_a_voice_note_and_everything_else_is_a_plain_attachment() {
        for (kind, expected) in [
            (MediaKind::Audio, "voice"),
            (MediaKind::Image, "attachment"),
            (MediaKind::Video, "attachment"),
            (MediaKind::Document, "attachment"),
        ] {
            let (adapter, mock) = build(vec![sent("m")]);
            let media = bytes_media(kind, b"payload").with_filename("clip.mp3");
            assert!(adapter.send_media("space-1", &media, None).await.success);
            let body = attachment_body(&mock);
            assert_eq!(body["kind"], expected, "{kind:?}");
            assert_eq!(body["spaceId"], "space-1");
            assert_eq!(body["name"], "clip.mp3");
        }
    }

    #[tokio::test]
    async fn mime_is_guessed_from_the_filename_or_omitted_for_the_sidecar_to_infer() {
        let (adapter, mock) = build(vec![sent("m")]);
        let guessed = bytes_media(MediaKind::Document, b"%PDF-1.7").with_filename("report.PDF");
        assert!(adapter.send_media("space-1", &guessed, None).await.success);
        assert_eq!(attachment_body(&mock)["mimeType"], "application/pdf");

        // No mime and no usable extension: the key is left out so spectrum-ts infers the type
        // (`sidecar/index.mjs:1108-1113`), exactly like the reference's optional keys.
        let (adapter, mock) = build(vec![sent("m")]);
        let unknown = bytes_media(MediaKind::Document, b"blob").with_filename("blob");
        assert!(adapter.send_media("space-1", &unknown, None).await.success);
        let body = attachment_body(&mock);
        assert!(body.get("mimeType").is_none(), "{body}");
        assert_eq!(body["name"], "blob");

        // A name without an extension but with a caller mime still gets one on disk, because
        // spectrum-ts infers the type from the extension (`sidecar/index.mjs:1108-1113`).
        let (adapter, mock) = build(vec![sent("m")]);
        let bare = bytes_media(MediaKind::Audio, b"ID3")
            .with_filename("note")
            .with_mime("audio/mp4");
        assert!(adapter.send_media("space-1", &bare, None).await.success);
        let body = attachment_body(&mock);
        assert_eq!(body["name"], "note");
        assert_eq!(body["mimeType"], "audio/mp4");
        assert!(
            body["path"].as_str().unwrap_or_default().ends_with(".m4a"),
            "{body}"
        );
        assert_eq!(body["kind"], "voice");
    }

    #[tokio::test]
    async fn a_blank_caption_is_left_out_of_the_body() {
        let (adapter, mock) = build(vec![sent("m")]);
        let media = bytes_media(MediaKind::Image, b"png").with_caption("   \n ");
        assert!(adapter.send_media("space-1", &media, None).await.success);
        let body = attachment_body(&mock);
        assert!(body.get("caption").is_none(), "{body}");
    }

    #[tokio::test]
    async fn reply_to_is_not_forwarded_because_the_sidecar_has_no_reply_field() {
        let (adapter, mock) = build(vec![sent("m")]);
        let media = bytes_media(MediaKind::Image, b"png")
            .with_filename("a.png")
            .with_mime("image/png")
            .with_caption("cap");
        assert!(
            adapter
                .send_media("space-1", &media, Some("quoted-1"))
                .await
                .success
        );
        let body = attachment_body(&mock);
        let keys: Vec<&String> = body
            .as_object()
            .expect("object body")
            .keys()
            .collect::<Vec<_>>();
        assert_eq!(
            keys,
            vec!["caption", "kind", "mimeType", "name", "path", "spaceId"],
            "the handler accepts only spaceId/path/name/mimeType/caption/kind (index.mjs:1100-1102)"
        );
        assert!(!body.to_string().contains("quoted-1"));
    }

    #[tokio::test]
    async fn empty_bytes_are_refused_before_the_wire() {
        let (adapter, mock) = build(vec![]);
        let result = adapter
            .send_media("space-1", &bytes_media(MediaKind::Image, b""), None)
            .await;
        assert!(!result.success);
        assert_eq!(result.error_kind.as_deref(), Some("bad_format"));
        assert!(mock.requests().is_empty());
    }

    #[tokio::test]
    async fn a_blank_space_id_is_refused_before_the_wire() {
        let (adapter, mock) = build(vec![]);
        let result = adapter
            .send_media("  ", &bytes_media(MediaKind::Image, b"png"), None)
            .await;
        assert!(!result.success);
        assert_eq!(result.error_kind.as_deref(), Some("bad_format"));
        assert!(mock.requests().is_empty());
    }

    #[tokio::test]
    async fn url_media_is_refused_truthfully_with_zero_requests() {
        let (adapter, mock) = build(vec![]);
        let media = OutboundMedia::image_url("https://example.test/a.png").with_caption("remote");
        let result = adapter.send_media("space-1", &media, None).await;
        assert!(!result.success);
        assert_eq!(result.error_kind.as_deref(), Some("bad_format"));
        let error = result.error.unwrap_or_default();
        assert!(error.contains("MediaSource::Bytes"), "{error}");
        assert!(error.contains("https://example.test/a.png"), "{error}");
        assert!(mock.requests().is_empty(), "no fetch-through is attempted");
    }

    #[tokio::test]
    async fn only_the_success_envelope_counts_as_delivered() {
        // `{"ok": true}` with no id: delivered, but unidentified (`index.mjs:923-927`).
        let (adapter, _) = build(vec![HttpResponse {
            status: 200,
            body: json!({"ok": true}).to_string(),
            retry_after: None,
        }]);
        let result = adapter
            .send_media("space-1", &bytes_media(MediaKind::Image, b"png"), None)
            .await;
        assert!(result.success);
        assert_eq!(result.message_id.as_deref(), Some(""));

        // HTTP 200 with `{"ok": false, ...}` is the sidecar's refusal envelope.
        let (adapter, mock) = build(vec![HttpResponse {
            status: 200,
            body: json!({"ok": false, "error": "attachment failed"}).to_string(),
            retry_after: None,
        }]);
        let result = adapter
            .send_media("space-1", &bytes_media(MediaKind::Image, b"png"), None)
            .await;
        assert!(!result.success);
        assert_eq!(result.error.as_deref(), Some("attachment failed"));
        // The temp file is gone even when the sidecar refuses.
        for (path, _) in mock.attachment_paths() {
            assert!(!Path::new(&path).exists(), "leaked temp file: {path}");
        }
    }

    #[tokio::test]
    async fn failure_statuses_map_into_the_shared_taxonomy() {
        for (status, kind, retryable) in [
            (429, "rate_limited", true),
            (413, "too_long", false),
            (401, "forbidden", false),
            (403, "forbidden", false),
            (404, "not_found", false),
            (500, "transient", true),
            (503, "transient", true),
        ] {
            let (adapter, mock) = build(vec![HttpResponse {
                status,
                body: "nope".into(),
                retry_after: (status == 429).then_some(7.0),
            }]);
            let result = adapter
                .send_media(
                    "space-1",
                    &bytes_media(MediaKind::Image, b"png").with_filename("a.png"),
                    None,
                )
                .await;
            assert!(!result.success, "{status} must fail");
            assert_eq!(result.error_kind.as_deref(), Some(kind), "{status}");
            assert_eq!(result.retryable, retryable, "{status}");
            if status == 429 {
                assert_eq!(result.retry_after, Some(7.0));
            }
            assert_eq!(result.message_id, None, "{status}");
            for (path, existed) in mock.attachment_paths() {
                assert!(existed, "{status}: file missing at request time");
                assert!(!Path::new(&path).exists(), "{status}: leaked {path}");
            }
        }
    }

    #[tokio::test]
    async fn transport_failures_are_retryable_and_clean_up() {
        // No scripted response: the mock answers with a transport error.
        let (adapter, mock) = build(vec![]);
        let result = adapter
            .send_media("space-1", &bytes_media(MediaKind::Document, b"doc"), None)
            .await;
        assert!(!result.success);
        assert_eq!(result.error_kind.as_deref(), Some("transient"));
        assert!(result.retryable);
        assert!(
            result
                .error
                .unwrap_or_default()
                .contains("photon sidecar transport error")
        );
        for (path, _) in mock.attachment_paths() {
            assert!(!Path::new(&path).exists(), "leaked temp file: {path}");
        }
    }

    #[tokio::test]
    async fn an_oversized_control_body_is_refused_locally() {
        let (adapter, mock) = build(vec![]);
        let caption = "c".repeat(PHOTON_MAX_ATTACHMENT_BODY_BYTES + 1);
        let media = bytes_media(MediaKind::Image, b"png").with_caption(caption);
        let result = adapter.send_media("space-1", &media, None).await;
        assert!(!result.success);
        assert_eq!(result.error_kind.as_deref(), Some("too_long"));
        assert!(
            result
                .error
                .unwrap_or_default()
                .contains("control-body cap"),
            "the refusal cites the sidecar's cap"
        );
        assert!(
            mock.requests().is_empty(),
            "no body above 2 MiB may reach the sidecar"
        );
    }

    #[test]
    fn attachment_names_are_sanitised() {
        assert_eq!(
            sanitize_attachment_name("../../etc/passwd").as_deref(),
            Some("passwd")
        );
        assert_eq!(
            sanitize_attachment_name("C:\\Users\\me\\notes.txt").as_deref(),
            Some("notes.txt")
        );
        assert_eq!(
            sanitize_attachment_name("my photo (1).png").as_deref(),
            Some("my_photo__1_.png")
        );
        assert_eq!(sanitize_attachment_name(".."), None);
        assert_eq!(sanitize_attachment_name(""), None);
        let long = format!("{}.png", "x".repeat(400));
        let capped = sanitize_attachment_name(&long).expect("name");
        assert_eq!(capped.chars().count(), PHOTON_MAX_ATTACHMENT_NAME_CHARS);
        assert!(capped.ends_with(".png"));
    }

    #[test]
    fn a_missing_filename_falls_back_to_a_kind_based_name() {
        let audio = OutboundMedia::new(MediaKind::Audio, MediaSource::Bytes(vec![1]))
            .with_mime("audio/mp4");
        assert_eq!(attachment_name(&audio), "audio.m4a");
        let document = bytes_media(MediaKind::Document, b"x");
        assert_eq!(attachment_name(&document), "attachment");
        let video = OutboundMedia::new(MediaKind::Video, MediaSource::Bytes(vec![1]))
            .with_filename("../")
            .with_mime("video/mp4");
        assert_eq!(attachment_name(&video), "video.mp4");
    }
}

#[cfg(test)]
mod sse_tests {
    use super::*;
    use crate::transport::{HttpResponse, Transport, TransportError};
    use std::sync::Arc;

    struct NoTransport;

    #[async_trait]
    impl Transport for NoTransport {
        async fn execute(
            &self,
            _request: crate::transport::HttpRequest,
        ) -> Result<HttpResponse, TransportError> {
            Err(TransportError::Request("unused".into()))
        }
    }

    fn adapter() -> PhotonAdapter {
        PhotonAdapter::new(
            Arc::new(NoTransport) as SharedTransport,
            "http://127.0.0.1:8789",
            "tok",
        )
    }

    struct MockSse {
        payloads: StdMutex<Vec<String>>,
    }

    impl MockSse {
        fn new(payloads: Vec<String>) -> Self {
            Self {
                payloads: StdMutex::new(payloads),
            }
        }
    }

    #[async_trait]
    impl SseSource for MockSse {
        async fn next_event(&self) -> Result<Option<String>, String> {
            let next = {
                let mut payloads = self.payloads.lock().unwrap();
                if payloads.is_empty() {
                    None
                } else {
                    Some(payloads.remove(0))
                }
            };
            Ok(next)
        }
    }

    #[test]
    fn ndjson_and_sse_lines_are_read_one_at_a_time() {
        let buffer = StdMutex::new(String::new());
        {
            let mut text = buffer.lock().unwrap();
            text.push_str(": keepalive\n");
            text.push_str("event: message\n");
            text.push_str("data: {\"a\":1}\n");
            text.push_str("{\"b\":2}\n");
            text.push_str("partial");
        }
        // Metadata lines and comments produce no payload; the caller skips them.
        assert_eq!(take_payload(&buffer).as_deref(), Some(""));
        assert_eq!(take_payload(&buffer).as_deref(), Some(""));
        assert_eq!(take_payload(&buffer).as_deref(), Some("{\"a\":1}"));
        // A bare JSON line is the sidecar's real NDJSON shape.
        assert_eq!(take_payload(&buffer).as_deref(), Some("{\"b\":2}"));
        // An unterminated line is held until its newline arrives.
        assert!(take_payload(&buffer).is_none());
        buffer.lock().unwrap().push('\n');
        assert_eq!(take_payload(&buffer).as_deref(), Some("partial"));
        assert!(take_payload(&buffer).is_none());
    }

    #[test]
    fn a_text_event_becomes_a_direct_message() {
        let payload = json!({
            "messageId": "m1",
            "space": {"id": "space-1", "type": "dm", "phone": "+1555"},
            "sender": {"id": "person-1"},
            "content": {"type": "text", "text": "hello photon"},
            "timestamp": "1700000000"
        })
        .to_string();
        let events = adapter().parse_sidecar_event(&payload).unwrap();
        assert_eq!(events.len(), 1);
        let event = &events[0];
        assert_eq!(event.text, "hello photon");
        assert_eq!(event.source.chat_id, "space-1");
        assert_eq!(event.source.chat_type, "dm");
        assert_eq!(event.user_id.as_deref(), Some("person-1"));
        assert_eq!(event.message_id.as_deref(), Some("m1"));
    }

    #[test]
    fn group_and_reply_and_media_fields_are_kept() {
        let payload = json!({
            "messageId": "m2",
            "space": {"id": "space-2", "type": "group"},
            "sender": {"id": "person-2"},
            "content": {
                "type": "reply",
                "targetMessageId": "m1",
                "content": {"type": "attachment", "mime": "image/png"}
            }
        })
        .to_string();
        let events = adapter().parse_sidecar_event(&payload).unwrap();
        assert_eq!(events.len(), 1);
        let event = &events[0];
        assert_eq!(event.source.chat_type, "group");
        assert_eq!(event.reply_to_message_id.as_deref(), Some("m1"));
        assert_eq!(event.text, "[attachment]");
        assert_eq!(event.media_types, vec!["image/png"]);
    }

    #[test]
    fn receipts_and_inbound_reactions_are_not_turns() {
        let adapter = adapter();
        for payload in [
            json!({"space":{"id":"s"},"content":{"type":"read"}}),
            json!({"space":{"id":"s"},"content":{"type":"reaction","emoji":"❤️","targetDirection":"inbound"}}),
            json!({"content":{"type":"text","text":"no space"}}),
            json!({"space":{"id":"s"},"content":{"type":"unknown"}}),
        ] {
            assert!(
                adapter
                    .parse_sidecar_event(&payload.to_string())
                    .unwrap()
                    .is_empty(),
                "{payload}"
            );
        }

        let outbound = json!({
            "space": {"id": "s"},
            "sender": {"id": "u"},
            "content": {"type": "reaction", "emoji": "👍", "targetDirection": "outbound"}
        })
        .to_string();
        let events = adapter.parse_sidecar_event(&outbound).unwrap();
        assert_eq!(events[0].text, "reaction:added:👍");

        assert!(adapter.parse_sidecar_event("not json").is_err());
        assert!(adapter.parse_sidecar_event("").unwrap().is_empty());
    }

    #[tokio::test]
    async fn next_event_skips_keepalives_and_receipts() {
        let adapter = adapter();
        let read = json!({"space":{"id":"s"},"content":{"type":"read"}}).to_string();
        let text = json!({
            "messageId": "m9",
            "space": {"id": "space-9", "type": "dm"},
            "sender": {"id": "u9"},
            "content": {"type": "text", "text": "after the noise"}
        })
        .to_string();
        let source = MockSse::new(vec![String::new(), read, text]);
        let event = adapter.next_event(&source).await.unwrap().expect("event");
        assert_eq!(event.text, "after the noise");
        assert!(
            adapter
                .next_event(&MockSse::new(vec![]))
                .await
                .unwrap()
                .is_none()
        );
    }
}
