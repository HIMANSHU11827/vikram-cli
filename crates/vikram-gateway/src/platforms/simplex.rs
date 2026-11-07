//! SimpleX Chat adapter — the `simplex-chat` daemon WebSocket API.
//!
//! The daemon accepts JSON commands (`{"corrId": "…", "cmd": "…"}`) over a WebSocket and pushes
//! events back on the same socket. Text is sent with `/_send <target> json <items>`; inbound
//! `newChatItem` events are normalized into gateway events.
//!
//! Transport is injected as a [`LineSocket`](crate::socket::LineSocket); [`WsSocket`] is the real
//! implementation for `ws://127.0.0.1:5225`. Outbound attachments ride the same `/_send` command
//! (see [`PlatformAdapter::send_media`]). Not ported: inbound file/voice transfers (`/freceive`),
//! contact requests/auto-accept, and the group allowlist (access decisions live in `access.rs`).

use async_trait::async_trait;
use serde_json::{Value, json};
use std::fs::OpenOptions;
use std::io::{ErrorKind, Write};
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use crate::event::{MessageEvent, Platform, SessionSource};
use crate::helpers::{codepoint_len, truncate_message};
use crate::platform::{
    ChatInfo, MediaKind, MediaSource, OutboundMedia, PlatformAdapter, SendResult,
};
use crate::socket::SharedLineSocket;

pub const SIMPLEX_MAX_MESSAGE_LENGTH: usize = 8000;

/// `duration` sent on an inline voice note when the caller did not measure one.
///
/// The reference's `send_voice` takes `duration: int = 0` and always serialises it
/// (`adapter.py:591-598`), so an unknown length travels as `0` rather than as a missing field.
pub const SIMPLEX_VOICE_DURATION_UNKNOWN: u32 = 0;

/// Longest filename component this adapter will stage.
///
/// The daemon takes the attachment's display name from the `filePath` basename (that is the only
/// name carrier `/_send … json` has, `adapter.py:108-112`), so it must stay one path component and
/// must stay short enough that the staged path cannot run past a platform path limit.
const MAX_STAGED_NAME_CHARS: usize = 96;

/// How many times a temp-file name collision is retried before the send is reported as failed.
const TEMP_FILE_ATTEMPTS: u8 = 3;

/// Prefix on every staged media path, so a leaked file is traceable to this adapter.
const STAGED_PREFIX: &str = "vikram-simplex";

static CORR_SEQ: AtomicU64 = AtomicU64::new(0);

pub struct SimplexAdapter {
    socket: SharedLineSocket,
    connected: bool,
}

impl SimplexAdapter {
    pub fn new(socket: SharedLineSocket) -> Self {
        Self {
            socket: Arc::clone(&socket),
            connected: false,
        }
    }

    fn corr_id() -> String {
        format!(
            "vikram-{}-{}",
            std::process::id(),
            CORR_SEQ.fetch_add(1, Ordering::Relaxed)
        )
    }

    /// The daemon's target syntax: `@contactId` for a contact, `#groupId` for a group.
    fn target(chat_id: &str) -> String {
        match chat_id.strip_prefix("group:") {
            Some(id) => format!("#{id}"),
            None => format!("@{chat_id}"),
        }
    }

    async fn send_json(&self, value: &Value) -> Result<(), String> {
        let line = serde_json::to_string(value).map_err(|err| err.to_string())?;
        self.socket
            .send_line(&line)
            .await
            .map_err(|err| err.to_string())
    }

    /// Read the next inbound message. Commands/responses without a chat item are skipped.
    pub async fn next_event(&self) -> Result<Option<MessageEvent>, String> {
        loop {
            let Some(line) = self
                .socket
                .next_line()
                .await
                .map_err(|err| err.to_string())?
            else {
                return Ok(None);
            };
            let Ok(value) = serde_json::from_str::<Value>(&line) else {
                continue;
            };
            if let Some(event) = parse_event(&value) {
                return Ok(Some(event));
            }
        }
    }
}

fn parse_event(root: &Value) -> Option<MessageEvent> {
    let body = root.get("resp").unwrap_or(root);
    let kind = body.get("type").and_then(Value::as_str)?;
    let chat_item = match kind {
        "newChatItem" => body.get("chatItem")?.clone(),
        "newChatItems" => body
            .get("chatItems")
            .and_then(Value::as_array)
            .and_then(|items| items.first())
            .cloned()?,
        _ => return None,
    };

    let chat_info = chat_item.get("chatInfo")?;
    let data = chat_item.get("chatItem")?;
    let direction = data
        .get("chatDir")
        .and_then(|dir| dir.get("type"))
        .and_then(Value::as_str)
        .unwrap_or_default();
    if direction == "directSnd" || direction == "groupSnd" {
        return None;
    }
    let content = data.get("content")?;
    if content.get("type").and_then(Value::as_str) != Some("rcvMsgContent") {
        return None;
    }
    let msg = content.get("msgContent")?;
    if msg.get("type").and_then(Value::as_str) != Some("text") {
        return None;
    }
    let text = msg
        .get("text")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_string();
    if text.trim().is_empty() {
        return None;
    }

    let chat_type = chat_info
        .get("type")
        .and_then(Value::as_str)
        .unwrap_or_default();
    let (chat_id, user_id, user_name, kind) = match chat_type {
        "direct" => {
            let contact = chat_info.get("contact")?;
            let id = contact.get("contactId")?.as_i64().map(|n| n.to_string())?;
            let name = display_name(contact, "profile").unwrap_or_else(|| id.clone());
            (id.clone(), id, name, "dm")
        }
        "group" => {
            let group = chat_info.get("groupInfo")?;
            let id = group.get("groupId")?.as_i64().map(|n| n.to_string())?;
            let member = data.get("chatDir").and_then(|dir| dir.get("groupMember"));
            let member_id = member
                .and_then(|m| m.get("memberId"))
                .and_then(Value::as_i64)
                .map(|n| n.to_string())?;
            let member_name = member
                .and_then(|m| display_name(m, "memberProfile"))
                .unwrap_or_else(|| member_id.clone());
            (format!("group:{id}"), member_id, member_name, "group")
        }
        _ => return None,
    };

    let platform = Platform::new("simplex").expect("valid platform");
    let mut source = SessionSource::new(platform, chat_id);
    source.chat_type = kind.to_string();
    source.user_id = Some(user_id.clone());
    source.user_name = Some(user_name.clone());
    let mut event = MessageEvent::new(text, source);
    event.user_id = Some(user_id);
    event.user_name = Some(user_name);
    event.raw_message = Some(root.clone());
    Some(event)
}

fn display_name(object: &Value, profile_key: &str) -> Option<String> {
    if let Some(name) = object.get("localDisplayName").and_then(Value::as_str)
        && !name.is_empty()
    {
        return Some(name.to_string());
    }
    object
        .get(profile_key)
        .and_then(|profile| profile.get("displayName"))
        .and_then(Value::as_str)
        .filter(|name| !name.is_empty())
        .map(str::to_owned)
}

// ----- outbound media helpers -----

/// The `msgContent.type` one outbound attachment travels as.
///
/// The mapping is the reference's: images are `image` (`adapter.py:572`), documents are `file` and
/// videos reuse that path — `send_video` simply delegates to `send_document` (`adapter.py:579-589`)
/// — and audio is an inline `voice` note (`adapter.py:596`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum WireKind {
    Image,
    File,
    Voice,
}

impl WireKind {
    /// The `msgContent.type` string the daemon expects.
    fn msg_type(self) -> &'static str {
        match self {
            Self::Image => "image",
            Self::File => "file",
            Self::Voice => "voice",
        }
    }
}

/// An image container SimpleX clients render inline.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum InlineImage {
    Png,
    Jpeg,
}

impl InlineImage {
    /// The filename extension to stage the payload under.
    fn extension(self) -> &'static str {
        match self {
            Self::Png => "png",
            Self::Jpeg => "jpg",
        }
    }
}

/// Recognise a PNG or JPEG payload by its magic bytes.
///
/// These are exactly the two containers `_prepare_image` leaves alone (`adapter.py:516`, whose
/// `needs_png` check is suffix-based); every other image format is one the reference would
/// transcode before sending.
fn inline_image(bytes: &[u8]) -> Option<InlineImage> {
    const PNG_MAGIC: [u8; 8] = [0x89, b'P', b'N', b'G', 0x0d, 0x0a, 0x1a, 0x0a];
    const JPEG_MAGIC: [u8; 3] = [0xff, 0xd8, 0xff];
    if bytes.starts_with(&PNG_MAGIC) {
        Some(InlineImage::Png)
    } else if bytes.starts_with(&JPEG_MAGIC) {
        Some(InlineImage::Jpeg)
    } else {
        None
    }
}

/// The filename used when the caller supplied none (or one that sanitised away).
///
/// The extension is a container the kind's readers expect; the bytes, not the name, decide how the
/// message renders.
fn default_media_name(kind: MediaKind) -> String {
    let extension = match kind {
        MediaKind::Image => "png",
        MediaKind::Video => "mp4",
        MediaKind::Audio => "m4a",
        MediaKind::Document => "bin",
    };
    format!("attachment.{extension}")
}

/// Reduce a caller filename to one bare path component.
///
/// Everything outside ASCII alphanumerics, `.`, `-` and `_` becomes `_` (path separators, spaces,
/// control characters, non-ASCII), leading dots are dropped so the name can never be `..` or a
/// hidden file, an empty result falls back to [`default_media_name`], and the length is capped at
/// [`MAX_STAGED_NAME_CHARS`]. The result is therefore safe as the last component of a staged path
/// in [`std::env::temp_dir()`], whatever the caller sent.
fn sanitize_media_name(filename: Option<&str>, kind: MediaKind) -> String {
    let base = filename
        .unwrap_or_default()
        .rsplit(['/', '\\'])
        .next()
        .unwrap_or_default();
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
        return default_media_name(kind);
    }
    if name.chars().count() > MAX_STAGED_NAME_CHARS {
        name = name.chars().take(MAX_STAGED_NAME_CHARS).collect();
    }
    name
}

/// `name` without its extension, so the sniffed image extension can replace it.
///
/// Only a short alphanumeric suffix counts as an extension; anything else stays part of the stem.
fn stem_of(name: &str) -> &str {
    match name.rsplit_once('.') {
        Some((stem, extension))
            if !stem.is_empty()
                && !extension.is_empty()
                && extension.chars().count() <= 5
                && extension.chars().all(|ch| ch.is_ascii_alphanumeric()) =>
        {
            stem
        }
        _ => name,
    }
}

/// A staged attachment file that deletes itself when dropped.
///
/// The daemon reads `filePath` off the local filesystem when it consumes the `/_send` command, so
/// the bytes have to exist on disk for the duration of the call. `Drop` removes the file however
/// `send_media` returns — delivered, refused after staging, transport error, or a panic unwinding
/// through it.
struct StagedFile {
    path: PathBuf,
}

impl StagedFile {
    /// Stage `bytes` in [`std::env::temp_dir()`] under a unique name ending in `filename`.
    ///
    /// The name is `vikram-simplex-<pid>-<nanos>-<sequence>-<filename>`: one path component, so a
    /// hostile `filename` cannot escape the temp directory ([`sanitize_media_name`] already reduced
    /// it to a bare name, and this prefix is added on top). `create_new` never reuses an existing
    /// path, so a pre-planted file or symlink at the chosen name fails the call instead of being
    /// followed or overwritten; pid + clock + sequence make a collision in practice impossible, and
    /// a genuine one is retried before it is reported.
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
                "{STAGED_PREFIX}-{}-{nanos}-{sequence}-{filename}",
                std::process::id()
            );
            let path = std::env::temp_dir().join(name);
            match OpenOptions::new().write(true).create_new(true).open(&path) {
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
        Err(last_error
            .unwrap_or_else(|| std::io::Error::other("could not pick an unused temp media name")))
    }

    /// The staged path as the JSON string the command carries.
    ///
    /// Lossy on purpose: a non-UTF-8 `TMPDIR` must not turn a sendable attachment into a panic, and
    /// a command carrying a lossily-rendered path fails at the daemon (path not found), which is
    /// reported as a real failure rather than a success.
    fn wire_path(&self) -> String {
        self.path.to_string_lossy().into_owned()
    }
}

impl Drop for StagedFile {
    /// Best-effort removal. A leftover file in the OS temp directory is not worth turning a sent
    /// message into an error.
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.path);
    }
}

#[async_trait]
impl PlatformAdapter for SimplexAdapter {
    async fn connect(&mut self, _is_reconnect: bool) -> bool {
        // The daemon has no handshake; a successful command proves the socket is live.
        let probe = json!({"corrId": Self::corr_id(), "cmd": "/user"});
        self.connected = self.send_json(&probe).await.is_ok();
        self.connected
    }

    async fn disconnect(&mut self) {
        self.connected = false;
    }

    async fn send(
        &self,
        chat_id: &str,
        content: &str,
        _reply_to: Option<&str>,
        _metadata: Option<Value>,
    ) -> SendResult {
        if chat_id.trim().is_empty() {
            return SendResult::failed("simplex needs a contact or group id", Some("bad_format"));
        }
        if content.trim().is_empty() {
            return SendResult::failed("empty message content", Some("bad_format"));
        }
        let chunks = truncate_message(content, SIMPLEX_MAX_MESSAGE_LENGTH, codepoint_len);
        for chunk in chunks {
            let items = json!([{ "msgContent": { "type": "text", "text": chunk } }]);
            let command = format!("/_send {} json {items}", Self::target(chat_id));
            let payload = json!({ "corrId": Self::corr_id(), "cmd": command });
            if let Err(err) = self.send_json(&payload).await {
                return SendResult::failed(
                    format!("simplex send failed: {err}"),
                    Some("transient"),
                )
                .retryable(None);
            }
        }
        SendResult::ok("")
    }

    fn splits_long_messages(&self) -> bool {
        true
    }

    fn max_message_length(&self) -> usize {
        SIMPLEX_MAX_MESSAGE_LENGTH
    }

    /// Attachments are implemented for [`MediaSource::Bytes`]; see [`send_media`].
    ///
    /// [`send_media`]: PlatformAdapter::send_media
    fn supports_media(&self) -> bool {
        true
    }

    /// Send one attachment as a `/_send <target> json [item]` command.
    ///
    /// The item shapes are the reference's, field for field:
    /// - image — `{"filePath": <path>, "msgContent": {"type": "image", "image": <thumb>, "text": <caption>}}`
    ///   (`adapter.py:572`);
    /// - document — `{"filePath": <path>, "msgContent": {"type": "file", "text": <caption>}}`
    ///   (`adapter.py:588`), and video reuses it because `send_video` delegates to `send_document`
    ///   (`adapter.py:579-582`);
    /// - audio — `{"msgContent": {"type": "voice", "text": <caption>, "duration": 0},
    ///   "fileSource": {"filePath": <path>}}` (`adapter.py:596-597`); the path rides under
    ///   `fileSource` here, not at the item's top level, and `duration` is the reference's default
    ///   of `0` (`adapter.py:592`) because a byte slice carries no length measurement.
    ///
    /// The command itself is `_send_cmd` (`adapter.py:108-112`) — `/_send @<contactId> json <items>`
    /// or `/_send #<groupId> json <items>`, the item list serialised inline — i.e. exactly the
    /// channel [`send`](PlatformAdapter::send) already uses. The caption goes into
    /// `msgContent.text` as `caption or ""` (`adapter.py:572,588,596`), so the field is always
    /// present and an absent caption is `""`.
    ///
    /// `reply_to` is accepted and ignored, following the reference: its media senders take the
    /// argument (`send_voice`, `adapter.py:591-598`; `send_image_file`, `adapter.py:575-577`) but
    /// never place a quote or reply anchor in the item, so no field is invented here.
    ///
    /// **Thumbnail, and the format downgrade.** `_prepare_image` (`adapter.py:508-552`) does two
    /// jobs that both need an image codec: transcode non-PNG/JPEG inputs to PNG, and build a
    /// downscaled 128px JPEG `data:` URI for `msgContent.image`. This crate has no image
    /// dependency, so neither is possible. PNG and JPEG payloads (recognised by magic bytes) are
    /// still sent as `msgContent.type == "image"` with `"image": ""` — the exact value the
    /// reference itself emits when Pillow *and* ImageMagick are unavailable
    /// (`adapter.py:538-552` leave `thumb_uri = ""`; `adapter.py:572` still writes the key) — and no
    /// scaled thumbnail is fabricated, because a mislabelled full-size image would be a worse lie
    /// than the empty string the reference sends. Any other image payload (WebP, GIF, AVIF …) is
    /// **downgraded to `msgContent.type == "file"`**: those are precisely the containers
    /// `_prepare_image` exists to transcode (`adapter.py:510-516`: SimpleX clients cannot show WebP
    /// inline), so sending one as `image` would render an empty bubble instead of the picture.
    ///
    /// Bytes are staged in [`std::env::temp_dir()`] under a unique sanitised name before the
    /// command is built, because the daemon reads `filePath` off the local filesystem
    /// ([`StagedFile`] removes it on every return path; the reference never deletes its staged
    /// files at all, `adapter.py:554-598`). Because the unlink happens as the command write
    /// returns, a daemon that only opens the path long after accepting the frame is the one case
    /// where the staged copy can be gone; the reference instead keeps the file forever and waits
    /// for the daemon's reply.
    ///
    /// [`MediaSource::Url`] is a typed refusal, not a silent downgrade: the reference downloads
    /// URL media to a file first (`cache_image_from_url`, `adapter.py:559-563`) and this adapter has
    /// no fetch step on its WebSocket path, so callers pass [`MediaSource::Bytes`] or send the URL
    /// as text themselves.
    ///
    /// There is no media size cap to apply: the reference checks the file only for existence
    /// (`adapter.py:564,586,594`), and its one length constant — [`SIMPLEX_MAX_MESSAGE_LENGTH`] =
    /// 8000 (`adapter.py:39`) — is a *text* chunking limit that media never passes through.
    ///
    /// Success means the command was accepted by the daemon socket, which is what
    /// [`send`](PlatformAdapter::send) already reports: this transport is fire-and-forget and routes
    /// no corrId replies back, so there is no acknowledgement to await (the reference's
    /// `_send_items`, `adapter.py:449-452`, does await one — see the module notes). A refused write
    /// is `transient` and retryable, and refuses the whole attachment rather than reporting a
    /// partial delivery.
    async fn send_media(
        &self,
        chat_id: &str,
        media: &OutboundMedia,
        reply_to: Option<&str>,
    ) -> SendResult {
        // The reference's media senders never place `reply_to` in the item (`adapter.py:554-598`).
        let _ = reply_to;
        if chat_id.trim().is_empty() {
            return SendResult::failed("simplex needs a contact or group id", Some("bad_format"));
        }
        let bytes = match &media.source {
            MediaSource::Bytes(bytes) => bytes,
            MediaSource::Url(url) => {
                return SendResult::failed(
                    format!(
                        "simplex media needs bytes: refusing MediaSource::Url({url}) because the \
                         reference downloads URL media to a local file before sending \
                         (`cache_image_from_url`, adapter.py:559-563) and this adapter has no fetch \
                         step — pass MediaSource::Bytes, or send the URL as text"
                    ),
                    Some("bad_format"),
                );
            }
        };
        if bytes.is_empty() {
            return SendResult::failed("refusing to send empty media bytes", Some("bad_format"));
        }

        let fallback = sanitize_media_name(media.filename.as_deref(), media.kind);
        let (name, wire) = match media.kind {
            MediaKind::Image => match inline_image(bytes) {
                // A container clients render inline: stage it under its own extension
                // (`p.with_suffix(".png")`, `adapter.py:517`).
                Some(format) => (
                    format!("{}.{}", stem_of(&fallback), format.extension()),
                    WireKind::Image,
                ),
                // Anything else is one the reference would have transcoded — send the file.
                None => (fallback, WireKind::File),
            },
            MediaKind::Video | MediaKind::Document => (fallback, WireKind::File),
            MediaKind::Audio => (fallback, WireKind::Voice),
        };

        let staged = match StagedFile::create(&name, bytes) {
            Ok(staged) => staged,
            Err(err) => {
                return SendResult::failed(
                    format!("simplex could not stage the attachment: {err}"),
                    Some("unknown"),
                );
            }
        };
        let path = staged.wire_path();
        let caption = media.caption.clone().unwrap_or_default();
        let item = match wire {
            WireKind::Image => json!({
                "filePath": path,
                "msgContent": {"type": wire.msg_type(), "image": "", "text": caption},
            }),
            WireKind::File => json!({
                "filePath": path,
                "msgContent": {"type": wire.msg_type(), "text": caption},
            }),
            WireKind::Voice => json!({
                "msgContent": {
                    "type": wire.msg_type(),
                    "text": caption,
                    "duration": SIMPLEX_VOICE_DURATION_UNKNOWN,
                },
                "fileSource": {"filePath": path},
            }),
        };
        let command = format!("/_send {} json {}", Self::target(chat_id), json!([item]));
        let payload = json!({ "corrId": Self::corr_id(), "cmd": command });
        // `staged` is still alive here; it is unlinked when this scope ends.
        match self.send_json(&payload).await {
            Ok(()) => SendResult::ok(""),
            Err(err) => SendResult::failed(
                format!("simplex media send failed: {err}"),
                Some("transient"),
            )
            .retryable(None),
        }
    }

    async fn get_chat_info(&self, chat_id: &str) -> ChatInfo {
        ChatInfo {
            name: chat_id.to_string(),
            chat_type: if chat_id.starts_with("group:") {
                "group".to_string()
            } else {
                "dm".to_string()
            },
        }
    }

    async fn send_typing(&self, _chat_id: &str, _metadata: Option<Value>) {}
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::socket::{LineSocket, SocketError};
    use async_trait::async_trait;
    use std::sync::Mutex;
    use std::sync::atomic::AtomicBool;

    /// One staged path named by a command, read back the instant the command was written.
    ///
    /// `bytes` is `Some` only when the file existed at that moment, which is what proves the
    /// attachment was on disk while the daemon could still have read it.
    #[derive(Clone)]
    struct StagedProbe {
        path: String,
        bytes: Option<Vec<u8>>,
    }

    /// One command as the mock socket saw it.
    #[derive(Clone)]
    struct SentCommand {
        command: String,
        items: Vec<Value>,
        staged: Vec<StagedProbe>,
    }

    impl SentCommand {
        fn item(&self) -> &Value {
            self.items.first().unwrap_or(&Value::Null)
        }
        fn msg_content(&self) -> &Value {
            self.item().get("msgContent").unwrap_or(&Value::Null)
        }
        fn msg_type(&self) -> &str {
            self.msg_content()
                .get("type")
                .and_then(Value::as_str)
                .unwrap_or_default()
        }
        fn staged_path(&self) -> &str {
            self.staged
                .first()
                .map(|probe| probe.path.as_str())
                .unwrap_or_default()
        }
    }

    /// Split a `/_send <target> json <items>` command back into its item list.
    fn observe(line: &str) -> SentCommand {
        let value: Value = serde_json::from_str(line).unwrap_or(Value::Null);
        let command = value
            .get("cmd")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string();
        let items = command
            .split_once(" json ")
            .and_then(|(_, tail)| serde_json::from_str::<Value>(tail).ok())
            .and_then(|parsed| parsed.as_array().cloned())
            .unwrap_or_default();
        let mut staged = Vec::new();
        for item in &items {
            let paths = [
                item.get("filePath"),
                item.get("fileSource")
                    .and_then(|source| source.get("filePath")),
            ];
            for path in paths.into_iter().flatten().filter_map(Value::as_str) {
                staged.push(StagedProbe {
                    path: path.to_string(),
                    bytes: std::fs::read(path).ok(),
                });
            }
        }
        SentCommand {
            command,
            items,
            staged,
        }
    }

    struct MockSocket {
        sent: Mutex<Vec<String>>,
        incoming: Mutex<std::collections::VecDeque<String>>,
        observed: Mutex<Vec<SentCommand>>,
        fail_sends: AtomicBool,
    }

    impl MockSocket {
        fn new(incoming: Vec<String>) -> Arc<Self> {
            Arc::new(Self {
                sent: Mutex::new(Vec::new()),
                incoming: Mutex::new(incoming.into()),
                observed: Mutex::new(Vec::new()),
                fail_sends: AtomicBool::new(false),
            })
        }
        fn sent(&self) -> Vec<String> {
            self.sent.lock().unwrap().clone()
        }
        /// Every command the adapter tried to write, including the failed ones.
        fn observed(&self) -> Vec<SentCommand> {
            self.observed.lock().unwrap().clone()
        }
        /// The single command the adapter wrote; panics when the count is not one.
        fn only(&self) -> SentCommand {
            let observed = self.observed.lock().unwrap();
            assert_eq!(observed.len(), 1, "expected exactly one command");
            observed[0].clone()
        }
    }

    #[async_trait]
    impl LineSocket for MockSocket {
        async fn send_line(&self, line: &str) -> Result<(), SocketError> {
            // Observed before the failure branch: a refused write still has to be inspectable.
            self.observed.lock().unwrap().push(observe(line));
            if self.fail_sends.load(Ordering::Relaxed) {
                return Err(SocketError::Closed);
            }
            self.sent.lock().unwrap().push(line.to_string());
            Ok(())
        }
        async fn next_line(&self) -> Result<Option<String>, SocketError> {
            Ok(self.incoming.lock().unwrap().pop_front())
        }
    }

    fn build(incoming: Vec<String>) -> (SimplexAdapter, Arc<MockSocket>) {
        let socket = MockSocket::new(incoming);
        let adapter = SimplexAdapter::new(Arc::clone(&socket) as SharedLineSocket);
        (adapter, socket)
    }

    /// An adapter whose socket refuses every write.
    fn build_failing() -> (SimplexAdapter, Arc<MockSocket>) {
        let (adapter, socket) = build(vec![]);
        socket.fail_sends.store(true, Ordering::Relaxed);
        (adapter, socket)
    }

    const PNG_BYTES: &[u8] = b"\x89PNG\r\n\x1a\npayload";
    const JPEG_BYTES: &[u8] = &[0xff, 0xd8, 0xff, 0xe0, b'j', b'f', b'i', b'f'];
    const WEBP_BYTES: &[u8] = b"RIFF\x24\x00\x00\x00WEBPVP8 ";

    fn media(
        kind: MediaKind,
        bytes: &[u8],
        filename: Option<&str>,
        caption: Option<&str>,
    ) -> OutboundMedia {
        let mut media = OutboundMedia::new(kind, MediaSource::Bytes(bytes.to_vec()));
        media.filename = filename.map(str::to_owned);
        media.caption = caption.map(str::to_owned);
        media
    }

    fn assert_removed(path: &str) {
        assert!(
            !std::path::Path::new(path).exists(),
            "staged file {path} should have been cleaned up"
        );
    }

    #[tokio::test]
    async fn connect_probes_the_daemon() {
        let (mut adapter, socket) = build(vec![]);
        assert!(adapter.connect(false).await);
        let sent = socket.sent();
        assert_eq!(sent.len(), 1);
        let value: Value = serde_json::from_str(&sent[0]).unwrap();
        assert_eq!(value["cmd"], "/user");
        assert!(value["corrId"].as_str().unwrap().starts_with("vikram-"));
    }

    #[tokio::test]
    async fn send_addresses_contacts_and_groups() {
        let (adapter, socket) = build(vec![]);
        assert!(adapter.send("42", "hello", None, None).await.success);
        assert!(
            adapter
                .send("group:7", "hi group", None, None)
                .await
                .success
        );
        let sent = socket.sent();
        let direct: Value = serde_json::from_str(&sent[0]).unwrap();
        let cmd = direct["cmd"].as_str().expect("command string");
        assert!(cmd.starts_with("/_send @42 json "), "got {cmd}");
        assert!(cmd.contains(r#""type":"text""#), "got {cmd}");
        assert!(cmd.contains(r#""text":"hello""#), "got {cmd}");
        let group: Value = serde_json::from_str(&sent[1]).unwrap();
        assert!(
            group["cmd"]
                .as_str()
                .unwrap()
                .starts_with("/_send #7 json ")
        );
    }

    #[tokio::test]
    async fn long_text_is_chunked() {
        let content = "x".repeat(SIMPLEX_MAX_MESSAGE_LENGTH + 5);
        let (adapter, socket) = build(vec![]);
        assert!(adapter.send("42", &content, None, None).await.success);
        assert_eq!(socket.sent().len(), 2);
    }

    #[tokio::test]
    async fn blank_inputs_make_no_request() {
        let (adapter, socket) = build(vec![]);
        assert!(!adapter.send("", "hi", None, None).await.success);
        assert!(!adapter.send("42", "  ", None, None).await.success);
        assert!(socket.sent().is_empty());
    }

    #[tokio::test]
    async fn inbound_direct_and_group_messages_parse() {
        let direct = json!({"resp": {"type": "newChatItem", "chatItem": {
            "chatInfo": {"type": "direct", "contact": {"contactId": 42, "localDisplayName": "Alice"}},
            "chatItem": {
                "chatDir": {"type": "directRcv"},
                "content": {"type": "rcvMsgContent", "msgContent": {"type": "text", "text": "hello"}}
            }
        }}})
        .to_string();
        let group = json!({"type": "newChatItem", "chatItem": {
            "chatInfo": {"type": "group", "groupInfo": {"groupId": 7, "localDisplayName": "Team"}},
            "chatItem": {
                "chatDir": {"type": "groupRcv", "groupMember": {"memberId": 9, "localDisplayName": "Bob"}},
                "content": {"type": "rcvMsgContent", "msgContent": {"type": "text", "text": "hi all"}}
            }
        }})
        .to_string();
        let own = json!({"resp": {"type": "newChatItem", "chatItem": {
            "chatInfo": {"type": "direct", "contact": {"contactId": 42}},
            "chatItem": {
                "chatDir": {"type": "directSnd"},
                "content": {"type": "rcvMsgContent", "msgContent": {"type": "text", "text": "mine"}}
            }
        }}})
        .to_string();
        let (adapter, _) = build(vec![direct, own, group]);
        let first = adapter.next_event().await.unwrap().expect("direct event");
        assert_eq!(first.text, "hello");
        assert_eq!(first.source.chat_type, "dm");
        assert_eq!(first.user_id.as_deref(), Some("42"));
        let second = adapter.next_event().await.unwrap().expect("group event");
        assert_eq!(second.text, "hi all");
        assert_eq!(second.source.chat_id, "group:7");
        assert_eq!(second.source.chat_type, "group");
        assert_eq!(second.user_id.as_deref(), Some("9"));
    }

    #[tokio::test]
    async fn chat_info_and_typing_are_local() {
        let (adapter, socket) = build(vec![]);
        assert_eq!(adapter.get_chat_info("group:7").await.chat_type, "group");
        assert_eq!(adapter.get_chat_info("42").await.chat_type, "dm");
        adapter.send_typing("42", None).await;
        assert!(socket.sent().is_empty());
    }

    // ----- outbound media (`supports_media` / `send_media`) -----

    #[tokio::test]
    async fn supports_media_reports_true() {
        let (adapter, _) = build(vec![]);
        assert!(adapter.supports_media());
    }

    #[tokio::test]
    async fn image_sends_an_image_item_with_the_staged_file() {
        let (adapter, socket) = build(vec![]);
        let result = adapter
            .send_media(
                "42",
                &media(MediaKind::Image, PNG_BYTES, Some("cat.png"), Some("a cat")),
                None,
            )
            .await;
        assert!(result.success);
        assert_eq!(result.error, None);

        let command = socket.only();
        assert!(
            command.command.starts_with("/_send @42 json "),
            "got {}",
            command.command
        );
        assert_eq!(command.msg_type(), "image");
        let item = command.item();
        assert!(
            item["filePath"]
                .as_str()
                .unwrap_or_default()
                .ends_with(".png")
        );
        // Reference: `{"type": "image", "image": thumb_uri, "text": caption or ""}`
        // (`adapter.py:572`). Without an image codec there is no 128px thumbnail, so `image` is the
        // empty string the reference itself sends when its converters are missing.
        assert_eq!(item["msgContent"]["image"], "");
        assert_eq!(item["msgContent"]["text"], "a cat");

        // Existed, with the caller's bytes, while the command was written.
        let probe = &command.staged[0];
        assert_eq!(probe.bytes.as_deref(), Some(PNG_BYTES));
        assert!(std::path::Path::new(&probe.path).starts_with(std::env::temp_dir()));
        // ... and is gone once the send returned.
        assert_removed(&probe.path);
    }

    #[tokio::test]
    async fn jpeg_images_are_staged_with_a_jpg_extension() {
        let (adapter, socket) = build(vec![]);
        let result = adapter
            .send_media(
                "42",
                &media(MediaKind::Image, JPEG_BYTES, Some("photo.webp"), None),
                None,
            )
            .await;
        assert!(result.success);
        let command = socket.only();
        assert_eq!(command.msg_type(), "image");
        // The sniffed container, not the caller's (wrong) extension.
        assert!(
            command.staged_path().ends_with(".jpg"),
            "{}",
            command.staged_path()
        );
        assert_eq!(command.msg_content()["text"], "");
        assert_removed(command.staged_path());
    }

    #[tokio::test]
    async fn documents_and_videos_send_as_file_items() {
        let (adapter, socket) = build(vec![]);
        assert!(
            adapter
                .send_media(
                    "42",
                    &media(
                        MediaKind::Document,
                        b"%PDF-1.7",
                        Some("notes.pdf"),
                        Some("read me")
                    ),
                    None,
                )
                .await
                .success
        );
        assert!(
            adapter
                .send_media(
                    "42",
                    &media(
                        MediaKind::Video,
                        b"\x00\x00\x00\x18ftypmp42",
                        Some("clip.mp4"),
                        None
                    ),
                    None,
                )
                .await
                .success
        );

        let commands = socket.observed();
        assert_eq!(commands.len(), 2);
        // `send_video` delegates to `send_document` in the reference (`adapter.py:579-582`), so both
        // ride `msgContent.type == "file"` (`adapter.py:588`).
        for command in &commands {
            assert_eq!(command.msg_type(), "file");
            assert!(command.item().get("fileSource").is_none());
        }
        assert!(commands[0].staged_path().ends_with(".pdf"));
        assert_eq!(commands[0].msg_content()["text"], "read me");
        assert_eq!(
            commands[0].staged[0].bytes.as_deref(),
            Some(&b"%PDF-1.7"[..])
        );
        assert!(commands[1].staged_path().ends_with(".mp4"));
        assert_eq!(commands[1].msg_content()["text"], "");
        for command in &commands {
            assert_removed(command.staged_path());
        }
    }

    #[tokio::test]
    async fn audio_sends_a_voice_item_with_zero_duration() {
        let (adapter, socket) = build(vec![]);
        let result = adapter
            .send_media(
                "42",
                &media(
                    MediaKind::Audio,
                    b"OggS\x00voice",
                    Some("note.ogg"),
                    Some("listen"),
                ),
                None,
            )
            .await;
        assert!(result.success);

        let command = socket.only();
        assert_eq!(command.msg_type(), "voice");
        // `{"type": "voice", "text": caption or "", "duration": duration}`
        // + `{"fileSource": {"filePath": ...}}` (`adapter.py:596-597`), duration defaulting to 0
        // (`adapter.py:592`).
        assert_eq!(
            command.msg_content()["duration"],
            SIMPLEX_VOICE_DURATION_UNKNOWN
        );
        assert_eq!(command.msg_content()["duration"], 0);
        assert_eq!(command.msg_content()["text"], "listen");
        // The path rides under `fileSource`, never at the item's top level.
        assert!(command.item().get("filePath").is_none());
        assert!(
            command.item()["fileSource"]["filePath"]
                .as_str()
                .unwrap_or_default()
                .ends_with(".ogg")
        );
        assert_eq!(
            command.staged[0].bytes.as_deref(),
            Some(&b"OggS\x00voice"[..])
        );
        assert_removed(command.staged_path());
    }

    #[tokio::test]
    async fn media_to_a_group_uses_the_group_target() {
        let (adapter, socket) = build(vec![]);
        assert!(
            adapter
                .send_media(
                    "group:7",
                    &media(MediaKind::Document, b"data", None, None),
                    Some("ignored")
                )
                .await
                .success
        );
        let command = socket.only();
        assert!(
            command.command.starts_with("/_send #7 json "),
            "got {}",
            command.command
        );
        // `reply_to` is accepted and ignored, like the reference's media senders.
        assert!(command.item().get("quotedItemId").is_none());
    }

    #[tokio::test]
    async fn non_png_images_downgrade_to_file_items() {
        let (adapter, socket) = build(vec![]);
        let result = adapter
            .send_media(
                "42",
                &media(
                    MediaKind::Image,
                    WEBP_BYTES,
                    Some("shot.webp"),
                    Some("webp"),
                ),
                None,
            )
            .await;
        assert!(result.success);
        let command = socket.only();
        // `_prepare_image` exists to transcode WebP-like formats to PNG before the item is built
        // (`adapter.py:510-524`); without a codec the honest wire form is a file attachment.
        assert_eq!(command.msg_type(), "file");
        assert!(command.staged_path().ends_with(".webp"));
        assert_eq!(command.msg_content()["text"], "webp");
        assert_removed(command.staged_path());
    }

    #[tokio::test]
    async fn url_media_is_refused_and_writes_nothing() {
        let (adapter, socket) = build(vec![]);
        let media = OutboundMedia::image_url("https://example.test/a.png");
        let result = adapter.send_media("42", &media, None).await;
        assert!(!result.success);
        assert_eq!(result.error_kind.as_deref(), Some("bad_format"));
        let error = result.error.unwrap_or_default();
        assert!(error.contains("MediaSource::Bytes"), "got {error}");
        assert!(error.contains("https://example.test/a.png"), "got {error}");
        // Zero frames: no command, no staged file.
        assert!(socket.observed().is_empty());
    }

    #[tokio::test]
    async fn empty_bytes_are_refused_and_write_nothing() {
        let (adapter, socket) = build(vec![]);
        let result = adapter
            .send_media(
                "42",
                &media(MediaKind::Document, b"", Some("empty.bin"), None),
                None,
            )
            .await;
        assert!(!result.success);
        assert_eq!(result.error_kind.as_deref(), Some("bad_format"));
        assert!(!result.retryable);
        assert!(socket.observed().is_empty());
    }

    #[tokio::test]
    async fn blank_chat_id_is_refused_for_media() {
        let (adapter, socket) = build(vec![]);
        let result = adapter
            .send_media(" ", &media(MediaKind::Document, b"x", None, None), None)
            .await;
        assert!(!result.success);
        assert_eq!(result.error_kind.as_deref(), Some("bad_format"));
        assert!(socket.observed().is_empty());
    }

    #[tokio::test]
    async fn refused_write_is_transient_and_leaves_no_staged_file() {
        let (adapter, socket) = build_failing();
        let result = adapter
            .send_media(
                "42",
                &media(
                    MediaKind::Document,
                    b"payload",
                    Some("doc.bin"),
                    Some("cap"),
                ),
                None,
            )
            .await;
        assert!(!result.success);
        assert_eq!(result.error_kind.as_deref(), Some("transient"));
        assert!(result.retryable);
        assert_eq!(result.message_id, None);
        let error = result.error.unwrap_or_default();
        assert!(error.contains("simplex media send failed"), "got {error}");

        // The write was attempted with the file on disk ...
        let command = socket.only();
        assert_eq!(command.staged[0].bytes.as_deref(), Some(&b"payload"[..]));
        // ... and the failure still cleaned it up.
        assert_removed(command.staged_path());
    }

    #[tokio::test]
    async fn hostile_filenames_stay_inside_the_temp_directory() {
        let (adapter, socket) = build(vec![]);
        let result = adapter
            .send_media(
                "42",
                &media(
                    MediaKind::Document,
                    b"payload",
                    Some("../../etc/pass wd\n.bin"),
                    None,
                ),
                None,
            )
            .await;
        assert!(result.success);
        let command = socket.only();
        let path = std::path::Path::new(command.staged_path());
        assert_eq!(
            path.parent(),
            Some(std::env::temp_dir().as_path()),
            "staged {path:?} escaped the temp directory"
        );
        let name = path
            .file_name()
            .and_then(|name| name.to_str())
            .unwrap_or_default();
        assert!(name.starts_with(STAGED_PREFIX), "got {name}");
        assert!(!name.contains(['/', '\\', ' ', '\n']), "got {name}");
        assert_removed(command.staged_path());
    }

    #[tokio::test]
    async fn text_sends_are_unchanged_by_the_media_path() {
        let (adapter, socket) = build(vec![]);
        assert!(adapter.send("42", "hello", None, None).await.success);
        let command = socket.only();
        assert_eq!(command.msg_type(), "text");
        assert!(command.item().get("filePath").is_none());
        assert!(command.staged.is_empty());
        assert_eq!(command.msg_content()["text"], "hello");
    }
}
