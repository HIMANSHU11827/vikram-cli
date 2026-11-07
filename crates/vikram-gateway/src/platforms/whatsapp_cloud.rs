//! WhatsApp Cloud adapter — Meta Graph API.
//!
//! Outbound is one REST call per message: `POST /{version}/{phone-number-id}/messages`.
//! Inbound is a webhook Meta posts to us: a `GET` handshake carries `hub.verify_token`, and every
//! `POST` carries an `X-Hub-Signature-256` HMAC over the raw body keyed by the app secret.
//!
//! Outbound media ([`PlatformAdapter::send_media`]) is the reference's two-step flow
//! (`research/hermes-platforms/whatsapp_cloud/adapter.py:528-574`): held bytes are uploaded to
//! `POST /{version}/{phone-number-id}/media` as `multipart/form-data` (`file`, `messaging_product`,
//! `type`), the returned `id` is then sent as an `image` / `video` / `audio` / `document`
//! message carrying it, while a [`MediaSource::Url`] skips the upload and travels as
//! `{"link": …}` because Meta fetches the link itself (`adapter.py:584-589`). A failed upload
//! never reaches the send, and a send only counts when Graph returns a `messages[0].id`.
//!
//! Port of `plugins/platforms/whatsapp_cloud/adapter.py`. Differences that are deliberate:
//! no in-process HTTP server (the host owns the listener and calls [`WhatsAppCloudAdapter::parse_webhook`] /
//! [`WhatsAppCloudAdapter::verify_signature`]), and no rich-sent store (the host decides what to do
//! with a quoted message id).

use async_trait::async_trait;
use serde_json::{Value, json};
use std::collections::HashMap;
use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, Ordering};

use crate::event::{MessageEvent, MessageType, Platform, SessionSource};
use crate::helpers::{codepoint_len, truncate_message};
use crate::platform::{
    ChatInfo, MediaKind, MediaSource, OutboundMedia, PlatformAdapter, SendResult,
};
use crate::transport::{HttpRequest, SharedTransport};

pub const GRAPH_API_BASE: &str = "https://graph.facebook.com";
pub const GRAPH_API_VERSION: &str = "v21.0";
/// Meta's text body limit; longer answers are chunked.
pub const MAX_MESSAGE_LENGTH: usize = 4096;

/// Meta's per-kind upload cap for images (5 MiB, `adapter.py:72-75`).
pub const MAX_IMAGE_BYTES: usize = 5 * 1024 * 1024;
/// Meta's per-kind upload cap for video (16 MiB, `adapter.py:72-75`).
pub const MAX_VIDEO_BYTES: usize = 16 * 1024 * 1024;
/// Meta's per-kind upload cap for audio (16 MiB, `adapter.py:72-75`).
pub const MAX_AUDIO_BYTES: usize = 16 * 1024 * 1024;
/// Meta's per-kind upload cap for documents (100 MiB, `adapter.py:72-75`).
pub const MAX_DOCUMENT_BYTES: usize = 100 * 1024 * 1024;

/// Longest filename or content type kept from a caller value, so one absurd string cannot bloat a
/// multipart part header.
const MAX_PART_TOKEN_CHARS: usize = 200;

pub struct WhatsAppCloudAdapter {
    transport: SharedTransport,
    access_token: String,
    phone_number_id: String,
    app_secret: Option<String>,
    verify_token: Option<String>,
    api_base: String,
    /// Last inbound message id per chat: Meta couples read receipts and the typing indicator into
    /// one call, and both need the wamid that started the turn.
    last_inbound: Mutex<HashMap<String, String>>,
}

impl WhatsAppCloudAdapter {
    pub fn new(
        transport: SharedTransport,
        access_token: impl Into<String>,
        phone_number_id: impl Into<String>,
    ) -> Self {
        Self {
            transport,
            access_token: access_token.into(),
            phone_number_id: phone_number_id.into(),
            app_secret: None,
            verify_token: None,
            api_base: GRAPH_API_BASE.to_string(),
            last_inbound: Mutex::new(HashMap::new()),
        }
    }

    /// Secrets needed to accept inbound webhooks. Without them, webhook verification and signature
    /// checks fail closed.
    pub fn with_webhook_secrets(
        mut self,
        app_secret: impl Into<String>,
        verify_token: impl Into<String>,
    ) -> Self {
        self.app_secret = Some(app_secret.into());
        self.verify_token = Some(verify_token.into());
        self
    }

    /// Point the adapter at a mirror or a test double.
    pub fn with_api_base(mut self, base: impl Into<String>) -> Self {
        self.api_base = base.into();
        self
    }

    fn graph_url(&self, leaf: &str) -> String {
        format!(
            "{}/{}/{}/{}",
            self.api_base.trim_end_matches('/'),
            GRAPH_API_VERSION,
            self.phone_number_id,
            leaf
        )
    }

    async fn post_messages(&self, payload: &Value) -> Result<Vec<Value>, Failure> {
        let request = crate::transport::HttpRequest::json(self.graph_url("messages"), payload)
            .bearer(&self.access_token);
        let response = self
            .transport
            .execute(request)
            .await
            .map_err(|err| Failure::transient(err.to_string()))?;
        let body: Value = response.json().unwrap_or(Value::Null);
        if !response.ok() {
            return Err(classify_failure(
                response.status,
                &body,
                response.retry_after,
            ));
        }
        Ok(body
            .get("messages")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default())
    }

    /// Upload one attachment to `POST /{phone-number-id}/media` and return Meta's media id.
    ///
    /// The body is `multipart/form-data` (RFC 7578, CRLF framing) with the reference's three
    /// fields in its order (`adapter.py:535-541`): `file` (filename, content type, raw bytes),
    /// `messaging_product=whatsapp`, and `type=<mime>` — Meta's upload `type` field carries the
    /// media's content type, not the `image`/`video`/`audio`/`document` message kind. The
    /// `Authorization: Bearer` header rides along because `/media` is a Graph endpoint like any
    /// other (`_auth_headers(json_body=False)`, `adapter.py:541`).
    ///
    /// Every failure is returned as the classified [`SendResult`] — transport error, non-2xx
    /// (Graph's `error.message` / `error.code` are surfaced by [`classify_failure`]), an
    /// unparseable body, or an answer without an `id` (`adapter.py:545-551`) — so the caller
    /// stops before sending a message that references nothing. The failure is boxed to keep the
    /// success path (`String`) small.
    ///
    /// Any 2xx counts as uploaded where the reference compares against `200` exactly
    /// (`adapter.py:545`): Graph answers `200` for an upload, and the sibling `/messages` path in
    /// this file already accepts 2xx ([`WhatsAppCloudAdapter::post_messages`]).
    async fn upload_media(
        &self,
        filename: &str,
        mime: &str,
        bytes: &[u8],
    ) -> Result<String, Box<SendResult>> {
        let boundary = multipart_boundary();
        let request = HttpRequest::post(self.graph_url("media"))
            .bearer(&self.access_token)
            .header(
                "Content-Type",
                format!("multipart/form-data; boundary={boundary}"),
            )
            .body(encode_media_multipart(&boundary, filename, mime, bytes));
        let response = self
            .transport
            .execute(request)
            .await
            .map_err(|err| Box::new(Failure::transient(err.to_string()).into_result()))?;
        let body: Value = response.json().unwrap_or(Value::Null);
        if !response.ok() {
            return Err(Box::new(
                classify_failure(response.status, &body, response.retry_after).into_result(),
            ));
        }
        body.get("id")
            .and_then(Value::as_str)
            .filter(|id| !id.trim().is_empty())
            .map(str::to_string)
            .ok_or_else(|| {
                Box::new(SendResult::failed(
                    "whatsapp_cloud media upload response missing 'id'",
                    Some("unknown"),
                ))
            })
    }

    /// POST one media message and report Meta's message id.
    ///
    /// Success requires `messages[0].id`, exactly like the reference (`_post_message_result`,
    /// `adapter.py:350-355`): an accepted-but-unidentified answer is reported as a failure rather
    /// than as a delivery that never happened.
    async fn post_media(&self, payload: &Value) -> SendResult {
        match self.post_messages(payload).await {
            Ok(messages) => match messages
                .first()
                .and_then(|message| message.get("id"))
                .and_then(Value::as_str)
                .filter(|id| !id.trim().is_empty())
            {
                Some(id) => SendResult::ok(id),
                None => SendResult::failed(
                    "whatsapp_cloud media send returned no message id",
                    Some("unknown"),
                ),
            },
            Err(failure) => failure.into_result(),
        }
    }

    /// Build the `/messages` envelope. `reply_to` quotes on the first chunk only, which is what the
    /// reference adapter does (quoting every chunk would produce N quotes for one answer).
    fn outbound_payload(chat_id: &str, kind: &str, block: Value) -> Value {
        json!({
            "messaging_product": "whatsapp",
            "recipient_type": "individual",
            "to": chat_id,
            "type": kind,
            kind: block,
        })
    }

    /// The `/messages` envelope for a media message; `reply_to` becomes `context.message_id`
    /// (`_outbound_payload`, `adapter.py:357-363`). A blank reference adds no `context`, which is
    /// the same as an absent one.
    fn media_payload(chat_id: &str, kind: &str, block: Value, reply_to: Option<&str>) -> Value {
        let mut payload = Self::outbound_payload(chat_id, kind, block);
        if let Some(reply_to) = reply_to.filter(|id| !id.trim().is_empty()) {
            payload["context"] = json!({ "message_id": reply_to });
        }
        payload
    }

    fn record_inbound(&self, chat_id: &str, message_id: &str) {
        if let Ok(mut map) = self.last_inbound.lock() {
            map.insert(chat_id.to_string(), message_id.to_string());
        }
    }

    /// The `GET` half of Meta's webhook handshake. Returns the challenge to echo, or why it refused.
    pub fn verify_webhook(
        &self,
        mode: &str,
        token: &str,
        challenge: &str,
    ) -> Result<String, String> {
        let expected = self
            .verify_token
            .as_deref()
            .ok_or_else(|| "verify_token is not configured".to_string())?;
        if mode != "subscribe" {
            return Err(format!("unsupported hub.mode `{mode}`"));
        }
        if !constant_time_eq(token.as_bytes(), expected.as_bytes()) {
            return Err("hub.verify_token mismatch".to_string());
        }
        Ok(challenge.to_string())
    }

    /// `X-Hub-Signature-256: sha256=<hex>` over the exact bytes Meta sent.
    pub fn verify_signature(&self, raw_body: &[u8], header: &str) -> bool {
        let Some(secret) = self.app_secret.as_deref() else {
            return false;
        };
        let Some(provided) = header.strip_prefix("sha256=") else {
            return false;
        };
        let expected = hmac_sha256_hex(secret.as_bytes(), raw_body);
        constant_time_eq(provided.as_bytes(), expected.as_bytes())
    }

    /// Parse one webhook delivery into events. Status callbacks (delivered/read) produce none.
    pub fn parse_webhook(&self, body: &str) -> Result<Vec<MessageEvent>, String> {
        let root: Value =
            serde_json::from_str(body).map_err(|err| format!("invalid json: {err}"))?;
        let mut events = Vec::new();
        let platform = Platform::new("whatsapp_cloud").expect("valid platform");

        let Some(entries) = root.get("entry").and_then(Value::as_array) else {
            return Ok(events);
        };
        for entry in entries {
            let Some(changes) = entry.get("changes").and_then(Value::as_array) else {
                continue;
            };
            for change in changes {
                let Some(value) = change.get("value") else {
                    continue;
                };
                let contact_name = value
                    .get("contacts")
                    .and_then(Value::as_array)
                    .and_then(|contacts| contacts.first())
                    .and_then(|contact| contact.get("profile"))
                    .and_then(|profile| profile.get("name"))
                    .and_then(Value::as_str)
                    .map(str::to_owned);
                let Some(messages) = value.get("messages").and_then(Value::as_array) else {
                    continue;
                };
                for message in messages {
                    if let Some(event) = parse_message(message, contact_name.clone(), &platform) {
                        if let Some(id) = event.message_id.as_deref() {
                            self.record_inbound(&event.source.chat_id, id);
                        }
                        events.push(event);
                    }
                }
            }
        }
        Ok(events)
    }
}

fn parse_message(
    message: &Value,
    contact_name: Option<String>,
    platform: &Platform,
) -> Option<MessageEvent> {
    let from = message.get("from")?.as_str()?;
    let id = message.get("id").and_then(Value::as_str).map(str::to_owned);
    let kind = message
        .get("type")
        .and_then(Value::as_str)
        .unwrap_or("text");

    let mut source = SessionSource::new(platform.clone(), from);
    source.chat_type = "dm".to_string();
    source.user_id = Some(from.to_string());
    source.user_name = contact_name;
    source.message_id = id.clone();

    let mut event = MessageEvent::new(String::new(), source);
    event.message_id = id;
    event.user_id = Some(from.to_string());
    event.user_name = event.source.user_name.clone();
    event.raw_message = Some(message.clone());
    event.reply_to_message_id = message
        .get("context")
        .and_then(|context| context.get("id"))
        .and_then(Value::as_str)
        .map(str::to_owned);

    match kind {
        "text" => {
            event.text = message
                .get("text")
                .and_then(|text| text.get("body"))
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string();
            event.message_type = MessageType::Text;
        }
        "image" | "video" | "audio" | "voice" | "document" | "sticker" => {
            event.message_type = match kind {
                "image" => MessageType::Photo,
                "video" => MessageType::Video,
                "audio" => MessageType::Audio,
                "voice" => MessageType::Voice,
                "document" => MessageType::Document,
                _ => MessageType::Sticker,
            };
            let block = message.get(kind)?;
            let media_id = block.get("id").and_then(Value::as_str);
            let caption = block.get("caption").and_then(Value::as_str);
            if let Some(media_id) = media_id {
                event.media_urls.push(media_id.to_string());
            }
            if let Some(mime) = block.get("mime_type").and_then(Value::as_str) {
                event.media_types.push(mime.to_string());
            }
            if let Some(name) = block.get("filename").and_then(Value::as_str) {
                event.media_filenames.push(name.to_string());
            }
            event.text = caption.unwrap_or_default().to_string();
        }
        "button" => {
            event.text = message
                .get("button")
                .and_then(|button| button.get("text"))
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string();
        }
        "interactive" => {
            let interactive = message.get("interactive");
            event.text = interactive
                .and_then(|node| node.get("button_reply"))
                .and_then(|reply| reply.get("title"))
                .or_else(|| {
                    interactive
                        .and_then(|node| node.get("list_reply"))
                        .and_then(|reply| reply.get("title"))
                })
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string();
        }
        _ => return None,
    }

    if event.text.trim().is_empty() && event.media_urls.is_empty() {
        return None;
    }
    Some(event)
}

struct Failure {
    message: String,
    kind: &'static str,
    retryable: bool,
    retry_after: Option<f64>,
}

impl Failure {
    fn transient(message: String) -> Self {
        Self {
            message,
            kind: "transient",
            retryable: true,
            retry_after: None,
        }
    }

    /// Render this failure as a [`SendResult`], carrying the retry hint when it is worth retrying.
    fn into_result(self) -> SendResult {
        let mut result = SendResult::failed(self.message, Some(self.kind));
        if self.retryable {
            result = result.retryable(self.retry_after);
        }
        result
    }
}

fn classify_failure(status: u16, body: &Value, retry_after: Option<f64>) -> Failure {
    let error = body.get("error");
    let message = error
        .and_then(|error| error.get("message"))
        .and_then(Value::as_str)
        .unwrap_or("graph api error")
        .to_string();
    let code = error
        .and_then(|error| error.get("code"))
        .and_then(Value::as_i64);
    let rendered = match code {
        Some(code) => format!("graph error {code} (HTTP {status}): {message}"),
        None => format!("HTTP {status}: {message}"),
    };
    // Meta's documented throttling codes: 130429, 131048, 131056.
    let throttled = matches!(code, Some(130429 | 131048 | 131056));
    let (kind, retryable) = match status {
        429 => ("rate_limited", true),
        401 | 403 => ("forbidden", false),
        404 => ("not_found", false),
        400 if throttled => ("rate_limited", true),
        400 if matches!(code, Some(131009 | 100)) => ("bad_format", false),
        500..=599 => ("transient", true),
        _ if throttled => ("rate_limited", true),
        _ => ("unknown", false),
    };
    Failure {
        message: rendered,
        kind,
        retryable,
        retry_after,
    }
}

#[async_trait]
impl PlatformAdapter for WhatsAppCloudAdapter {
    async fn connect(&mut self, _is_reconnect: bool) -> bool {
        // The Graph API has no cheap "who am I" call for Cloud API tokens that is always permitted;
        // connectivity is proven by the first send, and the webhook is proven by its handshake.
        !self.access_token.trim().is_empty() && !self.phone_number_id.trim().is_empty()
    }

    async fn disconnect(&mut self) {
        if let Ok(mut map) = self.last_inbound.lock() {
            map.clear();
        }
    }

    async fn send(
        &self,
        chat_id: &str,
        content: &str,
        reply_to: Option<&str>,
        _metadata: Option<Value>,
    ) -> SendResult {
        if content.trim().is_empty() {
            // The reference adapter treats a blank reply as "nothing to say", not as an error.
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

        let chunks = truncate_message(content, MAX_MESSAGE_LENGTH, codepoint_len);
        let mut ids: Vec<String> = Vec::new();
        for (index, chunk) in chunks.iter().enumerate() {
            let mut payload = Self::outbound_payload(
                chat_id,
                "text",
                json!({"body": chunk, "preview_url": true}),
            );
            if index == 0
                && let Some(reply_to) = reply_to
            {
                payload["context"] = json!({"message_id": reply_to});
            }
            match self.post_messages(&payload).await {
                Ok(messages) => {
                    if let Some(id) = messages
                        .first()
                        .and_then(|message| message.get("id"))
                        .and_then(Value::as_str)
                    {
                        ids.push(id.to_string());
                    }
                }
                Err(failure) => {
                    let mut result = SendResult::failed(failure.message, Some(failure.kind));
                    result.continuation_message_ids = ids;
                    if failure.retryable {
                        result = result.retryable(failure.retry_after);
                    }
                    return result;
                }
            }
        }

        match ids.last() {
            Some(last) => SendResult::ok_split(last.clone(), ids),
            None => SendResult {
                success: true,
                message_id: None,
                error: None,
                raw_response: None,
                retryable: false,
                retry_after: None,
                continuation_message_ids: Vec::new(),
                error_kind: None,
            },
        }
    }

    /// Meta couples "mark read" and "typing" into one call, and both need the wamid of the message
    /// being answered. Best effort: the main reply path is never blocked by this.
    async fn send_typing(&self, chat_id: &str, _metadata: Option<Value>) {
        let wamid = self
            .last_inbound
            .lock()
            .ok()
            .and_then(|map| map.get(chat_id).cloned());
        let Some(wamid) = wamid else {
            return;
        };
        let payload = json!({
            "messaging_product": "whatsapp",
            "status": "read",
            "message_id": wamid,
            "typing_indicator": {"type": "text"},
        });
        let request = crate::transport::HttpRequest::json(self.graph_url("messages"), &payload)
            .bearer(&self.access_token);
        let _ = self.transport.execute(request).await;
    }

    async fn send_image(
        &self,
        chat_id: &str,
        url: &str,
        caption: Option<&str>,
        _metadata: Option<Value>,
    ) -> SendResult {
        if !(url.starts_with("https://") || url.starts_with("http://")) {
            return SendResult::failed(
                "whatsapp_cloud sends images by public link; uploading a local file needs multipart, which the shared transport does not expose",
                Some("bad_format"),
            );
        }
        let mut block = json!({"link": url});
        if let Some(caption) = caption {
            block["caption"] = json!(caption);
        }
        let payload = Self::outbound_payload(chat_id, "image", block);
        match self.post_messages(&payload).await {
            Ok(messages) => {
                let id = messages
                    .first()
                    .and_then(|message| message.get("id"))
                    .and_then(Value::as_str)
                    .unwrap_or_default();
                SendResult::ok(id)
            }
            Err(failure) => {
                let mut result = SendResult::failed(failure.message, Some(failure.kind));
                if failure.retryable {
                    result = result.retryable(failure.retry_after);
                }
                result
            }
        }
    }

    async fn get_chat_info(&self, chat_id: &str) -> ChatInfo {
        // The Cloud API has no chat-info endpoint; the profile name arrives on the webhook.
        ChatInfo {
            name: chat_id.to_string(),
            chat_type: "dm".to_string(),
        }
    }

    /// Uploads are implemented: one attachment per message, capped per kind by Meta.
    fn supports_media(&self) -> bool {
        true
    }

    /// Send one attachment, following the reference's upload-then-send flow step for step.
    ///
    /// - [`MediaSource::Bytes`]: `POST /{phone-number-id}/media` as `multipart/form-data`
    ///   (`adapter.py:528-551`), read `id` off the answer, then `POST /{phone-number-id}/messages`
    ///   with `{"id": <media id>}` (`adapter.py:564`).
    /// - [`MediaSource::Url`]: no upload at all — one `POST /messages` with `{"link": <url>}` and
    ///   Meta fetches the public URL itself (`adapter.py:584-590`).
    ///
    /// The block's extra keys are the reference's rules (`adapter.py:565-568`): `caption` on
    /// image/video/document only (never on audio — Meta rejects the field there), `filename` on
    /// documents only. `reply_to` adds `context.message_id` (`adapter.py:357-363`).
    ///
    /// Refusals happen before the wire is touched: empty bytes are `bad_format`, bytes over the
    /// kind's Meta cap ([`MAX_IMAGE_BYTES`] / [`MAX_VIDEO_BYTES`] / [`MAX_AUDIO_BYTES`] /
    /// [`MAX_DOCUMENT_BYTES`], `adapter.py:70-75` and `adapter.py:534-536`) are `too_long`, and a
    /// non-HTTP URL is `bad_format` because there is no local path to upload in its place. A
    /// failed upload ends the send there — nothing half-referenced is ever posted — and a failed
    /// send is surfaced with its own classification; only `messages[0].id` counts as delivered.
    async fn send_media(
        &self,
        chat_id: &str,
        media: &OutboundMedia,
        reply_to: Option<&str>,
    ) -> SendResult {
        let kind = media_kind_name(media.kind);
        let caption = media
            .caption
            .as_deref()
            .filter(|caption| !caption.is_empty())
            .filter(|_| media_kind_takes_caption(media.kind));

        match &media.source {
            MediaSource::Bytes(bytes) => {
                if bytes.is_empty() {
                    return SendResult::failed(
                        "refusing to upload empty media bytes",
                        Some("bad_format"),
                    );
                }
                let cap = media_size_cap(media.kind);
                if bytes.len() > cap {
                    return SendResult::failed(
                        format!(
                            "whatsapp_cloud {kind} upload is {} bytes; Meta's cap is {cap} bytes",
                            bytes.len()
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

                // Step 1: the upload is a hard prerequisite. Its failure ends the send here.
                let media_id = match self.upload_media(&filename, &mime, bytes).await {
                    Ok(media_id) => media_id,
                    Err(failure) => return *failure,
                };

                // Step 2: the message that references the uploaded id.
                let mut block = json!({ "id": media_id });
                if let Some(caption) = caption {
                    block["caption"] = json!(caption);
                }
                if media.kind == MediaKind::Document {
                    block["filename"] = json!(filename);
                }
                self.post_media(&Self::media_payload(chat_id, kind, block, reply_to))
                    .await
            }
            MediaSource::Url(url) => {
                let url = url.trim();
                if !(url.starts_with("https://") || url.starts_with("http://")) {
                    return SendResult::failed(
                        format!(
                            "whatsapp_cloud sends media by public link; `{url}` is not an http(s) \
                             URL and this adapter cannot upload it"
                        ),
                        Some("bad_format"),
                    );
                }
                let mut block = json!({ "link": url });
                if let Some(caption) = caption {
                    block["caption"] = json!(caption);
                }
                if media.kind == MediaKind::Document
                    && let Some(filename) = media
                        .filename
                        .as_deref()
                        .map(sanitize_part_token)
                        .filter(|name| !name.is_empty())
                {
                    block["filename"] = json!(filename);
                }
                self.post_media(&Self::media_payload(chat_id, kind, block, reply_to))
                    .await
            }
        }
    }
}

/// The Graph `type` / block key Meta expects for a media kind (`adapter.py:99-104`).
fn media_kind_name(kind: MediaKind) -> &'static str {
    match kind {
        MediaKind::Image => "image",
        MediaKind::Video => "video",
        MediaKind::Audio => "audio",
        MediaKind::Document => "document",
    }
}

/// Whether a caption may ride along with this kind. The reference lists exactly these three
/// (`adapter.py:565`); audio has no caption field on the Cloud API.
fn media_kind_takes_caption(kind: MediaKind) -> bool {
    matches!(
        kind,
        MediaKind::Image | MediaKind::Video | MediaKind::Document
    )
}

/// Meta's hard per-kind upload cap (`_MEDIA_SIZE_LIMITS`, `adapter.py:70-75`).
fn media_size_cap(kind: MediaKind) -> usize {
    match kind {
        MediaKind::Image => MAX_IMAGE_BYTES,
        MediaKind::Video => MAX_VIDEO_BYTES,
        MediaKind::Audio => MAX_AUDIO_BYTES,
        MediaKind::Document => MAX_DOCUMENT_BYTES,
    }
}

/// Filename and content type used when the caller labels neither.
///
/// The reference guesses the mime from the path's extension and falls back to `_DEFAULT_MIME`
/// (`adapter.py:77-80`, `adapter.py:537`); held bytes have no path, so those defaults are what
/// remains. The filename is only ever read by Meta for documents, but every upload part needs
/// one to be well formed.
fn default_media_file(kind: MediaKind) -> (&'static str, &'static str) {
    match kind {
        MediaKind::Image => ("image.jpg", "image/jpeg"),
        MediaKind::Video => ("video.mp4", "video/mp4"),
        MediaKind::Audio => ("audio.mp3", "audio/mpeg"),
        MediaKind::Document => ("attachment.bin", "application/octet-stream"),
    }
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

/// A fresh `multipart/form-data` boundary for one upload.
///
/// The delimiter must not occur inside the parts, so it mixes the clock with a process counter
/// rather than being a constant. The clock read cannot fail an upload: an unavailable or
/// pre-epoch system clock contributes a zero nonce and the counter still makes the token unique.
fn multipart_boundary() -> String {
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let seq = COUNTER.fetch_add(1, Ordering::Relaxed);
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|elapsed| elapsed.as_nanos() as u64)
        .unwrap_or(0);
    format!("----VIKRAMFormBoundary{nanos:016x}{seq:08x}")
}

/// Encode the body `POST /media` expects, CRLF-framed per RFC 7578.
///
/// Field order and shapes are the reference's `files=` mapping (`adapter.py:540`): `file` with the
/// filename and content type attached to the part, then the plain `messaging_product` and `type`
/// fields — `type` carrying the content type, which is what Meta documents for this endpoint.
/// Nothing here can fail (every part is bytes), so the caller never has to invent a body.
fn encode_media_multipart(boundary: &str, filename: &str, mime: &str, bytes: &[u8]) -> Vec<u8> {
    let mut body = Vec::with_capacity(bytes.len() + filename.len() + mime.len() + 512);
    body.extend_from_slice(format!("--{boundary}\r\n").as_bytes());
    body.extend_from_slice(
        format!("Content-Disposition: form-data; name=\"file\"; filename=\"{filename}\"\r\n")
            .as_bytes(),
    );
    body.extend_from_slice(format!("Content-Type: {mime}\r\n\r\n").as_bytes());
    body.extend_from_slice(bytes);
    body.extend_from_slice(b"\r\n");
    for (name, value) in [("messaging_product", "whatsapp"), ("type", mime)] {
        body.extend_from_slice(format!("--{boundary}\r\n").as_bytes());
        body.extend_from_slice(
            format!("Content-Disposition: form-data; name=\"{name}\"\r\n\r\n").as_bytes(),
        );
        body.extend_from_slice(value.as_bytes());
        body.extend_from_slice(b"\r\n");
    }
    body.extend_from_slice(format!("--{boundary}--\r\n").as_bytes());
    body
}

fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    let mut diff = 0u8;
    for (x, y) in a.iter().zip(b.iter()) {
        diff |= x ^ y;
    }
    diff == 0
}

/// HMAC-SHA-256 over `message`, hex encoded. The hash lives in `pairing::sha256`; this only adds
/// the HMAC construction.
fn hmac_sha256_hex(key: &[u8], message: &[u8]) -> String {
    let mut block = [0u8; 64];
    if key.len() > 64 {
        block[..32].copy_from_slice(&crate::pairing::sha256::digest(key));
    } else {
        block[..key.len()].copy_from_slice(key);
    }
    let mut inner_pad = [0x36u8; 64];
    let mut outer_pad = [0x5cu8; 64];
    for i in 0..64 {
        inner_pad[i] ^= block[i];
        outer_pad[i] ^= block[i];
    }
    let mut inner = inner_pad.to_vec();
    inner.extend_from_slice(message);
    let inner_hash = crate::pairing::sha256::digest(&inner);
    let mut outer = outer_pad.to_vec();
    outer.extend_from_slice(&inner_hash);
    crate::pairing::sha256::digest(&outer)
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::transport::{HttpMethod, HttpRequest, HttpResponse, Transport, TransportError};
    use async_trait::async_trait;
    use std::collections::VecDeque;
    use std::sync::Arc;

    struct MockTransport {
        requests: Mutex<Vec<HttpRequest>>,
        responses: Mutex<VecDeque<HttpResponse>>,
    }

    impl MockTransport {
        fn new(responses: Vec<HttpResponse>) -> Arc<Self> {
            Arc::new(Self {
                requests: Mutex::new(Vec::new()),
                responses: Mutex::new(responses.into()),
            })
        }

        fn requests(&self) -> Vec<HttpRequest> {
            self.requests.lock().unwrap().clone()
        }
    }

    #[async_trait]
    impl Transport for MockTransport {
        async fn execute(&self, request: HttpRequest) -> Result<HttpResponse, TransportError> {
            self.requests.lock().unwrap().push(request);
            self.responses
                .lock()
                .unwrap()
                .pop_front()
                .ok_or_else(|| TransportError::Request("no scripted response".into()))
        }
    }

    fn adapter(responses: Vec<HttpResponse>) -> (WhatsAppCloudAdapter, Arc<MockTransport>) {
        let mock = MockTransport::new(responses);
        let adapter =
            WhatsAppCloudAdapter::new(Arc::clone(&mock) as SharedTransport, "token", "phone-id");
        (adapter, mock)
    }

    fn ok_with_id(id: &str) -> HttpResponse {
        HttpResponse {
            status: 200,
            body: json!({"messages": [{"id": id}]}).to_string(),
            retry_after: None,
        }
    }

    fn body_of(request: &HttpRequest) -> Value {
        serde_json::from_slice(request.body.as_ref().expect("body")).expect("json")
    }

    fn raw_body(request: &HttpRequest) -> &[u8] {
        request.body.as_deref().unwrap_or_default()
    }

    fn text_body(request: &HttpRequest) -> String {
        String::from_utf8_lossy(raw_body(request)).into_owned()
    }

    fn header<'a>(request: &'a HttpRequest, name: &str) -> Option<&'a str> {
        request
            .headers
            .iter()
            .find(|(key, _)| key.eq_ignore_ascii_case(name))
            .map(|(_, value)| value.as_str())
    }

    fn boundary_of(request: &HttpRequest) -> String {
        header(request, "Content-Type")
            .and_then(|value| value.strip_prefix("multipart/form-data; boundary="))
            .unwrap_or_default()
            .to_string()
    }

    fn contains_bytes(haystack: &[u8], needle: &[u8]) -> bool {
        !needle.is_empty()
            && haystack
                .windows(needle.len())
                .any(|window| window == needle)
    }

    fn upload_ok(id: &str) -> HttpResponse {
        HttpResponse {
            status: 200,
            body: json!({ "id": id }).to_string(),
            retry_after: None,
        }
    }

    fn error_body(code: i64, message: &str) -> String {
        json!({"error": {"code": code, "message": message, "type": "OAuthException"}}).to_string()
    }

    fn bytes_media(kind: MediaKind, bytes: &[u8]) -> OutboundMedia {
        OutboundMedia::new(kind, MediaSource::Bytes(bytes.to_vec()))
    }

    #[tokio::test]
    async fn send_builds_the_graph_envelope_and_quotes_the_first_chunk() {
        let (adapter, mock) = adapter(vec![ok_with_id("wamid.1")]);
        let result = adapter
            .send("15551234567", "hello", Some("wamid.0"), None)
            .await;
        assert!(result.success);
        assert_eq!(result.message_id.as_deref(), Some("wamid.1"));
        let requests = mock.requests();
        assert_eq!(requests.len(), 1);
        assert_eq!(
            requests[0].url,
            "https://graph.facebook.com/v21.0/phone-id/messages"
        );
        assert!(
            requests[0]
                .headers
                .iter()
                .any(|(name, value)| name == "Authorization" && value == "Bearer token")
        );
        let body = body_of(&requests[0]);
        assert_eq!(body["to"], "15551234567");
        assert_eq!(body["type"], "text");
        assert_eq!(body["text"]["body"], "hello");
        assert_eq!(body["context"]["message_id"], "wamid.0");
    }

    #[tokio::test]
    async fn a_long_answer_is_chunked_and_quoted_once() {
        let content = "x".repeat(5000);
        let (adapter, mock) = adapter(vec![ok_with_id("a"), ok_with_id("b")]);
        let result = adapter.send("1", &content, Some("wamid.0"), None).await;
        assert!(result.success);
        assert_eq!(result.continuation_message_ids, vec!["a", "b"]);
        assert_eq!(result.message_id.as_deref(), Some("b"));
        let requests = mock.requests();
        assert_eq!(requests.len(), 2);
        let first = body_of(&requests[0]);
        let second = body_of(&requests[1]);
        assert!(first["text"]["body"].as_str().unwrap().len() <= MAX_MESSAGE_LENGTH);
        assert!(second["text"]["body"].as_str().unwrap().len() <= MAX_MESSAGE_LENGTH);
        assert_eq!(first["context"]["message_id"], "wamid.0");
        assert!(
            second.get("context").is_none(),
            "only the first chunk quotes"
        );
    }

    #[tokio::test]
    async fn a_blank_reply_sends_nothing_and_is_not_an_error() {
        let (adapter, mock) = adapter(vec![]);
        let result = adapter.send("1", "   ", None, None).await;
        assert!(result.success);
        assert!(result.message_id.is_none());
        assert!(mock.requests().is_empty());
    }

    #[tokio::test]
    async fn throttling_is_retryable_with_the_server_delay() {
        let (adapter, _) = adapter(vec![HttpResponse {
            status: 429,
            body: json!({"error": {"code": 130429, "message": "rate limited"}}).to_string(),
            retry_after: Some(7.0),
        }]);
        let result = adapter.send("1", "hi", None, None).await;
        assert!(!result.success);
        assert!(result.retryable);
        assert_eq!(result.retry_after, Some(7.0));
        assert_eq!(result.error_kind.as_deref(), Some("rate_limited"));
    }

    #[tokio::test]
    async fn an_auth_failure_is_forbidden_and_not_retryable() {
        let (adapter, _) = adapter(vec![HttpResponse {
            status: 401,
            body: json!({"error": {"code": 190, "message": "bad token"}}).to_string(),
            retry_after: None,
        }]);
        let result = adapter.send("1", "hi", None, None).await;
        assert!(!result.success);
        assert!(!result.retryable);
        assert_eq!(result.error_kind.as_deref(), Some("forbidden"));
    }

    #[test]
    fn the_webhook_handshake_requires_the_configured_token() {
        let (adapter, _) = adapter(vec![]);
        assert!(adapter.verify_webhook("subscribe", "t", "c").is_err());
        let adapter = adapter.with_webhook_secrets("secret", "verify-me");
        assert_eq!(
            adapter
                .verify_webhook("subscribe", "verify-me", "challenge")
                .unwrap(),
            "challenge"
        );
        assert!(adapter.verify_webhook("subscribe", "wrong", "c").is_err());
        assert!(
            adapter
                .verify_webhook("unsubscribe", "verify-me", "c")
                .is_err()
        );
    }

    #[test]
    fn the_signature_requires_the_exact_body_and_secret() {
        let (adapter, _) = adapter(vec![]);
        let adapter = adapter.with_webhook_secrets("secret", "v");
        let body = br#"{"entry":[]}"#;
        let good = format!("sha256={}", hmac_sha256_hex(b"secret", body));
        assert!(adapter.verify_signature(body, &good));
        assert!(!adapter.verify_signature(br#"{"entry":[1]}"#, &good));
        assert!(!adapter.verify_signature(body, "sha256=deadbeef"));
        assert!(!adapter.verify_signature(body, &good.replace("sha256=", "")));
    }

    #[test]
    fn a_text_webhook_becomes_an_event_with_the_sender_and_id() {
        let (adapter, _) = adapter(vec![]);
        let body = json!({
            "entry": [{"changes": [{"value": {
                "contacts": [{"profile": {"name": "Asha"}}],
                "messages": [{
                    "from": "15551234567",
                    "id": "wamid.ABC",
                    "timestamp": "1700000000",
                    "type": "text",
                    "text": {"body": "hello there"}
                }]
            }}]}]
        })
        .to_string();
        let events = adapter.parse_webhook(&body).unwrap();
        assert_eq!(events.len(), 1);
        let event = &events[0];
        assert_eq!(event.text, "hello there");
        assert_eq!(event.source.chat_id, "15551234567");
        assert_eq!(event.source.chat_type, "dm");
        assert_eq!(event.user_id.as_deref(), Some("15551234567"));
        assert_eq!(event.user_name.as_deref(), Some("Asha"));
        assert_eq!(event.message_id.as_deref(), Some("wamid.ABC"));
        assert_eq!(event.message_type, MessageType::Text);
    }

    #[test]
    fn an_image_webhook_keeps_the_media_and_the_quoted_id() {
        let (adapter, _) = adapter(vec![]);
        let body = json!({
            "entry": [{"changes": [{"value": {"messages": [{
                "from": "1",
                "id": "wamid.IMG",
                "type": "image",
                "context": {"id": "wamid.QUOTED"},
                "image": {"id": "media-123", "mime_type": "image/jpeg", "caption": "look"}
            }]}}]}]
        })
        .to_string();
        let events = adapter.parse_webhook(&body).unwrap();
        assert_eq!(events.len(), 1);
        let event = &events[0];
        assert_eq!(event.text, "look");
        assert_eq!(event.message_type, MessageType::Photo);
        assert_eq!(event.media_urls, vec!["media-123"]);
        assert_eq!(event.media_types, vec!["image/jpeg"]);
        assert_eq!(event.reply_to_message_id.as_deref(), Some("wamid.QUOTED"));
    }

    #[test]
    fn status_callbacks_produce_no_events() {
        let (adapter, _) = adapter(vec![]);
        let body = json!({
            "entry": [{"changes": [{"value": {"statuses": [{"id": "wamid.X", "status": "read"}]}}]}]
        })
        .to_string();
        assert!(adapter.parse_webhook(&body).unwrap().is_empty());
    }

    // ----- outbound media (`supports_media` / `send_media`) -----

    #[test]
    fn media_is_supported() {
        let (adapter, _) = adapter(vec![]);
        assert!(adapter.supports_media());
    }

    #[tokio::test]
    async fn every_kind_uploads_multipart_then_sends_the_media_id() {
        for (kind, kind_name, filename, mime) in [
            (MediaKind::Image, "image", "photo.jpg", "image/jpeg"),
            (MediaKind::Video, "video", "clip.mp4", "video/mp4"),
            (MediaKind::Audio, "audio", "note.mp3", "audio/mpeg"),
            (
                MediaKind::Document,
                "document",
                "report.pdf",
                "application/pdf",
            ),
        ] {
            let (adapter, mock) = adapter(vec![upload_ok("media-9"), ok_with_id("wamid.9")]);
            let media = bytes_media(kind, b"-bytes-")
                .with_filename(filename)
                .with_mime(mime);
            let result = adapter.send_media("15551234567", &media, None).await;
            assert!(result.success, "{kind_name}: {:?}", result.error);
            assert_eq!(result.message_id.as_deref(), Some("wamid.9"));

            let requests = mock.requests();
            assert_eq!(requests.len(), 2, "{kind_name}: one upload, then one send");

            // 1. `POST /{phone}/media`, multipart, bearing the token.
            let upload = &requests[0];
            assert_eq!(upload.method, HttpMethod::Post);
            assert_eq!(
                upload.url,
                "https://graph.facebook.com/v21.0/phone-id/media"
            );
            assert_eq!(header(upload, "Authorization"), Some("Bearer token"));
            let boundary = boundary_of(upload);
            assert!(!boundary.is_empty(), "{kind_name}: boundary is declared");
            assert_eq!(
                header(upload, "Content-Type"),
                Some(format!("multipart/form-data; boundary={boundary}").as_str())
            );

            let body = text_body(upload);
            assert!(body.starts_with(&format!("--{boundary}\r\n")), "{body}");
            assert!(
                body.contains(&format!(
                    "Content-Disposition: form-data; name=\"file\"; filename=\"{filename}\"\r\n\
                     Content-Type: {mime}\r\n\r\n"
                )),
                "{kind_name}: the file part carries the filename and content type: {body}"
            );
            assert!(
                body.contains(
                    "Content-Disposition: form-data; name=\"messaging_product\"\r\n\r\nwhatsapp\r\n"
                ),
                "{kind_name}: {body}"
            );
            assert!(
                body.contains(&format!(
                    "Content-Disposition: form-data; name=\"type\"\r\n\r\n{mime}\r\n"
                )),
                "{kind_name}: Meta's upload `type` field is the content type: {body}"
            );
            assert!(body.ends_with(&format!("--{boundary}--\r\n")), "{body}");
            assert!(
                contains_bytes(raw_body(upload), b"-bytes-"),
                "{kind_name}: the raw bytes travel inside the file part"
            );

            // 2. `POST /{phone}/messages` with `type` = the media kind and `{kind: {id}}`.
            let send = &requests[1];
            assert_eq!(
                send.url,
                "https://graph.facebook.com/v21.0/phone-id/messages"
            );
            let payload = body_of(send);
            assert_eq!(payload["messaging_product"], "whatsapp");
            assert_eq!(payload["to"], "15551234567");
            assert_eq!(payload["type"], kind_name);
            assert_eq!(payload[kind_name]["id"], "media-9");
        }
    }

    #[tokio::test]
    async fn a_caption_and_a_quote_ride_on_an_image_send() {
        let (adapter, mock) = adapter(vec![upload_ok("media-1"), ok_with_id("wamid.1")]);
        let media = bytes_media(MediaKind::Image, b"png-bytes").with_caption("a chart");
        let result = adapter.send_media("1", &media, Some("wamid.0")).await;
        assert!(result.success);
        let requests = mock.requests();
        let payload = body_of(&requests[1]);
        assert_eq!(payload["type"], "image");
        assert_eq!(payload["image"]["id"], "media-1");
        assert_eq!(payload["image"]["caption"], "a chart");
        assert_eq!(payload["context"]["message_id"], "wamid.0");
        assert!(
            payload["image"].get("filename").is_none(),
            "filename is document-only: {payload}"
        );
    }

    #[tokio::test]
    async fn audio_never_carries_a_caption() {
        let (adapter, mock) = adapter(vec![upload_ok("media-a"), ok_with_id("wamid.a")]);
        let media = bytes_media(MediaKind::Audio, b"mp3")
            .with_caption("ignored")
            .with_filename("note.mp3");
        let result = adapter.send_media("1", &media, None).await;
        assert!(result.success);
        let payload = body_of(&mock.requests()[1]);
        assert_eq!(payload["type"], "audio");
        assert!(
            payload["audio"].get("caption").is_none(),
            "audio takes no caption: {payload}"
        );
        assert!(payload["audio"].get("filename").is_none(), "{payload}");
    }

    #[tokio::test]
    async fn a_document_carries_its_filename_on_the_wire_and_in_the_message() {
        let (adapter, mock) = adapter(vec![upload_ok("media-d"), ok_with_id("wamid.d")]);
        let media = bytes_media(MediaKind::Document, b"pdf")
            .with_filename("quarterly report.pdf")
            .with_mime("application/pdf")
            .with_caption("Q3");
        let result = adapter.send_media("1", &media, None).await;
        assert!(result.success);
        let requests = mock.requests();
        assert!(
            text_body(&requests[0]).contains("filename=\"quarterly report.pdf\""),
            "{}",
            text_body(&requests[0])
        );
        let payload = body_of(&requests[1]);
        assert_eq!(payload["document"]["filename"], "quarterly report.pdf");
        assert_eq!(payload["document"]["caption"], "Q3");
    }

    #[tokio::test]
    async fn url_media_is_sent_as_a_link_without_an_upload() {
        let (adapter, mock) = adapter(vec![ok_with_id("wamid.u")]);
        let media =
            OutboundMedia::image_url("https://cdn.example.test/a.png").with_caption("remote");
        let result = adapter.send_media("1", &media, Some("wamid.0")).await;
        assert!(result.success);
        assert_eq!(result.message_id.as_deref(), Some("wamid.u"));
        let requests = mock.requests();
        assert_eq!(
            requests.len(),
            1,
            "a link send is one call — Meta fetches the URL"
        );
        assert_eq!(
            requests[0].url,
            "https://graph.facebook.com/v21.0/phone-id/messages"
        );
        let payload = body_of(&requests[0]);
        assert_eq!(payload["type"], "image");
        assert_eq!(payload["image"]["link"], "https://cdn.example.test/a.png");
        assert_eq!(payload["image"]["caption"], "remote");
        assert_eq!(payload["context"]["message_id"], "wamid.0");
        assert!(payload["image"].get("id").is_none(), "{payload}");
    }

    #[tokio::test]
    async fn link_media_follows_the_same_caption_and_filename_rules() {
        let (adapter, mock) = adapter(vec![ok_with_id("wamid.a"), ok_with_id("wamid.d")]);

        let audio = OutboundMedia::new(
            MediaKind::Audio,
            MediaSource::Url("https://cdn.example.test/a.mp3".into()),
        )
        .with_caption("ignored")
        .with_filename("a.mp3");
        assert!(adapter.send_media("1", &audio, None).await.success);

        let document = OutboundMedia::new(
            MediaKind::Document,
            MediaSource::Url("https://cdn.example.test/r.pdf".into()),
        )
        .with_filename("r.pdf");
        assert!(adapter.send_media("1", &document, None).await.success);

        let requests = mock.requests();
        assert_eq!(requests.len(), 2);
        let audio_payload = body_of(&requests[0]);
        assert_eq!(audio_payload["type"], "audio");
        assert!(audio_payload["audio"].get("caption").is_none());
        assert!(audio_payload["audio"].get("filename").is_none());

        let document_payload = body_of(&requests[1]);
        assert_eq!(document_payload["type"], "document");
        assert_eq!(document_payload["document"]["filename"], "r.pdf");
        assert!(document_payload["document"].get("caption").is_none());
    }

    #[tokio::test]
    async fn a_failed_upload_never_reaches_the_send() {
        let (adapter, mock) = adapter(vec![HttpResponse {
            status: 500,
            body: error_body(2, "upload exploded"),
            retry_after: None,
        }]);
        let result = adapter
            .send_media("1", &bytes_media(MediaKind::Image, b"png"), None)
            .await;
        assert!(!result.success);
        assert_eq!(result.error_kind.as_deref(), Some("transient"));
        assert!(result.retryable);
        assert!(
            result
                .error
                .as_deref()
                .unwrap_or_default()
                .contains("upload exploded"),
            "{:?}",
            result.error
        );
        assert_eq!(
            mock.requests().len(),
            1,
            "the send must not happen after a failed upload"
        );
    }

    #[tokio::test]
    async fn a_failed_send_after_the_upload_is_surfaced() {
        let (adapter, mock) = adapter(vec![
            upload_ok("media-1"),
            HttpResponse {
                status: 400,
                body: error_body(100, "Invalid parameter"),
                retry_after: None,
            },
        ]);
        let result = adapter
            .send_media("1", &bytes_media(MediaKind::Image, b"png"), None)
            .await;
        assert!(!result.success);
        assert!(result.message_id.is_none());
        assert_eq!(result.error_kind.as_deref(), Some("bad_format"));
        assert_eq!(
            result.error.as_deref(),
            Some("graph error 100 (HTTP 400): Invalid parameter")
        );
        assert_eq!(mock.requests().len(), 2, "upload then the refused send");
    }

    #[tokio::test]
    async fn a_graph_error_envelope_on_the_upload_is_surfaced() {
        let (adapter, mock) = adapter(vec![HttpResponse {
            status: 400,
            body: error_body(131009, "Unsupported file type"),
            retry_after: None,
        }]);
        let result = adapter
            .send_media("1", &bytes_media(MediaKind::Document, b"pdf"), None)
            .await;
        assert!(!result.success);
        assert_eq!(result.error_kind.as_deref(), Some("bad_format"));
        assert_eq!(
            result.error.as_deref(),
            Some("graph error 131009 (HTTP 400): Unsupported file type")
        );
        assert_eq!(mock.requests().len(), 1);
    }

    #[tokio::test]
    async fn media_throttling_is_retryable_with_the_server_delay() {
        let (adapter, _) = adapter(vec![
            upload_ok("media-1"),
            HttpResponse {
                status: 429,
                body: error_body(130429, "rate limited"),
                retry_after: Some(11.0),
            },
        ]);
        let result = adapter
            .send_media("1", &bytes_media(MediaKind::Video, b"mp4"), None)
            .await;
        assert!(!result.success);
        assert!(result.retryable);
        assert_eq!(result.retry_after, Some(11.0));
        assert_eq!(result.error_kind.as_deref(), Some("rate_limited"));
    }

    #[tokio::test]
    async fn http_failures_on_the_upload_map_onto_the_send_taxonomy() {
        for (status, code, expected, retryable) in [
            (403u16, 190i64, "forbidden", false),
            (404, 100, "not_found", false),
            (500, 2, "transient", true),
        ] {
            let (adapter, mock) = adapter(vec![HttpResponse {
                status,
                body: error_body(code, "nope"),
                retry_after: None,
            }]);
            let result = adapter
                .send_media("1", &bytes_media(MediaKind::Image, b"png"), None)
                .await;
            assert!(!result.success, "{status}");
            assert_eq!(result.error_kind.as_deref(), Some(expected), "{status}");
            assert_eq!(result.retryable, retryable, "{status}");
            assert_eq!(mock.requests().len(), 1, "{status}");
        }
    }

    #[tokio::test]
    async fn a_transport_failure_is_transient_and_retryable() {
        // No scripted response: the mock's transport returns a `TransportError`.
        let (adapter, _) = adapter(vec![]);
        let result = adapter
            .send_media("1", &bytes_media(MediaKind::Video, b"mp4"), None)
            .await;
        assert!(!result.success);
        assert_eq!(result.error_kind.as_deref(), Some("transient"));
        assert!(result.retryable);
        assert!(result.retry_after.is_none());
    }

    #[tokio::test]
    async fn each_kind_refuses_bytes_over_metas_cap_before_the_wire() {
        for (kind, cap) in [
            (MediaKind::Image, MAX_IMAGE_BYTES),
            (MediaKind::Video, MAX_VIDEO_BYTES),
            (MediaKind::Audio, MAX_AUDIO_BYTES),
            (MediaKind::Document, MAX_DOCUMENT_BYTES),
        ] {
            assert_eq!(media_size_cap(kind), cap);
            let (adapter, mock) = adapter(vec![]);
            let oversize = vec![0u8; cap + 1];
            let result = adapter
                .send_media("1", &bytes_media(kind, &oversize), None)
                .await;
            assert!(!result.success, "{kind:?}");
            assert_eq!(result.error_kind.as_deref(), Some("too_long"), "{kind:?}");
            assert!(
                mock.requests().is_empty(),
                "{kind:?}: an over-cap upload must not reach the network"
            );
        }
        assert_eq!(MAX_IMAGE_BYTES, 5 * 1024 * 1024);
        assert_eq!(MAX_VIDEO_BYTES, 16 * 1024 * 1024);
        assert_eq!(MAX_AUDIO_BYTES, 16 * 1024 * 1024);
        assert_eq!(MAX_DOCUMENT_BYTES, 100 * 1024 * 1024);
    }

    #[tokio::test]
    async fn bytes_at_the_cap_are_still_uploaded() {
        let bytes = vec![7u8; MAX_IMAGE_BYTES];
        let (adapter, mock) = adapter(vec![upload_ok("media-cap"), ok_with_id("wamid.cap")]);
        let result = adapter
            .send_media("1", &bytes_media(MediaKind::Image, &bytes), None)
            .await;
        assert!(result.success, "{:?}", result.error);
        assert_eq!(mock.requests().len(), 2);
    }

    #[tokio::test]
    async fn empty_bytes_are_refused_before_the_wire() {
        let (adapter, mock) = adapter(vec![]);
        let result = adapter
            .send_media("1", &bytes_media(MediaKind::Document, b""), None)
            .await;
        assert!(!result.success);
        assert_eq!(result.error_kind.as_deref(), Some("bad_format"));
        assert!(mock.requests().is_empty());
    }

    #[tokio::test]
    async fn a_non_http_url_is_refused_before_the_wire() {
        let (adapter, mock) = adapter(vec![]);
        let media = OutboundMedia::new(MediaKind::Image, MediaSource::Url("/tmp/a.png".into()));
        let result = adapter.send_media("1", &media, None).await;
        assert!(!result.success);
        assert_eq!(result.error_kind.as_deref(), Some("bad_format"));
        assert!(mock.requests().is_empty());
    }

    #[tokio::test]
    async fn an_accepted_send_without_a_message_id_is_not_success() {
        let (adapter, _) = adapter(vec![
            upload_ok("media-1"),
            HttpResponse {
                status: 200,
                body: json!({"messages": []}).to_string(),
                retry_after: None,
            },
        ]);
        let result = adapter
            .send_media("1", &bytes_media(MediaKind::Image, b"png"), None)
            .await;
        assert!(!result.success);
        assert!(result.message_id.is_none());
        assert_eq!(result.error_kind.as_deref(), Some("unknown"));
    }

    #[tokio::test]
    async fn an_upload_without_an_id_is_a_failure_and_stops_the_send() {
        let (adapter, mock) = adapter(vec![HttpResponse {
            status: 200,
            body: json!({"id": ""}).to_string(),
            retry_after: None,
        }]);
        let result = adapter
            .send_media("1", &bytes_media(MediaKind::Image, b"png"), None)
            .await;
        assert!(!result.success);
        assert_eq!(result.error_kind.as_deref(), Some("unknown"));
        assert!(
            result
                .error
                .as_deref()
                .unwrap_or_default()
                .contains("missing 'id'"),
            "{:?}",
            result.error
        );
        assert_eq!(mock.requests().len(), 1);
    }

    #[tokio::test]
    async fn part_headers_cannot_be_injected_from_a_filename_or_mime() {
        let (adapter, mock) = adapter(vec![upload_ok("media-1"), ok_with_id("wamid.1")]);
        let media = bytes_media(MediaKind::Document, b"pdf")
            .with_filename("a\r\nContent-Type: text/html\r\n\r\n<b>hi</b>")
            .with_mime("application/pdf\r\nX-Evil: 1");
        let result = adapter.send_media("1", &media, None).await;
        assert!(result.success, "{:?}", result.error);
        let requests = mock.requests();
        let body = text_body(&requests[0]);
        // The payload survives as text on one line; what must never appear is a line break in front
        // of it, which is what would turn it into a header of its own.
        assert!(!body.contains("\r\nX-Evil"), "{body}");
        assert!(!body.contains("\r\nContent-Type: text/html"), "{body}");
        assert!(
            body.contains("filename=\"aContent-Type: text/html<b>hi</b>\""),
            "{body}"
        );
        assert!(
            body.contains("Content-Type: application/pdfX-Evil: 1\r\n\r\n"),
            "{body}"
        );
        assert_eq!(
            body_of(&requests[1])["document"]["filename"],
            "aContent-Type: text/html<b>hi</b>"
        );
    }
}
